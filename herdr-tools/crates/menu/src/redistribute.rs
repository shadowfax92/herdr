//! Plan a new split tree without replacing its live terminals.
//!
//! A plan owns the exact snapshot the user previewed. Execution can therefore
//! refuse stale plans instead of applying them to a different set of agents.

use std::collections::HashSet;
use std::fs::{self, OpenOptions};
use std::os::unix::fs::OpenOptionsExt;
use std::path::PathBuf;

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::herdr::Herdr;

// These are plugin usability limits, not Herdr protocol limits. Include the
// pane frame so every resulting terminal retains visible content and a title.
const MIN_PANE_WIDTH: usize = 12;
const MIN_PANE_HEIGHT: usize = 5;

/// Public pane.layout geometry, also used to validate the preview's freshness.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct LayoutSnapshot {
    pub workspace_id: String,
    pub tab_id: String,
    #[serde(default)]
    pub tab_label: Option<String>,
    pub zoomed: bool,
    pub area: PaneRect,
    pub focused_pane_id: String,
    pub panes: Vec<LayoutPane>,
}

/// A changed snapshot is not an error and, critically, is not permission to
/// continue: the UI must display it and obtain another explicit confirmation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ApplyOutcome {
    Applied,
    Refresh(Box<LayoutSnapshot>),
}

/// Owns one popup's source identity and the temporary-pane lifecycle.
/// It never closes an original pane or uses Herdr's process-replacing layout API.
#[derive(Clone, Debug)]
pub struct Redistributor {
    herdr: Herdr,
    source_pane: String,
    source_cwd: PathBuf,
    state_dir: PathBuf,
}

impl Redistributor {
    pub fn new(
        herdr: Herdr,
        source_pane: impl Into<String>,
        source_cwd: impl Into<PathBuf>,
        state_dir: impl Into<PathBuf>,
    ) -> Self {
        Self {
            herdr,
            source_pane: source_pane.into(),
            source_cwd: source_cwd.into(),
            state_dir: state_dir.into(),
        }
    }

    pub fn capture(&self) -> Result<LayoutSnapshot> {
        let mut layout = self.layout_for(&self.source_pane)?;
        let tab: Value = self.herdr.json(["tab", "get", &layout.tab_id])?;
        layout.tab_label = tab["result"]["tab"]["label"].as_str().map(str::to_owned);
        // Global focus may have changed since launch; the menu still belongs
        // to its explicitly captured source, including when it is a shell.
        layout.focused_pane_id = self.source_pane.clone();
        layout.panes.sort_by(|a, b| {
            (a.rect.y, a.rect.x, &a.pane_id).cmp(&(b.rect.y, b.rect.x, &b.pane_id))
        });
        Ok(layout)
    }

    pub fn apply(&self, plan: &LayoutPlan) -> Result<ApplyOutcome> {
        fs::create_dir_all(&self.state_dir).context("creating redistribution state directory")?;
        let lock = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .open(self.state_dir.join("redistribute.lock"))?;
        // Never block the popup waiting for another rearrangement. The OS
        // releases this advisory lock even if the popup process is interrupted.
        fs4::FileExt::try_lock(&lock)
            .context("another redistribution is running, or its lock is unavailable")?;
        let current = self.capture()?;
        if current.tab_id != plan.snapshot.tab_id
            || current.workspace_id != plan.snapshot.workspace_id
            || plan.snapshot.focused_pane_id != self.source_pane
        {
            bail!("The source pane moved to another tab; reopen Redistribute there");
        }
        if current != plan.snapshot {
            return Ok(ApplyOutcome::Refresh(Box::new(current)));
        }
        // Revalidate even a programmatically supplied plan at the execution seam.
        plan.validate()?;
        if current.zoomed {
            self.herdr
                .output(["pane", "zoom", "--pane", &self.source_pane, "--off"])?;
        }
        if plan.pane_count() == 1 {
            self.restore_focus(&current.tab_id)?;
            return Ok(ApplyOutcome::Applied);
        }

        let label = format!("menu-redistribute-{}", uuid::Uuid::new_v4().simple());
        let cwd = self
            .source_cwd
            .to_str()
            .context("source cwd is not valid UTF-8")?;
        let created: Value = self
            .herdr
            .json([
                "tab",
                "create",
                "--workspace",
                &current.workspace_id,
                "--cwd",
                cwd,
                "--label",
                &label,
                "--no-focus",
            ])
            .with_context(|| {
                format!(
                    "creating holding tab {label}; inspect that label if creation was interrupted"
                )
            })?;
        let holding_tab = created["result"]["tab"]["tab_id"]
            .as_str()
            .filter(|id| !id.is_empty() && *id != current.tab_id)
            .with_context(|| format!("holding tab {label} returned an invalid tab ID"))?;
        let holding_root = created["result"]["root_pane"]["pane_id"]
            .as_str()
            .filter(|id| !id.is_empty() && !current.panes.iter().any(|pane| pane.pane_id == *id))
            .with_context(|| {
                format!("holding tab {holding_tab} returned an invalid temporary pane ID")
            })?;

        // Recovery destinations come first so even the smallest popup exposes
        // them before the longer explanation and underlying CLI diagnostics.
        self.rebuild(plan, holding_tab, holding_root).with_context(|| format!(
            "Source {}\nHolding {}\nRedistribution stopped. Original panes were not closed.\nHolding tab label: {label}",
            current.tab_id, holding_tab,
        ))?;
        Ok(ApplyOutcome::Applied)
    }

    fn rebuild(&self, plan: &LayoutPlan, holding_tab: &str, holding_root: &str) -> Result<()> {
        let source_tab = &plan.snapshot.tab_id;
        let original_ids = plan.rows.iter().flatten().cloned().collect::<Vec<_>>();
        let holding_ids = std::iter::once(holding_root.to_owned())
            .chain(original_ids.iter().skip(1).cloned())
            .collect::<Vec<_>>();
        // Both staging and restoration use balanced trees. Repeatedly splitting
        // the same leaf would shrink it exponentially and hit Herdr's ratio cap.
        self.fill_tree(&SplitTree::row(&holding_ids), source_tab, holding_tab)?;
        self.fill_tree(&plan.tree()?, holding_tab, source_tab)?;

        let restored = self.layout_for(&original_ids[0])?;
        let restored_ids = restored
            .panes
            .iter()
            .map(|pane| pane.pane_id.as_str())
            .collect::<HashSet<_>>();
        let expected = original_ids
            .iter()
            .map(String::as_str)
            .collect::<HashSet<_>>();
        if restored.tab_id != *source_tab
            || restored.workspace_id != plan.snapshot.workspace_id
            || restored_ids != expected
        {
            bail!(
                "Source membership changed during redistribution; leaving the holding tab intact"
            );
        }
        let holding = self.layout_for(holding_root)?;
        if holding.tab_id != holding_tab
            || holding.panes.len() != 1
            || holding.panes[0].pane_id != holding_root
        {
            bail!("Holding tab still contains other panes; leaving it intact");
        }
        // Close only our root, never the entire tab. Even if another client
        // inserts a pane after this check, closing our shell cannot kill it.
        self.herdr
            .close_pane(holding_root)
            .context("removing the empty temporary shell")?;
        self.restore_focus(source_tab)
    }

    fn fill_tree(&self, tree: &SplitTree, from: &str, to: &str) -> Result<()> {
        if let SplitTree::Split {
            direction,
            ratio,
            first,
            second,
        } = tree
        {
            let pane = second.first_pane();
            let target = first.first_pane();
            let source = self.layout_for(pane)?;
            let destination = self.layout_for(target)?;
            if source.tab_id != from
                || destination.tab_id != to
                || source.workspace_id != destination.workspace_id
                || source.zoomed
                || destination.zoomed
            {
                bail!("Pane {pane} or its destination changed during redistribution");
            }
            let response: Value = self.herdr.json([
                "pane",
                "move",
                pane,
                "--tab",
                to,
                "--target-pane",
                target,
                "--split",
                direction,
                "--ratio",
                &ratio.to_string(),
                "--no-focus",
            ])?;
            let moved = &response["result"]["move_result"];
            if moved["changed"].as_bool() != Some(true) {
                bail!(
                    "Herdr did not move {pane}: {}",
                    moved["reason"].as_str().unwrap_or("invalid move response")
                );
            }
            if moved["pane"]["pane_id"].as_str() != Some(pane) {
                bail!("Herdr returned an unexpected identity for moved pane {pane}");
            }
            self.fill_tree(first, from, to)?;
            self.fill_tree(second, from, to)?;
        }
        Ok(())
    }

    fn layout_for(&self, pane: &str) -> Result<LayoutSnapshot> {
        let response: LayoutEnvelope = self.herdr.json(["pane", "layout", "--pane", pane])?;
        let layout = response.result.layout;
        if layout.tab_id.is_empty()
            || layout.workspace_id.is_empty()
            || !layout.panes.iter().any(|item| item.pane_id == pane)
        {
            bail!("Herdr returned no valid layout for pane {pane}");
        }
        Ok(layout)
    }

    fn restore_focus(&self, tab: &str) -> Result<()> {
        // pane.zoom always focuses its explicit target, even when already off.
        // Use that pane-level API: agent.focus rejects ordinary shell panes.
        // Redistribution's final state is unzoomed, so this is idempotent.
        self.herdr
            .output(["pane", "zoom", "--pane", &self.source_pane, "--off"])
            .context("restoring source pane focus")?;
        self.herdr
            .output(["tab", "focus", tab])
            .context("restoring source tab focus")?;
        Ok(())
    }
}

#[derive(Deserialize)]
struct LayoutEnvelope {
    result: LayoutResult,
}

#[derive(Deserialize)]
struct LayoutResult {
    layout: LayoutSnapshot,
}

/// Construction order doubles as the live-move schedule: retain each leftmost
/// leaf and insert the right subtree's first original, then fill both children.
enum SplitTree {
    Pane(String),
    Split {
        direction: &'static str,
        ratio: f64,
        first: Box<Self>,
        second: Box<Self>,
    },
}

impl SplitTree {
    fn panes(&self, rect: PaneRect, panes: &mut Vec<LayoutPane>) {
        match self {
            Self::Pane(pane_id) => panes.push(LayoutPane {
                pane_id: pane_id.clone(),
                rect,
            }),
            Self::Split {
                direction,
                ratio,
                first,
                second,
            } => {
                let mut left = rect;
                let mut right = rect;
                if *direction == "right" {
                    left.width = (f32::from(rect.width) * *ratio as f32).round() as u16;
                    right.x += left.width;
                    right.width -= left.width;
                } else {
                    left.height = (f32::from(rect.height) * *ratio as f32).round() as u16;
                    right.y += left.height;
                    right.height -= left.height;
                }
                first.panes(left, panes);
                second.panes(right, panes);
            }
        }
    }

    fn grid(rows: &[Vec<String>], heights: &[usize], widths: &[Vec<usize>]) -> Result<Self> {
        if rows.len() == 1 {
            return Self::weighted_row(&rows[0], &widths[0]);
        }
        let (middle, ratio) = weighted_split(heights)?;
        Ok(Self::Split {
            direction: "down",
            ratio,
            first: Box::new(Self::grid(
                &rows[..middle],
                &heights[..middle],
                &widths[..middle],
            )?),
            second: Box::new(Self::grid(
                &rows[middle..],
                &heights[middle..],
                &widths[middle..],
            )?),
        })
    }

    fn weighted_row(panes: &[String], widths: &[usize]) -> Result<Self> {
        if panes.len() == 1 {
            return Ok(Self::Pane(panes[0].clone()));
        }
        let (middle, ratio) = weighted_split(widths)?;
        Ok(Self::Split {
            direction: "right",
            ratio,
            first: Box::new(Self::weighted_row(&panes[..middle], &widths[..middle])?),
            second: Box::new(Self::weighted_row(&panes[middle..], &widths[middle..])?),
        })
    }

    fn validate_size(&self, width: usize, height: usize) -> Result<()> {
        match self {
            Self::Pane(_) if width < MIN_PANE_WIDTH || height < MIN_PANE_HEIGHT => {
                bail!("Panes would be too small (minimum {MIN_PANE_WIDTH} columns × {MIN_PANE_HEIGHT} lines); choose different rows or enlarge the terminal");
            }
            Self::Pane(_) => Ok(()),
            Self::Split {
                direction,
                ratio,
                first,
                second,
            } => {
                // Match Herdr's f32 rounding, including integer cell boundaries.
                if *direction == "right" {
                    let left = (width as f32 * *ratio as f32).round() as usize;
                    first.validate_size(left, height)?;
                    second.validate_size(width - left, height)
                } else {
                    let top = (height as f32 * *ratio as f32).round() as usize;
                    first.validate_size(width, top)?;
                    second.validate_size(width, height - top)
                }
            }
        }
    }

    fn row(panes: &[String]) -> Self {
        if panes.len() == 1 {
            return Self::Pane(panes[0].clone());
        }
        let middle = panes.len().div_ceil(2);
        Self::Split {
            direction: "right",
            ratio: middle as f64 / panes.len() as f64,
            first: Box::new(Self::row(&panes[..middle])),
            second: Box::new(Self::row(&panes[middle..])),
        }
    }

    fn first_pane(&self) -> &str {
        match self {
            Self::Pane(id) => id,
            Self::Split { first, .. } => first.first_pane(),
        }
    }
}

// Balance by cell weights when preserving geometry, or by counts for ordinary
// redistribution. A near-half boundary avoids exponentially narrow split chains.
fn weighted_split(weights: &[usize]) -> Result<(usize, f64)> {
    let total = weights.iter().sum::<usize>();
    let mut prefix = 0;
    let (middle, left) = weights
        .iter()
        .take(weights.len() - 1)
        .enumerate()
        .map(|(index, weight)| {
            prefix += weight;
            (index + 1, prefix)
        })
        .min_by_key(|(_, prefix)| (prefix * 2).abs_diff(total))
        .unwrap();
    let ratio = left as f64 / total as f64;
    if !(0.1..=0.9).contains(&ratio) {
        bail!("Row dimensions exceed Herdr's supported split ratios");
    }
    Ok((middle, ratio))
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct LayoutPane {
    pub pane_id: String,
    pub rect: PaneRect,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct PaneRect {
    pub x: u16,
    pub y: u16,
    pub width: u16,
    pub height: u16,
}

/// Confirmable, immutable row assignment for exactly the previewed panes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LayoutPlan {
    snapshot: LayoutSnapshot,
    rows: Vec<Vec<String>>,
    // Integer geometry weights allow fork normalization to preserve every
    // unaffected row even when the old tree was built column-first.
    heights: Vec<usize>,
    widths: Vec<Vec<usize>>,
}

impl LayoutPlan {
    /// Predict the same cell rounding as the mover's split tree. Fork planning
    /// can check the eventual extra pane before any original terminal moves.
    pub(crate) fn preview(&self) -> Result<LayoutSnapshot> {
        let mut snapshot = self.snapshot.clone();
        snapshot.zoomed = false;
        snapshot.panes.clear();
        self.tree()?.panes(snapshot.area, &mut snapshot.panes);
        Ok(snapshot)
    }

    /// Retain planned cell weights when the caller has revalidated geometry
    /// and captured the label/focus metadata required by the execution check.
    pub(crate) fn with_snapshot(&self, snapshot: LayoutSnapshot) -> Result<Self> {
        Self::with_sizes(
            snapshot,
            self.rows.clone(),
            self.heights.clone(),
            self.widths.clone(),
        )
    }

    pub fn new(mut snapshot: LayoutSnapshot, row_count: usize) -> Result<Self> {
        let count = snapshot.panes.len();
        if count == 0 {
            bail!("The source tab has no panes");
        }
        if row_count == 0 || row_count > count {
            bail!("Rows must be between 1 and {count}");
        }
        // Geometry, not API list order or agent status, defines reading order.
        snapshot.panes.sort_by(|a, b| {
            (a.rect.y, a.rect.x, &a.pane_id).cmp(&(b.rect.y, b.rect.x, &b.pane_id))
        });
        let mut panes = snapshot.panes.iter();
        let rows = (0..row_count)
            .map(|row| {
                let size = count / row_count + usize::from(row < count % row_count);
                panes
                    .by_ref()
                    .take(size)
                    .map(|p| p.pane_id.clone())
                    .collect()
            })
            .collect();
        Self::with_rows(snapshot, rows)
    }

    /// Explicit row membership is used when a vertical fork creates a second
    /// row: all originals stay together and the new pane occupies its own row.
    pub(crate) fn with_rows(snapshot: LayoutSnapshot, rows: Vec<Vec<String>>) -> Result<Self> {
        let heights = vec![1; rows.len()];
        let widths = rows.iter().map(|row| vec![1; row.len()]).collect();
        Self::with_sizes(snapshot, rows, heights, widths)
    }

    pub(crate) fn with_sizes(
        snapshot: LayoutSnapshot,
        rows: Vec<Vec<String>>,
        heights: Vec<usize>,
        widths: Vec<Vec<usize>>,
    ) -> Result<Self> {
        let plan = Self {
            snapshot,
            rows,
            heights,
            widths,
        };
        plan.validate()?;
        Ok(plan)
    }

    fn tree(&self) -> Result<SplitTree> {
        SplitTree::grid(&self.rows, &self.heights, &self.widths)
    }

    fn validate(&self) -> Result<()> {
        let Self {
            snapshot,
            rows,
            heights,
            widths,
        } = self;
        let mut ids = HashSet::new();
        if snapshot
            .panes
            .iter()
            .any(|pane| pane.pane_id.trim().is_empty() || !ids.insert(pane.pane_id.as_str()))
        {
            bail!("The source layout contains missing or duplicate pane IDs");
        }
        if !ids.contains(snapshot.focused_pane_id.as_str()) {
            bail!("The source layout has no matching focused pane");
        }
        let assigned = rows
            .iter()
            .flatten()
            .map(String::as_str)
            .collect::<HashSet<_>>();
        if rows.is_empty()
            || rows.iter().any(Vec::is_empty)
            || assigned != ids
            || rows.iter().map(Vec::len).sum::<usize>() != ids.len()
        {
            bail!("Rows must contain each source pane exactly once");
        }
        if heights.len() != rows.len()
            || widths.len() != rows.len()
            || heights.contains(&0)
            || rows
                .iter()
                .zip(widths)
                .any(|(row, sizes)| row.len() != sizes.len() || sizes.contains(&0))
        {
            bail!("Each row and pane must have a positive layout weight");
        }
        self.tree()?.validate_size(
            usize::from(snapshot.area.width),
            usize::from(snapshot.area.height),
        )
    }

    pub fn snapshot(&self) -> &LayoutSnapshot {
        &self.snapshot
    }

    pub fn rows(&self) -> &[Vec<String>] {
        &self.rows
    }

    pub fn pane_count(&self) -> usize {
        self.snapshot.panes.len()
    }
}
