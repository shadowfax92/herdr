//! Copy hints, the text half: which text on a captured screen is worth
//! copying, and the short labels typed to pick it.
//!
//! This module knows regexes and labels, and nothing about cells, panes, keys
//! or the wire. `HintRules` holds the label alphabet and the patterns in
//! priority order. `HintRules::scan` turns the logical lines of one capture
//! into labeled targets whose positions are byte ranges into those lines. The
//! client's copy-mode hint sub-state builds the lines from a pane's cells and
//! maps the byte ranges back to cells.
//!
//! The rules:
//! - custom patterns come first, then the enabled built-ins, in that order;
//! - a named `match` group is the copied text and the label anchor, while the
//!   whole match is what gets underlined; empty captures are skipped;
//! - per line, candidates sort by (start, priority, longest first) and are
//!   swept greedily, so the leftmost match wins and a tie goes to the earlier
//!   pattern;
//! - equal texts collapse into one target, drawn at every occurrence;
//! - labels are prefix-free and as short as possible (k-ary Huffman over the
//!   alphabet), sorted by length and then alphabet order;
//! - the bottom-most target gets the shortest, earliest label, because new
//!   output, an agent's above all, arrives at the bottom of a pane.

use std::cmp::Reverse;
use std::collections::{BinaryHeap, HashMap, HashSet};
use std::ops::Range;

use crate::config::CopyHintsConfig;

/// The default alphabet: 17 keys, home row first.
pub(crate) const DEFAULT_HINT_ALPHABET: &str = "asdfwerzxcuioptbm";

/// tmux-fingers' built-in patterns in their default priority order, copied
/// verbatim, noise included (see the tests).
pub(crate) const BUILTIN_HINT_PATTERNS: &[(&str, &str)] = &[
    ("ip", r"\d{1,3}(?:\.\d{1,3}){3}"),
    (
        "uuid",
        r"[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}",
    ),
    ("sha", r"[0-9a-f]{7,128}"),
    ("digit", r"[0-9]{4,}"),
    (
        "url",
        r#"(?:https?://|git@|git://|ssh://|ftp://|file:///)[^\s()"']+"#,
    ),
    ("path", r"(?:[.\w\-~$@]+)?(?:/[.\w\-@]+)+/?"),
    ("hex", r"0x[0-9a-fA-F]+"),
    (
        "kubernetes",
        r"(?:binding|componentstatuses|configmap|endpoints?|events?|limitrange|namespaces?|nodes?|persistentvolumeclaims?|persistentvolumes?|pods?|podtemplates?|replicationcontrollers?|resourcequotas?|secrets?|serviceaccounts?|services?|daemonset\.apps|deployment\.apps|replicaset\.apps|statefulset\.apps|cronjob\.batch|job\.batch|ingress\.networking\.k8s\.io|networkpolicy\.networking\.k8s\.io|clusterrolebindings?\.rbac\.authorization\.k8s\.io|roles?\.rbac\.authorization\.k8s\.io|customresourcedefinitions?\.apiextensions\.k8s\.io)[[:alnum:]_#$%&+=/@.-]+",
    ),
    (
        "git-status",
        r"(?:modified|deleted|deleted by us|new file): +(?<match>.+)",
    ),
    (
        "git-status-branch",
        r"Your branch is up to date with '(?<match>.*)'\.",
    ),
    ("diff", r"(?:---|\+\+\+) [ab]/(?<match>.*)"),
];

/// `[copy_hints]` validated and compiled: everything one scan needs.
///
/// Never invalid. `compile` drops every entry that fails validation, with a
/// diagnostic, and falls back to the default alphabet, so `scan` cannot fail
/// and nothing at key time handles a config error.
#[derive(Clone, Debug)]
pub(crate) struct HintRules {
    alphabet: HintAlphabet,
    /// Priority order: a lower index wins a tie at the same start.
    patterns: Vec<regex::Regex>,
}

/// At least two distinct keys, each a lowercase ASCII letter or a digit, most
/// comfortable first.
///
/// Only `parse` and `default` build one, so `labels` never meets an alphabet
/// that cannot form a prefix-free code and cannot fail.
#[derive(Clone, Debug, PartialEq, Eq)]
struct HintAlphabet(Vec<char>);

/// One distinct value on the captured screen, the label that copies it, and
/// every place it appears.
///
/// Within one `scan` result the labels are unique and none is a prefix of
/// another, so a typed prefix names at most one target exactly.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct HintTarget {
    pub(crate) label: String,
    /// The `match` group's text, or the whole match without one. Never empty.
    pub(crate) text: String,
    /// Top to bottom, left to right.
    pub(crate) occurrences: Vec<HintOccurrence>,
}

/// Where one occurrence sits, as byte ranges into `lines[line]` of the `scan`
/// input. `full` is the whole match, which is underlined. `capture` is the
/// copied text and anchors the label. `full` contains `capture`, and `capture`
/// is never empty.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct HintOccurrence {
    pub(crate) line: usize,
    pub(crate) full: Range<usize>,
    pub(crate) capture: Range<usize>,
}

impl HintRules {
    /// Validates and compiles a `[copy_hints]` table. It never fails: each
    /// problem becomes one diagnostic and costs only the item it is in.
    /// - `alphabet`: absent gives the default; an invalid one is reported and
    ///   the default is used.
    /// - `patterns`: an entry without a usable regex string, or whose regex
    ///   does not compile, is reported and skipped; the others still apply.
    /// - `enabled_builtin_patterns`: absent enables every built-in in table
    ///   order. A list enables those names in its order; an unknown name is
    ///   reported and skipped, and a repeated one is ignored.
    ///
    /// Startup diagnostics (`Config::collect_diagnostics`), live-reload
    /// diagnostics and the client's rules all come from this one function.
    pub(crate) fn compile(config: &CopyHintsConfig) -> (Self, Vec<String>) {
        let mut diagnostics = Vec::new();
        let alphabet = match config.alphabet.as_deref() {
            None => HintAlphabet::default(),
            Some(keys) => HintAlphabet::parse(keys).unwrap_or_else(|why| {
                diagnostics.push(format!(
                    "copy_hints.alphabet = {keys:?} is invalid ({why}); using the default {DEFAULT_HINT_ALPHABET:?}"
                ));
                HintAlphabet::default()
            }),
        };

        let mut patterns = Vec::new();
        for source in config.patterns.sources(&mut diagnostics) {
            match regex::Regex::new(source.regex) {
                Ok(regex) => patterns.push(regex),
                Err(err) => {
                    let name = source
                        .name
                        .map(|name| format!(" {name:?}"))
                        .unwrap_or_default();
                    diagnostics.push(format!(
                        "copy_hints.patterns[{}]{name} has an invalid regex ({}); skipping pattern",
                        source.index,
                        regex_error_reason(&err)
                    ));
                }
            }
        }

        let enabled = match config.enabled_builtin_patterns.as_deref() {
            None => BUILTIN_HINT_PATTERNS
                .iter()
                .map(|(name, _)| *name)
                .collect(),
            Some(names) => names.iter().map(String::as_str).collect::<Vec<_>>(),
        };
        let mut seen = HashSet::new();
        for name in enabled.into_iter().filter(|name| seen.insert(*name)) {
            let Some((_, source)) = BUILTIN_HINT_PATTERNS
                .iter()
                .find(|(builtin, _)| *builtin == name)
            else {
                let known = BUILTIN_HINT_PATTERNS
                    .iter()
                    .map(|(name, _)| *name)
                    .collect::<Vec<_>>()
                    .join(", ");
                diagnostics.push(format!(
                    "copy_hints.enabled_builtin_patterns has unknown pattern {name:?} (known: {known}); skipping it"
                ));
                continue;
            };
            // The built-ins go through the same fallible compile as custom
            // patterns, so the table needs no unwrap; a test keeps this arm
            // unreachable.
            match regex::Regex::new(source) {
                Ok(regex) => patterns.push(regex),
                Err(err) => diagnostics.push(format!(
                    "built-in copy hint pattern {name:?} does not compile ({}); skipping it",
                    regex_error_reason(&err)
                )),
            }
        }
        (Self { alphabet, patterns }, diagnostics)
    }

    /// Matches every line, collapses equal texts into one target, and labels
    /// the targets bottom-most first: by each target's last occurrence, the
    /// lowest line wins and then the rightmost start. Pure and deterministic.
    ///
    /// Cost: every pattern runs over every line once. The client calls this
    /// once per hint session, never while drawing a frame.
    pub(crate) fn scan<S: AsRef<str>>(&self, lines: &[S]) -> Vec<HintTarget> {
        let mut targets: Vec<(String, Vec<HintOccurrence>)> = Vec::new();
        let mut by_text: HashMap<String, usize> = HashMap::new();
        for (line_index, line) in lines.iter().enumerate() {
            let line = line.as_ref();
            for (full, capture) in sweep_line(line, &self.patterns) {
                let text = &line[capture.clone()];
                let occurrence = HintOccurrence {
                    line: line_index,
                    full,
                    capture,
                };
                match by_text.get(text) {
                    Some(&index) => targets[index].1.push(occurrence),
                    None => {
                        by_text.insert(text.to_owned(), targets.len());
                        targets.push((text.to_owned(), vec![occurrence]));
                    }
                }
            }
        }
        targets.sort_by_key(|(_, occurrences)| {
            Reverse(
                occurrences
                    .last()
                    .map(|occurrence| (occurrence.line, occurrence.full.start)),
            )
        });
        labels(&self.alphabet, targets.len())
            .into_iter()
            .zip(targets)
            .map(|(label, (text, occurrences))| HintTarget {
                label,
                text,
                occurrences,
            })
            .collect()
    }
}

impl HintAlphabet {
    fn parse(keys: &str) -> Result<Self, String> {
        let mut parsed = Vec::new();
        for key in keys.chars() {
            if !key.is_ascii_lowercase() && !key.is_ascii_digit() {
                return Err(format!("{key:?} is not a lowercase letter or a digit"));
            }
            if parsed.contains(&key) {
                return Err(format!("{key:?} appears twice"));
            }
            parsed.push(key);
        }
        if parsed.len() < 2 {
            return Err("needs at least two keys".to_owned());
        }
        Ok(Self(parsed))
    }
}

impl Default for HintAlphabet {
    fn default() -> Self {
        Self(DEFAULT_HINT_ALPHABET.chars().collect())
    }
}

/// The reason line of a regex error. Its `Display` spans several lines (the
/// pattern, a caret, then the reason), and a config diagnostic is one line.
fn regex_error_reason(err: &regex::Error) -> String {
    let message = err.to_string();
    let reason = message
        .lines()
        .rev()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .unwrap_or_default();
    reason.strip_prefix("error: ").unwrap_or(reason).to_owned()
}

/// Non-overlapping `(full, capture)` byte ranges of one line, leftmost first.
/// Gathers every pattern's matches, sorts them by (start, priority, longest
/// first), then keeps each one that starts at or after the end of the last one
/// kept.
fn sweep_line(line: &str, patterns: &[regex::Regex]) -> Vec<(Range<usize>, Range<usize>)> {
    let mut candidates = Vec::new();
    for (priority, pattern) in patterns.iter().enumerate() {
        for captures in pattern.captures_iter(line) {
            let Some(full) = captures.get(0) else {
                continue;
            };
            let capture = captures.name("match").unwrap_or(full);
            if capture.is_empty() {
                continue;
            }
            candidates.push((priority, full.range(), capture.range()));
        }
    }
    candidates.sort_by_key(|(priority, full, _)| (full.start, *priority, Reverse(full.end)));
    let mut kept = Vec::new();
    let mut occupied_until = 0;
    for (_, full, capture) in candidates {
        if full.start < occupied_until {
            continue;
        }
        occupied_until = full.end;
        kept.push((full, capture));
    }
    kept
}

/// `count` prefix-free labels over `alphabet`, shortest first, then in
/// alphabet order. Up to one label per key, each label is a single key in
/// alphabet order. Beyond that, a k-ary Huffman tree over equal weights,
/// padded with zero-weight leaves so every branch has a child per key, gives
/// the shortest prefix-free code.
fn labels(alphabet: &HintAlphabet, count: usize) -> Vec<String> {
    enum Node {
        Padding,
        Label,
        Branch(Vec<usize>),
    }

    let keys = &alphabet.0;
    if count <= keys.len() {
        return keys.iter().take(count).map(char::to_string).collect();
    }
    let arity = keys.len();
    let padding = (arity - 1 - (count - 1) % (arity - 1)) % (arity - 1);
    let mut nodes = Vec::new();
    // Entries are (weight, node index); the index breaks weight ties in
    // insertion order, which keeps the labels deterministic.
    let mut heap = BinaryHeap::new();
    for leaf in 0..padding + count {
        let (weight, node) = if leaf < padding {
            (0, Node::Padding)
        } else {
            (1, Node::Label)
        };
        heap.push(Reverse((weight, nodes.len())));
        nodes.push(node);
    }
    while heap.len() > 1 {
        let mut weight = 0;
        let mut children = Vec::with_capacity(arity);
        while children.len() < arity {
            let Some(Reverse((child_weight, child))) = heap.pop() else {
                break;
            };
            weight += child_weight;
            children.push(child);
        }
        heap.push(Reverse((weight, nodes.len())));
        nodes.push(Node::Branch(children));
    }
    let Some(Reverse((_, root))) = heap.pop() else {
        return Vec::new();
    };

    let mut paths: Vec<Vec<usize>> = Vec::with_capacity(count);
    let mut pending = vec![(root, Vec::new())];
    while let Some((node, path)) = pending.pop() {
        match &nodes[node] {
            Node::Padding => {}
            Node::Label => paths.push(path),
            Node::Branch(children) => {
                for (key, &child) in children.iter().enumerate() {
                    let mut child_path = path.clone();
                    child_path.push(key);
                    pending.push((child, child_path));
                }
            }
        }
    }
    paths.sort_by(|left, right| left.len().cmp(&right.len()).then_with(|| left.cmp(right)));
    paths
        .into_iter()
        .map(|path| path.into_iter().map(|key| keys[key]).collect())
        .collect()
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use super::*;

    /// The owner's custom patterns, byte for byte.
    const OWNER_PATTERNS: [&str; 5] = [
        r"git@[a-zA-Z0-9.-]+:[a-zA-Z0-9/.-]+\.git",
        "[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}",
        "[0-9a-f]{7,40}",
        r"/[^ \t\r\n]+:[0-9]+",
        r"(?i)\b(?:[a-z0-9](?:[a-z0-9-]{0,61}[a-z0-9])?\.)+[a-z]{2,63}\b",
    ];

    fn rules(patterns: &[&str]) -> HintRules {
        HintRules {
            alphabet: HintAlphabet::default(),
            patterns: patterns
                .iter()
                .map(|pattern| regex::Regex::new(pattern).expect("test pattern"))
                .collect(),
        }
    }

    fn builtin(name: &str) -> &'static str {
        BUILTIN_HINT_PATTERNS
            .iter()
            .find(|(builtin, _)| *builtin == name)
            .map(|(_, source)| *source)
            .expect("known built-in")
    }

    /// Custom patterns first, then every built-in, as an absent
    /// `enabled_builtin_patterns` gives.
    fn with_builtins(custom: &[&str]) -> HintRules {
        let all = custom
            .iter()
            .copied()
            .chain(BUILTIN_HINT_PATTERNS.iter().map(|(_, source)| *source))
            .collect::<Vec<_>>();
        rules(&all)
    }

    fn texts(targets: &[HintTarget]) -> Vec<&str> {
        targets.iter().map(|target| target.text.as_str()).collect()
    }

    fn labeled(targets: &[HintTarget]) -> Vec<(&str, &str)> {
        targets
            .iter()
            .map(|target| (target.label.as_str(), target.text.as_str()))
            .collect()
    }

    #[test]
    fn named_capture_controls_copied_text_and_label_anchor() {
        let targets = rules(&[r"modified: (?<match>.+)"]).scan(&["modified: src/main.rs"]);

        assert_eq!(texts(&targets), ["src/main.rs"]);
        assert_eq!(
            targets[0].occurrences,
            [HintOccurrence {
                line: 0,
                full: 0..21,
                capture: 10..21,
            }]
        );
    }

    #[test]
    fn earlier_patterns_win_same_start_and_leftmost_matches_win_overlap() {
        let targets = rules(&[r"abc(?<match>[0-9]+)", r"abc123", r"123 xyz"]).scan(&["abc123 xyz"]);

        assert_eq!(texts(&targets), ["123"]);
    }

    #[test]
    fn repeated_text_shares_one_target_with_every_occurrence() {
        let targets = rules(&["[0-9a-f]{8}"]).scan(&["deadbeef x", "deadbeef y"]);

        assert_eq!(texts(&targets), ["deadbeef"]);
        let lines = targets[0]
            .occurrences
            .iter()
            .map(|occurrence| occurrence.line)
            .collect::<Vec<_>>();
        assert_eq!(lines, [0, 1]);
    }

    #[test]
    fn empty_captures_are_ignored() {
        assert!(rules(&[r"(?<match>)abc"]).scan(&["abc"]).is_empty());
    }

    #[test]
    fn byte_ranges_stay_exact_on_long_logical_lines() {
        let line = format!("{} /tmp/a", "x".repeat(70_000));
        let targets = rules(&["/[^ ]+"]).scan(&[line]);

        assert_eq!(targets[0].occurrences[0].capture, 70_001..70_007);
    }

    #[test]
    fn every_builtin_matches_its_representative_value() {
        let cases = [
            ("ip", "10.0.0.1", "10.0.0.1"),
            (
                "uuid",
                "id 123e4567-e89b-12d3-a456-426614174000",
                "123e4567-e89b-12d3-a456-426614174000",
            ),
            ("sha", "commit deadbeef1", "deadbeef1"),
            ("digit", "ticket 1234", "1234"),
            ("url", "https://herdr.dev/docs/", "https://herdr.dev/docs/"),
            ("path", "./src/main.rs", "./src/main.rs"),
            ("hex", "0xDEAD", "0xDEAD"),
            ("kubernetes", "deployment.apps/api", "deployment.apps/api"),
            ("git-status", "modified: src/lib.rs", "src/lib.rs"),
            (
                "git-status-branch",
                "Your branch is up to date with 'main'.",
                "main",
            ),
            ("diff", "+++ b/src/main.rs", "src/main.rs"),
        ];
        assert_eq!(
            cases.iter().map(|(name, _, _)| *name).collect::<Vec<_>>(),
            BUILTIN_HINT_PATTERNS
                .iter()
                .map(|(name, _)| *name)
                .collect::<Vec<_>>(),
            "one case per built-in, in table order"
        );
        for (name, input, expected) in cases {
            let targets = rules(&[builtin(name)]).scan(&[input]);
            assert_eq!(texts(&targets), [expected], "pattern {name}");
        }
    }

    #[test]
    fn the_owners_patterns_label_a_realistic_screen_bottom_first() {
        let screen = [
            "⏺ Update(/Users/nik/herdr/src/copy_hints.rs)",
            "814366bf fix(copy-mode): page fullscreen apps with the copy-mode page keys",
            "Opened https://github.com/herdrdev/herdr/pull/412 in your browser",
            "HEAD 814366bf · edited /Users/nik/herdr/src/main.rs:88",
            "remote: git@github.com:herdrdev/herdr.git",
            "session 123e4567-e89b-12d3-a456-426614174000 resumed",
            "  • atlas-node.example.test - running (example/runtime)",
        ];

        let targets = with_builtins(&OWNER_PATTERNS).scan(&screen);

        assert_eq!(
            labeled(&targets),
            [
                ("a", "example/runtime"),
                // hostname, ahead of the built-ins
                ("s", "atlas-node.example.test"),
                // user-uuid beats user-sha at the same start
                ("d", "123e4567-e89b-12d3-a456-426614174000"),
                // git-ssh beats the url built-in at the same start
                ("f", "git@github.com:herdrdev/herdr.git"),
                // file-line keeps the line number the path built-in drops
                ("w", "/Users/nik/herdr/src/main.rs:88"),
                // user-sha; its bottom-most occurrence decides its label
                ("e", "814366bf"),
                ("r", "https://github.com/herdrdev/herdr/pull/412"),
                // a Claude Code tool line: the path built-in
                ("z", "/Users/nik/herdr/src/copy_hints.rs"),
            ]
        );
        let sha = targets
            .iter()
            .find(|target| target.text == "814366bf")
            .expect("sha target");
        assert_eq!(sha.occurrences.len(), 2, "one label for both lines");
    }

    #[test]
    fn the_bottom_most_target_gets_the_shortest_label() {
        let targets = rules(&["/[a-z]+"]).scan(&["/top", "/both /middle", "/right /both"]);

        assert_eq!(
            labeled(&targets),
            [
                ("a", "/both"),
                ("s", "/right"),
                ("d", "/middle"),
                ("f", "/top")
            ]
        );
    }

    #[test]
    fn with_more_targets_than_keys_the_top_rows_get_two_key_labels() {
        let screen = (0..18).map(|row| format!("/p/{row}")).collect::<Vec<_>>();

        let targets = rules(&["/p/[0-9]+"]).scan(&screen);

        let by_row = |row: usize| {
            targets
                .iter()
                .find(|target| target.text == format!("/p/{row}"))
                .map(|target| target.label.as_str())
        };
        assert_eq!(by_row(17), Some("a"));
        assert_eq!(by_row(16), Some("s"));
        assert_eq!(by_row(2), Some("b"));
        assert_eq!(by_row(1), Some("mb"));
        assert_eq!(by_row(0), Some("mm"));
    }

    fn compiled(toml: &str) -> (HintRules, Vec<String>) {
        let config: crate::config::Config = toml::from_str(toml).expect("config parses");
        HintRules::compile(&config.copy_hints)
    }

    #[test]
    fn an_absent_table_compiles_every_builtin_without_diagnostics() {
        let (rules, diagnostics) = HintRules::compile(&CopyHintsConfig::default());

        assert!(diagnostics.is_empty(), "{diagnostics:?}");
        assert_eq!(rules.alphabet, HintAlphabet::default());
        assert_eq!(rules.patterns.len(), BUILTIN_HINT_PATTERNS.len());
    }

    #[test]
    fn the_owners_table_compiles_cleanly_and_custom_patterns_win_ties() {
        let (rules, diagnostics) = compiled(
            r#"
[[copy_hints.patterns]]
name = "git-ssh"
regex = 'git@[a-zA-Z0-9.-]+:[a-zA-Z0-9/.-]+\.git'

[[copy_hints.patterns]]
name = "user-uuid"
regex = "[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}"

[[copy_hints.patterns]]
name = "user-sha"
regex = "[0-9a-f]{7,40}"

[[copy_hints.patterns]]
name = "file-line"
regex = '/[^ \t\r\n]+:[0-9]+'

[[copy_hints.patterns]]
name = "hostname"
regex = '(?i)\b(?:[a-z0-9](?:[a-z0-9-]{0,61}[a-z0-9])?\.)+[a-z]{2,63}\b'
"#,
        );

        assert!(diagnostics.is_empty(), "{diagnostics:?}");
        let targets = rules.scan(&["   HEAD 814366bf · edited /Users/nik/herdr/src/main.rs:88"]);
        assert_eq!(
            labeled(&targets),
            [("a", "/Users/nik/herdr/src/main.rs:88"), ("s", "814366bf")]
        );
    }

    #[test]
    fn a_bad_pattern_is_reported_and_the_others_still_match() {
        let (rules, diagnostics) = compiled(
            r#"
[copy_hints]
enabled_builtin_patterns = []

[[copy_hints.patterns]]
name = "ticket"
regex = 'TKT-[0-9]+'

[[copy_hints.patterns]]
name = "broken"
regex = '(oops'

[[copy_hints.patterns]]
regex = 'PR-[0-9]+'

[[copy_hints.patterns]]
regex = '[unclosed'
"#,
        );

        assert_eq!(
            diagnostics,
            [
                "copy_hints.patterns[1] \"broken\" has an invalid regex (unclosed group); skipping pattern",
                "copy_hints.patterns[3] has an invalid regex (unclosed character class); skipping pattern",
            ]
        );
        assert_eq!(
            texts(&rules.scan(&["see TKT-1 and PR-2 (oops"])),
            ["PR-2", "TKT-1"]
        );
    }

    #[test]
    fn unknown_builtins_and_bad_alphabets_fall_back_with_diagnostics() {
        let (rules, diagnostics) = compiled(
            r#"
[copy_hints]
alphabet = "aa"
enabled_builtin_patterns = ["url", "nope", "url"]
"#,
        );

        assert_eq!(
            diagnostics,
            [
                "copy_hints.alphabet = \"aa\" is invalid ('a' appears twice); using the default \"asdfwerzxcuioptbm\"",
                "copy_hints.enabled_builtin_patterns has unknown pattern \"nope\" (known: ip, uuid, sha, digit, url, path, hex, kubernetes, git-status, git-status-branch, diff); skipping it",
            ]
        );
        assert_eq!(rules.alphabet, HintAlphabet::default());
        assert_eq!(rules.patterns.len(), 1, "url, once");
        assert_eq!(
            texts(&rules.scan(&["https://herdr.dev /tmp/a"])),
            ["https://herdr.dev"]
        );
    }

    #[test]
    fn a_custom_alphabet_labels_the_targets() {
        let (rules, diagnostics) = compiled("[copy_hints]\nalphabet = \"jk\"\n");

        assert!(diagnostics.is_empty(), "{diagnostics:?}");
        assert_eq!(
            labeled(&rules.scan(&["/a/x", "/b/y", "/c/z"])),
            [("j", "/c/z"), ("kj", "/b/y"), ("kk", "/a/x")]
        );
    }

    /// The docs list the built-in names a user can enable; the table here is
    /// the one source, and this keeps the two from drifting.
    #[test]
    fn the_config_reference_lists_every_builtin_in_order() {
        let reference: serde_json::Value = serde_json::from_str(include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/docs/next/website/src/data/config-reference.json"
        )))
        .expect("config reference parses");
        let entry = reference["sections"]
            .as_array()
            .into_iter()
            .flatten()
            .flat_map(|section| section["keys"].as_array().into_iter().flatten())
            .find(|entry| entry["key"] == "copy_hints.enabled_builtin_patterns")
            .expect("copy_hints.enabled_builtin_patterns is documented");
        let documented = entry["values"]
            .as_array()
            .expect("values")
            .iter()
            .map(|value| value.as_str().expect("name"))
            .collect::<Vec<_>>();

        assert_eq!(
            documented,
            BUILTIN_HINT_PATTERNS
                .iter()
                .map(|(name, _)| *name)
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn small_sets_use_one_key_in_alphabet_order() {
        let alphabet = HintAlphabet::parse("asd").expect("alphabet");
        assert_eq!(labels(&alphabet, 3), ["a", "s", "d"]);
    }

    #[test]
    fn larger_sets_are_unique_prefix_free_and_short() {
        let alphabet = HintAlphabet::parse("asd").expect("alphabet");
        let generated = labels(&alphabet, 5);

        assert_eq!(generated.iter().collect::<HashSet<_>>().len(), 5);
        assert!(generated.iter().any(|label| label.len() == 1));
        assert!(generated.iter().any(|label| label.len() == 2));
        for left in &generated {
            for right in &generated {
                assert!(left == right || !right.starts_with(left.as_str()));
            }
        }
    }

    #[test]
    fn eighteen_targets_on_the_default_alphabet_take_two_two_key_labels() {
        let generated = labels(&HintAlphabet::default(), 18);
        let one_key = DEFAULT_HINT_ALPHABET[..16]
            .chars()
            .map(String::from)
            .collect::<Vec<_>>();

        assert_eq!(generated[..16], one_key[..]);
        assert_eq!(generated[16..], ["mb", "mm"]);
    }

    #[test]
    fn generation_is_deterministic() {
        let alphabet = HintAlphabet::parse("asdf").expect("alphabet");
        assert_eq!(labels(&alphabet, 20), labels(&alphabet, 20));
    }

    #[test]
    fn invalid_alphabets_are_refused_and_zero_targets_are_empty() {
        assert!(HintAlphabet::parse("a").is_err());
        assert!(HintAlphabet::parse("aa").is_err());
        assert!(HintAlphabet::parse("aB").is_err());
        assert!(HintAlphabet::parse("a s").is_err());
        assert_eq!(
            HintAlphabet::parse("a1"),
            Ok(HintAlphabet(vec!['a', '1'])),
            "digits are keys too"
        );
        let alphabet = HintAlphabet::parse("as").expect("alphabet");
        assert!(labels(&alphabet, 0).is_empty());
    }
}
