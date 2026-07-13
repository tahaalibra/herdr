use ratatui::{
    layout::{Constraint, Layout, Rect},
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Clear, Paragraph, Wrap},
    Frame,
};

use super::text::{display_width_u16, truncate_end};
use super::widgets::{
    action_button_row_rects, bottom_left_popup_rect, centered_popup_rect, panel_contrast_fg,
    render_action_button, render_modal_header, render_modal_shell, render_panel_shell,
    ActionButtonSpec,
};
use crate::app::state::Palette;
use crate::app::{state::WorktreeOpenState, AppState, Mode};

// ui-owned view structs for the composited client modals. These hold ONLY ui
// primitives / borrowed strings — they MUST NOT reference any supervisor-model
// type (one-way `ui` <- `client` layering). The compositor maps the model into
// these views before calling the `render_*_overlay` functions below. The
// `dialogs_does_not_reference_client_supervisor` test enforces this.

/// View for the add-remote modal. `focused_is_target` selects which of the two fields draws the
/// focused (filled) style + cursor block.
pub(crate) struct AddRemoteOverlayView<'a> {
    pub target: &'a str,
    pub name: &'a str,
    pub focused_is_target: bool,
    pub error: Option<&'a str>,
    /// When true, the status row shows an animated "connecting" line instead of an error. Takes
    /// precedence over `error` (the worker clears the error when it starts).
    pub in_progress: bool,
    /// Current spinner glyph for the in-progress line (advances with the shared animation tick).
    pub spinner: &'a str,
    /// When set (the destination), the status row shows a `[y/N]` prompt to restart an
    /// incompatible no-handoff remote server. Highest precedence.
    pub restart_confirm_destination: Option<&'a str>,
}

/// View for one new-workspace destination row.
pub(crate) struct DestinationView<'a> {
    pub display_name: &'a str,
}

/// ui-owned glyph for a remote's state in the management overlay. This is a ui-local enum, NOT
/// the supervisor `ConnectionState` (the one-way layering rule). The compositor maps the
/// supervisor row state into this before calling `render_remote_manage_overlay`.
// Constructed by the client compositor (next phase) and by geometry tests.
#[cfg_attr(not(test), allow(dead_code))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RemoteStateGlyph {
    Connected,
    Connecting,
    Disconnected,
    Disabled,
    ProtocolMismatch,
}

impl RemoteStateGlyph {
    /// The single-cell status glyph drawn at the start of the row.
    fn glyph(self) -> &'static str {
        match self {
            RemoteStateGlyph::Connected => "●",
            RemoteStateGlyph::Connecting => "◐",
            RemoteStateGlyph::Disconnected => "○",
            RemoteStateGlyph::Disabled => "✕",
            RemoteStateGlyph::ProtocolMismatch => "!",
        }
    }
}

/// ui-owned view for one management-overlay row. Holds only ui primitives / borrowed strings
/// (no supervisor type), per the layering rule.
pub(crate) struct RemoteManageRowView<'a> {
    pub glyph: RemoteStateGlyph,
    pub name: &'a str,
    pub target: &'a str,
    pub state_word: &'a str,
    pub disabled: bool,
}

/// ui-owned view for the workspace context menu. `label` is the workspace name shown as the
/// modal sub-header; `rows` are the menu item labels. Holds only borrowed strings (no supervisor
/// type), per the layering rule.
pub(crate) struct WorkspaceContextMenuView<'a> {
    pub label: &'a str,
    pub rows: &'a [&'a str],
}

const NEW_LINKED_WORKTREE_POPUP_WIDTH: u16 = 68;
const NEW_LINKED_WORKTREE_POPUP_HEIGHT: u16 = 12;

pub(crate) fn rename_button_rects(inner: Rect) -> (Rect, Rect, Rect) {
    let rects = action_button_row_rects(
        inner,
        &[
            ActionButtonSpec {
                hint: Some("↵"),
                label: "save",
            },
            ActionButtonSpec {
                hint: Some("^c"),
                label: "clear",
            },
            ActionButtonSpec {
                hint: Some("esc"),
                label: "cancel",
            },
        ],
        2,
        3,
    );
    (rects[0], rects[1], rects[2])
}

pub(super) fn render_rename_overlay(app: &AppState, frame: &mut Frame, area: Rect) {
    super::dim_background(frame, area);

    let title = match app.mode {
        Mode::RenameWorkspace => "rename workspace",
        Mode::RenameTab if app.creating_new_tab => "new tab",
        Mode::RenameTab => "rename tab",
        Mode::RenamePane => "rename pane",
        _ => return,
    };

    let Some(inner) = render_modal_shell(frame, area, 56, 7, &app.palette) else {
        return;
    };
    if inner.height < 4 {
        return;
    }

    let rows = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Min(0),
    ])
    .areas::<5>(inner);

    render_modal_header(frame, rows[0], title, &app.palette);

    let input_rect = Rect::new(rows[2].x, rows[2].y, rows[2].width, 1);
    frame.render_widget(Clear, input_rect);
    frame.render_widget(
        Paragraph::new(format!(" {}█", app.name_input)).style(
            Style::default()
                .fg(app.palette.text)
                .bg(app.palette.surface0),
        ),
        input_rect,
    );

    let (save_rect, clear_rect, cancel_rect) = rename_button_rects(inner);

    render_action_button(
        frame,
        save_rect,
        Some("↵"),
        "save",
        Style::default()
            .fg(panel_contrast_fg(&app.palette))
            .bg(app.palette.accent)
            .add_modifier(Modifier::BOLD),
    );
    render_action_button(
        frame,
        clear_rect,
        Some("^c"),
        "clear",
        Style::default()
            .fg(app.palette.text)
            .bg(app.palette.surface0)
            .add_modifier(Modifier::BOLD),
    );
    render_action_button(
        frame,
        cancel_rect,
        Some("esc"),
        "cancel",
        Style::default()
            .fg(app.palette.text)
            .bg(app.palette.surface0)
            .add_modifier(Modifier::BOLD),
    );
}

pub(crate) fn new_linked_worktree_inner_rect(area: Rect) -> Option<Rect> {
    centered_popup_rect(
        area,
        NEW_LINKED_WORKTREE_POPUP_WIDTH,
        NEW_LINKED_WORKTREE_POPUP_HEIGHT,
    )
    .map(|popup| {
        Rect::new(
            popup.x + 1,
            popup.y + 1,
            popup.width.saturating_sub(2),
            popup.height.saturating_sub(2),
        )
    })
}

pub(crate) fn new_linked_worktree_button_rects(inner: Rect) -> (Rect, Rect) {
    let rects = action_button_row_rects(
        inner,
        &[
            ActionButtonSpec {
                hint: Some("↵"),
                label: "create and open",
            },
            ActionButtonSpec {
                hint: Some("esc"),
                label: "cancel",
            },
        ],
        2,
        inner.height.saturating_sub(1),
    );
    (rects[0], rects[1])
}

pub(crate) fn remove_worktree_popup_rect(area: Rect) -> Option<Rect> {
    centered_popup_rect(area, 72, 10)
}

pub(crate) fn remove_worktree_button_rects(inner: Rect, force_confirmation: bool) -> (Rect, Rect) {
    let primary_label = if force_confirmation {
        "delete anyway"
    } else {
        "remove"
    };
    let rects = action_button_row_rects(
        inner,
        &[
            ActionButtonSpec {
                hint: Some("↵"),
                label: primary_label,
            },
            ActionButtonSpec {
                hint: Some("esc"),
                label: "cancel",
            },
        ],
        2,
        inner.height.saturating_sub(1),
    );
    (rects[0], rects[1])
}

pub(crate) fn open_existing_worktree_inner_rect(area: Rect, entry_count: usize) -> Option<Rect> {
    let height = (entry_count as u16)
        .saturating_mul(2)
        .saturating_add(7)
        .clamp(12, 26);
    centered_popup_rect(area, 96, height).map(|popup| {
        Rect::new(
            popup.x + 1,
            popup.y + 1,
            popup.width.saturating_sub(2),
            popup.height.saturating_sub(2),
        )
    })
}

pub(crate) fn open_existing_worktree_max_visible_rows(inner: Rect) -> usize {
    usize::from(inner.height.saturating_sub(5) / 2)
}

pub(crate) fn open_existing_worktree_visible_start(
    open: &WorktreeOpenState,
    max_rows: usize,
) -> usize {
    let filtered = open.filtered_indices();
    let selected = open.selected_entry_index().unwrap_or(open.selected);
    let selected_pos = filtered
        .iter()
        .position(|idx| *idx == selected)
        .unwrap_or(0);
    selected_pos.saturating_sub(max_rows.saturating_sub(1))
}

pub(crate) fn open_existing_worktree_button_rects(inner: Rect) -> (Rect, Rect) {
    let rects = action_button_row_rects(
        inner,
        &[
            ActionButtonSpec {
                hint: Some("↵"),
                label: "open",
            },
            ActionButtonSpec {
                hint: Some("esc"),
                label: "cancel",
            },
        ],
        2,
        inner.height.saturating_sub(1),
    );
    (rects[0], rects[1])
}

pub(super) fn render_new_linked_worktree_overlay(app: &AppState, frame: &mut Frame, area: Rect) {
    let Some(create) = app.worktree_create.as_ref() else {
        return;
    };

    super::dim_background(frame, area);
    let Some(inner) = render_modal_shell(
        frame,
        area,
        NEW_LINKED_WORKTREE_POPUP_WIDTH,
        NEW_LINKED_WORKTREE_POPUP_HEIGHT,
        &app.palette,
    ) else {
        return;
    };
    if inner.height < 9 {
        return;
    }

    let rows = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Length(3),
        Constraint::Length(1),
        Constraint::Min(0),
    ])
    .areas::<8>(inner);

    render_modal_header(frame, rows[0], "new worktree", &app.palette);

    frame.render_widget(
        Paragraph::new(" branch").style(Style::default().fg(app.palette.overlay0)),
        rows[1],
    );
    let input_rect = Rect::new(rows[2].x, rows[2].y, rows[2].width, 1);
    frame.render_widget(Clear, input_rect);
    frame.render_widget(
        Paragraph::new(format!(" {}█", app.name_input)).style(
            Style::default()
                .fg(app.palette.text)
                .bg(app.palette.surface0),
        ),
        input_rect,
    );

    let checkout = create.checkout_path.display().to_string();
    frame.render_widget(
        Paragraph::new(" checkout").style(Style::default().fg(app.palette.overlay0)),
        rows[3],
    );
    frame.render_widget(
        Paragraph::new(format!(" {checkout}")).style(Style::default().fg(app.palette.subtext0)),
        rows[4],
    );

    if create.creating {
        frame.render_widget(
            Paragraph::new(" creating…").style(Style::default().fg(app.palette.overlay0)),
            rows[5],
        );
    } else if let Some(error) = &create.error {
        frame.render_widget(
            Paragraph::new(format!(" {error}"))
                .style(Style::default().fg(app.palette.red))
                .wrap(Wrap { trim: false }),
            rows[5],
        );
    }

    let (create_rect, cancel_rect) = new_linked_worktree_button_rects(inner);
    render_action_button(
        frame,
        create_rect,
        Some("↵"),
        "create and open",
        Style::default()
            .fg(panel_contrast_fg(&app.palette))
            .bg(app.palette.accent)
            .add_modifier(Modifier::BOLD),
    );
    render_action_button(
        frame,
        cancel_rect,
        Some("esc"),
        "cancel",
        Style::default()
            .fg(app.palette.text)
            .bg(app.palette.surface0)
            .add_modifier(Modifier::BOLD),
    );
}

pub(super) fn render_remove_worktree_overlay(app: &AppState, frame: &mut Frame, area: Rect) {
    let Some(remove) = app.worktree_remove.as_ref() else {
        return;
    };

    super::dim_background(frame, area);
    let Some(popup) = remove_worktree_popup_rect(area) else {
        return;
    };
    let Some(inner) = render_panel_shell(frame, popup, app.palette.red, app.palette.panel_bg)
    else {
        return;
    };

    let rows = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Min(0),
    ])
    .areas::<8>(inner);

    frame.render_widget(
        Paragraph::new(Line::from(vec![Span::styled(
            " delete worktree checkout?",
            Style::default()
                .fg(app.palette.red)
                .add_modifier(Modifier::BOLD),
        )])),
        rows[0],
    );
    frame.render_widget(
        Paragraph::new(" This removes the checkout folder:")
            .style(Style::default().fg(app.palette.overlay0)),
        rows[1],
    );
    frame.render_widget(
        Paragraph::new(format!(" {}", remove.path.display()))
            .style(Style::default().fg(app.palette.text)),
        rows[2],
    );
    frame.render_widget(
        Paragraph::new(" The branch is not deleted. The Herdr workspace will close.")
            .style(Style::default().fg(app.palette.overlay0)),
        rows[3],
    );
    if remove.force_confirmation {
        frame.render_widget(
            Paragraph::new(" Dirty or untracked files will be permanently deleted.")
                .style(Style::default().fg(app.palette.red)),
            rows[4],
        );
    }
    if remove.removing {
        frame.render_widget(
            Paragraph::new(" removing…").style(Style::default().fg(app.palette.overlay0)),
            rows[5],
        );
    } else if let Some(error) = &remove.error {
        frame.render_widget(
            Paragraph::new(format!(" {error}")).style(Style::default().fg(app.palette.red)),
            rows[5],
        );
    }

    let (remove_rect, cancel_rect) = remove_worktree_button_rects(inner, remove.force_confirmation);
    let remove_label = if remove.force_confirmation {
        "delete anyway"
    } else {
        "remove"
    };
    render_action_button(
        frame,
        remove_rect,
        Some("↵"),
        remove_label,
        Style::default()
            .fg(panel_contrast_fg(&app.palette))
            .bg(app.palette.red)
            .add_modifier(Modifier::BOLD),
    );
    render_action_button(
        frame,
        cancel_rect,
        Some("esc"),
        "cancel",
        Style::default()
            .fg(app.palette.text)
            .bg(app.palette.surface0)
            .add_modifier(Modifier::BOLD),
    );
}

pub(super) fn render_open_existing_worktree_overlay(app: &AppState, frame: &mut Frame, area: Rect) {
    let Some(open) = app.worktree_open.as_ref() else {
        return;
    };

    super::dim_background(frame, area);
    let height = (open.entries.len() as u16)
        .saturating_mul(2)
        .saturating_add(7)
        .clamp(12, 26);
    let Some(inner) = render_modal_shell(frame, area, 96, height, &app.palette) else {
        return;
    };
    if inner.height < 8 {
        return;
    }

    render_modal_header(
        frame,
        Rect::new(inner.x, inner.y, inner.width, 1),
        "open worktree",
        &app.palette,
    );
    render_open_worktree_search(
        app,
        frame,
        Rect::new(inner.x, inner.y + 1, inner.width, 1),
        open,
    );
    frame.render_widget(
        Paragraph::new("─".repeat(inner.width as usize))
            .style(Style::default().fg(app.palette.surface1)),
        Rect::new(inner.x, inner.y.saturating_add(2), inner.width, 1),
    );

    let filtered = open.filtered_indices();
    let max_rows = open_existing_worktree_max_visible_rows(inner);
    let start = open_existing_worktree_visible_start(open, max_rows);
    for (visible_idx, entry_idx) in filtered.iter().skip(start).take(max_rows).enumerate() {
        let Some(entry) = open.entries.get(*entry_idx) else {
            continue;
        };
        let selected = Some(*entry_idx) == open.selected_entry_index();
        let y = inner.y.saturating_add(3 + (visible_idx as u16 * 2));
        let marker = if selected { "›" } else { " " };
        let row_style = if selected {
            Style::default()
                .fg(app.palette.text)
                .bg(app.palette.surface0)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(app.palette.subtext0)
        };
        let path_style = if selected {
            Style::default()
                .fg(app.palette.subtext0)
                .bg(app.palette.surface0)
        } else {
            Style::default().fg(app.palette.overlay0)
        };
        let status = entry.status_label();
        let title_width = inner
            .width
            .saturating_sub(display_width_u16(status))
            .saturating_sub(4) as usize;
        let mut title = format!(
            "{marker} {}",
            truncate_end(&entry.display_name(), title_width)
        );
        if !status.is_empty() {
            let pad = inner
                .width
                .saturating_sub(display_width_u16(&title))
                .saturating_sub(display_width_u16(status))
                .max(1);
            title.push_str(&" ".repeat(pad as usize));
            title.push_str(status);
        }
        frame.render_widget(
            Paragraph::new(truncate_end(&title, inner.width as usize)).style(row_style),
            Rect::new(inner.x, y, inner.width, 1),
        );
        frame.render_widget(
            Paragraph::new(truncate_end(
                &format!("  {}", entry.path.display()),
                inner.width as usize,
            ))
            .style(path_style),
            Rect::new(inner.x, y.saturating_add(1), inner.width, 1),
        );
    }

    if filtered.is_empty() {
        frame.render_widget(
            Paragraph::new(" no matching worktrees")
                .style(Style::default().fg(app.palette.overlay0)),
            Rect::new(inner.x, inner.y.saturating_add(3), inner.width, 1),
        );
    }

    if let Some(error) = &open.error {
        frame.render_widget(
            Paragraph::new(format!(" {error}")).style(Style::default().fg(app.palette.red)),
            Rect::new(
                inner.x,
                inner.y + inner.height.saturating_sub(2),
                inner.width,
                1,
            ),
        );
    }

    let (open_rect, cancel_rect) = open_existing_worktree_button_rects(inner);
    render_action_button(
        frame,
        open_rect,
        Some("↵"),
        "open",
        Style::default()
            .fg(panel_contrast_fg(&app.palette))
            .bg(app.palette.accent)
            .add_modifier(Modifier::BOLD),
    );
    render_action_button(
        frame,
        cancel_rect,
        Some("esc"),
        "cancel",
        Style::default()
            .fg(app.palette.text)
            .bg(app.palette.surface0)
            .add_modifier(Modifier::BOLD),
    );
}

fn render_open_worktree_search(
    app: &AppState,
    frame: &mut Frame,
    area: Rect,
    open: &WorktreeOpenState,
) {
    let focus_style = if open.search_focused {
        Style::default()
            .fg(app.palette.accent)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(app.palette.overlay0)
    };
    let filtered_count = open.filtered_indices().len();
    let count = if open.query.trim().is_empty() {
        format!("{} checkouts", open.entries.len())
    } else {
        format!("{filtered_count}/{} checkouts", open.entries.len())
    };
    let mut spans = vec![Span::styled(" / ", focus_style)];
    if open.query.trim().is_empty() {
        spans.push(Span::styled(
            "filter worktrees",
            Style::default().fg(app.palette.overlay0),
        ));
    } else {
        spans.push(Span::styled(
            open.query.clone(),
            Style::default().fg(app.palette.text),
        ));
    }
    spans.push(Span::styled(
        format!(
            "{count:>width$}",
            width = area.width.saturating_sub(18) as usize
        ),
        Style::default().fg(app.palette.overlay0),
    ));
    frame.render_widget(Paragraph::new(Line::from(spans)), area);
}

fn confirm_close_overlay_text(app: &AppState) -> (String, String) {
    let ws_name = app
        .workspaces
        .get(app.selected)
        .map(|ws| ws.display_name())
        .unwrap_or_else(|| "?".to_string());
    let selected_space = app
        .workspaces
        .get(app.selected)
        .and_then(|ws| ws.worktree_space());
    let group_member_indices = selected_space
        .filter(|space| !space.is_linked_worktree)
        .map(|space| {
            app.workspaces
                .iter()
                .enumerate()
                .filter_map(|(idx, ws)| {
                    ws.worktree_space()
                        .is_some_and(|member| member.key == space.key)
                        .then_some(idx)
                })
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    let closes_group = group_member_indices.len() > 1;
    let pane_count = if closes_group {
        group_member_indices
            .iter()
            .filter_map(|idx| app.workspaces.get(*idx))
            .map(|ws| ws.layout.pane_count())
            .sum()
    } else {
        app.workspaces
            .get(app.selected)
            .map(|ws| ws.layout.pane_count())
            .unwrap_or(0)
    };

    let pane_text = if pane_count == 1 {
        "1 pane".to_string()
    } else {
        format!("{pane_count} panes")
    };
    let workspace_text = if closes_group {
        let count = group_member_indices.len();
        if count == 1 {
            "1 workspace, ".to_string()
        } else {
            format!("{count} workspaces, ")
        }
    } else {
        String::new()
    };

    let title = if closes_group {
        "Close worktree group?"
    } else {
        "Close workspace?"
    };
    let detail = format!("{ws_name} — {workspace_text}{pane_text}");
    (title.to_string(), detail)
}

pub(super) fn render_confirm_close_overlay(app: &AppState, frame: &mut Frame, area: Rect) {
    let (title, detail) = confirm_close_overlay_text(app);

    super::dim_background(frame, area);

    let Some(popup) = confirm_close_popup_rect(area) else {
        return;
    };

    let warn = Style::default()
        .fg(app.palette.red)
        .add_modifier(Modifier::BOLD);
    let dim = Style::default().fg(app.palette.overlay0);

    let title_line = Line::from(vec![Span::styled(format!(" {title}"), warn)]);

    let detail_line = Line::from(vec![
        Span::styled(
            format!(" {}", detail.split(" — ").next().unwrap_or(&detail)),
            Style::default()
                .fg(app.palette.text)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            detail
                .split_once(" — ")
                .map(|(_, rest)| format!(" — {rest}"))
                .unwrap_or_default(),
            dim,
        ),
    ]);

    let Some(inner) = render_panel_shell(frame, popup, app.palette.red, app.palette.panel_bg)
    else {
        return;
    };

    if inner.height >= 3 {
        let rows = Layout::vertical([
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Length(1),
        ])
        .areas::<4>(inner);

        frame.render_widget(Paragraph::new(title_line), rows[0]);
        frame.render_widget(Paragraph::new(detail_line), rows[1]);

        let (confirm_rect, cancel_rect) = confirm_close_button_rects(inner);
        render_action_button(
            frame,
            confirm_rect,
            Some("↵"),
            "confirm",
            Style::default()
                .fg(panel_contrast_fg(&app.palette))
                .bg(app.palette.red)
                .add_modifier(Modifier::BOLD),
        );
        render_action_button(
            frame,
            cancel_rect,
            Some("esc"),
            "cancel",
            Style::default()
                .fg(app.palette.text)
                .bg(app.palette.surface0)
                .add_modifier(Modifier::BOLD),
        );
    }
}

pub(crate) fn confirm_close_popup_rect(area: Rect) -> Option<Rect> {
    centered_popup_rect(area, 64, 6)
}

pub(crate) fn confirm_close_button_rects(inner: Rect) -> (Rect, Rect) {
    let rects = action_button_row_rects(
        inner,
        &[
            ActionButtonSpec {
                hint: Some("↵"),
                label: "confirm",
            },
            ActionButtonSpec {
                hint: Some("esc"),
                label: "cancel",
            },
        ],
        2,
        3,
    );
    (rects[0], rects[1])
}

// Single-source-of-truth geometry for the composited client modals. Both render and the client
// compositor's hit test derive every row/button rect from these helpers, so render geometry ==
// hit-test geometry.

/// The outer popup rect for the add-remote overlay — footer-anchored (bottom-left of `area`,
/// opening upward) so it floats over the live content like the global launcher menu. Used by BOTH
/// the renderer and the compositor's content-copy exclusion / hit-test.
pub(crate) fn add_remote_popup_rect(area: Rect) -> Option<Rect> {
    bottom_left_popup_rect(area, 54, 9)
}

/// Fixed inner rect for the add-remote overlay. Returns `None` when the host is too small to fit
/// the overlay, in which case render and hit-test both no-op.
pub(crate) fn add_remote_inner_rect(area: Rect) -> Option<Rect> {
    add_remote_popup_rect(area).map(|popup| {
        Rect::new(
            popup.x + 1,
            popup.y + 1,
            popup.width.saturating_sub(2),
            popup.height.saturating_sub(2),
        )
    })
}

pub(crate) fn add_remote_button_rects(inner: Rect) -> (Rect, Rect) {
    let rects = action_button_row_rects(
        inner,
        &[
            ActionButtonSpec {
                hint: Some("↵"),
                label: "add",
            },
            ActionButtonSpec {
                hint: Some("esc"),
                label: "cancel",
            },
        ],
        2,
        inner.height.saturating_sub(1),
    );
    (rects[0], rects[1])
}

/// The picker popup height for `count` destinations, derived from the row budget (header, the
/// create-on label, one row per destination, the actions row, and vertical margins) and clamped to
/// a sane band. Used by BOTH the inner-rect helper and the renderer so they cannot diverge.
fn picker_popup_height(count: usize) -> u16 {
    (count as u16).saturating_add(5).clamp(7, 18)
}

/// The outer popup rect for the new-workspace picker — footer-anchored (bottom-left of `area`,
/// opening upward) so it floats over the live content like the global launcher menu. Used by BOTH
/// the renderer and the compositor's content-copy exclusion / hit-test.
pub(crate) fn new_workspace_picker_popup_rect(area: Rect, count: usize) -> Option<Rect> {
    bottom_left_popup_rect(area, 44, picker_popup_height(count))
}

/// Shared inner rect for the new-workspace picker overlay. The popup height is derived from the
/// destination `count` exactly the way `render_new_workspace_picker_overlay` sizes it, mirroring
/// `open_existing_worktree_inner_rect`.
pub(crate) fn new_workspace_picker_inner_rect(area: Rect, count: usize) -> Option<Rect> {
    new_workspace_picker_popup_rect(area, count).map(|popup| {
        Rect::new(
            popup.x + 1,
            popup.y + 1,
            popup.width.saturating_sub(2),
            popup.height.saturating_sub(2),
        )
    })
}

/// The rect for destination row `row_index` inside the picker's inner rect. Rows start two lines
/// below the inner top (header + create-on label). Used by BOTH render and hit-test.
pub(crate) fn new_workspace_picker_row_rect(inner: Rect, row_index: usize) -> Rect {
    let y = inner.y.saturating_add(2 + row_index as u16);
    Rect::new(inner.x, y, inner.width, 1)
}

pub(crate) fn new_workspace_picker_button_rects(inner: Rect) -> (Rect, Rect) {
    let rects = action_button_row_rects(
        inner,
        &[
            ActionButtonSpec {
                hint: Some("↵"),
                label: "create",
            },
            ActionButtonSpec {
                hint: Some("esc"),
                label: "cancel",
            },
        ],
        2,
        inner.height.saturating_sub(1),
    );
    (rects[0], rects[1])
}

/// Render the add-remote form as a footer-anchored ratatui modal, visually matching
/// `render_rename_overlay` (accent border, bold header, focused-field fill + cursor block, red
/// inline error, centered action buttons). Cursor stays hidden — the caller forces
/// `frame.cursor = None` while a modal is open.
// Called by the client compositor once the compositor phase lands; the geometry
// helpers above are its hit-test twins and are already test-covered.
#[allow(dead_code)]
pub(crate) fn render_add_remote_overlay(
    palette: &Palette,
    view: &AddRemoteOverlayView,
    frame: &mut Frame,
    area: Rect,
) {
    let Some(popup) = add_remote_popup_rect(area) else {
        return;
    };
    let Some(inner) = render_panel_shell(frame, popup, palette.accent, palette.panel_bg) else {
        return;
    };
    debug_assert_eq!(
        Some(inner),
        add_remote_inner_rect(area),
        "add-remote render and hit-test geometry diverged"
    );
    if inner.height < 5 {
        return;
    }

    let rows = Layout::vertical([
        Constraint::Length(1), // header
        Constraint::Length(1), // target label/input
        Constraint::Length(1), // name label/input
        Constraint::Length(1), // gap
        Constraint::Length(1), // error
        Constraint::Min(0),    // actions live on inner.height - 1
    ])
    .areas::<6>(inner);

    render_modal_header(frame, rows[0], "add remote", palette);

    render_add_remote_field(
        frame,
        rows[1],
        "target",
        view.target,
        view.focused_is_target,
        palette,
    );
    render_add_remote_field(
        frame,
        rows[2],
        "name",
        view.name,
        !view.focused_is_target,
        palette,
    );

    if let Some(destination) = view.restart_confirm_destination {
        frame.render_widget(
            Paragraph::new(format!(
                " {destination} runs an old herdr — restart it? stops its panes  [y/N]"
            ))
            .style(
                Style::default()
                    .fg(palette.yellow)
                    .add_modifier(Modifier::BOLD),
            ),
            rows[4],
        );
    } else if view.in_progress {
        frame.render_widget(
            Paragraph::new(format!(" {} connecting to remote…", view.spinner))
                .style(Style::default().fg(palette.accent)),
            rows[4],
        );
    } else if let Some(error) = view.error {
        frame.render_widget(
            Paragraph::new(format!(" {error}")).style(Style::default().fg(palette.red)),
            rows[4],
        );
    }

    let (submit_rect, cancel_rect) = add_remote_button_rects(inner);
    render_action_button(
        frame,
        submit_rect,
        Some("↵"),
        "add",
        Style::default()
            .fg(panel_contrast_fg(palette))
            .bg(palette.accent)
            .add_modifier(Modifier::BOLD),
    );
    render_action_button(
        frame,
        cancel_rect,
        Some("esc"),
        "cancel",
        Style::default()
            .fg(palette.text)
            .bg(palette.surface0)
            .add_modifier(Modifier::BOLD),
    );
}

fn render_add_remote_field(
    frame: &mut Frame,
    row: Rect,
    label: &str,
    value: &str,
    focused: bool,
    palette: &Palette,
) {
    if focused {
        frame.render_widget(Clear, row);
        let line = Line::from(vec![
            Span::styled(
                format!(" {label}  "),
                Style::default().fg(palette.overlay0).bg(palette.surface0),
            ),
            Span::styled(
                format!("{value}█"),
                Style::default().fg(palette.text).bg(palette.surface0),
            ),
        ]);
        frame.render_widget(
            Paragraph::new(line).style(Style::default().bg(palette.surface0)),
            row,
        );
    } else {
        let line = Line::from(vec![
            Span::styled(format!(" {label}  "), Style::default().fg(palette.overlay0)),
            Span::styled(value.to_string(), Style::default().fg(palette.subtext0)),
        ]);
        frame.render_widget(Paragraph::new(line), row);
    }
}

/// Render the new-workspace destination picker as a footer-anchored ratatui modal with a
/// selectable list (the `surface0`-filled `›`-marked selected row from the
/// `render_open_existing_worktree_overlay` pattern), a `create on` sub-label, and centered
/// create/cancel buttons. Geometry comes from `new_workspace_picker_inner_rect`/`_row_rect` so it
/// matches `hit_test`.
// Called by the client compositor once the compositor phase lands.
#[allow(dead_code)]
pub(crate) fn render_new_workspace_picker_overlay(
    palette: &Palette,
    destinations: &[DestinationView],
    selected: usize,
    hovered_row: Option<usize>,
    frame: &mut Frame,
    area: Rect,
) {
    // Draw the accent-bordered panel shell. Its inner rect is identical to
    // `new_workspace_picker_inner_rect(area, count)` (both derive from the same
    // `new_workspace_picker_popup_rect(area, count)`), so render geometry == hit-test geometry.
    let Some(popup) = new_workspace_picker_popup_rect(area, destinations.len()) else {
        return;
    };
    let Some(inner) = render_panel_shell(frame, popup, palette.accent, palette.panel_bg) else {
        return;
    };
    debug_assert_eq!(
        Some(inner),
        new_workspace_picker_inner_rect(area, destinations.len()),
        "picker render and hit-test geometry diverged"
    );
    if inner.height < 4 {
        return;
    }

    render_modal_header(
        frame,
        Rect::new(inner.x, inner.y, inner.width, 1),
        "new workspace",
        palette,
    );
    frame.render_widget(
        Paragraph::new(" create on").style(Style::default().fg(palette.overlay0)),
        Rect::new(inner.x, inner.y.saturating_add(1), inner.width, 1),
    );

    // defensive clamp on the render-time read (selection can be out of range).
    let selected = selected.min(destinations.len().saturating_sub(1));
    // rows live between the create-on label and the actions row (inner.height - 1).
    let max_rows = inner.height.saturating_sub(3) as usize;
    for (row_index, destination) in destinations.iter().enumerate().take(max_rows) {
        let is_selected = row_index == selected;
        // a hovered, non-selected row gets a subtle theme-derived bg lift
        // (selection always wins; hover never bolds).
        let is_hovered = !is_selected && hovered_row == Some(row_index);
        let marker = if is_selected { "›" } else { " " };
        let label = format!("{marker} {}", destination.display_name);
        let style = if is_selected {
            Style::default()
                .fg(palette.text)
                .bg(palette.surface0)
                .add_modifier(Modifier::BOLD)
        } else if is_hovered {
            Style::default().fg(palette.subtext0).bg(palette.hover_bg())
        } else {
            Style::default().fg(palette.subtext0)
        };
        let row = new_workspace_picker_row_rect(inner, row_index);
        frame.render_widget(
            Paragraph::new(truncate_end(&label, inner.width as usize)).style(style),
            row,
        );
    }

    let (confirm_rect, cancel_rect) = new_workspace_picker_button_rects(inner);
    render_action_button(
        frame,
        confirm_rect,
        Some("↵"),
        "create",
        Style::default()
            .fg(panel_contrast_fg(palette))
            .bg(palette.accent)
            .add_modifier(Modifier::BOLD),
    );
    render_action_button(
        frame,
        cancel_rect,
        Some("esc"),
        "cancel",
        Style::default()
            .fg(palette.text)
            .bg(palette.surface0)
            .add_modifier(Modifier::BOLD),
    );
}

/// The manage-overlay popup height for `count` rows, derived from the row budget (header, the
/// column-label row, one row per remote, the footer hint, and vertical margins) clamped to a sane
/// band. Used by BOTH the inner-rect helper and the renderer so they cannot diverge.
fn remote_manage_popup_height(count: usize) -> u16 {
    (count as u16).saturating_add(5).clamp(8, 20)
}

/// The outer popup rect for the management overlay — footer-anchored (bottom-left of `area`,
/// opening upward) so it floats over the live content like the global launcher menu. Width 64,
/// height derived from `count`. Used by BOTH the renderer and the compositor's content-copy
/// exclusion / hit-test.
pub(crate) fn remote_manage_popup_rect(area: Rect, count: usize) -> Option<Rect> {
    bottom_left_popup_rect(area, 64, remote_manage_popup_height(count))
}

/// The SHARED inner rect for the management overlay (render + hit-test). The popup width is 64,
/// height derived from `count` exactly the way `render_remote_manage_overlay` sizes it. Returns
/// `None` when the host is too small to fit.
pub(crate) fn remote_manage_inner_rect(area: Rect, count: usize) -> Option<Rect> {
    remote_manage_popup_rect(area, count).map(|popup| {
        Rect::new(
            popup.x + 1,
            popup.y + 1,
            popup.width.saturating_sub(2),
            popup.height.saturating_sub(2),
        )
    })
}

/// The rect for remote row `row_index` inside the manage overlay's inner rect. Rows start two
/// lines below the inner top (header + column-label row). Used by BOTH render and hit-test.
pub(crate) fn remote_manage_row_rect(inner: Rect, row_index: usize) -> Rect {
    let y = inner.y.saturating_add(2 + row_index as u16);
    Rect::new(inner.x, y, inner.width, 1)
}

/// The delete-confirm popup rect (centered, smaller, red panel). Used by render + hit-test.
pub(crate) fn remote_manage_confirm_popup_rect(area: Rect) -> Option<Rect> {
    centered_popup_rect(area, 56, 8)
}

/// The (delete, cancel) button rects inside the delete-confirm popup's inner rect.
pub(crate) fn remote_manage_confirm_button_rects(inner: Rect) -> (Rect, Rect) {
    let rects = action_button_row_rects(
        inner,
        &[
            ActionButtonSpec {
                hint: Some("↵"),
                label: "delete",
            },
            ActionButtonSpec {
                hint: Some("esc"),
                label: "cancel",
            },
        ],
        2,
        inner.height.saturating_sub(1),
    );
    (rects[0], rects[1])
}

/// Render the remote-management overlay as a footer-anchored ratatui modal — a scrollable
/// selectable list of remotes with per-remote state, plus a footer hint. When
/// `confirm_delete.is_some()` the destructive two-step sub-state is drawn on top as a red
/// `render_panel_shell` popup with delete/cancel buttons. Geometry comes from
/// `remote_manage_inner_rect`/`_row_rect`/`_confirm_*` so it matches `hit_test`. Square corners
/// (`border::PLAIN`). The caller forces `frame.cursor = None` while the modal is open.
// Called by the client compositor once the compositor phase lands.
#[allow(dead_code)]
pub(crate) fn render_remote_manage_overlay(
    palette: &Palette,
    rows: &[RemoteManageRowView],
    selected: usize,
    scroll: usize,
    confirm_delete: Option<&str>,
    frame: &mut Frame,
    area: Rect,
) {
    let Some(popup) = remote_manage_popup_rect(area, rows.len()) else {
        return;
    };
    let Some(inner) = render_panel_shell(frame, popup, palette.accent, palette.panel_bg) else {
        return;
    };
    debug_assert_eq!(
        Some(inner),
        remote_manage_inner_rect(area, rows.len()),
        "manage overlay render and hit-test geometry diverged"
    );
    if inner.height < 4 {
        return;
    }

    render_modal_header(
        frame,
        Rect::new(inner.x, inner.y, inner.width, 1),
        "manage remotes",
        palette,
    );
    frame.render_widget(
        Paragraph::new(" remote                          target              state")
            .style(Style::default().fg(palette.overlay0)),
        Rect::new(inner.x, inner.y.saturating_add(1), inner.width, 1),
    );

    // rows live between the column-label row and the footer hint row (inner.height - 1).
    let max_rows = inner.height.saturating_sub(3) as usize;
    let selected = selected.min(rows.len().saturating_sub(1));
    // clamp the visible window so the selected row stays on screen (shared scroll math).
    let start = scroll
        .min(rows.len().saturating_sub(max_rows.max(1)))
        .min(selected)
        .max(selected.saturating_sub(max_rows.max(1).saturating_sub(1)));
    for (visible_idx, (row_index, row)) in rows
        .iter()
        .enumerate()
        .skip(start)
        .take(max_rows)
        .enumerate()
    {
        let is_selected = row_index == selected;
        let marker = if is_selected { "›" } else { " " };
        let enabled_word = if row.disabled { "off" } else { "on" };
        let label = format!(
            "{marker} {} {:<24} {:<18} {}  [{}]",
            row.glyph.glyph(),
            truncate_end(row.name, 24),
            truncate_end(row.target, 18),
            row.state_word,
            enabled_word
        );
        let base = if row.disabled {
            Style::default().fg(palette.overlay0)
        } else {
            Style::default().fg(palette.subtext0)
        };
        let style = if is_selected {
            Style::default()
                .fg(palette.text)
                .bg(palette.surface0)
                .add_modifier(Modifier::BOLD)
        } else {
            base
        };
        let rect = remote_manage_row_rect(inner, visible_idx);
        frame.render_widget(
            Paragraph::new(truncate_end(&label, inner.width as usize)).style(style),
            rect,
        );
    }

    frame.render_widget(
        Paragraph::new(" space toggle   d delete   a add   esc close")
            .style(Style::default().fg(palette.overlay0)),
        Rect::new(
            inner.x,
            inner.y + inner.height.saturating_sub(1),
            inner.width,
            1,
        ),
    );

    if let Some(remote_id) = confirm_delete {
        render_remote_manage_confirm(palette, rows, selected, remote_id, frame, area);
    }
}

/// Render the destructive delete-confirm sub-state as a red panel popup over the list.
fn render_remote_manage_confirm(
    palette: &Palette,
    rows: &[RemoteManageRowView],
    selected: usize,
    _remote_id: &str,
    frame: &mut Frame,
    area: Rect,
) {
    let Some(popup) = remote_manage_confirm_popup_rect(area) else {
        return;
    };
    let Some(inner) = render_panel_shell(frame, popup, palette.red, palette.panel_bg) else {
        return;
    };
    if inner.height < 4 {
        return;
    }

    let name = rows.get(selected).map(|row| row.name).unwrap_or("remote");
    frame.render_widget(
        Paragraph::new(Line::from(vec![Span::styled(
            " delete remote?",
            Style::default()
                .fg(palette.red)
                .add_modifier(Modifier::BOLD),
        )])),
        Rect::new(inner.x, inner.y, inner.width, 1),
    );
    frame.render_widget(
        Paragraph::new(format!(" {name} will be removed from the registry."))
            .style(Style::default().fg(palette.text)),
        Rect::new(inner.x, inner.y.saturating_add(1), inner.width, 1),
    );
    frame.render_widget(
        Paragraph::new(" Active workspaces on it are not deleted.")
            .style(Style::default().fg(palette.overlay0)),
        Rect::new(inner.x, inner.y.saturating_add(2), inner.width, 1),
    );

    let (delete_rect, cancel_rect) = remote_manage_confirm_button_rects(inner);
    render_action_button(
        frame,
        delete_rect,
        Some("↵"),
        "delete",
        Style::default()
            .fg(panel_contrast_fg(palette))
            .bg(palette.red)
            .add_modifier(Modifier::BOLD),
    );
    render_action_button(
        frame,
        cancel_rect,
        Some("esc"),
        "cancel",
        Style::default()
            .fg(palette.text)
            .bg(palette.surface0)
            .add_modifier(Modifier::BOLD),
    );
}

// ----- workspace context menu + rename + confirm-close geometry/render -------------------------
// Single-source-of-truth geometry for the three client workspace overlays, mirroring the
// add-remote / manage-remotes modals above. Both the renderer and the compositor's hit test
// derive every row/button rect from these helpers, so render geometry == hit-test geometry.

/// The context-menu popup height for `count` rows: header + the workspace-name sub-row + one row
/// per item + vertical margins, clamped to a sane band. Used by BOTH the inner-rect helper and the
/// renderer so they cannot diverge. Mirrors `picker_popup_height`.
fn workspace_context_menu_popup_height(count: usize) -> u16 {
    (count as u16).saturating_add(4).clamp(6, 12)
}

/// The outer popup rect for the workspace context menu — footer-anchored (bottom-left of `area`,
/// opening upward) like the other client overlays. Used by BOTH render and hit-test.
pub(crate) fn workspace_context_menu_popup_rect(area: Rect, count: usize) -> Option<Rect> {
    bottom_left_popup_rect(area, 34, workspace_context_menu_popup_height(count))
}

/// Shared inner rect for the workspace context menu. Returns `None` when the host is too small.
pub(crate) fn workspace_context_menu_inner_rect(area: Rect, count: usize) -> Option<Rect> {
    workspace_context_menu_popup_rect(area, count).map(|popup| {
        Rect::new(
            popup.x + 1,
            popup.y + 1,
            popup.width.saturating_sub(2),
            popup.height.saturating_sub(2),
        )
    })
}

/// The rect for menu row `row_index` inside the context menu's inner rect. Rows start two lines
/// below the inner top (header + workspace-name sub-row). Used by BOTH render and hit-test.
pub(crate) fn workspace_context_menu_row_rect(inner: Rect, row_index: usize) -> Rect {
    let y = inner.y.saturating_add(2 + row_index as u16);
    Rect::new(inner.x, y, inner.width, 1)
}

/// The outer popup rect for the rename overlay — footer-anchored, like add-remote.
pub(crate) fn rename_workspace_popup_rect(area: Rect) -> Option<Rect> {
    bottom_left_popup_rect(area, 48, 7)
}

/// Fixed inner rect for the rename overlay. Returns `None` when the host is too small.
pub(crate) fn rename_workspace_inner_rect(area: Rect) -> Option<Rect> {
    rename_workspace_popup_rect(area).map(|popup| {
        Rect::new(
            popup.x + 1,
            popup.y + 1,
            popup.width.saturating_sub(2),
            popup.height.saturating_sub(2),
        )
    })
}

/// The (save, cancel) button rects inside the rename overlay's inner rect. Mirrors
/// `add_remote_button_rects`.
pub(crate) fn rename_workspace_button_rects(inner: Rect) -> (Rect, Rect) {
    let rects = action_button_row_rects(
        inner,
        &[
            ActionButtonSpec {
                hint: Some("↵"),
                label: "save",
            },
            ActionButtonSpec {
                hint: Some("esc"),
                label: "cancel",
            },
        ],
        2,
        inner.height.saturating_sub(1),
    );
    (rects[0], rects[1])
}

/// The close-confirm popup rect (centered red panel), mirroring `remote_manage_confirm_popup_rect`.
pub(crate) fn confirm_close_workspace_popup_rect(area: Rect) -> Option<Rect> {
    centered_popup_rect(area, 48, 6)
}

/// The (close, cancel) button rects inside the close-confirm popup's inner rect.
pub(crate) fn confirm_close_workspace_button_rects(inner: Rect) -> (Rect, Rect) {
    let rects = action_button_row_rects(
        inner,
        &[
            ActionButtonSpec {
                hint: Some("↵"),
                label: "close",
            },
            ActionButtonSpec {
                hint: Some("esc"),
                label: "cancel",
            },
        ],
        2,
        inner.height.saturating_sub(1),
    );
    (rects[0], rects[1])
}

/// Render the workspace context menu as a footer-anchored accent panel — a selectable list of
/// menu items with the workspace name as a sub-header. Geometry comes from
/// `workspace_context_menu_inner_rect`/`_row_rect` so it matches `hit_test`. Mirrors
/// `render_new_workspace_picker_overlay`.
// Called by the client compositor once the compositor phase lands.
#[allow(dead_code)]
pub(crate) fn render_workspace_context_menu_overlay(
    palette: &Palette,
    view: &WorkspaceContextMenuView,
    selected: usize,
    frame: &mut Frame,
    area: Rect,
) {
    let Some(popup) = workspace_context_menu_popup_rect(area, view.rows.len()) else {
        return;
    };
    let Some(inner) = render_panel_shell(frame, popup, palette.accent, palette.panel_bg) else {
        return;
    };
    debug_assert_eq!(
        Some(inner),
        workspace_context_menu_inner_rect(area, view.rows.len()),
        "context menu render and hit-test geometry diverged"
    );
    if inner.height < 4 {
        return;
    }

    render_modal_header(
        frame,
        Rect::new(inner.x, inner.y, inner.width, 1),
        "workspace",
        palette,
    );
    frame.render_widget(
        Paragraph::new(format!(
            " {}",
            truncate_end(view.label, inner.width as usize)
        ))
        .style(Style::default().fg(palette.overlay0)),
        Rect::new(inner.x, inner.y.saturating_add(1), inner.width, 1),
    );

    let selected = selected.min(view.rows.len().saturating_sub(1));
    let max_rows = inner.height.saturating_sub(2) as usize;
    for (row_index, label) in view.rows.iter().enumerate().take(max_rows) {
        let is_selected = row_index == selected;
        let marker = if is_selected { "›" } else { " " };
        let text = format!("{marker} {label}");
        let style = if is_selected {
            Style::default()
                .fg(palette.text)
                .bg(palette.surface0)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(palette.subtext0)
        };
        let row = workspace_context_menu_row_rect(inner, row_index);
        frame.render_widget(
            Paragraph::new(truncate_end(&text, inner.width as usize)).style(style),
            row,
        );
    }
}

/// Render the rename text overlay as a footer-anchored accent panel with a single focused text
/// field + save/cancel buttons. Mirrors `render_add_remote_overlay`. The caller forces
/// `frame.cursor = None` while the modal is open.
// Called by the client compositor once the compositor phase lands.
#[allow(dead_code)]
pub(crate) fn render_rename_workspace_overlay(
    palette: &Palette,
    label: &str,
    error: Option<&str>,
    frame: &mut Frame,
    area: Rect,
) {
    let Some(popup) = rename_workspace_popup_rect(area) else {
        return;
    };
    let Some(inner) = render_panel_shell(frame, popup, palette.accent, palette.panel_bg) else {
        return;
    };
    debug_assert_eq!(
        Some(inner),
        rename_workspace_inner_rect(area),
        "rename render and hit-test geometry diverged"
    );
    if inner.height < 4 {
        return;
    }

    let rows = Layout::vertical([
        Constraint::Length(1), // header
        Constraint::Length(1), // input
        Constraint::Length(1), // error
        Constraint::Min(0),    // actions live on inner.height - 1
    ])
    .areas::<4>(inner);

    render_modal_header(frame, rows[0], "rename workspace", palette);
    render_add_remote_field(frame, rows[1], "label", label, true, palette);

    if let Some(error) = error {
        frame.render_widget(
            Paragraph::new(format!(" {error}")).style(Style::default().fg(palette.red)),
            rows[2],
        );
    }

    let (save_rect, cancel_rect) = rename_workspace_button_rects(inner);
    render_action_button(
        frame,
        save_rect,
        Some("↵"),
        "save",
        Style::default()
            .fg(panel_contrast_fg(palette))
            .bg(palette.accent)
            .add_modifier(Modifier::BOLD),
    );
    render_action_button(
        frame,
        cancel_rect,
        Some("esc"),
        "cancel",
        Style::default()
            .fg(palette.text)
            .bg(palette.surface0)
            .add_modifier(Modifier::BOLD),
    );
}

/// Render the close-confirm overlay as a centered red panel ("Close <label>?") with close/cancel
/// buttons. Mirrors `render_remote_manage_confirm`.
// Called by the client compositor once the compositor phase lands.
#[allow(dead_code)]
pub(crate) fn render_confirm_close_workspace_overlay(
    palette: &Palette,
    label: &str,
    frame: &mut Frame,
    area: Rect,
) {
    let Some(popup) = confirm_close_workspace_popup_rect(area) else {
        return;
    };
    let Some(inner) = render_panel_shell(frame, popup, palette.red, palette.panel_bg) else {
        return;
    };
    if inner.height < 3 {
        return;
    }

    frame.render_widget(
        Paragraph::new(Line::from(vec![Span::styled(
            " close workspace?",
            Style::default()
                .fg(palette.red)
                .add_modifier(Modifier::BOLD),
        )])),
        Rect::new(inner.x, inner.y, inner.width, 1),
    );
    frame.render_widget(
        Paragraph::new(format!(
            " Close {}?",
            truncate_end(label, inner.width.saturating_sub(8) as usize)
        ))
        .style(Style::default().fg(palette.text)),
        Rect::new(inner.x, inner.y.saturating_add(1), inner.width, 1),
    );

    let (close_rect, cancel_rect) = confirm_close_workspace_button_rects(inner);
    render_action_button(
        frame,
        close_rect,
        Some("↵"),
        "close",
        Style::default()
            .fg(panel_contrast_fg(palette))
            .bg(palette.red)
            .add_modifier(Modifier::BOLD),
    );
    render_action_button(
        frame,
        cancel_rect,
        Some("esc"),
        "cancel",
        Style::default()
            .fg(palette.text)
            .bg(palette.surface0)
            .add_modifier(Modifier::BOLD),
    );
}

#[cfg(test)]
mod tests {
    use crate::{
        app::{state::WorktreeCreateState, AppState},
        workspace::Workspace,
    };
    use ratatui::{backend::TestBackend, layout::Rect, Terminal};

    use super::{
        add_remote_button_rects, add_remote_inner_rect, confirm_close_overlay_text,
        new_workspace_picker_button_rects, new_workspace_picker_inner_rect,
        new_workspace_picker_row_rect, render_new_linked_worktree_overlay,
    };

    fn rects_are_disjoint(a: Rect, b: Rect) -> bool {
        a.x + a.width <= b.x || b.x + b.width <= a.x
    }

    fn rect_within(child: Rect, parent: Rect) -> bool {
        child.x >= parent.x
            && child.y >= parent.y
            && child.x + child.width <= parent.x + parent.width
            && child.y + child.height <= parent.y + parent.height
    }

    #[test]
    fn add_remote_button_rects_lays_out_two_centered_buttons() {
        let area = Rect::new(0, 0, 80, 24);
        let inner = add_remote_inner_rect(area).expect("modal fits");
        let (submit, cancel) = add_remote_button_rects(inner);

        assert_eq!(submit.height, 1);
        assert_eq!(cancel.height, 1);
        assert!(rects_are_disjoint(submit, cancel));
        assert!(rect_within(submit, inner));
        assert!(rect_within(cancel, inner));
        // submit is to the left of cancel.
        assert!(submit.x < cancel.x);
    }

    #[test]
    fn new_workspace_picker_button_rects_lays_out_two_centered_buttons() {
        let area = Rect::new(0, 0, 80, 24);
        let inner = new_workspace_picker_inner_rect(area, 3).expect("modal fits");
        let (confirm, cancel) = new_workspace_picker_button_rects(inner);

        assert_eq!(confirm.height, 1);
        assert_eq!(cancel.height, 1);
        assert!(rects_are_disjoint(confirm, cancel));
        assert!(rect_within(confirm, inner));
        assert!(rect_within(cancel, inner));
        assert!(confirm.x < cancel.x);
    }

    #[test]
    fn new_workspace_picker_inner_rect_is_shared_geometry() {
        let area = Rect::new(0, 0, 80, 24);
        let count = 3usize;
        let inner = new_workspace_picker_inner_rect(area, count).expect("modal fits");

        // every destination row resolves inside the inner rect — the same helper render uses.
        for n in 0..count {
            let row = new_workspace_picker_row_rect(inner, n);
            assert!(
                rect_within(row, inner),
                "row {n} {row:?} not within inner {inner:?}"
            );
        }
        // a row past the count is allowed to spill below; the render-time max_rows clamp guards it.
    }

    #[test]
    fn remote_manage_geometry_is_shared_between_render_and_hit_test() {
        let area = Rect::new(0, 0, 100, 30);
        let inner = super::remote_manage_inner_rect(area, 2).expect("modal fits");
        for n in 0..2 {
            let row = super::remote_manage_row_rect(inner, n);
            assert!(rect_within(row, inner), "row {n} within inner");
        }
        let confirm_popup = super::remote_manage_confirm_popup_rect(area).expect("confirm fits");
        let confirm_inner = Rect::new(
            confirm_popup.x + 1,
            confirm_popup.y + 1,
            confirm_popup.width - 2,
            confirm_popup.height - 2,
        );
        let (delete, cancel) = super::remote_manage_confirm_button_rects(confirm_inner);
        assert!(rects_are_disjoint(delete, cancel));
        assert!(rect_within(delete, confirm_inner));
        assert!(rect_within(cancel, confirm_inner));
    }

    #[test]
    fn workspace_context_menu_geometry_is_shared() {
        let area = Rect::new(0, 0, 80, 24);
        let inner = super::workspace_context_menu_inner_rect(area, 2).expect("menu fits");
        for n in 0..2 {
            let row = super::workspace_context_menu_row_rect(inner, n);
            assert!(rect_within(row, inner), "row {n} within inner");
        }
    }

    #[test]
    fn rename_and_confirm_close_workspace_button_rects_fit_inner() {
        let area = Rect::new(0, 0, 80, 24);
        let rename_inner = super::rename_workspace_inner_rect(area).expect("rename fits");
        let (save, cancel) = super::rename_workspace_button_rects(rename_inner);
        assert!(rects_are_disjoint(save, cancel));
        assert!(rect_within(save, rename_inner));
        assert!(rect_within(cancel, rename_inner));

        let confirm_popup = super::confirm_close_workspace_popup_rect(area).expect("confirm fits");
        let confirm_inner = Rect::new(
            confirm_popup.x + 1,
            confirm_popup.y + 1,
            confirm_popup.width - 2,
            confirm_popup.height - 2,
        );
        let (close, cancel) = super::confirm_close_workspace_button_rects(confirm_inner);
        assert!(rects_are_disjoint(close, cancel));
        assert!(rect_within(close, confirm_inner));
        assert!(rect_within(cancel, confirm_inner));
    }

    #[test]
    fn remote_manage_overlay_renders_rows_and_states() {
        let app = AppState::test_new();
        let glyph_rows = [
            super::RemoteManageRowView {
                glyph: super::RemoteStateGlyph::Connected,
                name: "macmini",
                target: "mini.local",
                state_word: "connected",
                disabled: false,
            },
            super::RemoteManageRowView {
                glyph: super::RemoteStateGlyph::Connecting,
                name: "lab",
                target: "lab.local",
                state_word: "connecting",
                disabled: false,
            },
            super::RemoteManageRowView {
                glyph: super::RemoteStateGlyph::Disconnected,
                name: "edge",
                target: "edge.local",
                state_word: "offline",
                disabled: false,
            },
            super::RemoteManageRowView {
                glyph: super::RemoteStateGlyph::Disabled,
                name: "spare",
                target: "spare.local",
                state_word: "disabled",
                disabled: true,
            },
            super::RemoteManageRowView {
                glyph: super::RemoteStateGlyph::ProtocolMismatch,
                name: "old",
                target: "old.local",
                state_word: "protocol mismatch",
                disabled: false,
            },
        ];

        let mut terminal =
            Terminal::new(TestBackend::new(100, 30)).expect("test terminal should initialize");
        terminal
            .draw(|frame| {
                super::render_remote_manage_overlay(
                    &app.palette,
                    &glyph_rows,
                    0,
                    0,
                    None,
                    frame,
                    Rect::new(0, 0, 100, 30),
                )
            })
            .expect("manage overlay should render");

        let rendered = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(rendered.contains("manage remotes"));
        assert!(rendered.contains("macmini"));
        assert!(rendered.contains("connected"));
        assert!(rendered.contains("offline"));
        assert!(rendered.contains("disabled"));
        // Every glyph row is present ("protocol mismatch" itself may be width-clipped
        // in the fixed 64-col popup, so assert the row's name instead).
        assert!(rendered.contains("old.local"));
        assert!(rendered.contains("space toggle"));
    }

    #[test]
    fn add_remote_overlay_renders_progress_and_restart_prompt() {
        let app = AppState::test_new();

        // in-progress: the animated connecting line is shown instead of an error.
        let mut terminal =
            Terminal::new(TestBackend::new(80, 24)).expect("test terminal should initialize");
        terminal
            .draw(|frame| {
                super::render_add_remote_overlay(
                    &app.palette,
                    &super::AddRemoteOverlayView {
                        target: "user@mini",
                        name: "mini",
                        focused_is_target: true,
                        error: Some("stale error"),
                        in_progress: true,
                        spinner: "⠋",
                        restart_confirm_destination: None,
                    },
                    frame,
                    Rect::new(0, 0, 80, 24),
                )
            })
            .expect("add-remote overlay should render");
        let rendered = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(rendered.contains("add remote"));
        assert!(rendered.contains("user@mini"));
        assert!(rendered.contains("connecting to remote"));
        assert!(!rendered.contains("stale error"));

        // restart-confirm wins over both progress and error.
        let mut terminal =
            Terminal::new(TestBackend::new(80, 24)).expect("test terminal should initialize");
        terminal
            .draw(|frame| {
                super::render_add_remote_overlay(
                    &app.palette,
                    &super::AddRemoteOverlayView {
                        target: "user@mini",
                        name: "mini",
                        focused_is_target: false,
                        error: None,
                        in_progress: true,
                        spinner: "⠋",
                        restart_confirm_destination: Some("user@mini"),
                    },
                    frame,
                    Rect::new(0, 0, 80, 24),
                )
            })
            .expect("add-remote overlay should render");
        let rendered = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        // The restart prompt wins over the in-progress line (the trailing `[y/N]` may be
        // width-clipped in the 54-col popup, so assert the stable prompt prefix).
        assert!(rendered.contains("restart it?"));
        assert!(!rendered.contains("connecting to remote"));
    }

    #[test]
    fn dialogs_does_not_reference_client_supervisor() {
        // The `ui` layer must not depend on supervisor-model types. `render_*_overlay` take
        // ui-owned view structs only. Build the forbidden path token at runtime so it never
        // appears verbatim in this file (which the guard scans).
        let forbidden = format!("{}::{}::{}", "crate", "client", "supervisor");
        let source = include_str!("dialogs.rs");
        assert!(
            !source.contains(&forbidden),
            "src/ui/dialogs.rs must not reference the supervisor module path"
        );
        // also reject the shorter relative form.
        let forbidden_relative = format!("{}::{}", "client", "supervisor");
        assert!(
            !source.contains(&forbidden_relative),
            "src/ui/dialogs.rs must not reference the supervisor module path"
        );
    }

    #[test]
    fn confirm_close_text_reports_parent_group_scope() {
        let mut app = AppState::test_new();
        let mut parent = Workspace::test_new("main");
        parent.worktree_space = Some(crate::workspace::WorktreeSpaceMembership {
            key: "repo-key".into(),
            label: "herdr".into(),
            repo_root: "/repo/herdr".into(),
            checkout_path: "/repo/herdr".into(),
            is_linked_worktree: false,
        });
        let mut child = Workspace::test_new("issue");
        child.worktree_space = Some(crate::workspace::WorktreeSpaceMembership {
            key: "repo-key".into(),
            label: "herdr".into(),
            repo_root: "/repo/herdr".into(),
            checkout_path: "/repo/herdr-issue".into(),
            is_linked_worktree: true,
        });
        app.workspaces = vec![parent, child];
        app.selected = 0;

        let (title, detail) = confirm_close_overlay_text(&app);

        assert_eq!(title, "Close worktree group?");
        assert_eq!(detail, "main — 2 workspaces, 2 panes");
    }

    #[test]
    fn new_worktree_error_renders_fatal_stderr_line() {
        let mut app = AppState::test_new();
        app.name_input = "foo".into();
        app.worktree_create = Some(WorktreeCreateState {
            source_workspace_id: "source".into(),
            source_checkout_path: "/repo/herdr".into(),
            source_existing_membership: None,
            source_repo_root: "/repo/herdr".into(),
            repo_key: "repo-key".into(),
            repo_name: "herdr".into(),
            branch: "foo".into(),
            checkout_path: "/repo/.worktrees/herdr/foo".into(),
            error: Some(
                "Preparing worktree (new branch 'foo')\nfatal: a branch named 'foo' already exists"
                    .into(),
            ),
            creating: false,
        });

        let mut terminal =
            Terminal::new(TestBackend::new(100, 30)).expect("test terminal should initialize");
        terminal
            .draw(|frame| render_new_linked_worktree_overlay(&app, frame, Rect::new(0, 0, 100, 30)))
            .expect("new worktree overlay should render");
        let rendered = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();

        assert!(rendered.contains("fatal: a branch named 'foo' already exists"));
    }

    #[test]
    fn new_worktree_hit_test_geometry_matches_modal_size() {
        let area = Rect::new(0, 0, 100, 30);
        let inner = super::new_linked_worktree_inner_rect(area).unwrap();
        let (create, cancel) = super::new_linked_worktree_button_rects(inner);

        assert_eq!(inner.width, super::NEW_LINKED_WORKTREE_POPUP_WIDTH - 2);
        assert_eq!(inner.height, super::NEW_LINKED_WORKTREE_POPUP_HEIGHT - 2);
        assert_eq!(create.y, inner.y + inner.height - 1);
        assert_eq!(cancel.y, inner.y + inner.height - 1);
    }
}
