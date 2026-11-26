use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use crate::app::SessionId;

/// Minimum allowed split ratio (`first` fraction).
pub const MIN_SPLIT_RATIO: f32 = 0.1;

/// Maximum allowed split ratio (`first` fraction).
pub const MAX_SPLIT_RATIO: f32 = 0.9;

/// Default split ratio for newly created binary splits.
pub const DEFAULT_SPLIT_RATIO: f32 = 0.5;

/// Unique identifier for a workspace tab.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct TabId(pub u64);

/// Unique identifier for a single terminal pane within a workspace tab.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct PaneId(pub u64);

/// Unique identifier for a binary split divider inside a workspace tab.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct SplitId(pub u64);

/// Axis along which a pane is split.
///
/// - [`SplitAxis::Vertical`] places `first` on the left and `second` on the right (vertical divider).
/// - [`SplitAxis::Horizontal`] places `first` on top and `second` on the bottom (horizontal divider).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SplitAxis {
    Horizontal,
    Vertical,
}

/// Direction for cyclic tab or pane navigation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CycleDirection {
    Next,
    Previous,
}

/// Spatial direction for moving focus between adjacent panes inside a split tree.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FocusDirection {
    Left,
    Right,
    Up,
    Down,
}

/// Outcome returned when closing a pane within a [`Workspace`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClosePaneOutcome {
    /// Closed one pane inside a split tab; the tab remains open with `focused_pane` active.
    ClosedPane {
        session_id: SessionId,
        focused_pane: PaneId,
    },
    /// Closed the final pane in the tab, closing the entire tab.
    ClosedTab { session_id: SessionId },
}

/// Binary split-tree node representing either a leaf pane or a split container.
#[derive(Debug, Clone, PartialEq)]
pub enum SplitNode {
    Leaf {
        pane_id: PaneId,
        session_id: SessionId,
    },
    Split {
        split_id: SplitId,
        axis: SplitAxis,
        /// Fraction of space allocated to `first` in `[MIN_SPLIT_RATIO, MAX_SPLIT_RATIO]`.
        ratio: f32,
        first: Box<SplitNode>,
        second: Box<SplitNode>,
    },
}

impl SplitNode {
    /// Returns the total number of leaf panes in this subtree.
    pub fn pane_count(&self) -> usize {
        match self {
            Self::Leaf { .. } => 1,
            Self::Split { first, second, .. } => first.pane_count() + second.pane_count(),
        }
    }

    /// Returns all `PaneId`s in left-to-right / top-to-bottom traversal order.
    pub fn pane_ids(&self) -> Vec<PaneId> {
        let mut out = Vec::new();
        self.collect_pane_ids(&mut out);
        out
    }

    fn collect_pane_ids(&self, out: &mut Vec<PaneId>) {
        match self {
            Self::Leaf { pane_id, .. } => out.push(*pane_id),
            Self::Split { first, second, .. } => {
                first.collect_pane_ids(out);
                second.collect_pane_ids(out);
            }
        }
    }

    /// Returns all `SessionId`s hosted inside this subtree in traversal order.
    pub fn session_ids(&self) -> Vec<SessionId> {
        let mut out = Vec::new();
        self.collect_session_ids(&mut out);
        out
    }

    fn collect_session_ids(&self, out: &mut Vec<SessionId>) {
        match self {
            Self::Leaf { session_id, .. } => out.push(*session_id),
            Self::Split { first, second, .. } => {
                first.collect_session_ids(out);
                second.collect_session_ids(out);
            }
        }
    }

    /// Returns the `SessionId` hosted by `target_pane`, if present in this subtree.
    pub fn session_for_pane(&self, target_pane: PaneId) -> Option<SessionId> {
        match self {
            Self::Leaf {
                pane_id,
                session_id,
            } => (*pane_id == target_pane).then_some(*session_id),
            Self::Split { first, second, .. } => first
                .session_for_pane(target_pane)
                .or_else(|| second.session_for_pane(target_pane)),
        }
    }

    /// Returns the `PaneId` hosting `target_session`, if present in this subtree.
    pub fn pane_for_session(&self, target_session: SessionId) -> Option<PaneId> {
        match self {
            Self::Leaf {
                pane_id,
                session_id,
            } => (*session_id == target_session).then_some(*pane_id),
            Self::Split { first, second, .. } => first
                .pane_for_session(target_session)
                .or_else(|| second.pane_for_session(target_session)),
        }
    }

    /// Returns the `(SplitAxis, ratio)` of `target_split`, if present in this subtree.
    pub fn find_split(&self, target_split: SplitId) -> Option<(SplitAxis, f32)> {
        match self {
            Self::Leaf { .. } => None,
            Self::Split {
                split_id,
                axis,
                ratio,
                first,
                second,
            } => {
                if *split_id == target_split {
                    Some((*axis, *ratio))
                } else {
                    first
                        .find_split(target_split)
                        .or_else(|| second.find_split(target_split))
                }
            }
        }
    }

    fn first_pane(&self) -> PaneId {
        match self {
            Self::Leaf { pane_id, .. } => *pane_id,
            Self::Split { first, .. } => first.first_pane(),
        }
    }

    fn last_pane(&self) -> PaneId {
        match self {
            Self::Leaf { pane_id, .. } => *pane_id,
            Self::Split { second, .. } => second.last_pane(),
        }
    }

    fn split_leaf(
        &mut self,
        target_pane: PaneId,
        axis: SplitAxis,
        new_split_id: SplitId,
        new_pane_id: PaneId,
        new_session_id: SessionId,
    ) -> bool {
        match self {
            Self::Leaf {
                pane_id,
                session_id,
            } => {
                if *pane_id != target_pane {
                    return false;
                }
                let existing = Self::Leaf {
                    pane_id: *pane_id,
                    session_id: *session_id,
                };
                let added = Self::Leaf {
                    pane_id: new_pane_id,
                    session_id: new_session_id,
                };
                *self = Self::Split {
                    split_id: new_split_id,
                    axis,
                    ratio: DEFAULT_SPLIT_RATIO,
                    first: Box::new(existing),
                    second: Box::new(added),
                };
                true
            }
            Self::Split { first, second, .. } => {
                first.split_leaf(target_pane, axis, new_split_id, new_pane_id, new_session_id)
                    || second.split_leaf(
                        target_pane,
                        axis,
                        new_split_id,
                        new_pane_id,
                        new_session_id,
                    )
            }
        }
    }

    fn resize_split(&mut self, target_split: SplitId, new_ratio: f32) -> bool {
        if !new_ratio.is_finite() {
            return false;
        }
        match self {
            Self::Leaf { .. } => false,
            Self::Split {
                split_id,
                ratio,
                first,
                second,
                ..
            } => {
                if *split_id == target_split {
                    *ratio = new_ratio.clamp(MIN_SPLIT_RATIO, MAX_SPLIT_RATIO);
                    true
                } else {
                    first.resize_split(target_split, new_ratio)
                        || second.resize_split(target_split, new_ratio)
                }
            }
        }
    }

    /// Removes `target_pane` from a `Split` tree and collapses its parent split into the sibling.
    /// Returns `None` if `self` is already a single `Leaf` or `target_pane` was not found.
    fn remove_pane(&mut self, target_pane: PaneId) -> Option<(SessionId, PaneId)> {
        let Self::Split { first, second, .. } = self else {
            return None;
        };

        if let Self::Leaf {
            pane_id,
            session_id,
        } = **first
            && pane_id == target_pane
        {
            let placeholder = Self::Leaf {
                pane_id,
                session_id,
            };
            let sibling = std::mem::replace(second.as_mut(), placeholder);
            let next_focus = sibling.first_pane();
            *self = sibling;
            return Some((session_id, next_focus));
        }

        if let Self::Leaf {
            pane_id,
            session_id,
        } = **second
            && pane_id == target_pane
        {
            let placeholder = Self::Leaf {
                pane_id,
                session_id,
            };
            let sibling = std::mem::replace(first.as_mut(), placeholder);
            let next_focus = sibling.last_pane();
            *self = sibling;
            return Some((session_id, next_focus));
        }

        first
            .remove_pane(target_pane)
            .or_else(|| second.remove_pane(target_pane))
    }

    fn collect_rects(&self, rect: NormRect, out: &mut Vec<(PaneId, NormRect)>) {
        match self {
            Self::Leaf { pane_id, .. } => out.push((*pane_id, rect)),
            Self::Split {
                axis,
                ratio,
                first,
                second,
                ..
            } => match axis {
                SplitAxis::Vertical => {
                    let first_w = rect.w * *ratio;
                    first.collect_rects(
                        NormRect {
                            w: first_w,
                            ..rect
                        },
                        out,
                    );
                    second.collect_rects(
                        NormRect {
                            x: rect.x + first_w,
                            w: rect.w - first_w,
                            ..rect
                        },
                        out,
                    );
                }
                SplitAxis::Horizontal => {
                    let first_h = rect.h * *ratio;
                    first.collect_rects(
                        NormRect {
                            h: first_h,
                            ..rect
                        },
                        out,
                    );
                    second.collect_rects(
                        NormRect {
                            y: rect.y + first_h,
                            h: rect.h - first_h,
                            ..rect
                        },
                        out,
                    );
                }
            },
        }
    }

    /// Finds the nearest neighboring pane in `direction` using normalized split-tree geometry.
    pub fn neighbor_pane(&self, current: PaneId, direction: FocusDirection) -> Option<PaneId> {
        let mut rects = Vec::new();
        self.collect_rects(
            NormRect {
                x: 0.0,
                y: 0.0,
                w: 1.0,
                h: 1.0,
            },
            &mut rects,
        );
        let cur = rects.iter().find(|(id, _)| *id == current)?.1;
        let (cx, cy) = cur.center();
        const EPS: f32 = 1e-4;

        let mut best: Option<(PaneId, f32, f32)> = None;
        for &(pid, cand) in &rects {
            if pid == current {
                continue;
            }
            let (primary_dist, ortho_overlap) = match direction {
                FocusDirection::Right => {
                    if cand.x + EPS < cur.x + cur.w {
                        continue;
                    }
                    let dist = (cand.x - (cur.x + cur.w)).max(0.0);
                    let overlap = (cur.y + cur.h).min(cand.y + cand.h) - cur.y.max(cand.y);
                    (dist, overlap)
                }
                FocusDirection::Left => {
                    if cand.x + cand.w > cur.x + EPS {
                        continue;
                    }
                    let dist = (cur.x - (cand.x + cand.w)).max(0.0);
                    let overlap = (cur.y + cur.h).min(cand.y + cand.h) - cur.y.max(cand.y);
                    (dist, overlap)
                }
                FocusDirection::Down => {
                    if cand.y + EPS < cur.y + cur.h {
                        continue;
                    }
                    let dist = (cand.y - (cur.y + cur.h)).max(0.0);
                    let overlap = (cur.x + cur.w).min(cand.x + cand.w) - cur.x.max(cand.x);
                    (dist, overlap)
                }
                FocusDirection::Up => {
                    if cand.y + cand.h > cur.y + EPS {
                        continue;
                    }
                    let dist = (cur.y - (cand.y + cand.h)).max(0.0);
                    let overlap = (cur.x + cur.w).min(cand.x + cand.w) - cur.x.max(cand.x);
                    (dist, overlap)
                }
            };

            if ortho_overlap <= EPS {
                continue;
            }
            let (tx, ty) = cand.center();
            let center_dist_sq = (tx - cx) * (tx - cx) + (ty - cy) * (ty - cy);

            let is_better = match best {
                None => true,
                Some((_, best_primary, best_center)) => {
                    primary_dist + EPS < best_primary
                        || ((primary_dist - best_primary).abs() <= EPS
                            && center_dist_sq < best_center)
                }
            };
            if is_better {
                best = Some((pid, primary_dist, center_dist_sq));
            }
        }

        best.map(|(pid, _, _)| pid)
    }

    /// Returns the next or previous pane in DFS traversal order, wrapping around.
    pub fn cycle_pane(&self, current: PaneId, direction: CycleDirection) -> Option<PaneId> {
        let panes = self.pane_ids();
        if panes.is_empty() {
            return None;
        }
        let idx = panes.iter().position(|p| *p == current).unwrap_or(0);
        let next_idx = match direction {
            CycleDirection::Next => (idx + 1) % panes.len(),
            CycleDirection::Previous => (idx + panes.len() - 1) % panes.len(),
        };
        Some(panes[next_idx])
    }
}

#[derive(Clone, Copy)]
struct NormRect {
    x: f32,
    y: f32,
    w: f32,
    h: f32,
}

impl NormRect {
    fn center(&self) -> (f32, f32) {
        (self.x + self.w * 0.5, self.y + self.h * 0.5)
    }
}

/// State of a single workspace tab holding a binary split tree of `Session` panes.
#[derive(Debug, Clone)]
pub struct WorkspaceTab {
    pub id: TabId,
    pub title: Option<String>,
    pub color: Option<String>,
    pub pinned: bool,
    pub bell_active: bool,
    pub progress: termewnal_pty::TerminalProgressState,
    pub root: SplitNode,
    pub focused_pane: PaneId,
    pub zoomed_pane: Option<PaneId>,
    pub broadcast_input: bool,
}

impl WorkspaceTab {
    /// Returns the `SessionId` of the currently focused pane in this tab.
    pub fn focused_session_id(&self) -> Option<SessionId> {
        self.root.session_for_pane(self.focused_pane)
    }
}

/// Shared atomic ID allocator ensuring `TabId`, `PaneId`, and `SplitId` remain unique across windows.
#[derive(Debug, Clone)]
pub struct WorkspaceIds {
    next_tab_id: Arc<AtomicU64>,
    next_pane_id: Arc<AtomicU64>,
    next_split_id: Arc<AtomicU64>,
}

impl Default for WorkspaceIds {
    fn default() -> Self {
        Self::new()
    }
}

impl WorkspaceIds {
    /// Creates a fresh ID allocator starting at 1 for each ID kind.
    pub fn new() -> Self {
        Self {
            next_tab_id: Arc::new(AtomicU64::new(1)),
            next_pane_id: Arc::new(AtomicU64::new(1)),
            next_split_id: Arc::new(AtomicU64::new(1)),
        }
    }

    /// Allocates the next globally unique [`TabId`].
    pub fn alloc_tab_id(&self) -> TabId {
        TabId(self.next_tab_id.fetch_add(1, Ordering::Relaxed))
    }

    /// Allocates the next globally unique [`PaneId`].
    pub fn alloc_pane_id(&self) -> PaneId {
        PaneId(self.next_pane_id.fetch_add(1, Ordering::Relaxed))
    }

    /// Allocates the next globally unique [`SplitId`].
    pub fn alloc_split_id(&self) -> SplitId {
        SplitId(self.next_split_id.fetch_add(1, Ordering::Relaxed))
    }
}

/// Multi-tab workspace layout manager tracking tab order, active selection, and IDs.
#[derive(Debug, Clone)]
pub struct Workspace {
    tabs: Vec<WorkspaceTab>,
    active_tab: Option<TabId>,
    ids: WorkspaceIds,
}

impl Default for Workspace {
    fn default() -> Self {
        Self::new()
    }
}

impl Workspace {
    /// Creates an empty [`Workspace`] with no tabs and an isolated ID allocator.
    pub fn new() -> Self {
        Self::with_ids(WorkspaceIds::new())
    }

    /// Creates an empty [`Workspace`] sharing `ids` across multiple windows.
    pub fn with_ids(ids: WorkspaceIds) -> Self {
        Self {
            tabs: Vec::new(),
            active_tab: None,
            ids,
        }
    }

    /// Returns the shared [`WorkspaceIds`] allocator backing this workspace.
    pub fn ids(&self) -> &WorkspaceIds {
        &self.ids
    }

    /// Returns the number of open workspace tabs.
    pub fn tab_count(&self) -> usize {
        self.tabs.len()
    }

    /// Returns all workspace tabs in display order.
    pub fn tabs(&self) -> &[WorkspaceTab] {
        &self.tabs
    }

    /// Returns tab IDs in display order.
    pub fn tab_ids(&self) -> Vec<TabId> {
        self.tabs.iter().map(|t| t.id).collect()
    }

    /// Returns all `SessionId`s hosted across every tab and pane in this workspace.
    pub fn session_ids(&self) -> Vec<SessionId> {
        self.tabs
            .iter()
            .flat_map(|t| t.root.session_ids())
            .collect()
    }

    /// Returns `true` if `tab_id` belongs to this workspace.
    pub fn contains_tab(&self, tab_id: TabId) -> bool {
        self.tabs.iter().any(|t| t.id == tab_id)
    }

    /// Returns `true` if `pane_id` exists in any tab of this workspace.
    pub fn contains_pane(&self, pane_id: PaneId) -> bool {
        self.tabs
            .iter()
            .any(|t| t.root.session_for_pane(pane_id).is_some())
    }

    /// Returns `true` if `split_id` exists in any tab of this workspace.
    pub fn contains_split(&self, split_id: SplitId) -> bool {
        self.tabs
            .iter()
            .any(|t| t.root.find_split(split_id).is_some())
    }

    /// Returns `true` if `session_id` exists in any tab of this workspace.
    pub fn contains_session(&self, session_id: SessionId) -> bool {
        self.tabs
            .iter()
            .any(|t| t.root.pane_for_session(session_id).is_some())
    }

    /// Returns the currently active `TabId`, if any tab is open.
    pub fn active_tab_id(&self) -> Option<TabId> {
        self.active_tab
    }

    /// Returns a reference to the currently active [`WorkspaceTab`].
    pub fn active_tab(&self) -> Option<&WorkspaceTab> {
        let active = self.active_tab?;
        self.tabs.iter().find(|t| t.id == active)
    }

    /// Returns a reference to a tab by `TabId`.
    pub fn tab(&self, tab_id: TabId) -> Option<&WorkspaceTab> {
        self.tabs.iter().find(|t| t.id == tab_id)
    }

    fn tab_mut(&mut self, tab_id: TabId) -> Option<&mut WorkspaceTab> {
        self.tabs.iter_mut().find(|t| t.id == tab_id)
    }

    /// Creates a new tab hosting `session_id` and activates it, returning `(TabId, PaneId)`.
    pub fn create_tab(&mut self, session_id: SessionId, title: Option<String>) -> (TabId, PaneId) {
        let tab_id = self.ids.alloc_tab_id();
        let pane_id = self.ids.alloc_pane_id();

        self.tabs.push(WorkspaceTab {
            id: tab_id,
            title,
            color: None,
            pinned: false,
            bell_active: false,
            progress: termewnal_pty::TerminalProgressState::Hidden,
            root: SplitNode::Leaf {
                pane_id,
                session_id,
            },
            focused_pane: pane_id,
            zoomed_pane: None,
            broadcast_input: false,
        });
        self.active_tab = Some(tab_id);
        (tab_id, pane_id)
    }

    /// Inserts a pre-built split tree as a restored tab and returns its allocated [`TabId`].
    pub fn restore_tab(
        &mut self,
        title: Option<String>,
        root: SplitNode,
        focused_pane: PaneId,
        zoomed_pane: Option<PaneId>,
    ) -> TabId {
        let tab_id = self.ids.alloc_tab_id();
        self.tabs.push(WorkspaceTab {
            id: tab_id,
            title,
            color: None,
            pinned: false,
            bell_active: false,
            progress: termewnal_pty::TerminalProgressState::Hidden,
            root,
            focused_pane,
            zoomed_pane,
            broadcast_input: false,
        });
        if self.active_tab.is_none() {
            self.active_tab = Some(tab_id);
        }
        tab_id
    }

    /// Sets the active tab by 0-based tab index (clamped when non-empty).
    pub fn set_active_tab_index(&mut self, idx: usize) {
        if let Some(tab) = self.tabs.get(idx).or_else(|| self.tabs.last()) {
            let tid = tab.id;
            self.active_tab = Some(tid);
            self.set_tab_bell(tid, false);
        }
    }

    /// Activates the tab identified by `tab_id`, returning `true` if it exists.
    pub fn select_tab(&mut self, tab_id: TabId) -> bool {
        if let Some(tab) = self.tabs.iter_mut().find(|t| t.id == tab_id) {
            tab.bell_active = false;
            self.active_tab = Some(tab_id);
            true
        } else {
            false
        }
    }

    /// Renames `tab_id` (clearing explicit override when empty).
    pub fn rename_tab(&mut self, tab_id: TabId, title: Option<String>) -> bool {
        let Some(tab) = self.tab_mut(tab_id) else {
            return false;
        };
        tab.title = title.and_then(|s| {
            let trimmed = s.trim();
            (!trimmed.is_empty()).then(|| trimmed.to_string())
        });
        true
    }

    /// Updates custom hex accent color for `tab_id`.
    pub fn set_tab_color(&mut self, tab_id: TabId, color: Option<String>) -> bool {
        let Some(tab) = self.tab_mut(tab_id) else {
            return false;
        };
        tab.color = color.and_then(|s| {
            let trimmed = s.trim();
            (!trimmed.is_empty()).then(|| trimmed.to_string())
        });
        true
    }

    /// Toggles pinned state of `tab_id` and returns the new value.
    pub fn toggle_pin_tab(&mut self, tab_id: TabId) -> Option<bool> {
        let tab = self.tab_mut(tab_id)?;
        tab.pinned = !tab.pinned;
        Some(tab.pinned)
    }

    /// Explicitly sets `color` and `pinned` metadata on `tab_id`.
    pub fn set_tab_metadata(
        &mut self,
        tab_id: TabId,
        color: Option<String>,
        pinned: bool,
    ) -> bool {
        let Some(tab) = self.tab_mut(tab_id) else {
            return false;
        };
        tab.color = color;
        tab.pinned = pinned;
        true
    }

    /// Updates bell activity badge for `tab_id`.
    pub fn set_tab_bell(&mut self, tab_id: TabId, active: bool) -> bool {
        let Some(tab) = self.tab_mut(tab_id) else {
            return false;
        };
        tab.bell_active = active;
        true
    }

    /// Updates `OSC 9;4` progress bar state for `tab_id`.
    pub fn set_tab_progress(
        &mut self,
        tab_id: TabId,
        progress: termewnal_pty::TerminalProgressState,
    ) -> bool {
        let Some(tab) = self.tab_mut(tab_id) else {
            return false;
        };
        tab.progress = progress;
        true
    }

    /// Closes every unpinned tab other than `keep_tab_id`, returning closed `SessionId`s.
    pub fn close_other_tabs(&mut self, keep_tab_id: TabId) -> Vec<SessionId> {
        if !self.contains_tab(keep_tab_id) {
            return Vec::new();
        }
        let mut closed_sessions = Vec::new();
        self.tabs.retain(|tab| {
            if tab.id == keep_tab_id || tab.pinned {
                true
            } else {
                closed_sessions.extend(tab.root.session_ids());
                false
            }
        });
        if !self.active_tab.is_some_and(|id| self.contains_tab(id)) {
            self.active_tab = Some(keep_tab_id);
        }
        closed_sessions
    }

    /// Closes unpinned tabs strictly to the right of `pivot_tab_id`, returning closed `SessionId`s.
    pub fn close_tabs_to_right(&mut self, pivot_tab_id: TabId) -> Vec<SessionId> {
        let Some(pivot_idx) = self.tabs.iter().position(|t| t.id == pivot_tab_id) else {
            return Vec::new();
        };
        let mut closed_sessions = Vec::new();
        let mut idx = 0;
        self.tabs.retain(|tab| {
            let keep = idx <= pivot_idx || tab.pinned;
            idx += 1;
            if !keep {
                closed_sessions.extend(tab.root.session_ids());
            }
            keep
        });
        if !self.active_tab.is_some_and(|id| self.contains_tab(id)) {
            self.active_tab = Some(pivot_tab_id);
        }
        closed_sessions
    }

    /// Cycles active tab selection forward or backward with wrap-around.
    pub fn cycle_tab(&mut self, direction: CycleDirection) -> Option<TabId> {
        if self.tabs.is_empty() {
            return None;
        }
        let current_idx = self
            .active_tab
            .and_then(|id| self.tabs.iter().position(|t| t.id == id))
            .unwrap_or(0);
        let next_idx = match direction {
            CycleDirection::Next => (current_idx + 1) % self.tabs.len(),
            CycleDirection::Previous => (current_idx + self.tabs.len() - 1) % self.tabs.len(),
        };
        let next_id = self.tabs[next_idx].id;
        self.select_tab(next_id);
        Some(next_id)
    }

    /// Moves `tab_id` to `target_index` (clamped to valid bounds), preserving active selection.
    pub fn reorder_tab(&mut self, tab_id: TabId, target_index: usize) -> bool {
        let Some(from_idx) = self.tabs.iter().position(|t| t.id == tab_id) else {
            return false;
        };
        let to_idx = target_index.min(self.tabs.len() - 1);
        if from_idx != to_idx {
            let tab = self.tabs.remove(from_idx);
            self.tabs.insert(to_idx, tab);
        }
        true
    }

    /// Shifts `tab_id` left (`delta < 0`) or right (`delta > 0`) within the tab strip.
    pub fn move_tab_by(&mut self, tab_id: TabId, delta: isize) -> bool {
        let Some(from_idx) = self.tabs.iter().position(|t| t.id == tab_id) else {
            return false;
        };
        if self.tabs.len() <= 1 || delta == 0 {
            return true;
        }
        let max_idx = (self.tabs.len() - 1) as isize;
        let to_idx = (from_idx as isize + delta).clamp(0, max_idx) as usize;
        self.reorder_tab(tab_id, to_idx)
    }

    /// Closes `tab_id`, updating `active_tab` to an adjacent tab and returning closed `SessionId`s.
    pub fn close_tab(&mut self, tab_id: TabId) -> Option<Vec<SessionId>> {
        let idx = self.tabs.iter().position(|t| t.id == tab_id)?;
        let removed = self.tabs.remove(idx);
        let sessions = removed.root.session_ids();

        if self.active_tab == Some(tab_id) {
            self.active_tab = if self.tabs.is_empty() {
                None
            } else {
                let fallback_idx = idx.min(self.tabs.len() - 1);
                Some(self.tabs[fallback_idx].id)
            };
        }

        Some(sessions)
    }

    /// Splits `target_pane` inside `tab_id` along `axis`, hosting `new_session_id` in the new pane.
    /// Focus moves to the newly inserted pane and any active zoom is cleared.
    pub fn split_pane(
        &mut self,
        tab_id: TabId,
        target_pane: PaneId,
        axis: SplitAxis,
        new_session_id: SessionId,
    ) -> Option<(SplitId, PaneId)> {
        let split_id = self.ids.alloc_split_id();
        let pane_id = self.ids.alloc_pane_id();
        let tab = self.tab_mut(tab_id)?;
        if !tab
            .root
            .split_leaf(target_pane, axis, split_id, pane_id, new_session_id)
        {
            return None;
        }
        tab.focused_pane = pane_id;
        tab.zoomed_pane = None;
        Some((split_id, pane_id))
    }

    /// Updates the split ratio for `split_id` inside `tab_id`, clamped to `[MIN_SPLIT_RATIO, MAX_SPLIT_RATIO]`.
    pub fn resize_split(&mut self, tab_id: TabId, split_id: SplitId, ratio: f32) -> bool {
        let Some(tab) = self.tab_mut(tab_id) else {
            return false;
        };
        tab.root.resize_split(split_id, ratio)
    }

    /// Closes `target_pane` inside `tab_id`, collapsing its parent split or closing the tab if it was the last pane.
    pub fn close_pane(&mut self, tab_id: TabId, target_pane: PaneId) -> Option<ClosePaneOutcome> {
        let tab = self.tab_mut(tab_id)?;
        if let SplitNode::Leaf {
            pane_id,
            session_id,
        } = tab.root
        {
            if pane_id != target_pane {
                return None;
            }
            self.close_tab(tab_id);
            return Some(ClosePaneOutcome::ClosedTab { session_id });
        }

        let (session_id, sibling_focus) = tab.root.remove_pane(target_pane)?;
        if tab.focused_pane == target_pane {
            tab.focused_pane = sibling_focus;
        }
        tab.zoomed_pane = None;

        Some(ClosePaneOutcome::ClosedPane {
            session_id,
            focused_pane: tab.focused_pane,
        })
    }

    /// Focuses `pane_id` inside `tab_id` if present.
    pub fn focus_pane(&mut self, tab_id: TabId, pane_id: PaneId) -> bool {
        let Some(tab) = self.tab_mut(tab_id) else {
            return false;
        };
        if tab.root.session_for_pane(pane_id).is_some() {
            tab.focused_pane = pane_id;
            true
        } else {
            false
        }
    }

    /// Moves focus inside `tab_id` to the neighboring pane in `direction`, returning its `PaneId`.
    pub fn move_focus(&mut self, tab_id: TabId, direction: FocusDirection) -> Option<PaneId> {
        let tab = self.tab_mut(tab_id)?;
        let next = tab.root.neighbor_pane(tab.focused_pane, direction)?;
        tab.focused_pane = next;
        Some(next)
    }

    /// Cycles focus across panes inside `tab_id`, returning the newly focused `PaneId`.
    pub fn cycle_focus(&mut self, tab_id: TabId, direction: CycleDirection) -> Option<PaneId> {
        let tab = self.tab_mut(tab_id)?;
        let next = tab.root.cycle_pane(tab.focused_pane, direction)?;
        tab.focused_pane = next;
        Some(next)
    }

    /// Activates the tab and pane hosting `session_id`, returning `(TabId, PaneId)` if found.
    pub fn focus_session(&mut self, session_id: SessionId) -> Option<(TabId, PaneId)> {
        let found = self
            .tabs
            .iter()
            .find_map(|t| t.root.pane_for_session(session_id).map(|p| (t.id, p)))?;
        self.active_tab = Some(found.0);
        if let Some(tab) = self.tab_mut(found.0) {
            tab.focused_pane = found.1;
            tab.bell_active = false;
        }
        Some(found)
    }

    /// Toggles zoom on `tab_id`'s focused pane while preserving the underlying split tree.
    pub fn toggle_zoom(&mut self, tab_id: TabId) -> Option<Option<PaneId>> {
        let tab = self.tab_mut(tab_id)?;
        tab.zoomed_pane = match tab.zoomed_pane {
            Some(_) => None,
            None => Some(tab.focused_pane),
        };
        Some(tab.zoomed_pane)
    }

    /// Toggles Broadcast Input on `tab_id`, returning the new state.
    pub fn toggle_broadcast_input(&mut self, tab_id: TabId) -> Option<bool> {
        let tab = self.tab_mut(tab_id)?;
        tab.broadcast_input = !tab.broadcast_input;
        Some(tab.broadcast_input)
    }

    /// Explicitly sets Broadcast Input mode on `tab_id`.
    pub fn set_broadcast_input(&mut self, tab_id: TabId, enabled: bool) -> bool {
        let Some(tab) = self.tab_mut(tab_id) else {
            return false;
        };
        tab.broadcast_input = enabled;
        true
    }

    /// Returns all target `SessionId`s that should receive input arriving at `session_id`.
    ///
    /// When the owning tab has Broadcast Input active, every pane's `SessionId` in that tab is
    /// returned; otherwise only `[session_id]` is returned.
    pub fn input_targets_for_session(&self, session_id: SessionId) -> Vec<SessionId> {
        if let Some(tab) = self
            .tabs
            .iter()
            .find(|t| t.root.pane_for_session(session_id).is_some())
            && tab.broadcast_input
        {
            return tab.root.session_ids();
        }
        vec![session_id]
    }
}
