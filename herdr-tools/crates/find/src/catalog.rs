//! A flat view of live pane metadata. Pane and terminal identity travel together
//! from discovery to navigation, independent of titles, status, or sidebar paint.
//! Search prepares text at refresh time, then uses accepted alignments for both
//! relevance and grapheme highlights so a hidden-field match has an honest reason.
use std::collections::{HashMap, HashSet};
use std::path::Path;

use crate::matcher::{Field, Haystack, Matcher, Query, TextMatch};
use anyhow::{bail, Context, Result};
use serde::Deserialize;
#[cfg(test)]
use unicode_segmentation::UnicodeSegmentation;

/// Agent state as herdr reports it; `Ord` is needs-you-first priority.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub(crate) enum AgentState {
    Blocked,
    Done,
    Working,
    Idle,
    Neutral,
}
impl AgentState {
    pub fn from_host(agent: Option<&str>, status: &str) -> Self {
        if agent.is_none_or(str::is_empty) {
            return Self::Neutral;
        }
        match status {
            "blocked" => Self::Blocked,
            "done" => Self::Done,
            "working" => Self::Working,
            "idle" => Self::Idle,
            _ => Self::Neutral,
        }
    }
    #[allow(dead_code)] // The P2 renderer uses herdr's vocabulary directly.
    pub fn word(self) -> &'static str {
        match self {
            Self::Blocked => "blocked",
            Self::Done => "done",
            Self::Working => "working",
            Self::Idle => "idle",
            Self::Neutral => "",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
/// One open pane, its visible presentation, and search-only aliases. Identity
/// remains independent of the mutable labels users search for.
pub(crate) struct Candidate {
    pub pane_id: String,
    pub terminal_id: String,
    pub tab_id: String,
    pub workspace_id: String,
    pub workspace: String,
    pub tab: String,
    pub title: String,
    /// Pane label, falling back to the agent name when the label is absent.
    pub name: String,
    /// Foreground cwd, falling back to cwd, with HOME collapsed for display.
    pub path: String,
    pub detail: String,
    pub cwd: String,
    pub state: AgentState,
    /// Session start in Unix milliseconds; absent for neutral or unknown sessions.
    pub started_at: Option<i64>,
    search_fields: Vec<SearchField>,
    detail_search: Haystack,
}

/// Which hidden field explains a match the row can't otherwise show.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum ReasonKind {
    /// Pane label, agent name, or matching hidden alternate title. Alternate
    /// titles use this display payload while retaining Title-kind ranking.
    Name,
    Path,
}

/// A hidden field shown on the row only because the query matched it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Reason {
    pub kind: ReasonKind,
    /// Display text: a pane name, hidden alternate title, or `~`-collapsed path.
    pub text: String,
    /// Grapheme indices into `text`, sorted and deduped like `title_indices`.
    pub indices: Vec<u32>,
}

#[derive(Debug)]
/// A ranked catalog reference with grapheme offsets for visible match marks.
///
/// Whether the pane name is visible depends on the popup's width, which only the
/// renderer knows, so a hit carries reasons for both layouts and the renderer
/// picks one: `reasons` when rows have no Name column, `reasons_beside_name`
/// when they do.
pub(crate) struct Hit {
    pub index: usize,
    pub title_indices: Vec<u32>,
    pub workspace_indices: Vec<u32>,
    pub tab_indices: Vec<u32>,
    /// Matched letters of `Candidate.name`, for the Name column.
    pub name_indices: Vec<u32>,
    pub detail_indices: Vec<u32>,
    /// Hidden fields that explain this row, at most one per kind, ordered Name then Path.
    /// Empty when the visible fields explain every atom, and always empty for an empty query.
    /// Here the pane name counts as hidden, so a match in it gets a Name chip.
    pub reasons: Vec<Reason>,
    /// The same with the pane name visible in its column: atoms the name matched
    /// need no chip, and a remaining Name chip is a second name or alternate title.
    pub reasons_beside_name: Vec<Reason>,
}

#[derive(Deserialize)]
struct Snapshot {
    workspaces: Vec<Workspace>,
    tabs: Vec<Tab>,
    panes: Vec<Pane>,
    #[serde(default)]
    agents: Vec<Agent>,
}

#[derive(Deserialize)]
struct Workspace {
    workspace_id: String,
    #[serde(default)]
    label: String,
    #[serde(default)]
    number: u32,
}

#[derive(Deserialize)]
struct Tab {
    tab_id: String,
    workspace_id: String,
    #[serde(default)]
    label: String,
    #[serde(default)]
    number: u32,
}

/// This is the host identity contract used again immediately before navigation.
/// A pane ID alone is insufficient when the terminal occupying it was replaced.
#[derive(Clone, Deserialize)]
pub(crate) struct Pane {
    pub pane_id: String,
    pub terminal_id: String,
    pub tab_id: String,
    pub workspace_id: String,
    #[serde(default)]
    label: Option<String>,
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    terminal_title_stripped: Option<String>,
    #[serde(default)]
    cwd: Option<String>,
    #[serde(default)]
    foreground_cwd: Option<String>,
    #[serde(default)]
    agent: Option<String>,
    #[serde(default)]
    agent_status: String,
    #[serde(default)]
    started_at: Option<i64>,
}

#[derive(Deserialize)]
struct Agent {
    pane_id: String,
    #[serde(default)]
    name: Option<String>,
}

#[cfg(test)]
pub(crate) fn catalog(value: serde_json::Value, source: &str) -> Result<Vec<Candidate>> {
    catalog_with_own(value, source, "")
}

pub(crate) fn catalog_with_own(
    value: serde_json::Value,
    source: &str,
    own: &str,
) -> Result<Vec<Candidate>> {
    let snapshot: Snapshot = serde_json::from_value(value).context("invalid pane catalog")?;
    let home = std::env::var("HOME").ok();
    let workspaces: HashMap<_, _> = snapshot
        .workspaces
        .iter()
        .map(|w| (w.workspace_id.as_str(), w))
        .collect();
    let tabs: HashMap<_, _> = snapshot
        .tabs
        .iter()
        .map(|t| (t.tab_id.as_str(), t))
        .collect();
    if workspaces.len() != snapshot.workspaces.len() || tabs.len() != snapshot.tabs.len() {
        bail!("ambiguous workspace or tab identity");
    }
    let aliases: HashMap<_, _> = snapshot
        .agents
        .iter()
        .map(|a| (a.pane_id.as_str(), a.name.as_deref().unwrap_or_default()))
        .collect();
    let source_workspace = snapshot
        .panes
        .iter()
        .find(|p| p.pane_id == source)
        .map(|p| p.workspace_id.as_str());
    let mut seen = HashSet::new();
    let mut terminals = HashSet::new();
    let mut rows = Vec::new();
    for (order, pane) in snapshot.panes.iter().enumerate() {
        if [
            &pane.pane_id,
            &pane.terminal_id,
            &pane.tab_id,
            &pane.workspace_id,
        ]
        .iter()
        .any(|id| id.is_empty() || id.chars().any(char::is_control))
            || !seen.insert(&pane.pane_id)
            || !terminals.insert(&pane.terminal_id)
        {
            bail!("ambiguous or invalid pane identity");
        }
        let workspace = workspaces
            .get(pane.workspace_id.as_str())
            .context("pane workspace disappeared")?;
        let tab = tabs
            .get(pane.tab_id.as_str())
            .context("pane tab disappeared")?;
        if tab.workspace_id != pane.workspace_id {
            bail!("pane and tab workspace disagree");
        }
        // A tab-placed picker is present in snapshots, unlike native popups.
        // It must never become its own navigation or kill target.
        if pane.pane_id == own {
            continue;
        }
        let state = AgentState::from_host(pane.agent.as_deref(), &pane.agent_status);
        let cwd = clean(
            pane.foreground_cwd
                .as_deref()
                .or(pane.cwd.as_deref())
                .unwrap_or_default(),
        );
        let project = Path::new(&cwd)
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or_default();
        let terminal_title = clean(pane.terminal_title_stripped.as_deref().unwrap_or_default());
        let reported_title = clean(pane.title.as_deref().unwrap_or_default());
        let label = clean(pane.label.as_deref().unwrap_or_default());
        let alias = clean(
            aliases
                .get(pane.pane_id.as_str())
                .copied()
                .unwrap_or_default(),
        );
        let name = if label.is_empty() { &alias } else { &label }.clone();
        // Preserve the raw cwd for navigation details; searchable/display paths
        // abbreviate HOME so a shared home prefix cannot create noisy matches.
        let foreground = clean(pane.foreground_cwd.as_deref().unwrap_or_default());
        let path = display_path(
            &if foreground.is_empty() {
                clean(pane.cwd.as_deref().unwrap_or_default())
            } else {
                foreground
            },
            home.as_deref(),
        );
        // Hook-reported task metadata outranks a generic OSC terminal title.
        // Both remain searchable even when only the task title is displayed.
        let preferred_terminal = if terminal_title == project {
            ""
        } else {
            terminal_title.as_str()
        };
        let title = [
            reported_title.as_str(),
            preferred_terminal,
            label.as_str(),
            alias.as_str(),
            terminal_title.as_str(),
            project,
        ]
        .into_iter()
        .find(|s| !s.is_empty())
        .unwrap_or(&pane.pane_id)
        .to_owned();
        let workspace_name = clean(&workspace.label);
        let workspace_name = if workspace_name.is_empty() {
            workspace.workspace_id.clone()
        } else {
            workspace_name
        };
        let tab_name = clean(&tab.label);
        let tab_name = if tab_name.is_empty() {
            if tab.number > 0 {
                format!("Tab {}", tab.number)
            } else {
                tab.tab_id.clone()
            }
        } else {
            tab_name
        };
        let kind = pane.agent.as_deref().unwrap_or("terminal");
        let detail = clean(&format!(
            "{} › {} · {} · {} {}",
            workspace_name, tab_name, project, kind, pane.agent_status
        ));
        // Order within a kind is display preference: the pane label before an
        // agent alias, and the foreground directory before the launch directory.
        // `name` is whichever of the two is shown, so only it gets the column.
        let alias_shown = if label.is_empty() {
            Shown::NameColumn
        } else {
            Shown::Never
        };
        let mut search_fields = Vec::new();
        for (kind, shown, text) in [
            (FieldKind::Title, Shown::Always, title.clone()),
            (FieldKind::Title, Shown::Never, reported_title),
            (FieldKind::Title, Shown::Never, terminal_title),
            (FieldKind::Name, Shown::NameColumn, label),
            (FieldKind::Name, alias_shown, alias),
            (FieldKind::Tab, Shown::Always, tab_name.clone()),
            (FieldKind::Workspace, Shown::Always, workspace_name.clone()),
            (FieldKind::Path, Shown::Never, path.clone()),
            (
                FieldKind::Path,
                Shown::Never,
                display_path(
                    &clean(pane.cwd.as_deref().unwrap_or_default()),
                    home.as_deref(),
                ),
            ),
        ] {
            if !text.is_empty()
                && !search_fields
                    .iter()
                    .any(|field: &SearchField| field.kind == kind && field.value.text() == text)
            {
                search_fields.push(SearchField {
                    kind,
                    shown,
                    value: Haystack::new(text),
                });
            }
        }
        let detail_search = Haystack::new(detail.clone());
        let candidate = Candidate {
            pane_id: pane.pane_id.clone(),
            terminal_id: pane.terminal_id.clone(),
            tab_id: pane.tab_id.clone(),
            workspace_id: pane.workspace_id.clone(),
            workspace: workspace_name,
            tab: tab_name,
            title,
            name,
            path,
            detail,
            cwd,
            state,
            started_at: pane
                .started_at
                .filter(|at| *at > 0 && state != AgentState::Neutral),
            search_fields,
            detail_search,
        };
        rows.push((
            (
                Some(pane.workspace_id.as_str()) != source_workspace,
                workspace.number,
                tab.number,
                order,
            ),
            candidate,
        ));
    }
    rows.sort_by_key(|(key, _)| *key);
    Ok(rows.into_iter().map(|(_, candidate)| candidate).collect())
}

/// Compare path components so HOME never abbreviates a similarly named sibling.
/// This is lexical display formatting: no filesystem access or symlink resolution.
pub(crate) fn display_path(cwd: &str, home: Option<&str>) -> String {
    let Some(home) = home.filter(|home| !home.is_empty()) else {
        return cwd.to_owned();
    };
    let Ok(relative) = Path::new(cwd).strip_prefix(home) else {
        return cwd.to_owned();
    };
    if relative.as_os_str().is_empty() {
        "~".to_owned()
    } else {
        format!("~/{}", relative.display())
    }
}

#[cfg(test)]
pub(crate) fn search(rows: &[Candidate], query: &str) -> Vec<Hit> {
    search_view(rows, query, true, |_| true)
}

/// The field order is part of the non-empty-query ranking contract.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum FieldKind {
    Title,
    Name,
    Tab,
    Workspace,
    Path,
}

/// Search identity and presentation are separate: alternate titles can match,
/// while only the fields rows draw receive visible highlights.
#[derive(Clone, Debug, PartialEq, Eq)]
struct SearchField {
    kind: FieldKind,
    shown: Shown,
    value: Haystack,
}

/// Where rows draw a field. A match in a drawn field is highlighted in place;
/// an atom that matched only undrawn fields is explained by a reason chip.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Shown {
    /// The displayed title, tab and workspace, drawn on every row.
    Always,
    /// The displayed pane name, drawn in the Name column that narrow popups drop.
    NameColumn,
    /// Alternate titles, a second name and paths, drawn only as reason chips.
    Never,
}

/// Accepted hidden fields for one atom. Retain all choices until the whole
/// query is known: a launch path may explain more atoms than the foreground cwd.
struct HiddenAtom<'a> {
    fields: Vec<(&'a SearchField, TextMatch)>,
}

impl HiddenAtom<'_> {
    fn get(&self, field: &SearchField) -> Option<&TextMatch> {
        // Matches borrow the exact catalog fields, so identity does not depend
        // on equal text appearing in different field kinds.
        self.fields
            .iter()
            .find(|(candidate, _)| std::ptr::eq(*candidate, field))
            .map(|(_, found)| found)
    }

    /// The Name column explains this atom whenever it is on screen.
    fn names_the_pane(&self) -> bool {
        self.fields
            .iter()
            .any(|(field, _)| field.shown == Shown::NameColumn)
    }
}

/// Select at most one Name and one Path together, maximizing explained atoms.
/// Name wins when both selected chips explain an atom; display preferences only
/// break coverage ties, never lock the explanation to the query's first word.
fn hidden_reasons(row: &Candidate, atoms: &[&HiddenAtom<'_>]) -> Vec<Reason> {
    if atoms.is_empty() {
        return Vec::new();
    }
    let names: Vec<_> = row
        .search_fields
        .iter()
        .filter(|field| field.kind == FieldKind::Name)
        .chain(
            row.search_fields
                .iter()
                .filter(|field| field.kind == FieldKind::Title && field.shown == Shown::Never),
        )
        .map(Some)
        .chain(std::iter::once(None))
        .collect();
    let paths: Vec<_> = row
        .search_fields
        .iter()
        .filter(|field| field.kind == FieldKind::Path)
        .map(Some)
        .chain(std::iter::once(None))
        .collect();
    let mut best = None;
    let mut chosen = (None, None);
    for (name_preference, &name) in names.iter().enumerate() {
        for (path_preference, &path) in paths.iter().enumerate() {
            let mut covered = 0;
            let mut named = 0;
            for atom in atoms {
                let has_name = name.and_then(|field| atom.get(field)).is_some();
                let has_path = path.and_then(|field| atom.get(field)).is_some();
                covered += usize::from(has_name || has_path);
                named += usize::from(has_name);
            }
            let key = (
                covered,
                named,
                std::cmp::Reverse(name_preference),
                std::cmp::Reverse(path_preference),
            );
            if best.is_none_or(|best| key > best) {
                best = Some(key);
                chosen = (name, path);
            }
        }
    }
    let mut reasons = Vec::new();
    for (kind, field) in [(ReasonKind::Name, chosen.0), (ReasonKind::Path, chosen.1)] {
        let Some(field) = field else {
            continue;
        };
        let mut indices = Vec::new();
        for atom in atoms {
            if kind == ReasonKind::Path && chosen.0.and_then(|name| atom.get(name)).is_some() {
                continue;
            }
            if let Some(found) = atom.get(field) {
                indices.extend_from_slice(&found.indices);
            }
        }
        if !indices.is_empty() {
            reasons.push(Reason {
                kind,
                text: field.value.text().to_owned(),
                indices,
            });
        }
    }
    reasons
}
/// Empty queries retain the view's status/age order. For a query, each atom
/// chooses its best (class, field kind, score), then the row uses the worst atom
/// class, best chosen kind and total score before status, age and catalog order.
pub(crate) fn search_view(
    rows: &[Candidate],
    query: &str,
    status_first: bool,
    includes: impl Fn(AgentState) -> bool,
) -> Vec<Hit> {
    let empty_query = query.trim().is_empty();
    let query = Query::parse(query);
    let mut matcher = Matcher::default();
    let mut hits = Vec::new();
    for (index, row) in rows.iter().enumerate() {
        if !includes(row.state) {
            continue;
        }
        let mut hit = Hit {
            index,
            title_indices: Vec::new(),
            workspace_indices: Vec::new(),
            tab_indices: Vec::new(),
            name_indices: Vec::new(),
            detail_indices: Vec::new(),
            reasons: Vec::new(),
            reasons_beside_name: Vec::new(),
        };
        let fields: Vec<_> = row
            .search_fields
            .iter()
            .map(|field| Field {
                haystack: &field.value,
                priority: field.kind,
            })
            .collect();
        let Some(matched) = matcher.match_fields(&query, &fields) else {
            continue;
        };
        let mut best_kind = FieldKind::Path;
        let mut hidden_atoms = Vec::new();
        for atom in matched.atoms {
            // Details annotate accepted positive atoms but never make a row
            // searchable; atom indices retain correlation past negated words.
            if let Some(found) =
                matcher.match_atom(&query.atoms()[atom.atom_index], &row.detail_search)
            {
                hit.detail_indices.extend(found.indices);
            }
            best_kind = best_kind.min(row.search_fields[atom.best().field_index].kind);
            // The matcher owns relevance and accepted alignments; the catalog
            // attaches those field indices to visible columns and hidden chips.
            let matches: Vec<_> = atom
                .into_fields()
                .into_iter()
                .map(|found| (&row.search_fields[found.field_index], found.matched))
                .collect();
            for (field, found) in &matches {
                let indices = match (field.shown, field.kind) {
                    (Shown::Always, FieldKind::Title) => &mut hit.title_indices,
                    (Shown::Always, FieldKind::Tab) => &mut hit.tab_indices,
                    (Shown::Always, FieldKind::Workspace) => &mut hit.workspace_indices,
                    (Shown::NameColumn, _) => &mut hit.name_indices,
                    _ => continue,
                };
                indices.extend_from_slice(&found.indices);
            }
            if !matches
                .iter()
                .any(|(field, _)| field.shown == Shown::Always)
            {
                hidden_atoms.push(HiddenAtom { fields: matches });
            }
        }
        let hidden: Vec<_> = hidden_atoms.iter().collect();
        hit.reasons = hidden_reasons(row, &hidden);
        let unnamed: Vec<_> = hidden
            .into_iter()
            .filter(|atom| !atom.names_the_pane())
            .collect();
        hit.reasons_beside_name = hidden_reasons(row, &unnamed);
        for indices in [
            &mut hit.title_indices,
            &mut hit.workspace_indices,
            &mut hit.tab_indices,
            &mut hit.name_indices,
            &mut hit.detail_indices,
        ] {
            indices.sort_unstable();
            indices.dedup();
        }
        for reasons in [&mut hit.reasons, &mut hit.reasons_beside_name] {
            reasons.sort_by_key(|reason| reason.kind);
            for reason in reasons {
                reason.indices.sort_unstable();
                reason.indices.dedup();
            }
        }
        hits.push((
            (
                matched.worst_class,
                best_kind,
                std::cmp::Reverse(matched.total_score),
                if !empty_query || status_first {
                    row.state
                } else {
                    AgentState::Neutral
                },
                std::cmp::Reverse(row.started_at),
                index,
            ),
            hit,
        ));
    }
    hits.sort_by_key(|(key, _)| *key);
    hits.into_iter().map(|(_, hit)| hit).collect()
}

/// Metadata is untrusted terminal text. Never emit its control sequences into
/// the popup; whitespace stays readable and bidi controls cannot reorder IDs.
pub(crate) fn clean(text: &str) -> String {
    plain(text).trim().to_owned()
}

pub(crate) fn plain(text: &str) -> String {
    text.chars()
        .filter(|c| {
            !c.is_control() && !matches!(*c, '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}')
        })
        .take(4096)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn snapshot() -> serde_json::Value {
        json!({
            "workspaces":[{"workspace_id":"w1","label":"MAIN","number":1},
                          {"workspace_id":"w2","label":"ft","number":2}],
            "tabs":[{"tab_id":"w1:t1","workspace_id":"w1","label":"herdr","number":1},
                    {"tab_id":"w2:t1","workspace_id":"w2","label":"editor","number":1}],
            "panes":[
                {"pane_id":"w1:p1","terminal_id":"term1","tab_id":"w1:t1","workspace_id":"w1","label":"alias", "terminal_title_stripped":"Inspect Beacon unread", "cwd":"/code/herdr", "agent":"codex", "agent_status":"done"},
                {"pane_id":"w2:p1","terminal_id":"term2","tab_id":"w2:t1","workspace_id":"w2","label":"Beacon", "cwd":"/code/plain-shell"},
                {"pane_id":"w1:p2","terminal_id":"term3","tab_id":"w1:t1","workspace_id":"w1","terminal_title_stripped":"日本語 Café review", "cwd":"/code/café"}
            ],
            "agents":[{"pane_id":"w1:p1","name":"beacon-unread-check"}]
        })
    }

    // Labels, titles and directories mirror the live diagnosis; identities are
    // local to this fixture and no agent session or token metadata is copied.
    fn live_shapes() -> serde_json::Value {
        let home = std::env::var("HOME").unwrap();
        let panes = [
            ("vim-name", "skills", "vim-html-skill", "CLAUDE.md task label rules", ".config/skl/library/skills"),
            ("vim-title", "logseq", "", "Logseq fork with vim navigation", "code/oss/logseq"),
            ("nvim", "nvim", "diag-sidekick-send", "Rill diff colors contrast", ".config/nvim"),
            ("ferry", "herdr", "", "Review ferry kill mode", "code/plugins/my"),
            ("cloudflare", "model", "", "Explain the Cloudflare block | jev-like-model", "code/jev-like-model"),
            ("clis", "skills", "clis-fixes-fleet", "Herdr-factory-go @focus, @mac-notify, @hog fixes", "code/clis"),
            ("jump", "herdr", "engine-task", "Build search engine", "code/herdr-jump"),
        ].into_iter().map(|(id, tab, label, title, path)| json!({
            "pane_id":id, "terminal_id":format!("term-{id}"), "workspace_id":"setup", "tab_id":tab,
            "label":label, "terminal_title_stripped":title, "cwd":format!("{home}/{path}")
        })).collect::<Vec<_>>();
        json!({
            "workspaces":[{"workspace_id":"setup", "label":"SETUP"}],
            "tabs":(["skills", "logseq", "nvim", "herdr", "model"].map(|id| json!({
                "tab_id":id, "workspace_id":"setup", "label":id
            }))),
            "panes":panes
        })
    }

    fn hit_ids<'a>(rows: &'a [Candidate], hits: &[Hit]) -> Vec<&'a str> {
        hits.iter()
            .map(|hit| rows[hit.index].pane_id.as_str())
            .collect()
    }

    fn marked(text: &str, indices: &[u32]) -> String {
        let graphemes = text.graphemes(true).collect::<Vec<_>>();
        indices
            .iter()
            .map(|index| graphemes[*index as usize])
            .collect()
    }

    #[test]
    fn vim_keeps_visible_and_named_matches_and_drops_scattered_noise() {
        let rows = catalog(live_shapes(), "").unwrap();
        let hits = search(&rows, "vim");
        assert_eq!(hit_ids(&rows, &hits), ["vim-title", "vim-name", "nvim"]);
        assert_eq!(
            marked(&rows[hits[0].index].title, &hits[0].title_indices),
            "vim"
        );
        assert!(hits[0].reasons.is_empty());
        assert_eq!(
            hits[1].reasons,
            [Reason {
                kind: ReasonKind::Name,
                text: "vim-html-skill".to_owned(),
                indices: vec![0, 1, 2],
            }]
        );
        assert!(hits[1].title_indices.is_empty());
        assert_eq!(
            marked(&rows[hits[2].index].tab, &hits[2].tab_indices),
            "vim"
        );
        assert!(hits[2].reasons.is_empty());
    }

    #[test]
    fn paths_use_home_shorthand_and_explain_the_typed_fragment() {
        let rows = catalog(live_shapes(), "").unwrap();
        for (query, ids) in [
            ("skl/library", vec!["vim-name"]),
            ("~/.config", vec!["vim-name", "nvim"]),
            ("herdr-jump", vec!["jump"]),
            ("library/skills", vec!["vim-name"]),
        ] {
            let hits = search(&rows, query);
            assert_eq!(hit_ids(&rows, &hits), ids, "{query}");
            for hit in hits {
                assert_eq!(hit.reasons.len(), 1, "{query}");
                let reason = &hit.reasons[0];
                assert_eq!(reason.kind, ReasonKind::Path);
                assert!(reason.text.starts_with("~/"));
                assert_eq!(marked(&reason.text, &reason.indices), query);
            }
        }
        assert!(search(&rows, "").iter().all(|hit| hit.reasons.is_empty()));
    }

    #[test]
    fn foreground_and_launch_paths_are_searchable_and_foreground_wins_ties() {
        let home = std::env::var("HOME").unwrap();
        let mut value = live_shapes();
        value["panes"][0]["foreground_cwd"] = json!(format!("{home}/current/skills"));
        let rows = catalog(value, "").unwrap();
        assert_eq!(rows[0].path, "~/current/skills");
        for (query, reason) in [
            ("current", "~/current/skills"),
            ("library", "~/.config/skl/library/skills"),
            ("~/", "~/current/skills"),
        ] {
            let hits = search(&rows, query);
            let hit = hits
                .iter()
                .find(|hit| rows[hit.index].pane_id == "vim-name")
                .unwrap();
            assert_eq!(hit.reasons[0].text, reason);
        }
        let mut value = live_shapes();
        value["panes"][0]["cwd"] = json!(format!("{home}-sibling/library"));
        let rows = catalog(value, "").unwrap();
        assert_eq!(rows[0].path, format!("{home}-sibling/library"));
    }

    #[test]
    fn name_reasons_disappear_when_title_or_tab_explains_the_atom() {
        for (query, id, name) in [
            ("vim-html", "vim-name", "vim-html-skill"),
            ("clis-fixes", "clis", "clis-fixes-fleet"),
        ] {
            let rows = catalog(live_shapes(), "").unwrap();
            let hits = search(&rows, query);
            assert_eq!(hit_ids(&rows, &hits)[0], id);
            assert_eq!(hits[0].reasons[0].kind, ReasonKind::Name);
            assert_eq!(hits[0].reasons[0].text, name);
            assert_eq!(marked(name, &hits[0].reasons[0].indices), query);
        }
        for visible in ["title", "tab"] {
            let mut value = live_shapes();
            if visible == "title" {
                value["panes"][0]["title"] = json!("vim-html documentation");
            } else {
                value["tabs"][0]["label"] = json!("vim-html");
            }
            let rows = catalog(value, "").unwrap();
            let hits = search(&rows, "vim-html");
            let hit = hits
                .iter()
                .find(|hit| rows[hit.index].pane_id == "vim-name")
                .unwrap();
            assert!(hit.reasons.is_empty(), "{visible}");
        }
    }

    #[test]
    fn shown_names_highlight_in_their_column_and_need_no_chip_beside_it() {
        let rows = catalog(live_shapes(), "").unwrap();
        let hits = search(&rows, "vim");
        let hit = &hits[1];
        assert_eq!(rows[hit.index].pane_id, "vim-name");
        assert_eq!(marked("vim-html-skill", &hit.name_indices), "vim");
        assert!(hit.reasons_beside_name.is_empty());
        // Like the other columns, the name highlights even when the title
        // also explains the atom.
        let mut value = live_shapes();
        value["panes"][0]["title"] = json!("vim-html documentation");
        let rows = catalog(value, "").unwrap();
        let hits = search(&rows, "vim-html");
        assert_eq!(marked("vim-html-skill", &hits[0].name_indices), "vim-html");
        assert!(hits[0].reasons.is_empty() && hits[0].reasons_beside_name.is_empty());
        // A path still needs its chip beside the name.
        let rows = catalog(live_shapes(), "").unwrap();
        let hits = search(&rows, "vim library");
        let kinds = |reasons: &[Reason]| reasons.iter().map(|r| r.kind).collect::<Vec<_>>();
        assert_eq!(
            kinds(&hits[0].reasons),
            [ReasonKind::Name, ReasonKind::Path]
        );
        assert_eq!(kinds(&hits[0].reasons_beside_name), [ReasonKind::Path]);
        let path = &hits[0].reasons_beside_name[0];
        assert_eq!(marked(&path.text, &path.indices), "library");
        assert!(search(&rows, "")
            .iter()
            .all(|hit| hit.name_indices.is_empty() && hit.reasons_beside_name.is_empty()));
    }

    #[test]
    fn a_second_name_explains_what_the_shown_name_cannot() {
        // One Name chip explains one word when the name is hidden; beside a
        // shown label, that chip goes to the alias the other word matched.
        let mut value = live_shapes();
        value["panes"][0]["label"] = json!("alpha");
        value["agents"] = json!([{"pane_id":"vim-name", "name":"beta"}]);
        let rows = catalog(value, "").unwrap();
        let hits = search(&rows, "alpha beta");
        assert_eq!(hit_ids(&rows, &hits), ["vim-name"]);
        assert_eq!(hits[0].name_indices, [0, 1, 2, 3, 4]);
        assert_eq!(hits[0].reasons.len(), 1);
        assert_eq!(hits[0].reasons[0].text, "alpha");
        assert_eq!(hits[0].reasons_beside_name.len(), 1);
        assert_eq!(hits[0].reasons_beside_name[0].text, "beta");
        // Without a label the alias is the shown name, so it gets the column.
        let mut value = live_shapes();
        value["panes"][0]["label"] = json!("");
        value["agents"] = json!([{"pane_id":"vim-name", "name":"alpha-beta"}]);
        let rows = catalog(value, "").unwrap();
        assert_eq!(rows[0].name, "alpha-beta");
        let hits = search(&rows, "beta");
        assert_eq!(marked("alpha-beta", &hits[0].name_indices), "beta");
        assert!(hits[0].reasons_beside_name.is_empty());
        assert_eq!(hits[0].reasons[0].text, "alpha-beta");
    }

    #[test]
    fn reasons_merge_graphemes_and_order_names_before_paths() {
        let mut value = live_shapes();
        value["panes"][0]["label"] = json!("cafe\u{301}-vim-html");
        value["agents"] = json!([{"pane_id":"vim-name", "name":"alternate-alias"}]);
        let rows = catalog(value, "").unwrap();
        let hits = search(&rows, "library/skills cafe vim-html");
        assert_eq!(hit_ids(&rows, &hits), ["vim-name"]);
        assert_eq!(hits[0].reasons.len(), 2);
        assert_eq!(hits[0].reasons[0].kind, ReasonKind::Name);
        assert_eq!(
            marked(&hits[0].reasons[0].text, &hits[0].reasons[0].indices),
            "cafe\u{301}vim-html"
        );
        assert_eq!(hits[0].reasons[1].kind, ReasonKind::Path);
        let alias = search(&rows, "alternate-alias");
        assert_eq!(alias[0].reasons[0].text, "alternate-alias");
    }

    #[test]
    fn initials_prefix_runs_and_within_word_gaps_remain_searchable() {
        let rows = catalog(live_shapes(), "").unwrap();
        for query in ["rfkm", "revfer"] {
            let hits = search(&rows, query);
            assert_eq!(hit_ids(&rows, &hits), ["ferry"]);
            assert_eq!(
                marked(&rows[hits[0].index].title, &hits[0].title_indices).to_lowercase(),
                query
            );
        }
        for separator in [" ", "/", "-", "_", ".", "|", ":", "@", "›", "\u{2003}", ""] {
            let mut value = live_shapes();
            value["panes"][3]["title"] = json!(format!(
                "Review{separator}Ferry{separator}Kill{separator}Mode"
            ));
            let rows = catalog(value, "").unwrap();
            assert_eq!(
                hit_ids(&rows, &search(&rows, "rfkm")),
                ["ferry"],
                "{separator}"
            );
        }
        let rows = catalog(snapshot(), "").unwrap();
        assert_eq!(hit_ids(&rows, &search(&rows, "bcn unrd")), ["w1:p1"]);
        assert_eq!(hit_ids(&rows, &search(&rows, "main inspect")), ["w1:p1"]);
        assert_eq!(hit_ids(&rows, &search(&rows, "herdr unread")), ["w1:p1"]);
    }

    #[test]
    fn accepted_alignments_win_even_when_an_earlier_alignment_is_noise() {
        let mut value = live_shapes();
        value["panes"][3]["title"] = json!("Review ferry kill mode avim");
        value["panes"][4]["title"] = json!("Review ferry kill mode victim");
        let rows = catalog(value, "").unwrap();
        for (id, expected) in [("ferry", "vim"), ("cloudflare", "vim")] {
            let hits = search(&rows, "vim");
            let hit = hits
                .iter()
                .find(|hit| rows[hit.index].pane_id == id)
                .unwrap();
            assert_eq!(marked(&rows[hit.index].title, &hit.title_indices), expected);
            assert!(hit.title_indices.iter().all(|index| *index >= 23));
        }
        // The first 'a' at a word start cannot finish a legal run; a later
        // start can. A greedy subsequence followed by rejection misses this.
        value = live_shapes();
        value["panes"][3]["title"] = json!("Ax ab Cd");
        let rows = catalog(value, "").unwrap();
        let hits = search(&rows, "abcd");
        assert_eq!(hit_ids(&rows, &hits), ["ferry"]);
        assert_eq!(hits[0].title_indices, [3, 4, 6, 7]);
    }

    #[test]
    fn syntax_anchors_exact_negation_and_escaped_operators_are_preserved() {
        let rows = catalog(live_shapes(), "").unwrap();
        for (query, ids) in [
            ("'vim", vec!["vim-title", "vim-name", "nvim"]),
            ("^vim", vec!["vim-name"]),
            ("nvim$", vec!["nvim"]),
            ("^nvim$", vec!["nvim"]),
            ("'rfkm", vec![]),
            ("vim !skill", vec!["vim-title", "nvim"]),
            ("vim !^vim", vec!["vim-title", "nvim"]),
            ("vim !nvim$", vec!["vim-title", "vim-name"]),
            ("Review\\ ferry", vec!["ferry"]),
        ] {
            assert_eq!(hit_ids(&rows, &search(&rows, query)), ids, "{query}");
        }
        let hits = search(&rows, "!vim");
        assert_eq!(
            hit_ids(&rows, &hits),
            ["ferry", "cloudflare", "clis", "jump"]
        );
        assert!(hits
            .iter()
            .all(|hit| hit.title_indices.is_empty() && hit.reasons.is_empty()));
        let mut value = live_shapes();
        value["panes"][3]["title"] = json!("!wow ^hat cash$ 'quote");
        let rows = catalog(value, "").unwrap();
        for query in ["\\!wow", "\\^hat", "cash\\$", "\\'quote"] {
            assert_eq!(hit_ids(&rows, &search(&rows, query)), ["ferry"], "{query}");
        }
    }

    #[test]
    fn invisible_agent_kind_status_and_identity_are_not_search_fields() {
        let mut value = live_shapes();
        value["panes"][0]["agent"] = json!("codex");
        value["panes"][0]["agent_status"] = json!("blocked");
        value["panes"][0]["pane_id"] = json!("unique-pane-identity");
        let rows = catalog(value, "").unwrap();
        for query in ["codex", "blocked", "unique-pane-identity"] {
            assert!(search(&rows, query).is_empty(), "{query}");
        }
    }

    #[test]
    fn reasons_cover_the_whole_query_independently_of_atom_order() {
        let home = std::env::var("HOME").unwrap();
        for kind in [ReasonKind::Name, ReasonKind::Path] {
            let mut value = live_shapes();
            if kind == ReasonKind::Path {
                value["panes"][0]["foreground_cwd"] = json!(format!("{home}/alpha"));
                value["panes"][0]["cwd"] = json!(format!("{home}/alpha/beta"));
            } else {
                value["panes"][0]["label"] = json!("alpha");
                value["agents"] = json!([{"pane_id":"vim-name", "name":"alpha-beta"}]);
            }
            let rows = catalog(value, "").unwrap();
            for query in ["alpha beta", "beta alpha"] {
                let hits = search(&rows, query);
                assert_eq!(hit_ids(&rows, &hits), ["vim-name"]);
                assert_eq!(hits[0].reasons.len(), 1);
                let reason = &hits[0].reasons[0];
                assert_eq!(reason.kind, kind);
                assert_eq!(
                    reason.text,
                    if kind == ReasonKind::Name {
                        "alpha-beta"
                    } else {
                        "~/alpha/beta"
                    }
                );
                assert_eq!(marked(&reason.text, &reason.indices), "alphabeta");
            }
        }
    }

    #[test]
    fn hidden_alternate_titles_explain_matches_and_keep_title_ranking() {
        let mut value = live_shapes();
        value["panes"][0]["title"] = json!("Refactor authentication");
        value["panes"][0]["terminal_title_stripped"] = json!("Inspect Beacon unread");
        value["panes"][1]["label"] = json!("inspect-alias");
        let rows = catalog(value, "").unwrap();
        let hits = search(&rows, "inspect");
        assert_eq!(hit_ids(&rows, &hits), ["vim-name", "vim-title"]);
        assert!(hits[0].title_indices.is_empty());
        assert_eq!(
            hits[0].reasons,
            [Reason {
                kind: ReasonKind::Name,
                text: "Inspect Beacon unread".to_owned(),
                indices: (0..7).collect()
            }]
        );
    }

    #[test]
    fn name_and_path_are_selected_together_for_complete_coverage() {
        let home = std::env::var("HOME").unwrap();
        let mut value = live_shapes();
        value["panes"][0]["label"] = json!("alpha");
        value["panes"][0]["foreground_cwd"] = json!(format!("{home}/alpha"));
        value["panes"][0]["cwd"] = json!(format!("{home}/alpha/gamma"));
        value["agents"] = json!([{"pane_id":"vim-name", "name":"beta"}]);
        let rows = catalog(value, "").unwrap();
        for query in ["alpha beta gamma", "gamma beta alpha"] {
            let hits = search(&rows, query);
            assert_eq!(hit_ids(&rows, &hits), ["vim-name"]);
            assert_eq!(hits[0].reasons.len(), 2);
            assert_eq!(hits[0].reasons[0].text, "beta");
            assert_eq!(hits[0].reasons[1].text, "~/alpha/gamma");
            assert_eq!(
                marked(&hits[0].reasons[1].text, &hits[0].reasons[1].indices),
                "alphagamma"
            );
        }
    }

    fn ranking_rows(specs: &[(&str, &str, &str)]) -> Vec<Candidate> {
        let panes = specs
            .iter()
            .enumerate()
            .map(|(i, (title, name, path))| {
                json!({
                    "pane_id":format!("row-{i}"), "terminal_id":format!("term-{i}"),
                    "workspace_id":"w", "tab_id":"t", "title":title, "label":name, "cwd":path,
                    "agent":"codex", "agent_status":"idle"
                })
            })
            .collect::<Vec<_>>();
        catalog(
            json!({"workspaces":[{"workspace_id":"w","label":"Elsewhere"}],
            "tabs":[{"workspace_id":"w","tab_id":"t","label":"Tasks"}], "panes":panes}),
            "",
        )
        .unwrap()
    }

    #[test]
    fn classes_and_field_kinds_outrank_score_status_and_age() {
        let mut rows = ranking_rows(&[
            ("b e a c o n", "", ""),
            ("zbeaconx", "", ""),
            ("Hidden task", "", "/code/beacon-tools"),
            ("Beacon extras", "", ""),
            ("Hidden task", "beacon", ""),
            ("beacon", "", ""),
        ]);
        for (i, row) in rows.iter_mut().enumerate() {
            row.state = if i < 3 {
                AgentState::Blocked
            } else {
                AgentState::Idle
            };
            row.started_at = Some(9000 - i as i64);
        }
        for status_first in [true, false] {
            let hits = search_view(&rows, "beacon", status_first, |_| true);
            assert_eq!(
                hit_ids(&rows, &hits),
                ["row-5", "row-4", "row-3", "row-2", "row-1", "row-0"]
            );
        }
    }

    #[test]
    fn worst_atom_class_is_ranked_before_any_individual_strong_match() {
        let rows = ranking_rows(&[("cxd", "ab", ""), ("ab cd", "", "")]);
        assert_eq!(hit_ids(&rows, &search(&rows, "ab cd")), ["row-1", "row-0"]);
    }

    #[test]
    fn score_then_status_then_youngest_age_then_catalog_order_break_ties() {
        let mut rows = ranking_rows(&[("a distant b distant c", "", ""), ("a b c", "", "")]);
        rows[0].state = AgentState::Blocked;
        rows[1].state = AgentState::Idle;
        assert_eq!(hit_ids(&rows, &search(&rows, "abc")), ["row-1", "row-0"]);
        let mut rows = ranking_rows(&[("beacon", "", ""); 4]);
        for (row, (state, age)) in rows.iter_mut().zip([
            (AgentState::Idle, Some(500)),
            (AgentState::Blocked, Some(100)),
            (AgentState::Blocked, Some(200)),
            (AgentState::Blocked, Some(200)),
        ]) {
            row.state = state;
            row.started_at = age;
        }
        for status_first in [true, false] {
            let hits = search_view(&rows, "beacon", status_first, |_| true);
            assert_eq!(hit_ids(&rows, &hits), ["row-2", "row-3", "row-1", "row-0"]);
        }
    }

    #[test]
    fn fallback_search_agrees_with_exhaustive_small_alignment_oracle() {
        // Enumerate alignments independently of the DP. This checks existence,
        // including repeated-letter dead ends, through the catalog/search seam.
        for code in 0..729 {
            let mut digits = code;
            let text: String = (0..6)
                .map(|_| {
                    let c = ['a', 'b', ' '][digits % 3];
                    digits /= 3;
                    c
                })
                .collect();
            let text = text.trim();
            let chars: Vec<_> = text.chars().collect();
            let rows = ranking_rows(&[(text, "", "")]);
            for query in ["aa", "ab", "ba", "bb", "aba"] {
                let needle: Vec<_> = query.chars().collect();
                let mut accepted = false;
                for mask in 0_u32..(1 << chars.len()) {
                    if mask.count_ones() as usize != needle.len() {
                        continue;
                    }
                    let positions: Vec<_> =
                        (0..chars.len()).filter(|j| mask & (1 << j) != 0).collect();
                    if !positions.iter().zip(&needle).all(|(&j, &c)| chars[j] == c) {
                        continue;
                    }
                    let contiguous = positions.windows(2).all(|p| p[1] == p[0] + 1);
                    let one_word = chars[positions[0]..=positions[positions.len() - 1]]
                        .iter()
                        .all(|c| *c != ' ');
                    let starts = positions.iter().enumerate().all(|(i, &j)| {
                        (i > 0 && j == positions[i - 1] + 1) || j == 0 || chars[j - 1] == ' '
                    });
                    accepted |= contiguous || one_word || starts;
                }
                let hits = search(&rows, query);
                assert_eq!(!hits.is_empty(), accepted, "text={text:?}, query={query}");
                if let Some(hit) = hits.first() {
                    assert_eq!(marked(text, &hit.title_indices), query);
                }
            }
        }
    }

    #[test]
    #[ignore = "release timing: cargo test --release --locked search_500_rows -- --ignored --nocapture"]
    fn search_500_rows_with_three_atoms_fits_a_frame() {
        let home = std::env::var("HOME").unwrap();
        let path = format!("{home}/code/side-projects/herdr-custom-plugins/my/herdr-jump");
        let rows = ranking_rows(&vec![
            (
                "Review ferry kill mode",
                "review-task",
                path.as_str()
            );
            500
        ]);
        let query = "revfer kill herdr-jump";
        assert_eq!(search(&rows, query).len(), 500);
        let mut timings = Vec::new();
        for _ in 0..100 {
            let start = std::time::Instant::now();
            std::hint::black_box(search(
                std::hint::black_box(&rows),
                std::hint::black_box(query),
            ));
            timings.push(start.elapsed());
        }
        timings.sort();
        eprintln!(
            "500 rows × 3 atoms: median {:?}, p95 {:?}, max {:?}",
            timings[50], timings[95], timings[99]
        );
        assert!(
            timings[95] < std::time::Duration::from_millis(16),
            "{timings:?}"
        );
    }

    /// Every workspace's panes are rows, `ft` included: Find hides none. The
    /// Agents workspace policy herdr-jump last consulted is gone, and with
    /// Agents disabled it never hid anything.
    #[test]
    fn tests_that_all_panes_include_shells_and_hidden_workspaces() {
        let rows = catalog(snapshot(), "w2:p1").unwrap();
        assert_eq!(rows.len(), 3);
        assert_eq!(rows[0].pane_id, "w2:p1");
        assert_eq!(rows[0].terminal_id, "term2");
        assert_eq!(rows[1].title, "Inspect Beacon unread");
        assert_eq!(search(&rows, "plain-shell").len(), 1);
    }

    #[test]
    fn own_popup_is_never_a_row() {
        assert_eq!(
            catalog_with_own(snapshot(), "w2:p1", "w2:p1")
                .unwrap()
                .len(),
            2
        );
    }

    #[test]
    fn tests_that_exact_names_rank_above_title_substrings_and_fuzzy_matches() {
        let rows = catalog(snapshot(), "w1:p1").unwrap();
        let hits = search(&rows, "BEACON");
        assert_eq!(hits.len(), 2);
        assert_eq!(rows[hits[0].index].pane_id, "w2:p1");
        let hits = search(&rows, "bcn unrd");
        assert_eq!(hits.len(), 1);
        assert_eq!(rows[hits[0].index].pane_id, "w1:p1");
    }

    #[test]
    fn tests_that_context_and_agent_aliases_are_searchable() {
        let rows = catalog(snapshot(), "w1:p1").unwrap();
        for query in [
            "main inspect",
            "herdr unread",
            "beacon-unread-check",
            "alias",
        ] {
            let hits = search(&rows, query);
            assert!(!hits.is_empty(), "query: {query}");
            assert_eq!(rows[hits[0].index].pane_id, "w1:p1");
        }
        assert!(search(&rows, "nonexistentqzx").is_empty());
        assert_eq!(search(&rows, "  ").len(), 3);
    }

    #[test]
    fn tests_that_unicode_highlights_use_character_positions() {
        let rows = catalog(snapshot(), "w1:p1").unwrap();
        let hits = search(&rows, "cafe");
        assert_eq!(hits.len(), 1);
        let title: Vec<char> = rows[hits[0].index].title.chars().collect();
        let highlighted: String = hits[0]
            .title_indices
            .iter()
            .map(|i| title[*i as usize])
            .collect();
        assert_eq!(highlighted, "Café");
        assert_eq!(search(&rows, "日本").len(), 1);
    }

    #[test]
    fn tests_that_ambiguous_identity_is_rejected() {
        let mut value = snapshot();
        let duplicate = value["panes"][0].clone();
        value["panes"].as_array_mut().unwrap().push(duplicate);
        assert!(catalog(value, "w1:p1").is_err());
    }

    #[test]
    fn tests_that_duplicate_titles_remain_distinct_destinations() {
        let mut value = snapshot();
        value["panes"][0]["terminal_title_stripped"] = json!("Beacon");
        let rows = catalog(value, "w1:p1").unwrap();
        let hits = search(&rows, "beacon");
        assert_eq!(hits.len(), 2);
        assert_ne!(rows[hits[0].index].pane_id, rows[hits[1].index].pane_id);
    }

    #[test]
    fn tests_that_reported_task_titles_are_primary_and_terminal_titles_remain_searchable() {
        let mut value = snapshot();
        value["panes"][0]["title"] = json!("Refactor authentication");
        let rows = catalog(value, "w1:p1").unwrap();
        assert_eq!(rows[0].title, "Refactor authentication");
        for query in ["refactor auth", "inspect unread", "beacon-unread-check"] {
            let hits = search(&rows, query);
            assert!(!hits.is_empty(), "{query}");
            assert_eq!(rows[hits[0].index].pane_id, "w1:p1");
        }
    }

    #[test]
    fn tests_that_decomposed_unicode_matches_and_highlights_graphemes() {
        use unicode_segmentation::UnicodeSegmentation;
        let mut value = snapshot();
        value["panes"][2]["terminal_title_stripped"] = json!("cafe\u{301} review");
        let mut rows = catalog(value, "w1:p1").unwrap();
        rows.retain(|row| row.pane_id == "w1:p2");
        for query in ["cafe\u{301}", "café", "review"] {
            let hits = search(&rows, query);
            assert_eq!(hits.len(), 1, "{query}");
            if query == "review" {
                let graphemes: Vec<_> = rows[0].title.graphemes(true).collect();
                let marked: String = hits[0]
                    .title_indices
                    .iter()
                    .map(|i| graphemes.get(*i as usize).copied().unwrap_or("INVALID"))
                    .collect();
                assert_eq!(marked, "review");
            }
        }
    }
    #[test]
    fn every_host_status_maps_only_when_an_agent_is_present() {
        for (status, expected) in [
            ("blocked", AgentState::Blocked),
            ("done", AgentState::Done),
            ("working", AgentState::Working),
            ("idle", AgentState::Idle),
            ("unknown", AgentState::Neutral),
            ("future-state", AgentState::Neutral),
            ("", AgentState::Neutral),
        ] {
            assert_eq!(AgentState::from_host(Some("codex"), status), expected);
            assert_eq!(AgentState::from_host(None, status), AgentState::Neutral);
            assert_eq!(
                expected.word(),
                if expected == AgentState::Neutral {
                    ""
                } else {
                    status
                }
            );
        }
        let mut value = snapshot();
        value["panes"][0]["agent"] = serde_json::Value::Null;
        value["panes"][0]["agent_status"] = json!("working");
        value["panes"][0]["started_at"] = json!(1000);
        let row = catalog(value, "").unwrap().remove(0);
        assert_eq!(row.state, AgentState::Neutral);
        assert_eq!(row.started_at, None);
    }

    #[test]
    fn own_picker_is_dropped_but_source_remains_a_jump_target() {
        let rows = catalog_with_own(snapshot(), "w1:p1", "w1:p2").unwrap();
        assert_eq!(rows.len(), 2);
        assert!(rows.iter().any(|c| c.pane_id == "w1:p1"));
        assert!(!rows.iter().any(|c| c.pane_id == "w1:p2"));
    }

    #[test]
    fn all_sorts_status_then_youngest_known_age_then_stable_catalog_order() {
        let base = catalog(snapshot(), "w2:p1").unwrap();
        let mut rows = Vec::new();
        for (state, at) in [
            (AgentState::Working, Some(300)),
            (AgentState::Blocked, None),
            (AgentState::Done, Some(500)),
            (AgentState::Blocked, Some(100)),
            (AgentState::Blocked, Some(200)),
            (AgentState::Blocked, Some(200)),
            (AgentState::Idle, Some(900)),
            (AgentState::Neutral, None),
        ] {
            let mut row = base[0].clone();
            row.state = state;
            row.started_at = at;
            rows.push(row);
        }
        assert_eq!(
            search(&rows, "")
                .iter()
                .map(|h| h.index)
                .collect::<Vec<_>>(),
            [4, 5, 3, 1, 2, 0, 6, 7]
        );
        let hits = search_view(&rows, "", false, |s| {
            matches!(s, AgentState::Blocked | AgentState::Done)
        });
        assert_eq!(
            hits.iter().map(|h| h.index).collect::<Vec<_>>(),
            [2, 4, 5, 3, 1]
        );
        let neutral = catalog(snapshot(), "w2:p1")
            .unwrap()
            .into_iter()
            .map(|mut c| {
                c.state = AgentState::Neutral;
                c
            })
            .collect::<Vec<_>>();
        assert_eq!(neutral[search(&neutral, "")[0].index].pane_id, "w2:p1");
    }

    #[test]
    fn query_exact_prefix_and_fuzzy_ranking_wins_over_status_and_age() {
        let mut rows = ranking_rows(&[
            ("beacon", "", ""),
            ("beacon extra", "", ""),
            ("b-e-a-c-o-n", "", ""),
        ]);
        for (i, row) in rows.iter_mut().enumerate() {
            row.state = [AgentState::Neutral, AgentState::Idle, AgentState::Blocked][i];
            row.started_at = Some(i as i64 * 1000);
        }
        for status_first in [true, false] {
            assert_eq!(
                search_view(&rows, "beacon", status_first, |_| true)
                    .iter()
                    .map(|h| h.index)
                    .collect::<Vec<_>>(),
                [0, 1, 2]
            );
        }
    }
}
