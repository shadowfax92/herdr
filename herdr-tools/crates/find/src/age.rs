//! Bounded session-start recovery, owned by one catalog refresh on its read worker.
//! Transcript contents stay here: only timestamps escape, and neither file mtime
//! nor activity in another session can make an old or forked session look young.
use serde_json::Value;
use std::collections::HashMap;
use std::ffi::OsString;
use std::fs::{self, File};
use std::io::{BufRead, BufReader, Read};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

const MAX_ENTRIES: usize = 20_000;
const HEAD_BYTES: usize = 256 * 1024;
const TOTAL_HEAD_BYTES: usize = 16 * 1024 * 1024;

/// One refresh shares discovery and recovered starts across all panes. A new
/// instance on the next refresh also retries transcripts that did not exist yet.
pub(crate) struct Starts {
    roots: HashMap<&'static str, Vec<PathBuf>>,
    rtr: Option<PathBuf>,
    files: HashMap<String, Vec<PathBuf>>,
    starts: HashMap<(String, String), Option<i64>>,
    entries_left: usize,
    bytes_left: usize,
}

impl Starts {
    pub fn from_environment() -> Self {
        Self::from_env(|key| std::env::var_os(key))
    }

    // Injecting the environment keeps fixture tests independent of real homes
    // without racing process-global environment changes in Rust's test runner.
    fn from_env(env: impl Fn(&str) -> Option<OsString>) -> Self {
        let path = |key| env(key).filter(|v| !v.is_empty()).map(PathBuf::from);
        let home = path("HOME");
        let state =
            path("XDG_STATE_HOME").or_else(|| home.as_ref().map(|h| h.join(".local/state")));
        let mut roots = HashMap::new();
        for (vendor, var, default, child) in [
            ("codex", "CODEX_HOME", ".codex", "sessions"),
            ("claude", "CLAUDE_CONFIG_DIR", ".claude", "projects"),
        ] {
            let mut paths = Vec::new();
            if let Some(home) = &home {
                paths.push(home.join(default).join(child));
            }
            if let Some(root) = path(var) {
                paths.push(root.join(child));
            }
            roots.insert(vendor, paths);
        }
        Self {
            roots,
            rtr: state.map(|s| s.join("rtr/homes")),
            files: HashMap::new(),
            starts: HashMap::new(),
            entries_left: MAX_ENTRIES,
            bytes_left: TOTAL_HEAD_BYTES,
        }
    }

    pub fn resolve(&mut self, agent: Option<&str>, session: &Value) -> Option<i64> {
        let vendor = agent?;
        if !matches!(vendor, "codex" | "claude")
            || session["kind"] != "id"
            || session["agent"] != vendor
        {
            return None;
        }
        let id = session["value"].as_str()?;
        if id.is_empty()
            || id.len() > 160
            || !id.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'-')
        {
            return None;
        }
        let key = (vendor.to_owned(), id.to_owned());
        if let Some(start) = self.starts.get(&key) {
            return *start;
        }
        if !self.files.contains_key(vendor) {
            let mut roots = self.roots.get(vendor).cloned().unwrap_or_default();
            if let Some(rtr) = &self.rtr {
                if let Ok(profiles) = fs::read_dir(rtr.join(vendor)) {
                    for profile in profiles.flatten() {
                        if self.entries_left == 0 {
                            break;
                        }
                        self.entries_left -= 1;
                        if profile.file_type().is_ok_and(|t| t.is_dir()) {
                            roots.push(profile.path().join(if vendor == "codex" {
                                "sessions"
                            } else {
                                "projects"
                            }));
                        }
                    }
                }
            }
            roots.sort();
            roots.dedup();
            let mut files = Vec::new();
            for root in roots {
                collect(&root, 4, &mut self.entries_left, &mut files);
            }
            files.sort();
            files.dedup();
            self.files.insert(vendor.into(), files);
        }
        let suffix = format!("-{id}");
        let start = self.files[vendor]
            .iter()
            .filter(|file| {
                let name = file
                    .file_stem()
                    .and_then(|s| s.to_str())
                    .unwrap_or_default();
                name == id || (vendor == "codex" && name.ends_with(&suffix))
            })
            .filter_map(|file| start_timestamp(file, vendor, id, &mut self.bytes_left))
            .min()
            .or_else(|| (vendor == "codex").then(|| uuid_v7_start(id)).flatten());
        self.starts.insert(key, start);
        start
    }
}

fn collect(dir: &Path, depth: usize, budget: &mut usize, files: &mut Vec<PathBuf>) {
    if *budget == 0 {
        return;
    }
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        if *budget == 0 {
            break;
        }
        *budget -= 1;
        let Ok(kind) = entry.file_type() else {
            continue;
        };
        // Symlinks and special files are never traversed or opened. A pipe
        // masquerading as JSONL must not strand a catalog worker in read().
        if kind.is_dir() && depth > 0 {
            collect(&entry.path(), depth - 1, budget, files);
        } else if kind.is_file() && entry.path().extension().is_some_and(|e| e == "jsonl") {
            files.push(entry.path());
        }
    }
}

fn start_timestamp(path: &Path, vendor: &str, session: &str, budget: &mut usize) -> Option<i64> {
    let limit = HEAD_BYTES.min(*budget);
    if limit == 0 {
        return None;
    }
    // Small reads avoid spending the shared budget on transcript tails when
    // the start is on line one. Take bounds even a newline-free record; charge
    // actual file reads (including BufReader prefetch), not a full head per file.
    let mut reader = BufReader::with_capacity(1024, File::open(path).ok()?.take(limit as u64));
    let start = (|| {
        let mut line = Vec::new();
        loop {
            line.clear();
            if reader.read_until(b'\n', &mut line).ok()? == 0 {
                return None;
            }
            let Ok(value) = serde_json::from_slice::<Value>(&line) else {
                continue;
            };
            let timestamp = match vendor {
                "codex" if value["type"] == "session_meta" && value["payload"]["id"] == session => {
                    value["payload"]["timestamp"].as_str()
                }
                "claude"
                    if value["sessionId"] == session
                        && value.get("parentUuid").is_some_and(Value::is_null)
                        && value["isSidechain"] != true
                        && matches!(
                            value["type"].as_str(),
                            Some("user" | "assistant" | "attachment")
                        ) =>
                {
                    value["timestamp"].as_str()
                }
                _ => None,
            };
            if let Some(at) = timestamp.and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok()) {
                let millis = at.timestamp_millis();
                if millis > 0 {
                    return Some(millis);
                }
            }
        }
    })();
    *budget -= limit - reader.get_ref().limit() as usize;
    start
}

fn uuid_v7_start(id: &str) -> Option<i64> {
    let bytes = id.as_bytes();
    if bytes.len() != 36
        || [8, 13, 18, 23].iter().any(|&i| bytes[i] != b'-')
        || bytes[14] != b'7'
        || !matches!(bytes[19], b'8' | b'9' | b'a' | b'b' | b'A' | b'B')
        || bytes
            .iter()
            .enumerate()
            .any(|(i, c)| ![8, 13, 18, 23].contains(&i) && !c.is_ascii_hexdigit())
    {
        return None;
    }
    let start = i64::from_str_radix(&format!("{}{}", &id[..8], &id[9..13]), 16).ok()?;
    (start > 0).then_some(start)
}

pub(crate) fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis().min(i64::MAX as u128) as i64)
        .unwrap_or(0)
}

/// Compact session age, rounded down; future clocks clamp to zero.
pub(crate) fn label(started_at_ms: i64, now_ms: i64) -> String {
    let seconds = now_ms.saturating_sub(started_at_ms).max(0) / 1000;
    match seconds {
        0..60 => format!("{seconds}s"),
        60..3600 => format!("{}m", seconds / 60),
        3600..86400 => format!("{}h", seconds / 3600),
        _ if seconds / 86400 > 9999 => "9999d+".into(),
        _ => format!("{}d", seconds / 86400),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn starts(root: &Path) -> Starts {
        Starts::from_env(|key| match key {
            "HOME" => Some(root.join("home").into_os_string()),
            "XDG_STATE_HOME" => Some(root.join("state").into_os_string()),
            "CODEX_HOME" => Some(root.join("codex").into_os_string()),
            "CLAUDE_CONFIG_DIR" => Some(root.join("claude").into_os_string()),
            _ => None,
        })
    }
    fn transcript(root: &Path, path: &str, contents: &str) {
        let path = root.join(path);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, contents).unwrap();
    }
    fn session(vendor: &str, id: &str) -> Value {
        json!({"agent":vendor,"kind":"id","value":id})
    }
    const CODEX: &str = "{\"type\":\"session_meta\",\"payload\":{\"id\":\"child\",\"timestamp\":\"2026-01-01T00:00:00Z\"}}\n";
    const CLAUDE: &str = "{\"type\":\"user\",\"sessionId\":\"child\",\"parentUuid\":null,\"timestamp\":\"2026-01-01T00:00:00Z\"}\n";

    #[test]
    fn codex_session_meta_uses_matching_fork_identity_and_payload_timestamp() {
        let root = tempfile::tempdir().unwrap();
        transcript(root.path(), "codex/sessions/2026/01/01/rollout-child.jsonl", &format!(
            "{{\"type\":\"session_meta\",\"payload\":{{\"id\":\"parent\",\"timestamp\":\"2025-01-01T00:00:00Z\"}}}}\n{CODEX}{{\"timestamp\":\"2026-02-01T00:00:00Z\"}}\n"));
        assert_eq!(
            starts(root.path()).resolve(Some("codex"), &session("codex", "child")),
            Some(1767225600000)
        );
    }

    #[test]
    fn claude_requires_matching_non_sidechain_root_conversation_record() {
        let root = tempfile::tempdir().unwrap();
        let mut lines = Vec::new();
        for extra in [
            json!({"sessionId":"parent"}),
            json!({"parentUuid":"parent"}),
            json!({"isSidechain":true}),
            json!({"type":"progress"}),
        ] {
            let mut record: Value = serde_json::from_str(CLAUDE).unwrap();
            record["timestamp"] = json!("2025-01-01T00:00:00Z");
            record
                .as_object_mut()
                .unwrap()
                .extend(extra.as_object().unwrap().clone());
            lines.push(record.to_string());
        }
        transcript(
            root.path(),
            "claude/projects/project/child.jsonl",
            &(lines.join("\n") + "\n" + CLAUDE),
        );
        assert_eq!(
            starts(root.path()).resolve(Some("claude"), &session("claude", "child")),
            Some(1767225600000)
        );
    }

    #[test]
    fn default_configured_and_rtr_roots_resolve_for_both_vendors() {
        for (vendor, path, contents) in [
            ("codex", "home/.codex/sessions/child.jsonl", CODEX),
            (
                "claude",
                "home/.claude/projects/project/child.jsonl",
                CLAUDE,
            ),
            ("codex", "codex/sessions/child.jsonl", CODEX),
            ("claude", "claude/projects/project/child.jsonl", CLAUDE),
            (
                "codex",
                "state/rtr/homes/codex/nik/sessions/2026/01/01/rollout-child.jsonl",
                CODEX,
            ),
            (
                "claude",
                "state/rtr/homes/claude/nik/projects/project/child.jsonl",
                CLAUDE,
            ),
        ] {
            let root = tempfile::tempdir().unwrap();
            transcript(root.path(), path, contents);
            assert_eq!(
                starts(root.path()).resolve(Some(vendor), &session(vendor, "child")),
                Some(1767225600000),
                "{path}"
            );
        }
        let root = tempfile::tempdir().unwrap();
        transcript(
            root.path(),
            "home/.local/state/rtr/homes/codex/nik/sessions/child.jsonl",
            CODEX,
        );
        let mut fallback = Starts::from_env(|key| {
            (key == "HOME").then(|| root.path().join("home").into_os_string())
        });
        assert_eq!(
            fallback.resolve(Some("codex"), &session("codex", "child")),
            Some(1767225600000)
        );
    }

    #[test]
    fn uuid_v7_is_codex_only_and_transcript_evidence_wins() {
        let root = tempfile::tempdir().unwrap();
        let id = "01a10008-9e82-7000-8000-000000000001";
        let at = i64::from_str_radix("01a100089e82", 16).unwrap();
        assert_eq!(
            starts(root.path()).resolve(Some("codex"), &session("codex", id)),
            Some(at)
        );
        assert_eq!(
            starts(root.path()).resolve(Some("claude"), &session("claude", id)),
            None
        );
        transcript(
            root.path(),
            &format!("codex/sessions/rollout-{id}.jsonl"),
            &CODEX.replace("child", id),
        );
        assert_eq!(
            starts(root.path()).resolve(Some("codex"), &session("codex", id)),
            Some(1767225600000)
        );
        for invalid in [
            "01a10008-9e82-4000-8000-000000000001",
            "01a10008-9e82-7000-0000-000000000001",
            "01a10008-9e82-7000-8000-00000000000z",
            "missing",
        ] {
            assert_eq!(uuid_v7_start(invalid), None);
        }
    }

    #[test]
    fn unknown_or_untrusted_session_stays_unknown_not_mtime() {
        let root = tempfile::tempdir().unwrap();
        transcript(
            root.path(),
            "codex/sessions/child.jsonl",
            "{\"timestamp\":\"2026-01-01T00:00:00Z\"}\n",
        );
        let mut recovery = starts(root.path());
        for value in [
            session("codex", "child"),
            session("codex", "missing"),
            session("claude", "child"),
            json!({"agent":"codex","kind":"path","value":"child"}),
            session("codex", "../child"),
        ] {
            assert_eq!(recovery.resolve(Some("codex"), &value), None);
        }
        assert_eq!(recovery.resolve(None, &session("codex", "child")), None);
    }

    #[test]
    fn discovery_and_head_parsing_obey_depth_entry_and_byte_bounds() {
        let root = tempfile::tempdir().unwrap();
        transcript(root.path(), "codex/sessions/a/b/c/d/e/child.jsonl", CODEX);
        assert_eq!(
            starts(root.path()).resolve(Some("codex"), &session("codex", "child")),
            None
        );
        transcript(root.path(), "codex/sessions/child.jsonl", CODEX);
        let mut bounded = starts(root.path());
        bounded.entries_left = 0;
        assert_eq!(
            bounded.resolve(Some("codex"), &session("codex", "child")),
            None
        );
        let mut bounded = starts(root.path());
        bounded.bytes_left = 10;
        assert_eq!(
            bounded.resolve(Some("codex"), &session("codex", "child")),
            None
        );
        assert_eq!(bounded.bytes_left, 0);
        transcript(
            root.path(),
            "codex/sessions/child.jsonl",
            &(" ".repeat(HEAD_BYTES) + "\n" + CODEX),
        );
        assert_eq!(
            starts(root.path()).resolve(Some("codex"), &session("codex", "child")),
            None
        );
    }

    #[test]
    fn discovery_skips_symlinks_and_a_refresh_can_find_new_transcripts() {
        let root = tempfile::tempdir().unwrap();
        transcript(root.path(), "outside/child.jsonl", CODEX);
        fs::create_dir_all(root.path().join("codex/sessions")).unwrap();
        std::os::unix::fs::symlink(
            root.path().join("outside"),
            root.path().join("codex/sessions/link"),
        )
        .unwrap();
        let mut recovery = starts(root.path());
        assert_eq!(
            recovery.resolve(Some("codex"), &session("codex", "child")),
            None
        );
        transcript(root.path(), "codex/sessions/child.jsonl", CODEX);
        assert_eq!(
            recovery.resolve(Some("codex"), &session("codex", "child")),
            None
        );
        assert_eq!(
            starts(root.path()).resolve(Some("codex"), &session("codex", "child")),
            Some(1767225600000)
        );
    }

    #[test]
    fn label_boundaries_future_and_huge_ages() {
        for (elapsed, expected) in [
            (0, "0s"),
            (59_999, "59s"),
            (60_000, "1m"),
            (3_599_999, "59m"),
            (3_600_000, "1h"),
            (86_399_999, "23h"),
            (86_400_000, "1d"),
            (-1, "0s"),
            (9999 * 86_400_000, "9999d"),
            (10000 * 86_400_000, "9999d+"),
        ] {
            assert_eq!(label(1, 1 + elapsed), expected);
        }
        assert_eq!(label(i64::MIN, i64::MAX), "9999d+");
        assert!(now_ms() > 0);
    }

    #[test]
    fn many_first_line_starts_fit_within_a_small_shared_budget() {
        for (vendor, header, folder) in [
            ("claude", CLAUDE, "claude/projects"),
            ("codex", CODEX, "codex/sessions"),
        ] {
            let root = tempfile::tempdir().unwrap();
            for n in 0..80 {
                let id = format!("session-{n}");
                let contents = header.replace("child", &id) + &" ".repeat(64 * 1024);
                transcript(root.path(), &format!("{folder}/{id}.jsonl"), &contents);
            }
            let mut recovery = starts(root.path());
            recovery.bytes_left = 80 * 1024;
            for n in 0..80 {
                assert_eq!(
                    recovery.resolve(Some(vendor), &session(vendor, &format!("session-{n}"))),
                    Some(1767225600000),
                    "{vendor} session {n}"
                );
            }
        }
    }
}
