//! Client-side compositor for mixed-server sessions.
//!
//! The compositor owns the client-rendered unified sidebar: it projects the
//! supervisor model into a display-only [`crate::app::AppState`], renders the
//! sidebar and the client overlays through the SHARED `ui` renderers into an
//! offscreen ratatui buffer, and composites the active server's
//! embedded-content frame next to it. It also owns sidebar mouse hit-testing
//! so render geometry and input geometry cannot drift.

use std::collections::HashMap;
use std::time::Instant;

use ratatui::{
    layout::{Alignment, Rect},
    style::{Modifier, Style},
    text::Span,
    widgets::Paragraph,
};
use tracing::warn;
use unicode_width::UnicodeWidthStr;

use crate::app::state::{MenuListState, ViewLayout};
use crate::app::Mode;
use crate::client::supervisor::{AgentSidebarRow, ServerId};
use crate::detect::AgentState;
use crate::protocol::{CursorState, FrameData};
use crate::terminal::{TerminalId, TerminalRuntimeRegistry, TerminalState};

pub(crate) const DEFAULT_SIDEBAR_WIDTH: u16 = 26;

/// #26: double-click window for the width-divider reset, matching the monolithic host's
/// `DOUBLE_CLICK_WINDOW` (`src/app/input/mouse.rs`).
const DIVIDER_DOUBLE_CLICK_WINDOW: std::time::Duration = std::time::Duration::from_millis(400);

/// #23: the two fixed workspace context-menu rows, in render order. Kept here (ui-side strings) so
/// the renderer and the geometry/hit-test that derive their row count from `.len()` cannot drift.
/// Mirrors `ClientSupervisorModel::workspace_context_menu_items` (the supervisor side that maps a
/// row index to a `Rename`/`Close` action); the two MUST stay in lockstep.
const WORKSPACE_CONTEXT_MENU_ITEMS: [&str; 2] = ["rename", "close"];

/// #19: minimum pointer travel (in cells) before a workspace press becomes a drag-reorder, mirroring
/// the monolithic host's `WORKSPACE_DRAG_THRESHOLD` (`src/app/input/mod.rs`).
const WORKSPACE_DRAG_THRESHOLD: u16 = 1;

/// #21: which sidebar list a scrollbar drag is acting on.
#[derive(Clone, Copy, PartialEq, Eq)]
enum ScrollbarPanel {
    Workspace,
    Agent,
}

/// #21: an in-progress scrollbar thumb drag (client-local). `grab_row_offset` keeps the grab point
/// fixed under the pointer, matching the monolithic host's scrollbar drag.
#[derive(Clone, Copy)]
struct ScrollbarDrag {
    panel: ScrollbarPanel,
    grab_row_offset: u16,
}

/// #19: an in-progress press on a workspace card. A press that moves past `WORKSPACE_DRAG_THRESHOLD`
/// becomes a drag-reorder; on release it commits a `workspace.reorder` to the owning server.
#[derive(Clone)]
struct WorkspacePress {
    server_id: ServerId,
    workspace_id: String,
    origin_col: u16,
    origin_row: u16,
    dragging: bool,
    /// #19: latest pointer row while dragging, so `from_model` can render a live drop indicator
    /// each frame (the drop position the release would commit). `None` until the press becomes a drag.
    last_drag_row: Option<u16>,
}

/// #19: outcome of feeding a mouse event to the workspace drag-reorder tracker.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum WorkspaceReorderOutcome {
    /// Not part of a drag-reorder (caller continues its normal dispatch).
    Ignored,
    /// An active drag moved — the caller should redraw (drop indicator follows the pointer).
    Dragging,
    /// A drag was released over a valid slot — send `workspace.reorder` to `server_id`.
    Commit {
        server_id: ServerId,
        workspace_id: String,
        insert_index: usize,
    },
    /// A drag was released but resolved to no valid slot — swallow it (no request).
    Cancelled,
}

/// #19 (host half): an in-progress press on a host banner. Mirrors `WorkspacePress`: a press that
/// moves past `WORKSPACE_DRAG_THRESHOLD` becomes a host drag-reorder; on release it commits a
/// client-local host move (host order is client-owned, no server round-trip).
#[derive(Clone)]
struct HostPress {
    server_id: ServerId,
    origin_col: u16,
    origin_row: u16,
    dragging: bool,
    /// Latest pointer row while dragging, so `from_model` can render a live host drop indicator
    /// each frame. `None` until the press becomes a drag.
    last_drag_row: Option<u16>,
}

/// #19 (host half): outcome of feeding a mouse event to the host drag-reorder tracker. Mirrors
/// `WorkspaceReorderOutcome`, but `Commit` carries an `insert_index` into the ORDERED host list
/// and is applied client-locally (`model.reorder_server`), never sent to a server.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum HostReorderOutcome {
    /// Not part of a host drag-reorder (caller continues its normal dispatch).
    Ignored,
    /// An active host drag moved — the caller should redraw (drop indicator follows the pointer).
    Dragging,
    /// A host drag was released over a valid slot — reorder `source_server_id` to `insert_index`.
    Commit {
        source_server_id: ServerId,
        insert_index: usize,
    },
    /// A host drag was released but resolved to no valid slot — swallow it (no reorder).
    Cancelled,
}

/// #19: resolve a drop row to a `0..=cards.len()` insert position within one server's contiguous
/// workspace cards (render order == stored order). The index is the count of cards whose vertical
/// midpoint sits strictly above the drop row — the same rule used for both the live drop indicator
/// (`from_model`) and the committed `workspace.reorder` (`workspace_reorder_target`).
fn server_insert_index(cards: &[Rect], drop_row: u16) -> usize {
    let mut insert = 0usize;
    for rect in cards {
        let midpoint = rect.y.saturating_add(rect.height / 2);
        if drop_row > midpoint {
            insert += 1;
        } else {
            break;
        }
    }
    insert.min(cards.len())
}

/// Outcome of a width-divider mouse interaction. A change to the sidebar width changes the content
/// area, so it must resize the remote PTY (`Resized`); merely beginning/ending a drag only needs a
/// local redraw (`Redraw`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SidebarResizeOutcome {
    Redraw,
    Resized(u16, u16),
}

pub(crate) struct ClientCompositor {
    sidebar_width: u16,
    workspace_scroll: usize,
    agent_panel_scroll: usize,
    resizing_sidebar: bool,
    // #16: client-local spaces↔agents split override. `None` follows the server/settings default;
    // `Some(ratio)` is a user drag. Fed into `from_model` and tracked while `resizing_section`.
    section_split: Option<f32>,
    resizing_section: bool,
    // #26: instant of the last left-press on the width divider, for double-click reset detection.
    last_divider_down: Option<Instant>,
    // #19: in-progress press on a workspace card, promoted to a drag-reorder past the threshold.
    workspace_press: Option<WorkspacePress>,
    // #19 (host half): in-progress press on a host banner, promoted to a host drag-reorder past
    // the threshold. Only one of `workspace_press`/`host_press` is ever dragging at a time.
    host_press: Option<HostPress>,
    // #21: in-progress scrollbar thumb drag for the workspace or agent list.
    scrollbar_drag: Option<ScrollbarDrag>,
    animation_tick: u32,                                  // item 5
    hover: Option<crate::app::state::SidebarHoverTarget>, // item 7
    // #20: client-local agents-panel sort (spaces vs priority). The server never owns this; it is
    // a per-client view preference fed into the render snapshot (`from_model`). The flat
    // `agent_routes` are derived from the SAME `agent_panel_entries` order the renderer walks, so
    // agent-row hit-testing stays aligned with the rendered entries under either sort.
    agent_panel_sort: crate::app::state::AgentPanelSort,
    // #25: client-local collapsed-sidebar view state. The server never owns this; it is a per-client
    // view preference fed into `from_model` (sets `app.sidebar_collapsed`) so the SHARED renderer
    // branches to the narrow collapsed layout and `hit_test` reads the collapsed geometry.
    sidebar_collapsed: bool,
    // #24: client-local prefix-mode flag. Mirrors the server's `Mode::Prefix` state machine so the
    // configured prefix key (a modified chord, e.g. `ctrl+b`) arms interception of the very next
    // key for prefix-bound sidebar-nav actions. Bare keys are never intercepted unless armed, so
    // normal terminal input is preserved.
    /// #24: the raw bytes of the pressed prefix key while prefix mode is armed. `Some` means
    /// armed; the bytes are replayed to the active server when a follow-up key matches no
    /// client-side binding, so server-side prefix chords keep working.
    pending_prefix_bytes: Option<Vec<u8>>,
    // #22: client-local collapsed worktree-group keys. The server persists its OWN set
    // (`AppState.collapsed_space_keys`); collapse of the client's AGGREGATED multi-host view is a
    // per-client display concern, so the client owns this set (no server round-trip). Fed into
    // `from_model` (sets `app.collapsed_space_keys`) so the SHARED grouping renderer collapses the
    // right groups.
    collapsed_space_keys: std::collections::HashSet<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum SidebarHitTarget {
    Filter,
    Workspace {
        server_id: crate::client::supervisor::ServerId,
        workspace_id: String,
    },
    Agent {
        server_id: crate::client::supervisor::ServerId,
        agent_id: String,
    },
    NewWorkspaceDestination {
        server_id: crate::client::supervisor::ServerId,
    },
    /// #19 (host half): a host banner row. A `Down(Left)` here arms a host drag-reorder (and
    /// otherwise focuses the host's first workspace via the existing dispatch). Client-local.
    HostBanner {
        server_id: crate::client::supervisor::ServerId,
    },
    ClientGlobalMenuItem {
        index: usize,
    },
    New,
    Menu,
    /// #20: the agents-panel sort toggle (`spaces`/`priority`). Client-local view state (no server
    /// round-trip), handled in `dispatch_composited_mouse_input` since it needs `&mut compositor`.
    /// Upstream renamed the PoC's all/current scope toggle to a sort toggle; the hover target is
    /// the shared `SidebarHoverTarget::ScopeToggle`.
    AgentPanelSortToggle,
    /// #25: the collapse/expand sidebar toggle. Drawn in BOTH modes by `render_sidebar_toggle`
    /// (bottom-right in expanded mode via `expanded_sidebar_toggle_rect`, bottom-center in
    /// collapsed mode via `collapsed_sidebar_toggle_rect`), so it is hittable to collapse
    /// (expanded) AND to expand (collapsed). Client-local view state, flipped via
    /// `compositor.toggle_sidebar_collapsed()`.
    CollapsedSidebarToggle,
    /// #22: the chevron column of a worktree-group PARENT row. A `Down(Left)` here toggles the
    /// group's collapsed state in the client-local set (no server round-trip), handled in
    /// `dispatch_composited_mouse_input` since it needs `&mut compositor`. A NON-chevron click on the
    /// parent row stays `Workspace` (focus), matching the server's `mouse.rs` chevron geometry.
    WorktreeChevron {
        group_key: String,
    },
    // item 1: composited-modal action buttons (footer-anchored ratatui popups).
    AddRemoteSubmit,
    AddRemoteCancel,
    NewWorkspacePickerConfirm,
    NewWorkspacePickerCancel,
    // item 3 (Area 5): remote-management overlay targets.
    RemoteManageRow {
        index: usize,
    },
    RemoteManageAdd,
    RemoteManageConfirmDelete,
    RemoteManageCancelDelete,
    // #23: workspace context-menu + rename + confirm-close overlay targets.
    WorkspaceContextMenuRow {
        index: usize,
    },
    RenameWorkspaceSubmit,
    RenameWorkspaceCancel,
    ConfirmCloseWorkspaceConfirm,
    ConfirmCloseWorkspaceCancel,
}

#[derive(Clone)]
struct WorkspaceRoute {
    server_id: ServerId,
    workspace_id: Option<String>,
    disabled: bool,
}

#[derive(Clone)]
struct AgentRoute {
    server_id: ServerId,
    agent_id: String,
}

struct ClientSidebarSnapshot {
    app: crate::app::AppState,
    filter_label: String,
    workspace_routes: Vec<WorkspaceRoute>,
    // #19 (host half): `banner_idx -> server_id`, in the SAME order banners are emitted
    // (`host_banner_server_ids`), so `host_banner_areas[i]` resolves to `host_banner_server_ids[i]`
    // deterministically (render == hit geometry).
    host_banner_server_ids: Vec<ServerId>,
    // Flat agent routes, index-aligned with `crate::ui::agent_panel_entries(&app)` (the SAME
    // entries the renderer walks, including the client-local sort), so an agent-row hit resolves
    // to the right `(server_id, agent_id)` under either sort order. The collapsed agent-detail
    // rows are the same entries (one row each, unscrolled), so they resolve through this too.
    agent_routes: Vec<AgentRoute>,
    // overlay carriers (items 1 & 3), all ui-owned/cloned — see Area 3:
    add_remote_form: Option<crate::client::supervisor::AddRemoteForm>, // item 1
    new_workspace_picker: Option<(Vec<crate::client::supervisor::ServerDestination>, usize)>, // item 1
    // item 3: the overlay state plus the snapshot of secondary rows it renders. The closure maps
    // `RemoteManageRow` -> ui-owned `RemoteManageRowView` before calling `render_*` (layering).
    remote_manage: Option<(
        crate::client::supervisor::RemoteManageOverlay,
        Vec<crate::client::supervisor::RemoteManageRow>,
    )>,
    // #23: the workspace context menu / rename / confirm-close overlay state, cloned out of the
    // model. The render closure maps these into ui-owned views before drawing (layering rule).
    workspace_context_menu: Option<crate::client::supervisor::WorkspaceContextMenu>,
    rename_workspace: Option<crate::client::supervisor::RenameWorkspaceForm>,
    confirm_close_workspace: Option<crate::client::supervisor::ConfirmCloseWorkspace>,
}

impl ClientCompositor {
    pub(crate) fn new(sidebar_width: u16) -> Self {
        Self {
            sidebar_width,
            workspace_scroll: 0,
            agent_panel_scroll: 0,
            resizing_sidebar: false,
            section_split: None,
            resizing_section: false,
            last_divider_down: None,
            workspace_press: None,
            host_press: None,
            scrollbar_drag: None,
            animation_tick: 0,
            hover: None,
            agent_panel_sort: crate::app::state::AgentPanelSort::default(),
            sidebar_collapsed: false,
            pending_prefix_bytes: None,
            collapsed_space_keys: std::collections::HashSet::new(),
        }
    }

    pub(crate) fn sidebar_width(&self) -> u16 {
        self.sidebar_width
    }

    /// #24: whether the prefix key has been pressed and the next key should be matched against
    /// prefix-mode bindings.
    pub(crate) fn prefix_armed(&self) -> bool {
        self.pending_prefix_bytes.is_some()
    }

    /// #24: arm prefix mode (the configured prefix key was pressed), stashing the prefix
    /// keypress's raw bytes so an unmatched follow-up key can replay `prefix + key` to the
    /// active server — server-side prefix bindings (splits, tabs, zoom, copy mode, …) have no
    /// client-rendered equivalent and must reach the server's own prefix state machine.
    pub(crate) fn arm_prefix(&mut self, prefix_bytes: Vec<u8>) {
        self.pending_prefix_bytes = Some(prefix_bytes);
    }

    /// #24: clear prefix mode, returning the stashed prefix bytes so the caller can replay
    /// them ahead of the current input when no client-side binding matched.
    pub(crate) fn take_prefix_bytes(&mut self) -> Option<Vec<u8>> {
        self.pending_prefix_bytes.take()
    }

    /// #24: clear prefix mode and drop the stashed bytes (a client-side binding consumed the
    /// chord, or the prefix was cancelled).
    pub(crate) fn disarm_prefix(&mut self) {
        self.pending_prefix_bytes = None;
    }

    /// #24: test accessor for the client-local collapsed-sidebar flag (the value `from_model`
    /// feeds into `app.sidebar_collapsed`). Lets the #24 key-nav tests assert the collapse toggle
    /// without reaching into the private field.
    #[cfg(test)]
    pub(crate) fn sidebar_collapsed_for_test(&self) -> bool {
        self.sidebar_collapsed
    }

    #[cfg(test)]
    pub(crate) fn agent_panel_sort(&self) -> crate::app::state::AgentPanelSort {
        self.agent_panel_sort
    }

    /// #20: flip the agents-panel sort and reset the panel scroll, mirroring the monolithic
    /// host's toggle handler (`src/app/input/mouse.rs`). Client-local; no server traffic.
    pub(crate) fn toggle_agent_panel_sort(&mut self) {
        use crate::app::state::AgentPanelSort;
        self.agent_panel_sort = match self.agent_panel_sort {
            AgentPanelSort::Spaces => AgentPanelSort::Priority,
            AgentPanelSort::Priority => AgentPanelSort::Spaces,
        };
        self.agent_panel_scroll = 0;
    }

    /// #25: flip the client-local collapsed-sidebar view state. Mirrors the monolithic host's
    /// collapse toggle; `from_model` feeds the flag into `app.sidebar_collapsed`, which gates the
    /// SHARED renderer onto its narrow collapsed layout. Client-local; no server traffic.
    pub(crate) fn toggle_sidebar_collapsed(&mut self) {
        self.sidebar_collapsed = !self.sidebar_collapsed;
    }

    /// #22: toggle a worktree group's collapsed state in the client-local set (no server round-trip,
    /// since collapse of the aggregated multi-host view is a per-client display concern). Mirrors the
    /// server's `mouse.rs` chevron toggle (remove if present, else insert). Fed into `from_model`.
    pub(crate) fn toggle_collapsed_space_key(&mut self, key: String) {
        if !self.collapsed_space_keys.remove(&key) {
            self.collapsed_space_keys.insert(key);
        }
    }

    /// #22: test accessor for the client-local collapsed worktree-group set, so the chevron-toggle
    /// tests can assert the set's membership without reaching into the private field.
    #[cfg(test)]
    pub(crate) fn collapsed_space_keys_for_test(&self) -> &std::collections::HashSet<String> {
        &self.collapsed_space_keys
    }

    /// Advance the single client-owned animation clock by `step`. Called ONLY from the
    /// `run_client_loop` `Timer` arm (never during render). `from_model` reads it into
    /// `AppState.spinner_tick`; items 2/7 consume the SAME tick (no second clock).
    pub(crate) fn advance_animation_tick(&mut self, step: u32) {
        self.animation_tick = self.animation_tick.wrapping_add(step);
    }

    pub(crate) fn animation_tick(&self) -> u32 {
        self.animation_tick
    }

    /// item 7: update the client-truth sidebar hover target, returning whether it changed. The
    /// caller redraws only on a change so a same-row motion sweep coalesces to zero redraws.
    pub(crate) fn set_hover(
        &mut self,
        next: Option<crate::app::state::SidebarHoverTarget>,
    ) -> bool {
        let changed = self.hover != next;
        self.hover = next;
        changed
    }

    /// item 7: the current client-truth hover target. Read by the `Moved` dispatch so motion off
    /// the sidebar still clears a stale highlight, and by render mirroring in `from_model`.
    pub(crate) fn hover(&self) -> Option<crate::app::state::SidebarHoverTarget> {
        self.hover
    }

    pub(crate) fn handle_sidebar_resize_mouse(
        &mut self,
        mouse: &crossterm::event::MouseEvent,
        host_width: u16,
        host_height: u16,
        settings: &crate::api::schema::UiSettingsInfo,
    ) -> Option<SidebarResizeOutcome> {
        use crossterm::event::{MouseButton, MouseEventKind};

        let sidebar_width = self.effective_sidebar_width(host_width);
        let divider_col = sidebar_width.checked_sub(1)?;
        match mouse.kind {
            MouseEventKind::Down(MouseButton::Left) if mouse.column == divider_col => {
                // #26: a second press on the divider within the double-click window resets the
                // width to the configured default (mirrors the host's divider double-click), rather
                // than starting another drag. Clear the timestamp so a third click can't chain.
                let now = Instant::now();
                let double_click = self
                    .last_divider_down
                    .is_some_and(|prev| now.duration_since(prev) <= DIVIDER_DOUBLE_CLICK_WINDOW);
                if double_click {
                    self.resizing_sidebar = false;
                    self.last_divider_down = None;
                    self.sidebar_width = settings
                        .sidebar_default_width
                        .clamp(settings.sidebar_min_width, settings.sidebar_max_width);
                    // The reset changes the content width, so the remote PTY must resize.
                    let (cols, rows) = self.content_size(host_width, host_height);
                    Some(SidebarResizeOutcome::Resized(cols, rows))
                } else {
                    self.resizing_sidebar = true;
                    self.last_divider_down = Some(now);
                    // Starting a drag does not change the width yet — only redraw.
                    Some(SidebarResizeOutcome::Redraw)
                }
            }
            MouseEventKind::Drag(MouseButton::Left) if self.resizing_sidebar => {
                // #26: a drag is a resize gesture, not part of a double-click — clear the press
                // timestamp so the next press starts a fresh double-click window (a press → drag →
                // press sequence must NOT reset to default).
                self.last_divider_down = None;
                self.set_sidebar_width_from_column(
                    mouse.column,
                    host_width,
                    settings.sidebar_min_width,
                    settings.sidebar_max_width,
                );
                let (cols, rows) = self.content_size(host_width, host_height);
                Some(SidebarResizeOutcome::Resized(cols, rows))
            }
            MouseEventKind::Up(MouseButton::Left) if self.resizing_sidebar => {
                self.resizing_sidebar = false;
                Some(SidebarResizeOutcome::Redraw)
            }
            _ => None,
        }
    }

    pub(crate) fn handle_sidebar_scroll_mouse(
        &mut self,
        model: &crate::client::supervisor::ClientSupervisorModel,
        mouse: &crossterm::event::MouseEvent,
        host_width: u16,
        host_height: u16,
    ) -> Option<bool> {
        use crossterm::event::MouseEventKind;

        let delta = match mouse.kind {
            MouseEventKind::ScrollUp => -1,
            MouseEventKind::ScrollDown => 1,
            _ => return None,
        };
        let sidebar_width = self.effective_sidebar_width(host_width);
        if sidebar_width == 0
            || host_height == 0
            || mouse.column >= sidebar_width
            || mouse.row >= host_height
        {
            return None;
        }

        let snapshot = ClientSidebarSnapshot::from_model(
            model,
            self,
            sidebar_width,
            host_width,
            host_height,
            Instant::now(),
        );
        let (_, detail_area) = crate::ui::expanded_sidebar_sections(
            snapshot.app.view.sidebar_rect,
            snapshot.app.sidebar_section_split,
        );
        let over_agent_panel = detail_area != Rect::default()
            && mouse.row >= detail_area.y
            && mouse.row < detail_area.y.saturating_add(detail_area.height);

        if over_agent_panel {
            let metrics = crate::ui::agent_panel_scroll_metrics(&snapshot.app, detail_area);
            if !crate::ui::should_show_scrollbar(metrics) {
                return Some(false);
            }
            let next = scrolled_offset(snapshot.app.agent_panel_scroll, delta, metrics);
            let changed = next != snapshot.app.agent_panel_scroll;
            self.agent_panel_scroll = next;
            return Some(changed);
        }

        let area = crate::ui::workspace_list_rect(
            snapshot.app.view.sidebar_rect,
            snapshot.app.sidebar_section_split,
        );
        let metrics = crate::ui::workspace_list_scroll_metrics(&snapshot.app, area);
        if !crate::ui::should_show_scrollbar(metrics) {
            return Some(false);
        }
        let next = scrolled_offset(snapshot.app.workspace_scroll, delta, metrics);
        let changed = next != snapshot.app.workspace_scroll;
        self.workspace_scroll = next;
        Some(changed)
    }

    /// #16: drag the spaces↔agents section divider, mirroring the monolithic host's
    /// `on_sidebar_section_divider` + `set_sidebar_section_split` (`src/app/input/`). Client-local
    /// (no server traffic): it only re-splits the sidebar's two panels, the content area is
    /// unchanged, so the caller redraws rather than resizing the remote PTY. Returns `Some(true)`
    /// when it owned the event (Down on the divider, or Drag/Up while dragging), else `None`.
    pub(crate) fn handle_sidebar_section_divider_mouse(
        &mut self,
        model: &crate::client::supervisor::ClientSupervisorModel,
        mouse: &crossterm::event::MouseEvent,
        host_width: u16,
        host_height: u16,
    ) -> Option<bool> {
        use crossterm::event::{MouseButton, MouseEventKind};

        let sidebar_width = self.effective_sidebar_width(host_width);
        if sidebar_width == 0 || host_height == 0 {
            return None;
        }
        // Resolve the divider row from the SAME rendered geometry the renderer uses, so a click
        // lands on exactly the drawn divider (render == hit_test). `sidebar_rect`/`split` are Copy,
        // so the snapshot borrow ends here and the match below can mutate `self`.
        let snapshot = ClientSidebarSnapshot::from_model(
            model,
            self,
            sidebar_width,
            host_width,
            host_height,
            Instant::now(),
        );
        let sidebar_rect = snapshot.app.view.sidebar_rect;
        let divider = crate::ui::sidebar_section_divider_rect(
            sidebar_rect,
            snapshot.app.sidebar_section_split,
        );

        match mouse.kind {
            MouseEventKind::Down(MouseButton::Left)
                if rect_contains(divider, mouse.column, mouse.row) =>
            {
                self.resizing_section = true;
                self.set_section_split_from_row(sidebar_rect, mouse.row);
                Some(true)
            }
            MouseEventKind::Drag(MouseButton::Left) if self.resizing_section => {
                self.set_section_split_from_row(sidebar_rect, mouse.row);
                Some(true)
            }
            MouseEventKind::Up(MouseButton::Left) if self.resizing_section => {
                self.resizing_section = false;
                Some(true)
            }
            _ => None,
        }
    }

    /// #16: set the section split from an absolute row, clamped 0.1..0.9 (mirrors the host's
    /// `set_sidebar_section_split`). Requires a tall-enough sidebar (>= 6 rows), matching
    /// `sidebar_section_divider_rect`'s guard.
    fn set_section_split_from_row(&mut self, sidebar_rect: Rect, row: u16) {
        let content_height = sidebar_rect.height;
        if content_height < 6 {
            return;
        }
        let relative_y = row.saturating_sub(sidebar_rect.y);
        let ratio = (relative_y as f32) / (content_height as f32);
        self.section_split = Some(ratio.clamp(0.1, 0.9));
    }

    /// #21: scrollbar track-click + thumb-drag for the workspace and agent lists (client-local; the
    /// scroll offsets already live on the compositor). A `Down` on a thumb starts a drag, a `Down`
    /// on the track pages to that position, a `Drag` moves the active thumb, and `Up` ends it.
    /// Returns `Some(changed)` when it owned the event, else `None` (caller continues dispatch).
    pub(crate) fn handle_sidebar_scrollbar_mouse(
        &mut self,
        model: &crate::client::supervisor::ClientSupervisorModel,
        mouse: &crossterm::event::MouseEvent,
        host_width: u16,
        host_height: u16,
    ) -> Option<bool> {
        use crossterm::event::{MouseButton, MouseEventKind};

        // A release always ends an active drag, regardless of geometry.
        if matches!(mouse.kind, MouseEventKind::Up(MouseButton::Left)) {
            return self.scrollbar_drag.take().map(|_| false);
        }
        let is_press = matches!(mouse.kind, MouseEventKind::Down(MouseButton::Left));
        let is_drag = matches!(mouse.kind, MouseEventKind::Drag(MouseButton::Left))
            && self.scrollbar_drag.is_some();
        if !is_press && !is_drag {
            return None;
        }
        let sidebar_width = self.effective_sidebar_width(host_width);
        if sidebar_width == 0 || host_height == 0 {
            return None;
        }

        // Snapshot geometry/metrics are Copy, so the immutable borrow of `self` (via `from_model`)
        // ends here and the offset writes below can mutate `self`.
        let snapshot = ClientSidebarSnapshot::from_model(
            model,
            self,
            sidebar_width,
            host_width,
            host_height,
            Instant::now(),
        );
        let ws_area = crate::ui::workspace_list_rect(
            snapshot.app.view.sidebar_rect,
            snapshot.app.sidebar_section_split,
        );
        let (_, agent_area) = crate::ui::expanded_sidebar_sections(
            snapshot.app.view.sidebar_rect,
            snapshot.app.sidebar_section_split,
        );
        let ws_track = crate::ui::workspace_list_scrollbar_rect(&snapshot.app, ws_area);
        let agent_track = crate::ui::agent_panel_scrollbar_rect(&snapshot.app, agent_area);
        let ws_metrics = crate::ui::workspace_list_scroll_metrics(&snapshot.app, ws_area);
        let agent_metrics = crate::ui::agent_panel_scroll_metrics(&snapshot.app, agent_area);

        if is_press {
            if let Some(track) = ws_track {
                if rect_contains(track, mouse.column, mouse.row) {
                    if let Some(grab) =
                        crate::ui::scrollbar_thumb_grab_offset(ws_metrics, track, mouse.row)
                    {
                        self.scrollbar_drag = Some(ScrollbarDrag {
                            panel: ScrollbarPanel::Workspace,
                            grab_row_offset: grab,
                        });
                        return Some(false);
                    }
                    let offset = crate::ui::scrollbar_offset_from_row(ws_metrics, track, mouse.row);
                    return Some(self.set_workspace_scroll_from_bottom(ws_metrics, offset));
                }
            }
            if let Some(track) = agent_track {
                if rect_contains(track, mouse.column, mouse.row) {
                    if let Some(grab) =
                        crate::ui::scrollbar_thumb_grab_offset(agent_metrics, track, mouse.row)
                    {
                        self.scrollbar_drag = Some(ScrollbarDrag {
                            panel: ScrollbarPanel::Agent,
                            grab_row_offset: grab,
                        });
                        return Some(false);
                    }
                    let offset =
                        crate::ui::scrollbar_offset_from_row(agent_metrics, track, mouse.row);
                    return Some(self.set_agent_scroll_from_bottom(agent_metrics, offset));
                }
            }
            return None;
        }

        // is_drag: move the active thumb, keeping the grab point under the pointer.
        let drag = self.scrollbar_drag?;
        match drag.panel {
            ScrollbarPanel::Workspace => {
                let track = ws_track?;
                let offset = crate::ui::scrollbar_offset_from_drag_row(
                    ws_metrics,
                    track,
                    mouse.row,
                    drag.grab_row_offset,
                );
                Some(self.set_workspace_scroll_from_bottom(ws_metrics, offset))
            }
            ScrollbarPanel::Agent => {
                let track = agent_track?;
                let offset = crate::ui::scrollbar_offset_from_drag_row(
                    agent_metrics,
                    track,
                    mouse.row,
                    drag.grab_row_offset,
                );
                Some(self.set_agent_scroll_from_bottom(agent_metrics, offset))
            }
        }
    }

    /// #21: convert a scrollbar `offset_from_bottom` to the workspace scroll offset and store it,
    /// mirroring the host's `set_workspace_list_offset_from_bottom`. Returns whether it changed.
    fn set_workspace_scroll_from_bottom(
        &mut self,
        metrics: crate::pane::ScrollMetrics,
        offset_from_bottom: usize,
    ) -> bool {
        let next = metrics
            .max_offset_from_bottom
            .saturating_sub(offset_from_bottom);
        let changed = next != self.workspace_scroll;
        self.workspace_scroll = next;
        changed
    }

    /// #21: agent-panel sibling of `set_workspace_scroll_from_bottom`.
    fn set_agent_scroll_from_bottom(
        &mut self,
        metrics: crate::pane::ScrollMetrics,
        offset_from_bottom: usize,
    ) -> bool {
        let next = metrics
            .max_offset_from_bottom
            .saturating_sub(offset_from_bottom);
        let changed = next != self.agent_panel_scroll;
        self.agent_panel_scroll = next;
        changed
    }

    /// #19: record a press on a workspace card (called when a `Down(Left)` hit-tests to a
    /// workspace). The click still focuses immediately (focus-on-down, unchanged); this only arms a
    /// potential drag-reorder that commits on release.
    pub(crate) fn begin_workspace_press(
        &mut self,
        server_id: ServerId,
        workspace_id: String,
        col: u16,
        row: u16,
    ) {
        self.workspace_press = Some(WorkspacePress {
            server_id,
            workspace_id,
            origin_col: col,
            origin_row: row,
            dragging: false,
            last_drag_row: None,
        });
    }

    /// #19: feed a `Drag`/`Up` to the workspace drag-reorder tracker. `Down` is recorded separately
    /// via `begin_workspace_press` (so the click can still focus). Returns `Ignored` for everything
    /// that is not an active drag, so the caller falls through to its normal dispatch.
    pub(crate) fn handle_workspace_reorder_mouse(
        &mut self,
        model: &crate::client::supervisor::ClientSupervisorModel,
        mouse: &crossterm::event::MouseEvent,
        host_width: u16,
        host_height: u16,
    ) -> WorkspaceReorderOutcome {
        use crossterm::event::{MouseButton, MouseEventKind};

        match mouse.kind {
            MouseEventKind::Drag(MouseButton::Left) => {
                let Some(press) = self.workspace_press.as_mut() else {
                    return WorkspaceReorderOutcome::Ignored;
                };
                let moved = mouse
                    .column
                    .abs_diff(press.origin_col)
                    .max(mouse.row.abs_diff(press.origin_row));
                if !press.dragging && moved < WORKSPACE_DRAG_THRESHOLD {
                    return WorkspaceReorderOutcome::Ignored;
                }
                press.dragging = true;
                // #19: remember the pointer row so `from_model` can draw a live drop indicator
                // (the slot this drag would commit to) every frame until release.
                press.last_drag_row = Some(mouse.row);
                WorkspaceReorderOutcome::Dragging
            }
            MouseEventKind::Up(MouseButton::Left) => {
                let Some(press) = self.workspace_press.take() else {
                    return WorkspaceReorderOutcome::Ignored;
                };
                if !press.dragging {
                    // A plain click (no drag) already focused on the down-press; nothing to commit.
                    return WorkspaceReorderOutcome::Ignored;
                }
                match self.workspace_reorder_target(
                    model,
                    &press,
                    mouse.row,
                    host_width,
                    host_height,
                ) {
                    Some(insert_index) => WorkspaceReorderOutcome::Commit {
                        server_id: press.server_id,
                        workspace_id: press.workspace_id,
                        insert_index,
                    },
                    None => WorkspaceReorderOutcome::Cancelled,
                }
            }
            _ => WorkspaceReorderOutcome::Ignored,
        }
    }

    /// #19: resolve a drop row to an insert position within the pressed server's workspace list.
    /// Reorder is constrained to the source server's rows (a workspace belongs to exactly one
    /// server), so the index is the count of that server's cards whose midpoint sits above the drop
    /// row — a 0..=len insert position matching the monolithic `move_workspace` contract.
    fn workspace_reorder_target(
        &self,
        model: &crate::client::supervisor::ClientSupervisorModel,
        press: &WorkspacePress,
        drop_row: u16,
        host_width: u16,
        host_height: u16,
    ) -> Option<usize> {
        let sidebar_width = self.effective_sidebar_width(host_width);
        if sidebar_width == 0 || host_height == 0 {
            return None;
        }
        let snapshot = ClientSidebarSnapshot::from_model(
            model,
            self,
            sidebar_width,
            host_width,
            host_height,
            Instant::now(),
        );
        // The pressed server's cards, in render order (== the server's stored workspace order).
        let server_rects: Vec<Rect> = snapshot
            .app
            .view
            .workspace_card_areas
            .iter()
            .filter_map(|card| {
                let route = snapshot.workspace_routes.get(card.ws_idx)?;
                (route.server_id == press.server_id && route.workspace_id.is_some())
                    .then_some(card.rect)
            })
            .collect();
        if server_rects.is_empty() {
            return None;
        }
        Some(server_insert_index(&server_rects, drop_row))
    }

    /// #19 (host half): record a press on a host banner (called when a `Down(Left)` hit-tests to a
    /// `HostBanner`). Mirrors `begin_workspace_press`: the click still focuses-on-down (handled by
    /// the normal dispatch); this only arms a potential host drag-reorder that commits on release.
    pub(crate) fn begin_host_press(&mut self, server_id: ServerId, col: u16, row: u16) {
        self.host_press = Some(HostPress {
            server_id,
            origin_col: col,
            origin_row: row,
            dragging: false,
            last_drag_row: None,
        });
    }

    /// #19 (host half): feed a `Drag`/`Up` to the host drag-reorder tracker, mirroring
    /// `handle_workspace_reorder_mouse`. `Down` is recorded separately via `begin_host_press` (so
    /// the click can still focus). Returns `Ignored` for everything that is not an active host drag.
    pub(crate) fn handle_host_reorder_mouse(
        &mut self,
        model: &crate::client::supervisor::ClientSupervisorModel,
        mouse: &crossterm::event::MouseEvent,
        host_width: u16,
        host_height: u16,
    ) -> HostReorderOutcome {
        use crossterm::event::{MouseButton, MouseEventKind};

        match mouse.kind {
            MouseEventKind::Drag(MouseButton::Left) => {
                let Some(press) = self.host_press.as_mut() else {
                    return HostReorderOutcome::Ignored;
                };
                let moved = mouse
                    .column
                    .abs_diff(press.origin_col)
                    .max(mouse.row.abs_diff(press.origin_row));
                if !press.dragging && moved < WORKSPACE_DRAG_THRESHOLD {
                    return HostReorderOutcome::Ignored;
                }
                press.dragging = true;
                press.last_drag_row = Some(mouse.row);
                HostReorderOutcome::Dragging
            }
            MouseEventKind::Up(MouseButton::Left) => {
                let Some(press) = self.host_press.take() else {
                    return HostReorderOutcome::Ignored;
                };
                if !press.dragging {
                    // A plain click (no drag) already focused on the down-press; nothing to commit.
                    return HostReorderOutcome::Ignored;
                }
                match self.host_reorder_target(model, mouse.row, host_width, host_height) {
                    Some(insert_index) => HostReorderOutcome::Commit {
                        source_server_id: press.server_id,
                        insert_index,
                    },
                    None => HostReorderOutcome::Cancelled,
                }
            }
            _ => HostReorderOutcome::Ignored,
        }
    }

    /// #19 (host half): resolve a drop row to an insert position among the ORDERED host list. The
    /// index is the count of host banners whose vertical midpoint sits above the drop row (the SAME
    /// `server_insert_index` rule used for workspaces), so the host drop slot is ALWAYS a host
    /// boundary — never inside a space block.
    fn host_reorder_target(
        &self,
        model: &crate::client::supervisor::ClientSupervisorModel,
        drop_row: u16,
        host_width: u16,
        host_height: u16,
    ) -> Option<usize> {
        let sidebar_width = self.effective_sidebar_width(host_width);
        if sidebar_width == 0 || host_height == 0 {
            return None;
        }
        let snapshot = ClientSidebarSnapshot::from_model(
            model,
            self,
            sidebar_width,
            host_width,
            host_height,
            Instant::now(),
        );
        let banner_rects: Vec<Rect> = snapshot
            .app
            .view
            .host_banner_areas
            .iter()
            .map(|banner| banner.rect)
            .collect();
        if banner_rects.is_empty() {
            return None;
        }
        Some(server_insert_index(&banner_rects, drop_row))
    }

    fn set_sidebar_width_from_column(
        &mut self,
        column: u16,
        host_width: u16,
        configured_min_width: u16,
        configured_max_width: u16,
    ) {
        if host_width <= 1 {
            self.sidebar_width = host_width;
            return;
        }
        let max_width = configured_max_width.min(host_width.saturating_sub(1));
        let min_width = configured_min_width.min(max_width);
        self.sidebar_width = column.saturating_add(1).clamp(min_width, max_width);
    }

    pub(crate) fn compose_frame(
        &self,
        model: &crate::client::supervisor::ClientSupervisorModel,
        active_frame: &FrameData,
        host_width: u16,
        host_height: u16,
        now: Instant,
    ) -> FrameData {
        let sidebar_width = self.effective_sidebar_width(host_width);
        let content_width = host_width.saturating_sub(sidebar_width);
        let snapshot = ClientSidebarSnapshot::from_model(
            model,
            self,
            sidebar_width,
            host_width,
            host_height,
            now,
        );
        // The composited client overlays (add-remote / new-workspace picker / manage-remotes) float
        // over the live content like the global launcher menu: they are footer-anchored popups (NOT
        // centered, full-screen-dimmed modals). The content copy must protect EXACTLY the open
        // popup's rect(s) — so the overlay stays visible AND the rest of the content shows around
        // it. Both render and these exclusion rects derive from the SAME `*_popup_rect(anchor_area)`
        // helpers, so what we protect lines up cell-for-cell with what gets drawn. Without an open
        // overlay we fall back to protecting just the open global-menu rect.
        let anchor_area = self.overlay_anchor_area(model, host_width, host_height);
        let mut excluded_rects: Vec<Rect> = Vec::new();
        if model.add_remote_form().is_some() {
            excluded_rects.extend(crate::ui::add_remote_popup_rect(anchor_area));
        } else if let Some((dests, _)) = snapshot.new_workspace_picker.as_ref() {
            excluded_rects.extend(crate::ui::new_workspace_picker_popup_rect(
                anchor_area,
                dests.len(),
            ));
        } else if let Some((overlay, rows)) = snapshot.remote_manage.as_ref() {
            excluded_rects.extend(crate::ui::remote_manage_popup_rect(anchor_area, rows.len()));
            if overlay.confirm_delete.is_some() {
                excluded_rects.extend(crate::ui::remote_manage_confirm_popup_rect(anchor_area));
            }
        } else if snapshot.workspace_context_menu.is_some() {
            // #23: protect the context-menu popup rect (same `*_popup_rect(anchor_area)` the
            // renderer uses), so the menu floats over the content cell-for-cell.
            excluded_rects.extend(crate::ui::workspace_context_menu_popup_rect(
                anchor_area,
                WORKSPACE_CONTEXT_MENU_ITEMS.len(),
            ));
        } else if snapshot.rename_workspace.is_some() {
            excluded_rects.extend(crate::ui::rename_workspace_popup_rect(anchor_area));
        } else if snapshot.confirm_close_workspace.is_some() {
            excluded_rects.extend(crate::ui::confirm_close_workspace_popup_rect(anchor_area));
        } else {
            excluded_rects.extend(snapshot.global_menu_rect());
        }
        let mut frame = render_client_shell(&snapshot, host_width, host_height);

        copy_active_content_excluding(
            active_frame,
            &mut frame,
            sidebar_width,
            content_width,
            &excluded_rects,
        );

        // item 1/3: the add-remote / new-workspace-picker / manage modals are rendered as ratatui
        // widgets inside `render_client_shell` (composited). Here we only force the cursor hidden
        // while ANY modal is open so the real terminal cursor never leaks through the modal.
        if model.add_remote_form().is_some()
            || model.new_workspace_picker().is_some()
            || model.remote_manage_overlay().is_some()
            || model.workspace_context_menu().is_some()
            || model.rename_workspace_form().is_some()
            || model.confirm_close_workspace().is_some()
        {
            frame.cursor = None;
        } else {
            frame.cursor =
                offset_cursor(active_frame.cursor.as_ref(), sidebar_width, content_width);
        }
        frame.hyperlinks = active_frame.hyperlinks.clone();
        if sidebar_width == 0 {
            frame.graphics = active_frame.graphics.clone();
        }

        frame
    }

    /// The footer-anchored `anchor_area` the composited client overlays (add-remote /
    /// new-workspace picker / manage-remotes) are positioned within: it spans the host top down to
    /// the sidebar footer row, so the popups open upward from the footer like the global launcher
    /// menu (instead of dead-centered). Render, content-copy exclusion, hit-test and hover-test all
    /// derive overlay geometry from this SAME rect, so they cannot drift.
    pub(crate) fn overlay_anchor_area(
        &self,
        model: &crate::client::supervisor::ClientSupervisorModel,
        host_width: u16,
        host_height: u16,
    ) -> Rect {
        let sidebar_width = self.effective_sidebar_width(host_width);
        let snapshot = ClientSidebarSnapshot::from_model(
            model,
            self,
            sidebar_width,
            host_width,
            host_height,
            Instant::now(),
        );
        Rect::new(0, 0, host_width, snapshot.app.sidebar_footer_rect().y)
    }

    pub(crate) fn content_size(&self, host_width: u16, host_height: u16) -> (u16, u16) {
        (
            host_width
                .saturating_sub(self.effective_sidebar_width(host_width))
                .max(1),
            host_height,
        )
    }

    fn effective_sidebar_width(&self, host_width: u16) -> u16 {
        if host_width <= 1 {
            return 0;
        }
        self.sidebar_width.min(host_width.saturating_sub(1))
    }

    pub(crate) fn hit_test(
        &self,
        model: &crate::client::supervisor::ClientSupervisorModel,
        x: u16,
        y: u16,
        host_width: u16,
        host_height: u16,
    ) -> Option<SidebarHitTarget> {
        let sidebar_width = self.effective_sidebar_width(host_width);
        if sidebar_width == 0 || host_height == 0 || y >= host_height {
            return None;
        }

        let snapshot = ClientSidebarSnapshot::from_model(
            model,
            self,
            sidebar_width,
            host_width,
            host_height,
            Instant::now(),
        );

        if let Some(target) = hit_test_global_menu(&snapshot.app, x, y) {
            return Some(target);
        }

        // item 1: the composited overlays are footer-anchored popups that float over the live
        // content, so their hit-test runs before the sidebar-width guard. Geometry is derived from
        // the SAME shared helpers the renderer uses (`new_workspace_picker_inner_rect`/`_row_rect`/
        // `add_remote_inner_rect` + the button-rect helpers) over the SAME `anchor_area`,
        // guaranteeing render == hit_test.
        let anchor_area = Rect::new(0, 0, host_width, snapshot.app.sidebar_footer_rect().y);
        if let Some(target) = hit_test_new_workspace_picker(&snapshot, anchor_area, x, y) {
            return Some(target);
        }
        if let Some(target) = hit_test_add_remote(&snapshot, anchor_area, x, y) {
            return Some(target);
        }
        // item 3 (Area 5): the manage overlay intercepts the whole host rect first (so a click on
        // a sidebar workspace row while the overlay is open never resolves to a `Workspace` hit).
        if snapshot.remote_manage.is_some() {
            return hit_test_remote_manage(&snapshot, anchor_area, x, y);
        }
        // #23: the workspace context menu / rename / confirm-close overlays are modal too — when
        // open they own the whole host rect (their own targets or none), so a click on a sidebar
        // row beneath never resolves to a `Workspace` hit. Geometry comes from the SAME shared `ui`
        // helpers the renderer uses, so render == hit_test.
        if snapshot.workspace_context_menu.is_some() {
            return hit_test_workspace_context_menu(&snapshot, anchor_area, x, y);
        }
        if snapshot.rename_workspace.is_some() {
            return hit_test_rename_workspace(&snapshot, anchor_area, x, y);
        }
        if snapshot.confirm_close_workspace.is_some() {
            return hit_test_confirm_close_workspace(&snapshot, anchor_area, x, y);
        }

        if x >= sidebar_width {
            return None;
        }

        // #25: the collapse/expand toggle is drawn in BOTH modes by `render_sidebar_toggle`,
        // using `expanded_sidebar_toggle_rect` (expanded) / `collapsed_sidebar_toggle_rect`
        // (collapsed) — the SAME rects the monolithic `on_sidebar_toggle` checks. Check it before
        // any mode-specific rows so it is hittable to collapse (expanded) AND to expand (collapsed).
        let toggle_rect = if snapshot.app.sidebar_collapsed {
            crate::ui::collapsed_sidebar_toggle_rect(snapshot.app.view.sidebar_rect)
        } else {
            crate::ui::expanded_sidebar_toggle_rect(snapshot.app.view.sidebar_rect)
        };
        if rect_contains(toggle_rect, x, y) {
            return Some(SidebarHitTarget::CollapsedSidebarToggle);
        }

        // #25: in collapsed mode the renderer drew the narrow workspace-glance + agent-detail
        // sections, so hit-test reads the COLLAPSED geometry and SKIPS every expanded hit-test
        // (filter/new/menu/banners/cards/sort-toggle/agent-rows all use expanded geometry).
        if snapshot.app.sidebar_collapsed {
            return collapsed_hit_test(&snapshot, x, y);
        }

        if rect_contains(
            filter_label_rect(snapshot.app.view.sidebar_rect, &snapshot.filter_label),
            x,
            y,
        ) {
            return Some(SidebarHitTarget::Filter);
        }
        if rect_contains(snapshot.app.sidebar_new_button_rect(), x, y) {
            return Some(SidebarHitTarget::New);
        }
        if rect_contains(snapshot.app.global_launcher_rect(), x, y) {
            return Some(SidebarHitTarget::Menu);
        }

        // #19 (host half): a press on a host banner resolves to that banner's host. Banner rows
        // produce no `WorkspaceCardArea`, so the card loop below never matches them; resolved here
        // before the cards so a `Down(Left)` arms a host drag-reorder. `host_banner_areas[i]` maps
        // to `host_banner_server_ids[i]` (same emission order — render == hit geometry).
        for (banner_idx, banner) in snapshot.app.view.host_banner_areas.iter().enumerate() {
            if rect_contains(banner.rect, x, y) {
                let server_id = snapshot.host_banner_server_ids.get(banner_idx)?;
                return Some(SidebarHitTarget::HostBanner {
                    server_id: server_id.clone(),
                });
            }
        }

        for card in &snapshot.app.view.workspace_card_areas {
            if rect_contains(card.rect, x, y) {
                // #22: a click in the chevron column (column 0 of a worktree-PARENT row) toggles the
                // group's collapsed state instead of focusing. Reuses the SERVER's chevron geometry
                // (`mouse.rs`: the chevron glyph is drawn at `card.rect.x`) and the SHARED
                // `workspace_parent_group_state` (parent = a group with >=2 members, not a linked
                // child). A NON-chevron click on the parent falls through to the `Workspace` focus
                // arm below, so the body click still focuses (matches the server contract).
                if x == card.rect.x {
                    if let Some((group_key, _collapsed)) =
                        crate::ui::workspace_parent_group_state(&snapshot.app, card.ws_idx)
                    {
                        return Some(SidebarHitTarget::WorktreeChevron { group_key });
                    }
                }
                let route = snapshot.workspace_routes.get(card.ws_idx)?;
                if route.disabled {
                    return None;
                }
                return route.workspace_id.clone().map(|workspace_id| {
                    SidebarHitTarget::Workspace {
                        server_id: route.server_id.clone(),
                        workspace_id,
                    }
                });
            }
        }

        // #20: the agents-panel sort toggle sits in the panel header; resolve it before the
        // entry rows so a click on `spaces`/`priority` flips the sort instead of focusing an agent.
        // Geometry is the SAME helper the renderer uses (`agent_panel_toggle_rect` over the
        // `expanded_sidebar_sections` detail area), so render == hit_test.
        if rect_contains(agent_panel_toggle_hit_rect(&snapshot.app), x, y) {
            return Some(SidebarHitTarget::AgentPanelSortToggle);
        }

        hit_test_agent_panel(&snapshot, x, y)
    }

    /// item 7 (Area 4): resolve a mouse-motion position to a sidebar hover target, sharing the
    /// SAME `ClientSidebarSnapshot` + rect checks as `hit_test` so render geometry and hover
    /// geometry cannot drift. Returns `None` (no highlight) for:
    /// - a collapsed/zero-width sidebar (`effective_sidebar_width == 0`),
    /// - an open add-remote form / global menu / manage overlay — those own their own hover (the
    ///   global menu moves its highlight on motion via `client_global_menu_item_at`, handled in the
    ///   client `Moved` arm before this fn), so the sidebar must not fight them,
    /// - positions outside the sidebar content,
    /// - disabled remote rows and `None`-`workspace_id` placeholders (matches `hit_test`),
    /// - non-selectable layout rows (divider/banner-skip + headers/separator — they produce no
    ///   card), and undrawn affordances (the ` new`/`menu` gate is `app.mouse_capture`).
    ///
    /// The new-workspace picker is a footer-anchored popup that DOES hover (its destination rows
    /// resolve to `NewWorkspaceDestination { row }`, before the sidebar-width guard, like
    /// `hit_test`). Never issues server traffic.
    pub(crate) fn hover_test(
        &self,
        model: &crate::client::supervisor::ClientSupervisorModel,
        x: u16,
        y: u16,
        host_width: u16,
        host_height: u16,
    ) -> Option<crate::app::state::SidebarHoverTarget> {
        use crate::app::state::SidebarHoverTarget;

        let sidebar_width = self.effective_sidebar_width(host_width);
        if sidebar_width == 0 || host_height == 0 || y >= host_height {
            return None;
        }

        // An open add-remote form / global menu / manage overlay owns input; the sidebar hover
        // must yield so the existing overlay hover is authoritative. The global menu moves its
        // highlight on motion (`client_global_menu_item_at`), handled in the client `Moved` arm.
        if model.client_global_menu_highlighted().is_some()
            || model.add_remote_form().is_some()
            || model.remote_manage_overlay().is_some()
        {
            return None;
        }

        let snapshot = ClientSidebarSnapshot::from_model(
            model,
            self,
            sidebar_width,
            host_width,
            host_height,
            Instant::now(),
        );

        // The new-workspace picker is a footer-anchored popup (item 1), so it hovers before the
        // sidebar-width guard — the SAME order/geometry `hit_test` uses for it.
        let anchor_area = Rect::new(0, 0, host_width, snapshot.app.sidebar_footer_rect().y);
        if let Some(target) = hover_test_new_workspace_picker(&snapshot, anchor_area, x, y) {
            return Some(target);
        }
        // While the picker is open the dimmed sidebar beneath is inert (matches `hit_test`).
        if snapshot.new_workspace_picker.is_some() {
            return None;
        }

        if x >= sidebar_width {
            return None;
        }

        if rect_contains(
            filter_label_rect(snapshot.app.view.sidebar_rect, &snapshot.filter_label),
            x,
            y,
        ) {
            return Some(SidebarHoverTarget::Filter);
        }
        // Affordance hover respects the SAME draw gate as the renderer (`app.mouse_capture` at
        // `sidebar.rs`): the ` new`/`menu` affordances only hover when they are actually drawn.
        if snapshot.app.mouse_capture {
            if rect_contains(snapshot.app.sidebar_new_button_rect(), x, y) {
                return Some(SidebarHoverTarget::New);
            }
            if rect_contains(snapshot.app.global_launcher_rect(), x, y) {
                return Some(SidebarHoverTarget::Menu);
            }
        }

        // host-banner rect (item 2): hoverable as `HostBanner { banner_idx }` when drawn. The
        // banner rows produce no `WorkspaceCardArea`, so they are skipped by the card loop below.
        for banner in &snapshot.app.view.host_banner_areas {
            if rect_contains(banner.rect, x, y) {
                return Some(SidebarHoverTarget::HostBanner {
                    banner_idx: banner.banner_idx,
                });
            }
        }

        // item-4 space-divider rows are non-selectable (they produce no card, so the card loop
        // below would skip them). Resolve them to the defensive `Divider` target, which render
        // treats as NO-highlight (a stable `None`-equivalent). Render never lifts a divider row —
        // the contract's "hover never highlights the divider" (Decision 4).
        if snapshot.app.view.divider_rows.contains(&y) {
            return Some(SidebarHoverTarget::Divider);
        }

        for card in &snapshot.app.view.workspace_card_areas {
            if rect_contains(card.rect, x, y) {
                let route = snapshot.workspace_routes.get(card.ws_idx)?;
                // disabled remote rows and `None`-id placeholders are not selectable → no hover
                // (matches `hit_test`'s rejection so click and hover agree).
                if route.disabled || route.workspace_id.is_none() {
                    return None;
                }
                return Some(SidebarHoverTarget::Workspace {
                    ws_idx: card.ws_idx,
                });
            }
        }

        // #20: hover the sort toggle (the monolithic host's `resolve_sidebar_hover` maps the same
        // rect onto `ScopeToggle`). Same geometry as `hit_test` so hover and click agree.
        if rect_contains(agent_panel_toggle_hit_rect(&snapshot.app), x, y) {
            return Some(SidebarHoverTarget::ScopeToggle);
        }

        hover_test_agent_panel(&snapshot, x, y)
    }

    /// item 7: resolve a mouse-motion position to a 0-based item index in the open client global
    /// menu, or `None` when the menu is closed / the position misses it. Shares the SAME snapshot +
    /// `global_menu_item_index_at` geometry as `hit_test`, so motion-driven highlight and click
    /// resolve identical rows. The client `Moved` arm feeds the result to
    /// `model.hover_client_global_menu_item`, mirroring the monolithic host's `global_menu.hover`.
    pub(crate) fn client_global_menu_item_at(
        &self,
        model: &crate::client::supervisor::ClientSupervisorModel,
        x: u16,
        y: u16,
        host_width: u16,
        host_height: u16,
    ) -> Option<usize> {
        let sidebar_width = self.effective_sidebar_width(host_width);
        if sidebar_width == 0
            || host_height == 0
            || model.client_global_menu_highlighted().is_none()
        {
            return None;
        }
        let snapshot = ClientSidebarSnapshot::from_model(
            model,
            self,
            sidebar_width,
            host_width,
            host_height,
            Instant::now(),
        );
        global_menu_item_index_at(&snapshot.app, x, y)
    }
}

fn scrolled_offset(current: usize, delta: i16, metrics: crate::pane::ScrollMetrics) -> usize {
    if delta.is_negative() {
        current.saturating_sub(delta.unsigned_abs() as usize)
    } else {
        current
            .saturating_add(delta as usize)
            .min(metrics.max_offset_from_bottom)
    }
}

impl Default for ClientCompositor {
    fn default() -> Self {
        Self::new(DEFAULT_SIDEBAR_WIDTH)
    }
}

impl ClientSidebarSnapshot {
    fn from_model(
        model: &crate::client::supervisor::ClientSupervisorModel,
        compositor: &ClientCompositor,
        sidebar_width: u16,
        host_width: u16,
        host_height: u16,
        now: Instant,
    ) -> Self {
        let mut app = crate::app::AppState::empty_for_client_rendering();
        let settings = model.ui_settings();
        app.sidebar_width = sidebar_width;
        app.default_sidebar_width = settings.sidebar_default_width;
        app.sidebar_min_width = settings.sidebar_min_width;
        app.sidebar_max_width = settings.sidebar_max_width;
        // #16: a client drag on the section divider overrides the settings default; otherwise
        // follow the server-provided default split.
        app.sidebar_section_split = compositor
            .section_split
            .unwrap_or_else(|| settings.sidebar_section_split());
        // #20: the client-local sort drives both the rendered entries and the flat `agent_routes`
        // alignment below, so it must be set before `agent_panel_entries` is consulted.
        app.agent_panel_sort = compositor.agent_panel_sort;
        app.sidebar_spaces = settings.sidebar_spaces.clone();
        app.sidebar_agents = settings.sidebar_agents.clone();
        // item 2 (C3): host-banner styling rides UiSettingsInfo over the wire.
        app.sidebar_host = settings.sidebar_host.clone();
        app.global_menu_extra_labels = vec!["add remote", "manage remotes"];
        // #25: gate the SHARED renderer onto its collapsed layout BEFORE geometry is computed, so
        // the collapsed sections + toggle rect are what gets laid out and what `hit_test` reads
        // back. Collapsed keeps the normal sidebar width (the client path never narrows the column;
        // width 0 still means "no sidebar", never collapsed).
        app.sidebar_collapsed = compositor.sidebar_collapsed;
        // #22: feed the client-local collapsed worktree-group keys into the SHARED grouping renderer
        // BEFORE geometry is computed, so `workspace_list_entries` collapses the right groups and the
        // chevron glyph renders the matching state. Client-owned (the aggregated view's collapse is a
        // per-client display concern), unlike the server's persisted set.
        app.collapsed_space_keys = compositor.collapsed_space_keys.clone();
        app.view.layout = ViewLayout::Desktop;
        app.view.sidebar_rect = Rect::new(0, 0, sidebar_width, host_height);
        app.view.terminal_area = Rect::new(
            sidebar_width,
            0,
            host_width.saturating_sub(sidebar_width),
            host_height,
        );
        app.mode = match model.client_global_menu_highlighted() {
            Some(highlighted) => {
                app.global_menu = MenuListState::new(
                    highlighted.min(app.global_menu_labels().len().saturating_sub(1)),
                );
                Mode::GlobalMenu
            }
            None => Mode::Navigate,
        };

        let mut agents_by_workspace = HashMap::<(ServerId, String), Vec<AgentSidebarRow>>::new();
        for group in model.agent_groups() {
            agents_by_workspace
                .entry((group.server_id, group.workspace_id))
                .or_default()
                .extend(group.agents);
        }

        let mut workspace_routes = Vec::new();
        // Per-workspace `pane_id -> AgentRoute` maps, index-aligned with `app.workspaces`, so the
        // flat `agent_routes` can be assembled from the SAME `agent_panel_entries` order the
        // renderer walks (which may reorder entries under the priority sort).
        let mut per_ws_pane_routes: Vec<HashMap<crate::layout::PaneId, AgentRoute>> = Vec::new();
        let mut active_idx = None;
        // #20: EVERY connected server permanently reports a focused workspace and a focused
        // pane — focus flags are per-server facts, not "which server is the user looking at".
        // Only the ACTIVE server's rows may claim the sidebar highlight: without this gate, a
        // later host's always-focused row steals the selected styling from the workspace/agent
        // actually on screen (spaces AND agents both key off `app.active`). Within the active
        // server, an agent-focused row outranks a merely workspace-focused one; ties keep
        // last-wins (the optimistic-focus override relies on it, and `focus_workspace_route`
        // switches the active server before flipping row focus, so optimistic focus stays
        // covered by the gate).
        let active_server = model.active_server_id().clone();
        let mut active_rank = 0u8; // 0 = none, 1 = workspace-focused, 2 = agent-focused
                                   // Fallback highlight when the active server reports no focused row (e.g. its rows are
                                   // disconnected placeholders): its first row still marks which host owns the view.
        let mut active_server_first_idx = None;
        let workspace_rows = model.workspace_rows();
        for (idx, row) in workspace_rows.into_iter().enumerate() {
            let row_on_active_server = row.server_id == active_server;
            if row_on_active_server && active_server_first_idx.is_none() {
                active_server_first_idx = Some(idx);
            }
            let agents = row
                .workspace_id
                .as_ref()
                .and_then(|workspace_id| {
                    agents_by_workspace.remove(&(row.server_id.clone(), workspace_id.clone()))
                })
                .unwrap_or_default();
            let focused_agent_idx = agents.iter().position(|agent| agent.focused);
            let row_rank = if focused_agent_idx.is_some() {
                2
            } else if row.focused {
                1
            } else {
                0
            };
            if row_on_active_server && row_rank > 0 && row_rank >= active_rank {
                active_idx = Some(idx);
                active_rank = row_rank;
            }

            let mut pane_terminals = Vec::new();
            let mut ws_agent_routes = Vec::new();
            for agent in &agents {
                let terminal_id = TerminalId::alloc();
                let (state, seen) = agent_state_from_status(&agent.status);
                let mut terminal = TerminalState::new(terminal_id.clone(), "/".into());
                terminal.set_agent_name(agent.label.clone());
                if state == AgentState::Working {
                    // Working agents flow through the detection setter so the Unknown→Working
                    // transition fires (a fresh terminal starts Unknown) and the shared renderer
                    // animates the working spinner exactly like a locally detected agent.
                    terminal.set_detected_state_with_screen_signals_at(
                        None, // agent: no detected Agent on the client path
                        crate::detect::AgentState::Working,
                        false, // visible_blocker
                        false, // visible_idle
                        true,  // visible_working
                        false, // process_exited
                        now,
                    );
                } else {
                    terminal.state = state;
                }
                app.terminals.insert(terminal_id.clone(), terminal);
                pane_terminals.push((terminal_id, seen));
                ws_agent_routes.push(AgentRoute {
                    server_id: row.server_id.clone(),
                    agent_id: agent.agent_id.clone(),
                });
            }

            let workspace_id = row
                .workspace_id
                .clone()
                .unwrap_or_else(|| format!("client-sidebar-row-{idx}"));
            let (mut workspace, pane_ids) = crate::workspace::Workspace::sidebar_placeholder(
                workspace_id,
                row.label.clone(),
                row.branch.clone(),
                pane_terminals,
                focused_agent_idx,
            );
            // #22: populate the placeholder's worktree grouping from the wire field so the SHARED
            // `workspace_list_entries` groups worktree parents/children exactly like the server. The
            // grouping renderer + `workspace_parent_group_state` only read `key` and
            // `is_linked_worktree`; the path fields are display-only placeholders here (the client
            // sidebar derives the child label from `branch`, not these paths).
            workspace.worktree_space =
                row.worktree_key
                    .clone()
                    .map(|key| crate::workspace::WorktreeSpaceMembership {
                        key,
                        label: row.label.clone(),
                        repo_root: std::path::PathBuf::new(),
                        checkout_path: std::path::PathBuf::new(),
                        is_linked_worktree: row.worktree_is_linked,
                    });
            app.workspaces.push(workspace);
            // item 4: mirror the per-row local/remote signal into AppState, index-aligned with
            // app.workspaces. Empty in monolithic mode (no rows), so monolithic emits no divider.
            app.client_workspace_remote.push(row.is_remote);
            // Zip the placeholder's created pane ids (in `pane_terminals` order) with the routes so
            // `agent_panel_entries` output can resolve `(ws_idx, pane_id)` back to an agent route.
            per_ws_pane_routes.push(
                pane_ids
                    .into_iter()
                    .zip(ws_agent_routes)
                    .collect::<HashMap<_, _>>(),
            );
            workspace_routes.push(WorkspaceRoute {
                server_id: row.server_id,
                workspace_id: row.workspace_id,
                disabled: row.disabled,
            });
        }

        if !app.workspaces.is_empty() {
            let selected = active_idx
                .or(active_server_first_idx)
                .unwrap_or(0)
                .min(app.workspaces.len() - 1);
            app.active = Some(selected);
            app.selected = selected;
        }

        // #20: flatten the per-workspace routes into `agent_routes`, in the SAME order the shared
        // `agent_panel_entries` walks them (including the client-local sort). Entries whose pane has
        // no route (defensive) are skipped — `agent_panel_entries` only yields panes whose terminal
        // carries an agent label, which the loop above created 1:1 with routes.
        let agent_routes: Vec<AgentRoute> = crate::ui::agent_panel_entries(&app)
            .iter()
            .filter_map(|entry| {
                per_ws_pane_routes
                    .get(entry.ws_idx)?
                    .get(&entry.pane_id)
                    .cloned()
            })
            .collect();
        app.workspace_scroll = crate::ui::normalized_workspace_scroll(
            &app,
            app.view.sidebar_rect,
            compositor.workspace_scroll,
        );
        let (_, detail_area) =
            crate::ui::expanded_sidebar_sections(app.view.sidebar_rect, app.sidebar_section_split);
        app.agent_panel_scroll = compositor
            .agent_panel_scroll
            .min(crate::ui::agent_panel_scroll_metrics(&app, detail_area).max_offset_from_bottom);
        // item 2 (C3): populate the per-host banner specs (one per visible server, in
        // visible_servers() order) and the coordination flag BEFORE computing geometry, so that
        // `workspace_list_entries` emits the HostBanner rows and flips the divider to plain. The
        // banner specs ride positionally: `HostBannerArea.banner_idx` indexes `app.host_banners`.
        let host_banner_specs = model.host_banner_specs();
        // The insertion index from `host_banner_specs` is a position in the flat
        // `workspace_rows()` stream, which is 1:1 with `app.workspaces` (each row pushed in
        // order above) — so it is a valid `ws_idx`. `host_banner_rows[i]` is the workspace the
        // i-th banner is emitted immediately before; `host_banners[i]` is its spec.
        app.host_banner_rows = host_banner_specs.iter().map(|(idx, _)| *idx).collect();
        app.host_banners = host_banner_specs
            .into_iter()
            .map(|(_, spec)| spec)
            .collect();
        app.host_banner_active = model.host_banner_active();
        // #19 (host half): the parallel `banner_idx -> server_id` map, built from the SAME
        // emission order as `host_banner_specs`, so hit-test and the drag preview resolve a banner
        // to its host. Carried on the snapshot for `hit_test`.
        let host_banner_server_ids = model.host_banner_server_ids();
        // item 4: one pass produces card rects, host-banner rects (item 2), and divider rows,
        // so render and hit-test share one geometry source. `host_banner_areas` is populated
        // from the second slot of THIS single call (render == hit_test geometry), and
        // `host_banner_active` (set above) flips the divider to plain when a banner is live.
        let (cards, banners, dividers) =
            crate::ui::compute_workspace_list_areas_full(&app, app.view.sidebar_rect);
        app.view.workspace_card_areas = cards;
        app.view.host_banner_areas = banners;
        app.view.divider_rows = dividers;
        // item 5: feed the single client-owned animation tick into the rendered AppState so
        // the braille agent spinner advances (was frozen at 0 via empty_for_client_rendering).
        app.spinner_tick = compositor.animation_tick();
        // item 7 (Area 4): mirror the compositor's hover truth into the render snapshot (Copy;
        // pure read). Render reads `app.sidebar_hover` and never mutates it.
        app.set_sidebar_hover(compositor.hover);

        // #19: while a workspace card is being dragged, populate `app.drag` so the shared sidebar
        // renderer draws the same live drop indicator the server-rendered sidebar shows (the client
        // path previously showed nothing until release). Reuse the existing render state — only
        // compute the dragged card's global index and the global insert slot the release would
        // commit to; the renderer (`render_workspace_list`) does the rest.
        if let Some(press) = compositor
            .workspace_press
            .as_ref()
            .filter(|press| press.dragging)
        {
            if let Some(drop_row) = press.last_drag_row {
                // The dragged card's GLOBAL index (into `app.workspaces` / `workspace_routes`).
                let source_ws_idx = workspace_routes.iter().position(|route| {
                    route.server_id == press.server_id
                        && route.workspace_id.as_deref() == Some(press.workspace_id.as_str())
                });
                // The source server's cards, in render order (== its stored workspace order). They
                // are contiguous in global `ws_idx`, so `base` is the block's first global index.
                let mut server_rects: Vec<Rect> = Vec::new();
                let mut base: Option<usize> = None;
                for card in &app.view.workspace_card_areas {
                    let Some(route) = workspace_routes.get(card.ws_idx) else {
                        continue;
                    };
                    if route.server_id == press.server_id && route.workspace_id.is_some() {
                        server_rects.push(card.rect);
                        base = Some(base.map_or(card.ws_idx, |b: usize| b.min(card.ws_idx)));
                    }
                }
                if let (Some(source_ws_idx), Some(base)) = (source_ws_idx, base) {
                    // Per-server insert position via the SAME midpoint rule as the commit path,
                    // offset into the source server's contiguous global block. Clamped to the block
                    // so the indicator can never point into another host's spaces.
                    let insert_idx = base + server_insert_index(&server_rects, drop_row);
                    app.drag = Some(crate::app::state::DragState {
                        target: crate::app::state::DragTarget::WorkspaceReorder {
                            source_ws_idx,
                            insert_idx: Some(insert_idx),
                        },
                    });
                }
            }
        }

        // #19 (host half): while a host banner is being dragged, populate `app.drag` with a
        // `HostReorder` so the shared renderer draws a host drop indicator at the boundary the
        // release would commit to. The insert slot is computed from the banner rects ONLY (via
        // `server_insert_index`), so it can never land inside a space block. Only one of
        // workspace_press/host_press is ever dragging, so this never fights the branch above.
        if let Some(press) = compositor
            .host_press
            .as_ref()
            .filter(|press| press.dragging)
        {
            if let Some(drop_row) = press.last_drag_row {
                let source_host_idx = host_banner_server_ids
                    .iter()
                    .position(|id| id == &press.server_id);
                let banner_rects: Vec<Rect> = app
                    .view
                    .host_banner_areas
                    .iter()
                    .map(|banner| banner.rect)
                    .collect();
                if let Some(source_host_idx) = source_host_idx {
                    if !banner_rects.is_empty() {
                        let insert_idx = server_insert_index(&banner_rects, drop_row);
                        app.drag = Some(crate::app::state::DragState {
                            target: crate::app::state::DragTarget::HostReorder {
                                source_host_idx,
                                insert_idx: Some(insert_idx),
                            },
                        });
                    }
                }
            }
        }

        Self {
            app,
            filter_label: model.filter_label(),
            workspace_routes,
            host_banner_server_ids,
            agent_routes,
            // item 1: clone the overlay state out of the model into ui-owned carriers (pure read).
            // The closure maps these into ui view structs before rendering.
            add_remote_form: model.add_remote_form().cloned(),
            new_workspace_picker: model
                .new_workspace_picker()
                .map(|picker| (picker.destinations.clone(), picker.selected)),
            // item 3: clone the overlay state + the secondary rows it renders out of the model
            // (pure read). The render closure maps the rows into ui-owned views.
            remote_manage: model
                .remote_manage_overlay()
                .map(|overlay| (overlay.clone(), model.remote_manage_rows())),
            // #23: clone the workspace context-menu / rename / confirm-close overlay state out of
            // the model into ui-owned carriers (pure read). The render closure maps these into ui
            // view structs before drawing.
            workspace_context_menu: model.workspace_context_menu().cloned(),
            rename_workspace: model.rename_workspace_form().cloned(),
            confirm_close_workspace: model.confirm_close_workspace().cloned(),
        }
    }

    fn global_menu_rect(&self) -> Option<Rect> {
        matches!(self.app.mode, Mode::GlobalMenu).then(|| self.app.global_menu_rect())
    }
}

fn render_client_shell(
    snapshot: &ClientSidebarSnapshot,
    host_width: u16,
    host_height: u16,
) -> FrameData {
    if host_width == 0 || host_height == 0 {
        return FrameData::blank(host_width, host_height);
    }

    let backend = ratatui::backend::TestBackend::new(host_width, host_height);
    let mut terminal = match ratatui::Terminal::new(backend) {
        Ok(terminal) => terminal,
        Err(err) => {
            warn!(err = %err, "client shell offscreen terminal setup failed");
            return FrameData::blank(host_width, host_height);
        }
    };
    let terminal_runtimes = TerminalRuntimeRegistry::new();
    let draw_result = terminal.draw(|frame| {
        // #25: the SHARED renderer branch — collapsed draws the narrow glance/detail layout
        // (mirroring `render_with_runtime_registry`); expanded draws the full sidebar + the
        // client-owned filter label overlay (top-right, over the ` spaces` header row).
        if snapshot.app.sidebar_collapsed {
            crate::ui::render_sidebar_collapsed(
                &snapshot.app,
                frame,
                snapshot.app.view.sidebar_rect,
            );
        } else {
            crate::ui::render_sidebar(
                &snapshot.app,
                &terminal_runtimes,
                frame,
                snapshot.app.view.sidebar_rect,
            );
            render_filter_label(snapshot, frame);
        }
        if matches!(snapshot.app.mode, Mode::GlobalMenu) {
            crate::ui::render_global_launcher_menu(&snapshot.app, frame);
        }
        // item 1: render the composited client overlays as footer-anchored popups that float
        // over the live content — the proven `render_global_launcher_menu` compositing path.
        // `anchor_area` spans the host top down to the sidebar footer row so the popups open
        // upward from the footer (matching the launcher menu), NOT dead-centered. The
        // compositor maps the ui-owned snapshot carriers into ui view structs here (no
        // supervisor types reach `ui`).
        let anchor_area = Rect::new(0, 0, host_width, snapshot.app.sidebar_footer_rect().y);
        if let Some((dests, selected)) = &snapshot.new_workspace_picker {
            let views: Vec<crate::ui::DestinationView> = dests
                .iter()
                .map(|d| crate::ui::DestinationView {
                    display_name: &d.display_name,
                })
                .collect();
            // item 7 (Area 4): pass the hovered destination row (mirrored into the snapshot)
            // so the modal lifts it; the picker's `Moved` resolves `NewWorkspaceDestination`.
            let hovered_row = match snapshot.app.sidebar_hover {
                Some(crate::app::state::SidebarHoverTarget::NewWorkspaceDestination { row }) => {
                    Some(row as usize)
                }
                _ => None,
            };
            crate::ui::render_new_workspace_picker_overlay(
                &snapshot.app.palette,
                &views,
                *selected,
                hovered_row,
                frame,
                anchor_area,
            );
        }
        if let Some(form) = &snapshot.add_remote_form {
            let view = crate::ui::AddRemoteOverlayView {
                target: &form.target,
                name: &form.name,
                focused_is_target: form.focused_field
                    == crate::client::supervisor::AddRemoteField::Target,
                error: form.error.as_deref(),
                in_progress: form.in_progress,
                spinner: crate::ui::spinner_frame(snapshot.app.spinner_tick),
                restart_confirm_destination: form
                    .restart_confirm
                    .as_ref()
                    .map(|confirm| confirm.destination.as_str()),
            };
            crate::ui::render_add_remote_overlay(&snapshot.app.palette, &view, frame, anchor_area);
        }
        // item 3 (Area 5): render the remote-management overlay as a footer-anchored popup. The
        // compositor maps the supervisor rows into ui-owned views here (no supervisor types
        // reach `ui`).
        if let Some((overlay, rows)) = &snapshot.remote_manage {
            let views = model_remote_manage_row_views(rows);
            crate::ui::render_remote_manage_overlay(
                &snapshot.app.palette,
                &views,
                overlay.selected,
                overlay.scroll,
                overlay.confirm_delete.as_deref(),
                frame,
                anchor_area,
            );
        }
        // #23: render the workspace context menu / rename / confirm-close overlays. The
        // compositor maps the supervisor state into ui-owned views here (no supervisor types
        // reach `ui`). At most one is ever open (single `client_overlay` slot).
        if let Some(menu) = &snapshot.workspace_context_menu {
            let rows: Vec<&str> = WORKSPACE_CONTEXT_MENU_ITEMS.to_vec();
            let view = crate::ui::WorkspaceContextMenuView {
                label: &menu.label,
                rows: &rows,
            };
            crate::ui::render_workspace_context_menu_overlay(
                &snapshot.app.palette,
                &view,
                menu.selected,
                frame,
                anchor_area,
            );
        }
        if let Some(form) = &snapshot.rename_workspace {
            crate::ui::render_rename_workspace_overlay(
                &snapshot.app.palette,
                &form.label,
                form.error.as_deref(),
                frame,
                anchor_area,
            );
        }
        if let Some(confirm) = &snapshot.confirm_close_workspace {
            crate::ui::render_confirm_close_workspace_overlay(
                &snapshot.app.palette,
                &confirm.label,
                frame,
                anchor_area,
            );
        }
    });
    if let Err(err) = draw_result {
        warn!(err = %err, "client shell offscreen render failed");
        return FrameData::blank(host_width, host_height);
    }

    let buffer = terminal.backend().buffer().clone();
    FrameData::from_ratatui_buffer_with_hyperlinks(&buffer, None, &[])
}

fn render_filter_label(snapshot: &ClientSidebarSnapshot, frame: &mut ratatui::Frame) {
    let rect = filter_label_rect(snapshot.app.view.sidebar_rect, &snapshot.filter_label);
    if rect == Rect::default() {
        return;
    }
    // item 7 (Area 4): the filter label hover lifts its fg overlay0 → subtext0.
    let fg = if snapshot.app.sidebar_hover == Some(crate::app::state::SidebarHoverTarget::Filter) {
        snapshot.app.palette.subtext0
    } else {
        snapshot.app.palette.overlay0
    };
    frame.render_widget(
        Paragraph::new(Span::styled(
            snapshot.filter_label.clone(),
            Style::default().fg(fg).add_modifier(Modifier::BOLD),
        ))
        .alignment(Alignment::Right),
        rect,
    );
}

fn filter_label_rect(sidebar: Rect, label: &str) -> Rect {
    if sidebar.width <= 1 || sidebar.height == 0 || label.is_empty() {
        return Rect::default();
    }
    let content_width = sidebar.width.saturating_sub(1);
    let width = (UnicodeWidthStr::width(label) as u16).min(content_width);
    Rect::new(
        sidebar.x + content_width.saturating_sub(width),
        sidebar.y,
        width,
        1,
    )
}

fn agent_state_from_status(status: &str) -> (AgentState, bool) {
    match status {
        "working" => (AgentState::Working, true),
        "blocked" => (AgentState::Blocked, true),
        "done" => (AgentState::Idle, false),
        "idle" => (AgentState::Idle, true),
        _ => (AgentState::Unknown, true),
    }
}

/// Whether anything on the client sidebar is currently animating, gating the animation
/// cadence (no idle CPU spin). Read-only over the cached model; performs NO I/O. The ONLY
/// banner-active input is `host_banner_animation_active` (contract Area 1: do not invent a
/// second clock or second flag).
pub(crate) fn sidebar_wants_animation(
    model: &crate::client::supervisor::ClientSupervisorModel,
) -> bool {
    model
        .agent_groups()
        .iter()
        .any(|g| g.agents.iter().any(|r| r.status == "working"))
        || model.host_banner_animation_active()
        || model.add_remote_in_progress()
}

/// item 3 (Area 5): map the supervisor `RemoteManageRow`s into ui-owned `RemoteManageRowView`s
/// (borrowing the row strings). This keeps `client::supervisor` types out of `ui` (the one-way
/// layering rule, contradiction 13).
fn model_remote_manage_row_views(
    rows: &[crate::client::supervisor::RemoteManageRow],
) -> Vec<crate::ui::RemoteManageRowView<'_>> {
    use crate::client::supervisor::RemoteManageState;
    rows.iter()
        .map(|row| crate::ui::RemoteManageRowView {
            glyph: match row.state {
                RemoteManageState::Connected => crate::ui::RemoteStateGlyph::Connected,
                RemoteManageState::Connecting => crate::ui::RemoteStateGlyph::Connecting,
                RemoteManageState::Disconnected => crate::ui::RemoteStateGlyph::Disconnected,
                RemoteManageState::Disabled => crate::ui::RemoteStateGlyph::Disabled,
                RemoteManageState::ProtocolMismatch => {
                    crate::ui::RemoteStateGlyph::ProtocolMismatch
                }
            },
            name: &row.name,
            target: &row.target,
            state_word: row.state.state_word(),
            disabled: !row.enabled,
        })
        .collect()
}

/// item 1: hit-test the footer-anchored new-workspace picker popup. Returns a destination row
/// target, the confirm/cancel buttons, or `None` when the picker is closed or the click misses.
/// Geometry is derived from the SAME helpers the renderer uses (`new_workspace_picker_inner_rect`/
/// `_row_rect` + `new_workspace_picker_button_rects`) over the same `full_rect`, so render ==
/// hit_test.
fn hit_test_new_workspace_picker(
    snapshot: &ClientSidebarSnapshot,
    full_rect: Rect,
    x: u16,
    y: u16,
) -> Option<SidebarHitTarget> {
    let (destinations, _) = snapshot.new_workspace_picker.as_ref()?;
    let inner = crate::ui::new_workspace_picker_inner_rect(full_rect, destinations.len())?;

    // buttons take precedence over the (overlapping) actions row.
    let (confirm_rect, cancel_rect) = crate::ui::new_workspace_picker_button_rects(inner);
    if rect_contains(confirm_rect, x, y) {
        return Some(SidebarHitTarget::NewWorkspacePickerConfirm);
    }
    if rect_contains(cancel_rect, x, y) {
        return Some(SidebarHitTarget::NewWorkspacePickerCancel);
    }

    // destination rows — same `max_rows` clamp the renderer applies.
    let max_rows = inner.height.saturating_sub(3) as usize;
    for (row_index, destination) in destinations.iter().enumerate().take(max_rows) {
        let row = crate::ui::new_workspace_picker_row_rect(inner, row_index);
        if rect_contains(row, x, y) {
            return Some(SidebarHitTarget::NewWorkspaceDestination {
                server_id: destination.server_id.clone(),
            });
        }
    }
    None
}

/// item 7 (Area 4): hover sibling of `hit_test_new_workspace_picker`. Returns
/// `NewWorkspaceDestination { row }` (keyed on the modal's logical row index, which the modal
/// render keys on) for a hovered destination row. The confirm/cancel buttons have their own
/// styling and are not hover targets. Uses the SAME geometry the renderer + hit-test use, so
/// render == hover_test for the modal rows.
fn hover_test_new_workspace_picker(
    snapshot: &ClientSidebarSnapshot,
    full_rect: Rect,
    x: u16,
    y: u16,
) -> Option<crate::app::state::SidebarHoverTarget> {
    let (destinations, _) = snapshot.new_workspace_picker.as_ref()?;
    let inner = crate::ui::new_workspace_picker_inner_rect(full_rect, destinations.len())?;
    let max_rows = inner.height.saturating_sub(3) as usize;
    for row_index in 0..destinations.len().min(max_rows) {
        let row = crate::ui::new_workspace_picker_row_rect(inner, row_index);
        if rect_contains(row, x, y) {
            return Some(
                crate::app::state::SidebarHoverTarget::NewWorkspaceDestination {
                    row: row_index as u16,
                },
            );
        }
    }
    None
}

/// item 1: hit-test the add-remote modal's submit/cancel buttons. Returns `None` when the
/// form is closed or the click misses. Uses the shared fixed `add_remote_inner_rect` geometry.
fn hit_test_add_remote(
    snapshot: &ClientSidebarSnapshot,
    full_rect: Rect,
    x: u16,
    y: u16,
) -> Option<SidebarHitTarget> {
    snapshot.add_remote_form.as_ref()?;
    let inner = crate::ui::add_remote_inner_rect(full_rect)?;
    let (submit_rect, cancel_rect) = crate::ui::add_remote_button_rects(inner);
    if rect_contains(submit_rect, x, y) {
        return Some(SidebarHitTarget::AddRemoteSubmit);
    }
    if rect_contains(cancel_rect, x, y) {
        return Some(SidebarHitTarget::AddRemoteCancel);
    }
    None
}

/// item 3 (Area 5): hit-test the remote-management overlay. When delete-confirm is
/// active the red popup OWNS input (its buttons are the only hit targets; list rows are inert).
/// Otherwise a click on a rendered row selects it, and the footer `add` affordance opens the
/// add-remote form. Geometry comes from the SAME shared helpers the renderer uses
/// (`remote_manage_inner_rect`/`_row_rect`/`_confirm_*`), guaranteeing render == hit_test.
fn hit_test_remote_manage(
    snapshot: &ClientSidebarSnapshot,
    full_rect: Rect,
    x: u16,
    y: u16,
) -> Option<SidebarHitTarget> {
    let (overlay, rows) = snapshot.remote_manage.as_ref()?;

    // delete-confirm sub-state: only the popup buttons are hit-testable.
    if overlay.confirm_delete.is_some() {
        let popup = crate::ui::remote_manage_confirm_popup_rect(full_rect)?;
        let inner = Rect::new(
            popup.x + 1,
            popup.y + 1,
            popup.width.saturating_sub(2),
            popup.height.saturating_sub(2),
        );
        let (delete_rect, cancel_rect) = crate::ui::remote_manage_confirm_button_rects(inner);
        if rect_contains(delete_rect, x, y) {
            return Some(SidebarHitTarget::RemoteManageConfirmDelete);
        }
        if rect_contains(cancel_rect, x, y) {
            return Some(SidebarHitTarget::RemoteManageCancelDelete);
        }
        return None;
    }

    let inner = crate::ui::remote_manage_inner_rect(full_rect, rows.len())?;

    // footer hint row hosts the `add` affordance (whole footer row).
    let footer = Rect::new(
        inner.x,
        inner.y + inner.height.saturating_sub(1),
        inner.width,
        1,
    );
    if rect_contains(footer, x, y) {
        return Some(SidebarHitTarget::RemoteManageAdd);
    }

    // rows — same `max_rows`/visible-window clamp the renderer applies
    // (`render_remote_manage_overlay`'s shared scroll math).
    let max_rows = inner.height.saturating_sub(3) as usize;
    let selected = overlay.selected.min(rows.len().saturating_sub(1));
    let start = overlay
        .scroll
        .min(rows.len().saturating_sub(max_rows.max(1)))
        .min(selected)
        .max(selected.saturating_sub(max_rows.max(1).saturating_sub(1)));
    for (visible_idx, (row_index, _)) in rows
        .iter()
        .enumerate()
        .skip(start)
        .take(max_rows)
        .enumerate()
    {
        let rect = crate::ui::remote_manage_row_rect(inner, visible_idx);
        if rect_contains(rect, x, y) {
            return Some(SidebarHitTarget::RemoteManageRow { index: row_index });
        }
    }
    None
}

/// #23: hit-test the workspace context menu. Resolves a click on a menu row to its index, derived
/// from the SAME shared `ui` geometry the renderer uses (`workspace_context_menu_inner_rect` /
/// `_row_rect`), guaranteeing render == hit_test. The overlay is modal: a click that misses every
/// row returns `None` (the dimmed sidebar beneath never hit-tests).
fn hit_test_workspace_context_menu(
    snapshot: &ClientSidebarSnapshot,
    full_rect: Rect,
    x: u16,
    y: u16,
) -> Option<SidebarHitTarget> {
    snapshot.workspace_context_menu.as_ref()?;
    let count = WORKSPACE_CONTEXT_MENU_ITEMS.len();
    let inner = crate::ui::workspace_context_menu_inner_rect(full_rect, count)?;
    for index in 0..count {
        let rect = crate::ui::workspace_context_menu_row_rect(inner, index);
        if rect_contains(rect, x, y) {
            return Some(SidebarHitTarget::WorkspaceContextMenuRow { index });
        }
    }
    None
}

/// #23: hit-test the rename overlay's submit/cancel buttons. Geometry comes from the shared
/// `rename_workspace_inner_rect` + `rename_workspace_button_rects`, so render == hit_test.
fn hit_test_rename_workspace(
    snapshot: &ClientSidebarSnapshot,
    full_rect: Rect,
    x: u16,
    y: u16,
) -> Option<SidebarHitTarget> {
    snapshot.rename_workspace.as_ref()?;
    let inner = crate::ui::rename_workspace_inner_rect(full_rect)?;
    let (submit_rect, cancel_rect) = crate::ui::rename_workspace_button_rects(inner);
    if rect_contains(submit_rect, x, y) {
        return Some(SidebarHitTarget::RenameWorkspaceSubmit);
    }
    if rect_contains(cancel_rect, x, y) {
        return Some(SidebarHitTarget::RenameWorkspaceCancel);
    }
    None
}

/// #23: hit-test the close-confirm overlay's confirm/cancel buttons. Geometry comes from the
/// shared `confirm_close_workspace_popup_rect` + `confirm_close_workspace_button_rects`.
fn hit_test_confirm_close_workspace(
    snapshot: &ClientSidebarSnapshot,
    full_rect: Rect,
    x: u16,
    y: u16,
) -> Option<SidebarHitTarget> {
    snapshot.confirm_close_workspace.as_ref()?;
    let popup = crate::ui::confirm_close_workspace_popup_rect(full_rect)?;
    let inner = Rect::new(
        popup.x + 1,
        popup.y + 1,
        popup.width.saturating_sub(2),
        popup.height.saturating_sub(2),
    );
    let (confirm_rect, cancel_rect) = crate::ui::confirm_close_workspace_button_rects(inner);
    if rect_contains(confirm_rect, x, y) {
        return Some(SidebarHitTarget::ConfirmCloseWorkspaceConfirm);
    }
    if rect_contains(cancel_rect, x, y) {
        return Some(SidebarHitTarget::ConfirmCloseWorkspaceCancel);
    }
    None
}

/// Shared geometry for the open global launcher menu: resolve a position to a 0-based item index,
/// or `None` when the menu is closed or the position misses the menu's inner item rows. Both
/// `hit_test_global_menu` (click) and `client_global_menu_item_at` (motion) resolve through this so
/// click and hover geometry cannot drift from the `render_global_launcher_menu` row layout.
fn global_menu_item_index_at(app: &crate::app::AppState, x: u16, y: u16) -> Option<usize> {
    if !matches!(app.mode, Mode::GlobalMenu) {
        return None;
    }
    let rect = app.global_menu_rect();
    let inner_x = rect.x.saturating_add(1);
    let inner_y = rect.y.saturating_add(1);
    let inner_right = rect.x.saturating_add(rect.width).saturating_sub(1);
    let inner_bottom = rect.y.saturating_add(rect.height).saturating_sub(1);
    if x < inner_x || x >= inner_right || y < inner_y || y >= inner_bottom {
        return None;
    }
    let index = (y - inner_y) as usize;
    (index < app.global_menu_labels().len()).then_some(index)
}

fn hit_test_global_menu(app: &crate::app::AppState, x: u16, y: u16) -> Option<SidebarHitTarget> {
    global_menu_item_index_at(app, x, y)
        .map(|index| SidebarHitTarget::ClientGlobalMenuItem { index })
}

/// #20: the rect of the agents-panel sort toggle in the rendered snapshot, derived from
/// the SAME `expanded_sidebar_sections` detail area + `agent_panel_toggle_rect` the renderer uses
/// (`render_agent_detail`). Returns an empty rect when the panel is too short to draw the toggle.
fn agent_panel_toggle_hit_rect(app: &crate::app::AppState) -> Rect {
    let (_, detail_area) =
        crate::ui::expanded_sidebar_sections(app.view.sidebar_rect, app.sidebar_section_split);
    crate::ui::agent_panel_toggle_rect(detail_area, app.agent_panel_sort)
}

/// #25: collapsed-mode row hit-test. The collapsed renderer draws (top→bottom) a narrow
/// workspace-glance section then an agent-detail section, both from the SHARED
/// `collapsed_sidebar_sections` geometry. Mirror the server's `collapsed_*_at` row math here,
/// resolving each row to the owning server via the SAME `workspace_routes`/`agent_routes` the
/// expanded path uses, so a collapsed click focuses the right server exactly like the expanded one.
/// The toggle is resolved by the caller; this only covers the two row sections.
fn collapsed_hit_test(
    snapshot: &ClientSidebarSnapshot,
    x: u16,
    y: u16,
) -> Option<SidebarHitTarget> {
    let app = &snapshot.app;
    let sidebar_rect = app.view.sidebar_rect;
    if x >= sidebar_rect.x + sidebar_rect.width {
        return None;
    }
    let (ws_area, _, detail_area) = crate::ui::collapsed_sidebar_sections(sidebar_rect);

    // Agent-detail rows (mirrors the server's `collapsed_agent_detail_target_at`): the collapsed
    // detail section draws one row per `agent_panel_entries` entry (unscrolled), and the flat
    // `agent_routes` are index-aligned with those entries — so row `i` resolves to route `i`, the
    // SAME `AgentRoute { server_id, agent_id }` the expanded path focuses. The last detail row is
    // reserved (the renderer subtracts 1 for the toggle row), so use `detail_content_area`.
    let detail_content_area = Rect::new(
        detail_area.x,
        detail_area.y,
        detail_area.width,
        detail_area.height.saturating_sub(1),
    );
    if detail_content_area != Rect::default()
        && y >= detail_content_area.y
        && y < detail_content_area.y + detail_content_area.height
    {
        let detail_idx = (y - detail_content_area.y) as usize;
        if let Some(route) = snapshot.agent_routes.get(detail_idx) {
            return Some(SidebarHitTarget::Agent {
                server_id: route.server_id.clone(),
                agent_id: route.agent_id.clone(),
            });
        }
        return None;
    }

    // Workspace-glance rows (mirrors the server's `collapsed_workspace_at_row`): one row per
    // workspace, `idx == y - ws_area.y`, resolved through `workspace_routes` exactly like the
    // expanded card path (disabled → no hit, Some(id) → Workspace, None → new-workspace dest).
    if ws_area != Rect::default() && y >= ws_area.y && y < ws_area.y + ws_area.height {
        let idx = (y - ws_area.y) as usize;
        if let Some(route) = snapshot.workspace_routes.get(idx) {
            if route.disabled {
                return None;
            }
            return Some(match route.workspace_id.clone() {
                Some(workspace_id) => SidebarHitTarget::Workspace {
                    server_id: route.server_id.clone(),
                    workspace_id,
                },
                None => SidebarHitTarget::NewWorkspaceDestination {
                    server_id: route.server_id.clone(),
                },
            });
        }
    }

    None
}

/// Resolve a position inside the expanded agents panel to the flat `agent_panel_entries` index
/// (`agent_panel_scroll + visible offset`), mirroring the monolithic `agent_detail_target_at`
/// walk — entries have PER-ENTRY heights (`agent_entry_height_in_body`, token rows) with a
/// one-row gap between them, so a fixed stride would misresolve multi-row entries.
fn agent_panel_route_index_at(app: &crate::app::AppState, x: u16, y: u16) -> Option<usize> {
    let (_, detail_area) =
        crate::ui::expanded_sidebar_sections(app.view.sidebar_rect, app.sidebar_section_split);
    let metrics = crate::ui::agent_panel_scroll_metrics(app, detail_area);
    let body =
        crate::ui::agent_panel_body_rect(detail_area, crate::ui::should_show_scrollbar(metrics));
    if !rect_contains(body, x, y) {
        return None;
    }

    let mut row_y = body.y;
    for (offset, entry) in crate::ui::agent_panel_entries(app)
        .iter()
        .skip(app.agent_panel_scroll)
        .enumerate()
    {
        let height = crate::ui::agent_entry_height_in_body(app, entry, body.height);
        if row_y.saturating_add(height) > body.y + body.height {
            break;
        }
        if y >= row_y && y < row_y.saturating_add(height) {
            return Some(app.agent_panel_scroll + offset);
        }
        row_y = row_y.saturating_add(height);
        if row_y < body.y + body.height {
            row_y = row_y.saturating_add(1);
        }
    }
    None
}

fn hit_test_agent_panel(
    snapshot: &ClientSidebarSnapshot,
    x: u16,
    y: u16,
) -> Option<SidebarHitTarget> {
    let route_idx = agent_panel_route_index_at(&snapshot.app, x, y)?;
    let route = snapshot.agent_routes.get(route_idx)?;
    Some(SidebarHitTarget::Agent {
        server_id: route.server_id.clone(),
        agent_id: route.agent_id.clone(),
    })
}

/// item 7 (Area 4): hover sibling of `hit_test_agent_panel`. Returns `AgentRoute { route_idx }`
/// where `route_idx` is the SAME flat `agent_routes` index `hit_test_agent_panel` resolves. The
/// index is positional in `agent_panel_entries` order, so it survives recompose (a captured
/// `pane_id` would not, contradiction 11).
fn hover_test_agent_panel(
    snapshot: &ClientSidebarSnapshot,
    x: u16,
    y: u16,
) -> Option<crate::app::state::SidebarHoverTarget> {
    let route_idx = agent_panel_route_index_at(&snapshot.app, x, y)?;
    // only a real agent route resolves (the gap rows / over-scroll resolve to None).
    snapshot.agent_routes.get(route_idx)?;
    Some(crate::app::state::SidebarHoverTarget::AgentRoute { route_idx })
}

fn rect_contains(rect: Rect, x: u16, y: u16) -> bool {
    rect.width > 0
        && rect.height > 0
        && x >= rect.x
        && x < rect.x.saturating_add(rect.width)
        && y >= rect.y
        && y < rect.y.saturating_add(rect.height)
}

fn copy_active_content_excluding(
    active_frame: &FrameData,
    target: &mut FrameData,
    target_x: u16,
    target_width: u16,
    excluded_rects: &[Rect],
) {
    let copy_width = target_width.min(active_frame.width);
    let copy_height = target.height.min(active_frame.height);
    for row in 0..copy_height {
        for col in 0..copy_width {
            let source_idx = (row as usize) * (active_frame.width as usize) + (col as usize);
            let target_col = target_x + col;
            if excluded_rects
                .iter()
                .any(|rect| rect_contains(*rect, target_col, row))
            {
                continue;
            }
            let target_idx = (row as usize) * (target.width as usize) + (target_col as usize);
            if let (Some(source), Some(target_cell)) = (
                active_frame.cells.get(source_idx),
                target.cells.get_mut(target_idx),
            ) {
                *target_cell = source.clone();
            }
        }
    }
}

fn offset_cursor(
    cursor: Option<&CursorState>,
    sidebar_width: u16,
    content_width: u16,
) -> Option<CursorState> {
    let cursor = cursor?;
    if cursor.x >= content_width {
        return None;
    }
    Some(CursorState {
        x: sidebar_width + cursor.x,
        y: cursor.y,
        visible: cursor.visible,
        shape: cursor.shape,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::client::supervisor::{
        AgentSummary, ClientSupervisorModel, NewWorkspaceRoute, ServerId, ServerSummary,
        WorkspaceSummary,
    };
    use crate::protocol::{CellData, CursorState};

    fn cell(symbol: &str) -> CellData {
        CellData {
            symbol: symbol.into(),
            fg: 0,
            bg: 0,
            modifier: 0,
            skip: false,
            hyperlink: None,
        }
    }

    fn frame(width: u16, height: u16, rows: &[&str]) -> FrameData {
        let mut cells = Vec::new();
        for row in 0..height as usize {
            let line = rows.get(row).copied().unwrap_or("");
            for col in 0..width as usize {
                let symbol = line
                    .chars()
                    .nth(col)
                    .map(|ch| ch.to_string())
                    .unwrap_or_else(|| " ".into());
                cells.push(cell(&symbol));
            }
        }
        FrameData {
            cells,
            width,
            height,
            cursor: Some(CursorState {
                x: 1,
                y: 1,
                visible: true,
                shape: 2,
            }),
            hyperlinks: Vec::new(),
            graphics: Vec::new(),
        }
    }

    /// The footer-anchored `anchor_area` the renderer/hit-test derive the composited client
    /// overlays from: spans the host top down to the sidebar footer row. Tests derive their
    /// expected popup coordinates from this SAME rect so render geometry == hit-test geometry.
    fn anchor_area(
        model: &ClientSupervisorModel,
        compositor: &ClientCompositor,
        host_w: u16,
        host_h: u16,
    ) -> Rect {
        compositor.overlay_anchor_area(model, host_w, host_h)
    }

    fn row_text(frame: &FrameData, row: u16) -> String {
        (0..frame.width)
            .map(|col| {
                frame.cells[(row as usize) * (frame.width as usize) + (col as usize)]
                    .symbol
                    .as_str()
            })
            .collect()
    }

    #[test]
    fn compose_frame_draws_server_sidebar_shell_and_offsets_active_content() {
        let mut model = ClientSupervisorModel::new("local");
        let remote_id = model.add_secondary(crate::remote_registry::RemoteDefinitionSnapshot {
            id: "remote-x".into(),
            name: "x".into(),
            target: crate::remote_registry::RemoteTargetSnapshot::Local {
                session: Some("x".into()),
            },
            session: None,
            keybindings: crate::remote_registry::RemoteKeybindingsSnapshot::Local,
            disabled: false,
        });
        model
            .set_summary(
                &ServerId::main(),
                ServerSummary {
                    workspaces: vec![WorkspaceSummary {
                        workspace_id: "main-herdr".into(),
                        label: "herdr".into(),
                        branch: Some("master".into()),
                        focused: true,
                        ..Default::default()
                    }],
                    agents: Vec::new(),
                },
            )
            .unwrap();
        model
            .set_summary(
                &remote_id,
                ServerSummary {
                    workspaces: vec![WorkspaceSummary {
                        workspace_id: "remote-api".into(),
                        label: "api".into(),
                        branch: Some("feature/api".into()),
                        focused: false,
                        ..Default::default()
                    }],
                    agents: vec![AgentSummary {
                        agent_id: "remote-agent".into(),
                        workspace_id: "remote-api".into(),
                        label: "claude".into(),
                        status: "idle".into(),
                        focused: false,
                    }],
                },
            )
            .unwrap();

        let content = frame(8, 3, &["content", "frame"]);
        // item 2 (C3): the host banner adds a row to the spaces list, so render at a taller
        // sidebar to keep the remote card's branch line on screen.
        let composed = ClientCompositor::new(26).compose_frame(
            &model,
            &content,
            60,
            28,
            std::time::Instant::now(),
        );

        assert_eq!(composed.width, 60);
        assert_eq!(composed.height, 28);
        let rows: Vec<_> = (0..composed.height)
            .map(|row| row_text(&composed, row))
            .collect();
        assert!(row_text(&composed, 0).starts_with(" spaces"));
        assert!(row_text(&composed, 0)
            .chars()
            .take(25)
            .collect::<String>()
            .ends_with("all"));
        assert_eq!(composed.cells[25].symbol, "│");
        assert!(rows.iter().any(|row| row.contains("herdr")));
        assert!(rows.iter().any(|row| row.contains("master")));
        // item 2 (C3): bare space label "api" (host "x" now lives in the banner row above).
        assert!(rows.iter().any(|row| row.contains("api")));
        assert!(rows.iter().any(|row| row.contains("feature/api")));
        assert!(rows.iter().any(|row| row.starts_with(" agents")));
        assert!(rows.iter().any(|row| row.contains("claude")));
        let row0_content: String = row_text(&composed, 0).chars().skip(26).collect();
        let row1_content: String = row_text(&composed, 1).chars().skip(26).collect();
        assert!(row0_content.starts_with("content"));
        assert!(row1_content.starts_with("frame"));
        assert_eq!(
            composed.cursor,
            Some(CursorState {
                x: 27,
                y: 1,
                visible: true,
                shape: 2,
            })
        );
    }

    #[test]
    fn compose_frame_uses_main_ui_settings_for_sidebar_fields() {
        let mut model = ClientSupervisorModel::new("local");
        let remote_id = model.add_secondary(crate::remote_registry::RemoteDefinitionSnapshot {
            id: "remote-x".into(),
            name: "x".into(),
            target: crate::remote_registry::RemoteTargetSnapshot::Local {
                session: Some("x".into()),
            },
            session: None,
            keybindings: crate::remote_registry::RemoteKeybindingsSnapshot::Local,
            disabled: false,
        });
        model
            .set_summary(
                &remote_id,
                ServerSummary {
                    workspaces: vec![WorkspaceSummary {
                        workspace_id: "remote-api".into(),
                        label: "api".into(),
                        branch: Some("feature/api".into()),
                        focused: true,
                        ..Default::default()
                    }],
                    agents: Vec::new(),
                },
            )
            .unwrap();
        let mut settings = crate::api::schema::UiSettingsInfo::default();
        // Drop the branch/git row from the token config (the API the PoC's
        // `SidebarSpaceItem::Branch` flag was replaced by), so the branch line disappears.
        settings.sidebar_spaces.rows = vec![vec![
            crate::config::SpaceSidebarToken::StateIcon,
            crate::config::SpaceSidebarToken::Workspace,
        ]];
        model.set_ui_settings(settings);

        let content = frame(8, 3, &["content", "frame"]);
        let composed = ClientCompositor::new(26).compose_frame(
            &model,
            &content,
            60,
            16,
            std::time::Instant::now(),
        );
        let rows: Vec<_> = (0..composed.height)
            .map(|row| row_text(&composed, row))
            .collect();

        // item 2 (C3): the workspace label is now the bare space name (the host name lives in
        // the banner above), and the branch row is dropped by the ui-settings token override.
        assert!(rows.iter().any(|row| row.contains("api")));
        assert!(!rows.iter().any(|row| row.contains("feature/api")));
    }

    #[test]
    fn content_size_reserves_sidebar_width_and_keeps_one_column_minimum() {
        let compositor = ClientCompositor::new(12);

        assert_eq!(compositor.content_size(80, 24), (68, 24));
        assert_eq!(compositor.content_size(8, 24), (1, 24));
    }

    #[test]
    fn compose_frame_reserves_content_column_when_host_is_narrower_than_sidebar() {
        let model = ClientSupervisorModel::new("local");
        let compositor = ClientCompositor::new(12);
        let content = frame(1, 1, &["x"]);

        let composed = compositor.compose_frame(&model, &content, 8, 3, std::time::Instant::now());

        assert_eq!(composed.width, 8);
        assert_eq!(composed.cells[7].symbol, "x");
    }

    #[test]
    fn filter_label_rect_uses_display_width_for_wide_text() {
        let rect = filter_label_rect(Rect::new(0, 0, 6, 1), "전체");

        assert_eq!(rect.x, 1);
        assert_eq!(rect.width, 4);
    }

    #[test]
    fn hit_test_uses_server_sidebar_geometry() {
        let mut model = ClientSupervisorModel::new("local");
        let remote_id = model.add_secondary(crate::remote_registry::RemoteDefinitionSnapshot {
            id: "remote-x".into(),
            name: "x".into(),
            target: crate::remote_registry::RemoteTargetSnapshot::Local {
                session: Some("x".into()),
            },
            session: None,
            keybindings: crate::remote_registry::RemoteKeybindingsSnapshot::Local,
            disabled: false,
        });
        model
            .set_summary(
                &ServerId::main(),
                ServerSummary {
                    workspaces: vec![WorkspaceSummary {
                        workspace_id: "main-herdr".into(),
                        label: "herdr".into(),
                        branch: None,
                        focused: true,
                        ..Default::default()
                    }],
                    agents: Vec::new(),
                },
            )
            .unwrap();
        model
            .set_summary(
                &remote_id,
                ServerSummary {
                    workspaces: vec![WorkspaceSummary {
                        workspace_id: "remote-api".into(),
                        label: "api".into(),
                        branch: None,
                        focused: false,
                        ..Default::default()
                    }],
                    agents: vec![AgentSummary {
                        agent_id: "remote-agent".into(),
                        workspace_id: "remote-api".into(),
                        label: "claude".into(),
                        status: "idle".into(),
                        focused: false,
                    }],
                },
            )
            .unwrap();

        let compositor = ClientCompositor::new(26);
        // Derive row geometry from the same snapshot render uses (render == hit_test): the main
        // card, the divider, item 2's host banner, and the remote card all come from one pass.
        let snapshot =
            ClientSidebarSnapshot::from_model(&model, &compositor, 26, 60, 28, Instant::now());
        let main_card = snapshot
            .app
            .view
            .workspace_card_areas
            .iter()
            .find(|c| c.ws_idx == 0)
            .expect("main card");
        let remote_card = snapshot
            .app
            .view
            .workspace_card_areas
            .iter()
            .find(|c| c.ws_idx == 1)
            .expect("remote card");
        let divider_y = snapshot.app.view.divider_rows[0];
        // #19 (host half): in multi-host mode the Local banner is the FIRST banner (above the local
        // card — it is the host's drag handle); the remote banner is the LAST one (above the remote
        // card). `local_banner_y` keeps the original "a banner row resolves to no workspace" check.
        let local_banner_y = snapshot.app.view.host_banner_areas[0].rect.y;
        let remote_banner_y = snapshot
            .app
            .view
            .host_banner_areas
            .last()
            .expect("remote banner")
            .rect
            .y;

        assert_eq!(
            compositor.hit_test(&model, 23, 0, 60, 28),
            Some(SidebarHitTarget::Filter)
        );
        assert_eq!(
            compositor.hit_test(&model, 1, main_card.rect.y, 60, 28),
            Some(SidebarHitTarget::Workspace {
                server_id: ServerId::main(),
                workspace_id: "main-herdr".into(),
            })
        );
        // item 4: the local→remote divider row resolves to no workspace. item 2 (C3): the host
        // banner row (below the divider, above the remote card) also resolves to no workspace.
        assert!(!matches!(
            compositor.hit_test(&model, 1, divider_y, 60, 28),
            Some(SidebarHitTarget::Workspace { .. })
        ));
        assert!(!matches!(
            compositor.hit_test(&model, 1, local_banner_y, 60, 28),
            Some(SidebarHitTarget::Workspace { .. })
        ));
        // #19 (host half): the Local banner sits ABOVE the local card (it is the host drag handle).
        // The divider and the remote banner are host boundaries between the local block and the
        // remote card.
        assert!(
            local_banner_y < main_card.rect.y,
            "local banner is above the local card"
        );
        assert!(main_card.rect.y < divider_y);
        assert!(divider_y < remote_card.rect.y);
        assert!(
            main_card.rect.y < remote_banner_y && remote_banner_y < remote_card.rect.y,
            "remote banner sits between the local block and the remote card"
        );
        assert_eq!(
            compositor.hit_test(&model, 1, remote_card.rect.y, 60, 28),
            Some(SidebarHitTarget::Workspace {
                server_id: remote_id.clone(),
                workspace_id: "remote-api".into(),
            })
        );
        // The agent row + affordances still resolve to their targets at their geometry.
        let new_rect = snapshot.app.sidebar_new_button_rect();
        assert_eq!(
            compositor.hit_test(&model, new_rect.x, new_rect.y, 60, 28),
            Some(SidebarHitTarget::New)
        );
        let menu_rect = snapshot.app.global_launcher_rect();
        assert_eq!(
            compositor.hit_test(
                &model,
                menu_rect.x + menu_rect.width - 1,
                menu_rect.y,
                60,
                28
            ),
            Some(SidebarHitTarget::Menu)
        );
        assert_eq!(
            compositor.hit_test(&model, 27, main_card.rect.y, 60, 28),
            None
        );
    }

    #[test]
    fn hit_test_ignores_disabled_workspace_rows() {
        let mut model = ClientSupervisorModel::new("local");
        let remote_id = model.add_secondary(crate::remote_registry::RemoteDefinitionSnapshot {
            id: "remote-x".into(),
            name: "x".into(),
            target: crate::remote_registry::RemoteTargetSnapshot::Local {
                session: Some("x".into()),
            },
            session: None,
            keybindings: crate::remote_registry::RemoteKeybindingsSnapshot::Local,
            disabled: false,
        });
        model
            .set_summary(
                &remote_id,
                ServerSummary {
                    workspaces: vec![WorkspaceSummary {
                        workspace_id: "remote-api".into(),
                        label: "api".into(),
                        branch: None,
                        focused: false,
                        ..Default::default()
                    }],
                    agents: Vec::new(),
                },
            )
            .unwrap();
        model
            .set_connection_state(
                &remote_id,
                crate::client::supervisor::ConnectionState::Disconnected,
            )
            .unwrap();

        let compositor = ClientCompositor::new(26);

        // The disconnected remote's row is a placeholder (no workspace_id), so no row in the
        // sidebar resolves to a `Workspace` hit. #19 (host half): the Local/remote banner rows are
        // `HostBanner` hits, not `Workspace` ones, so the invariant holds across the whole column.
        for y in 0..16 {
            assert!(
                !matches!(
                    compositor.hit_test(&model, 1, y, 60, 16),
                    Some(SidebarHitTarget::Workspace { .. })
                ),
                "row {y} unexpectedly hit-tested to a workspace",
            );
        }
    }

    /// #16/#20/#26: a single local server with two workspaces, each owning one agent. ws-1 is
    /// focused (→ the snapshot's active/selected workspace), so it exercises both the sort
    /// alignment and the section-divider/divider-reset geometry without the host-banner rows a
    /// secondary server would add.
    fn single_server_two_ws_model() -> ClientSupervisorModel {
        let mut model = ClientSupervisorModel::new("local");
        model
            .set_summary(
                &ServerId::main(),
                ServerSummary {
                    workspaces: vec![
                        WorkspaceSummary {
                            workspace_id: "ws-1".into(),
                            label: "one".into(),
                            branch: None,
                            focused: true,
                            ..Default::default()
                        },
                        WorkspaceSummary {
                            workspace_id: "ws-2".into(),
                            label: "two".into(),
                            branch: None,
                            focused: false,
                            ..Default::default()
                        },
                    ],
                    agents: vec![
                        AgentSummary {
                            agent_id: "agent-1".into(),
                            workspace_id: "ws-1".into(),
                            label: "claude".into(),
                            status: "idle".into(),
                            focused: false,
                        },
                        AgentSummary {
                            agent_id: "agent-2".into(),
                            workspace_id: "ws-2".into(),
                            label: "codex".into(),
                            status: "idle".into(),
                            focused: false,
                        },
                    ],
                },
            )
            .unwrap();
        model
    }

    // #20: the agents-panel sort toggle resolves to `AgentPanelSortToggle`, and toggling it flips
    // the compositor's client-local sort and zeroes the panel scroll (mirrors the monolithic
    // host's toggle handler).
    #[test]
    fn sort_toggle_hit_test_resolves_and_toggles_sort() {
        use crate::app::state::AgentPanelSort;
        let model = single_server_two_ws_model();
        let mut compositor = ClientCompositor::new(26);
        assert_eq!(compositor.agent_panel_sort(), AgentPanelSort::Spaces);

        let snapshot =
            ClientSidebarSnapshot::from_model(&model, &compositor, 26, 60, 28, Instant::now());
        let rect = agent_panel_toggle_hit_rect(&snapshot.app);
        assert!(rect.width > 0, "the sort toggle should be drawn");
        assert_eq!(
            compositor.hit_test(&model, rect.x, rect.y, 60, 28),
            Some(SidebarHitTarget::AgentPanelSortToggle)
        );

        compositor.agent_panel_scroll = 3;
        compositor.toggle_agent_panel_sort();
        assert_eq!(compositor.agent_panel_sort(), AgentPanelSort::Priority);
        assert_eq!(compositor.agent_panel_scroll, 0);
    }

    // #20: under the priority sort the shared `agent_panel_entries` reorders entries by attention
    // priority; the flat `agent_routes` must follow the SAME order so an agent-row hit resolves to
    // the reordered agent (render == hit_test under either sort).
    #[test]
    fn priority_sort_keeps_agent_routes_aligned_with_entries() {
        let mut model = ClientSupervisorModel::new("local");
        model
            .set_summary(
                &ServerId::main(),
                ServerSummary {
                    workspaces: vec![
                        WorkspaceSummary {
                            workspace_id: "ws-1".into(),
                            label: "one".into(),
                            branch: None,
                            focused: true,
                            ..Default::default()
                        },
                        WorkspaceSummary {
                            workspace_id: "ws-2".into(),
                            label: "two".into(),
                            branch: None,
                            focused: false,
                            ..Default::default()
                        },
                    ],
                    agents: vec![
                        AgentSummary {
                            agent_id: "agent-1".into(),
                            workspace_id: "ws-1".into(),
                            label: "claude".into(),
                            status: "idle".into(),
                            focused: false,
                        },
                        AgentSummary {
                            agent_id: "agent-2".into(),
                            workspace_id: "ws-2".into(),
                            label: "codex".into(),
                            status: "blocked".into(),
                            focused: false,
                        },
                    ],
                },
            )
            .unwrap();

        let mut compositor = ClientCompositor::new(26);
        // Spaces sort: workspace order (agent-1 first).
        let spaces =
            ClientSidebarSnapshot::from_model(&model, &compositor, 26, 60, 28, Instant::now());
        assert_eq!(spaces.agent_routes.len(), 2);
        assert_eq!(spaces.agent_routes[0].agent_id, "agent-1");

        // Priority sort: the blocked agent-2 outranks the idle agent-1 and the routes follow.
        compositor.toggle_agent_panel_sort();
        let priority =
            ClientSidebarSnapshot::from_model(&model, &compositor, 26, 60, 28, Instant::now());
        assert_eq!(priority.agent_routes.len(), 2);
        assert_eq!(priority.agent_routes[0].agent_id, "agent-2");
    }

    // #20 regression: in mixed-remote mode each connected server independently reports its own
    // focused workspace, so the local `main-herdr` row (focused, owning the focused agent the user
    // is attached to) AND the trailing remote `remote-api` row both carry `focused == true`. A
    // plain last-wins selected the remote; the agent-focused local row must win over the merely
    // workspace-focused remote row so active/selected stay on the local workspace.
    #[test]
    fn agent_focused_row_wins_active_selection_over_workspace_focused_remote() {
        let mut model = ClientSupervisorModel::new("local");
        let remote_id = model.add_secondary(crate::remote_registry::RemoteDefinitionSnapshot {
            id: "remote-x".into(),
            name: "x".into(),
            target: crate::remote_registry::RemoteTargetSnapshot::Local {
                session: Some("x".into()),
            },
            session: None,
            keybindings: crate::remote_registry::RemoteKeybindingsSnapshot::Local,
            disabled: false,
        });
        // Local server (row 0): workspace focused AND its agent is the focused one.
        model
            .set_summary(
                &ServerId::main(),
                ServerSummary {
                    workspaces: vec![WorkspaceSummary {
                        workspace_id: "main-herdr".into(),
                        label: "herdr".into(),
                        branch: None,
                        focused: true,
                        ..Default::default()
                    }],
                    agents: vec![AgentSummary {
                        agent_id: "main-agent".into(),
                        workspace_id: "main-herdr".into(),
                        label: "claude".into(),
                        status: "working".into(),
                        focused: true,
                    }],
                },
            )
            .unwrap();
        // Remote server (row 1): workspace also reports focused, but no focused agent.
        model
            .set_summary(
                &remote_id,
                ServerSummary {
                    workspaces: vec![WorkspaceSummary {
                        workspace_id: "remote-api".into(),
                        label: "api".into(),
                        branch: None,
                        focused: true,
                        ..Default::default()
                    }],
                    agents: vec![AgentSummary {
                        agent_id: "remote-agent".into(),
                        workspace_id: "remote-api".into(),
                        label: "codex".into(),
                        status: "idle".into(),
                        focused: false,
                    }],
                },
            )
            .unwrap();

        let compositor = ClientCompositor::new(26);
        let snapshot =
            ClientSidebarSnapshot::from_model(&model, &compositor, 26, 60, 28, Instant::now());
        // The local workspace (row 0, the agent-focused one) wins the active selection.
        assert_eq!(snapshot.app.active, Some(0));
        assert_eq!(snapshot.app.selected, 0);
        // The default spaces sort keeps the local agent first in the flat routes.
        assert_eq!(snapshot.agent_routes.len(), 2);
        assert_eq!(snapshot.agent_routes[0].agent_id, "main-agent");
    }

    // #16: dragging the spaces↔agents section divider sets a client-local split override that
    // grows the workspace section as the divider moves down.
    #[test]
    fn section_divider_drag_sets_local_split() {
        use crossterm::event::{KeyModifiers, MouseButton, MouseEventKind};
        let model = single_server_two_ws_model();
        let mut compositor = ClientCompositor::new(26);
        assert!(compositor.section_split.is_none());

        let snapshot =
            ClientSidebarSnapshot::from_model(&model, &compositor, 26, 60, 28, Instant::now());
        let divider = crate::ui::sidebar_section_divider_rect(
            snapshot.app.view.sidebar_rect,
            snapshot.app.sidebar_section_split,
        );
        assert!(divider.width > 0, "the section divider should be drawn");

        let ev = |kind, row| crossterm::event::MouseEvent {
            kind,
            column: divider.x,
            row,
            modifiers: KeyModifiers::empty(),
        };

        assert_eq!(
            compositor.handle_sidebar_section_divider_mouse(
                &model,
                &ev(MouseEventKind::Down(MouseButton::Left), divider.y),
                60,
                28,
            ),
            Some(true)
        );
        let after_press = compositor.section_split.expect("press sets a split");
        assert_eq!(
            compositor.handle_sidebar_section_divider_mouse(
                &model,
                &ev(MouseEventKind::Drag(MouseButton::Left), divider.y + 2),
                60,
                28,
            ),
            Some(true)
        );
        let after_drag = compositor.section_split.expect("drag keeps a split");
        assert!(
            after_drag > after_press,
            "dragging down increases the workspace ratio ({after_press} -> {after_drag})"
        );

        // Release ends the drag; a later drag with no active drag is ignored.
        assert_eq!(
            compositor.handle_sidebar_section_divider_mouse(
                &model,
                &ev(MouseEventKind::Up(MouseButton::Left), divider.y + 2),
                60,
                28,
            ),
            Some(true)
        );
        assert_eq!(
            compositor.handle_sidebar_section_divider_mouse(
                &model,
                &ev(MouseEventKind::Drag(MouseButton::Left), divider.y + 4),
                60,
                28,
            ),
            None
        );
    }

    // #26: a second divider press within the double-click window resets the width to the default
    // and reports a content resize; the first press only starts a drag (redraw).
    #[test]
    fn divider_double_click_resets_width_to_default() {
        use crossterm::event::{KeyModifiers, MouseButton, MouseEventKind};
        let model = single_server_two_ws_model();
        let mut compositor = ClientCompositor::new(26);
        let settings = model.ui_settings().clone();

        // The divider sits at the rightmost sidebar column (`sidebar_width - 1`); a press only
        // registers when it lands on that exact column, which moves as the width changes.
        let press = |column: u16| crossterm::event::MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column,
            row: 5,
            modifiers: KeyModifiers::empty(),
        };

        assert_eq!(
            compositor.handle_sidebar_resize_mouse(&press(25), 80, 24, &settings),
            Some(SidebarResizeOutcome::Redraw)
        );
        assert_eq!(compositor.sidebar_width(), 26);

        // Drag narrower → the divider (and thus the press column) moves to col 18.
        let drag = crossterm::event::MouseEvent {
            kind: MouseEventKind::Drag(MouseButton::Left),
            column: 18,
            row: 5,
            modifiers: KeyModifiers::empty(),
        };
        assert!(matches!(
            compositor.handle_sidebar_resize_mouse(&drag, 80, 24, &settings),
            Some(SidebarResizeOutcome::Resized(..))
        ));
        assert_eq!(compositor.sidebar_width(), 19);

        // Two quick presses on the NEW divider column (18) → reset to default, reported as a
        // resize. The drag cleared the press timestamp, so the first of these starts a fresh
        // double-click window (redraw) and the second triggers the reset.
        assert_eq!(
            compositor.handle_sidebar_resize_mouse(&press(18), 80, 24, &settings),
            Some(SidebarResizeOutcome::Redraw)
        );
        assert!(matches!(
            compositor.handle_sidebar_resize_mouse(&press(18), 80, 24, &settings),
            Some(SidebarResizeOutcome::Resized(..))
        ));
        assert_eq!(
            compositor.sidebar_width(),
            settings
                .sidebar_default_width
                .clamp(settings.sidebar_min_width, settings.sidebar_max_width)
        );
    }

    // #19: pressing a workspace card and dragging past the threshold to another card's row, then
    // releasing, commits a `workspace.reorder` with the drop position within the owning server.
    #[test]
    fn workspace_drag_reorder_commits_insert_index() {
        use crossterm::event::{KeyModifiers, MouseButton, MouseEventKind};
        let model = single_server_two_ws_model();
        let mut compositor = ClientCompositor::new(26);
        let host = (60u16, 28u16);

        let snapshot = ClientSidebarSnapshot::from_model(
            &model,
            &compositor,
            26,
            host.0,
            host.1,
            Instant::now(),
        );
        let card = |ws_idx: usize| {
            snapshot
                .app
                .view
                .workspace_card_areas
                .iter()
                .find(|c| c.ws_idx == ws_idx)
                .expect("card")
                .rect
        };
        let ws1_row = card(0).y;
        let ws2_row = card(1).y;

        // Arm the press on ws-2 (the second workspace), like the Down hit arm does.
        compositor.begin_workspace_press(ServerId::main(), "ws-2".into(), 1, ws2_row);

        // Drag up onto ws-1's row.
        let drag = crossterm::event::MouseEvent {
            kind: MouseEventKind::Drag(MouseButton::Left),
            column: 1,
            row: ws1_row,
            modifiers: KeyModifiers::empty(),
        };
        assert_eq!(
            compositor.handle_workspace_reorder_mouse(&model, &drag, host.0, host.1),
            WorkspaceReorderOutcome::Dragging
        );

        // Release on ws-1's row → reorder ws-2 to index 0 within the (single) server's list.
        let up = crossterm::event::MouseEvent {
            kind: MouseEventKind::Up(MouseButton::Left),
            column: 1,
            row: ws1_row,
            modifiers: KeyModifiers::empty(),
        };
        assert_eq!(
            compositor.handle_workspace_reorder_mouse(&model, &up, host.0, host.1),
            WorkspaceReorderOutcome::Commit {
                server_id: ServerId::main(),
                workspace_id: "ws-2".into(),
                insert_index: 0,
            }
        );
    }

    // #19: while dragging a space the snapshot must carry a live drop indicator (`app.drag`) so the
    // shared renderer draws the drop line every frame — the client path used to show nothing until
    // release. The dragged card's global index and the global insert slot must both be correct.
    #[test]
    fn workspace_drag_preview_populates_app_drag() {
        use crossterm::event::{KeyModifiers, MouseButton, MouseEventKind};
        let mut model = ClientSupervisorModel::new("local");
        model
            .set_summary(
                &ServerId::main(),
                ServerSummary {
                    workspaces: vec![
                        WorkspaceSummary {
                            workspace_id: "ws-1".into(),
                            label: "one".into(),
                            branch: None,
                            focused: false,
                            ..Default::default()
                        },
                        WorkspaceSummary {
                            workspace_id: "ws-2".into(),
                            label: "two".into(),
                            branch: None,
                            focused: true,
                            ..Default::default()
                        },
                        WorkspaceSummary {
                            workspace_id: "ws-3".into(),
                            label: "three".into(),
                            branch: None,
                            focused: false,
                            ..Default::default()
                        },
                    ],
                    agents: Vec::new(),
                },
            )
            .unwrap();

        let mut compositor = ClientCompositor::new(26);
        let host = (60u16, 28u16);
        let snapshot = ClientSidebarSnapshot::from_model(
            &model,
            &compositor,
            26,
            host.0,
            host.1,
            Instant::now(),
        );
        // No drag in progress yet: the renderer gets no drop indicator.
        assert!(snapshot.app.drag.is_none());

        let card = |ws_idx: usize| {
            snapshot
                .app
                .view
                .workspace_card_areas
                .iter()
                .find(|c| c.ws_idx == ws_idx)
                .expect("card")
                .rect
        };
        let ws1_row = card(0).y;
        let ws3_rect = card(2);

        // Press on ws-2 (global index 1), then drag down past ws-3's midpoint.
        compositor.begin_workspace_press(ServerId::main(), "ws-2".into(), 1, ws1_row);
        let drag = crossterm::event::MouseEvent {
            kind: MouseEventKind::Drag(MouseButton::Left),
            column: 1,
            row: ws3_rect.y + ws3_rect.height,
            modifiers: KeyModifiers::empty(),
        };
        assert_eq!(
            compositor.handle_workspace_reorder_mouse(&model, &drag, host.0, host.1),
            WorkspaceReorderOutcome::Dragging
        );

        let dragging = ClientSidebarSnapshot::from_model(
            &model,
            &compositor,
            26,
            host.0,
            host.1,
            Instant::now(),
        );
        match dragging.app.drag.as_ref().map(|d| &d.target) {
            Some(crate::app::state::DragTarget::WorkspaceReorder {
                source_ws_idx,
                insert_idx,
            }) => {
                // ws-2 is global index 1; dropping past ws-3 (the third, single-server card) lands
                // at global insert position 3 (after the whole block).
                assert_eq!(*source_ws_idx, 1);
                assert_eq!(*insert_idx, Some(3));
            }
            _ => panic!("expected a WorkspaceReorder drag preview"),
        }
    }

    // #19: the drop indicator must stay inside the SOURCE server's contiguous row block. Dragging a
    // local space far below the remote's rows must still resolve to an insert index at the end of
    // the local block (≤ local card count) and never point into the remote block.
    #[test]
    fn workspace_drag_preview_clamps_to_source_server_block() {
        use crossterm::event::{KeyModifiers, MouseButton, MouseEventKind};
        let mut model = ClientSupervisorModel::new("local");
        let remote_id = model.add_secondary(crate::remote_registry::RemoteDefinitionSnapshot {
            id: "remote-x".into(),
            name: "x".into(),
            target: crate::remote_registry::RemoteTargetSnapshot::Local {
                session: Some("x".into()),
            },
            session: None,
            keybindings: crate::remote_registry::RemoteKeybindingsSnapshot::Local,
            disabled: false,
        });
        model
            .set_summary(
                &ServerId::main(),
                ServerSummary {
                    workspaces: vec![
                        WorkspaceSummary {
                            workspace_id: "local-1".into(),
                            label: "l1".into(),
                            branch: None,
                            focused: true,
                            ..Default::default()
                        },
                        WorkspaceSummary {
                            workspace_id: "local-2".into(),
                            label: "l2".into(),
                            branch: None,
                            focused: false,
                            ..Default::default()
                        },
                    ],
                    agents: Vec::new(),
                },
            )
            .unwrap();
        model
            .set_summary(
                &remote_id,
                ServerSummary {
                    workspaces: vec![
                        WorkspaceSummary {
                            workspace_id: "remote-1".into(),
                            label: "r1".into(),
                            branch: None,
                            focused: false,
                            ..Default::default()
                        },
                        WorkspaceSummary {
                            workspace_id: "remote-2".into(),
                            label: "r2".into(),
                            branch: None,
                            focused: false,
                            ..Default::default()
                        },
                    ],
                    agents: Vec::new(),
                },
            )
            .unwrap();

        let mut compositor = ClientCompositor::new(26);
        let host = (60u16, 28u16);
        let snapshot = ClientSidebarSnapshot::from_model(
            &model,
            &compositor,
            26,
            host.0,
            host.1,
            Instant::now(),
        );
        // Global indices of the LOCAL cards (the source block): base + count == block end.
        let local_indices: Vec<usize> = snapshot
            .workspace_routes
            .iter()
            .enumerate()
            .filter(|(_, route)| {
                route.server_id == ServerId::main() && route.workspace_id.is_some()
            })
            .map(|(idx, _)| idx)
            .collect();
        let local_base = *local_indices.first().expect("local cards present");
        let local_block_end = local_base + local_indices.len();
        let first_local_row = snapshot
            .app
            .view
            .workspace_card_areas
            .iter()
            .find(|c| c.ws_idx == local_base)
            .expect("first local card")
            .rect
            .y;

        // Press the first local space, then drag all the way to the bottom (past every remote card).
        compositor.begin_workspace_press(ServerId::main(), "local-1".into(), 1, first_local_row);
        let drag = crossterm::event::MouseEvent {
            kind: MouseEventKind::Drag(MouseButton::Left),
            column: 1,
            row: host.1 - 1,
            modifiers: KeyModifiers::empty(),
        };
        assert_eq!(
            compositor.handle_workspace_reorder_mouse(&model, &drag, host.0, host.1),
            WorkspaceReorderOutcome::Dragging
        );

        let dragging = ClientSidebarSnapshot::from_model(
            &model,
            &compositor,
            26,
            host.0,
            host.1,
            Instant::now(),
        );
        match dragging.app.drag.as_ref().map(|d| &d.target) {
            Some(crate::app::state::DragTarget::WorkspaceReorder {
                source_ws_idx,
                insert_idx,
            }) => {
                assert_eq!(*source_ws_idx, local_base);
                // Clamped to the local block: the insert slot never points into the remote block.
                let insert = insert_idx.expect("preview carries an insert slot");
                assert!(
                    insert <= local_block_end,
                    "insert {insert} escaped local block end {local_block_end}"
                );
            }
            _ => panic!("expected a WorkspaceReorder drag preview"),
        }
    }

    // #19: a sub-threshold press (no drag) must leave `app.drag` empty so no phantom drop line shows.
    #[test]
    fn workspace_sub_threshold_press_has_no_drag_preview() {
        use crossterm::event::{KeyModifiers, MouseButton, MouseEventKind};
        let model = single_server_two_ws_model();
        let mut compositor = ClientCompositor::new(26);
        let host = (60u16, 28u16);
        let snapshot = ClientSidebarSnapshot::from_model(
            &model,
            &compositor,
            26,
            host.0,
            host.1,
            Instant::now(),
        );
        let ws1_row = snapshot
            .app
            .view
            .workspace_card_areas
            .iter()
            .find(|c| c.ws_idx == 0)
            .expect("first card")
            .rect
            .y;
        // Press, then a zero-movement "drag" stays under the threshold: no drag begins.
        compositor.begin_workspace_press(ServerId::main(), "ws-1".into(), 1, ws1_row);
        let drag = crossterm::event::MouseEvent {
            kind: MouseEventKind::Drag(MouseButton::Left),
            column: 1,
            row: ws1_row,
            modifiers: KeyModifiers::empty(),
        };
        assert_eq!(
            compositor.handle_workspace_reorder_mouse(&model, &drag, host.0, host.1),
            WorkspaceReorderOutcome::Ignored
        );

        let after = ClientSidebarSnapshot::from_model(
            &model,
            &compositor,
            26,
            host.0,
            host.1,
            Instant::now(),
        );
        assert!(after.app.drag.is_none());
    }

    // #19 (host half): a 3-host model — Local + two connected remotes — used by the host
    // drag-reorder tests. Mirrors `mixed_supervisor_model` but with a second remote so reorder
    // has somewhere to move.
    fn three_host_model() -> (ClientSupervisorModel, ServerId, ServerId) {
        let mut model = ClientSupervisorModel::new("local");
        let remote_x = model.add_secondary(crate::remote_registry::RemoteDefinitionSnapshot {
            id: "remote-x".into(),
            name: "x".into(),
            target: crate::remote_registry::RemoteTargetSnapshot::Local {
                session: Some("x".into()),
            },
            session: None,
            keybindings: crate::remote_registry::RemoteKeybindingsSnapshot::Local,
            disabled: false,
        });
        let remote_y = model.add_secondary(crate::remote_registry::RemoteDefinitionSnapshot {
            id: "remote-y".into(),
            name: "y".into(),
            target: crate::remote_registry::RemoteTargetSnapshot::Local {
                session: Some("y".into()),
            },
            session: None,
            keybindings: crate::remote_registry::RemoteKeybindingsSnapshot::Local,
            disabled: false,
        });
        for (id, ws, label, focused) in [
            (ServerId::main(), "main-herdr", "herdr", true),
            (remote_x.clone(), "x-api", "api", false),
            (remote_y.clone(), "y-api", "api", false),
        ] {
            model
                .set_summary(
                    &id,
                    ServerSummary {
                        workspaces: vec![WorkspaceSummary {
                            workspace_id: ws.into(),
                            label: label.into(),
                            branch: None,
                            focused,
                            worktree_key: None,
                            worktree_is_linked: false,
                        }],
                        agents: Vec::new(),
                    },
                )
                .unwrap();
        }
        (model, remote_x, remote_y)
    }

    // #19 (host half): multi-host mode emits a Local banner (the draggable host handle); the
    // single-local case must stay banner-free. Asserts both directions explicitly.
    #[test]
    fn local_host_banner_only_in_multi_host_mode() {
        // Lone local: no banner.
        let local_only = ClientSupervisorModel::new("local");
        assert!(local_only.host_banner_specs().is_empty());
        assert!(local_only.host_banner_server_ids().is_empty());

        // Local + 2 remotes: a banner per host, in visible order, with Local first.
        let (model, remote_x, remote_y) = three_host_model();
        let ids = model.host_banner_server_ids();
        assert_eq!(ids, vec![ServerId::main(), remote_x, remote_y]);
        let specs = model.host_banner_specs();
        assert_eq!(specs.len(), 3);
        assert_eq!(specs[0].1.display_name, "local");
    }

    // #19 (host half): pressing a host banner then dragging past the threshold populates
    // `app.drag` with a `HostReorder` whose `insert_idx` is a HOST boundary (a banner row),
    // never inside a space block.
    #[test]
    fn host_drag_preview_populates_app_drag_at_host_boundary() {
        use crossterm::event::{KeyModifiers, MouseButton, MouseEventKind};
        let (model, _x, _y) = three_host_model();
        let mut compositor = ClientCompositor::new(26);
        let host = (60u16, 28u16);

        // Banner rects in render order: [local, x, y].
        let snapshot = ClientSidebarSnapshot::from_model(
            &model,
            &compositor,
            26,
            host.0,
            host.1,
            Instant::now(),
        );
        let banner_y = |idx: usize| snapshot.app.view.host_banner_areas[idx].rect.y;
        let local_banner_y = banner_y(0);
        let y_banner_y = banner_y(2);
        // The set of every host boundary row (banner tops, and just below the last banner).
        let banner_rects: Vec<Rect> = snapshot
            .app
            .view
            .host_banner_areas
            .iter()
            .map(|b| b.rect)
            .collect();

        // Press the LOCAL banner (host idx 0), then drag down onto the third host's banner.
        compositor.begin_host_press(ServerId::main(), 1, local_banner_y);
        let drag = crossterm::event::MouseEvent {
            kind: MouseEventKind::Drag(MouseButton::Left),
            column: 1,
            row: y_banner_y,
            modifiers: KeyModifiers::empty(),
        };
        assert_eq!(
            compositor.handle_host_reorder_mouse(&model, &drag, host.0, host.1),
            HostReorderOutcome::Dragging
        );

        let preview = ClientSidebarSnapshot::from_model(
            &model,
            &compositor,
            26,
            host.0,
            host.1,
            Instant::now(),
        );
        let (source_host_idx, insert_idx) = match preview.app.drag.as_ref().map(|d| &d.target) {
            Some(crate::app::state::DragTarget::HostReorder {
                source_host_idx,
                insert_idx,
            }) => (*source_host_idx, *insert_idx),
            _ => panic!("expected HostReorder drag in the preview snapshot"),
        };
        assert_eq!(source_host_idx, 0, "dragged host is Local (idx 0)");
        let insert_idx = insert_idx.expect("preview carries a live insert slot");
        // The insert slot must equal the count of banner midpoints above the drop row — i.e. a
        // host boundary — and never resolve into a space row.
        assert_eq!(insert_idx, server_insert_index(&banner_rects, y_banner_y));
        // A host insert slot is `0..=host_count`; it can never index a space card.
        assert!(insert_idx <= banner_rects.len());
    }

    // #19 (host half): releasing a host drag commits a client-local `reorder_server` — `servers`
    // order changes, `workspace_rows()` follows, and `active_server_id` is preserved.
    #[test]
    fn host_drag_commit_reorders_servers_client_local() {
        use crossterm::event::{KeyModifiers, MouseButton, MouseEventKind};
        let (mut model, remote_x, remote_y) = three_host_model();
        // Focus the second remote so we can prove the active server survives the move.
        model.focus_workspace_route(&remote_y, "y-api");
        assert_eq!(model.active_server_id(), &remote_y);

        let mut compositor = ClientCompositor::new(26);
        let host = (60u16, 28u16);
        let snapshot = ClientSidebarSnapshot::from_model(
            &model,
            &compositor,
            26,
            host.0,
            host.1,
            Instant::now(),
        );
        let local_banner_y = snapshot.app.view.host_banner_areas[0].rect.y;
        let y_banner_rect = snapshot.app.view.host_banner_areas[2].rect;

        // Drag Local down past the LAST host's midpoint → Local moves to the end.
        compositor.begin_host_press(ServerId::main(), 1, local_banner_y);
        let drag = crossterm::event::MouseEvent {
            kind: MouseEventKind::Drag(MouseButton::Left),
            column: 1,
            row: y_banner_rect.y + y_banner_rect.height,
            modifiers: KeyModifiers::empty(),
        };
        assert_eq!(
            compositor.handle_host_reorder_mouse(&model, &drag, host.0, host.1),
            HostReorderOutcome::Dragging
        );
        let up = crossterm::event::MouseEvent {
            kind: MouseEventKind::Up(MouseButton::Left),
            column: 1,
            row: y_banner_rect.y + y_banner_rect.height,
            modifiers: KeyModifiers::empty(),
        };
        let outcome = compositor.handle_host_reorder_mouse(&model, &up, host.0, host.1);
        let (source_server_id, insert_index) = match outcome {
            HostReorderOutcome::Commit {
                source_server_id,
                insert_index,
            } => (source_server_id, insert_index),
            other => panic!("expected Commit, got {other:?}"),
        };
        assert_eq!(source_server_id, ServerId::main());

        // Order before: [main, x, y]. Apply the client-local reorder.
        let before: Vec<ServerId> = model
            .workspace_rows()
            .iter()
            .map(|r| r.server_id.clone())
            .collect();
        assert_eq!(
            before,
            vec![ServerId::main(), remote_x.clone(), remote_y.clone()]
        );
        assert!(model.reorder_server(&source_server_id, insert_index));
        // workspace_rows() now reflects the new host order with Local last.
        let after: Vec<ServerId> = model
            .workspace_rows()
            .iter()
            .map(|r| r.server_id.clone())
            .collect();
        assert_eq!(after, vec![remote_x, remote_y.clone(), ServerId::main()]);
        // The active server is unchanged by the host move.
        assert_eq!(model.active_server_id(), &remote_y);
    }

    // #19 (host half): the host drop indicator only ever lands on a host boundary (a banner row
    // or just below the last banner) — never inside a space block — for every drop row.
    #[test]
    fn host_drop_indicator_never_inside_a_space_block() {
        let (model, _x, _y) = three_host_model();
        let compositor = ClientCompositor::new(26);
        let host = (60u16, 28u16);
        let snapshot = ClientSidebarSnapshot::from_model(
            &model,
            &compositor,
            26,
            host.0,
            host.1,
            Instant::now(),
        );
        let banner_rects: Vec<Rect> = snapshot
            .app
            .view
            .host_banner_areas
            .iter()
            .map(|b| b.rect)
            .collect();
        let card_rows: std::collections::HashSet<u16> = snapshot
            .app
            .view
            .workspace_card_areas
            .iter()
            .flat_map(|c| c.rect.y..c.rect.y + c.rect.height)
            .collect();

        // Sweep every drop row over the whole sidebar height and confirm the resolved host
        // drop-indicator row is always a banner-derived boundary, never a space card row.
        let area = snapshot.app.view.sidebar_rect;
        for drop_row in 0..host.1 {
            let insert_idx = server_insert_index(&banner_rects, drop_row);
            if let Some(y) = crate::ui::host_drop_indicator_row(
                &snapshot.app.view.host_banner_areas,
                area,
                insert_idx,
            ) {
                assert!(
                    !card_rows.contains(&y),
                    "host drop indicator at row {y} fell inside a space block",
                );
            }
        }
    }

    // #19 (host half): a press on a SPACE arms a workspace drag (not a host drag); a press on a
    // BANNER hit-tests to `HostBanner` (the host-drag arming target).
    #[test]
    fn space_press_is_workspace_and_banner_press_is_host() {
        let (model, remote_x, _y) = three_host_model();
        let compositor = ClientCompositor::new(26);
        let host = (60u16, 28u16);
        let snapshot = ClientSidebarSnapshot::from_model(
            &model,
            &compositor,
            26,
            host.0,
            host.1,
            Instant::now(),
        );

        // A banner row hit-tests to HostBanner for the right host (banner idx 1 == remote_x).
        let x_banner_y = snapshot.app.view.host_banner_areas[1].rect.y;
        assert_eq!(
            compositor.hit_test(&model, 1, x_banner_y, host.0, host.1),
            Some(SidebarHitTarget::HostBanner {
                server_id: remote_x,
            })
        );

        // A workspace card row hit-tests to a Workspace, never a HostBanner.
        let card_y = snapshot.app.view.workspace_card_areas[0].rect.y;
        assert!(matches!(
            compositor.hit_test(&model, 1, card_y, host.0, host.1),
            Some(SidebarHitTarget::Workspace { .. })
        ));
    }

    #[test]
    fn workspace_press_without_drag_is_not_a_reorder() {
        use crossterm::event::{KeyModifiers, MouseButton, MouseEventKind};
        let model = single_server_two_ws_model();
        let mut compositor = ClientCompositor::new(26);
        let host = (60u16, 28u16);

        compositor.begin_workspace_press(ServerId::main(), "ws-2".into(), 1, 5);
        let up = crossterm::event::MouseEvent {
            kind: MouseEventKind::Up(MouseButton::Left),
            column: 1,
            row: 5,
            modifiers: KeyModifiers::empty(),
        };
        assert_eq!(
            compositor.handle_workspace_reorder_mouse(&model, &up, host.0, host.1),
            WorkspaceReorderOutcome::Ignored
        );
    }

    // #21: pressing the workspace scrollbar thumb and dragging it down scrolls the list; release
    // ends the drag and a later drag is no longer owned.
    #[test]
    fn workspace_scrollbar_thumb_drag_scrolls_list() {
        use crossterm::event::{KeyModifiers, MouseButton, MouseEventKind};
        let mut model = ClientSupervisorModel::new("local");
        let workspaces: Vec<_> = (0..20)
            .map(|i| WorkspaceSummary {
                workspace_id: format!("ws-{i}"),
                label: format!("w{i}"),
                branch: None,
                focused: i == 0,
                worktree_key: None,
                worktree_is_linked: false,
            })
            .collect();
        model
            .set_summary(
                &ServerId::main(),
                ServerSummary {
                    workspaces,
                    agents: Vec::new(),
                },
            )
            .unwrap();
        let mut compositor = ClientCompositor::new(26);
        let host = (60u16, 16u16);

        let snapshot = ClientSidebarSnapshot::from_model(
            &model,
            &compositor,
            26,
            host.0,
            host.1,
            Instant::now(),
        );
        let ws_area = crate::ui::workspace_list_rect(
            snapshot.app.view.sidebar_rect,
            snapshot.app.sidebar_section_split,
        );
        let track = crate::ui::workspace_list_scrollbar_rect(&snapshot.app, ws_area)
            .expect("a long list shows the scrollbar");
        assert!(track.height >= 2);

        let ev = |kind, row| crossterm::event::MouseEvent {
            kind,
            column: track.x,
            row,
            modifiers: KeyModifiers::empty(),
        };

        // Press the thumb (top of track at scroll 0) → starts a drag, no movement yet.
        assert_eq!(
            compositor.handle_sidebar_scrollbar_mouse(
                &model,
                &ev(MouseEventKind::Down(MouseButton::Left), track.y),
                host.0,
                host.1,
            ),
            Some(false)
        );
        assert_eq!(compositor.workspace_scroll, 0);

        // Drag to the bottom of the track → scrolls down.
        assert_eq!(
            compositor.handle_sidebar_scrollbar_mouse(
                &model,
                &ev(
                    MouseEventKind::Drag(MouseButton::Left),
                    track.y + track.height - 1,
                ),
                host.0,
                host.1,
            ),
            Some(true)
        );
        assert!(compositor.workspace_scroll > 0);

        // Release ends the drag; a later drag is no longer owned by the scrollbar.
        assert_eq!(
            compositor.handle_sidebar_scrollbar_mouse(
                &model,
                &ev(
                    MouseEventKind::Up(MouseButton::Left),
                    track.y + track.height - 1,
                ),
                host.0,
                host.1,
            ),
            Some(false)
        );
        assert_eq!(
            compositor.handle_sidebar_scrollbar_mouse(
                &model,
                &ev(
                    MouseEventKind::Drag(MouseButton::Left),
                    track.y + track.height - 1,
                ),
                host.0,
                host.1,
            ),
            None
        );
    }

    // item 4: a [Main, Secondary] model with workspaces on both sides.
    fn mixed_supervisor_model() -> (ClientSupervisorModel, ServerId) {
        let mut model = ClientSupervisorModel::new("local");
        let remote_id = model.add_secondary(crate::remote_registry::RemoteDefinitionSnapshot {
            id: "remote-x".into(),
            name: "x".into(),
            target: crate::remote_registry::RemoteTargetSnapshot::Local {
                session: Some("x".into()),
            },
            session: None,
            keybindings: crate::remote_registry::RemoteKeybindingsSnapshot::Local,
            disabled: false,
        });
        model
            .set_summary(
                &ServerId::main(),
                ServerSummary {
                    workspaces: vec![WorkspaceSummary {
                        workspace_id: "main-herdr".into(),
                        label: "herdr".into(),
                        branch: None,
                        focused: true,
                        ..Default::default()
                    }],
                    agents: Vec::new(),
                },
            )
            .unwrap();
        model
            .set_summary(
                &remote_id,
                ServerSummary {
                    workspaces: vec![WorkspaceSummary {
                        workspace_id: "remote-api".into(),
                        label: "api".into(),
                        branch: None,
                        focused: false,
                        ..Default::default()
                    }],
                    agents: Vec::new(),
                },
            )
            .unwrap();
        (model, remote_id)
    }

    #[test]
    fn from_model_aligns_client_workspace_remote_with_workspaces() {
        let (model, _remote_id) = mixed_supervisor_model();
        let compositor = ClientCompositor::new(26);
        let snapshot =
            ClientSidebarSnapshot::from_model(&model, &compositor, 26, 60, 16, Instant::now());

        // Index-aligned with app.workspaces, and matches each row's is_remote.
        assert_eq!(
            snapshot.app.client_workspace_remote.len(),
            snapshot.app.workspaces.len()
        );
        let rows = model.workspace_rows();
        let expected: Vec<bool> = rows.iter().map(|row| row.is_remote).collect();
        assert_eq!(snapshot.app.client_workspace_remote, expected);
        // [Main, Secondary] => exactly [false, true].
        assert_eq!(snapshot.app.client_workspace_remote, vec![false, true]);
    }

    #[test]
    fn from_model_populates_divider_rows_for_mixed_model() {
        let (model, _remote_id) = mixed_supervisor_model();
        let compositor = ClientCompositor::new(26);
        let snapshot =
            ClientSidebarSnapshot::from_model(&model, &compositor, 26, 60, 16, Instant::now());
        // A mixed model yields exactly one divider row. #19 (host half): in multi-host mode
        // BOTH hosts emit a banner (Local + the one visible Secondary), so two host-banner
        // areas come out of the same compute pass.
        assert_eq!(snapshot.app.view.divider_rows.len(), 1);
        assert_eq!(snapshot.app.view.host_banner_areas.len(), 2);
    }

    #[test]
    fn from_model_populates_host_banner_areas() {
        // item 2 (C3): the host-banner specs + the second slot of the single
        // compute_workspace_list_areas pass populate `app.host_banners` and
        // `app.view.host_banner_areas`, and flip `host_banner_active`. #19 (host half):
        // in multi-host mode the banners are [Local, remote] in visible_servers() order;
        // banner_idx indexes app.host_banners.
        let (model, _remote_id) = mixed_supervisor_model();
        let compositor = ClientCompositor::new(26);
        let snapshot =
            ClientSidebarSnapshot::from_model(&model, &compositor, 26, 60, 16, Instant::now());

        assert!(snapshot.app.host_banner_active);
        assert_eq!(snapshot.app.host_banners.len(), 2);
        assert_eq!(snapshot.app.host_banners[0].display_name, "local");
        assert_eq!(snapshot.app.host_banners[1].display_name, "x");
        assert_eq!(snapshot.app.view.host_banner_areas.len(), 2);
        // Every banner area never overlaps a workspace card (render == hit_test).
        for area in &snapshot.app.view.host_banner_areas {
            assert!(snapshot.app.view.workspace_card_areas.iter().all(|card| {
                !(area.rect.y >= card.rect.y && area.rect.y < card.rect.y + card.rect.height)
            }));
        }
    }

    #[test]
    fn divider_banner_insertion_does_not_shift_active_idx() {
        // item 6 (Area 6) / Area 2 no-shift regression: the optimistic override flips a
        // `focused` bool on a real Workspace row; `from_model` derives `active_idx` from the FLAT
        // `workspace_rows()` stream (which contains NO divider/banner entries — those are
        // layout-only). So even though this mixed model emits a divider AND a host banner,
        // `app.active`/`app.selected` land on the optimistic remote workspace's flat index,
        // unshifted by the non-selectable rows.
        let (mut model, remote_id) = mixed_supervisor_model();

        // Sanity: the model really does emit the non-selectable rows.
        let compositor = ClientCompositor::new(26);
        let pre =
            ClientSidebarSnapshot::from_model(&model, &compositor, 26, 60, 16, Instant::now());
        assert_eq!(pre.app.view.divider_rows.len(), 1);
        // #19 (host half): multi-host mode banners both hosts (Local + remote).
        assert_eq!(pre.app.view.host_banner_areas.len(), 2);

        // The remote workspace's index in the flat workspace_rows() stream (no divider/banner).
        let remote_idx = model
            .workspace_rows()
            .iter()
            .position(|row| {
                row.server_id == remote_id && row.workspace_id.as_deref() == Some("remote-api")
            })
            .expect("remote workspace row should be present in the flat stream");

        model.focus_workspace_route(&remote_id, "remote-api");

        let snapshot =
            ClientSidebarSnapshot::from_model(&model, &compositor, 26, 60, 16, Instant::now());
        // active/selected point at the optimistic remote row's flat index, NOT shifted by the
        // divider/banner rows that sit above it in the rendered list.
        assert_eq!(snapshot.app.active, Some(remote_idx));
        assert_eq!(snapshot.app.selected, remote_idx);
        // The flat workspace_rows() index is unchanged by the divider/banner insertion: the
        // optimistic remote row is at the same index whether or not the layout rows exist.
        assert_eq!(snapshot.app.workspaces.len(), model.workspace_rows().len());
    }

    #[test]
    fn agents_panel_follows_optimistic_group() {
        // item 6 (Area 6): with an optimistic agent focus, the agents panel follows the
        // optimistic server's group — the active workspace is the agent's workspace and that
        // workspace's pane (the agent) is focused, and `agent_groups()` reports the group focused.
        let mut model = ClientSupervisorModel::new("local");
        let remote_id = model.add_secondary(crate::remote_registry::RemoteDefinitionSnapshot {
            id: "remote-x".into(),
            name: "x".into(),
            target: crate::remote_registry::RemoteTargetSnapshot::Local {
                session: Some("x".into()),
            },
            session: None,
            keybindings: crate::remote_registry::RemoteKeybindingsSnapshot::Local,
            disabled: false,
        });
        model
            .set_summary(
                &ServerId::main(),
                ServerSummary {
                    workspaces: vec![WorkspaceSummary {
                        workspace_id: "main-herdr".into(),
                        label: "herdr".into(),
                        branch: None,
                        focused: true,
                        ..Default::default()
                    }],
                    agents: Vec::new(),
                },
            )
            .unwrap();
        model
            .set_summary(
                &remote_id,
                ServerSummary {
                    workspaces: vec![WorkspaceSummary {
                        workspace_id: "remote-api".into(),
                        label: "api".into(),
                        branch: None,
                        focused: false,
                        ..Default::default()
                    }],
                    agents: vec![AgentSummary {
                        agent_id: "remote-agent".into(),
                        workspace_id: "remote-api".into(),
                        label: "claude".into(),
                        status: "idle".into(),
                        focused: false,
                    }],
                },
            )
            .unwrap();

        model.focus_agent_route(&remote_id, "remote-agent");

        // The optimistic agent's group renders focused (the panel reads agent_groups()).
        let group = model
            .agent_groups()
            .into_iter()
            .find(|group| group.workspace_id == "remote-api")
            .expect("the agent's workspace group should exist");
        assert!(group.focused);
        assert!(group
            .agents
            .iter()
            .any(|agent| agent.agent_id == "remote-agent" && agent.focused));

        let compositor = ClientCompositor::new(26);
        let snapshot =
            ClientSidebarSnapshot::from_model(&model, &compositor, 26, 60, 16, Instant::now());

        // active/selected point at the agent's workspace row, and that workspace has a focused
        // pane (the agent) — so the composited agents panel renders that group as focused.
        let remote_idx = model
            .workspace_rows()
            .iter()
            .position(|row| {
                row.server_id == remote_id && row.workspace_id.as_deref() == Some("remote-api")
            })
            .expect("remote workspace row should be present");
        assert_eq!(snapshot.app.active, Some(remote_idx));
        assert!(snapshot.app.workspaces[remote_idx]
            .focused_pane_id()
            .is_some());
    }

    #[test]
    fn later_hosts_own_focus_does_not_steal_the_highlight() {
        // Every connected server permanently reports its own focused workspace + focused
        // agent. With the user on the MAIN server, a later remote's focused rows must not
        // claim `app.active`/`app.selected` (which drive both the spaces AND agents
        // highlights) — the regression showed the last host's agent rendered as selected
        // while a main-server pane was actually on screen.
        let mut model = ClientSupervisorModel::new("local");
        let remote_id = model.add_secondary(crate::remote_registry::RemoteDefinitionSnapshot {
            id: "remote-x".into(),
            name: "x".into(),
            target: crate::remote_registry::RemoteTargetSnapshot::Local {
                session: Some("x".into()),
            },
            session: None,
            keybindings: crate::remote_registry::RemoteKeybindingsSnapshot::Local,
            disabled: false,
        });
        model
            .set_summary(
                &ServerId::main(),
                ServerSummary {
                    workspaces: vec![
                        WorkspaceSummary {
                            workspace_id: "main-dev".into(),
                            label: "dev".into(),
                            branch: None,
                            focused: true,
                            ..Default::default()
                        },
                        WorkspaceSummary {
                            workspace_id: "main-learn".into(),
                            label: "learn".into(),
                            branch: None,
                            focused: false,
                            ..Default::default()
                        },
                    ],
                    agents: vec![AgentSummary {
                        agent_id: "main-agent".into(),
                        workspace_id: "main-dev".into(),
                        label: "grok".into(),
                        status: "working".into(),
                        focused: true,
                    }],
                },
            )
            .unwrap();
        // The remote also reports a focused workspace AND a focused agent — its permanent
        // per-server facts, not a user focus change.
        model
            .set_summary(
                &remote_id,
                ServerSummary {
                    workspaces: vec![WorkspaceSummary {
                        workspace_id: "remote-herdr".into(),
                        label: "herdr".into(),
                        branch: None,
                        focused: true,
                        ..Default::default()
                    }],
                    agents: vec![AgentSummary {
                        agent_id: "remote-agent".into(),
                        workspace_id: "remote-herdr".into(),
                        label: "grok".into(),
                        status: "working".into(),
                        focused: true,
                    }],
                },
            )
            .unwrap();
        assert_eq!(model.active_server_id(), &ServerId::main());

        let compositor = ClientCompositor::new(26);
        let snapshot =
            ClientSidebarSnapshot::from_model(&model, &compositor, 26, 60, 20, Instant::now());

        // The main server's focused workspace row (flat index 0) owns the highlight, and its
        // focused pane is the highlighted agent row — NOT the remote's trailing focused pair.
        let main_idx = model
            .workspace_rows()
            .iter()
            .position(|row| row.workspace_id.as_deref() == Some("main-dev"))
            .expect("main dev row present");
        assert_eq!(snapshot.app.active, Some(main_idx));
        assert_eq!(snapshot.app.selected, main_idx);
        assert!(snapshot.app.workspaces[main_idx]
            .focused_pane_id()
            .is_some());

        // Switching the active server moves the highlight to the remote's focused row.
        model.focus_workspace_route(&remote_id, "remote-herdr");
        let snapshot =
            ClientSidebarSnapshot::from_model(&model, &compositor, 26, 60, 20, Instant::now());
        let remote_idx = model
            .workspace_rows()
            .iter()
            .position(|row| row.workspace_id.as_deref() == Some("remote-herdr"))
            .expect("remote herdr row present");
        assert_eq!(snapshot.app.active, Some(remote_idx));
        assert_eq!(snapshot.app.selected, remote_idx);
    }

    #[test]
    fn hit_test_none_over_banner_row() {
        // The host-banner row is not a Workspace/affordance target — hit-test yields no
        // Workspace target over it (render == hit_test; banners are non-selectable).
        let (model, _remote_id) = mixed_supervisor_model();
        let compositor = ClientCompositor::new(26);
        let snapshot =
            ClientSidebarSnapshot::from_model(&model, &compositor, 26, 60, 16, Instant::now());
        let banner_y = snapshot.app.view.host_banner_areas[0].rect.y;
        let hit = compositor.hit_test(&model, 1, banner_y, 60, 16);
        assert!(
            !matches!(hit, Some(SidebarHitTarget::Workspace { .. })),
            "banner row {banner_y} hit-tested to a workspace: {hit:?}"
        );
        // No card overlaps the banner row, so the real rows still resolve to their cards.
        for card in &snapshot.app.view.workspace_card_areas {
            assert_ne!(card.rect.y, banner_y, "a card overlaps the banner row");
        }
    }

    #[test]
    fn from_model_no_divider_rows_for_all_local_model() {
        let mut model = ClientSupervisorModel::new("local");
        model
            .set_summary(
                &ServerId::main(),
                ServerSummary {
                    workspaces: vec![WorkspaceSummary {
                        workspace_id: "main-herdr".into(),
                        label: "herdr".into(),
                        branch: None,
                        focused: true,
                        ..Default::default()
                    }],
                    agents: Vec::new(),
                },
            )
            .unwrap();
        let compositor = ClientCompositor::new(26);
        let snapshot =
            ClientSidebarSnapshot::from_model(&model, &compositor, 26, 60, 16, Instant::now());
        assert!(snapshot.app.view.divider_rows.is_empty());
    }

    #[test]
    fn client_hit_test_returns_no_workspace_for_divider_row() {
        let (model, _remote_id) = mixed_supervisor_model();
        let compositor = ClientCompositor::new(26);
        // Derive the divider y from the same snapshot geometry render uses (render == hit_test).
        let snapshot =
            ClientSidebarSnapshot::from_model(&model, &compositor, 26, 60, 16, Instant::now());
        let divider_y = snapshot.app.view.divider_rows[0];

        // The divider row resolves to no Workspace target.
        let divider_hit = compositor.hit_test(&model, 1, divider_y, 60, 16);
        assert!(
            !matches!(divider_hit, Some(SidebarHitTarget::Workspace { .. })),
            "divider row {divider_y} hit-tested to a workspace: {divider_hit:?}"
        );
        // The real workspace rows still resolve to their cards (none at the divider y).
        for card in &snapshot.app.view.workspace_card_areas {
            assert_ne!(card.rect.y, divider_y, "a card overlaps the divider row");
            assert!(matches!(
                compositor.hit_test(&model, 1, card.rect.y, 60, 16),
                Some(SidebarHitTarget::Workspace { .. })
            ));
        }
    }

    #[test]
    fn hover_hit_test_skips_divider_row() {
        // Regression lock for the hover impl (item 7): the click-path geometry used by
        // hover (workspace_card_areas) yields no workspace for the divider row.
        let (model, _remote_id) = mixed_supervisor_model();
        let compositor = ClientCompositor::new(26);
        let snapshot =
            ClientSidebarSnapshot::from_model(&model, &compositor, 26, 60, 16, Instant::now());
        let divider_y = snapshot.app.view.divider_rows[0];
        assert!(snapshot
            .app
            .view
            .workspace_card_areas
            .iter()
            .all(|card| !(divider_y >= card.rect.y && divider_y < card.rect.y + card.rect.height)));
    }

    /// Build a mixed two-destination model (main `local` + remote `x`) and open the picker.
    fn two_destination_picker_model() -> (ClientSupervisorModel, ServerId) {
        let mut model = ClientSupervisorModel::new("local");
        let remote_id = model.add_secondary(crate::remote_registry::RemoteDefinitionSnapshot {
            id: "remote-x".into(),
            name: "x".into(),
            target: crate::remote_registry::RemoteTargetSnapshot::Local {
                session: Some("x".into()),
            },
            session: None,
            keybindings: crate::remote_registry::RemoteKeybindingsSnapshot::Local,
            disabled: false,
        });
        model
            .set_summary(
                &ServerId::main(),
                ServerSummary {
                    workspaces: vec![WorkspaceSummary {
                        workspace_id: "main-herdr".into(),
                        label: "herdr".into(),
                        branch: None,
                        focused: true,
                        ..Default::default()
                    }],
                    agents: Vec::new(),
                },
            )
            .unwrap();
        model
            .set_summary(
                &remote_id,
                ServerSummary {
                    workspaces: vec![WorkspaceSummary {
                        workspace_id: "remote-api".into(),
                        label: "api".into(),
                        branch: None,
                        focused: false,
                        ..Default::default()
                    }],
                    agents: vec![AgentSummary {
                        agent_id: "remote-agent".into(),
                        workspace_id: "remote-api".into(),
                        label: "claude".into(),
                        status: "idle".into(),
                        focused: false,
                    }],
                },
            )
            .unwrap();
        model.open_new_workspace_picker();
        (model, remote_id)
    }

    #[test]
    fn new_workspace_picker_renders_footer_anchored_selectable_list() {
        let (model, _) = two_destination_picker_model();

        let compositor = ClientCompositor::new(26);
        let content = frame(8, 3, &["content", "frame"]);
        let composed =
            compositor.compose_frame(&model, &content, 60, 20, std::time::Instant::now());
        let rows: Vec<_> = (0..composed.height)
            .map(|row| row_text(&composed, row))
            .collect();

        // footer-anchored ratatui popup: square corner, header, sub-label, both destinations,
        // buttons.
        assert!(rows.iter().any(|row| row.contains("┌")));
        assert!(rows.iter().any(|row| row.contains("new workspace")));
        assert!(rows.iter().any(|row| row.contains("create on")));
        assert!(rows.iter().any(|row| row.contains("local")));
        assert!(rows.iter().any(|row| row.contains("x")));
        // default selection (index 0) carries the `›` marker.
        assert!(rows.iter().any(|row| row.contains("›")));
        assert!(rows.iter().any(|row| row.contains("create")));
        assert!(rows.iter().any(|row| row.contains("cancel")));

        // the popup is footer-anchored (opens upward from the sidebar footer), NOT centered: its
        // top border sits below the host top, and it never reaches the bottom rows.
        let popup =
            crate::ui::new_workspace_picker_popup_rect(anchor_area(&model, &compositor, 60, 20), 2)
                .expect("popup fits");
        assert!(popup.y > 0, "popup is not flush to host top");
        assert!(rows[popup.y as usize].contains("┌"), "top border row");
        // rows below the popup show server content / blanks, not the picker.
        for row in &rows[(popup.y + popup.height) as usize..] {
            assert!(!row.contains("new workspace"));
        }
    }

    #[test]
    fn new_workspace_picker_mouse_click_hit_tests_footer_anchored_rows() {
        let (model, remote_id) = two_destination_picker_model();
        let compositor = ClientCompositor::new(26);

        // derive the FOOTER-ANCHORED row coordinates from the SAME shared geometry + anchor_area
        // the renderer/hit-test use.
        let anchor = anchor_area(&model, &compositor, 60, 20);
        let inner = crate::ui::new_workspace_picker_inner_rect(anchor, 2).expect("modal fits");
        let row0 = crate::ui::new_workspace_picker_row_rect(inner, 0);
        let row1 = crate::ui::new_workspace_picker_row_rect(inner, 1);

        // the popup is footer-anchored, so its rows sit below the host top (not centered, not the
        // old bottom-anchored geometry).
        assert!(row0.y > 0);

        assert_eq!(
            compositor.hit_test(&model, row0.x, row0.y, 60, 20),
            Some(SidebarHitTarget::NewWorkspaceDestination {
                server_id: ServerId::main(),
            })
        );
        assert_eq!(
            compositor.hit_test(&model, row1.x, row1.y, 60, 20),
            Some(SidebarHitTarget::NewWorkspaceDestination {
                server_id: remote_id,
            })
        );
    }

    #[test]
    fn new_workspace_picker_keyboard_navigates_and_confirms() {
        let (mut model, remote_id) = two_destination_picker_model();
        assert_eq!(model.new_workspace_picker().map(|p| p.selected), Some(0));

        model.move_new_workspace_picker_next();
        assert_eq!(model.new_workspace_picker().map(|p| p.selected), Some(1));

        let route = model.accept_new_workspace_picker();
        assert_eq!(route, NewWorkspaceRoute::CreateOn(remote_id));
    }

    #[test]
    fn picker_confirm_and_cancel_buttons_hit_test() {
        let (model, _) = two_destination_picker_model();
        let compositor = ClientCompositor::new(26);

        let anchor = anchor_area(&model, &compositor, 60, 20);
        let inner = crate::ui::new_workspace_picker_inner_rect(anchor, 2).expect("modal fits");
        let (confirm, cancel) = crate::ui::new_workspace_picker_button_rects(inner);

        assert_eq!(
            compositor.hit_test(&model, confirm.x, confirm.y, 60, 20),
            Some(SidebarHitTarget::NewWorkspacePickerConfirm)
        );
        assert_eq!(
            compositor.hit_test(&model, cancel.x, cancel.y, 60, 20),
            Some(SidebarHitTarget::NewWorkspacePickerCancel)
        );
    }

    #[test]
    fn client_global_menu_uses_server_launcher_menu_surface() {
        let mut model = ClientSupervisorModel::new("local");
        model.open_client_global_menu();

        let compositor = ClientCompositor::new(26);
        let content = frame(8, 3, &["content", "frame"]);
        let composed =
            compositor.compose_frame(&model, &content, 60, 16, std::time::Instant::now());

        let rows: Vec<_> = (0..composed.height)
            .map(|row| row_text(&composed, row))
            .collect();
        assert!(rows.iter().any(|row| row.contains("┌")));
        assert!(rows.iter().any(|row| row.contains("settings")));
        assert!(rows.iter().any(|row| row.contains("keybinds")));
        assert!(rows.iter().any(|row| row.contains("reload config")));
        assert!(rows.iter().any(|row| row.contains("detach")));
        assert!(rows.iter().any(|row| row.contains("add remote")));
        assert_eq!(
            compositor.hit_test(&model, 21, 1, 60, 16),
            Some(SidebarHitTarget::ClientGlobalMenuItem { index: 0 })
        );
        assert_eq!(
            compositor.hit_test(&model, 21, 5, 60, 16),
            Some(SidebarHitTarget::ClientGlobalMenuItem { index: 4 })
        );
    }

    #[test]
    fn client_global_menu_hover_moves_highlight_render() {
        // item 7: moving the highlight (as a hover `Moved` does via `hover_client_global_menu_item`)
        // repaints the accent bg onto the newly highlighted row and clears it from the old one — the
        // shared launcher-menu surface renders `highlighted` identically to the monolithic host.
        let compositor = ClientCompositor::new(26);
        let content = frame(8, 3, &["content", "frame"]);

        let mut model = ClientSupervisorModel::new("local");
        model.open_client_global_menu(); // highlighted defaults to index 0.
        let before = compositor.compose_frame(&model, &content, 60, 16, std::time::Instant::now());

        assert!(model.hover_client_global_menu_item(Some(2)));
        let after = compositor.compose_frame(&model, &content, 60, 16, std::time::Instant::now());

        let snapshot = ClientSidebarSnapshot::from_model(
            &model,
            &compositor,
            26,
            60,
            16,
            std::time::Instant::now(),
        );
        let rect = snapshot.app.global_menu_rect();
        let row0 = rect.y + 1; // item index 0 ("settings").
        let row1 = rect.y + 2; // item index 1 ("keybinds"): never highlighted in this test.
        let row2 = rect.y + 3; // item index 2 ("reload config").

        let bgs = |frame: &FrameData, row: u16| -> Vec<u32> {
            (0..frame.width)
                .map(|x| cell_at(frame, x, row).bg)
                .collect()
        };
        // before: index 0 is highlighted, so its bg differs from the unhighlighted neighbour row 1
        // (same-width label, so an unhighlighted row 0 would match row 1 exactly).
        assert_ne!(
            bgs(&before, row0),
            bgs(&before, row1),
            "row 0 starts highlighted"
        );
        // after moving the highlight to index 2: row 0 reverts to an unhighlighted row (matches the
        // unhighlighted row 1), and row 2 now carries a highlight bg row 1 lacks.
        assert_eq!(
            bgs(&after, row0),
            bgs(&after, row1),
            "row 0 reverts to unhighlighted"
        );
        assert_ne!(
            bgs(&after, row2),
            bgs(&after, row1),
            "row 2 becomes highlighted"
        );
    }

    /// Read the cell at (x, y) of a composited frame.
    fn cell_at(frame: &FrameData, x: u16, y: u16) -> &CellData {
        &frame.cells[(y as usize) * (frame.width as usize) + (x as usize)]
    }

    /// Encode an RGB color the same way `FrameData::from_ratatui_buffer_with_hyperlinks` does, so
    /// the modal tests can match against palette colors.
    fn encode_rgb(r: u8, g: u8, b: u8) -> u32 {
        0x02_00_00_00 | ((r as u32) << 16) | ((g as u32) << 8) | (b as u32)
    }

    /// True iff any cell on `row` carries `bg` (e.g. the focused field's `surface0` fill).
    fn row_has_bg(frame: &FrameData, row: u16, bg: u32) -> bool {
        (0..frame.width).any(|x| cell_at(frame, x, row).bg == bg)
    }

    #[test]
    fn add_remote_modal_renders_accent_border_and_action_buttons() {
        let mut model = ClientSupervisorModel::new("local");
        model.open_add_remote_form();

        let compositor = ClientCompositor::new(26);
        let content = frame(20, 8, &["content", "frame"]);
        let composed =
            compositor.compose_frame(&model, &content, 80, 24, std::time::Instant::now());
        let rows: Vec<_> = (0..composed.height)
            .map(|row| row_text(&composed, row))
            .collect();

        // border::PLAIN square corner (NOT the legacy rounded `╭`).
        assert!(rows.iter().any(|row| row.contains("┌")));
        assert!(!rows.iter().any(|row| row.contains("╭")));
        assert!(rows.iter().any(|row| row.contains("add remote")));
        assert!(rows.iter().any(|row| row.contains("target")));
        assert!(rows.iter().any(|row| row.contains("name")));
        // action buttons.
        assert!(rows.iter().any(|row| row.contains("add")));
        assert!(rows.iter().any(|row| row.contains("cancel")));
        // legacy ASCII art / raw markers / the old footer literal are ABSENT.
        assert!(!rows.iter().any(|row| row.contains("+---")));
        assert!(!rows.iter().any(|row| row.contains("enter add   esc close")));
    }

    #[test]
    fn modal_survives_full_screen_content_overwrite() {
        // Regression: a composited overlay must stay visible even when the server content frame
        // fills the ENTIRE content area (a real pane full of text). The content copy protects
        // EXACTLY the open popup's rect, so the overlay survives AND the rest of the content stays
        // visible around it (the popup is footer-anchored, NOT a full-screen-dimmed centered modal).
        let mut model = ClientSupervisorModel::new("local");
        model.open_add_remote_form();
        // content frame that fills the whole content area with a sentinel (like a busy pane).
        let filled = "#".repeat(54);
        let rows: Vec<&str> = vec![filled.as_str(); 24];
        let content = frame(54, 24, &rows);

        let compositor = ClientCompositor::new(26);
        let composed =
            compositor.compose_frame(&model, &content, 80, 24, std::time::Instant::now());
        let texts: Vec<String> = (0..composed.height)
            .map(|r| row_text(&composed, r))
            .collect();

        // the overlay (header, a field label, the cancel button) must survive — these sit in the
        // content columns and would be overwritten by the '#' fill without the popup-rect exclusion.
        assert!(
            texts.iter().any(|t| t.contains("add remote")),
            "overlay header overwritten by content; rows={texts:?}"
        );
        assert!(
            texts.iter().any(|t| t.contains("target")),
            "overlay field overwritten by content"
        );
        assert!(
            texts.iter().any(|t| t.contains("cancel")),
            "overlay action button overwritten by content"
        );

        // ...AND the content sentinel must STILL be visible somewhere in the content area OUTSIDE
        // the popup rect — proving the fix protects only the popup, not the whole screen (the old
        // bug blanked everything).
        let popup =
            crate::ui::add_remote_popup_rect(compositor.overlay_anchor_area(&model, 80, 24))
                .expect("popup fits");
        let sidebar_width = 26u16;
        let sentinel_outside_popup = (0..composed.height).any(|row| {
            (sidebar_width..composed.width).any(|col| {
                let inside_popup = col >= popup.x
                    && col < popup.x + popup.width
                    && row >= popup.y
                    && row < popup.y + popup.height;
                !inside_popup && cell_at(&composed, col, row).symbol == "#"
            })
        });
        assert!(
            sentinel_outside_popup,
            "content '#' fully blanked; only the popup rect should be protected; rows={texts:?}"
        );
    }

    #[test]
    fn add_remote_modal_marks_focused_field() {
        let mut model = ClientSupervisorModel::new("local");
        model.open_add_remote_form(); // focus defaults to Target.

        let compositor = ClientCompositor::new(26);
        let content = frame(20, 8, &["content", "frame"]);
        let composed =
            compositor.compose_frame(&model, &content, 80, 24, std::time::Instant::now());

        // inner rect for the footer-anchored overlay: rows[1] (target) and rows[2] (name).
        let inner = crate::ui::add_remote_inner_rect(anchor_area(&model, &compositor, 80, 24))
            .expect("modal fits");
        let target_row = inner.y.saturating_add(1);
        let name_row = inner.y.saturating_add(2);

        // catppuccin surface0 = Rgb(49, 50, 68); the focused target field carries that fill.
        let surface0 = encode_rgb(49, 50, 68);
        assert!(row_has_bg(&composed, target_row, surface0));
        // the target label cells now carry non-zero fg (regression vs. the old colorless draw).
        assert!((0..composed.width).any(|x| cell_at(&composed, x, target_row).fg != 0));
        // the unfocused name row does NOT carry the focused `surface0` bg (it has panel_bg).
        assert!(!row_has_bg(&composed, name_row, surface0));
    }

    #[test]
    fn add_remote_modal_shows_inline_error() {
        let mut model = ClientSupervisorModel::new("local");
        model.open_add_remote_form();
        // Enter with an empty target produces the `target required` inline error.
        model.handle_add_remote_key(crate::input::TerminalKey::new(
            crossterm::event::KeyCode::Enter,
            crossterm::event::KeyModifiers::empty(),
        ));

        let compositor = ClientCompositor::new(26);
        let content = frame(20, 8, &["content", "frame"]);
        let composed =
            compositor.compose_frame(&model, &content, 80, 24, std::time::Instant::now());
        let rows: Vec<_> = (0..composed.height)
            .map(|row| row_text(&composed, row))
            .collect();
        assert!(rows.iter().any(|row| row.contains("target required")));

        // an async-style error string renders too.
        model.set_add_remote_error("adding remote...");
        let composed =
            compositor.compose_frame(&model, &content, 80, 24, std::time::Instant::now());
        let rows: Vec<_> = (0..composed.height)
            .map(|row| row_text(&composed, row))
            .collect();
        assert!(rows.iter().any(|row| row.contains("adding remote...")));
    }

    #[test]
    fn add_remote_modal_keeps_cursor_hidden() {
        let mut model = ClientSupervisorModel::new("local");
        model.open_add_remote_form();

        let compositor = ClientCompositor::new(26);
        let content = frame(20, 8, &["content", "frame"]);
        let composed =
            compositor.compose_frame(&model, &content, 80, 24, std::time::Instant::now());

        assert!(composed.cursor.is_none());
    }

    #[test]
    fn add_remote_button_click_submits_and_cancel_closes() {
        let mut model = ClientSupervisorModel::new("local");
        model.open_add_remote_form();
        let compositor = ClientCompositor::new(26);

        let inner = crate::ui::add_remote_inner_rect(anchor_area(&model, &compositor, 80, 24))
            .expect("modal fits");
        let (submit, cancel) = crate::ui::add_remote_button_rects(inner);

        assert_eq!(
            compositor.hit_test(&model, submit.x, submit.y, 80, 24),
            Some(SidebarHitTarget::AddRemoteSubmit)
        );
        assert_eq!(
            compositor.hit_test(&model, cancel.x, cancel.y, 80, 24),
            Some(SidebarHitTarget::AddRemoteCancel)
        );
    }

    // --- item 5: client agent animation --------------------------------------------------

    /// Build a mixed model with one main workspace and one remote workspace whose single agent
    /// has `agent_status` (e.g. "working" / "idle"). Both servers connect by default.
    fn model_with_agent_status(agent_status: &str) -> (ClientSupervisorModel, ServerId) {
        let mut model = ClientSupervisorModel::new("local");
        let remote_id = model.add_secondary(crate::remote_registry::RemoteDefinitionSnapshot {
            id: "remote-x".into(),
            name: "x".into(),
            target: crate::remote_registry::RemoteTargetSnapshot::Local {
                session: Some("x".into()),
            },
            session: None,
            keybindings: crate::remote_registry::RemoteKeybindingsSnapshot::Local,
            disabled: false,
        });
        model
            .set_summary(
                &ServerId::main(),
                ServerSummary {
                    workspaces: vec![WorkspaceSummary {
                        workspace_id: "main-herdr".into(),
                        label: "herdr".into(),
                        branch: Some("master".into()),
                        focused: true,
                        ..Default::default()
                    }],
                    agents: Vec::new(),
                },
            )
            .unwrap();
        model
            .set_summary(
                &remote_id,
                ServerSummary {
                    workspaces: vec![WorkspaceSummary {
                        workspace_id: "remote-api".into(),
                        label: "api".into(),
                        branch: Some("feature/api".into()),
                        focused: false,
                        ..Default::default()
                    }],
                    agents: vec![AgentSummary {
                        agent_id: "remote-agent".into(),
                        workspace_id: "remote-api".into(),
                        label: "claude".into(),
                        status: agent_status.into(),
                        focused: false,
                    }],
                },
            )
            .unwrap();
        (model, remote_id)
    }

    #[test]
    fn animation_tick_feeds_spinner_tick() {
        let (model, _) = model_with_agent_status("working");
        let mut compositor = ClientCompositor::new(26);
        compositor.advance_animation_tick(8);

        let snapshot = ClientSidebarSnapshot::from_model(
            &model,
            &compositor,
            26,
            60,
            16,
            std::time::Instant::now(),
        );

        assert_eq!(snapshot.app.spinner_tick, 8);
    }

    #[test]
    fn spinner_cell_differs_between_tick_0_and_8() {
        let (model, _) = model_with_agent_status("working");
        let content = frame(8, 3, &["content", "frame"]);

        let at_zero = ClientCompositor::new(26);
        let frame_zero = at_zero.compose_frame(&model, &content, 60, 16, std::time::Instant::now());

        let mut at_eight = ClientCompositor::new(26);
        at_eight.advance_animation_tick(8);
        let frame_eight =
            at_eight.compose_frame(&model, &content, 60, 16, std::time::Instant::now());

        let symbols_zero: Vec<_> = frame_zero.cells.iter().map(|c| c.symbol.clone()).collect();
        let symbols_eight: Vec<_> = frame_eight.cells.iter().map(|c| c.symbol.clone()).collect();
        assert_ne!(
            symbols_zero, symbols_eight,
            "spinner_frame should advance the agent-status cell between tick 0 and tick 8"
        );
        // The spinner glyph at tick 0 is SPINNERS[0] = ⠋, at tick 8 it is SPINNERS[1] = ⠙.
        assert!(symbols_zero.iter().any(|s| s == "⠋"));
        assert!(symbols_eight.iter().any(|s| s == "⠙"));
    }

    #[test]
    fn advance_animation_tick_wraps_and_steps() {
        let mut compositor = ClientCompositor::new(26);
        assert_eq!(compositor.animation_tick(), 0);
        compositor.advance_animation_tick(8);
        assert_eq!(compositor.animation_tick(), 8);
        compositor.advance_animation_tick(8);
        assert_eq!(compositor.animation_tick(), 16);

        // Wrap cleanly at the u32 boundary with no discontinuity in the visible step `tick/8`.
        let mut wrapping = ClientCompositor::new(26);
        wrapping.advance_animation_tick(u32::MAX - 3);
        let before = wrapping.animation_tick();
        wrapping.advance_animation_tick(8);
        let after = wrapping.animation_tick();
        assert_eq!(after, before.wrapping_add(8));
        assert!(after < before, "tick should wrap past the u32 boundary");
    }

    #[test]
    fn sidebar_wants_animation_true_with_working_agent() {
        let (model, _) = model_with_agent_status("working");
        assert!(sidebar_wants_animation(&model));
    }

    /// Force the host-banner animation off so a test can isolate the agent-driven animation
    /// gate (item 2 (C3): a visible Secondary now animates its banner by default).
    fn with_static_host_banner(model: &mut ClientSupervisorModel) {
        let mut ui_settings = model.ui_settings().clone();
        ui_settings.sidebar_host.animation = crate::config::HostBannerAnimation::Static;
        model.set_ui_settings(ui_settings);
    }

    #[test]
    fn sidebar_wants_animation_false_when_all_idle() {
        // With the banner animation forced Static, only the agent gate remains — idle agents
        // never request animation.
        for status in ["idle", "done", "blocked", "unknown"] {
            let (mut model, _) = model_with_agent_status(status);
            with_static_host_banner(&mut model);
            assert!(
                !sidebar_wants_animation(&model),
                "status {status:?} should not request animation"
            );
        }
    }

    #[test]
    fn sidebar_wants_animation_true_with_banner() {
        // item 2 (C3): the banner hook is now the real gate. With no working agent the gate is
        // driven solely by `host_banner_animation_active` — a visible Secondary with the default
        // Animated setting makes the gate true (proving the banner hook is the single
        // banner-active input the gate reads).
        let (model, _) = model_with_agent_status("idle");
        assert!(model.host_banner_animation_active());
        assert!(sidebar_wants_animation(&model));
        assert_eq!(
            sidebar_wants_animation(&model),
            model.host_banner_animation_active(),
            "with no working agent the gate equals the banner hook"
        );
    }

    // A working agent flows through the detection setter so the placeholder terminal renders in
    // `Working` state (the shared renderer animates it with the client's spinner tick). Upstream
    // has no live working-duration display, so the PoC's `working_since` seeding is not ported.
    #[test]
    fn working_agent_status_projects_working_terminal_state() {
        let (model, _remote_id) = model_with_agent_status("working");
        let compositor = ClientCompositor::new(26);
        let snapshot =
            ClientSidebarSnapshot::from_model(&model, &compositor, 26, 60, 16, Instant::now());

        assert!(
            snapshot
                .app
                .terminals
                .values()
                .any(|terminal| terminal.state == AgentState::Working),
            "a working remote agent should project a Working placeholder terminal"
        );
    }

    #[test]
    fn disabled_remote_agent_rows_do_not_gate_animation() {
        // A disabled remote's placeholder rows are not `working`, so they never request
        // animation on themselves (parity with the render==hit_test disabled-row rejection).
        let mut model = ClientSupervisorModel::new("local");
        let remote_id = model.add_secondary(crate::remote_registry::RemoteDefinitionSnapshot {
            id: "remote-x".into(),
            name: "x".into(),
            target: crate::remote_registry::RemoteTargetSnapshot::Local {
                session: Some("x".into()),
            },
            session: None,
            keybindings: crate::remote_registry::RemoteKeybindingsSnapshot::Local,
            disabled: false,
        });
        model
            .set_connection_state(
                &remote_id,
                crate::client::supervisor::ConnectionState::Disconnected,
            )
            .unwrap();
        // Force the banner animation Static to isolate the agent gate: a disconnected remote's
        // placeholder rows are not `working`, so they never request animation on themselves.
        with_static_host_banner(&mut model);
        assert!(!sidebar_wants_animation(&model));
    }

    // ----- item 3 (Area 5): remote-management overlay render == hit_test --------------------

    fn manage_overlay_model() -> ClientSupervisorModel {
        let mut model = ClientSupervisorModel::new("local");
        model.add_secondary(crate::remote_registry::RemoteDefinitionSnapshot {
            id: "r1".into(),
            name: "alpha".into(),
            target: crate::remote_registry::RemoteTargetSnapshot::Ssh {
                target: "alpha".into(),
                args: Vec::new(),
            },
            session: None,
            keybindings: crate::remote_registry::RemoteKeybindingsSnapshot::Local,
            disabled: false,
        });
        model.add_secondary(crate::remote_registry::RemoteDefinitionSnapshot {
            id: "r2".into(),
            name: "beta".into(),
            target: crate::remote_registry::RemoteTargetSnapshot::Ssh {
                target: "beta".into(),
                args: Vec::new(),
            },
            session: None,
            keybindings: crate::remote_registry::RemoteKeybindingsSnapshot::Local,
            disabled: false,
        });
        model.open_remote_manage_overlay();
        model
    }

    #[test]
    fn remote_manage_render_equals_hit_test_geometry() {
        let model = manage_overlay_model();
        let compositor = ClientCompositor::new(26);
        let anchor = anchor_area(&model, &compositor, 80, 24);

        // the rect the renderer draws row N into is the rect hit_test checks.
        let inner = crate::ui::remote_manage_inner_rect(anchor, 2).expect("modal fits");
        let row0 = crate::ui::remote_manage_row_rect(inner, 0);
        let row1 = crate::ui::remote_manage_row_rect(inner, 1);
        assert_eq!(
            compositor.hit_test(&model, row0.x, row0.y, 80, 24),
            Some(SidebarHitTarget::RemoteManageRow { index: 0 })
        );
        assert_eq!(
            compositor.hit_test(&model, row1.x, row1.y, 80, 24),
            Some(SidebarHitTarget::RemoteManageRow { index: 1 })
        );
    }

    #[test]
    fn hit_test_returns_manage_targets() {
        let model = manage_overlay_model();
        let compositor = ClientCompositor::new(26);
        let anchor = anchor_area(&model, &compositor, 80, 24);
        let inner = crate::ui::remote_manage_inner_rect(anchor, 2).expect("modal fits");

        // row click selects.
        let row0 = crate::ui::remote_manage_row_rect(inner, 0);
        assert_eq!(
            compositor.hit_test(&model, row0.x, row0.y, 80, 24),
            Some(SidebarHitTarget::RemoteManageRow { index: 0 })
        );
        // footer `add` affordance.
        let footer_y = inner.y + inner.height.saturating_sub(1);
        assert_eq!(
            compositor.hit_test(&model, inner.x, footer_y, 80, 24),
            Some(SidebarHitTarget::RemoteManageAdd)
        );
        // click well outside the modal → None.
        assert_eq!(compositor.hit_test(&model, 0, 0, 80, 24), None);
    }

    #[test]
    fn manage_overlay_render_is_pure() {
        let model = manage_overlay_model();
        let compositor = ClientCompositor::new(26);
        let content = frame(8, 3, &["content", "frame"]);

        let a = compositor.compose_frame(&model, &content, 80, 24, std::time::Instant::now());
        let b = compositor.compose_frame(&model, &content, 80, 24, std::time::Instant::now());
        assert_eq!(a.cells, b.cells, "list render must be deterministic");

        // delete-confirm sub-state is also pure. `compose_frame` takes `&model` (shared ref), so
        // non-mutation is structural; determinism across two renders confirms purity.
        let mut model = manage_overlay_model();
        model.begin_remote_manage_delete();
        let c = compositor.compose_frame(&model, &content, 80, 24, std::time::Instant::now());
        let d = compositor.compose_frame(&model, &content, 80, 24, std::time::Instant::now());
        assert_eq!(c.cells, d.cells, "confirm render must be deterministic");
        assert!(model
            .remote_manage_overlay()
            .unwrap()
            .confirm_delete
            .is_some());
    }

    #[test]
    fn confirm_delete_renders_red_panel() {
        let mut model = manage_overlay_model();
        model.begin_remote_manage_delete();
        let compositor = ClientCompositor::new(26);
        let content = frame(8, 3, &["content", "frame"]);
        let composed =
            compositor.compose_frame(&model, &content, 80, 24, std::time::Instant::now());
        let rows: Vec<_> = (0..composed.height)
            .map(|row| row_text(&composed, row))
            .collect();
        assert!(rows.iter().any(|row| row.contains("delete remote?")));
        assert!(rows.iter().any(|row| row.contains("delete")));
        assert!(rows.iter().any(|row| row.contains("cancel")));

        // while confirm is active the list rows are NOT hit-testable; only the popup buttons are.
        let anchor = anchor_area(&model, &compositor, 80, 24);
        let inner = crate::ui::remote_manage_inner_rect(anchor, 2).expect("modal fits");
        let row0 = crate::ui::remote_manage_row_rect(inner, 0);
        assert!(!matches!(
            compositor.hit_test(&model, row0.x, row0.y, 80, 24),
            Some(SidebarHitTarget::RemoteManageRow { .. })
        ));
        let popup = crate::ui::remote_manage_confirm_popup_rect(anchor).expect("popup fits");
        let pinner = Rect::new(
            popup.x + 1,
            popup.y + 1,
            popup.width.saturating_sub(2),
            popup.height.saturating_sub(2),
        );
        let (delete_rect, cancel_rect) = crate::ui::remote_manage_confirm_button_rects(pinner);
        assert_eq!(
            compositor.hit_test(&model, delete_rect.x, delete_rect.y, 80, 24),
            Some(SidebarHitTarget::RemoteManageConfirmDelete)
        );
        assert_eq!(
            compositor.hit_test(&model, cancel_rect.x, cancel_rect.y, 80, 24),
            Some(SidebarHitTarget::RemoteManageCancelDelete)
        );
    }

    #[test]
    fn manage_overlay_hit_test_skips_non_modal() {
        // A model with a focused main workspace AND the overlay open: a click on the (sidebar)
        // workspace row resolves to a manage target or None, NEVER a Workspace hit — the overlay
        // intercepts the whole host rect first.
        let mut model = manage_overlay_model();
        model
            .set_summary(
                &ServerId::main(),
                ServerSummary {
                    workspaces: vec![WorkspaceSummary {
                        workspace_id: "main-ws".into(),
                        label: "herdr".into(),
                        branch: None,
                        focused: true,
                        ..Default::default()
                    }],
                    agents: Vec::new(),
                },
            )
            .unwrap();
        let compositor = ClientCompositor::new(26);
        // sweep a column of the sidebar; none may resolve to a Workspace.
        for y in 0..24u16 {
            let hit = compositor.hit_test(&model, 1, y, 80, 24);
            assert!(
                !matches!(hit, Some(SidebarHitTarget::Workspace { .. })),
                "overlay must intercept sidebar workspace hits, got {hit:?} at y={y}"
            );
        }
    }

    // ---- item 7 (Area 4): hover_test / set_hover / from_model mirror ----

    use crate::app::state::SidebarHoverTarget;

    #[test]
    fn set_hover_reports_change() {
        let mut compositor = ClientCompositor::new(26);
        assert!(compositor.set_hover(Some(SidebarHoverTarget::Workspace { ws_idx: 0 })));
        assert!(!compositor.set_hover(Some(SidebarHoverTarget::Workspace { ws_idx: 0 })));
        assert!(compositor.set_hover(Some(SidebarHoverTarget::Workspace { ws_idx: 1 })));
        assert!(compositor.set_hover(None));
        assert!(!compositor.set_hover(None));
    }

    #[test]
    fn from_model_mirrors_compositor_hover() {
        // A hover set on the compositor truth appears in the render snapshot (Copy; pure read).
        let (model, _remote_id) = mixed_supervisor_model();
        let mut compositor = ClientCompositor::new(26);
        compositor.set_hover(Some(SidebarHoverTarget::Workspace { ws_idx: 1 }));
        let snapshot =
            ClientSidebarSnapshot::from_model(&model, &compositor, 26, 60, 16, Instant::now());
        assert_eq!(
            snapshot.app.sidebar_hover,
            Some(SidebarHoverTarget::Workspace { ws_idx: 1 })
        );
    }

    #[test]
    fn hover_test_resolves_workspace_row() {
        // render == hit_test geometry: drive the row index off `hit_test` (like the click test),
        // then assert `hover_test` over the same (x,y) resolves the matching Workspace ws_idx.
        let (model, remote_id) = mixed_supervisor_model();
        let compositor = ClientCompositor::new(26);
        let host = (60u16, 28u16);
        let snapshot = ClientSidebarSnapshot::from_model(
            &model,
            &compositor,
            26,
            host.0,
            host.1,
            Instant::now(),
        );
        let remote_card = snapshot
            .app
            .view
            .workspace_card_areas
            .iter()
            .find(|c| c.ws_idx == 1)
            .expect("remote card");
        // sanity: the click path resolves the remote workspace at this row.
        assert_eq!(
            compositor.hit_test(&model, 1, remote_card.rect.y, host.0, host.1),
            Some(SidebarHitTarget::Workspace {
                server_id: remote_id.clone(),
                workspace_id: "remote-api".into(),
            })
        );
        assert_eq!(
            compositor.hover_test(&model, 1, remote_card.rect.y, host.0, host.1),
            Some(SidebarHoverTarget::Workspace { ws_idx: 1 })
        );
    }

    #[test]
    fn hover_test_skips_non_selectable_rows() {
        // the ` spaces`/` agents` header rows, the `─` separator, the right-edge resize column,
        // and the item-4 divider row never resolve to a Workspace/Agent hover target.
        let (model, _remote_id) = mixed_supervisor_model();
        let compositor = ClientCompositor::new(26);
        let host = (60u16, 28u16);
        let snapshot = ClientSidebarSnapshot::from_model(
            &model,
            &compositor,
            26,
            host.0,
            host.1,
            Instant::now(),
        );

        // ` spaces` header is the sidebar's first row.
        let header_y = snapshot.app.view.sidebar_rect.y;
        let hover = compositor.hover_test(&model, 1, header_y, host.0, host.1);
        assert!(!matches!(
            hover,
            Some(SidebarHoverTarget::Workspace { .. })
                | Some(SidebarHoverTarget::AgentRoute { .. })
        ));

        // the right-edge resize column (x == sidebar_width - 1 .. is the divider `│`): a position
        // at x >= effective_sidebar_width resolves None.
        assert_eq!(
            compositor.hover_test(&model, 27, header_y, host.0, host.1),
            None
        );

        // the item-4 divider row resolves to the defensive Divider (NEVER a Workspace/Agent) —
        // render treats Divider as no-highlight.
        let divider_y = snapshot.app.view.divider_rows[0];
        assert_eq!(
            compositor.hover_test(&model, 1, divider_y, host.0, host.1),
            Some(SidebarHoverTarget::Divider)
        );
    }

    #[test]
    fn hover_test_ignores_disabled_workspace_rows() {
        // mirror hit_test_ignores_disabled_workspace_rows: a disabled remote route hovers None.
        let mut model = ClientSupervisorModel::new("local");
        let remote_id = model.add_secondary(crate::remote_registry::RemoteDefinitionSnapshot {
            id: "remote-x".into(),
            name: "x".into(),
            target: crate::remote_registry::RemoteTargetSnapshot::Local {
                session: Some("x".into()),
            },
            session: None,
            keybindings: crate::remote_registry::RemoteKeybindingsSnapshot::Local,
            disabled: false,
        });
        model
            .set_summary(
                &remote_id,
                ServerSummary {
                    workspaces: vec![WorkspaceSummary {
                        workspace_id: "remote-api".into(),
                        label: "api".into(),
                        branch: None,
                        focused: false,
                        ..Default::default()
                    }],
                    agents: Vec::new(),
                },
            )
            .unwrap();
        model
            .set_connection_state(
                &remote_id,
                crate::client::supervisor::ConnectionState::Disconnected,
            )
            .unwrap();
        let compositor = ClientCompositor::new(26);
        // sweep the sidebar column: no row resolves to a Workspace hover (disabled rows rejected).
        for y in 0..16u16 {
            assert!(!matches!(
                compositor.hover_test(&model, 1, y, 60, 16),
                Some(SidebarHoverTarget::Workspace { .. })
            ));
        }
    }

    #[test]
    fn hover_test_agent_resolves_route_index_and_survives_recompose() {
        // contradiction-11 regression: hover over an agent entry returns AgentRoute { route_idx };
        // rebuilding the snapshot (recompose) keeps the SAME route_idx mapping to the same agent
        // even though the placeholder pane_id is freshly alloc'd each recompose.
        let (model, _remote_id) = mixed_supervisor_model_with_agent();
        let compositor = ClientCompositor::new(26);
        let host = (60u16, 20u16);

        // find the agent row by sweeping (the click path resolves the agent there).
        let agent_row = (0..host.1)
            .find(|y| {
                matches!(
                    compositor.hit_test(&model, 1, *y, host.0, host.1),
                    Some(SidebarHitTarget::Agent { agent_id, .. }) if agent_id == "remote-agent"
                )
            })
            .expect("agent row should be hit-testable");

        let hover = compositor.hover_test(&model, 1, agent_row, host.0, host.1);
        let route_idx = match hover {
            Some(SidebarHoverTarget::AgentRoute { route_idx }) => route_idx,
            other => panic!("expected AgentRoute, got {other:?}"),
        };

        // recompose: the snapshot's agent_routes are rebuilt; the same route_idx still points at
        // the same agent (positional index, not the dead pane_id).
        let snap_a = ClientSidebarSnapshot::from_model(
            &model,
            &compositor,
            26,
            host.0,
            host.1,
            Instant::now(),
        );
        let snap_b = ClientSidebarSnapshot::from_model(
            &model,
            &compositor,
            26,
            host.0,
            host.1,
            Instant::now(),
        );
        assert_eq!(
            snap_a.agent_routes[route_idx].agent_id,
            snap_b.agent_routes[route_idx].agent_id
        );
        assert_eq!(snap_a.agent_routes[route_idx].agent_id, "remote-agent");
        // hover_test resolves the SAME route_idx after recompose.
        assert_eq!(
            compositor.hover_test(&model, 1, agent_row, host.0, host.1),
            Some(SidebarHoverTarget::AgentRoute { route_idx })
        );
    }

    #[test]
    fn hover_test_affordances_respect_draw_gate() {
        // over `new`/`menu`/`filter` the hover resolves to the matching affordance. The client
        // snapshot is always `mouse_capture == true` (empty_for_client_rendering), so the
        // affordances are drawn and hoverable (the gate-off branch is exercised monolithically,
        // where mouse_capture can be false — see the input/mouse tests).
        let (model, _remote_id) = mixed_supervisor_model();
        let compositor = ClientCompositor::new(26);
        let host = (60u16, 16u16);
        let snapshot = ClientSidebarSnapshot::from_model(
            &model,
            &compositor,
            26,
            host.0,
            host.1,
            Instant::now(),
        );

        let new_rect = snapshot.app.sidebar_new_button_rect();
        let menu_rect = snapshot.app.global_launcher_rect();
        // empty_for_client_rendering defaults mouse_capture = true, so affordances are drawn.
        assert!(snapshot.app.mouse_capture);
        assert_eq!(
            compositor.hover_test(&model, new_rect.x, new_rect.y, host.0, host.1),
            Some(SidebarHoverTarget::New)
        );
        assert_eq!(
            compositor.hover_test(
                &model,
                menu_rect.x + menu_rect.width - 1,
                menu_rect.y,
                host.0,
                host.1
            ),
            Some(SidebarHoverTarget::Menu)
        );
        // the filter label (top-right of the sidebar).
        assert_eq!(
            compositor.hover_test(&model, 23, snapshot.app.view.sidebar_rect.y, host.0, host.1),
            Some(SidebarHoverTarget::Filter)
        );
    }

    #[test]
    fn hover_test_suppressed_when_overlay_open() {
        // with the add-remote form open OR the client global menu highlighted, sidebar hover_test
        // returns None (the overlay owns its own hover; the global menu moves its highlight via the
        // separate `client_global_menu_item_at` path in the client `Moved` arm).
        let (mut model, _remote_id) = mixed_supervisor_model();
        model.open_add_remote_form();
        for y in 0..16u16 {
            assert_eq!(model_hover_anywhere(&model, y), None);
        }
        model.close_client_overlay();

        let (mut model, _remote_id) = mixed_supervisor_model();
        model.open_client_global_menu();
        for y in 0..16u16 {
            assert_eq!(model_hover_anywhere(&model, y), None);
        }
    }

    #[test]
    fn client_global_menu_item_at_resolves_hovered_row() {
        // item 7: motion over the open menu resolves to the row index under the cursor (same
        // geometry `hit_test` uses); a far-left column off the right-anchored menu resolves to None;
        // a closed menu resolves to None.
        let (mut model, _remote_id) = mixed_supervisor_model();
        let compositor = ClientCompositor::new(26);
        let host = (60u16, 16u16);
        // closed menu → None.
        assert_eq!(
            compositor.client_global_menu_item_at(&model, 21, 1, host.0, host.1),
            None
        );

        model.open_client_global_menu();
        let snapshot = ClientSidebarSnapshot::from_model(
            &model,
            &compositor,
            26,
            host.0,
            host.1,
            Instant::now(),
        );
        let rect = snapshot.app.global_menu_rect();
        // the first item row sits one cell inside the menu's top-left border.
        assert_eq!(
            compositor.client_global_menu_item_at(&model, rect.x + 1, rect.y + 1, host.0, host.1),
            Some(0)
        );
        // a deeper row resolves to its index.
        assert_eq!(
            compositor.client_global_menu_item_at(&model, rect.x + 1, rect.y + 3, host.0, host.1),
            Some(2)
        );
        // the far-left sidebar column misses the right-anchored menu → None.
        assert_eq!(
            compositor.client_global_menu_item_at(&model, 0, rect.y + 1, host.0, host.1),
            None
        );
    }

    fn model_hover_anywhere(model: &ClientSupervisorModel, y: u16) -> Option<SidebarHoverTarget> {
        ClientCompositor::new(26).hover_test(model, 1, y, 60, 16)
    }

    #[test]
    fn hover_test_none_when_collapsed() {
        // effective_sidebar_width == 0 (host_width <= 1) → None.
        let (model, _remote_id) = mixed_supervisor_model();
        let compositor = ClientCompositor::new(26);
        assert_eq!(compositor.hover_test(&model, 0, 0, 1, 16), None);
    }

    #[test]
    fn hover_test_resolves_new_workspace_picker_destination_row() {
        // the footer-anchored picker popup hovers its destination rows to
        // NewWorkspaceDestination { row }.
        let (model, _remote_id) = two_destination_picker_model();
        let compositor = ClientCompositor::new(26);
        let host = (60u16, 20u16);
        let anchor = anchor_area(&model, &compositor, host.0, host.1);
        let inner = crate::ui::new_workspace_picker_inner_rect(anchor, 2).expect("modal fits");
        let row1 = crate::ui::new_workspace_picker_row_rect(inner, 1);
        assert_eq!(
            compositor.hover_test(&model, row1.x, row1.y, host.0, host.1),
            Some(SidebarHoverTarget::NewWorkspaceDestination { row: 1 })
        );
    }

    // a [Main, Secondary] model with a remote agent, for agent-hover tests.
    fn mixed_supervisor_model_with_agent() -> (ClientSupervisorModel, ServerId) {
        let mut model = ClientSupervisorModel::new("local");
        let remote_id = model.add_secondary(crate::remote_registry::RemoteDefinitionSnapshot {
            id: "remote-x".into(),
            name: "x".into(),
            target: crate::remote_registry::RemoteTargetSnapshot::Local {
                session: Some("x".into()),
            },
            session: None,
            keybindings: crate::remote_registry::RemoteKeybindingsSnapshot::Local,
            disabled: false,
        });
        model
            .set_summary(
                &ServerId::main(),
                ServerSummary {
                    workspaces: vec![WorkspaceSummary {
                        workspace_id: "main-herdr".into(),
                        label: "herdr".into(),
                        branch: None,
                        focused: true,
                        ..Default::default()
                    }],
                    agents: Vec::new(),
                },
            )
            .unwrap();
        model
            .set_summary(
                &remote_id,
                ServerSummary {
                    workspaces: vec![WorkspaceSummary {
                        workspace_id: "remote-api".into(),
                        label: "api".into(),
                        branch: None,
                        focused: false,
                        ..Default::default()
                    }],
                    agents: vec![AgentSummary {
                        agent_id: "remote-agent".into(),
                        workspace_id: "remote-api".into(),
                        label: "claude".into(),
                        status: "idle".into(),
                        focused: false,
                    }],
                },
            )
            .unwrap();
        (model, remote_id)
    }

    // ---- #25: collapsed sidebar (client compositor) ----

    // A single-server model whose focused workspace owns an agent, so the collapsed detail section
    // (which draws one row per agent-panel entry) is non-empty. Returns the agent id for assertions.
    fn collapsed_model() -> (ClientSupervisorModel, ServerId, String) {
        let mut model = ClientSupervisorModel::new("local");
        model
            .set_summary(
                &ServerId::main(),
                ServerSummary {
                    workspaces: vec![
                        WorkspaceSummary {
                            workspace_id: "main-herdr".into(),
                            label: "herdr".into(),
                            branch: None,
                            focused: true,
                            ..Default::default()
                        },
                        WorkspaceSummary {
                            workspace_id: "ws-2".into(),
                            label: "two".into(),
                            branch: None,
                            focused: false,
                            ..Default::default()
                        },
                    ],
                    agents: vec![AgentSummary {
                        agent_id: "agent-1".into(),
                        workspace_id: "main-herdr".into(),
                        label: "claude".into(),
                        status: "idle".into(),
                        focused: false,
                    }],
                },
            )
            .unwrap();
        (model, ServerId::main(), "agent-1".into())
    }

    // The compositor's collapsed flag drives `from_model`'s `app.sidebar_collapsed`, which is what
    // gates the SHARED renderer onto its collapsed layout. Toggling flips it both ways.
    #[test]
    fn collapsed_flag_gates_snapshot() {
        let (model, _local, _agent) = collapsed_model();
        let mut compositor = ClientCompositor::new(26);
        let host = (60u16, 28u16);
        let snap = |c: &ClientCompositor| {
            ClientSidebarSnapshot::from_model(&model, c, 26, host.0, host.1, Instant::now())
        };

        assert!(!snap(&compositor).app.sidebar_collapsed);
        compositor.toggle_sidebar_collapsed();
        assert!(snap(&compositor).app.sidebar_collapsed);
        compositor.toggle_sidebar_collapsed();
        assert!(!snap(&compositor).app.sidebar_collapsed);
    }

    // The collapse/expand toggle is hittable in EXPANDED mode (so the user can collapse) — the
    // renderer draws it at `expanded_sidebar_toggle_rect`, the SAME rect the monolithic
    // `on_sidebar_toggle` checks.
    #[test]
    fn expanded_toggle_rect_hits_toggle_target() {
        let (model, _local, _agent) = collapsed_model();
        let compositor = ClientCompositor::new(26);
        let host = (60u16, 28u16);
        let snap = ClientSidebarSnapshot::from_model(
            &model,
            &compositor,
            26,
            host.0,
            host.1,
            Instant::now(),
        );
        let rect = crate::ui::expanded_sidebar_toggle_rect(snap.app.view.sidebar_rect);
        assert!(rect.width > 0);
        assert_eq!(
            compositor.hit_test(&model, rect.x, rect.y, host.0, host.1),
            Some(SidebarHitTarget::CollapsedSidebarToggle)
        );
    }

    // In COLLAPSED mode the toggle rect still hit-tests to the toggle; clicking it (mirrored by the
    // mod.rs dispatch flipping the compositor flag) returns to expanded.
    #[test]
    fn collapsed_toggle_rect_hits_and_clicking_expands() {
        let (model, _local, _agent) = collapsed_model();
        let mut compositor = ClientCompositor::new(26);
        compositor.toggle_sidebar_collapsed();
        let host = (60u16, 28u16);
        let snap = ClientSidebarSnapshot::from_model(
            &model,
            &compositor,
            26,
            host.0,
            host.1,
            Instant::now(),
        );
        assert!(snap.app.sidebar_collapsed);

        let rect = crate::ui::collapsed_sidebar_toggle_rect(snap.app.view.sidebar_rect);
        assert_eq!(
            compositor.hit_test(&model, rect.x, rect.y, host.0, host.1),
            Some(SidebarHitTarget::CollapsedSidebarToggle)
        );

        // The mod.rs dispatch flips the compositor flag on this target.
        compositor.toggle_sidebar_collapsed();
        let snap = ClientSidebarSnapshot::from_model(
            &model,
            &compositor,
            26,
            host.0,
            host.1,
            Instant::now(),
        );
        assert!(!snap.app.sidebar_collapsed);
    }

    // In COLLAPSED mode a workspace-glance row resolves to Workspace for the owning server, and that
    // target focuses the right workspace via `focus_workspace_route` (mirrors the expanded path).
    #[test]
    fn collapsed_workspace_row_hits_workspace_target() {
        let (mut model, local, _agent) = collapsed_model();
        let mut compositor = ClientCompositor::new(26);
        compositor.toggle_sidebar_collapsed();
        let host = (60u16, 28u16);
        let snap = ClientSidebarSnapshot::from_model(
            &model,
            &compositor,
            26,
            host.0,
            host.1,
            Instant::now(),
        );
        let (ws_area, _, _) = crate::ui::collapsed_sidebar_sections(snap.app.view.sidebar_rect);
        assert!(ws_area.width > 0 && ws_area.height > 0);

        // Second workspace glance row -> workspace index 1 ("ws-2").
        match compositor.hit_test(&model, ws_area.x, ws_area.y + 1, host.0, host.1) {
            Some(SidebarHitTarget::Workspace {
                server_id,
                workspace_id,
            }) => {
                assert_eq!(server_id, local);
                assert_eq!(workspace_id, "ws-2");
                // Same focus round-trip the expanded path uses.
                assert!(model
                    .focus_workspace_route(&server_id, &workspace_id)
                    .api_request("test")
                    .is_some());
            }
            other => panic!("expected Workspace target, got {other:?}"),
        }
    }

    // In COLLAPSED mode an agent-detail row resolves to Agent for the owning server, and that target
    // focuses the agent via `focus_agent_route`.
    #[test]
    fn collapsed_agent_detail_row_hits_agent_target() {
        let (mut model, local, agent) = collapsed_model();
        let mut compositor = ClientCompositor::new(26);
        compositor.toggle_sidebar_collapsed();
        let host = (60u16, 28u16);
        let snap = ClientSidebarSnapshot::from_model(
            &model,
            &compositor,
            26,
            host.0,
            host.1,
            Instant::now(),
        );
        let (_, _, detail_area) = crate::ui::collapsed_sidebar_sections(snap.app.view.sidebar_rect);
        assert!(detail_area.width > 0 && detail_area.height > 1);

        // First agent-detail row (the flat agent-panel entries, one row each).
        match compositor.hit_test(&model, detail_area.x, detail_area.y, host.0, host.1) {
            Some(SidebarHitTarget::Agent {
                server_id,
                agent_id,
            }) => {
                assert_eq!(server_id, local);
                assert_eq!(agent_id, agent);
                assert!(model
                    .focus_agent_route(&server_id, &agent_id)
                    .api_request("test")
                    .is_some());
            }
            other => panic!("expected Agent target, got {other:?}"),
        }
    }

    // #22: a worktree GROUP on the local server — a parent (non-linked) plus one linked child that
    // share a worktree key, so the SHARED grouping renderer (`workspace_parent_group_state`) treats
    // it as a collapsible group.
    fn worktree_grouped_model() -> ClientSupervisorModel {
        let mut model = ClientSupervisorModel::new("local");
        model
            .set_summary(
                &ServerId::main(),
                ServerSummary {
                    workspaces: vec![
                        WorkspaceSummary {
                            workspace_id: "parent".into(),
                            label: "herdr".into(),
                            branch: Some("master".into()),
                            focused: true,
                            worktree_key: Some("repo-key".into()),
                            worktree_is_linked: false,
                        },
                        WorkspaceSummary {
                            workspace_id: "child".into(),
                            label: "herdr".into(),
                            branch: Some("feature".into()),
                            focused: false,
                            worktree_key: Some("repo-key".into()),
                            worktree_is_linked: true,
                        },
                    ],
                    agents: Vec::new(),
                },
            )
            .unwrap();
        model
    }

    fn worktree_parent_card(
        snapshot: &ClientSidebarSnapshot,
    ) -> crate::app::state::WorkspaceCardArea {
        snapshot
            .app
            .view
            .workspace_card_areas
            .iter()
            .find(|card| {
                crate::ui::workspace_parent_group_state(&snapshot.app, card.ws_idx).is_some()
            })
            .cloned()
            .expect("a worktree-parent card")
    }

    // #22: a click in the chevron column (column 0) of a worktree-parent row resolves to the chevron
    // (NOT focus); toggling it flips the client-local collapsed set, and the next snapshot renders
    // the group collapsed. Mirrors the server's `clicking_worktree_parent_chevron_toggles_group_only`.
    #[test]
    fn worktree_parent_chevron_hit_test_resolves_and_toggles_group() {
        let model = worktree_grouped_model();
        let mut compositor = ClientCompositor::new(26);
        let host = (60u16, 28u16);
        let snapshot = ClientSidebarSnapshot::from_model(
            &model,
            &compositor,
            26,
            host.0,
            host.1,
            Instant::now(),
        );
        let parent = worktree_parent_card(&snapshot);
        let (group_key, collapsed) =
            crate::ui::workspace_parent_group_state(&snapshot.app, parent.ws_idx).unwrap();
        assert!(!collapsed, "the group starts expanded");

        let chevron_hit = compositor.hit_test(&model, parent.rect.x, parent.rect.y, host.0, host.1);
        assert_eq!(
            chevron_hit,
            Some(SidebarHitTarget::WorktreeChevron {
                group_key: group_key.clone()
            })
        );

        compositor.toggle_collapsed_space_key(group_key.clone());
        assert!(compositor
            .collapsed_space_keys_for_test()
            .contains(&group_key));
        let collapsed_snap = ClientSidebarSnapshot::from_model(
            &model,
            &compositor,
            26,
            host.0,
            host.1,
            Instant::now(),
        );
        assert_eq!(
            crate::ui::workspace_parent_group_state(&collapsed_snap.app, parent.ws_idx)
                .map(|(_, collapsed)| collapsed),
            Some(true),
            "the group renders collapsed after the toggle"
        );
        // A second toggle expands it again (server's remove-if-present contract).
        compositor.toggle_collapsed_space_key(group_key.clone());
        assert!(!compositor
            .collapsed_space_keys_for_test()
            .contains(&group_key));
    }

    // #22: a click on the parent row BODY (past the chevron column) focuses the workspace and never
    // toggles the group. Mirrors `clicking_worktree_parent_row_focuses_workspace_without_toggling`.
    #[test]
    fn worktree_parent_body_click_focuses_without_toggling() {
        let model = worktree_grouped_model();
        let compositor = ClientCompositor::new(26);
        let host = (60u16, 28u16);
        let snapshot = ClientSidebarSnapshot::from_model(
            &model,
            &compositor,
            26,
            host.0,
            host.1,
            Instant::now(),
        );
        let parent = worktree_parent_card(&snapshot);
        let body_hit =
            compositor.hit_test(&model, parent.rect.x + 2, parent.rect.y, host.0, host.1);
        match body_hit {
            Some(SidebarHitTarget::Workspace {
                server_id,
                workspace_id,
            }) => {
                assert_eq!(server_id, ServerId::main());
                assert_eq!(workspace_id, "parent");
            }
            other => panic!("expected Workspace focus on a body click, got {other:?}"),
        }
    }

    // #22: a standalone (ungrouped) workspace exposes no chevron — a column-0 click focuses it.
    #[test]
    fn standalone_workspace_has_no_chevron_hit() {
        let model = single_server_two_ws_model();
        let compositor = ClientCompositor::new(26);
        let host = (60u16, 28u16);
        let snapshot = ClientSidebarSnapshot::from_model(
            &model,
            &compositor,
            26,
            host.0,
            host.1,
            Instant::now(),
        );
        for card in &snapshot.app.view.workspace_card_areas {
            let hit = compositor.hit_test(&model, card.rect.x, card.rect.y, host.0, host.1);
            assert!(
                !matches!(hit, Some(SidebarHitTarget::WorktreeChevron { .. })),
                "standalone workspace must not expose a chevron hit region"
            );
        }
    }
}
