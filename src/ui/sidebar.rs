mod tokens;

use ratatui::{
    layout::{Alignment, Rect},
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::Paragraph,
    Frame,
};

use self::tokens::{ResolvedToken, SpaceTokenContext};
use super::scrollbar::{render_scrollbar, should_show_scrollbar};
use super::status::{agent_icon, state_dot, state_label, state_label_color};
use super::text::{display_width, display_width_u16, truncate_end};
use crate::app::state::{AgentPanelSort, Palette};
use crate::app::{AppState, Mode};
use crate::detect::AgentState;
use crate::terminal::TerminalRuntimeRegistry;

const WORKSPACE_SECTION_HEADER_ROWS: u16 = 2;
const AGENT_PANEL_HEADER_ROWS: u16 = 3;

pub(crate) struct AgentPanelEntry {
    pub ws_idx: usize,
    pub tab_idx: usize,
    pub pane_id: crate::layout::PaneId,
    pub primary_label: String,
    pub primary_tab_label: Option<String>,
    pub pane_label: Option<String>,
    pub terminal_title: Option<String>,
    pub terminal_title_stripped: Option<String>,
    pub agent_label: Option<String>,
    pub agent: Option<crate::detect::Agent>,
    pub state: AgentState,
    pub seen: bool,
    pub last_agent_state_change_seq: Option<u64>,
    pub state_labels: std::collections::HashMap<String, String>,
    pub tokens: std::collections::HashMap<String, String>,
}

fn sidebar_section_heights(total_h: u16, split_ratio: f32) -> (u16, u16) {
    if total_h == 0 {
        return (0, 0);
    }

    if total_h < 6 {
        let ws_h = total_h.div_ceil(2);
        return (ws_h, total_h.saturating_sub(ws_h));
    }

    let ratio = split_ratio.clamp(0.1, 0.9);
    let ws_h = ((total_h as f32) * ratio).round() as u16;
    let ws_h = ws_h.clamp(3, total_h.saturating_sub(3));
    let detail_h = total_h.saturating_sub(ws_h);
    (ws_h, detail_h)
}

pub(crate) fn expanded_sidebar_sections(area: Rect, split_ratio: f32) -> (Rect, Rect) {
    let content = Rect::new(area.x, area.y, area.width.saturating_sub(1), area.height);
    if content.width == 0 || content.height == 0 {
        return (Rect::default(), Rect::default());
    }

    let (ws_h, detail_h) = sidebar_section_heights(content.height, split_ratio);
    let ws_area = Rect::new(content.x, content.y, content.width, ws_h);
    let detail_area = Rect::new(content.x, content.y + ws_h, content.width, detail_h);
    (ws_area, detail_area)
}

pub(crate) fn sidebar_section_divider_rect(area: Rect, split_ratio: f32) -> Rect {
    let content = Rect::new(area.x, area.y, area.width.saturating_sub(1), area.height);
    if content.width == 0 || content.height < 6 {
        return Rect::default();
    }

    let (ws_h, _) = sidebar_section_heights(content.height, split_ratio);
    Rect::new(content.x, content.y + ws_h, content.width, 1)
}

fn agent_panel_sort_label(sort: AgentPanelSort) -> &'static str {
    match sort {
        AgentPanelSort::Spaces => "grouped",
        AgentPanelSort::Priority => "priority",
    }
}

pub(crate) fn agent_panel_toggle_rect(area: Rect, sort: AgentPanelSort) -> Rect {
    if area.width == 0 || area.height < 2 {
        return Rect::default();
    }

    let label = agent_panel_sort_label(sort);
    let width = display_width_u16(label);
    Rect::new(
        area.x + area.width.saturating_sub(width),
        area.y + 1,
        width,
        1,
    )
}

pub(crate) fn agent_panel_entries(app: &AppState) -> Vec<AgentPanelEntry> {
    agent_panel_entries_with_runtimes(app, None)
}

pub(crate) fn agent_panel_entries_from(
    app: &AppState,
    terminal_runtimes: &TerminalRuntimeRegistry,
) -> Vec<AgentPanelEntry> {
    agent_panel_entries_with_runtimes(app, Some(terminal_runtimes))
}

fn agent_panel_entries_with_runtimes(
    app: &AppState,
    terminal_runtimes: Option<&TerminalRuntimeRegistry>,
) -> Vec<AgentPanelEntry> {
    let empty_runtimes;
    let terminal_runtimes = match terminal_runtimes {
        Some(terminal_runtimes) => terminal_runtimes,
        None => {
            empty_runtimes = TerminalRuntimeRegistry::new();
            &empty_runtimes
        }
    };

    let mut entries: Vec<_> = app
        .workspaces
        .iter()
        .enumerate()
        .flat_map(|(ws_idx, ws)| {
            let multi_tab = ws.tabs.len() > 1;
            let workspace_label = ws.display_name_from(&app.terminals, terminal_runtimes);
            ws.pane_details(&app.terminals)
                .into_iter()
                .map(move |detail| AgentPanelEntry {
                    ws_idx,
                    tab_idx: detail.tab_idx,
                    pane_id: detail.pane_id,
                    primary_label: workspace_label.clone(),
                    primary_tab_label: multi_tab.then_some(detail.tab_label),
                    pane_label: detail.pane_label,
                    terminal_title: detail.terminal_title,
                    terminal_title_stripped: detail.terminal_title_stripped,
                    agent_label: Some(detail.agent_label),
                    agent: detail.agent,
                    state: detail.state,
                    seen: detail.seen,
                    last_agent_state_change_seq: detail.last_agent_state_change_seq,
                    state_labels: detail.state_labels,
                    tokens: detail.tokens,
                })
        })
        .collect();

    if matches!(app.agent_panel_sort, AgentPanelSort::Priority) {
        entries.sort_by_key(|entry| {
            (
                std::cmp::Reverse(workspace_attention_priority(entry.state, entry.seen)),
                std::cmp::Reverse(entry.last_agent_state_change_seq),
            )
        });
    }

    entries
}

pub(super) fn agent_panel_status_key(state: AgentState, seen: bool) -> &'static str {
    match (state, seen) {
        (AgentState::Idle, false) => "done",
        (AgentState::Idle, true) => "idle",
        (AgentState::Working, _) => "working",
        (AgentState::Blocked, _) => "blocked",
        (AgentState::Unknown, _) => "unknown",
    }
}

fn workspace_row_height(app: &AppState, ws: &crate::workspace::Workspace, indented: bool) -> u16 {
    let (state, seen) = ws.aggregate_state(&app.terminals);
    let label = if indented {
        grouped_child_display_label(
            &ws.display_name(),
            ws.branch().as_deref(),
            ws.custom_name.is_some(),
        )
    } else {
        ws.display_name()
    };
    let token_values = ws.metadata_tokens.values();
    tokens::space_rows(
        &app.sidebar_spaces,
        SpaceTokenContext {
            workspace: &label,
            branch: ws.branch().as_deref(),
            state_text: state_label(state, seen),
            ahead_behind: ws.git_ahead_behind(),
            tokens: &token_values,
            suppress_git_details: indented,
        },
    )
    .len()
    .max(1)
    .min(u16::MAX as usize) as u16
}

fn workspace_row_height_in_body(
    app: &AppState,
    workspace: &crate::workspace::Workspace,
    indented: bool,
    body_height: u16,
) -> u16 {
    workspace_row_height(app, workspace, indented).min(body_height)
}

fn workspace_entry_gap(entries: &[WorkspaceListEntry], entry_idx: usize, indented: bool) -> u16 {
    u16::from(
        entry_idx + 1 < entries.len()
            && !(indented && next_entry_is_indented_workspace(entries, entry_idx)),
    )
}

fn workspace_attention_priority(state: AgentState, seen: bool) -> u8 {
    match (state, seen) {
        (AgentState::Blocked, _) => 4,
        (AgentState::Idle, false) => 3,
        (AgentState::Working, _) => 2,
        (AgentState::Idle, true) => 1,
        (AgentState::Unknown, _) => 0,
    }
}

fn space_aggregate_state(app: &AppState, key: &str) -> (AgentState, bool) {
    app.workspaces
        .iter()
        .filter(|ws| ws.worktree_space().is_some_and(|space| space.key == key))
        .map(|ws| ws.aggregate_state(&app.terminals))
        .max_by_key(|(state, seen)| workspace_attention_priority(*state, *seen))
        .unwrap_or((AgentState::Unknown, true))
}

pub(crate) fn workspace_parent_group_state(
    app: &AppState,
    ws_idx: usize,
) -> Option<(String, bool)> {
    let space = app.workspaces.get(ws_idx)?.worktree_space()?;
    if space.is_linked_worktree {
        return None;
    }
    let member_count = app
        .workspaces
        .iter()
        .filter(|ws| {
            ws.worktree_space()
                .is_some_and(|member| member.key == space.key)
        })
        .count();
    (member_count >= 2).then(|| {
        (
            space.key.clone(),
            app.collapsed_space_keys.contains(&space.key),
        )
    })
}

pub(crate) fn grouped_child_display_label(
    label: &str,
    branch: Option<&str>,
    has_custom_name: bool,
) -> String {
    if has_custom_name {
        return label.to_string();
    }
    let Some(branch) = branch else {
        return label.to_string();
    };
    branch
        .strip_prefix("worktree/")
        .unwrap_or(branch)
        .to_string()
}

/// The bold host-banner name span: one flat theme accent color, no gradients
/// or animation (the former `[ui.sidebar.host]` style knobs were removed).
fn host_banner_name_span(name: &str, p: &Palette) -> Span<'static> {
    Span::styled(
        name.to_string(),
        Style::default().fg(p.accent).add_modifier(Modifier::BOLD),
    )
}

/// The leading connection-state glyph for a host banner.
fn host_banner_glyph(state: crate::app::state::HostBannerState) -> &'static str {
    use crate::app::state::HostBannerState;
    match state {
        HostBannerState::Connected => "◆",
        HostBannerState::Connecting
        | HostBannerState::Disconnected
        | HostBannerState::ProtocolMismatch
        | HostBannerState::Disabled => "◇",
    }
}

/// The dim suffix that follows the host name, keyed off the banner state.
/// `Connected` shows nothing; the other states surface the state word.
fn host_banner_suffix(state: crate::app::state::HostBannerState) -> Option<&'static str> {
    use crate::app::state::HostBannerState;
    match state {
        HostBannerState::Connected => None,
        HostBannerState::Connecting => Some(" · connecting"),
        HostBannerState::Disconnected => Some(" · offline"),
        HostBannerState::ProtocolMismatch => Some(" · protocol mismatch"),
        HostBannerState::Disabled => Some(" · disabled"),
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum WorkspaceListEntry {
    Workspace {
        ws_idx: usize,
        indented: bool,
    },
    /// The local→remote divider rule. Empty (never emitted) in monolithic mode.
    Divider {
        labeled: bool,
    },
    /// A remote host's banner row; `banner_idx` indexes `app.host_banners`.
    HostBanner {
        banner_idx: usize,
    },
}

/// One per rendered `HostBanner` entry in the workspace list. Empty in monolithic mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct HostBannerArea {
    pub banner_idx: usize,
    pub rect: Rect,
}

pub(crate) fn next_entry_is_indented_workspace(entries: &[WorkspaceListEntry], idx: usize) -> bool {
    matches!(
        entries.get(idx.saturating_add(1)),
        Some(WorkspaceListEntry::Workspace { indented: true, .. })
    )
}

pub(crate) fn normalized_workspace_scroll(app: &AppState, area: Rect, requested: usize) -> usize {
    let ws_area = workspace_list_rect(area, app.sidebar_section_split);
    let body = workspace_list_body_rect(ws_area, false);
    if body.height == 0 {
        return requested;
    }

    if workspace_list_entries(app).is_empty() {
        0
    } else {
        requested.min(workspace_list_bottom_start(app, ws_area))
    }
}

pub(crate) fn workspace_list_entries(app: &AppState) -> Vec<WorkspaceListEntry> {
    let entries = workspace_list_entries_inner(app, false);
    let entries = insert_local_remote_divider(app, entries);
    insert_host_banners(app, entries)
}

/// Insert one `HostBanner { banner_idx }` immediately before each remote host group's first
/// workspace, in visible-server order (the order `app.host_banners` and `app.host_banner_rows`
/// are built in). `host_banner_rows[i]` is the `ws_idx` of the i-th banner's host group's first
/// workspace; the banner sits BELOW the local→remote divider (which was already inserted) and
/// ABOVE that host's `Workspace` rows. Empty in monolithic mode (no banners). `banner_idx`
/// indexes `app.host_banners`.
fn insert_host_banners(
    app: &AppState,
    entries: Vec<WorkspaceListEntry>,
) -> Vec<WorkspaceListEntry> {
    if app.host_banner_rows.is_empty() {
        return entries;
    }

    let mut result = Vec::with_capacity(entries.len() + app.host_banner_rows.len());
    for entry in entries {
        if let WorkspaceListEntry::Workspace {
            ws_idx,
            indented: false,
        } = entry
        {
            // A banner precedes the un-indented first row of its host group. Each host's first
            // workspace is recorded once in `host_banner_rows`; emit its banner here.
            if let Some(banner_idx) = app.host_banner_rows.iter().position(|row| *row == ws_idx) {
                result.push(WorkspaceListEntry::HostBanner { banner_idx });
            }
        }
        result.push(entry);
    }
    result
}

/// Insert exactly ONE `Divider` at the first local→remote boundary in the EMITTED (visual)
/// entry order. The per-row local/remote signal is `app.client_workspace_remote` (index-aligned
/// with `app.workspaces`, empty in monolithic mode so no divider). Inserted only when BOTH a
/// local (`false`) and a remote (`true`) workspace are present in the emitted order — never a
/// dangling rule for all-local / all-remote / single-server-filter (one role group → no
/// transition). The divider sits ABOVE offline/empty-remote placeholder rows (those carry
/// `is_remote == true`). `labeled = !app.host_banner_active`: a standalone `─ remote ─` rule
/// until the host banner owns the host name, then a plain rule.
fn insert_local_remote_divider(
    app: &AppState,
    entries: Vec<WorkspaceListEntry>,
) -> Vec<WorkspaceListEntry> {
    let is_remote = |entry: &WorkspaceListEntry| match entry {
        WorkspaceListEntry::Workspace { ws_idx, .. } => {
            app.client_workspace_remote.get(*ws_idx).copied()
        }
        // Non-selectable rows carry no role; the host banner rides positionally below.
        WorkspaceListEntry::Divider { .. } | WorkspaceListEntry::HostBanner { .. } => None,
    };

    let any_local = entries.iter().any(|entry| is_remote(entry) == Some(false));
    let boundary = entries
        .iter()
        .position(|entry| is_remote(entry) == Some(true));

    // Both groups must be present in the emitted order, or there is no boundary to mark.
    let Some(boundary) = boundary.filter(|_| any_local) else {
        return entries;
    };

    let mut with_divider = Vec::with_capacity(entries.len() + 1);
    with_divider.extend(entries[..boundary].iter().cloned());
    with_divider.push(WorkspaceListEntry::Divider {
        labeled: !app.host_banner_active,
    });
    with_divider.extend(entries[boundary..].iter().cloned());
    with_divider
}

/// Like [`workspace_list_entries`] but always expands worktree groups, ignoring
/// `collapsed_space_keys`. The mobile switcher has no collapse affordance and
/// always shows the full worktree tree.
pub(crate) fn workspace_list_entries_expanded(app: &AppState) -> Vec<WorkspaceListEntry> {
    workspace_list_entries_inner(app, true)
}

fn workspace_list_entries_inner(app: &AppState, force_expanded: bool) -> Vec<WorkspaceListEntry> {
    let mut members_by_key = std::collections::HashMap::<String, Vec<usize>>::new();
    for (ws_idx, ws) in app.workspaces.iter().enumerate() {
        if let Some(space) = ws.worktree_space() {
            members_by_key
                .entry(space.key.clone())
                .or_default()
                .push(ws_idx);
        }
    }
    let grouped_keys = members_by_key
        .iter()
        .filter(|(_, members)| {
            members.len() >= 2
                && members.iter().any(|idx| {
                    app.workspaces
                        .get(*idx)
                        .and_then(|ws| ws.worktree_space())
                        .is_some_and(|space| !space.is_linked_worktree)
                })
        })
        .map(|(key, _)| key.clone())
        .collect::<std::collections::HashSet<_>>();

    let visible_group_idx = if matches!(app.mode, Mode::Navigate) {
        Some(app.selected)
    } else {
        app.active
    };
    let active_group = visible_group_idx.and_then(|idx| {
        app.workspaces
            .get(idx)
            .and_then(|ws| ws.worktree_space())
            .map(|space| space.key.clone())
    });

    let mut emitted_groups = std::collections::HashSet::<String>::new();
    let mut entries = Vec::new();
    for (ws_idx, ws) in app.workspaces.iter().enumerate() {
        let Some(space) = ws
            .worktree_space()
            .filter(|space| grouped_keys.contains(&space.key))
        else {
            entries.push(WorkspaceListEntry::Workspace {
                ws_idx,
                indented: false,
            });
            continue;
        };

        if !emitted_groups.insert(space.key.clone()) {
            continue;
        }

        let Some(members) = members_by_key.get(&space.key) else {
            continue;
        };
        let Some(parent_idx) = members.iter().copied().find(|idx| {
            app.workspaces
                .get(*idx)
                .and_then(|member| member.worktree_space())
                .is_some_and(|member_space| !member_space.is_linked_worktree)
        }) else {
            entries.push(WorkspaceListEntry::Workspace {
                ws_idx,
                indented: false,
            });
            continue;
        };
        let collapsed = !force_expanded && app.collapsed_space_keys.contains(&space.key);
        entries.push(WorkspaceListEntry::Workspace {
            ws_idx: parent_idx,
            indented: false,
        });

        if collapsed {
            if let Some(active_idx) = visible_group_idx
                .filter(|idx| *idx != parent_idx)
                .filter(|_| active_group.as_deref() == Some(space.key.as_str()))
            {
                entries.push(WorkspaceListEntry::Workspace {
                    ws_idx: active_idx,
                    indented: true,
                });
            }
        } else {
            for member_idx in members {
                if *member_idx == parent_idx {
                    continue;
                }
                entries.push(WorkspaceListEntry::Workspace {
                    ws_idx: *member_idx,
                    indented: true,
                });
            }
        }
    }
    entries
}

pub(crate) fn workspace_list_rect(area: Rect, split_ratio: f32) -> Rect {
    let (ws_area, _) = expanded_sidebar_sections(area, split_ratio);
    ws_area
}

pub(crate) fn workspace_list_body_rect(area: Rect, has_scrollbar: bool) -> Rect {
    if area.width == 0 || area.height <= WORKSPACE_SECTION_HEADER_ROWS {
        return Rect::default();
    }

    let body_y = area.y.saturating_add(WORKSPACE_SECTION_HEADER_ROWS);
    let footer_y = area.y + area.height.saturating_sub(1);
    let body_height = footer_y.saturating_sub(body_y);
    let body_width = area.width.saturating_sub(u16::from(has_scrollbar));
    Rect::new(area.x, body_y, body_width, body_height)
}

fn workspace_list_visible_count(app: &AppState, area: Rect, scroll: usize) -> usize {
    let body = workspace_list_body_rect(area, false);
    if body.width == 0 || body.height == 0 {
        return 0;
    }

    let mut used_rows = 0u16;
    let mut visible = 0usize;
    let entries = workspace_list_entries(app);
    for (entry_idx, entry) in entries.iter().enumerate().skip(scroll) {
        let (row_height, gap) = match entry {
            WorkspaceListEntry::Workspace { ws_idx, indented } => {
                let Some(ws) = app.workspaces.get(*ws_idx) else {
                    continue;
                };
                (
                    workspace_row_height_in_body(app, ws, *indented, body.height),
                    workspace_entry_gap(&entries, entry_idx, *indented),
                )
            }
            // Each non-selectable layout row consumes exactly one row, tight (no gap).
            WorkspaceListEntry::Divider { .. } | WorkspaceListEntry::HostBanner { .. } => (1, 0),
        };
        if used_rows.saturating_add(row_height) > body.height {
            break;
        }
        used_rows = used_rows.saturating_add(row_height);
        visible += 1;
        if gap > 0 && used_rows < body.height {
            used_rows = used_rows.saturating_add(1);
        }
    }
    visible
}

fn workspace_list_bottom_start(app: &AppState, area: Rect) -> usize {
    let body = workspace_list_body_rect(area, false);
    let entries = workspace_list_entries(app);
    let mut used_rows = 0u16;
    let mut start = entries.len();
    for (entry_idx, entry) in entries.iter().enumerate().rev() {
        let needed = match entry {
            WorkspaceListEntry::Workspace { ws_idx, indented } => {
                let Some(workspace) = app.workspaces.get(*ws_idx) else {
                    continue;
                };
                let gap = workspace_entry_gap(&entries, entry_idx, *indented);
                workspace_row_height_in_body(app, workspace, *indented, body.height)
                    .saturating_add(gap)
            }
            // Each non-selectable layout row consumes exactly one row, tight (no gap).
            WorkspaceListEntry::Divider { .. } | WorkspaceListEntry::HostBanner { .. } => 1,
        };
        if used_rows.saturating_add(needed) > body.height {
            break;
        }
        used_rows = used_rows.saturating_add(needed);
        start = entry_idx;
    }
    start.min(entries.len().saturating_sub(1))
}

pub(crate) fn workspace_list_scroll_metrics(
    app: &AppState,
    area: Rect,
) -> crate::pane::ScrollMetrics {
    let max_scroll = workspace_list_bottom_start(app, area);
    let scroll = app.workspace_scroll.min(max_scroll);
    let viewport_rows = workspace_list_visible_count(app, area, scroll);

    crate::pane::ScrollMetrics {
        offset_from_bottom: max_scroll.saturating_sub(scroll),
        max_offset_from_bottom: max_scroll,
        viewport_rows,
    }
}

pub(crate) fn workspace_list_scrollbar_rect(app: &AppState, area: Rect) -> Option<Rect> {
    let metrics = workspace_list_scroll_metrics(app, area);
    let body = workspace_list_body_rect(area, true);
    (should_show_scrollbar(metrics) && body.width > 0 && body.height > 0).then_some(Rect::new(
        area.x + area.width.saturating_sub(1),
        body.y,
        1,
        body.height,
    ))
}

pub(crate) fn agent_panel_body_rect(area: Rect, has_scrollbar: bool) -> Rect {
    if area.width == 0 || area.height <= AGENT_PANEL_HEADER_ROWS {
        return Rect::default();
    }

    let body_y = area.y.saturating_add(AGENT_PANEL_HEADER_ROWS);
    let body_height = (area.y + area.height).saturating_sub(body_y);
    let body_width = area.width.saturating_sub(u16::from(has_scrollbar));
    Rect::new(area.x, body_y, body_width, body_height)
}

fn resolved_agent_rows(app: &AppState, entry: &AgentPanelEntry) -> Vec<Vec<ResolvedToken>> {
    let label = entry
        .state_labels
        .get(agent_panel_status_key(entry.state, entry.seen))
        .map(String::as_str)
        .unwrap_or_else(|| state_label(entry.state, entry.seen));
    tokens::agent_rows(&app.sidebar_agents, entry, label)
}

pub(crate) fn agent_entry_height_in_body(
    app: &AppState,
    entry: &AgentPanelEntry,
    body_height: u16,
) -> u16 {
    (resolved_agent_rows(app, entry)
        .len()
        .max(1)
        .min(u16::MAX as usize) as u16)
        .min(body_height)
}

fn agent_panel_visible_count_from(app: &AppState, area: Rect, scroll: usize) -> usize {
    let body = agent_panel_body_rect(area, false);
    if body.width == 0 || body.height == 0 {
        return 0;
    }

    let mut used_rows = 0u16;
    let mut visible = 0usize;
    for entry in agent_panel_entries(app).iter().skip(scroll) {
        let height = agent_entry_height_in_body(app, entry, body.height);
        if used_rows.saturating_add(height) > body.height {
            break;
        }
        used_rows = used_rows.saturating_add(height);
        visible += 1;
        if used_rows < body.height {
            used_rows = used_rows.saturating_add(1);
        }
    }
    visible
}

fn agent_panel_bottom_start(app: &AppState, area: Rect) -> usize {
    let body = agent_panel_body_rect(area, false);
    let entries = agent_panel_entries(app);
    let mut used_rows = 0u16;
    let mut start = entries.len();
    for (index, entry) in entries.iter().enumerate().rev() {
        let gap = u16::from(index + 1 < entries.len());
        let needed = agent_entry_height_in_body(app, entry, body.height).saturating_add(gap);
        if used_rows.saturating_add(needed) > body.height {
            break;
        }
        used_rows = used_rows.saturating_add(needed);
        start = index;
    }
    start.min(entries.len().saturating_sub(1))
}

pub(crate) fn agent_panel_scroll_for_target(
    app: &AppState,
    area: Rect,
    current_scroll: usize,
    target: usize,
) -> usize {
    let max_scroll = agent_panel_bottom_start(app, area);
    if target < current_scroll {
        return target.min(max_scroll);
    }
    let mut scroll = current_scroll.min(max_scroll);
    while scroll < target {
        let visible = agent_panel_visible_count_from(app, area, scroll);
        if visible > 0 && target < scroll.saturating_add(visible) {
            break;
        }
        scroll += 1;
    }
    scroll.min(max_scroll)
}

pub(crate) fn agent_panel_scroll_metrics(app: &AppState, area: Rect) -> crate::pane::ScrollMetrics {
    let max_scroll = agent_panel_bottom_start(app, area);
    let scroll = app.agent_panel_scroll.min(max_scroll);
    let viewport_rows = agent_panel_visible_count_from(app, area, scroll);

    crate::pane::ScrollMetrics {
        offset_from_bottom: max_scroll.saturating_sub(scroll),
        max_offset_from_bottom: max_scroll,
        viewport_rows,
    }
}

pub(crate) fn agent_panel_scrollbar_rect(app: &AppState, area: Rect) -> Option<Rect> {
    let metrics = agent_panel_scroll_metrics(app, area);
    let body = agent_panel_body_rect(area, true);
    (should_show_scrollbar(metrics) && body.width > 0 && body.height > 0).then_some(Rect::new(
        area.x + area.width.saturating_sub(1),
        body.y,
        1,
        body.height,
    ))
}

pub(crate) fn compute_workspace_list_areas(
    app: &AppState,
    area: Rect,
) -> (
    Vec<crate::app::state::WorkspaceCardArea>,
    Vec<HostBannerArea>,
) {
    let (cards, banner_areas, _divider_rows) = compute_workspace_list_areas_full(app, area);
    (cards, banner_areas)
}

/// Single-pass producer of every workspace-list row geometry: workspace card rects, host
/// banner rects, and divider rows. Render AND hit-test both consume the outputs of this ONE
/// pass, so the divider's `y` can never drift from the card geometry (render == hit_test
/// invariant). The two-tuple `compute_workspace_list_areas` and the `.0`-only
/// `compute_workspace_card_areas` delegate to this; the client compositor assigns all three
/// view channels from one call.
pub(crate) fn compute_workspace_list_areas_full(
    app: &AppState,
    area: Rect,
) -> (
    Vec<crate::app::state::WorkspaceCardArea>,
    Vec<HostBannerArea>,
    Vec<u16>,
) {
    let ws_area = workspace_list_rect(area, app.sidebar_section_split);
    if ws_area == Rect::default() {
        return (Vec::new(), Vec::new(), Vec::new());
    }

    let metrics = workspace_list_scroll_metrics(app, ws_area);
    let body = workspace_list_body_rect(ws_area, should_show_scrollbar(metrics));
    if body.width == 0 || body.height == 0 {
        return (Vec::new(), Vec::new(), Vec::new());
    }

    let scroll = app.workspace_scroll;
    let mut row_y = body.y;
    let body_bottom = body.y + body.height;
    let mut cards = Vec::new();
    let mut banner_areas: Vec<HostBannerArea> = Vec::new();
    let mut divider_rows: Vec<u16> = Vec::new();

    let entries = workspace_list_entries(app);
    for (entry_idx, entry) in entries.iter().enumerate().skip(scroll) {
        match entry {
            WorkspaceListEntry::Workspace { ws_idx, indented } => {
                let Some(ws) = app.workspaces.get(*ws_idx) else {
                    continue;
                };
                let row_height = workspace_row_height_in_body(app, ws, *indented, body.height);
                let gap = workspace_entry_gap(&entries, entry_idx, *indented);
                if row_y.saturating_add(row_height) > body_bottom {
                    break;
                }
                cards.push(crate::app::state::WorkspaceCardArea {
                    ws_idx: *ws_idx,
                    rect: Rect::new(body.x, row_y, body.width, row_height),
                    indented: *indented,
                });
                row_y = row_y.saturating_add(row_height);
                if gap > 0 && row_y < body_bottom {
                    row_y = row_y.saturating_add(1);
                }
            }
            // Advance one row, record the divider y, no card, no banner area (tight).
            WorkspaceListEntry::Divider { .. } => {
                let row_height = 1;
                if row_y.saturating_add(row_height) > body_bottom {
                    break;
                }
                divider_rows.push(row_y);
                row_y = row_y.saturating_add(row_height);
            }
            // Advance one row AND push a banner area (tight, no gap).
            WorkspaceListEntry::HostBanner { banner_idx } => {
                let row_height = 1;
                if row_y.saturating_add(row_height) > body_bottom {
                    break;
                }
                banner_areas.push(HostBannerArea {
                    banner_idx: *banner_idx,
                    rect: Rect::new(body.x, row_y, body.width, row_height),
                });
                row_y = row_y.saturating_add(row_height);
            }
        }
    }

    (cards, banner_areas, divider_rows)
}

pub(crate) fn compute_workspace_card_areas(
    app: &AppState,
    area: Rect,
) -> Vec<crate::app::state::WorkspaceCardArea> {
    compute_workspace_list_areas(app, area).0
}

/// Auto-scale sidebar width based on workspace identity + agent summary.
pub(crate) fn collapsed_sidebar_sections(area: Rect) -> (Rect, Option<u16>, Rect) {
    let content = Rect::new(area.x, area.y, area.width.saturating_sub(1), area.height);
    if content.width == 0 || content.height == 0 {
        return (Rect::default(), None, Rect::default());
    }

    if content.height < 7 {
        return (content, None, Rect::default());
    }

    let total_h = content.height as usize;
    let ws_h = total_h.div_ceil(2);
    let detail_h = total_h.saturating_sub(ws_h + 1);
    if ws_h == 0 || detail_h == 0 {
        return (content, None, Rect::default());
    }

    let divider_y = content.y + ws_h as u16;
    let ws_area = Rect::new(content.x, content.y, content.width, ws_h as u16);
    let detail_area = Rect::new(content.x, divider_y + 1, content.width, detail_h as u16);
    (ws_area, Some(divider_y), detail_area)
}

/// Collapsed sidebar: workspace glance on top, compact agent list below.
pub(crate) fn render_sidebar_collapsed(app: &AppState, frame: &mut Frame, area: Rect) {
    if area.width == 0 || area.height == 0 {
        return;
    }

    let is_navigating = matches!(app.mode, Mode::Navigate);

    let p = &app.palette;
    let sep_style = if is_navigating {
        Style::default().fg(p.accent)
    } else {
        Style::default().fg(p.surface_dim)
    };
    let sep_x = area.x + area.width.saturating_sub(1);
    let buf = frame.buffer_mut();
    for y in area.y..area.y + area.height {
        buf[(sep_x, y)].set_symbol("│");
        buf[(sep_x, y)].set_style(sep_style);
    }

    let (ws_area, divider_y, detail_area) = collapsed_sidebar_sections(area);
    if ws_area == Rect::default() {
        render_sidebar_toggle(app, frame, area, true, p);
        return;
    }

    for (visible_idx, ws) in app.workspaces.iter().enumerate() {
        let y = ws_area.y + visible_idx as u16;
        if y >= ws_area.y + ws_area.height {
            break;
        }
        let (agg_state, agg_seen) = ws.aggregate_state(&app.terminals);
        let (icon, icon_style) = state_dot(agg_state, agg_seen, p);
        let is_selected = visible_idx == app.selected && is_navigating;
        let is_active = Some(visible_idx) == app.active;
        let row_style = if is_selected {
            Style::default().bg(p.surface0)
        } else if is_active {
            Style::default().bg(p.surface_dim)
        } else {
            Style::default()
        };
        let num_style = if is_selected {
            Style::default().fg(p.overlay1).bg(p.surface0)
        } else if is_active {
            Style::default().fg(p.text).bg(p.surface_dim)
        } else {
            Style::default().fg(p.overlay0)
        };

        if is_selected || is_active {
            let buf = frame.buffer_mut();
            for x in ws_area.x..ws_area.x + ws_area.width {
                buf[(x, y)].set_style(row_style);
            }
        }

        frame.render_widget(
            Paragraph::new(Line::from(vec![
                Span::styled(format!("{}", visible_idx + 1), num_style),
                Span::styled(" ", row_style),
                Span::styled(icon, icon_style),
            ])),
            Rect::new(ws_area.x, y, ws_area.width, 1),
        );
    }

    if let Some(divider_y) = divider_y {
        let buf = frame.buffer_mut();
        for x in ws_area.x..ws_area.x + ws_area.width {
            buf[(x, divider_y)].set_symbol("─");
            buf[(x, divider_y)].set_style(Style::default().fg(p.surface_dim));
        }
    }

    let detail_content_area = Rect::new(
        detail_area.x,
        detail_area.y,
        detail_area.width,
        detail_area.height.saturating_sub(1),
    );
    if detail_content_area != Rect::default() {
        for (detail_idx, detail) in agent_panel_entries(app).iter().enumerate() {
            let y = detail_content_area.y + detail_idx as u16;
            if y >= detail_content_area.y + detail_content_area.height {
                break;
            }
            let position = detail_idx + 1;
            let position_style = Style::default().fg(p.overlay0);
            let (icon, icon_style) = agent_icon(detail.state, detail.seen, app.spinner_tick, p);
            frame.render_widget(
                Paragraph::new(Line::from(vec![
                    Span::styled(format!("{position:<2}"), position_style),
                    Span::styled(icon, icon_style),
                ])),
                Rect::new(detail_content_area.x, y, detail_content_area.width, 1),
            );
        }
    }

    render_sidebar_toggle(app, frame, area, true, p);
}

pub(crate) fn workspace_drop_indicator_row(
    cards: &[crate::app::state::WorkspaceCardArea],
    area: Rect,
    insert_idx: usize,
) -> Option<u16> {
    if area.height == 0 {
        return None;
    }
    let list_bottom = area.y + area.height.saturating_sub(1);

    let first = cards.first()?;
    if insert_idx == first.ws_idx {
        return first.rect.y.checked_sub(1).filter(|y| *y < list_bottom);
    }

    if let Some(row) = cards
        .last()
        .filter(|card| insert_idx == card.ws_idx.saturating_add(1))
        .map(|card| card.rect.y.saturating_add(card.rect.height))
        .filter(|y| *y < list_bottom)
    {
        return Some(row);
    }

    if let Some(card) = cards.iter().find(|card| card.ws_idx == insert_idx) {
        return card.rect.y.checked_sub(1).filter(|y| *y < list_bottom);
    }

    None
}

/// Resolve a host insert position (`0..=banners.len()`) to the screen row the host drop
/// indicator draws on, mirroring `workspace_drop_indicator_row` but operating on host banner
/// rects. `banners[i]` is the i-th host's banner; an `insert_idx` of `i` draws just above
/// banner `i`, and `banners.len()` draws just below the last banner (end of the host list). The
/// returned row is therefore ALWAYS a host boundary — never inside a space block.
pub(crate) fn host_drop_indicator_row(
    banners: &[HostBannerArea],
    area: Rect,
    insert_idx: usize,
) -> Option<u16> {
    if area.height == 0 {
        return None;
    }
    let list_bottom = area.y + area.height.saturating_sub(1);

    if let Some(banner) = banners.get(insert_idx) {
        return banner.rect.y.checked_sub(1).filter(|y| *y < list_bottom);
    }
    // insert_idx == banners.len(): just below the last banner (end of the host list).
    if insert_idx == banners.len() {
        if let Some(last) = banners.last() {
            return last
                .rect
                .y
                .saturating_add(last.rect.height)
                .checked_sub(1)
                .filter(|y| *y < list_bottom);
        }
    }
    None
}

pub(crate) fn render_sidebar(
    app: &AppState,
    terminal_runtimes: &TerminalRuntimeRegistry,
    frame: &mut Frame,
    area: Rect,
) {
    let p = &app.palette;
    let is_navigating = matches!(app.mode, Mode::Navigate);
    let sep_style = if is_navigating {
        Style::default().fg(p.accent)
    } else {
        Style::default().fg(p.surface_dim)
    };

    let sep_x = area.x + area.width.saturating_sub(1);
    let buf = frame.buffer_mut();
    for y in area.y..area.y + area.height {
        buf[(sep_x, y)].set_symbol("│");
        buf[(sep_x, y)].set_style(sep_style);
    }

    let (ws_area, detail_area) = expanded_sidebar_sections(area, app.sidebar_section_split);

    render_workspace_list(app, terminal_runtimes, frame, ws_area, is_navigating);
    render_agent_detail(app, terminal_runtimes, frame, detail_area);
    render_sidebar_toggle(app, frame, area, false, p);
}

fn resolved_token_spans(
    resolved: &[ResolvedToken],
    state_icon: (&str, Style),
    state_text_style: Style,
    workspace_style: Style,
    secondary_style: Style,
    custom_style: Style,
    p: &Palette,
    max_width: usize,
) -> Vec<Span<'static>> {
    let fixed_widths = resolved
        .iter()
        .map(|token| match token {
            ResolvedToken::StateIcon => display_width(state_icon.0),
            ResolvedToken::GitStatus { ahead, behind } => {
                usize::from(*ahead > 0) * display_width(&format!("↑{ahead}"))
                    + usize::from(*behind > 0) * display_width(&format!("↓{behind}"))
                    + usize::from(*ahead > 0 && *behind > 0)
            }
            _ => 0,
        })
        .collect::<Vec<_>>();
    let flexible_widths = resolved
        .iter()
        .map(|token| match token {
            ResolvedToken::StateText(text)
            | ResolvedToken::Workspace(text)
            | ResolvedToken::Tab(text)
            | ResolvedToken::Pane(text)
            | ResolvedToken::Agent(text)
            | ResolvedToken::TerminalTitle(text)
            | ResolvedToken::Branch(text)
            | ResolvedToken::Custom(text) => display_width(text),
            _ => 0,
        })
        .collect::<Vec<_>>();
    let minimum_width = |active: &[bool]| {
        let indices = active
            .iter()
            .enumerate()
            .filter_map(|(index, active)| active.then_some(index))
            .collect::<Vec<_>>();
        let content = indices
            .iter()
            .map(|index| fixed_widths[*index] + usize::from(flexible_widths[*index] > 0))
            .sum::<usize>();
        let separators = indices
            .windows(2)
            .map(|pair| display_width(tokens::separator(&resolved[pair[0]], &resolved[pair[1]])))
            .sum::<usize>();
        content + separators
    };
    let mut active = resolved.iter().map(|_| true).collect::<Vec<_>>();
    if minimum_width(&active) > max_width {
        for (index, width) in flexible_widths.iter().enumerate() {
            if *width > 0 {
                active[index] = false;
            }
        }
        for index in (0..resolved.len()).rev() {
            if flexible_widths[index] == 0 {
                continue;
            }
            active[index] = true;
            if minimum_width(&active) > max_width {
                active[index] = false;
            }
        }
    }
    let visible_indices = active
        .iter()
        .enumerate()
        .filter_map(|(index, active)| active.then_some(index))
        .collect::<Vec<_>>();
    let separator_width = visible_indices
        .windows(2)
        .map(|pair| display_width(tokens::separator(&resolved[pair[0]], &resolved[pair[1]])))
        .sum::<usize>();
    let fixed_width = visible_indices
        .iter()
        .map(|index| fixed_widths[*index])
        .sum::<usize>();
    let mut budgets = flexible_widths
        .iter()
        .enumerate()
        .map(|(index, width)| usize::from(active[index] && *width > 0))
        .collect::<Vec<_>>();
    let minimum = budgets.iter().sum::<usize>();
    let mut remaining = max_width
        .saturating_sub(separator_width + fixed_width)
        .saturating_sub(minimum);
    while remaining > 0 {
        let mut grew = false;
        for (budget, width) in budgets.iter_mut().zip(&flexible_widths) {
            if *budget > 0 && *budget < *width {
                *budget += 1;
                remaining -= 1;
                grew = true;
                if remaining == 0 {
                    break;
                }
            }
        }
        if !grew {
            break;
        }
    }
    let mut spans = Vec::new();
    for (position, index) in visible_indices.iter().copied().enumerate() {
        let token = &resolved[index];
        if position > 0 {
            let previous = &resolved[visible_indices[position - 1]];
            spans.push(Span::styled(
                tokens::separator(previous, token),
                Style::default().fg(p.overlay0).add_modifier(Modifier::DIM),
            ));
        }
        match token {
            ResolvedToken::StateIcon => {
                spans.push(Span::styled(state_icon.0.to_string(), state_icon.1));
            }
            ResolvedToken::StateText(text) => {
                spans.push(Span::styled(
                    truncate_end(text, budgets[index]),
                    state_text_style,
                ));
            }
            ResolvedToken::Workspace(text) => {
                spans.push(Span::styled(
                    truncate_end(text, budgets[index]),
                    workspace_style,
                ));
            }
            ResolvedToken::Tab(text) | ResolvedToken::Pane(text) | ResolvedToken::Agent(text) => {
                spans.push(Span::styled(
                    truncate_end(text, budgets[index]),
                    secondary_style,
                ));
            }
            ResolvedToken::Branch(text) => {
                spans.push(Span::styled(
                    truncate_end(text, budgets[index]),
                    secondary_style,
                ));
            }
            ResolvedToken::GitStatus { ahead, behind } => {
                if *ahead > 0 {
                    spans.push(Span::styled(
                        format!("↑{ahead}"),
                        Style::default().fg(p.green),
                    ));
                }
                if *ahead > 0 && *behind > 0 {
                    spans.push(Span::raw(" "));
                }
                if *behind > 0 {
                    spans.push(Span::styled(
                        format!("↓{behind}"),
                        Style::default().fg(p.red),
                    ));
                }
            }
            ResolvedToken::TerminalTitle(text) | ResolvedToken::Custom(text) => {
                spans.push(Span::styled(
                    truncate_end(text, budgets[index]),
                    custom_style,
                ));
            }
        }
    }
    spans
}

fn render_workspace_list(
    app: &AppState,
    terminal_runtimes: &TerminalRuntimeRegistry,
    frame: &mut Frame,
    area: Rect,
    is_navigating: bool,
) {
    let p = &app.palette;
    let dragged_ws_idx = match app.drag.as_ref().map(|drag| &drag.target) {
        Some(crate::app::state::DragTarget::WorkspaceReorder { source_ws_idx, .. }) => {
            Some(*source_ws_idx)
        }
        _ => None,
    };
    let insertion_row = match app.drag.as_ref().map(|drag| &drag.target) {
        Some(crate::app::state::DragTarget::WorkspaceReorder {
            insert_idx: Some(insert_idx),
            ..
        }) => workspace_drop_indicator_row(&app.view.workspace_card_areas, area, *insert_idx),
        // A host drag draws the SAME accent drop line, but at a host boundary computed from
        // the banner rects only (never inside a space block).
        Some(crate::app::state::DragTarget::HostReorder {
            insert_idx: Some(insert_idx),
            ..
        }) => host_drop_indicator_row(&app.view.host_banner_areas, area, *insert_idx),
        _ => None,
    };
    // The dragged host's banner index (== its position in the ordered host list), so its
    // banner row dims while dragging — mirroring the dragged-workspace lift.
    let dragged_host_idx = match app.drag.as_ref().map(|drag| &drag.target) {
        Some(crate::app::state::DragTarget::HostReorder {
            source_host_idx, ..
        }) => Some(*source_host_idx),
        _ => None,
    };

    let list_bottom = area.y + area.height.saturating_sub(1);
    if area.height > 0 {
        frame.render_widget(
            Paragraph::new(Line::from(vec![Span::styled(
                " spaces",
                Style::default().fg(p.overlay0).add_modifier(Modifier::BOLD),
            )])),
            Rect::new(area.x, area.y, area.width, 1),
        );
    }

    let metrics = workspace_list_scroll_metrics(app, area);
    let scrollbar_rect = workspace_list_scrollbar_rect(app, area);
    let cards = &app.view.workspace_card_areas;

    for card in cards {
        let i = card.ws_idx;
        let ws = &app.workspaces[i];
        let row_y = card.rect.y;
        let row_height = card.rect.height;
        let selected = i == app.selected && is_navigating;
        let is_active = Some(i) == app.active;
        let is_dragged = dragged_ws_idx == Some(i);
        // A row is hovered when the mirrored hover target names this ws_idx. Hover is the
        // LOWEST-priority highlight (selection/drag/active always win) and never bolds (the
        // `name_style` gate below is untouched).
        let hovered = app.sidebar_hover
            == Some(crate::app::state::SidebarHoverTarget::Workspace { ws_idx: i });
        let highlighted = selected || is_active || is_dragged || hovered;
        let (agg_state, agg_seen) = ws.aggregate_state(&app.terminals);

        if highlighted {
            let bg = if selected {
                p.surface0
            } else if is_dragged {
                p.surface1
            } else if is_active {
                p.surface_dim
            } else {
                // hovered-only (the prior three arms are false) → subtle theme-derived lift.
                p.hover_bg()
            };
            let buf = frame.buffer_mut();
            for y in row_y..row_y + row_height {
                if y >= list_bottom {
                    break;
                }
                for x in card.rect.x..card.rect.x + card.rect.width {
                    buf[(x, y)].set_style(Style::default().bg(bg));
                }
            }
        }

        let name_style = if selected || is_active || is_dragged {
            Style::default().fg(p.text).add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(p.subtext0)
        };

        let label = ws.display_name_from(&app.terminals, terminal_runtimes);
        let display_label = if card.indented {
            grouped_child_display_label(&label, ws.branch().as_deref(), ws.custom_name.is_some())
        } else {
            label
        };
        let parent_group = (!card.indented)
            .then(|| workspace_parent_group_state(app, i))
            .flatten();
        let (display_state, display_seen) = parent_group
            .as_ref()
            .filter(|(_, collapsed)| *collapsed)
            .map(|(key, _)| space_aggregate_state(app, key))
            .unwrap_or((agg_state, agg_seen));
        let state_icon = state_dot(display_state, display_seen, p);
        let state_text_style = Style::default()
            .fg(state_label_color(display_state, display_seen, p))
            .add_modifier(Modifier::DIM);
        let branch_style = Style::default().fg(if selected || is_active {
            p.mauve
        } else {
            p.overlay0
        });
        let token_values = ws.metadata_tokens.values();
        let rows = tokens::space_rows(
            &app.sidebar_spaces,
            SpaceTokenContext {
                workspace: &display_label,
                branch: ws.branch().as_deref(),
                state_text: state_label(display_state, display_seen),
                ahead_behind: ws.git_ahead_behind(),
                tokens: &token_values,
                suppress_git_details: card.indented,
            },
        );

        for (row_index, resolved) in rows.iter().enumerate() {
            if row_index as u16 >= row_height || row_y + row_index as u16 >= list_bottom {
                break;
            }
            let mut spans = Vec::new();
            if row_index == 0 {
                if card.indented {
                    spans.push(Span::raw("   "));
                } else if let Some((_, collapsed)) = parent_group.as_ref() {
                    spans.push(Span::styled(
                        if *collapsed { "▸" } else { "▾" },
                        Style::default().fg(p.accent),
                    ));
                    spans.push(Span::raw(" "));
                } else {
                    spans.push(Span::raw(" "));
                }
            } else {
                spans.push(Span::raw(if card.indented { "     " } else { "   " }));
            }
            let prefix_width = if row_index == 0 {
                if card.indented {
                    3
                } else if parent_group.is_some() {
                    2
                } else {
                    1
                }
            } else if card.indented {
                5
            } else {
                3
            };
            spans.extend(resolved_token_spans(
                resolved,
                state_icon,
                state_text_style,
                name_style,
                branch_style,
                branch_style,
                p,
                card.rect.width.saturating_sub(prefix_width) as usize,
            ));
            frame.render_widget(
                Paragraph::new(Line::from(spans)),
                Rect::new(card.rect.x, row_y + row_index as u16, card.rect.width, 1),
            );
        }
    }

    // Draw each host banner at its rect from `app.view.host_banner_areas` (the SAME single
    // `compute_workspace_list_areas_full` pass that produced the card geometry, so render never
    // recomputes a banner y — render == hit_test). Content left→right: connection glyph, the
    // solid theme-accent host name (bold), an optional dim state suffix.
    for (host_idx, banner_area) in app.view.host_banner_areas.iter().enumerate() {
        let row_y = banner_area.rect.y;
        if row_y >= list_bottom {
            continue;
        }
        let Some(spec) = app.host_banners.get(banner_area.banner_idx) else {
            continue;
        };
        // Dim the dragged host's banner row while a host drag is live (mirrors the
        // dragged-workspace `surface1` lift). Banner index == host position in the ordered list.
        if dragged_host_idx == Some(host_idx) {
            let buf = frame.buffer_mut();
            for x in banner_area.rect.x..banner_area.rect.x + banner_area.rect.width {
                buf[(x, row_y)].set_style(Style::default().bg(p.surface1));
            }
        }
        let glyph_color = match spec.connection_state {
            crate::app::state::HostBannerState::Connected => p.accent,
            crate::app::state::HostBannerState::ProtocolMismatch
            | crate::app::state::HostBannerState::Disconnected => p.red,
            _ => p.overlay0,
        };
        let mut spans: Vec<Span<'static>> = vec![
            Span::styled(
                host_banner_glyph(spec.connection_state),
                Style::default().fg(glyph_color),
            ),
            Span::raw(" "),
            host_banner_name_span(&spec.display_name, p),
        ];
        if let Some(suffix) = host_banner_suffix(spec.connection_state) {
            spans.push(Span::styled(suffix, Style::default().fg(p.overlay0)));
        }
        frame.render_widget(
            Paragraph::new(Line::from(spans)),
            Rect::new(banner_area.rect.x, row_y, banner_area.rect.width, 1),
        );
    }

    // Draw the local→remote divider rule at each `y` from `app.view.divider_rows`. The `y`s
    // come from the SAME `compute_workspace_list_areas_full` pass that produced the card
    // geometry, so render never recomputes a y and can never drift from hit-test. Labeled mode
    // (no host banner yet) draws a centered `─ remote ─` rule; plain mode (the banner owns host
    // naming) draws an unbroken `─` rule. Both dim (`surface_dim`), matching the
    // collapsed-sidebar separator precedent.
    let divider_labeled = !app.host_banner_active;
    for &row_y in &app.view.divider_rows {
        if row_y >= list_bottom {
            continue;
        }
        let rule_right = scrollbar_rect
            .map(|rect| rect.x)
            .unwrap_or(area.x + area.width);
        if rule_right <= area.x {
            continue;
        }
        let rule_width = rule_right - area.x;
        let style = Style::default().fg(p.surface_dim);
        {
            let buf = frame.buffer_mut();
            for x in area.x..rule_right {
                buf[(x, row_y)].set_symbol("─");
                buf[(x, row_y)].set_style(style);
            }
        }
        if divider_labeled {
            const LABEL: &str = " remote ";
            let label_len = LABEL.chars().count() as u16;
            if rule_width > label_len {
                let label_x = area.x + (rule_width - label_len) / 2;
                frame.render_widget(
                    Paragraph::new(Line::from(Span::styled(LABEL, style))),
                    Rect::new(label_x, row_y, label_len, 1),
                );
            }
        }
    }

    if let Some(y) = insertion_row.filter(|y| *y < list_bottom) {
        let indicator_right = scrollbar_rect
            .map(|rect| rect.x)
            .unwrap_or(area.x + area.width);
        let buf = frame.buffer_mut();
        for x in area.x..indicator_right {
            buf[(x, y)].set_symbol("─");
            buf[(x, y)].set_style(Style::default().fg(p.accent));
        }
    }

    if let Some(track) = scrollbar_rect {
        render_scrollbar(frame, metrics, track, p.surface_dim, p.overlay0, "▕");
    }

    if app.mouse_capture && list_bottom > area.y {
        // Hovering an affordance lifts its fg overlay0 → subtext0 (the badge `●` color is
        // unchanged); the `app.mouse_capture` gate above also gates whether hover can ever
        // resolve `New`/`Menu` (the hover hit-test mirrors this draw gate).
        let new_fg = if app.sidebar_hover == Some(crate::app::state::SidebarHoverTarget::New) {
            p.subtext0
        } else {
            p.overlay0
        };
        let menu_fg = if app.sidebar_hover == Some(crate::app::state::SidebarHoverTarget::Menu) {
            p.subtext0
        } else {
            p.overlay0
        };
        let new_rect = app.sidebar_new_button_rect();
        frame.render_widget(
            Paragraph::new(Span::styled(" new", Style::default().fg(new_fg))),
            new_rect,
        );

        let menu_rect = app.global_launcher_rect();
        let menu_line = if app.global_menu_attention_badge_visible() {
            Line::from(vec![
                Span::styled(
                    "● ",
                    Style::default().fg(p.accent).add_modifier(Modifier::BOLD),
                ),
                Span::styled("menu", Style::default().fg(menu_fg)),
            ])
        } else {
            Line::from(vec![Span::styled("menu", Style::default().fg(menu_fg))])
        };
        frame.render_widget(
            Paragraph::new(menu_line).alignment(Alignment::Right),
            menu_rect,
        );
    }
}

fn render_agent_detail(
    app: &AppState,
    terminal_runtimes: &TerminalRuntimeRegistry,
    frame: &mut Frame,
    area: Rect,
) {
    let p = &app.palette;

    if area.height < 3 {
        return;
    }

    let sep_line = "─".repeat(area.width as usize);
    frame.render_widget(
        Paragraph::new(Span::styled(&sep_line, Style::default().fg(p.surface_dim))),
        Rect::new(area.x, area.y, area.width, 1),
    );

    frame.render_widget(
        Paragraph::new(Line::from(vec![Span::styled(
            " agents",
            Style::default().fg(p.overlay0).add_modifier(Modifier::BOLD),
        )])),
        Rect::new(area.x, area.y + 1, area.width, 1),
    );
    let toggle_rect = agent_panel_toggle_rect(area, app.agent_panel_sort);
    if toggle_rect != Rect::default() {
        // Sort-toggle hover lifts fg overlay0 → subtext0 (monolithic-only — the client
        // hover path has no ScopeToggle target).
        let toggle_fg =
            if app.sidebar_hover == Some(crate::app::state::SidebarHoverTarget::ScopeToggle) {
                p.subtext0
            } else {
                p.overlay0
            };
        frame.render_widget(
            Paragraph::new(Span::styled(
                agent_panel_sort_label(app.agent_panel_sort),
                Style::default().fg(toggle_fg).add_modifier(Modifier::BOLD),
            ))
            .alignment(Alignment::Right),
            toggle_rect,
        );
    }

    let details = agent_panel_entries_from(app, terminal_runtimes);
    let metrics = agent_panel_scroll_metrics(app, area);
    let scrollbar_rect = agent_panel_scrollbar_rect(app, area);
    let body = agent_panel_body_rect(area, should_show_scrollbar(metrics));
    if body == Rect::default() {
        return;
    }

    let mut row_y = body.y;
    let body_bottom = body.y + body.height;
    // `skip(agent_panel_scroll)` drops the leading entries, so recover the GLOBAL entry index
    // (`agent_panel_scroll + offset`) to compare against the client `AgentRoute { route_idx }`
    // (route_idx is the flat global index, stable across recompose).
    for (offset, detail) in details.iter().skip(app.agent_panel_scroll).enumerate() {
        let global_idx = app.agent_panel_scroll.saturating_add(offset);
        let label_color = state_label_color(detail.state, detail.seen, p);
        let rows = resolved_agent_rows(app, detail);
        let height = (rows.len().max(1) as u16).min(body.height);
        if row_y.saturating_add(height) > body_bottom {
            break;
        }

        let is_active = app.is_active_pane(detail.ws_idx, detail.tab_idx, detail.pane_id);
        // hover matches the monolithic pane-keyed variant OR the client route-index variant.
        // Active always wins; hover never bolds.
        let hovered = match app.sidebar_hover {
            Some(crate::app::state::SidebarHoverTarget::AgentMono {
                ws_idx,
                tab_idx,
                pane_id,
            }) => ws_idx == detail.ws_idx && tab_idx == detail.tab_idx && pane_id == detail.pane_id,
            Some(crate::app::state::SidebarHoverTarget::AgentRoute { route_idx }) => {
                route_idx == global_idx
            }
            _ => false,
        };
        let row_style = if is_active {
            Style::default().bg(p.surface_dim)
        } else if hovered {
            Style::default().bg(p.hover_bg())
        } else {
            Style::default()
        };
        let name_style = if is_active {
            Style::default().fg(p.text).add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(p.subtext0).add_modifier(Modifier::BOLD)
        };
        let status_style = if is_active {
            Style::default().fg(label_color)
        } else {
            Style::default().fg(label_color).add_modifier(Modifier::DIM)
        };
        let agent_style = Style::default().fg(p.overlay0).add_modifier(Modifier::DIM);
        let state_icon = agent_icon(detail.state, detail.seen, app.spinner_tick, p);

        for (row_index, resolved) in rows.iter().take(height as usize).enumerate() {
            let mut spans = vec![Span::raw(if row_index == 0 { " " } else { "   " })];
            spans.extend(resolved_token_spans(
                resolved,
                state_icon,
                status_style,
                name_style,
                agent_style,
                agent_style,
                p,
                body.width
                    .saturating_sub(if row_index == 0 { 1 } else { 3 }) as usize,
            ));
            frame.render_widget(
                Paragraph::new(Line::from(spans)).style(row_style),
                Rect::new(body.x, row_y + row_index as u16, body.width, 1),
            );
        }
        row_y = row_y.saturating_add(height);
        if row_y < body_bottom {
            row_y += 1;
        }
    }

    if let Some(track) = scrollbar_rect {
        render_scrollbar(frame, metrics, track, p.surface_dim, p.overlay0, "▕");
    }
}

pub(crate) fn collapsed_sidebar_toggle_rect(area: Rect) -> Rect {
    let bottom_y = area.y + area.height.saturating_sub(1);
    let content_w = area.width.saturating_sub(1);
    if content_w == 0 || area.height == 0 {
        return Rect::default();
    }
    let x = area.x + content_w / 2;
    Rect::new(x, bottom_y, 1, 1)
}

pub(crate) fn expanded_sidebar_toggle_rect(area: Rect) -> Rect {
    if area.width <= 1 || area.height == 0 {
        return Rect::default();
    }
    Rect::new(
        area.x + area.width.saturating_sub(2),
        area.y + area.height.saturating_sub(1),
        1,
        1,
    )
}

fn render_sidebar_toggle(
    app: &AppState,
    frame: &mut Frame,
    area: Rect,
    collapsed: bool,
    p: &Palette,
) {
    let toggle_area = if collapsed {
        collapsed_sidebar_toggle_rect(area)
    } else {
        expanded_sidebar_toggle_rect(area)
    };
    if toggle_area == Rect::default() {
        return;
    }
    let icon = if collapsed { "»" } else { "«" };
    let icon_style = if collapsed && app.global_menu_attention_badge_visible() {
        Style::default().fg(p.accent).add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(p.overlay0)
    };
    frame.render_widget(Paragraph::new(Span::styled(icon, icon_style)), toggle_area);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{detect::Agent, workspace::Workspace};
    use ratatui::{backend::TestBackend, Terminal};

    fn row_text(buffer: &ratatui::buffer::Buffer, row: u16, width: u16) -> String {
        (0..width)
            .map(|x| buffer[(x, row)].symbol())
            .collect::<String>()
            .trim_end()
            .to_string()
    }

    #[test]
    fn default_agent_rows_remove_redundant_state_text() {
        let mut app = crate::app::state::AppState::test_new();
        let workspace = Workspace::test_new("one");
        let pane_id = workspace.tabs[0].root_pane;
        app.workspaces = vec![workspace];
        app.ensure_test_terminals();
        let terminal_id = app.workspaces[0].tabs[0].panes[&pane_id]
            .attached_terminal_id
            .clone();
        let terminal_state = app.terminals.get_mut(&terminal_id).unwrap();
        terminal_state.detected_agent = Some(Agent::Pi);
        terminal_state.state = AgentState::Working;

        let area = Rect::new(0, 0, 26, 20);
        let mut terminal = Terminal::new(TestBackend::new(26, 20)).unwrap();
        terminal
            .draw(|frame| render_sidebar(&app, &TerminalRuntimeRegistry::new(), frame, area))
            .unwrap();
        let buffer = terminal.backend().buffer();
        let (_, agent_area) = expanded_sidebar_sections(area, app.sidebar_section_split);
        let body = agent_panel_body_rect(agent_area, false);

        let first = row_text(buffer, body.y, 25);
        let second = row_text(buffer, body.y + 1, 25);
        assert!(first.contains("one"));
        assert_eq!(second, "   pi");
        assert!(!first.contains("working"));
        assert!(!second.contains("working"));
    }

    #[test]
    fn narrow_agent_rows_preserve_later_tab_tokens() {
        let mut app = crate::app::state::AppState::test_new();
        let mut workspace = Workspace::test_new("very-long-workspace-name");
        let tab_idx = workspace.test_add_tab(Some("logs"));
        let pane_id = workspace.tabs[tab_idx].root_pane;
        app.workspaces = vec![workspace];
        app.ensure_test_terminals();
        let terminal_id = app.workspaces[0].tabs[tab_idx].panes[&pane_id]
            .attached_terminal_id
            .clone();
        app.terminals.get_mut(&terminal_id).unwrap().detected_agent = Some(Agent::Pi);

        let area = Rect::new(0, 0, 18, 20);
        let mut terminal = Terminal::new(TestBackend::new(18, 20)).unwrap();
        terminal
            .draw(|frame| render_sidebar(&app, &TerminalRuntimeRegistry::new(), frame, area))
            .unwrap();
        let buffer = terminal.backend().buffer();
        let (_, agent_area) = expanded_sidebar_sections(area, app.sidebar_section_split);
        let body = agent_panel_body_rect(agent_area, false);
        let first = row_text(buffer, body.y, 17);

        assert!(first.contains("logs"), "rendered row: {first:?}");
        assert!(first.contains('·'), "rendered row: {first:?}");
    }

    #[test]
    fn stripped_terminal_title_renders_with_unicode_width_truncation() {
        let mut app = crate::app::state::AppState::test_new();
        let workspace = Workspace::test_new("one");
        let pane_id = workspace.tabs[0].root_pane;
        app.workspaces = vec![workspace];
        app.ensure_test_terminals();
        let terminal_id = app.workspaces[0].tabs[0].panes[&pane_id]
            .attached_terminal_id
            .clone();
        let terminal = app.terminals.get_mut(&terminal_id).unwrap();
        terminal.detected_agent = Some(Agent::Claude);
        terminal.set_terminal_title(Some("⠋ 修复🙂标题很长".into()));
        app.sidebar_agents.rows = vec![vec![
            crate::config::AgentSidebarToken::TerminalTitleStripped,
        ]];

        let area = Rect::new(0, 0, 10, 12);
        let mut renderer = Terminal::new(TestBackend::new(10, 12)).unwrap();
        renderer
            .draw(|frame| render_sidebar(&app, &TerminalRuntimeRegistry::new(), frame, area))
            .unwrap();
        let (_, agent_area) = expanded_sidebar_sections(area, app.sidebar_section_split);
        let body = agent_panel_body_rect(agent_area, false);
        let rendered = row_text(renderer.backend().buffer(), body.y, 9);

        assert!(!rendered.contains('⠋'));
        assert!(rendered.contains('修') && rendered.contains('复'));

        let spans = resolved_token_spans(
            &[ResolvedToken::TerminalTitle("修复🙂标题很长".into())],
            ("", Style::default()),
            Style::default(),
            Style::default(),
            Style::default(),
            Style::default(),
            &app.palette,
            8,
        );
        let text = spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect::<String>();
        assert!(display_width(&text) <= 8, "resolved title: {text:?}");
    }

    #[test]
    fn variable_agent_heights_pack_the_bottom_and_reveal_targets() {
        let mut app = crate::app::state::AppState::test_new();
        app.workspaces = vec![
            Workspace::test_new("one"),
            Workspace::test_new("two"),
            Workspace::test_new("three"),
        ];
        app.ensure_test_terminals();
        for workspace in &app.workspaces {
            let pane_id = workspace.tabs[0].root_pane;
            let terminal_id = workspace.tabs[0].panes[&pane_id]
                .attached_terminal_id
                .clone();
            app.terminals.get_mut(&terminal_id).unwrap().detected_agent = Some(Agent::Pi);
        }
        let first_pane = app.workspaces[0].tabs[0].root_pane;
        let first_terminal = app.workspaces[0].tabs[0].panes[&first_pane]
            .attached_terminal_id
            .clone();
        app.terminals
            .get_mut(&first_terminal)
            .unwrap()
            .metadata_tokens
            .patch(
                std::collections::HashMap::from([
                    ("a".into(), Some("a".into())),
                    ("b".into(), Some("b".into())),
                ]),
                None,
                std::time::Instant::now(),
            );
        app.sidebar_agents.rows = vec![
            vec![crate::config::AgentSidebarToken::Agent],
            vec![crate::config::AgentSidebarToken::Custom("a".into())],
            vec![crate::config::AgentSidebarToken::Custom("b".into())],
        ];
        let area = Rect::new(0, 0, 20, 6);

        let metrics = agent_panel_scroll_metrics(&app, area);
        assert_eq!(metrics.max_offset_from_bottom, 1);
        assert_eq!(agent_panel_scroll_for_target(&app, area, 0, 2), 1);
    }

    #[test]
    fn oversized_space_layout_is_clipped_to_the_section_body() {
        let mut app = crate::app::state::AppState::test_new();
        app.workspaces = vec![Workspace::test_new("one"), Workspace::test_new("two")];
        app.sidebar_spaces.rows = vec![vec![crate::config::SpaceSidebarToken::Workspace]; 6];
        let area = Rect::new(0, 0, 20, 10);
        let workspace_area = workspace_list_rect(area, app.sidebar_section_split);
        let body = workspace_list_body_rect(workspace_area, false);

        let metrics = workspace_list_scroll_metrics(&app, workspace_area);
        let (cards, _) = compute_workspace_list_areas(&app, area);

        assert_eq!(metrics.viewport_rows, 1);
        assert_eq!(cards.len(), 1);
        assert_eq!(cards[0].ws_idx, 0);
        assert_eq!(cards[0].rect.height, body.height);
    }

    #[test]
    fn oversized_agent_override_is_clipped_to_the_panel_body() {
        let mut app = crate::app::state::AppState::test_new();
        let workspace = Workspace::test_new("one");
        let pane_id = workspace.tabs[0].root_pane;
        app.workspaces = vec![workspace];
        app.ensure_test_terminals();
        let terminal_id = app.workspaces[0].tabs[0].panes[&pane_id]
            .attached_terminal_id
            .clone();
        app.terminals.get_mut(&terminal_id).unwrap().detected_agent = Some(Agent::Claude);
        app.sidebar_agents.rows_by_agent.insert(
            "claude".into(),
            vec![vec![crate::config::AgentSidebarToken::Agent]; 6],
        );
        let panel = Rect::new(0, 0, 20, 5);

        let metrics = agent_panel_scroll_metrics(&app, panel);

        assert_eq!(metrics.viewport_rows, 1);
        assert_eq!(metrics.max_offset_from_bottom, 0);
        let entry = agent_panel_entries(&app).pop().unwrap();
        assert_eq!(
            agent_entry_height_in_body(&app, &entry, agent_panel_body_rect(panel, false).height),
            agent_panel_body_rect(panel, false).height
        );
    }

    #[test]
    fn render_sidebar_toggle_draws_expanded_collapse_icon() {
        let app = crate::app::state::AppState::test_new();
        let area = Rect::new(0, 0, 26, 20);
        let mut terminal =
            Terminal::new(TestBackend::new(26, 20)).expect("test terminal should initialize");

        terminal
            .draw(|frame| render_sidebar_toggle(&app, frame, area, false, &app.palette))
            .expect("sidebar toggle should render");

        let toggle = expanded_sidebar_toggle_rect(area);
        assert_eq!(
            terminal.backend().buffer()[(toggle.x, toggle.y)].symbol(),
            "«"
        );
    }

    #[test]
    fn expanded_sidebar_toggle_sits_inside_sidebar_content() {
        let area = Rect::new(0, 0, 26, 20);
        let toggle = expanded_sidebar_toggle_rect(area);

        assert_eq!(toggle.x, area.x + area.width - 2);
        assert_eq!(toggle.y, area.y + area.height - 1);
    }

    #[test]
    fn all_workspaces_agent_panel_entries_use_workspace_and_optional_tab_labels() {
        let mut app = crate::app::state::AppState::test_new();
        let first = Workspace::test_new("one");
        let first_pane = first.tabs[0].root_pane;
        let mut second = Workspace::test_new("two");
        let second_tab = second.test_add_tab(Some("logs"));
        let second_pane = second.tabs[second_tab].root_pane;

        app.workspaces = vec![first, second];
        app.ensure_test_terminals();
        let first_terminal_id = app.workspaces[0].tabs[0].panes[&first_pane]
            .attached_terminal_id
            .clone();
        app.terminals
            .get_mut(&first_terminal_id)
            .unwrap()
            .detected_agent = Some(Agent::Pi);
        let second_terminal_id = app.workspaces[1].tabs[second_tab].panes[&second_pane]
            .attached_terminal_id
            .clone();
        app.terminals
            .get_mut(&second_terminal_id)
            .unwrap()
            .detected_agent = Some(Agent::Claude);
        app.active = Some(0);
        app.selected = 0;

        let entries = agent_panel_entries(&app);
        assert_eq!(entries[0].primary_label, "one");
        assert!(entries[0].primary_tab_label.is_none());
        assert_eq!(entries[0].agent_label.as_deref(), Some("pi"));
        assert_eq!(entries[1].primary_label, "two");
        assert_eq!(entries[1].primary_tab_label.as_deref(), Some("logs"));
        assert_eq!(entries[1].agent_label.as_deref(), Some("claude"));
    }

    #[test]
    fn priority_agent_panel_sort_uses_attention_then_space_order() {
        let mut app = crate::app::state::AppState::test_new();
        app.workspaces = vec![
            Workspace::test_new("one"),
            Workspace::test_new("two"),
            Workspace::test_new("three"),
            Workspace::test_new("four"),
        ];
        app.ensure_test_terminals();
        app.active = Some(0);
        app.selected = 0;
        app.agent_panel_sort = crate::app::state::AgentPanelSort::Priority;

        let set_state = |app: &mut crate::app::state::AppState, ws_idx: usize, state| {
            let pane = app.workspaces[ws_idx].tabs[0].root_pane;
            let terminal_id = app.workspaces[ws_idx].tabs[0].panes[&pane]
                .attached_terminal_id
                .clone();
            let terminal = app.terminals.get_mut(&terminal_id).unwrap();
            terminal.detected_agent = Some(Agent::Claude);
            terminal.state = state;
        };
        set_state(&mut app, 0, AgentState::Working);
        set_state(&mut app, 1, AgentState::Idle);
        set_state(&mut app, 2, AgentState::Working);
        set_state(&mut app, 3, AgentState::Blocked);

        let done_pane = app.workspaces[1].tabs[0].root_pane;
        app.workspaces[1].tabs[0]
            .panes
            .get_mut(&done_pane)
            .unwrap()
            .seen = false;

        let labels: Vec<String> = agent_panel_entries(&app)
            .into_iter()
            .map(|entry| entry.primary_label)
            .collect();

        assert_eq!(labels, ["four", "two", "one", "three"]);
    }

    #[test]
    fn collapsed_sidebar_numbers_grouped_agents_by_list_position() {
        let mut app = crate::app::state::AppState::test_new();
        app.workspaces = vec![Workspace::test_new("one"), Workspace::test_new("two")];
        app.ensure_test_terminals();

        for ws_idx in 0..app.workspaces.len() {
            let pane = app.workspaces[ws_idx].tabs[0].root_pane;
            let terminal_id = app.workspaces[ws_idx].tabs[0].panes[&pane]
                .attached_terminal_id
                .clone();
            app.terminals.get_mut(&terminal_id).unwrap().detected_agent = Some(Agent::Claude);
        }

        let area = Rect::new(0, 0, 4, 12);
        let (_, _, detail_area) = collapsed_sidebar_sections(area);
        let mut terminal = Terminal::new(TestBackend::new(area.width, area.height))
            .expect("test terminal should initialize");

        terminal
            .draw(|frame| render_sidebar_collapsed(&app, frame, area))
            .expect("collapsed sidebar should render");

        let buffer = terminal.backend().buffer();
        assert_eq!(buffer[(detail_area.x, detail_area.y)].symbol(), "1");
        assert_eq!(buffer[(detail_area.x, detail_area.y + 1)].symbol(), "2");
    }

    #[test]
    fn collapsed_sidebar_keeps_status_visible_for_two_digit_positions() {
        let mut app = crate::app::state::AppState::test_new();
        app.workspaces = (1..=10)
            .map(|idx| Workspace::test_new(&format!("workspace-{idx}")))
            .collect();
        app.ensure_test_terminals();

        for ws_idx in 0..app.workspaces.len() {
            let pane = app.workspaces[ws_idx].tabs[0].root_pane;
            let terminal_id = app.workspaces[ws_idx].tabs[0].panes[&pane]
                .attached_terminal_id
                .clone();
            app.terminals.get_mut(&terminal_id).unwrap().detected_agent = Some(Agent::Claude);
        }

        let area = Rect::new(0, 0, 4, 25);
        let (_, _, detail_area) = collapsed_sidebar_sections(area);
        let mut terminal = Terminal::new(TestBackend::new(area.width, area.height))
            .expect("test terminal should initialize");

        terminal
            .draw(|frame| render_sidebar_collapsed(&app, frame, area))
            .expect("collapsed sidebar should render");

        let tenth_row = detail_area.y + 9;
        let buffer = terminal.backend().buffer();
        assert_eq!(buffer[(detail_area.x, tenth_row)].symbol(), "1");
        assert_eq!(buffer[(detail_area.x + 1, tenth_row)].symbol(), "0");
        assert_eq!(buffer[(detail_area.x + 2, tenth_row)].symbol(), "○");
    }

    #[test]
    fn collapsed_sidebar_numbers_priority_agents_by_list_position() {
        let first = Workspace::test_new("one");
        let first_pane = first.tabs[0].root_pane;
        let mut second = Workspace::test_new("two");
        let second_pane = second.tabs[0].root_pane;
        let urgent_pane = second.test_split(ratatui::layout::Direction::Horizontal);

        let mut app = crate::app::state::AppState::test_new();
        app.workspaces = vec![first, second];
        app.ensure_test_terminals();
        app.agent_panel_sort = crate::app::state::AgentPanelSort::Priority;

        let set_state = |app: &mut crate::app::state::AppState, ws_idx: usize, pane_id, state| {
            let terminal_id = app.workspaces[ws_idx].tabs[0].panes[&pane_id]
                .attached_terminal_id
                .clone();
            let terminal = app.terminals.get_mut(&terminal_id).unwrap();
            terminal.detected_agent = Some(Agent::Claude);
            terminal.state = state;
        };
        set_state(&mut app, 0, first_pane, AgentState::Working);
        set_state(&mut app, 1, second_pane, AgentState::Working);
        set_state(&mut app, 1, urgent_pane, AgentState::Blocked);

        assert_eq!(app.workspaces[1].public_pane_number(urgent_pane), Some(2));
        assert_eq!(agent_panel_entries(&app)[0].pane_id, urgent_pane);

        let area = Rect::new(0, 0, 4, 16);
        let (_, _, detail_area) = collapsed_sidebar_sections(area);
        let mut terminal = Terminal::new(TestBackend::new(area.width, area.height))
            .expect("test terminal should initialize");

        terminal
            .draw(|frame| render_sidebar_collapsed(&app, frame, area))
            .expect("collapsed sidebar should render");

        let buffer = terminal.backend().buffer();
        assert_eq!(buffer[(detail_area.x, detail_area.y)].symbol(), "1");
        assert_eq!(buffer[(detail_area.x, detail_area.y + 1)].symbol(), "2");
        assert_eq!(buffer[(detail_area.x, detail_area.y + 2)].symbol(), "3");
        assert_eq!(buffer[(detail_area.x + 2, detail_area.y)].symbol(), "◉");
        assert_eq!(
            buffer[(detail_area.x + 2, detail_area.y)].style().fg,
            Some(app.palette.red)
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn all_workspaces_agent_panel_entries_use_live_root_runtime_cwd_for_workspace_label() {
        let unique = format!(
            "herdr-agent-panel-runtime-cwd-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        );
        let root = std::env::temp_dir().join(unique);
        let stale_cwd = root.join("issue-264-nix-support");
        let live_cwd = root.join("herdr");
        std::fs::create_dir_all(stale_cwd.join(".git")).unwrap();
        std::fs::create_dir_all(live_cwd.join(".git")).unwrap();

        let mut app = crate::app::state::AppState::test_new();
        let mut workspace = Workspace::test_new("stale-name");
        workspace.custom_name = None;
        workspace.identity_cwd = stale_cwd.clone();
        let pane = workspace.tabs[0].root_pane;

        app.workspaces = vec![workspace];
        app.ensure_test_terminals();
        let terminal_id = app.workspaces[0].tabs[0].panes[&pane]
            .attached_terminal_id
            .clone();
        let terminal = app.terminals.get_mut(&terminal_id).unwrap();
        terminal.cwd = stale_cwd;
        terminal.detected_agent = Some(Agent::Pi);
        app.active = Some(0);
        app.selected = 0;

        let (events, _) = tokio::sync::mpsc::channel(4);
        let runtime = crate::terminal::TerminalRuntime::spawn(
            pane,
            24,
            80,
            live_cwd.clone(),
            0,
            crate::terminal_theme::TerminalTheme::default(),
            crate::pane::PaneShellConfig::new("/bin/sh", crate::config::ShellModeConfig::NonLogin),
            &crate::pane::PaneLaunchEnv::default(),
            events,
            std::sync::Arc::new(tokio::sync::Notify::new()),
            std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
        )
        .unwrap();

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        while runtime.cwd() != Some(live_cwd.clone()) && std::time::Instant::now() < deadline {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }

        let mut runtime_registry = TerminalRuntimeRegistry::new();
        runtime_registry.insert(terminal_id, runtime);
        let entries = agent_panel_entries_from(&app, &runtime_registry);
        let primary_label = entries[0].primary_label.clone();

        for (_, runtime) in runtime_registry.drain() {
            runtime.shutdown();
        }
        let _ = std::fs::remove_dir_all(root);

        assert_eq!(primary_label, "herdr");
    }

    #[test]
    fn all_workspaces_agent_panel_entries_prefer_agent_names_for_agent_identity() {
        let mut app = crate::app::state::AppState::test_new();
        let workspace = Workspace::test_new("bridge");
        let first_pane = workspace.tabs[0].root_pane;

        app.workspaces = vec![workspace];
        app.ensure_test_terminals();
        let first_terminal_id = app.workspaces[0].tabs[0].panes[&first_pane]
            .attached_terminal_id
            .clone();
        app.terminals
            .get_mut(&first_terminal_id)
            .unwrap()
            .detected_agent = Some(Agent::Pi);
        app.terminals
            .get_mut(&first_terminal_id)
            .unwrap()
            .set_agent_name("planner".into());
        app.active = Some(0);
        app.selected = 0;

        let entries = agent_panel_entries(&app);
        assert_eq!(entries[0].primary_label, "bridge");
        assert_eq!(entries[0].agent_label.as_deref(), Some("planner"));
    }

    #[test]
    fn expanded_sidebar_sections_handle_tiny_heights() {
        let (ws_area, detail_area) = expanded_sidebar_sections(Rect::new(0, 0, 20, 5), 0.9);

        assert_eq!(ws_area, Rect::new(0, 0, 19, 3));
        assert_eq!(detail_area, Rect::new(0, 3, 19, 2));
    }

    #[test]
    fn sidebar_section_divider_is_hidden_for_tiny_heights() {
        let divider = sidebar_section_divider_rect(Rect::new(0, 0, 20, 5), 0.5);

        assert_eq!(divider, Rect::default());
    }

    #[test]
    fn grouped_child_label_keeps_custom_workspace_name() {
        assert_eq!(
            grouped_child_display_label("renamed issue", Some("worktree/issue-137"), true),
            "renamed issue"
        );
    }

    #[test]
    fn grouped_child_label_uses_short_branch_for_auto_named_workspace() {
        assert_eq!(
            grouped_child_display_label("herdr-issue", Some("worktree/issue-137"), false),
            "issue-137"
        );
    }

    #[test]
    fn workspace_list_truncates_cjk_branch_without_panic() {
        let mut app = crate::app::state::AppState::test_new();
        let mut ws = Workspace::test_new("repo");
        ws.cached_git_branch = Some("feature/中文-分支-644".into());
        app.workspaces = vec![ws];
        app.active = Some(0);
        app.selected = 0;
        app.mode = Mode::Terminal;
        app.view.workspace_card_areas = vec![crate::app::state::WorkspaceCardArea {
            ws_idx: 0,
            rect: Rect::new(0, 1, 15, 2),
            indented: false,
        }];

        let mut terminal = Terminal::new(TestBackend::new(15, 6)).expect("test terminal");
        let runtimes = crate::terminal::TerminalRuntimeRegistry::new();

        terminal
            .draw(|frame| {
                render_workspace_list(&app, &runtimes, frame, Rect::new(0, 0, 15, 6), false)
            })
            .expect("workspace list should render");
    }

    fn workspace_with_worktree_space(
        name: &str,
        key: Option<&str>,
        checkout_key: &str,
    ) -> crate::workspace::Workspace {
        let mut ws = crate::workspace::Workspace::test_new(name);
        if let Some(key) = key {
            ws.worktree_space = Some(crate::workspace::WorktreeSpaceMembership {
                key: key.into(),
                label: "herdr".into(),
                repo_root: std::path::PathBuf::from("/repo/herdr"),
                checkout_path: std::path::PathBuf::from(checkout_key),
                is_linked_worktree: name != "main",
            });
        }
        ws
    }

    fn workspace_with_git_space(name: &str, key: &str) -> crate::workspace::Workspace {
        let mut ws = crate::workspace::Workspace::test_new(name);
        ws.cached_git_space = Some(crate::workspace::GitSpaceMetadata {
            key: key.into(),
            checkout_key: format!("/repo/{name}"),
            label: "herdr".into(),
            repo_root: std::path::PathBuf::from(format!("/repo/{name}")),
            is_linked_worktree: false,
        });
        ws
    }

    #[test]
    fn parent_workspace_row_stays_clickable_when_grouped() {
        let mut app = AppState::test_new();
        app.workspaces = vec![
            workspace_with_worktree_space("main", Some("repo-key"), "/repo/herdr"),
            workspace_with_worktree_space("issue", Some("repo-key"), "/repo/herdr-issue"),
        ];

        let (cards, headers) = compute_workspace_list_areas(&app, Rect::new(0, 0, 30, 20));

        assert!(headers.is_empty());
        assert_eq!(cards[0].ws_idx, 0);
        assert!(!cards[0].indented);
        assert_eq!(cards[1].ws_idx, 1);
        assert!(cards[1].indented);
        assert_eq!(cards[1].rect.y, cards[0].rect.y + cards[0].rect.height + 1);
    }

    #[test]
    fn linked_only_worktree_members_do_not_form_parentless_group() {
        let mut app = AppState::test_new();
        app.workspaces = vec![
            workspace_with_worktree_space("issue", Some("repo-key"), "/repo/herdr-issue"),
            workspace_with_worktree_space("review", Some("repo-key"), "/repo/herdr-review"),
        ];

        let entries = workspace_list_entries(&app);

        assert_eq!(
            entries,
            vec![
                WorkspaceListEntry::Workspace {
                    ws_idx: 0,
                    indented: false
                },
                WorkspaceListEntry::Workspace {
                    ws_idx: 1,
                    indented: false
                },
            ]
        );
    }

    #[test]
    fn compact_space_group_scroll_clamps_when_all_entries_fit() {
        let mut app = AppState::test_new();
        app.workspaces = vec![
            workspace_with_worktree_space("main", Some("repo-key"), "/repo/herdr"),
            workspace_with_worktree_space("one", Some("repo-key"), "/repo/herdr-one"),
            workspace_with_worktree_space("two", Some("repo-key"), "/repo/herdr-two"),
        ];
        let area = Rect::new(0, 0, 30, 20);
        app.workspace_scroll = normalized_workspace_scroll(&app, area, 2);

        let (cards, headers) = compute_workspace_list_areas(&app, area);

        assert!(headers.is_empty());
        assert_eq!(app.workspace_scroll, 0);
        assert_eq!(cards.len(), 3);
        assert_eq!(cards[2].ws_idx, 2);
    }

    #[test]
    fn workspace_scroll_metrics_count_display_entries_not_raw_workspaces() {
        let mut app = AppState::test_new();
        app.workspaces = vec![
            workspace_with_worktree_space("main", Some("repo-key"), "/repo/herdr"),
            workspace_with_worktree_space("issue", Some("repo-key"), "/repo/herdr-issue"),
            Workspace::test_new("notes"),
        ];
        app.collapsed_space_keys.insert("repo-key".into());
        app.active = None;
        app.mode = Mode::Terminal;

        let ws_area = Rect::new(0, 0, 30, 6);
        let metrics = workspace_list_scroll_metrics(&app, ws_area);

        assert_eq!(metrics.viewport_rows, 1);
        assert_eq!(metrics.max_offset_from_bottom, 1);
        assert_eq!(metrics.offset_from_bottom, 1);
    }

    #[test]
    fn workspace_scroll_offset_applies_to_group_children() {
        let mut app = AppState::test_new();
        app.workspaces = vec![
            workspace_with_worktree_space("main", Some("repo-key"), "/repo/herdr"),
            workspace_with_worktree_space("issue", Some("repo-key"), "/repo/herdr-issue"),
            Workspace::test_new("notes"),
        ];
        app.collapsed_space_keys.insert("repo-key".into());
        app.active = None;
        app.mode = Mode::Terminal;
        app.workspace_scroll = 1;

        let (cards, headers) = compute_workspace_list_areas(&app, Rect::new(0, 0, 30, 12));

        assert!(headers.is_empty());
        assert_eq!(cards.len(), 1);
        assert_eq!(cards[0].ws_idx, 2);
    }

    #[test]
    fn workspace_list_entries_group_multiple_workspaces_in_same_git_space() {
        let mut app = AppState::test_new();
        app.workspaces = vec![
            workspace_with_worktree_space("main", Some("repo-key"), "/repo/herdr"),
            workspace_with_worktree_space("issue", Some("repo-key"), "/repo/herdr-issue"),
        ];

        assert_eq!(
            workspace_list_entries(&app),
            vec![
                WorkspaceListEntry::Workspace {
                    ws_idx: 0,
                    indented: false,
                },
                WorkspaceListEntry::Workspace {
                    ws_idx: 1,
                    indented: true,
                },
            ]
        );
    }

    #[test]
    fn workspace_list_entries_group_non_contiguous_explicit_members() {
        let mut app = AppState::test_new();
        app.workspaces = vec![
            workspace_with_worktree_space("main", Some("repo-key"), "/repo/herdr"),
            workspace_with_git_space("normal", "other-key"),
            workspace_with_worktree_space("issue", Some("repo-key"), "/repo/herdr-issue"),
        ];

        assert_eq!(
            workspace_list_entries(&app),
            vec![
                WorkspaceListEntry::Workspace {
                    ws_idx: 0,
                    indented: false,
                },
                WorkspaceListEntry::Workspace {
                    ws_idx: 2,
                    indented: true,
                },
                WorkspaceListEntry::Workspace {
                    ws_idx: 1,
                    indented: false,
                },
            ]
        );
    }

    #[test]
    fn workspace_list_entries_do_not_group_normal_git_workspaces() {
        let mut app = AppState::test_new();
        app.workspaces = vec![
            workspace_with_git_space("one", "repo-key"),
            workspace_with_git_space("two", "repo-key"),
        ];

        assert_eq!(
            workspace_list_entries(&app),
            vec![
                WorkspaceListEntry::Workspace {
                    ws_idx: 0,
                    indented: false,
                },
                WorkspaceListEntry::Workspace {
                    ws_idx: 1,
                    indented: false,
                },
            ]
        );
    }

    #[test]
    fn workspace_list_entries_do_not_auto_attach_normal_git_workspace_to_group() {
        let mut app = AppState::test_new();
        app.workspaces = vec![
            workspace_with_worktree_space("main", Some("repo-key"), "/repo/herdr"),
            workspace_with_git_space("scratch", "repo-key"),
            workspace_with_worktree_space("issue", Some("repo-key"), "/repo/herdr-issue"),
        ];

        assert_eq!(
            workspace_list_entries(&app),
            vec![
                WorkspaceListEntry::Workspace {
                    ws_idx: 0,
                    indented: false,
                },
                WorkspaceListEntry::Workspace {
                    ws_idx: 2,
                    indented: true,
                },
                WorkspaceListEntry::Workspace {
                    ws_idx: 1,
                    indented: false,
                },
            ]
        );
    }

    #[test]
    fn workspace_list_entries_leave_single_git_and_non_git_workspaces_flat() {
        let mut app = AppState::test_new();
        app.workspaces = vec![
            workspace_with_git_space("one", "repo-key"),
            workspace_with_worktree_space("notes", None, "/notes"),
        ];

        assert_eq!(
            workspace_list_entries(&app),
            vec![
                WorkspaceListEntry::Workspace {
                    ws_idx: 0,
                    indented: false,
                },
                WorkspaceListEntry::Workspace {
                    ws_idx: 1,
                    indented: false,
                },
            ]
        );
    }

    #[test]
    fn collapsed_group_hides_inactive_children_but_keeps_active_visible() {
        let mut app = AppState::test_new();
        app.workspaces = vec![
            workspace_with_worktree_space("main", Some("repo-key"), "/repo/herdr"),
            workspace_with_worktree_space("issue", Some("repo-key"), "/repo/herdr-issue"),
        ];
        app.active = Some(1);
        app.mode = Mode::Terminal;
        app.collapsed_space_keys.insert("repo-key".into());

        assert_eq!(
            workspace_list_entries(&app),
            vec![
                WorkspaceListEntry::Workspace {
                    ws_idx: 0,
                    indented: false,
                },
                WorkspaceListEntry::Workspace {
                    ws_idx: 1,
                    indented: true,
                },
            ]
        );

        app.active = None;
        app.mode = Mode::Terminal;
        assert_eq!(
            workspace_list_entries(&app),
            vec![WorkspaceListEntry::Workspace {
                ws_idx: 0,
                indented: false,
            }]
        );
    }

    #[test]
    fn collapsed_group_keeps_selected_child_visible_in_navigate_mode() {
        let mut app = AppState::test_new();
        app.workspaces = vec![
            workspace_with_worktree_space("main", Some("repo-key"), "/repo/herdr"),
            workspace_with_worktree_space("issue", Some("repo-key"), "/repo/herdr-issue"),
        ];
        app.mode = Mode::Navigate;
        app.selected = 1;
        app.active = Some(1);
        app.collapsed_space_keys.insert("repo-key".into());

        assert_eq!(
            workspace_list_entries(&app),
            vec![
                WorkspaceListEntry::Workspace {
                    ws_idx: 0,
                    indented: false,
                },
                WorkspaceListEntry::Workspace {
                    ws_idx: 1,
                    indented: true,
                },
            ]
        );
    }

    // ---- host banner readouts ----

    // ---- hover render precedence + distinct subtle style ----

    fn two_card_hover_app() -> AppState {
        let mut app = AppState::test_new();
        app.workspaces = vec![Workspace::test_new("alpha"), Workspace::test_new("beta")];
        app.view.workspace_card_areas = vec![
            crate::app::state::WorkspaceCardArea {
                ws_idx: 0,
                rect: Rect::new(0, 2, 40, 1),
                indented: false,
            },
            crate::app::state::WorkspaceCardArea {
                ws_idx: 1,
                rect: Rect::new(0, 4, 40, 1),
                indented: false,
            },
        ];
        app
    }

    fn render_workspace_list_buffer(
        app: &AppState,
        is_navigating: bool,
    ) -> ratatui::buffer::Buffer {
        let backend = TestBackend::new(40, 8);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| {
                render_workspace_list(
                    app,
                    &TerminalRuntimeRegistry::new(),
                    frame,
                    Rect::new(0, 0, 40, 8),
                    is_navigating,
                )
            })
            .unwrap();
        terminal.backend().buffer().clone()
    }

    #[test]
    fn render_selected_row_wins_over_hover() {
        use crate::app::state::SidebarHoverTarget;
        let mut app = two_card_hover_app();
        // ws 0 is BOTH selected (navigating) and hovered → selection must win (surface0 + bold);
        app.selected = 0;
        app.active = None;
        app.set_sidebar_hover(Some(SidebarHoverTarget::Workspace { ws_idx: 0 }));

        let buf = render_workspace_list_buffer(&app, true);
        let selected_cell = &buf[(1, 2)];
        assert_eq!(selected_cell.style().bg, Some(app.palette.surface0));
        assert_ne!(selected_cell.style().bg, Some(app.palette.hover_bg()));
        // selection bolds the label (the name_style gate is untouched by hover).
        let row0 = (0..40)
            .map(|x| (x, &buf[(x, 2)]))
            .find(|(_, c)| c.modifier.contains(Modifier::BOLD));
        assert!(
            row0.is_some(),
            "selected row should render a BOLD label span"
        );

        // ws 1 is hovered ONLY (not selected/active) → hover_bg, and NOT bold.
        let mut app = two_card_hover_app();
        app.selected = 0;
        app.active = None;
        app.set_sidebar_hover(Some(SidebarHoverTarget::Workspace { ws_idx: 1 }));
        let buf = render_workspace_list_buffer(&app, true);
        let hovered_cell = &buf[(1, 4)];
        assert_eq!(hovered_cell.style().bg, Some(app.palette.hover_bg()));
        assert_ne!(hovered_cell.style().bg, Some(app.palette.surface0));
        // hovered-only row must NOT bold its label.
        let any_bold_on_hover_row = (0..40).any(|x| buf[(x, 4)].modifier.contains(Modifier::BOLD));
        assert!(
            !any_bold_on_hover_row,
            "hovered-only row must not bold its label"
        );
    }

    #[test]
    fn render_active_row_wins_over_hover() {
        use crate::app::state::SidebarHoverTarget;
        // an active + hovered row renders the active surface_dim, not hover_bg (active wins).
        let mut app = two_card_hover_app();
        app.selected = 1;
        app.active = Some(0);
        app.set_sidebar_hover(Some(SidebarHoverTarget::Workspace { ws_idx: 0 }));
        let buf = render_workspace_list_buffer(&app, true);
        let active_cell = &buf[(1, 2)];
        assert_eq!(active_cell.style().bg, Some(app.palette.surface_dim));
        assert_ne!(active_cell.style().bg, Some(app.palette.hover_bg()));
    }

    #[test]
    fn render_agent_hover_uses_hover_bg_when_not_active() {
        use crate::app::state::SidebarHoverTarget;
        // a hovered, non-active agent row renders hover_bg (active wins over hover, so make this
        // row non-active by clearing `active`).
        let mut app = AppState::test_new();
        let ws = Workspace::test_new("test");
        let pane = ws.tabs[0].root_pane;
        app.workspaces = vec![ws];
        app.ensure_test_terminals();
        let terminal_id = app.workspaces[0].tabs[0].panes[&pane]
            .attached_terminal_id
            .clone();
        app.terminals.get_mut(&terminal_id).unwrap().detected_agent = Some(Agent::Claude);
        app.active = None;
        let details = agent_panel_entries_from(&app, &TerminalRuntimeRegistry::new());
        assert!(!details.is_empty(), "agent panel should have an entry");

        // hovered via the route-index (the flat global index of the first entry).
        app.agent_panel_scroll = 0;
        app.set_sidebar_hover(Some(SidebarHoverTarget::AgentRoute { route_idx: 0 }));

        let backend = TestBackend::new(40, 9);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| {
                render_agent_detail(
                    &app,
                    &TerminalRuntimeRegistry::new(),
                    frame,
                    Rect::new(0, 0, 40, 9),
                )
            })
            .unwrap();
        let buf = terminal.backend().buffer().clone();

        // the first agent row sits in the agent-panel body. Find a cell whose bg == hover_bg.
        let any_hover_bg = (0..9u16)
            .any(|y| (0..40u16).any(|x| buf[(x, y)].style().bg == Some(app.palette.hover_bg())));
        assert!(
            any_hover_bg,
            "hovered non-active agent row should render hover_bg"
        );
    }

    // Hover adds NO second animation clock: a hover redraw rides the existing redraw path and
    // reads `spinner_tick` only through the shared tick. Guard: this module introduces no
    // hover-specific animation clock. Needles are assembled at runtime so the guard does not
    // match itself.
    #[test]
    fn moved_event_cadence_no_second_clock() {
        let src = include_str!("sidebar.rs");
        let hover = "hover";
        let needles = [
            format!("{hover}_animation_interval"),
            format!("{hover}_tick"),
            format!("advance_{hover}_tick"),
        ];
        for needle in needles {
            assert!(
                !src.to_lowercase().contains(&needle),
                "hover must not introduce a second animation clock (found `{needle}`)"
            );
        }
    }

    // ---- local→remote space divider ----

    /// Build an ungrouped app where `client_workspace_remote[i]` aligns with `app.workspaces[i]`
    /// and the entry stream's `ws_idx` equals the array index (no worktree grouping). Each card
    /// is configured to a single render row so the divider geometry is deterministic.
    fn divider_app(remote: &[bool]) -> AppState {
        let mut app = AppState::test_new();
        app.sidebar_spaces.rows = vec![vec![
            crate::config::SpaceSidebarToken::StateIcon,
            crate::config::SpaceSidebarToken::Workspace,
        ]];
        app.workspaces = remote
            .iter()
            .enumerate()
            .map(|(i, _)| Workspace::test_new(&format!("ws{i}")))
            .collect();
        app.client_workspace_remote = remote.to_vec();
        app.active = None;
        app.mode = Mode::Terminal;
        app
    }

    fn divider_positions(entries: &[WorkspaceListEntry]) -> Vec<usize> {
        entries
            .iter()
            .enumerate()
            .filter_map(|(idx, entry)| {
                matches!(entry, WorkspaceListEntry::Divider { .. }).then_some(idx)
            })
            .collect()
    }

    #[test]
    fn workspace_list_entries_inserts_one_divider_at_local_remote_boundary() {
        let app = divider_app(&[false, false, true, true]);
        let entries = workspace_list_entries(&app);

        let dividers = divider_positions(&entries);
        assert_eq!(dividers.len(), 1, "exactly one divider: {entries:?}");
        // ws_idx 1 (local) then the divider then ws_idx 2 (remote).
        assert_eq!(
            entries[dividers[0] - 1],
            WorkspaceListEntry::Workspace {
                ws_idx: 1,
                indented: false
            }
        );
        assert_eq!(
            entries[dividers[0] + 1],
            WorkspaceListEntry::Workspace {
                ws_idx: 2,
                indented: false
            }
        );
        assert_eq!(
            entries[dividers[0]],
            WorkspaceListEntry::Divider { labeled: true }
        );
    }

    #[test]
    fn workspace_list_entries_no_divider_when_all_local() {
        let app = divider_app(&[false, false, false]);
        assert!(divider_positions(&workspace_list_entries(&app)).is_empty());
    }

    #[test]
    fn workspace_list_entries_no_divider_when_all_remote() {
        let app = divider_app(&[true, true, true]);
        assert!(divider_positions(&workspace_list_entries(&app)).is_empty());
    }

    #[test]
    fn workspace_list_entries_no_divider_when_marker_empty() {
        // Monolithic mode: client_workspace_remote is empty even though workspaces exist.
        let mut app = AppState::test_new();
        app.workspaces = vec![Workspace::test_new("a"), Workspace::test_new("b")];
        app.client_workspace_remote = Vec::new();
        app.active = None;
        app.mode = Mode::Terminal;
        assert!(divider_positions(&workspace_list_entries(&app)).is_empty());
    }

    #[test]
    fn workspace_list_entries_no_divider_single_server_filter() {
        // A single-server filter yields one uniform role group upstream, so the marker is
        // all-true or all-false and no transition exists.
        assert!(divider_positions(&workspace_list_entries(&divider_app(&[true, true]))).is_empty());
        assert!(
            divider_positions(&workspace_list_entries(&divider_app(&[false, false]))).is_empty()
        );
    }

    #[test]
    fn workspace_list_entries_divider_above_offline_remote_placeholder() {
        // Offline/empty-remote placeholder rows carry is_remote == true, so the divider sits
        // ABOVE them (the split shows before the remote finishes connecting).
        let app = divider_app(&[false, true]);
        let entries = workspace_list_entries(&app);
        let dividers = divider_positions(&entries);
        assert_eq!(dividers.len(), 1);
        // divider immediately precedes the first (placeholder) remote entry.
        assert_eq!(
            entries[dividers[0] + 1],
            WorkspaceListEntry::Workspace {
                ws_idx: 1,
                indented: false
            }
        );
    }

    #[test]
    fn workspace_list_entries_divider_on_visual_order_with_grouped_local_worktrees() {
        // Local grouped worktree members (parent + indented child) then a remote row: the
        // divider lands after the LAST local entry by VISUAL order, before the first remote.
        let mut app = AppState::test_new();
        app.workspaces = vec![
            workspace_with_worktree_space("main", Some("repo-key"), "/repo/herdr"),
            workspace_with_worktree_space("issue", Some("repo-key"), "/repo/herdr-issue"),
            Workspace::test_new("remote-space"),
        ];
        app.client_workspace_remote = vec![false, false, true];
        app.active = None;
        app.mode = Mode::Terminal;

        let entries = workspace_list_entries(&app);
        assert_eq!(
            entries,
            vec![
                WorkspaceListEntry::Workspace {
                    ws_idx: 0,
                    indented: false
                },
                WorkspaceListEntry::Workspace {
                    ws_idx: 1,
                    indented: true
                },
                WorkspaceListEntry::Divider { labeled: true },
                WorkspaceListEntry::Workspace {
                    ws_idx: 2,
                    indented: false
                },
            ]
        );
    }

    #[test]
    fn workspace_list_entries_divider_labeled_false_when_host_banner_active() {
        let mut app = divider_app(&[false, true]);
        app.host_banner_active = true;
        let entries = workspace_list_entries(&app);
        let dividers = divider_positions(&entries);
        assert_eq!(dividers.len(), 1);
        assert_eq!(
            entries[dividers[0]],
            WorkspaceListEntry::Divider { labeled: false }
        );
    }

    #[test]
    fn compute_workspace_list_areas_emits_no_card_for_divider() {
        let app = divider_app(&[false, false, true, true]);
        let (cards, _banners, divider_rows) =
            compute_workspace_list_areas_full(&app, Rect::new(0, 0, 30, 24));
        // One card per real workspace, none for the divider.
        assert_eq!(cards.len(), app.workspaces.len());
        assert_eq!(divider_rows.len(), 1);
        // No card rect overlaps the divider y.
        let divider_y = divider_rows[0];
        assert!(cards
            .iter()
            .all(|card| !(divider_y >= card.rect.y && divider_y < card.rect.y + card.rect.height)));
    }

    #[test]
    fn compute_workspace_list_areas_full_divider_row_consumes_one_row() {
        let app = divider_app(&[false, true]);
        let (cards, _banners, divider_rows) =
            compute_workspace_list_areas_full(&app, Rect::new(0, 0, 30, 24));
        assert_eq!(cards.len(), 2);
        assert_eq!(divider_rows.len(), 1);
        let last_local = &cards[0];
        let first_remote = &cards[1];
        let divider_y = divider_rows[0];
        // The divider consumes exactly one row: it sits one row below the last local card's
        // bottom (after the standard one-row inter-card gap), and the first remote card sits
        // exactly one row below the divider — a tight one-row separator.
        assert_eq!(divider_y, last_local.rect.y + last_local.rect.height + 1);
        assert_eq!(first_remote.rect.y, divider_y + 1);
    }

    #[test]
    fn divider_rows_match_render_and_hit_test_geometry() {
        // The divider y from the single compute pass equals the gap between adjacent card
        // rects, and the same geometry makes hit-test miss the divider row (render == hit_test).
        let mut app = divider_app(&[false, true]);
        let area = Rect::new(0, 0, 30, 20);
        app.view.sidebar_rect = area;
        let (cards, _banners, divider_rows) = compute_workspace_list_areas_full(&app, area);
        app.view.workspace_card_areas = cards;
        app.view.divider_rows = divider_rows;

        let divider_y = app.view.divider_rows[0];
        // No card covers the divider row.
        assert!(app
            .view
            .workspace_card_areas
            .iter()
            .all(|card| !(divider_y >= card.rect.y && divider_y < card.rect.y + card.rect.height)));
    }

    #[test]
    fn workspace_list_scroll_metrics_counts_divider_row() {
        let app = divider_app(&[false, true]);
        // Two real workspaces + one divider = three entry rows.
        assert_eq!(workspace_list_entries(&app).len(), 3);
        let metrics = workspace_list_scroll_metrics(&app, Rect::new(0, 0, 30, 20));
        assert_eq!(metrics.viewport_rows, 3);
    }

    #[test]
    fn scrolling_does_not_desync_card_ws_idx_with_divider_present() {
        let mut app = divider_app(&[false, false, true, true]);
        let area = Rect::new(0, 0, 30, 6);
        // Scroll past the first local workspace; every visible card's ws_idx must still map to
        // the correct workspace (the divider does not shift card ws_idx).
        app.workspace_scroll = normalized_workspace_scroll(&app, area, 2);
        let (cards, _banners, _divider_rows) = compute_workspace_list_areas_full(&app, area);
        for card in &cards {
            assert!(card.ws_idx < app.workspaces.len());
            // Cards keep their declared role per client_workspace_remote (no off-by-one).
            assert_eq!(
                app.client_workspace_remote[card.ws_idx],
                card.ws_idx >= 2,
                "card ws_idx {} role desynced",
                card.ws_idx
            );
        }
    }

    fn render_divider_buffer(app: &AppState, width: u16, height: u16) -> ratatui::buffer::Buffer {
        let backend = TestBackend::new(width, height);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| {
                render_workspace_list(
                    app,
                    &TerminalRuntimeRegistry::new(),
                    frame,
                    Rect::new(0, 0, width, height),
                    false,
                )
            })
            .unwrap();
        terminal.backend().buffer().clone()
    }

    fn buffer_row_text(buffer: &ratatui::buffer::Buffer, y: u16, width: u16) -> String {
        (0..width).map(|x| buffer[(x, y)].symbol()).collect()
    }

    fn prepared_divider_app(remote: &[bool], width: u16, height: u16) -> AppState {
        let mut app = divider_app(remote);
        let area = Rect::new(0, 0, width, height);
        app.view.sidebar_rect = area;
        let (cards, banners, divider_rows) = compute_workspace_list_areas_full(&app, area);
        app.view.workspace_card_areas = cards;
        app.view.host_banner_areas = banners;
        app.view.divider_rows = divider_rows;
        app
    }

    #[test]
    fn render_writes_dim_remote_rule_in_labeled_mode() {
        let app = prepared_divider_app(&[false, true], 30, 20);
        let divider_y = app.view.divider_rows[0];
        let buffer = render_divider_buffer(&app, 30, 20);

        let row = buffer_row_text(&buffer, divider_y, 30);
        assert!(row.contains('─'), "divider rule should draw `─`: {row:?}");
        assert!(
            row.contains("remote"),
            "labeled divider should contain `remote`: {row:?}"
        );
        // The rule glyph is drawn in surface_dim.
        let dim = app.palette.surface_dim;
        assert!(
            (0..30u16).any(|x| buffer[(x, divider_y)].symbol() == "─"
                && buffer[(x, divider_y)].style().fg == Some(dim)),
            "rule glyph should be dim"
        );
    }

    #[test]
    fn render_writes_plain_rule_in_plain_mode() {
        let mut app = prepared_divider_app(&[false, true], 30, 20);
        app.host_banner_active = true;
        // Recompute entries/divider so the labeled flag flips (geometry unchanged: 1 row).
        let area = app.view.sidebar_rect;
        let (cards, banners, divider_rows) = compute_workspace_list_areas_full(&app, area);
        app.view.workspace_card_areas = cards;
        app.view.host_banner_areas = banners;
        app.view.divider_rows = divider_rows;

        let divider_y = app.view.divider_rows[0];
        let buffer = render_divider_buffer(&app, 30, 20);
        let row = buffer_row_text(&buffer, divider_y, 30);
        assert!(row.contains('─'), "plain divider still draws `─`: {row:?}");
        assert!(
            !row.contains("remote"),
            "plain divider must NOT contain `remote`: {row:?}"
        );
    }

    #[test]
    fn render_writes_no_divider_when_single_role_group() {
        let app = prepared_divider_app(&[false, false], 30, 20);
        assert!(app.view.divider_rows.is_empty());
        let buffer = render_divider_buffer(&app, 30, 20);
        // No row anywhere contains the `remote` label.
        let any_remote = (0..20u16).any(|y| buffer_row_text(&buffer, y, 30).contains("remote"));
        assert!(!any_remote, "all-local sidebar must render no divider");
    }

    // ---- host banner ----

    use crate::app::state::{HostBannerSpec, HostBannerState};

    /// Build an app like `divider_app` but with `host_banners`/`host_banner_rows`/
    /// `host_banner_active` populated, mirroring what the client compositor does on the client
    /// path. `banner_rows` are the `ws_idx`s the banners precede; `states` the per-banner state.
    fn host_banner_app(
        remote: &[bool],
        banner_rows: &[usize],
        states: &[HostBannerState],
    ) -> AppState {
        let mut app = divider_app(remote);
        app.host_banner_rows = banner_rows.to_vec();
        app.host_banners = banner_rows
            .iter()
            .zip(states.iter())
            .enumerate()
            .map(|(i, (_, state))| HostBannerSpec {
                display_name: format!("host{i}"),
                connection_state: *state,
            })
            .collect();
        app.host_banner_active = !banner_rows.is_empty();
        app
    }

    fn banner_positions(entries: &[WorkspaceListEntry]) -> Vec<(usize, usize)> {
        entries
            .iter()
            .enumerate()
            .filter_map(|(idx, entry)| match entry {
                WorkspaceListEntry::HostBanner { banner_idx } => Some((idx, *banner_idx)),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn host_banner_entry_emitted_per_remote_host() {
        // Local ws 0, two remote hosts: host A's first row is ws 1, host B's first row is ws 2.
        let app = host_banner_app(
            &[false, true, true],
            &[1, 2],
            &[HostBannerState::Connected, HostBannerState::Connected],
        );
        let entries = workspace_list_entries(&app);
        let banners = banner_positions(&entries);
        assert_eq!(banners.len(), 2, "one banner per remote host: {entries:?}");
        // banner_idx aligns with host_banners order.
        assert_eq!(banners[0].1, 0);
        assert_eq!(banners[1].1, 1);
        // Each banner sits immediately before its host group's first workspace.
        assert_eq!(
            entries[banners[0].0 + 1],
            WorkspaceListEntry::Workspace {
                ws_idx: 1,
                indented: false
            }
        );
        assert_eq!(
            entries[banners[1].0 + 1],
            WorkspaceListEntry::Workspace {
                ws_idx: 2,
                indented: false
            }
        );
        // The divider precedes the first banner (banner below divider).
        let divider_idx = entries
            .iter()
            .position(|e| matches!(e, WorkspaceListEntry::Divider { .. }))
            .expect("divider present");
        assert!(divider_idx < banners[0].0, "divider above first banner");
    }

    #[test]
    fn host_banner_no_entry_for_local_only() {
        let app = host_banner_app(&[false, false], &[], &[]);
        assert!(banner_positions(&workspace_list_entries(&app)).is_empty());
    }

    #[test]
    fn divider_plain_when_banner_active() {
        // When a banner is visible (host_banner_active) the divider flips to plain.
        let app = host_banner_app(&[false, true], &[1], &[HostBannerState::Connected]);
        let entries = workspace_list_entries(&app);
        let divider = entries
            .iter()
            .find(|e| matches!(e, WorkspaceListEntry::Divider { .. }))
            .expect("divider present");
        assert_eq!(divider, &WorkspaceListEntry::Divider { labeled: false });

        // Without a banner the divider is labeled.
        let plain = divider_app(&[false, true]);
        let labeled = workspace_list_entries(&plain)
            .into_iter()
            .find(|e| matches!(e, WorkspaceListEntry::Divider { .. }))
            .expect("divider present");
        assert_eq!(labeled, WorkspaceListEntry::Divider { labeled: true });
    }

    #[test]
    fn compute_areas_render_equals_hit_test() {
        // The render==hit_test invariant for the host banner: compute_workspace_list_areas_full
        // emits NO WorkspaceCardArea for HostBanner/Divider; HostBanner pushes exactly one
        // HostBannerArea at the right y (one row); Divider pushes nothing to the banner slot.
        let app = host_banner_app(&[false, true], &[1], &[HostBannerState::Connected]);
        let area = Rect::new(0, 0, 30, 24);
        let (cards, banners, divider_rows) = compute_workspace_list_areas_full(&app, area);
        // One card per real workspace — none for banner/divider.
        assert_eq!(cards.len(), app.workspaces.len());
        assert_eq!(banners.len(), 1, "one HostBannerArea");
        assert_eq!(divider_rows.len(), 1, "one divider row");
        let banner_y = banners[0].rect.y;
        assert_eq!(banners[0].rect.height, 1, "banner consumes one row");
        assert_eq!(banners[0].banner_idx, 0);
        // No card overlaps the banner row.
        assert!(cards
            .iter()
            .all(|card| !(banner_y >= card.rect.y && banner_y < card.rect.y + card.rect.height)));
        // Banner sits below the divider, above the remote card.
        assert!(divider_rows[0] < banner_y);
        let first_remote = cards.iter().find(|c| c.ws_idx == 1).unwrap();
        assert!(banner_y < first_remote.rect.y);

        // The two-tuple compute_workspace_list_areas agrees with the full pass (same geometry).
        let (cards2, banners2) = compute_workspace_list_areas(&app, area);
        assert_eq!(cards2.len(), cards.len());
        assert_eq!(banners2, banners);
    }

    #[test]
    fn host_banner_solid_renders_one_flat_accent_span() {
        // The host name is one flat bold accent span — no per-character shading
        // and nothing tick-dependent (the style options were removed).
        let p = Palette::catppuccin();
        let span = host_banner_name_span("demo", &p);
        assert_eq!(span.content.as_ref(), "demo");
        assert_eq!(span.style.fg, Some(p.accent));
        assert!(span.style.add_modifier.contains(Modifier::BOLD));
    }

    fn prepared_host_banner_app(
        remote: &[bool],
        banner_rows: &[usize],
        states: &[HostBannerState],
        width: u16,
        height: u16,
    ) -> AppState {
        let mut app = host_banner_app(remote, banner_rows, states);
        let area = Rect::new(0, 0, width, height);
        app.view.sidebar_rect = area;
        let (cards, banners, divider_rows) = compute_workspace_list_areas_full(&app, area);
        app.view.workspace_card_areas = cards;
        app.view.host_banner_areas = banners;
        app.view.divider_rows = divider_rows;
        app
    }

    #[test]
    fn connected_banner_renders_solid_accent_name_span() {
        let mut app =
            prepared_host_banner_app(&[false, true], &[1], &[HostBannerState::Connected], 40, 20);
        app.host_banners[0].display_name = "prod".into();
        let banner_y = app.view.host_banner_areas[0].rect.y;
        let buffer = render_divider_buffer(&app, 40, 20);

        // The host name renders as bold theme-accent cells.
        let mut found_accent_bold = false;
        for x in 0..40u16 {
            let cell = &buffer[(x, banner_y)];
            if cell.style().fg == Some(app.palette.accent)
                && cell.style().add_modifier.contains(Modifier::BOLD)
                && cell.symbol() != " "
            {
                found_accent_bold = true;
            }
        }
        assert!(found_accent_bold, "banner name should be bold accent");

        let row = buffer_row_text(&buffer, banner_y, 40);
        assert!(row.contains("prod"), "banner shows host name: {row:?}");
        assert!(row.contains('◆'), "connected glyph `◆`: {row:?}");
        assert!(
            !row.contains("spaces"),
            "no space-count suffix on connected banners: {row:?}"
        );
    }

    #[test]
    fn remote_spaces_sit_under_banner_local_has_none() {
        // The single remote host group (ws 1 & 2) is introduced by exactly one banner above its
        // first row; the local space (ws 0) is the first entry with no banner above it.
        let mut app = host_banner_app(&[false, true, true], &[1], &[HostBannerState::Connected]);
        app.host_banner_rows = vec![1]; // one host group starting at ws 1
        let entries = workspace_list_entries(&app);

        // Local space is the first entry; nothing precedes it.
        assert_eq!(
            entries.first(),
            Some(&WorkspaceListEntry::Workspace {
                ws_idx: 0,
                indented: false
            })
        );
        // Exactly one banner, and it sits before the first remote workspace (ws 1).
        let banners = banner_positions(&entries);
        assert_eq!(banners.len(), 1);
        assert_eq!(
            entries[banners[0].0 + 1],
            WorkspaceListEntry::Workspace {
                ws_idx: 1,
                indented: false
            }
        );
        // The second remote space (ws 2) follows under the same group, with no extra banner.
        assert!(entries
            .iter()
            .any(|e| matches!(e, WorkspaceListEntry::Workspace { ws_idx: 2, .. })));
    }

    #[test]
    fn host_drag_draws_drop_indicator_at_host_boundary() {
        // A live HostReorder drag draws the accent drop line at a host boundary (one row above
        // the banner) and lifts the dragged host's banner row with surface1.
        let mut app =
            prepared_host_banner_app(&[false, true], &[1], &[HostBannerState::Connected], 30, 20);
        app.drag = Some(crate::app::state::DragState {
            target: crate::app::state::DragTarget::HostReorder {
                source_host_idx: 0,
                insert_idx: Some(0),
            },
        });
        let area = Rect::new(0, 0, 30, 20);
        let banner_y = app.view.host_banner_areas[0].rect.y;
        let indicator_y = host_drop_indicator_row(&app.view.host_banner_areas, area, 0)
            .expect("host boundary row");
        assert_eq!(indicator_y, banner_y - 1, "boundary sits above the banner");
        // insert at the end of the host list resolves to the row below the last banner.
        assert_eq!(
            host_drop_indicator_row(&app.view.host_banner_areas, area, 1),
            Some(banner_y),
        );

        let buffer = render_divider_buffer(&app, 30, 20);
        assert!(
            (0..30u16).any(|x| buffer[(x, indicator_y)].symbol() == "─"
                && buffer[(x, indicator_y)].style().fg == Some(app.palette.accent)),
            "accent drop indicator drawn at the host boundary"
        );
        assert!(
            (0..30u16).any(|x| buffer[(x, banner_y)].style().bg == Some(app.palette.surface1)),
            "dragged host banner row lifts with surface1"
        );
    }
}
