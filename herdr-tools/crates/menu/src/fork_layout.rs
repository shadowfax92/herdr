//! Plan a fork's destination before creating its shell. Existing rows are
//! resized in place through split ratios. Trees whose rows cross column splits
//! use the process-preserving mover, retaining untouched rows as cell weights.

use std::collections::{BTreeMap, HashSet};
use std::path::Path;

use anyhow::{bail, Context, Result};
use serde::Deserialize;
use serde_json::{json, Value};

use crate::config::ForkPlacement;
use crate::herdr::Herdr;
use crate::redistribute::{ApplyOutcome, LayoutPlan, LayoutSnapshot, Redistributor};

#[derive(Clone, Debug, Deserialize, PartialEq)]
struct Export {
    workspace_id: String,
    tab_id: String,
    zoomed: bool,
    root: Node,
}

/// Only topology, IDs, and ratios participate in concurrency checks. Agent
/// commands, labels, and working directories may change while a fork is planned.
#[derive(Clone, Debug, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
enum Node {
    Pane {
        pane_id: String,
    },
    Split {
        direction: String,
        ratio: f32,
        first: Box<Node>,
        second: Box<Node>,
    },
}

/// Ordinary forks reuse edge rows. Irregular normalization may reserve a
/// standalone vertical edge when the tab is too narrow for adjacent panes.
#[derive(Clone, Copy, PartialEq)]
enum ForkRows {
    Reuse,
    StandaloneEdge,
}

/// Captures both the source and destination. All later mutations are checked
/// against this tree, so a focus change cannot redirect the fork to another tab.
pub(crate) struct ForkLayout {
    before: Export,
    snapshot: LayoutSnapshot,
    // Irregular layouts are normalized before shell creation. The normal fork
    // transaction then owns rollback from that stable arrangement onward.
    normalization: Option<LayoutPlan>,
    placement: ForkPlacement,
    row_mode: ForkRows,
    restore_zoom: bool,
    geometry: RowGeometry,
    source: String,
    rebuild: bool,
    rebuild_started: bool,
    rebuilt: bool,
    // A rollback may restore ratios only while this exact tree is still current.
    after: Option<Export>,
    row_path: Vec<bool>,
    originals: Vec<String>,
    new_row: bool,
    top: bool,
    pub target: String,
    pub direction: &'static str,
    pub swap: Option<(&'static str, &'static str)>,
}

/// Cleanup can remove the shell before a later geometry restore fails. Report
/// ownership separately so rotation tracking never points at a deleted fork.
pub(crate) struct ForkCleanup {
    pub pane_closed: bool,
    pub result: Result<()>,
}

impl ForkLayout {
    pub fn capture(herdr: &Herdr, source: &str, placement: ForkPlacement) -> Result<Self> {
        let before = export(herdr, source)?;
        let response: Value = herdr.json(["pane", "layout", "--pane", source])?;
        let snapshot: LayoutSnapshot =
            serde_json::from_value(response["result"]["layout"].clone())?;
        if export(herdr, source)? != before {
            bail!("Layout changed while planning the fork; reopen the menu");
        }
        Self::plan(before, snapshot, source, placement)
    }

    fn plan(
        before: Export,
        snapshot: LayoutSnapshot,
        source: &str,
        placement: ForkPlacement,
    ) -> Result<Self> {
        Self::plan_rows(before, snapshot, source, placement, ForkRows::Reuse)
    }

    fn plan_rows(
        before: Export,
        snapshot: LayoutSnapshot,
        source: &str,
        placement: ForkPlacement,
        row_mode: ForkRows,
    ) -> Result<Self> {
        let ids = snapshot
            .panes
            .iter()
            .map(|p| p.pane_id.as_str())
            .collect::<HashSet<_>>();
        let tree_ids = before.root.ids();
        if before.tab_id != snapshot.tab_id
            || before.workspace_id != snapshot.workspace_id
            || before.zoomed != snapshot.zoomed
            || tree_ids.len() != ids.len()
            || snapshot.panes.len() != ids.len()
            || tree_ids.iter().map(String::as_str).collect::<HashSet<_>>() != ids
        {
            bail!("Layout changed while planning the fork; reopen the menu");
        }
        validate_tiling(&snapshot)?;
        if RowGeometry::capture(&snapshot).is_err() {
            return Self::normalize(before, snapshot, source, placement);
        }
        let source_pane = snapshot
            .panes
            .iter()
            .find(|p| p.pane_id == source)
            .context("Source pane is absent from its layout")?;
        let mut rows = BTreeMap::new();
        for pane in &snapshot.panes {
            rows.entry((pane.rect.y, pane.rect.height))
                .or_insert_with(Vec::new)
                .push(pane);
        }
        for row in rows.values_mut() {
            row.sort_by_key(|p| p.rect.x);
        }
        let vertical = matches!(placement, ForkPlacement::Top | ForkPlacement::Bottom);
        let top = placement == ForkPlacement::Top;
        let row = match placement {
            ForkPlacement::Top => rows.first_key_value().unwrap().1,
            ForkPlacement::Bottom => rows.last_key_value().unwrap().1,
            _ => &rows[&(source_pane.rect.y, source_pane.rect.height)],
        };
        let originals = row.iter().map(|p| p.pane_id.clone()).collect::<Vec<_>>();
        let geometry = RowGeometry::capture(&snapshot)?;
        let row_path = before.root.row_path(&originals);
        let rebuild = row_path.is_none();
        let row_path = row_path.unwrap_or_default();
        let new_row = vertical && (rows.len() == 1 || row_mode == ForkRows::StandaloneEdge);
        let target = if row_mode == ForkRows::StandaloneEdge {
            originals[0].clone()
        } else if !vertical || new_row {
            source.to_owned()
        } else {
            // Keep a vertical fork near the source column even when rows have
            // different pane counts. Ties prefer the leftmost existing pane.
            let center = u32::from(source_pane.rect.x) * 2 + u32::from(source_pane.rect.width);
            row.iter()
                .min_by_key(|p| {
                    (u32::from(p.rect.x) * 2 + u32::from(p.rect.width)).abs_diff(center)
                })
                .unwrap()
                .pane_id
                .clone()
        };
        let columns = originals.len() + usize::from(!new_row);
        let width: usize = row.iter().map(|p| usize::from(p.rect.width)).sum();
        let height = usize::from(row[0].rect.height) / if new_row { 2 } else { 1 };
        if width / columns < 12 || height < 5 {
            bail!("Fork would make the row too small (minimum 12 columns × 5 lines per pane)");
        }
        let mut plan = Self {
            restore_zoom: before.zoomed,
            before,
            snapshot,
            normalization: None,
            placement,
            row_mode,
            geometry,
            source: source.into(),
            rebuild,
            rebuild_started: false,
            rebuilt: false,
            after: None,
            row_path,
            originals,
            new_row,
            top,
            target,
            direction: if new_row { "down" } else { "right" },
            swap: if placement == ForkPlacement::Left {
                Some(("left", "left"))
            } else if new_row && top {
                Some(("up", "top"))
            } else {
                None
            },
        };
        // A skewed historical tree can exceed Herdr's ratio clamp. Rebuild
        // that row with balanced branches instead of depending on split history.
        if !new_row && !plan.rebuild {
            plan.rebuild = plan
                .expected("new-fork")
                .root
                .at(&plan.row_path)
                .unwrap()
                .ratios(&plan.row_path, &mut Vec::new())
                .is_err();
        }
        Ok(plan)
    }

    /// Read-order packing reserves a cell for the requested fork. Prefer the
    /// existing number of visual bands, then try other usable row counts once.
    /// Reserving before packing also handles tabs that only just fit N+1 panes.
    fn normalize(
        before: Export,
        snapshot: LayoutSnapshot,
        source: &str,
        placement: ForkPlacement,
    ) -> Result<Self> {
        let mut panes = snapshot.panes.clone();
        panes.sort_by(|a, b| {
            (a.rect.y, a.rect.x, &a.pane_id).cmp(&(b.rect.y, b.rect.x, &b.pane_id))
        });
        let preferred = panes.iter().map(|p| p.rect.y).collect::<HashSet<_>>().len();
        let count = panes.len();
        let mut row_counts =
            (1..=count.min(usize::from(snapshot.area.height) / 5)).collect::<Vec<_>>();
        row_counts.sort_by_key(|rows| (rows.abs_diff(preferred), *rows));
        for row_count in row_counts {
            for relocate_source in [false, true] {
                for reserved in 0..row_count {
                    if (placement == ForkPlacement::Top && reserved != 0)
                        || (placement == ForkPlacement::Bottom && reserved != row_count - 1)
                    {
                        continue;
                    }
                    let mut sizes = (0..row_count)
                        .map(|row| {
                            (count + 1) / row_count + usize::from(row < (count + 1) % row_count)
                        })
                        .collect::<Vec<_>>();
                    // A short final row still needs an original beside its
                    // reserved fork. Borrow a cell from a longer row first.
                    if sizes[reserved] == 1 {
                        if let Some(donor) = sizes.iter().position(|size| *size > 1) {
                            sizes[donor] -= 1;
                            sizes[reserved] += 1;
                        }
                    }
                    sizes[reserved] -= 1;
                    let mut offset = 0;
                    let mut rows = Vec::new();
                    for size in sizes {
                        rows.push(
                            panes[offset..offset + size]
                                .iter()
                                .map(|p| p.pane_id.clone())
                                .collect::<Vec<_>>(),
                        );
                        offset += size;
                    }
                    if rows.iter().any(Vec::is_empty) {
                        continue;
                    }
                    if matches!(placement, ForkPlacement::Left | ForkPlacement::Right)
                        && !rows[reserved].iter().any(|id| id == source)
                    {
                        if !relocate_source {
                            continue;
                        }
                        // At capacity, reading order may straddle the only free
                        // cell. Move the source to that row, retaining peer order.
                        let sizes = rows.iter().map(Vec::len).collect::<Vec<_>>();
                        let start: usize = sizes[..reserved].iter().sum();
                        let mut ids = rows.into_iter().flatten().collect::<Vec<_>>();
                        let old = ids
                            .iter()
                            .position(|id| id == source)
                            .context("Source pane is absent from its layout")?;
                        let id = ids.remove(old);
                        ids.insert(old.clamp(start, start + sizes[reserved] - 1), id);
                        let mut ids = ids.into_iter();
                        rows = sizes
                            .into_iter()
                            .map(|size| ids.by_ref().take(size).collect())
                            .collect();
                    }
                    let Ok(normalization) = LayoutPlan::with_rows(snapshot.clone(), rows) else {
                        continue;
                    };
                    let preview = normalization.preview()?;
                    let mut unzoomed = before.clone();
                    unzoomed.zoomed = false;
                    let Ok(mut plan) = Self::plan(unzoomed, preview, source, placement) else {
                        continue;
                    };
                    plan.restore_zoom = before.zoomed;
                    plan.before = before;
                    plan.snapshot = snapshot;
                    plan.normalization = Some(normalization);
                    return Ok(plan);
                }
            }
        }
        if matches!(placement, ForkPlacement::Top | ForkPlacement::Bottom) {
            // A resized tab may have only one usable column. Reserve two cells
            // of height for the edge original, then split it into two rows.
            // This preflights N+1 usable rows without creating a placeholder or
            // changing the established behavior of existing full-row layouts.
            let mut heights = vec![1; count];
            heights[if placement == ForkPlacement::Top {
                0
            } else {
                count - 1
            }] = 2;
            let rows = panes
                .iter()
                .map(|pane| vec![pane.pane_id.clone()])
                .collect();
            if let Ok(normalization) =
                LayoutPlan::with_sizes(snapshot.clone(), rows, heights, vec![vec![1]; count])
            {
                let mut unzoomed = before.clone();
                unzoomed.zoomed = false;
                if let Ok(mut plan) = Self::plan_rows(
                    unzoomed,
                    normalization.preview()?,
                    source,
                    placement,
                    ForkRows::StandaloneEdge,
                ) {
                    plan.restore_zoom = before.zoomed;
                    plan.before = before;
                    plan.snapshot = snapshot;
                    plan.normalization = Some(normalization);
                    return Ok(plan);
                }
            }
        }
        bail!("Fork would make panes too small (minimum 12 columns × 5 lines per pane)")
    }

    pub fn prepare(&mut self, herdr: &Herdr, cwd: &Path, state_dir: &Path) -> Result<()> {
        if export(herdr, &self.target)? != self.before {
            bail!("Layout changed before forking; reopen the menu");
        }
        let mover = Redistributor::new(herdr.clone(), &self.source, cwd, state_dir);
        let response: Value = herdr.json(["pane", "layout", "--pane", &self.source])?;
        let current: LayoutSnapshot = serde_json::from_value(response["result"]["layout"].clone())?;
        if !same_geometry(&current, &self.snapshot) {
            bail!("Layout changed before forking; reopen the menu");
        }
        if let Some(normalization) = &self.normalization {
            // capture() adds the tab label used by Redistributor's freshness
            // check. Never replace an old snapshot with an unchecked fresh one.
            let current = mover.capture()?;
            if !same_geometry(&current, &self.snapshot)
                || export(herdr, &self.source)? != self.before
            {
                bail!("Layout changed before normalizing the fork; reopen the menu");
            }
            let plan = normalization.with_snapshot(current)?;
            let preview = plan.preview()?;
            match mover.apply(&plan)? {
                ApplyOutcome::Applied => {}
                ApplyOutcome::Refresh(_) => {
                    bail!("Layout changed before normalizing the fork; reopen the menu")
                }
            }
            let normalized = mover.capture()?;
            if !same_geometry(&normalized, &preview) {
                bail!("Layout changed after normalizing the fork; no fork was created");
            }
            let restore_zoom = self.restore_zoom;
            *self = Self::plan_rows(
                export(herdr, &self.source)?,
                normalized,
                &self.source,
                self.placement,
                self.row_mode,
            )?;
            self.restore_zoom = restore_zoom;
            // The normal transaction begins only after normalization succeeded.
            // Recheck its new snapshot immediately before creating the shell.
            return self.prepare(herdr, cwd, state_dir);
        }
        if self.before.zoomed {
            herdr.output(["pane", "zoom", "--pane", &self.target, "--off"])?;
        }
        Ok(())
    }

    pub fn finish(
        &mut self,
        herdr: &Herdr,
        fork: &str,
        cwd: &Path,
        state_dir: &Path,
    ) -> Result<()> {
        let mut expected = self.expected(fork);
        if export(herdr, fork)? != expected {
            bail!("Layout changed while creating the fork; no row ratios were changed");
        }
        self.after = Some(expected.clone());
        if (self.new_row && self.originals.len() > 1) || self.rebuild {
            // A full-width row cannot be expressed by splitting one leaf in a
            // column-first tree. Move the existing terminals into a row tree;
            // only the destination row gets new widths, all other cells persist.
            let mover = Redistributor::new(herdr.clone(), fork, cwd, state_dir);
            let snapshot = mover.capture()?;
            let plan = if self.new_row {
                let rows = if self.top {
                    vec![vec![fork.into()], self.originals.clone()]
                } else {
                    vec![self.originals.clone(), vec![fork.into()]]
                };
                LayoutPlan::with_rows(snapshot, rows)?
            } else {
                let mut geometry = self.geometry.clone();
                let row = geometry
                    .rows
                    .iter()
                    .position(|row| row == &self.originals)
                    .unwrap();
                let index = geometry.rows[row]
                    .iter()
                    .position(|id| id == &self.target)
                    .unwrap();
                geometry.rows[row].insert(index + usize::from(self.swap.is_none()), fork.into());
                geometry.widths[row] = vec![1; geometry.rows[row].len()];
                geometry.plan(snapshot)?
            };
            self.rebuild_started = true;
            match mover.apply(&plan)? {
                ApplyOutcome::Applied => {}
                ApplyOutcome::Refresh(_) => {
                    self.rebuild_started = false;
                    bail!("Layout changed before arranging the fork row; reopen the menu")
                }
            }
            self.rebuilt = true;
            self.after = Some(export(herdr, fork)?);
        } else if !self.new_row {
            let mut updates = Vec::new();
            expected
                .root
                .at(&self.row_path)
                .context("Destination row disappeared")?
                .ratios(&self.row_path, &mut updates)?;
            for (path, ratio) in updates {
                // Paths identify tree positions, not panes. Recheck before each
                // write so another client's split cannot redirect a pending ratio.
                if export(herdr, fork)? != expected {
                    bail!("Layout changed while balancing the fork row; stopped resizing");
                }
                herdr.layout_request(
                    "layout.set_split_ratio",
                    json!({"tab_id": expected.tab_id, "path": path, "ratio": ratio}),
                )?;
                expected.root.set_ratio(&path, ratio);
                self.after = Some(expected.clone());
            }
            if export(herdr, fork)? != expected {
                bail!("Layout changed after balancing the fork row");
            }
        }
        herdr.output(["pane", "zoom", "--pane", fork, "--off"])?;
        Ok(())
    }

    /// Undo only this attempt. Failed staging can leave the empty fork as the
    /// source tab's sole anchor; keeping it is necessary for manual recovery.
    /// Original terminals are never closed, including on cleanup failures.
    pub fn cleanup(&self, herdr: &Herdr, fork: &str, cwd: &Path, state_dir: &Path) -> ForkCleanup {
        let mut pane_closed = false;
        let result = (|| {
            let mut current = export(herdr, fork).context("checking fork rollback ownership")?;
            if current.tab_id == self.before.tab_id && current.root.ids() == [fork] {
                bail!(
                "Kept empty fork {fork} to preserve Source {} while original panes are elsewhere",
                self.before.tab_id
            );
            }
            if self.rebuild_started && !self.rebuilt {
                herdr
                    .close_pane(fork)
                    .context("closing the empty fork after interrupted staging")?;
                pane_closed = true;
                return Ok(());
            }
            if let Some(after) = &self.after {
                if &current != after {
                    // The fork ID is ours, but tree paths may now belong to another
                    // client's splits. Remove only our pane and report skipped undo.
                    herdr.close_pane(fork)?;
                    pane_closed = true;
                    bail!("Layout changed during fork rollback; closed {fork} without overwriting concurrent layout changes");
                }
                if !self.rebuilt {
                    let mut ratios = Vec::new();
                    self.before.root.original_ratios(&[], &mut ratios);
                    for (path, ratio) in ratios {
                        if export(herdr, fork)? != current {
                            bail!("Layout changed during rollback; kept fork {fork} for recovery");
                        }
                        let Some(Node::Split { ratio: actual, .. }) = current.root.at(&path) else {
                            bail!("Original split disappeared during rollback; kept fork {fork}");
                        };
                        if *actual != ratio {
                            herdr.layout_request(
                                "layout.set_split_ratio",
                                json!({"tab_id": current.tab_id, "path": path, "ratio": ratio}),
                            )?;
                            current.root.set_ratio(&path, ratio);
                        }
                    }
                }
            }
            herdr
                .close_pane(fork)
                .context("closing newly-created fork pane")?;
            pane_closed = true;
            if self.rebuilt {
                // A completed rebuild has different paths. Restore original cell
                // geometry through live moves instead of applying stale tree paths.
                let mover = Redistributor::new(herdr.clone(), &self.source, cwd, state_dir);
                let plan = self.geometry.plan(mover.capture()?)?;
                if let ApplyOutcome::Refresh(_) = mover.apply(&plan)? {
                    bail!("Layout changed before restoring the original fork layout");
                }
            }
            if self.restore_zoom {
                herdr.output(["pane", "zoom", "--pane", &self.source, "--on"])?;
            }
            Ok(())
        })();
        ForkCleanup {
            pane_closed,
            result,
        }
    }

    fn expected(&self, fork: &str) -> Export {
        let mut expected = self.before.clone();
        expected.zoomed = false;
        expected
            .root
            .insert(&self.target, fork, self.direction, self.swap.is_some());
        expected
    }
}

fn same_geometry(a: &LayoutSnapshot, b: &LayoutSnapshot) -> bool {
    let sorted = |snapshot: &LayoutSnapshot| {
        let mut panes = snapshot.panes.clone();
        panes.sort_by(|a, b| a.pane_id.cmp(&b.pane_id));
        panes
    };
    a.workspace_id == b.workspace_id
        && a.tab_id == b.tab_id
        && a.area == b.area
        && a.zoomed == b.zoomed
        && sorted(a) == sorted(b)
}

/// Shape normalization is allowed only for a complete tiling. Gaps, overlaps,
/// or out-of-bounds rectangles indicate a stale or invalid host snapshot.
fn validate_tiling(snapshot: &LayoutSnapshot) -> Result<()> {
    let area = snapshot.area;
    let right = u32::from(area.x) + u32::from(area.width);
    let bottom = u32::from(area.y) + u32::from(area.height);
    let mut cells = 0u64;
    for (index, pane) in snapshot.panes.iter().enumerate() {
        let r = pane.rect;
        let r_right = u32::from(r.x) + u32::from(r.width);
        let r_bottom = u32::from(r.y) + u32::from(r.height);
        if r.width == 0
            || r.height == 0
            || r.x < area.x
            || r.y < area.y
            || r_right > right
            || r_bottom > bottom
        {
            bail!("Invalid fork layout geometry; reopen the menu");
        }
        for other in &snapshot.panes[..index] {
            let o = other.rect;
            if u32::from(r.x) < u32::from(o.x) + u32::from(o.width)
                && u32::from(o.x) < r_right
                && u32::from(r.y) < u32::from(o.y) + u32::from(o.height)
                && u32::from(o.y) < r_bottom
            {
                bail!("Overlapping fork layout geometry; reopen the menu");
            }
        }
        cells += u64::from(r.width) * u64::from(r.height);
    }
    if cells != u64::from(area.width) * u64::from(area.height) {
        bail!("Incomplete fork layout geometry; reopen the menu");
    }
    Ok(())
}

/// Visual rows are the user-facing contract, independent of split-tree shape.
/// Cell weights preserve boundaries exactly when normalization changes topology.
#[derive(Clone)]
struct RowGeometry {
    rows: Vec<Vec<String>>,
    heights: Vec<usize>,
    widths: Vec<Vec<usize>>,
}

impl RowGeometry {
    fn capture(snapshot: &LayoutSnapshot) -> Result<Self> {
        let mut bands = BTreeMap::new();
        for pane in &snapshot.panes {
            bands
                .entry((pane.rect.y, pane.rect.height))
                .or_insert_with(Vec::new)
                .push(pane);
        }
        let mut geometry = Self {
            rows: Vec::new(),
            heights: Vec::new(),
            widths: Vec::new(),
        };
        let mut y = usize::from(snapshot.area.y);
        for ((top, height), mut panes) in bands {
            panes.sort_by_key(|pane| pane.rect.x);
            let mut x = usize::from(snapshot.area.x);
            if usize::from(top) != y || height == 0 {
                bail!("Fork needs full-width rows; use Redistribute to arrange this irregular layout first");
            }
            for pane in &panes {
                if usize::from(pane.rect.x) != x || pane.rect.width == 0 {
                    bail!("Fork needs full-width rows; use Redistribute to arrange this irregular layout first");
                }
                x += usize::from(pane.rect.width);
            }
            if x != usize::from(snapshot.area.x) + usize::from(snapshot.area.width) {
                bail!("Fork needs full-width rows; use Redistribute to arrange this irregular layout first");
            }
            y += usize::from(height);
            geometry
                .rows
                .push(panes.iter().map(|pane| pane.pane_id.clone()).collect());
            geometry.heights.push(usize::from(height));
            geometry.widths.push(
                panes
                    .iter()
                    .map(|pane| usize::from(pane.rect.width))
                    .collect(),
            );
        }
        if y != usize::from(snapshot.area.y) + usize::from(snapshot.area.height) {
            bail!("Fork row geometry does not cover the source tab");
        }
        Ok(geometry)
    }

    fn plan(&self, snapshot: LayoutSnapshot) -> Result<LayoutPlan> {
        LayoutPlan::with_sizes(
            snapshot,
            self.rows.clone(),
            self.heights.clone(),
            self.widths.clone(),
        )
    }
}

fn export(herdr: &Herdr, pane: &str) -> Result<Export> {
    let response = herdr.layout_request("layout.export", json!({"pane_id": pane}))?;
    serde_json::from_value(response["layout"].clone()).context("Invalid exported fork layout")
}

impl Node {
    fn ids(&self) -> Vec<String> {
        match self {
            Self::Pane { pane_id } => vec![pane_id.clone()],
            Self::Split { first, second, .. } => {
                let mut ids = first.ids();
                ids.extend(second.ids());
                ids
            }
        }
    }

    fn horizontal(&self) -> bool {
        match self {
            Self::Pane { .. } => true,
            Self::Split {
                direction,
                first,
                second,
                ..
            } => direction == "right" && first.horizontal() && second.horizontal(),
        }
    }

    fn row_path(&self, ids: &[String]) -> Option<Vec<bool>> {
        if self.horizontal() && self.ids() == ids {
            return Some(Vec::new());
        }
        if let Self::Split { first, second, .. } = self {
            for (side, child) in [(false, first), (true, second)] {
                if let Some(mut path) = child.row_path(ids) {
                    path.insert(0, side);
                    return Some(path);
                }
            }
        }
        None
    }

    fn at(&self, path: &[bool]) -> Option<&Self> {
        if path.is_empty() {
            return Some(self);
        }
        if let Self::Split { first, second, .. } = self {
            (if path[0] { second } else { first }).at(&path[1..])
        } else {
            None
        }
    }

    fn insert(&mut self, target: &str, fork: &str, direction: &str, before: bool) {
        match self {
            Self::Pane { pane_id } if pane_id == target => {
                let old = Box::new(self.clone());
                let new = Box::new(Self::Pane {
                    pane_id: fork.into(),
                });
                let (first, second) = if before { (new, old) } else { (old, new) };
                *self = Self::Split {
                    direction: direction.into(),
                    ratio: 0.5,
                    first,
                    second,
                };
            }
            Self::Split { first, second, .. } => {
                first.insert(target, fork, direction, before);
                second.insert(target, fork, direction, before);
            }
            _ => {}
        }
    }

    fn ratios(&self, path: &[bool], updates: &mut Vec<(Vec<bool>, f32)>) -> Result<usize> {
        let Self::Split {
            first,
            second,
            ratio,
            ..
        } = self
        else {
            return Ok(1);
        };
        let mut first_path = path.to_vec();
        first_path.push(false);
        let mut second_path = path.to_vec();
        second_path.push(true);
        let left = first.ratios(&first_path, updates)?;
        let right = second.ratios(&second_path, updates)?;
        let target = left as f32 / (left + right) as f32;
        if !(0.1..=0.9).contains(&target) {
            bail!("Row split tree needs balanced branches");
        }
        if (*ratio - target).abs() > 0.00001 {
            updates.push((path.to_vec(), target));
        }
        Ok(left + right)
    }

    fn original_ratios(&self, path: &[bool], updates: &mut Vec<(Vec<bool>, f32)>) {
        if let Self::Split {
            ratio,
            first,
            second,
            ..
        } = self
        {
            updates.push((path.to_vec(), *ratio));
            for (side, child) in [(false, first), (true, second)] {
                let mut child_path = path.to_vec();
                child_path.push(side);
                child.original_ratios(&child_path, updates);
            }
        }
    }

    fn set_ratio(&mut self, path: &[bool], ratio: f32) {
        if let Self::Split {
            first,
            second,
            ratio: current,
            ..
        } = self
        {
            if path.is_empty() {
                *current = ratio;
            } else {
                (if path[0] { second } else { first }).set_ratio(&path[1..], ratio);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(start: usize, count: usize) -> Node {
        if count == 1 {
            return Node::Pane {
                pane_id: format!("w1:p{start}"),
            };
        }
        let middle = count.div_ceil(2);
        Node::Split {
            direction: "right".into(),
            ratio: middle as f32 / count as f32,
            first: Box::new(row(start, middle)),
            second: Box::new(row(start + middle, count - middle)),
        }
    }

    fn fixture(two_rows: bool) -> (Export, LayoutSnapshot) {
        let root = if two_rows {
            Node::Split {
                direction: "down".into(),
                ratio: 0.4,
                first: Box::new(row(1, 5)),
                second: Box::new(row(6, 5)),
            }
        } else {
            row(1, 5)
        };
        let snapshot = serde_json::from_value(json!({
            "workspace_id":"w1", "tab_id":"w1:t1", "focused_pane_id":"w1:p3", "zoomed":false,
            "area":{"x":0,"y":0,"width":240,"height":60},
            "panes":(1..=if two_rows {10} else {5}).map(|i| json!({"pane_id":format!("w1:p{i}"),
                "rect":{"x":((i-1)%5)*48,"y":if i>5 {24} else {0},"width":48,
                    "height":if !two_rows {60} else if i>5 {36} else {24}}})).collect::<Vec<_>>()
        }))
        .unwrap();
        (
            Export {
                workspace_id: "w1".into(),
                tab_id: "w1:t1".into(),
                zoomed: false,
                root,
            },
            snapshot,
        )
    }

    fn t_fixture() -> (Export, LayoutSnapshot) {
        let before = Export {
            workspace_id: "w1".into(),
            tab_id: "w1:t1".into(),
            zoomed: false,
            root: Node::Split {
                direction: "right".into(),
                ratio: 0.5,
                first: Box::new(row(1, 1)),
                second: Box::new(Node::Split {
                    direction: "down".into(),
                    ratio: 0.5,
                    first: Box::new(row(2, 1)),
                    second: Box::new(row(3, 1)),
                }),
            },
        };
        let snapshot = serde_json::from_value(json!({
            "workspace_id":"w1", "tab_id":"w1:t1", "focused_pane_id":"w1:p1", "zoomed":false,
            "area":{"x":0,"y":0,"width":120,"height":40},
            "panes":[
                {"pane_id":"w1:p1","rect":{"x":0,"y":0,"width":60,"height":40}},
                {"pane_id":"w1:p2","rect":{"x":60,"y":0,"width":60,"height":20}},
                {"pane_id":"w1:p3","rect":{"x":60,"y":20,"width":60,"height":20}}
            ]
        }))
        .unwrap();
        (before, snapshot)
    }

    #[test]
    fn tests_that_screenshot_t_layout_can_fork_left() {
        let (before, snapshot) = t_fixture();
        ForkLayout::plan(before, snapshot, "w1:p1", ForkPlacement::Left)
            .unwrap_or_else(|error| panic!("valid T-shaped fork was rejected: {error:#}"));
    }

    #[test]
    fn tests_that_irregular_forks_use_all_available_cells() {
        for width in [24, 120] {
            for zoomed in [false, true] {
                for source in ["w1:p1", "w1:p2", "w1:p3"] {
                    for placement in [
                        ForkPlacement::Left,
                        ForkPlacement::Right,
                        ForkPlacement::Top,
                        ForkPlacement::Bottom,
                    ] {
                        let (mut before, mut snapshot) = t_fixture();
                        before.zoomed = zoomed;
                        snapshot.zoomed = zoomed;
                        snapshot.focused_pane_id = source.into();
                        snapshot.area.width = width;
                        snapshot.area.height = 10;
                        for pane in &mut snapshot.panes {
                            pane.rect.x = pane.rect.x * width / 120;
                            pane.rect.width = pane.rect.width * width / 120;
                            pane.rect.y /= 4;
                            pane.rect.height /= 4;
                        }
                        let plan = ForkLayout::plan(before, snapshot, source, placement)
                            .unwrap_or_else(|e| panic!("width={width} zoom={zoomed} source={source} {placement:?}: {e:#}"));
                        assert!(plan.normalization.is_some());
                        assert_eq!(plan.restore_zoom, zoomed);
                        assert_eq!(plan.source, source);
                    }
                }
            }
        }
    }

    #[test]
    fn tests_that_bottom_fork_fits_a_short_last_row() {
        let (mut before, mut snapshot) = t_fixture();
        before.root = row(1, 6);
        snapshot.area.width = 24;
        snapshot.area.height = 20;
        snapshot.panes = serde_json::from_value(json!([
            {"pane_id":"w1:p1","rect":{"x":0,"y":0,"width":12,"height":20}},
            {"pane_id":"w1:p2","rect":{"x":12,"y":0,"width":12,"height":4}},
            {"pane_id":"w1:p3","rect":{"x":12,"y":4,"width":12,"height":4}},
            {"pane_id":"w1:p4","rect":{"x":12,"y":8,"width":12,"height":4}},
            {"pane_id":"w1:p5","rect":{"x":12,"y":12,"width":12,"height":4}},
            {"pane_id":"w1:p6","rect":{"x":12,"y":16,"width":12,"height":4}}
        ]))
        .unwrap();
        ForkLayout::plan(before, snapshot, "w1:p1", ForkPlacement::Bottom)
            .unwrap_or_else(|e| panic!("seven panes fit into four 24x5 rows: {e:#}"));
    }

    #[test]
    fn tests_that_usable_capacity_accepts_irregular_tilings() {
        for count in 3usize..=12 {
            for columns in 1usize..=4 {
                let width = (columns * 12) as u16;
                let height = ((count + 1).div_ceil(columns).max(2) * 5) as u16;
                let (mut before, mut snapshot) = t_fixture();
                before.root = row(1, count);
                snapshot.area.width = width;
                snapshot.area.height = height;
                snapshot.panes.clear();
                snapshot.panes.push(crate::redistribute::LayoutPane {
                    pane_id: "w1:p1".into(),
                    rect: crate::redistribute::PaneRect {
                        x: 0,
                        y: 0,
                        width: width / 2,
                        height,
                    },
                });
                for index in 0..count - 1 {
                    let top = usize::from(height) * index / (count - 1);
                    let bottom = usize::from(height) * (index + 1) / (count - 1);
                    snapshot.panes.push(crate::redistribute::LayoutPane {
                        pane_id: format!("w1:p{}", index + 2),
                        rect: crate::redistribute::PaneRect {
                            x: width / 2,
                            y: top as u16,
                            width: width / 2,
                            height: (bottom - top) as u16,
                        },
                    });
                }
                for source in [1, count / 2, count] {
                    for placement in [
                        ForkPlacement::Left,
                        ForkPlacement::Right,
                        ForkPlacement::Top,
                        ForkPlacement::Bottom,
                    ] {
                        if columns == 1
                            && matches!(placement, ForkPlacement::Left | ForkPlacement::Right)
                        {
                            continue;
                        }
                        ForkLayout::plan(before.clone(), snapshot.clone(), &format!("w1:p{source}"), placement)
                            .unwrap_or_else(|e| panic!("count={count} width={width} height={height} source={source} {placement:?}: {e:#}"));
                    }
                }
            }
        }
    }

    #[test]
    fn tests_that_irregular_forks_reject_insufficient_or_invalid_area() {
        let (before, mut snapshot) = t_fixture();
        snapshot.area.width = 12;
        snapshot.area.height = 10;
        for pane in &mut snapshot.panes {
            pane.rect.x /= 10;
            pane.rect.width /= 10;
            pane.rect.y /= 4;
            pane.rect.height /= 4;
        }
        for placement in [
            ForkPlacement::Left,
            ForkPlacement::Top,
            ForkPlacement::Bottom,
        ] {
            let error = ForkLayout::plan(before.clone(), snapshot.clone(), "w1:p1", placement)
                .err()
                .unwrap();
            assert!(error.to_string().contains("too small"), "{error}");
        }
        let (before, mut snapshot) = t_fixture();
        snapshot.panes[1].rect.x = 0;
        assert!(ForkLayout::plan(before, snapshot, "w1:p1", ForkPlacement::Left).is_err());
    }

    #[test]
    fn directions_reuse_destination_rows_and_keep_other_tree_unchanged() {
        for (source, placement, path, target, expected_order) in [
            (
                "w1:p3",
                ForkPlacement::Left,
                false,
                "w1:p3",
                vec!["w1:p1", "w1:p2", "fork", "w1:p3", "w1:p4", "w1:p5"],
            ),
            (
                "w1:p3",
                ForkPlacement::Right,
                false,
                "w1:p3",
                vec!["w1:p1", "w1:p2", "w1:p3", "fork", "w1:p4", "w1:p5"],
            ),
            (
                "w1:p8",
                ForkPlacement::Left,
                true,
                "w1:p8",
                vec!["w1:p6", "w1:p7", "fork", "w1:p8", "w1:p9", "w1:p10"],
            ),
            (
                "w1:p3",
                ForkPlacement::Bottom,
                true,
                "w1:p8",
                vec!["w1:p6", "w1:p7", "w1:p8", "fork", "w1:p9", "w1:p10"],
            ),
            (
                "w1:p8",
                ForkPlacement::Top,
                false,
                "w1:p3",
                vec!["w1:p1", "w1:p2", "w1:p3", "fork", "w1:p4", "w1:p5"],
            ),
            (
                "w1:p8",
                ForkPlacement::Bottom,
                true,
                "w1:p8",
                vec!["w1:p6", "w1:p7", "w1:p8", "fork", "w1:p9", "w1:p10"],
            ),
            (
                "w1:p3",
                ForkPlacement::Top,
                false,
                "w1:p3",
                vec!["w1:p1", "w1:p2", "w1:p3", "fork", "w1:p4", "w1:p5"],
            ),
        ] {
            let (before, snapshot) = fixture(true);
            let plan = ForkLayout::plan(before.clone(), snapshot, source, placement).unwrap();
            assert!(!plan.new_row);
            assert_eq!(plan.target, target);
            assert_eq!(plan.direction, "right");
            assert_eq!(plan.row_path, vec![path]);
            let mut expected = plan.expected("fork");
            assert_eq!(expected.root.at(&[path]).unwrap().ids(), expected_order);
            let mut updates = Vec::new();
            expected
                .root
                .at(&[path])
                .unwrap()
                .ratios(&[path], &mut updates)
                .unwrap();
            for (path, ratio) in updates {
                expected.root.set_ratio(&path, ratio);
            }
            assert_eq!(expected.root.at(&[!path]), before.root.at(&[!path]));
            let Node::Split { ratio, .. } = expected.root else {
                panic!()
            };
            assert_eq!(ratio, 0.4, "row heights stay unchanged");
        }
    }

    #[test]
    fn single_row_vertical_fork_requests_a_second_row() {
        for placement in [ForkPlacement::Top, ForkPlacement::Bottom] {
            let (before, snapshot) = fixture(false);
            let plan = ForkLayout::plan(before, snapshot, "w1:p3", placement).unwrap();
            assert!(plan.new_row);
            assert_eq!(plan.direction, "down");
            assert_eq!(plan.originals.len(), 5);
        }
    }

    #[test]
    fn small_rows_and_stale_membership_fail_before_pane_creation() {
        let (before, mut snapshot) = fixture(true);
        snapshot.panes[0].pane_id = "foreign".into();
        assert!(ForkLayout::plan(before, snapshot, "w1:p3", ForkPlacement::Right).is_err());
        let (before, mut snapshot) = fixture(true);
        for pane in &mut snapshot.panes {
            pane.rect.width = 12;
        }
        assert!(ForkLayout::plan(before, snapshot, "w1:p3", ForkPlacement::Right).is_err());
    }
}
