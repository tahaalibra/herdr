//! Thin client mode — connects to the server's client socket.
//!
//! The client:
//! - Connects to `herdr-client.sock`, sends Hello with terminal size and protocol version
//! - Sets up the real terminal (raw mode, mouse capture, keyboard enhancements)
//! - Receives Frame messages and blits them to the terminal (diff against last frame)
//! - Reads stdin events (keystrokes, mouse, paste) and sends them as ClientMessage::Input
//! - Detects terminal resize and sends ClientMessage::Resize
//! - Restores terminal on exit (normal or error)
//! - Handles ServerShutdown gracefully (clean exit, informative message to stderr)
//! - Handles server unreachable (clear error screen, not blank/hang)
//! - Forwards OSC 52 clipboard writes from server to its own stdout
//! - Displays sound/toast notifications forwarded from server

// The client compositor and multi-server supervisor drive the mixed-server
// client, which needs the ssh remote bridge (`crate::remote` real impl) and is
// therefore unix-only, mirroring the PoC. Windows keeps the single-server
// client untouched.
#[cfg(unix)]
mod compositor;
mod input;
#[cfg(unix)]
mod supervisor;

#[cfg(unix)]
use std::collections::HashMap;
use std::collections::HashSet;
use std::io::{self, BufRead, Write as _};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;
#[cfg(unix)]
use std::time::Instant;

use base64::Engine;
use crossterm::event::{
    DisableBracketedPaste, DisableFocusChange, DisableMouseCapture, EnableBracketedPaste,
    EnableFocusChange, EnableMouseCapture,
};
#[cfg(unix)]
use crossterm::event::{
    KeyCode, KeyEventKind, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};
#[cfg(not(windows))]
use crossterm::event::{PopKeyboardEnhancementFlags, PushKeyboardEnhancementFlags};
use crossterm::execute;
use interprocess::local_socket::traits::Stream as _;
use interprocess::TryClone as _;
use tracing::{debug, info, warn};

use crate::ipc::LocalStream;
use crate::protocol::render_ansi;
use crate::protocol::{
    self, AttachScrollDirection, AttachScrollSource, ClientKeybindings, ClientLaunchMode,
    ClientMessage, NotifyKind, RenderEncoding, ServerMessage, MAX_FRAME_SIZE,
    MAX_GRAPHICS_FRAME_SIZE, PROTOCOL_VERSION,
};
#[cfg(unix)]
use crate::protocol::{ClientSurfaceMode, MAX_CLIPBOARD_IMAGE_PAYLOAD};
use crate::server::socket_paths::client_socket_path;

static RECEIVED_KITTY_GRAPHICS_IDS: OnceLock<Mutex<HashSet<u32>>> = OnceLock::new();
/// Background cadence for the local main/registry/ui-settings refresh and non-active
/// secondary summary polls.
#[cfg(unix)]
const CLIENT_SUPERVISOR_REFRESH_INTERVAL: Duration = Duration::from_secs(2);
/// item 6 (Area 6): the focused-remote summary poll cadence. The active remote polls at 400ms
/// (vs the 2s background cadence) so a focus or in-flight change reconciles within one round-trip.
#[cfg(unix)]
const CLIENT_FOCUSED_SUMMARY_REFRESH_INTERVAL: Duration = Duration::from_millis(400);
#[cfg(unix)]
const CLIENT_SUPERVISOR_API_TIMEOUT: Duration = Duration::from_secs(2);
#[cfg(unix)]
const CLIENT_60FPS_FRAME_BUDGET: Duration = Duration::from_micros(16_667);
// item 5: the single client animation cadence. 80ms / step 8 advances exactly one visible
// spinner frame per interval (`spinner_frame` maps `tick/8`), i.e. ~12.5 fps. SSH remotes use
// the SAME cadence (the recompose is local; link speed only affects the encoded diff).
#[cfg(unix)]
const CLIENT_ANIMATION_INTERVAL: Duration = Duration::from_millis(80);
#[cfg(unix)]
const CLIENT_ANIMATION_TICK_STEP: u32 = 8;
#[cfg(unix)]
const ADD_REMOTE_TARGET_VALIDATE_TIMEOUT: Duration = Duration::from_secs(5);
#[cfg(unix)]
const ADD_REMOTE_TARGET_VALIDATE_RETRY_DELAY: Duration = Duration::from_millis(50);
// Hard ceiling for bringing up an ssh remote bridge (connect + detect + auto-install + server
// start). Without this an unreachable/slow/auth-prompting host would block the add-remote worker
// forever, leaving the dialog stuck on its in-progress state with no error. Generous enough to
// cover a real binary install over ssh; the ssh `ConnectTimeout` bounds the unreachable case.
#[cfg(unix)]
const ADD_REMOTE_BRIDGE_TIMEOUT: Duration = Duration::from_secs(90);
/// How often the downstream-throughput sampler converts cumulative byte counters into a
/// bytes/sec rate for the host banner (issue #13). ~1s gives a steady, readable number.
#[cfg(unix)]
const RX_RATE_SAMPLE_INTERVAL: Duration = Duration::from_millis(1000);
/// Cadence for stream latency probes (issue #13). A Ping over the persistent stream measures true
/// round-trip time without the per-request bridge-connection / remote-process-spawn cost.
#[cfg(unix)]
const SERVER_PING_INTERVAL: Duration = Duration::from_millis(1000);

// ---------------------------------------------------------------------------
// Client state
// ---------------------------------------------------------------------------

struct ClientLoopOptions {
    sound_config: crate::config::SoundConfig,
    mouse_scroll_lines: usize,
    redraw_on_focus_gained: bool,
    host_cursor: crate::config::HostCursorModeConfig,
    kitty_graphics_enabled: bool,
    mouse_capture_active: bool,
    #[cfg(unix)]
    remote_image_paste_key: Option<(crossterm::event::KeyCode, crossterm::event::KeyModifiers)>,
    /// The outer host terminal size at startup (the compositor owns it in mixed mode).
    host_size: (u16, u16),
    /// The size reported to the server in Hello (the content column in mixed mode).
    reported_size: (u16, u16),
    /// Last known host cell size in pixels, used for secondary handshakes.
    #[cfg(unix)]
    cell_size_px: (u32, u32),
    /// Client-owned sidebar/frame compositor used for mixed-server sessions.
    #[cfg(unix)]
    compositor: Option<compositor::ClientCompositor>,
    /// Runtime multi-server summary state used by the client-owned sidebar.
    #[cfg(unix)]
    supervisor_model: Option<supervisor::ClientSupervisorModel>,
}

/// State tracking for the thin client.
struct ClientState {
    /// Stateful semantic-frame encoder used when the server sends FrameData.
    blit_encoder: render_ansi::BlitEncoder,
    /// Last full server frame, exactly as received, used as the baseline for
    /// `ServerMessage::FrameDelta` reconstruction. Kept separate from the blit
    /// encoder's committed frame, which may contain a locally drawn cursor.
    /// On unix the per-server `frame_cache` plays this role instead.
    #[cfg(windows)]
    server_frame_baseline: Option<protocol::FrameData>,
    /// Whether host mouse capture is currently active.
    mouse_capture_active: bool,
    /// The terminal size we reported to the server in our last Hello/Resize.
    reported_size: (u16, u16),
    /// The outer terminal size owned by the client compositor in mixed mode.
    #[cfg(unix)]
    host_size: (u16, u16),
    /// Last known host cell size in pixels, used for secondary handshakes.
    #[cfg(unix)]
    cell_size_px: (u32, u32),
    /// Client-local sound playback config, refreshed on server request.
    sound_config: crate::config::SoundConfig,
    /// Whether this client may write Kitty graphics bytes to its host terminal.
    kitty_graphics_enabled: bool,
    /// Direct attach prefix escape state. None for full-app clients.
    attach_escape: Option<AttachEscapeState>,
    /// Rows scrolled for one direct-attach wheel notch.
    #[cfg(unix)]
    mouse_scroll_lines: usize,
    /// Local-client shortcut that sends a clipboard image to a remote Herdr session.
    #[cfg(unix)]
    remote_image_paste_key: Option<(crossterm::event::KeyCode, crossterm::event::KeyModifiers)>,
    /// Whether outer focus gain should force a full host-terminal redraw.
    redraw_on_focus_gained: bool,
    /// Whether this client draws the cursor into frame cells instead of using the host cursor.
    draw_host_cursor: bool,
    /// Client-side frame timing stats for render FPS diagnostics.
    #[cfg(unix)]
    frame_stats: ClientFrameStats,
    /// Client-owned sidebar/frame compositor used for mixed-server sessions.
    #[cfg(unix)]
    compositor: Option<compositor::ClientCompositor>,
    /// Runtime multi-server summary state used by the client-owned sidebar.
    #[cfg(unix)]
    supervisor_model: Option<supervisor::ClientSupervisorModel>,
    /// Last time the client refreshed the local main/registry/ui-settings state.
    #[cfg(unix)]
    last_supervisor_summary_refresh: Instant,
    /// Last full semantic frame received from each connected server stream, exactly as
    /// received. Doubles as the per-server `FrameDelta` baseline and the composited
    /// switch cache (single-server mode keys everything on `ServerId::main()`).
    #[cfg(unix)]
    frame_cache: HashMap<supervisor::ServerId, protocol::FrameData>,
    /// issue #13: per-server cumulative downstream bytes (fed by reader threads).
    #[cfg(unix)]
    rx_counters: RxByteCounters,
    /// issue #13: last sampled (bytes, instant) per server, for deriving the banner bytes/sec rate.
    #[cfg(unix)]
    server_rx_sample: HashMap<supervisor::ServerId, (u64, Instant)>,
    /// issue #13: last time the downstream-rate sampler ran.
    #[cfg(unix)]
    last_rx_sample_at: Instant,
    /// issue #13: monotonic nonce for stream latency probes.
    #[cfg(unix)]
    ping_nonce: u64,
    /// issue #13: outstanding ping per server (nonce + send time) awaiting a Pong.
    #[cfg(unix)]
    pending_pings: HashMap<supervisor::ServerId, (u64, Instant)>,
    /// issue #13: last time stream latency probes were sent.
    #[cfg(unix)]
    last_ping_at: Instant,
    /// Servers with active summary-event subscription workers.
    #[cfg(unix)]
    summary_subscription_server_ids: HashSet<supervisor::ServerId>,
    /// Secondary servers with a summary refresh already running off the UI loop.
    #[cfg(unix)]
    pending_summary_refresh_server_ids: HashSet<supervisor::ServerId>,
    /// Servers whose change event arrived WHILE a summary fetch was already in
    /// flight. The in-flight fetch may have read pre-change state, so a rerun
    /// fires as soon as it completes instead of waiting for the next poll tick.
    #[cfg(unix)]
    queued_summary_refresh_server_ids: HashSet<supervisor::ServerId>,
    /// Secondary servers with a client-stream connection attempt running off the UI loop.
    #[cfg(unix)]
    pending_secondary_connect_server_ids: HashSet<supervisor::ServerId>,
    /// Whether an add-remote submission is running off the UI loop.
    #[cfg(unix)]
    pending_add_remote: bool,
    /// SSH bridges owned by this client for secondary servers. Kept alive here so the
    /// per-remote master connection outlives individual requests; torn down on
    /// remove/disable/disconnect.
    #[cfg(unix)]
    ssh_bridges: HashMap<supervisor::ServerId, crate::remote::RemoteBridge>,
    /// Backoff state for secondary servers that should be reconnected.
    #[cfg(unix)]
    secondary_retries: HashMap<supervisor::ServerId, SecondaryRetryState>,
    /// item 5: last time the client advanced the sidebar animation tick (80ms cadence).
    #[cfg(unix)]
    last_animation_tick: Instant,
    /// item 6 (Area 6): last time each connected secondary's summary refresh was STARTED. Drives
    /// the adaptive cadence in `due_secondary_summary_refreshes` (400ms active / 2s background)
    /// and is recorded on start and on completion so a slow SSH fetch does not stack.
    #[cfg(unix)]
    last_summary_refresh: HashMap<supervisor::ServerId, Instant>,
}

#[cfg(unix)]
#[derive(Debug, Clone, Copy)]
struct SecondaryRetryState {
    attempt: usize,
    next_retry_at: Instant,
}

#[cfg(unix)]
struct SecondaryConnectionAttempt {
    stream: LocalStream,
    bridge: Option<crate::remote::RemoteBridge>,
}

#[cfg(unix)]
struct ClientAddRemoteSuccess {
    remote: crate::remote_registry::RemoteDefinitionSnapshot,
    stream: LocalStream,
    bridge: Option<crate::remote::RemoteBridge>,
}

#[cfg(unix)]
#[derive(Clone)]
struct ServerWriteHandle {
    tx: std::sync::mpsc::Sender<ClientMessage>,
}

#[cfg(unix)]
#[derive(Debug, Default)]
struct ClientFrameStats {
    last_render_duration: Option<Duration>,
    last_render_fps: Option<f64>,
}

#[cfg(unix)]
#[derive(Debug, Clone, Copy, PartialEq)]
struct ClientFrameSample {
    render_duration: Duration,
    render_fps: f64,
    missed_sixty_fps_budget: bool,
}

#[cfg(unix)]
impl ClientFrameStats {
    fn record_render_duration(&mut self, render_duration: Duration) -> ClientFrameSample {
        let render_fps = fps_for_frame_duration(render_duration);
        let sample = ClientFrameSample {
            render_duration,
            render_fps,
            missed_sixty_fps_budget: render_duration > CLIENT_60FPS_FRAME_BUDGET,
        };
        self.last_render_duration = Some(render_duration);
        self.last_render_fps = Some(render_fps);
        sample
    }
}

#[cfg(unix)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ClientRenderPlan {
    surface_mode: ClientSurfaceMode,
    requested_encoding: RenderEncoding,
    server_size: (u16, u16),
    use_client_compositor: bool,
}

#[derive(Debug, Default)]
#[cfg(windows)]
struct AttachEscapeState;

#[derive(Debug, Default)]
#[cfg(unix)]
struct AttachEscapeState {
    pending_prefix: bool,
}

#[derive(Debug)]
#[cfg(unix)]
enum AttachInputAction {
    Forward(Vec<u8>),
    Scroll {
        source: AttachScrollSource,
        direction: AttachScrollDirection,
        lines: u16,
        column: Option<u16>,
        row: Option<u16>,
        modifiers: u8,
    },
    Detach,
    None,
}

impl AttachEscapeState {
    #[cfg(unix)]
    fn filter_input(
        &mut self,
        data: Vec<u8>,
        viewport_rows: u16,
        mouse_scroll_lines: usize,
    ) -> AttachInputAction {
        const PREFIX: u8 = 0x02; // Ctrl+B

        let mut output = Vec::with_capacity(data.len());
        for byte in data {
            if self.pending_prefix {
                self.pending_prefix = false;
                match byte {
                    b'q' => return AttachInputAction::Detach,
                    PREFIX => output.push(PREFIX),
                    other => {
                        output.push(PREFIX);
                        output.push(other);
                    }
                }
                continue;
            }

            if byte == PREFIX {
                self.pending_prefix = true;
            } else {
                output.push(byte);
            }
        }

        if output.is_empty() {
            AttachInputAction::None
        } else if let Some(action) =
            attach_scroll_action(&output, viewport_rows, mouse_scroll_lines)
        {
            action
        } else {
            AttachInputAction::Forward(output)
        }
    }
}

#[cfg(unix)]
fn attach_scroll_action(
    data: &[u8],
    viewport_rows: u16,
    mouse_scroll_lines: usize,
) -> Option<AttachInputAction> {
    let mut events = crate::raw_input::parse_raw_input_bytes_sync(data);
    if events.len() != 1 {
        return None;
    }

    match events.pop()? {
        crate::raw_input::RawInputEvent::Mouse(mouse) => {
            let direction = match mouse.kind {
                MouseEventKind::ScrollUp => AttachScrollDirection::Up,
                MouseEventKind::ScrollDown => AttachScrollDirection::Down,
                _ => return Some(AttachInputAction::None),
            };
            Some(AttachInputAction::Scroll {
                source: AttachScrollSource::Wheel,
                direction,
                lines: mouse_scroll_lines.max(1).min(u16::MAX as usize) as u16,
                column: Some(mouse.column),
                row: Some(mouse.row),
                modifiers: mouse.modifiers.bits(),
            })
        }
        crate::raw_input::RawInputEvent::Key(key)
            if key.modifiers.is_empty()
                && matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) =>
        {
            let direction = match key.code {
                KeyCode::PageUp => AttachScrollDirection::Up,
                KeyCode::PageDown => AttachScrollDirection::Down,
                _ => return None,
            };
            Some(AttachInputAction::Scroll {
                source: AttachScrollSource::PageKey {
                    input: data.to_vec(),
                },
                direction,
                lines: viewport_rows.saturating_sub(1).max(1),
                column: None,
                row: None,
                modifiers: KeyModifiers::empty().bits(),
            })
        }
        crate::raw_input::RawInputEvent::Key(key)
            if key.modifiers.is_empty()
                && key.kind == KeyEventKind::Release
                && matches!(key.code, KeyCode::PageUp | KeyCode::PageDown) =>
        {
            Some(AttachInputAction::None)
        }
        _ => None,
    }
}

#[cfg(unix)]
#[derive(Debug, Clone, PartialEq, Eq)]
enum ClientApiRefreshPolicy {
    Immediate,
    Deferred,
    // item 6 (Area 6): fire a targeted single-server refresh for the focused server only (not
    // the whole fleet) so a focus reconciles within one round-trip.
    ImmediateFocused,
}

#[cfg(unix)]
#[derive(Debug, Clone, PartialEq)]
enum ClientInputDispatch {
    Forward(Vec<u8>),
    ServerControl {
        server_id: supervisor::ServerId,
        message: ClientMessage,
    },
    ApiRequest {
        server_id: supervisor::ServerId,
        refresh: ClientApiRefreshPolicy,
        request: Box<crate::api::schema::Request>,
    },
    AddRemote(supervisor::AddRemoteDraft),
    // item 3 (Area 5): toggle / delete a remote off the UI loop against ServerId::main().
    SetRemoteEnabled {
        remote_id: String,
        enabled: bool,
    },
    DeleteRemote {
        remote_id: String,
    },
    Resize {
        cols: u16,
        rows: u16,
    },
    DetachAll,
    Redraw,
    Consumed,
}

/// item 3 (Area 5): map a `RemoteManageOutcome` from the model into the client input dispatch.
/// `OpenAddRemote` maps to `Redraw` because the model already switched overlay state.
#[cfg(unix)]
fn dispatch_for_remote_manage_outcome(
    outcome: supervisor::RemoteManageOutcome,
) -> ClientInputDispatch {
    match outcome {
        supervisor::RemoteManageOutcome::Redraw
        | supervisor::RemoteManageOutcome::OpenAddRemote => ClientInputDispatch::Redraw,
        supervisor::RemoteManageOutcome::SetEnabled { remote_id, enabled } => {
            ClientInputDispatch::SetRemoteEnabled { remote_id, enabled }
        }
        supervisor::RemoteManageOutcome::Delete { remote_id } => {
            ClientInputDispatch::DeleteRemote { remote_id }
        }
    }
}

#[cfg(unix)]
fn dispatch_composited_input(
    data: Vec<u8>,
    compositor: &mut compositor::ClientCompositor,
    model: &mut supervisor::ClientSupervisorModel,
    host_size: (u16, u16),
) -> ClientInputDispatch {
    if model.add_remote_form().is_some()
        || model.client_global_menu_highlighted().is_some()
        || model.new_workspace_picker().is_some()
        || model.remote_manage_overlay().is_some()
        || model.workspace_context_menu().is_some()
        || model.rename_workspace_form().is_some()
        || model.confirm_close_workspace().is_some()
    {
        return dispatch_client_overlay_input(data, compositor, model, host_size);
    }

    let events = crate::raw_input::parse_raw_input_bytes_sync(&data);
    if let [crate::raw_input::RawInputEvent::Mouse(mouse)] = events.as_slice() {
        return dispatch_composited_mouse_input(data, compositor, model, host_size, mouse);
    }

    // #24: client-side sidebar keyboard navigation. Before forwarding a key to the focused remote
    // terminal, check whether it matches a configured sidebar-nav binding and, if so, route it to
    // the SAME client action the mouse path uses. Only a single bare Key event is considered; any
    // multi-event/paste/unmatched input falls through to Forward so terminal input is preserved.
    if let [crate::raw_input::RawInputEvent::Key(key)] = events.as_slice() {
        if let Some(dispatch) =
            dispatch_composited_key_input(*key, &data, compositor, model, host_size)
        {
            return dispatch;
        }
    }

    // A dangling prefix (armed, but the next input is a paste/mouse/multi-event batch) belongs
    // to the server: replay the stashed prefix bytes ahead of the current input.
    if let Some(mut replay) = compositor.take_prefix_bytes() {
        replay.extend_from_slice(&data);
        return ClientInputDispatch::Forward(replay);
    }

    ClientInputDispatch::Forward(data)
}

/// #24: route a single keypress to a client-side sidebar-navigation action, mirroring the
/// server's gating (`src/app/input/navigate.rs`). Returns `None` for any key that is NOT a
/// configured sidebar-nav binding so the caller forwards it to the focused remote terminal.
///
/// Gating mirrors the server's two-stage `Mode::Prefix` state machine: the configured prefix key
/// (a modified chord, e.g. `ctrl+b`) arms client prefix mode; only WHILE armed is the next key
/// matched against the configured prefix-mode SIDEBAR bindings (next/prev workspace+agent,
/// new/rename/close workspace, sidebar collapse). A prefix chord that matches no sidebar binding
/// is NOT swallowed: the stashed prefix bytes plus the key are replayed to the active server, so
/// every server-side prefix binding (splits, tabs, zoom, copy mode, detach, …) keeps working.
/// Direct (modified-chord) bindings still fire without the prefix, exactly as the server's
/// `terminal_direct_navigation_action` does. Bare, unmodified keys are never intercepted unless
/// prefix-armed, so normal typing reaches the agent.
#[cfg(unix)]
fn dispatch_composited_key_input(
    key: crate::input::TerminalKey,
    raw_bytes: &[u8],
    compositor: &mut compositor::ClientCompositor,
    model: &mut supervisor::ClientSupervisorModel,
    host_size: (u16, u16),
) -> Option<ClientInputDispatch> {
    let (keybinds, prefix) = client_navigation_keybinds();

    if compositor.prefix_armed() {
        let key_event = key.as_key_event();
        // Esc or a repeated prefix press cancels prefix mode without reaching the server,
        // matching the server's own prefix-mode escape behavior.
        if key_event.code == crossterm::event::KeyCode::Esc
            || crate::config::terminal_key_matches_combo(key, prefix)
        {
            compositor.disarm_prefix();
            return Some(ClientInputDispatch::Redraw);
        }
        if let Some(dispatch) = sidebar_action_dispatch(
            &keybinds,
            key,
            compositor,
            model,
            ActionTrigger::Prefix,
            host_size,
        ) {
            compositor.disarm_prefix();
            return Some(dispatch);
        }
        // Not a sidebar chord: this prefix binding lives on the server. Replay the stashed
        // prefix bytes followed by the key so the server's prefix state machine resolves it.
        let mut replay = compositor.take_prefix_bytes().unwrap_or_default();
        replay.extend_from_slice(raw_bytes);
        return Some(ClientInputDispatch::Forward(replay));
    }

    // Not armed: a press of the configured prefix key arms prefix mode (and is held back until
    // the follow-up key decides whether the chord is client- or server-side). Other keys only
    // fire if they match a DIRECT (modified-chord) sidebar-nav binding; everything else returns
    // None so the caller forwards it to the terminal.
    if crate::config::terminal_key_matches_combo(key, prefix) {
        compositor.arm_prefix(raw_bytes.to_vec());
        return Some(ClientInputDispatch::Redraw);
    }

    sidebar_action_dispatch(
        &keybinds,
        key,
        compositor,
        model,
        ActionTrigger::Direct,
        host_size,
    )
}

/// #24: whether to match the configured prefix-mode side of a binding (`prefix+x`, only checked
/// while the prefix is armed) or the direct side (a modified chord like `alt+a`, checked always).
#[cfg(unix)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ActionTrigger {
    Direct,
    Prefix,
}

/// #24: resolve the client's effective keybindings (and the prefix combo). The client renders the
/// sidebar with the SAME shared code as the server, and its sidebar-nav keys come from the same
/// config the server reads; the client reuses `Config::keybinds()` / `Config::prefix_key()` (the
/// identical resolution the server uses) rather than inventing a parallel binding source. Computed
/// per keypress (keypresses are rare), so no caching is needed and a live config reload is
/// naturally picked up on the next key.
#[cfg(unix)]
fn client_navigation_keybinds() -> (crate::config::Keybinds, (KeyCode, KeyModifiers)) {
    let config = crate::config::Config::load().config;
    (config.keybinds(), config.prefix_key())
}

/// #24: match `key` against the sidebar-nav bindings for the given trigger side and, if it matches,
/// perform the SAME client action the mouse path performs. Returns `None` when no sidebar-nav
/// binding matches (so the key is forwarded / consumed by the caller). Bindings and actions are a
/// strict subset of the server's `navigate.rs` map — the workspace-tree / pane / tab actions that
/// have no client-rendered-sidebar equivalent are intentionally NOT bound here.
#[cfg(unix)]
fn sidebar_action_dispatch(
    keybinds: &crate::config::Keybinds,
    key: crate::input::TerminalKey,
    compositor: &mut compositor::ClientCompositor,
    model: &mut supervisor::ClientSupervisorModel,
    trigger: ActionTrigger,
    host_size: (u16, u16),
) -> Option<ClientInputDispatch> {
    let matches = |binding: &crate::config::ActionKeybinds| match trigger {
        ActionTrigger::Direct => binding.matches_direct_key(key),
        ActionTrigger::Prefix => binding.matches_prefix_key(key),
    };

    // next/prev workspace + agent: step the aggregated list and route the focus exactly like the
    // mouse workspace/agent-click path (FocusRoute -> ApiRequest, crossing servers as needed).
    if matches(&keybinds.next_workspace) {
        return Some(step_workspace_focus(model, 1));
    }
    if matches(&keybinds.previous_workspace) {
        return Some(step_workspace_focus(model, -1));
    }
    if matches(&keybinds.next_agent) {
        return Some(step_agent_focus(model, 1));
    }
    if matches(&keybinds.previous_agent) {
        return Some(step_agent_focus(model, -1));
    }

    // new workspace: open the same picker the New button / `SidebarHitTarget::New` path opens.
    if matches(&keybinds.new_workspace) || matches(&keybinds.workspace_picker) {
        return Some(open_new_workspace_picker_dispatch(model));
    }

    // rename / close the focused workspace: open the #23 overlays (the existing overlay key-gate
    // then handles typing/confirm), reusing the supervisor opener methods the context menu uses.
    if matches(&keybinds.rename_workspace) {
        return Some(open_focused_workspace_overlay(
            model,
            FocusedWorkspaceOverlay::Rename,
        ));
    }
    if matches(&keybinds.close_workspace) {
        return Some(open_focused_workspace_overlay(
            model,
            FocusedWorkspaceOverlay::Close,
        ));
    }

    // toggle sidebar collapse (#25): flip the client-local compositor flag, then resize —
    // collapsing reclaims the sidebar columns for the content, so every connected server
    // must re-render at the new content width (the SAME dispatch the mouse toggle and the
    // width-divider drag use).
    if matches(&keybinds.toggle_sidebar) {
        compositor.toggle_sidebar_collapsed();
        let (cols, rows) = compositor.content_size(host_size.0, host_size.1);
        return Some(ClientInputDispatch::Resize { cols, rows });
    }

    None
}

/// #24: enumerate the aggregated workspace rows (across servers, in render order), find the focused
/// one, step ±1 with wraparound, and route the focus through `focus_workspace_route` -> ApiRequest
/// exactly like the mouse workspace-click path (including switching the active server across a
/// host boundary, handled inside `focus_workspace_route`). Placeholder rows (`workspace_id == None`)
/// are skipped so a step always lands on a real, focusable workspace.
#[cfg(unix)]
fn step_workspace_focus(
    model: &mut supervisor::ClientSupervisorModel,
    delta: isize,
) -> ClientInputDispatch {
    let targets: Vec<(supervisor::ServerId, String)> = model
        .workspace_rows()
        .into_iter()
        .filter_map(|row| row.workspace_id.map(|id| (row.server_id, id)))
        .collect();
    let Some((server_id, workspace_id)) = step_focus_target(
        &targets,
        model
            .workspace_rows()
            .iter()
            .position(|row| row.focused && row.workspace_id.is_some()),
        delta,
    ) else {
        return ClientInputDispatch::Consumed;
    };

    let refresh = focus_refresh_policy(model.active_server_id(), &server_id);
    model
        .focus_workspace_route(&server_id, &workspace_id)
        .api_request("client:workspace-focus")
        .map(|request| ClientInputDispatch::ApiRequest {
            server_id,
            refresh,
            request: Box::new(request),
        })
        .unwrap_or(ClientInputDispatch::Consumed)
}

/// #24: agent sibling of `step_workspace_focus`. Flattens `agent_groups()` (across servers, in
/// render order) into `(server_id, agent_id)` rows, steps ±1 with wraparound from the focused
/// agent, and routes through `focus_agent_route` -> ApiRequest like the mouse agent-click path.
#[cfg(unix)]
fn step_agent_focus(
    model: &mut supervisor::ClientSupervisorModel,
    delta: isize,
) -> ClientInputDispatch {
    let mut targets: Vec<(supervisor::ServerId, String)> = Vec::new();
    let mut focused_index = None;
    for group in model.agent_groups() {
        for agent in group.agents {
            if agent.focused {
                focused_index = Some(targets.len());
            }
            targets.push((group.server_id.clone(), agent.agent_id));
        }
    }

    let Some((server_id, agent_id)) = step_focus_target(&targets, focused_index, delta) else {
        return ClientInputDispatch::Consumed;
    };

    let refresh = focus_refresh_policy(model.active_server_id(), &server_id);
    model
        .focus_agent_route(&server_id, &agent_id)
        .api_request("client:agent-focus")
        .map(|request| ClientInputDispatch::ApiRequest {
            server_id,
            refresh,
            request: Box::new(request),
        })
        .unwrap_or(ClientInputDispatch::Consumed)
}

/// #24: pick the `delta`-step neighbour (with wraparound) of `current` in `targets`. When nothing
/// is currently focused, a forward step lands on the first entry and a backward step on the last.
#[cfg(unix)]
fn step_focus_target<T: Clone>(targets: &[T], current: Option<usize>, delta: isize) -> Option<T> {
    if targets.is_empty() {
        return None;
    }
    let len = targets.len() as isize;
    let base = match current {
        Some(idx) => idx as isize,
        // No focus yet: stepping forward starts at 0, backward at the last entry.
        None => {
            if delta >= 0 {
                -1
            } else {
                0
            }
        }
    };
    let next = ((base + delta) % len + len) % len;
    targets.get(next as usize).cloned()
}

/// #24: open the new-workspace picker the SAME way the New button does, mapping a single-destination
/// route straight to a create request and a multi-destination route to the picker overlay (Redraw).
#[cfg(unix)]
fn open_new_workspace_picker_dispatch(
    model: &mut supervisor::ClientSupervisorModel,
) -> ClientInputDispatch {
    match model.open_new_workspace_picker() {
        route @ supervisor::NewWorkspaceRoute::CreateOn(_) => route
            .api_request("client:workspace-create")
            .map(|(server_id, request)| ClientInputDispatch::ApiRequest {
                server_id,
                refresh: ClientApiRefreshPolicy::Immediate,
                request: Box::new(request),
            })
            .unwrap_or(ClientInputDispatch::Consumed),
        supervisor::NewWorkspaceRoute::PickDestination(_) => ClientInputDispatch::Redraw,
        supervisor::NewWorkspaceRoute::Unavailable { .. } => ClientInputDispatch::Consumed,
    }
}

/// #24: which #23 overlay to open for the focused workspace.
#[cfg(unix)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FocusedWorkspaceOverlay {
    Rename,
    Close,
}

/// #24: open the rename / confirm-close overlay (#23) for the currently focused workspace. The
/// supervisor's `open_rename_workspace` / `open_confirm_close_workspace` openers read the workspace
/// context menu, so this first opens that menu for the focused row (the SAME state a right-click
/// produces) and then promotes it to the requested overlay. The existing overlay key-gate then
/// handles typing / confirmation.
#[cfg(unix)]
fn open_focused_workspace_overlay(
    model: &mut supervisor::ClientSupervisorModel,
    overlay: FocusedWorkspaceOverlay,
) -> ClientInputDispatch {
    let Some((server_id, workspace_id)) = model
        .workspace_rows()
        .into_iter()
        .find(|row| row.focused && row.workspace_id.is_some())
        .and_then(|row| row.workspace_id.map(|id| (row.server_id, id)))
    else {
        return ClientInputDispatch::Consumed;
    };
    let label = model
        .workspace_label(&server_id, &workspace_id)
        .unwrap_or_else(|| workspace_id.clone());
    model.open_workspace_context_menu(server_id, workspace_id, label);
    match overlay {
        FocusedWorkspaceOverlay::Rename => model.open_rename_workspace(),
        FocusedWorkspaceOverlay::Close => model.open_confirm_close_workspace(),
    }
    ClientInputDispatch::Redraw
}

#[cfg(unix)]
fn dispatch_client_overlay_input(
    data: Vec<u8>,
    compositor: &mut compositor::ClientCompositor,
    model: &mut supervisor::ClientSupervisorModel,
    host_size: (u16, u16),
) -> ClientInputDispatch {
    let events = crate::raw_input::parse_raw_input_bytes_sync(&data);
    if events.is_empty() {
        return ClientInputDispatch::Consumed;
    }

    let mut dispatch = ClientInputDispatch::Consumed;
    for event in events {
        let next = match event {
            crate::raw_input::RawInputEvent::Key(key) if model.add_remote_form().is_some() => {
                match model.handle_add_remote_key(key) {
                    supervisor::AddRemoteFormOutcome::Redraw => ClientInputDispatch::Redraw,
                    supervisor::AddRemoteFormOutcome::Submit(draft) => {
                        ClientInputDispatch::AddRemote(draft)
                    }
                }
            }
            crate::raw_input::RawInputEvent::Paste(text) if model.add_remote_form().is_some() => {
                match model.append_add_remote_paste(&text) {
                    supervisor::AddRemoteFormOutcome::Redraw => ClientInputDispatch::Redraw,
                    supervisor::AddRemoteFormOutcome::Submit(draft) => {
                        ClientInputDispatch::AddRemote(draft)
                    }
                }
            }
            crate::raw_input::RawInputEvent::Key(key)
                if model.client_global_menu_highlighted().is_some() =>
            {
                dispatch_client_global_menu_key(model, key)
            }
            crate::raw_input::RawInputEvent::Key(key) if model.new_workspace_picker().is_some() => {
                dispatch_new_workspace_picker_key(model, key)
            }
            crate::raw_input::RawInputEvent::Key(key)
                if model.remote_manage_overlay().is_some() =>
            {
                dispatch_for_remote_manage_outcome(model.handle_remote_manage_key(key))
            }
            // #23: workspace context menu / rename / confirm-close key + paste handling. Each
            // delegates to the supervisor key handler and maps its typed outcome into a dispatch
            // (mirrors the add-remote / remote-manage arms above).
            crate::raw_input::RawInputEvent::Key(key)
                if model.workspace_context_menu().is_some() =>
            {
                match model.handle_workspace_context_menu_key(key) {
                    supervisor::WorkspaceContextOutcome::Redraw
                    | supervisor::WorkspaceContextOutcome::OpenRename
                    | supervisor::WorkspaceContextOutcome::OpenConfirmClose => {
                        ClientInputDispatch::Redraw
                    }
                }
            }
            crate::raw_input::RawInputEvent::Key(key)
                if model.rename_workspace_form().is_some() =>
            {
                dispatch_for_rename_workspace_outcome(model.handle_rename_workspace_key(key))
            }
            crate::raw_input::RawInputEvent::Paste(text)
                if model.rename_workspace_form().is_some() =>
            {
                dispatch_for_rename_workspace_outcome(model.append_rename_workspace_paste(&text))
            }
            crate::raw_input::RawInputEvent::Key(key)
                if model.confirm_close_workspace().is_some() =>
            {
                let outcome = model.handle_confirm_close_workspace_key(key);
                dispatch_for_confirm_close_outcome(model, outcome)
            }
            crate::raw_input::RawInputEvent::Mouse(mouse) => {
                dispatch_composited_mouse_input(data.clone(), compositor, model, host_size, &mouse)
            }
            _ => ClientInputDispatch::Consumed,
        };

        if matches!(
            next,
            ClientInputDispatch::AddRemote(_)
                | ClientInputDispatch::SetRemoteEnabled { .. }
                | ClientInputDispatch::DeleteRemote { .. }
                | ClientInputDispatch::ApiRequest { .. }
                | ClientInputDispatch::ServerControl { .. }
                | ClientInputDispatch::Resize { .. }
                | ClientInputDispatch::DetachAll
        ) {
            dispatch = next;
            break;
        }
        if matches!(next, ClientInputDispatch::Redraw) {
            dispatch = ClientInputDispatch::Redraw;
        }
    }
    dispatch
}

#[cfg(unix)]
fn dispatch_client_global_menu_key(
    model: &mut supervisor::ClientSupervisorModel,
    key: crate::input::TerminalKey,
) -> ClientInputDispatch {
    if !matches!(
        key.kind,
        crossterm::event::KeyEventKind::Press | crossterm::event::KeyEventKind::Repeat
    ) {
        return ClientInputDispatch::Consumed;
    }

    match key.code {
        KeyCode::Esc => {
            model.close_client_overlay();
            ClientInputDispatch::Redraw
        }
        KeyCode::Up | KeyCode::Char('k') => {
            model.move_client_global_menu_prev();
            ClientInputDispatch::Redraw
        }
        KeyCode::Down | KeyCode::Char('j') => {
            model.move_client_global_menu_next();
            ClientInputDispatch::Redraw
        }
        KeyCode::Enter => {
            let action = model.accept_client_global_menu_item();
            dispatch_client_global_menu_action(model, action)
        }
        _ => ClientInputDispatch::Consumed,
    }
}

/// item 1: keyboard navigation for the composited new-workspace destination picker. ↑/k and ↓/j
/// move the highlight, Enter confirms the highlighted destination, Esc closes the picker.
#[cfg(unix)]
fn dispatch_new_workspace_picker_key(
    model: &mut supervisor::ClientSupervisorModel,
    key: crate::input::TerminalKey,
) -> ClientInputDispatch {
    if !matches!(
        key.kind,
        crossterm::event::KeyEventKind::Press | crossterm::event::KeyEventKind::Repeat
    ) {
        return ClientInputDispatch::Consumed;
    }

    match key.code {
        KeyCode::Esc => {
            model.close_new_workspace_picker();
            ClientInputDispatch::Redraw
        }
        KeyCode::Up | KeyCode::Char('k') => {
            model.move_new_workspace_picker_prev();
            ClientInputDispatch::Redraw
        }
        KeyCode::Down | KeyCode::Char('j') => {
            model.move_new_workspace_picker_next();
            ClientInputDispatch::Redraw
        }
        KeyCode::Enter => accept_new_workspace_picker_dispatch(model),
        _ => ClientInputDispatch::Consumed,
    }
}

/// item 1: resolve the highlighted picker destination into a create-workspace API request, reusing
/// the same `NewWorkspaceRoute::api_request` mapping the mouse destination-row path uses. Shared by
/// the picker Enter key and the confirm button.
#[cfg(unix)]
fn accept_new_workspace_picker_dispatch(
    model: &mut supervisor::ClientSupervisorModel,
) -> ClientInputDispatch {
    model
        .accept_new_workspace_picker()
        .api_request("client:workspace-create")
        .map(|(server_id, request)| ClientInputDispatch::ApiRequest {
            server_id,
            refresh: ClientApiRefreshPolicy::Immediate,
            request: Box::new(request),
        })
        .unwrap_or(ClientInputDispatch::Consumed)
}

#[cfg(unix)]
fn dispatch_client_global_menu_action(
    model: &mut supervisor::ClientSupervisorModel,
    action: Option<supervisor::ClientGlobalMenuAction>,
) -> ClientInputDispatch {
    match action {
        Some(supervisor::ClientGlobalMenuAction::Settings) => {
            model.activate_main_server();
            ClientInputDispatch::ServerControl {
                server_id: supervisor::ServerId::main(),
                message: ClientMessage::OpenSettings,
            }
        }
        Some(supervisor::ClientGlobalMenuAction::Keybinds) => {
            model.activate_main_server();
            ClientInputDispatch::ServerControl {
                server_id: supervisor::ServerId::main(),
                message: ClientMessage::OpenKeybindHelp,
            }
        }
        Some(supervisor::ClientGlobalMenuAction::ReloadConfig) => ClientInputDispatch::ApiRequest {
            server_id: supervisor::ServerId::main(),
            refresh: ClientApiRefreshPolicy::Immediate,
            request: Box::new(crate::api::schema::Request {
                id: "client:reload-config".into(),
                method: crate::api::schema::Method::ServerReloadConfig(
                    crate::api::schema::EmptyParams::default(),
                ),
            }),
        },
        Some(supervisor::ClientGlobalMenuAction::Detach) => ClientInputDispatch::DetachAll,
        Some(supervisor::ClientGlobalMenuAction::AddRemote) => ClientInputDispatch::Redraw,
        // item 3 (Area 5): the overlay was already opened by `select_client_global_menu_item`;
        // just repaint.
        Some(supervisor::ClientGlobalMenuAction::ManageRemotes) => ClientInputDispatch::Redraw,
        None => ClientInputDispatch::Consumed,
    }
}

#[cfg(unix)]
fn dispatch_composited_mouse_input(
    data: Vec<u8>,
    compositor: &mut compositor::ClientCompositor,
    model: &mut supervisor::ClientSupervisorModel,
    host_size: (u16, u16),
    mouse: &MouseEvent,
) -> ClientInputDispatch {
    // item 7 (Area 4): handle motion BEFORE resize/scroll/hit_test. The `hit_test` dispatch below
    // early-returns `Consumed` for any non-`Down(Left)` kind, so without this top-of-fn arm a
    // `Moved` over a sidebar row would never reach `hover_test`. Intercept only when over the
    // sidebar OR a hover is currently set (so leaving the sidebar clears it); otherwise fall
    // through so a content `Moved` still forwards its bytes via `translate_content_mouse_input`.
    // The `Redraw` arm recomposes locally (no supervisor request, no server I/O).
    if matches!(mouse.kind, MouseEventKind::Moved) {
        // item 7: while the global menu is open, motion moves its highlight to the hovered row
        // (mirrors the monolithic host's `global_menu.hover`); the same shared launcher-menu surface
        // then renders it. The overlay mouse arm routes the `Moved` here regardless of column.
        if model.client_global_menu_highlighted().is_some() {
            let hovered = compositor.client_global_menu_item_at(
                model,
                mouse.column,
                mouse.row,
                host_size.0,
                host_size.1,
            );
            return if model.hover_client_global_menu_item(hovered) {
                ClientInputDispatch::Redraw
            } else {
                ClientInputDispatch::Consumed
            };
        }
        let sidebar_width = compositor.sidebar_width().min(host_size.0);
        if mouse.column < sidebar_width || compositor.hover().is_some() {
            let next =
                compositor.hover_test(model, mouse.column, mouse.row, host_size.0, host_size.1);
            return if compositor.set_hover(next) {
                ClientInputDispatch::Redraw
            } else {
                ClientInputDispatch::Consumed
            };
        }
    }

    // #23: a right-click (Down, MouseButton::Right) over a workspace card opens the client-rendered
    // context menu anchored at that row, capturing the workspace's current label for the rename
    // prefill / close-confirm text. Resolved through the SAME `hit_test` the left-click path uses.
    if matches!(mouse.kind, MouseEventKind::Down(MouseButton::Right)) {
        if let Some(compositor::SidebarHitTarget::Workspace {
            server_id,
            workspace_id,
        }) = compositor.hit_test(model, mouse.column, mouse.row, host_size.0, host_size.1)
        {
            let label = model
                .workspace_label(&server_id, &workspace_id)
                .unwrap_or_else(|| workspace_id.clone());
            model.open_workspace_context_menu(server_id, workspace_id, label);
            return ClientInputDispatch::Redraw;
        }
    }

    if let Some(outcome) =
        compositor.handle_sidebar_resize_mouse(mouse, host_size.0, host_size.1, model.ui_settings())
    {
        // #26: the outcome decides resize vs redraw — a drag or a double-click reset changes the
        // content width (resize the remote PTY); beginning/ending a drag only redraws.
        return match outcome {
            compositor::SidebarResizeOutcome::Resized(cols, rows) => {
                ClientInputDispatch::Resize { cols, rows }
            }
            compositor::SidebarResizeOutcome::Redraw => ClientInputDispatch::Redraw,
        };
    }

    // #16: the spaces↔agents section divider re-splits the sidebar locally (no content resize), so
    // it is checked after the width divider (which wins at the shared corner column) and always
    // redraws rather than resizing the remote PTY.
    if compositor
        .handle_sidebar_section_divider_mouse(model, mouse, host_size.0, host_size.1)
        .is_some()
    {
        return ClientInputDispatch::Redraw;
    }

    // #21: scrollbar track-click + thumb-drag (client-local). Runs before the wheel handler and
    // hit_test so a press on the scrollbar column scrolls instead of focusing the card beneath it.
    if let Some(changed) =
        compositor.handle_sidebar_scrollbar_mouse(model, mouse, host_size.0, host_size.1)
    {
        return if changed {
            ClientInputDispatch::Redraw
        } else {
            ClientInputDispatch::Consumed
        };
    }

    if let Some(changed) =
        compositor.handle_sidebar_scroll_mouse(model, mouse, host_size.0, host_size.1)
    {
        return if changed {
            ClientInputDispatch::Redraw
        } else {
            ClientInputDispatch::Consumed
        };
    }

    // #19: workspace drag-to-reorder. `Drag`/`Up` over the sidebar are otherwise swallowed by the
    // sidebar-width guard below, so the tracker runs before `hit_test` (which only acts on
    // `Down(Left)` anyway). The `Down` that arms the press is recorded in the Workspace hit arm
    // below, where the click still focuses-on-down.
    match compositor.handle_workspace_reorder_mouse(model, mouse, host_size.0, host_size.1) {
        compositor::WorkspaceReorderOutcome::Dragging => return ClientInputDispatch::Redraw,
        compositor::WorkspaceReorderOutcome::Commit {
            server_id,
            workspace_id,
            insert_index,
        } => {
            return ClientInputDispatch::ApiRequest {
                server_id,
                // Reorder changes persisted server state; refresh the fleet so the new order shows.
                refresh: ClientApiRefreshPolicy::Immediate,
                request: Box::new(crate::api::schema::Request {
                    id: "client:workspace-move".into(),
                    // Upstream's `workspace.move` is the PoC's `workspace.reorder` — same
                    // `workspace_id` + 0..=len `insert_index` contract.
                    method: crate::api::schema::Method::WorkspaceMove(
                        crate::api::schema::WorkspaceMoveParams {
                            workspace_id,
                            insert_index,
                        },
                    ),
                }),
            };
        }
        compositor::WorkspaceReorderOutcome::Cancelled => return ClientInputDispatch::Consumed,
        compositor::WorkspaceReorderOutcome::Ignored => {}
    }

    // #19 (host half): host drag-to-reorder. Runs alongside the workspace tracker, before
    // hit_test, for the same reason (Drag/Up over the sidebar are otherwise swallowed). The
    // commit is CLIENT-LOCAL: host order is client-owned, so it mutates the model and redraws
    // with no server round-trip (contrast with workspace.reorder above).
    match compositor.handle_host_reorder_mouse(model, mouse, host_size.0, host_size.1) {
        compositor::HostReorderOutcome::Dragging => return ClientInputDispatch::Redraw,
        compositor::HostReorderOutcome::Commit {
            source_server_id,
            insert_index,
        } => {
            model.reorder_server(&source_server_id, insert_index);
            return ClientInputDispatch::Redraw;
        }
        compositor::HostReorderOutcome::Cancelled => return ClientInputDispatch::Consumed,
        compositor::HostReorderOutcome::Ignored => {}
    }

    if let Some(target) =
        compositor.hit_test(model, mouse.column, mouse.row, host_size.0, host_size.1)
    {
        // #20: the sort toggle mutates client-local compositor state (no server round-trip), so it
        // is handled here where `&mut compositor` is in scope rather than in the model-only
        // `dispatch_sidebar_hit_target`.
        if matches!(target, compositor::SidebarHitTarget::AgentPanelSortToggle) {
            return if matches!(mouse.kind, MouseEventKind::Down(MouseButton::Left)) {
                compositor.toggle_agent_panel_sort();
                ClientInputDispatch::Redraw
            } else {
                ClientInputDispatch::Consumed
            };
        }
        // #25: the collapse/expand toggle mutates client-local compositor state, handled here
        // where `&mut compositor` is in scope (like the sort toggle). Collapsing reclaims the
        // sidebar columns for the content, so the flip dispatches a Resize — every connected
        // server re-renders at the new content width (same path as the width-divider drag).
        if matches!(target, compositor::SidebarHitTarget::CollapsedSidebarToggle) {
            return if matches!(mouse.kind, MouseEventKind::Down(MouseButton::Left)) {
                compositor.toggle_sidebar_collapsed();
                let (cols, rows) = compositor.content_size(host_size.0, host_size.1);
                ClientInputDispatch::Resize { cols, rows }
            } else {
                ClientInputDispatch::Consumed
            };
        }
        // #22: a chevron click toggles the worktree group's collapsed state in the client-local set
        // (no server round-trip — the aggregated view's collapse is a per-client display concern).
        // Handled here where `&mut compositor` is in scope (like the sort / collapse toggles).
        if let compositor::SidebarHitTarget::WorktreeChevron { group_key } = &target {
            return if matches!(mouse.kind, MouseEventKind::Down(MouseButton::Left)) {
                compositor.toggle_collapsed_space_key(group_key.clone());
                ClientInputDispatch::Redraw
            } else {
                ClientInputDispatch::Consumed
            };
        }
        // #19: arm a drag-reorder on a workspace down-press. The click still focuses-on-down via
        // `dispatch_sidebar_hit_target` below; a subsequent drag promotes the press to a reorder.
        if let compositor::SidebarHitTarget::Workspace {
            server_id,
            workspace_id,
        } = &target
        {
            if matches!(mouse.kind, MouseEventKind::Down(MouseButton::Left)) {
                compositor.begin_workspace_press(
                    server_id.clone(),
                    workspace_id.clone(),
                    mouse.column,
                    mouse.row,
                );
            }
        }
        // #19 (host half): arm a host drag-reorder on a host-banner down-press. A subsequent drag
        // promotes it to a reorder (committed client-locally); a plain click just consumes (the
        // banner is the host's drag handle). Mirrors the workspace press above.
        if let compositor::SidebarHitTarget::HostBanner { server_id } = &target {
            if matches!(mouse.kind, MouseEventKind::Down(MouseButton::Left)) {
                compositor.begin_host_press(server_id.clone(), mouse.column, mouse.row);
            }
        }
        return dispatch_sidebar_hit_target(target, model, mouse);
    }

    let sidebar_width = compositor.sidebar_width().min(host_size.0);
    if mouse.column < sidebar_width {
        return ClientInputDispatch::Consumed;
    }

    translate_content_mouse_input(data, mouse, sidebar_width)
}

/// item 6 (Area 6): the refresh policy for a focus dispatch. A focus that switches the active
/// server returns `ImmediateFocused` (fire a targeted single-server fetch so the new server
/// reconciles within one round-trip). A focus that stays on the already-active server returns
/// `Deferred` — the active remote's 400ms fast poll already covers it, so an extra immediate
/// fetch would be redundant SSH load. `current_active` is read BEFORE the focus route mutates it.
#[cfg(unix)]
fn focus_refresh_policy(
    current_active: &supervisor::ServerId,
    target_server: &supervisor::ServerId,
) -> ClientApiRefreshPolicy {
    if current_active == target_server {
        ClientApiRefreshPolicy::Deferred
    } else {
        ClientApiRefreshPolicy::ImmediateFocused
    }
}

#[cfg(unix)]
fn dispatch_sidebar_hit_target(
    target: compositor::SidebarHitTarget,
    model: &mut supervisor::ClientSupervisorModel,
    mouse: &MouseEvent,
) -> ClientInputDispatch {
    if !matches!(mouse.kind, MouseEventKind::Down(MouseButton::Left)) {
        return ClientInputDispatch::Consumed;
    }

    match target {
        compositor::SidebarHitTarget::Filter => {
            model.cycle_filter();
            ClientInputDispatch::Redraw
        }
        compositor::SidebarHitTarget::Workspace {
            server_id,
            workspace_id,
        } => {
            // item 6 (Area 6): focusing a row that SWITCHES the active server fires a targeted
            // single-server fetch (`ImmediateFocused`) so the new server reconciles within one
            // round-trip. Re-focusing within the already-active server needs no extra immediate
            // fetch — the active remote's 400ms fast poll already covers it — so it carries the
            // `Deferred` (no-refresh) policy.
            let refresh = focus_refresh_policy(model.active_server_id(), &server_id);
            model
                .focus_workspace_route(&server_id, &workspace_id)
                .api_request("client:workspace-focus")
                .map(|request| ClientInputDispatch::ApiRequest {
                    server_id,
                    refresh,
                    request: Box::new(request),
                })
                .unwrap_or(ClientInputDispatch::Consumed)
        }
        compositor::SidebarHitTarget::Agent {
            server_id,
            agent_id,
        } => {
            let refresh = focus_refresh_policy(model.active_server_id(), &server_id);
            model
                .focus_agent_route(&server_id, &agent_id)
                .api_request("client:agent-focus")
                .map(|request| ClientInputDispatch::ApiRequest {
                    server_id,
                    refresh,
                    request: Box::new(request),
                })
                .unwrap_or(ClientInputDispatch::Consumed)
        }
        compositor::SidebarHitTarget::New => open_new_workspace_picker_dispatch(model),
        compositor::SidebarHitTarget::NewWorkspaceDestination { server_id } => model
            .choose_new_workspace_destination(&server_id)
            .api_request("client:workspace-create")
            .map(|(server_id, request)| ClientInputDispatch::ApiRequest {
                server_id,
                refresh: ClientApiRefreshPolicy::Immediate,
                request: Box::new(request),
            })
            .unwrap_or(ClientInputDispatch::Consumed),
        compositor::SidebarHitTarget::ClientGlobalMenuItem { index } => {
            let action = model.select_client_global_menu_item(index);
            dispatch_client_global_menu_action(model, action)
        }
        compositor::SidebarHitTarget::Menu => {
            model.open_client_global_menu();
            ClientInputDispatch::Redraw
        }
        // #20: handled earlier in `dispatch_composited_mouse_input` (needs `&mut compositor`); this
        // arm only keeps the match exhaustive and is not reached in practice.
        compositor::SidebarHitTarget::AgentPanelSortToggle => ClientInputDispatch::Consumed,
        // #25: handled earlier in `dispatch_composited_mouse_input` (needs `&mut compositor`); this
        // arm only keeps the match exhaustive and is not reached in practice.
        compositor::SidebarHitTarget::CollapsedSidebarToggle => ClientInputDispatch::Consumed,
        // #22: handled earlier in `dispatch_composited_mouse_input` (needs `&mut compositor`); this
        // arm only keeps the match exhaustive and is not reached in practice.
        compositor::SidebarHitTarget::WorktreeChevron { .. } => ClientInputDispatch::Consumed,
        // #19 (host half): a host-banner press arms a host drag-reorder in
        // `dispatch_composited_mouse_input` (needs `&mut compositor`); the click itself only arms
        // (the banner is the host's drag handle). This arm only keeps the match exhaustive.
        compositor::SidebarHitTarget::HostBanner { .. } => ClientInputDispatch::Consumed,
        // item 1: composited-modal action buttons.
        compositor::SidebarHitTarget::AddRemoteSubmit => {
            // re-run the SAME empty-target validation as the Enter key by replaying an Enter
            // through `handle_add_remote_key`; an empty target yields the inline error (Redraw),
            // a valid target yields the submit draft.
            match model.handle_add_remote_key(enter_key()) {
                supervisor::AddRemoteFormOutcome::Redraw => ClientInputDispatch::Redraw,
                supervisor::AddRemoteFormOutcome::Submit(draft) => {
                    ClientInputDispatch::AddRemote(draft)
                }
            }
        }
        compositor::SidebarHitTarget::AddRemoteCancel => {
            model.close_client_overlay();
            ClientInputDispatch::Redraw
        }
        compositor::SidebarHitTarget::NewWorkspacePickerConfirm => {
            accept_new_workspace_picker_dispatch(model)
        }
        compositor::SidebarHitTarget::NewWorkspacePickerCancel => {
            model.close_new_workspace_picker();
            ClientInputDispatch::Redraw
        }
        // item 3 (Area 5): manage-overlay mouse targets. A row click selects it (toggle/delete are
        // keyboard-driven); `add` jumps to the add-remote form; the confirm popup buttons confirm
        // or cancel the two-step delete.
        compositor::SidebarHitTarget::RemoteManageRow { index } => {
            model.set_remote_manage_selected(index);
            ClientInputDispatch::Redraw
        }
        compositor::SidebarHitTarget::RemoteManageAdd => {
            model.open_add_remote_form();
            ClientInputDispatch::Redraw
        }
        compositor::SidebarHitTarget::RemoteManageConfirmDelete => {
            dispatch_for_remote_manage_outcome(model.confirm_remote_manage_delete())
        }
        compositor::SidebarHitTarget::RemoteManageCancelDelete => {
            model.cancel_remote_manage_delete();
            ClientInputDispatch::Redraw
        }
        // #23: workspace context-menu mouse targets. Clicking a menu row selects AND activates it
        // (opening the rename / confirm-close follow-on overlay); the rename submit/cancel and
        // confirm close/cancel buttons replay the SAME paths the keys use.
        compositor::SidebarHitTarget::WorkspaceContextMenuRow { index } => {
            model.set_workspace_context_menu_selected(index);
            match model.select_workspace_context_menu_item(index) {
                supervisor::WorkspaceContextOutcome::Redraw
                | supervisor::WorkspaceContextOutcome::OpenRename
                | supervisor::WorkspaceContextOutcome::OpenConfirmClose => {
                    ClientInputDispatch::Redraw
                }
            }
        }
        compositor::SidebarHitTarget::RenameWorkspaceSubmit => {
            // replay an Enter through the rename key handler so the BUTTON re-runs the exact same
            // empty-label validation / submit path as the Enter KEY (mirrors AddRemoteSubmit).
            dispatch_for_rename_workspace_outcome(model.handle_rename_workspace_key(enter_key()))
        }
        compositor::SidebarHitTarget::RenameWorkspaceCancel => {
            model.close_client_overlay();
            ClientInputDispatch::Redraw
        }
        compositor::SidebarHitTarget::ConfirmCloseWorkspaceConfirm => {
            let outcome = model.accept_confirm_close_workspace();
            dispatch_for_confirm_close_outcome(model, outcome)
        }
        compositor::SidebarHitTarget::ConfirmCloseWorkspaceCancel => {
            model.close_client_overlay();
            ClientInputDispatch::Redraw
        }
    }
}

/// item 1: a synthetic Enter key-press, used so the add-remote submit BUTTON re-runs the exact
/// same validation/submit path as the Enter KEY in `handle_add_remote_key`.
#[cfg(unix)]
fn enter_key() -> crate::input::TerminalKey {
    crate::input::TerminalKey::new(KeyCode::Enter, KeyModifiers::empty())
}

/// #23: map a `RenameWorkspaceOutcome` into a dispatch. `Submit` becomes a `workspace.rename`
/// round-trip to the OWNING server with an `Immediate` refresh so the renamed row reconciles on
/// the next summary; `Redraw` just repaints. Mirrors `dispatch_for_remote_manage_outcome`.
#[cfg(unix)]
fn dispatch_for_rename_workspace_outcome(
    outcome: supervisor::RenameWorkspaceOutcome,
) -> ClientInputDispatch {
    match outcome {
        supervisor::RenameWorkspaceOutcome::Redraw => ClientInputDispatch::Redraw,
        supervisor::RenameWorkspaceOutcome::Submit {
            server_id,
            workspace_id,
            label,
        } => ClientInputDispatch::ApiRequest {
            server_id,
            refresh: ClientApiRefreshPolicy::Immediate,
            request: Box::new(crate::api::schema::Request {
                id: "client:workspace-rename".into(),
                method: crate::api::schema::Method::WorkspaceRename(
                    crate::api::schema::WorkspaceRenameParams {
                        workspace_id,
                        label,
                    },
                ),
            }),
        },
    }
}

/// #23: map a `ConfirmCloseOutcome` into a dispatch. `Confirm` becomes a `workspace.close`
/// round-trip to the OWNING server with an `Immediate` refresh so the closed row disappears on the
/// next summary; `Redraw` just repaints.
#[cfg(unix)]
fn dispatch_for_confirm_close_outcome(
    model: &mut supervisor::ClientSupervisorModel,
    outcome: supervisor::ConfirmCloseOutcome,
) -> ClientInputDispatch {
    match outcome {
        supervisor::ConfirmCloseOutcome::Redraw => ClientInputDispatch::Redraw,
        supervisor::ConfirmCloseOutcome::Confirm {
            server_id,
            workspace_id,
        } => {
            // Optimistic removal: drop the row with the confirmation instead of
            // after the close + refresh round-trips; a failed close is restored
            // by the follow-up summary refresh.
            model.apply_closed_workspace(&server_id, &workspace_id);
            ClientInputDispatch::ApiRequest {
                server_id,
                refresh: ClientApiRefreshPolicy::Immediate,
                request: Box::new(crate::api::schema::Request {
                    id: "client:workspace-close".into(),
                    method: crate::api::schema::Method::WorkspaceClose(
                        crate::api::schema::WorkspaceTarget { workspace_id },
                    ),
                }),
            }
        }
    }
}

#[cfg(unix)]
fn translate_content_mouse_input(
    original: Vec<u8>,
    mouse: &MouseEvent,
    sidebar_width: u16,
) -> ClientInputDispatch {
    let Some(column) = mouse.column.checked_sub(sidebar_width) else {
        return ClientInputDispatch::Consumed;
    };

    let encoded = match mouse.kind {
        MouseEventKind::ScrollUp
        | MouseEventKind::ScrollDown
        | MouseEventKind::ScrollLeft
        | MouseEventKind::ScrollRight => crate::input::encode_mouse_scroll(
            mouse.kind,
            column,
            mouse.row,
            mouse.modifiers,
            crate::input::MouseProtocolEncoding::Sgr,
        ),
        MouseEventKind::Down(_) | MouseEventKind::Up(_) | MouseEventKind::Drag(_) => {
            crate::input::encode_mouse_button(
                mouse.kind,
                column,
                mouse.row,
                mouse.modifiers,
                crate::input::MouseProtocolEncoding::Sgr,
            )
        }
        MouseEventKind::Moved => None,
    };

    ClientInputDispatch::Forward(encoded.unwrap_or(original))
}

impl ClientState {
    fn request_full_redraw(&mut self) {
        self.blit_encoder = render_ansi::BlitEncoder::new();
    }
}

#[cfg(unix)]
fn client_render_plan(
    supervisor_model: Option<&supervisor::ClientSupervisorModel>,
    requested_encoding: RenderEncoding,
    host_size: (u16, u16),
) -> ClientRenderPlan {
    let use_client_compositor = supervisor_model.is_some();
    if use_client_compositor {
        let compositor = compositor::ClientCompositor::default();
        return ClientRenderPlan {
            surface_mode: ClientSurfaceMode::EmbeddedContent,
            requested_encoding: RenderEncoding::SemanticFrame,
            server_size: compositor.content_size(host_size.0, host_size.1),
            use_client_compositor,
        };
    }

    ClientRenderPlan {
        surface_mode: ClientSurfaceMode::FullApp,
        requested_encoding,
        server_size: host_size,
        use_client_compositor: false,
    }
}

// ---------------------------------------------------------------------------
// Error types
// ---------------------------------------------------------------------------

/// Errors that can occur during client operation.
#[derive(Debug)]
pub enum ClientError {
    /// Could not connect to the server's client socket.
    ConnectionFailed(io::Error),
    /// Server rejected our handshake.
    HandshakeRejected { version: u32, error: String },
    /// Server shut down.
    ServerShutdown { reason: Option<String> },
    /// Lost connection to the server.
    ConnectionLost(io::Error),
    /// Protocol error (framing, deserialization).
    Protocol(protocol::FramingError),
}

impl std::fmt::Display for ClientError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ClientError::ConnectionFailed(err) => {
                write!(f, "failed to connect to server: {err}")?;
                let path = client_socket_path();
                write!(
                    f,
                    "\nIs herdr server running? Start it with `herdr server`."
                )?;
                write!(f, "\nSocket path: {}", path.display())
            }
            ClientError::HandshakeRejected { version, error } => {
                write!(f, "server rejected handshake (version {version}): {error}")
            }
            ClientError::ServerShutdown { reason } => {
                match reason.as_deref() {
                    Some("detached") => {
                        if let Ok(reattach_command) =
                            std::env::var(crate::remote::REATTACH_COMMAND_ENV_VAR)
                        {
                            write!(f, "detached from remote server")?;
                            write!(f, "\nRun `{reattach_command}` to reattach")?;
                        } else {
                            write!(f, "detached from server")?;
                            write!(
                                f,
                                "\nRun `{}` to reattach",
                                crate::session::local_attach_command()
                            )?;
                        }
                    }
                    _ => {
                        write!(f, "server shut down")?;
                        if let Some(reason) = reason {
                            write!(f, ": {reason}")?;
                        }
                    }
                }
                Ok(())
            }
            ClientError::ConnectionLost(err) => {
                if let Ok(reattach_command) = std::env::var(crate::remote::REATTACH_COMMAND_ENV_VAR)
                {
                    write!(f, "lost connection to remote Herdr: {err}")?;
                    write!(f, "\nIf the remote server survived the SSH or network drop, its panes may still be running.")?;
                    write!(f, "\nRun `{reattach_command}` to reattach")
                } else {
                    write!(f, "lost connection to server: {err}")
                }
            }
            ClientError::Protocol(err) => {
                write!(f, "protocol error: {err}")
            }
        }
    }
}

impl std::error::Error for ClientError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            ClientError::ConnectionFailed(err) => Some(err),
            ClientError::ConnectionLost(err) => Some(err),
            ClientError::Protocol(err) => Some(err),
            _ => None,
        }
    }
}

impl From<protocol::FramingError> for ClientError {
    fn from(err: protocol::FramingError) -> Self {
        match err {
            protocol::FramingError::UnexpectedEof => ClientError::ConnectionLost(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "server closed connection",
            )),
            protocol::FramingError::Io(err) => ClientError::ConnectionLost(err),
            err => ClientError::Protocol(err),
        }
    }
}

// ---------------------------------------------------------------------------
// Terminal setup / restore
// ---------------------------------------------------------------------------

/// Sets up the terminal for client mode (raw mode, optional mouse, keyboard enhancements).
///
/// Returns a guard that restores the terminal when dropped.
fn setup_terminal(mouse_capture: bool) -> io::Result<TerminalGuard> {
    setup_terminal_with_capabilities(true, mouse_capture)
}

/// Sets up a direct attach terminal.
///
/// Direct attach forwards stdin to the attached PTY. It enables mouse capture
/// so wheel events can drive the attached viewport or be forwarded to child
/// programs that requested mouse input.
fn setup_direct_attach_terminal() -> io::Result<TerminalGuard> {
    setup_terminal_with_capabilities(false, true)
}

fn setup_terminal_with_capabilities(
    enable_client_protocols: bool,
    mouse_capture: bool,
) -> io::Result<TerminalGuard> {
    ratatui::init();
    crate::terminal_modes::clear_host_mouse_reporting(&mut io::stdout())?;
    let host_color_scheme_reports =
        should_enable_host_color_scheme_reports(enable_client_protocols);

    if enable_client_protocols {
        if mouse_capture {
            set_mouse_capture(true)?;
        } else {
            set_mouse_capture(false)?;
        }
        execute!(io::stdout(), EnableBracketedPaste, EnableFocusChange)?;
        if host_color_scheme_reports {
            write_host_color_scheme_report_mode(&mut io::stdout(), true)?;
        }
        push_keyboard_enhancement_flags()?;
    } else {
        if should_query_host_terminal_theme() {
            write_host_color_scheme_report_mode(&mut io::stdout(), false)?;
        }
        if mouse_capture {
            set_mouse_capture(true)?;
        } else {
            set_mouse_capture(false)?;
        }
    }

    #[cfg(windows)]
    let windows_virtual_terminal_input =
        if enable_client_protocols && windows_vti_input_backend_enabled() {
            enable_windows_virtual_terminal_input()
        } else {
            WindowsVirtualTerminalInputSetup::default()
        };

    #[cfg(windows)]
    if enable_client_protocols
        && windows_vti_input_backend_enabled()
        && windows_virtual_terminal_input.active
        && windows_win32_input_mode_enabled()
    {
        if let Err(err) = enable_windows_win32_input_mode(&mut io::stdout()) {
            if let Some(mode) = windows_virtual_terminal_input.restore_mode {
                restore_windows_input_mode_value(mode);
            }
            return Err(err);
        }
    }

    let modify_other_keys_mode = enable_client_protocols
        .then(crate::input::host_modify_other_keys_mode)
        .flatten();
    if let Some(mode) = modify_other_keys_mode {
        io::stdout().write_all(mode.set_sequence())?;
        io::stdout().flush()?;
    }

    Ok(TerminalGuard {
        reset_modify_other_keys: modify_other_keys_mode.is_some(),
        reset_host_color_scheme_reports: host_color_scheme_reports,
        #[cfg(windows)]
        restore_windows_input_mode: windows_virtual_terminal_input.restore_mode,
    })
}

fn should_enable_host_color_scheme_reports(enable_client_protocols: bool) -> bool {
    enable_client_protocols && should_query_host_terminal_theme()
}

/// Guard that restores the terminal when dropped.
struct TerminalGuard {
    reset_modify_other_keys: bool,
    reset_host_color_scheme_reports: bool,
    #[cfg(windows)]
    restore_windows_input_mode: Option<u32>,
}

fn write_host_color_scheme_report_mode(
    writer: &mut impl io::Write,
    enabled: bool,
) -> io::Result<()> {
    let sequence = if enabled {
        crate::terminal_theme::HOST_COLOR_SCHEME_REPORT_ENABLE_SEQUENCE
    } else {
        crate::terminal_theme::HOST_COLOR_SCHEME_REPORT_DISABLE_SEQUENCE
    };
    writer.write_all(sequence.as_bytes())?;
    writer.flush()
}

fn write_terminal_restore_postlude(
    writer: &mut impl io::Write,
    reset_host_color_scheme_reports: bool,
) -> io::Result<()> {
    if reset_host_color_scheme_reports {
        writer.write_all(
            crate::terminal_theme::HOST_COLOR_SCHEME_REPORT_DISABLE_SEQUENCE.as_bytes(),
        )?;
    }
    // Restore a visible cursor and reset DECSCUSR back to the terminal default.
    writer.write_all(b"\x1b[?25h\x1b[0 q")?;
    writer.flush()
}

fn should_draw_host_cursor(mode: crate::config::HostCursorModeConfig) -> bool {
    match mode {
        crate::config::HostCursorModeConfig::Auto => {
            crate::platform::should_draw_host_cursor_by_default()
        }
        crate::config::HostCursorModeConfig::Native => false,
        crate::config::HostCursorModeConfig::Drawn => true,
    }
}

#[cfg(windows)]
#[derive(Default)]
struct WindowsVirtualTerminalInputSetup {
    active: bool,
    restore_mode: Option<u32>,
}

#[cfg(windows)]
fn enable_windows_virtual_terminal_input() -> WindowsVirtualTerminalInputSetup {
    use windows_sys::Win32::Foundation::{HANDLE, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::System::Console::{
        GetConsoleMode, GetStdHandle, SetConsoleMode, ENABLE_VIRTUAL_TERMINAL_INPUT,
        STD_INPUT_HANDLE,
    };

    let handle: HANDLE = unsafe { GetStdHandle(STD_INPUT_HANDLE) };
    if handle.is_null() || handle == INVALID_HANDLE_VALUE {
        tracing::warn!("failed to get Windows console input handle for VT input");
        return WindowsVirtualTerminalInputSetup::default();
    }

    let mut mode = 0;
    if unsafe { GetConsoleMode(handle, &mut mode) } == 0 {
        tracing::warn!("failed to read Windows console input mode for VT input");
        return WindowsVirtualTerminalInputSetup::default();
    }

    let desired = windows_virtual_terminal_input_mode(mode);
    if desired == mode {
        return WindowsVirtualTerminalInputSetup {
            active: true,
            restore_mode: None,
        };
    }

    if unsafe { SetConsoleMode(handle, desired) } == 0 {
        tracing::warn!("failed to enable Windows virtual terminal input");
        return WindowsVirtualTerminalInputSetup::default();
    }

    let mut applied = 0;
    if unsafe { GetConsoleMode(handle, &mut applied) } == 0 {
        tracing::warn!("failed to verify Windows virtual terminal input mode");
        let _ = unsafe { SetConsoleMode(handle, mode) };
        return WindowsVirtualTerminalInputSetup::default();
    }
    if applied & ENABLE_VIRTUAL_TERMINAL_INPUT == 0 {
        tracing::warn!("Windows virtual terminal input bit did not stick");
        let _ = unsafe { SetConsoleMode(handle, mode) };
        return WindowsVirtualTerminalInputSetup::default();
    }

    WindowsVirtualTerminalInputSetup {
        active: true,
        restore_mode: Some(mode),
    }
}

#[cfg(windows)]
fn windows_vti_input_backend_enabled() -> bool {
    std::env::var("HERDR_WINDOWS_INPUT_BACKEND")
        .map(|backend| !backend.eq_ignore_ascii_case("crossterm"))
        .unwrap_or(true)
}

#[cfg(any(windows, test))]
fn windows_virtual_terminal_input_mode(mode: u32) -> u32 {
    mode | 0x0200
}

#[cfg(windows)]
fn restore_windows_input_mode_value(mode: u32) {
    use windows_sys::Win32::Foundation::{HANDLE, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::System::Console::{GetStdHandle, SetConsoleMode, STD_INPUT_HANDLE};

    let handle: HANDLE = unsafe { GetStdHandle(STD_INPUT_HANDLE) };
    if handle.is_null() || handle == INVALID_HANDLE_VALUE {
        return;
    }
    if unsafe { SetConsoleMode(handle, mode) } == 0 {
        tracing::warn!("failed to restore Windows console input mode");
    }
}

fn set_mouse_capture(enabled: bool) -> io::Result<()> {
    crate::terminal_modes::clear_host_mouse_reporting(&mut io::stdout())?;
    if enabled {
        execute!(io::stdout(), EnableMouseCapture)
    } else {
        match execute!(io::stdout(), DisableMouseCapture) {
            Ok(()) => Ok(()),
            #[cfg(windows)]
            Err(err) if err.to_string() == "Initial console modes not set" => Ok(()),
            Err(err) => Err(err),
        }
    }
}

/// Whether host mouse capture should be on: the server's request wins in single-server mode,
/// while a client-owned compositor always needs capture for its sidebar mouse UI.
#[cfg(unix)]
fn desired_mouse_capture(server_enabled: bool, client_compositor_enabled: bool) -> bool {
    server_enabled || client_compositor_enabled
}

fn restore_terminal_state(
    reset_modify_other_keys: bool,
    reset_host_color_scheme_reports: bool,
    #[cfg(windows)] restore_windows_input_mode: Option<u32>,
) {
    let _ = clear_received_kitty_graphics(&mut io::stdout());

    // Reset modifyOtherKeys if we enabled it.
    if reset_modify_other_keys {
        let _ = io::stdout().write_all(b"\x1b[>4;0m");
        let _ = io::stdout().flush();
    }

    let _ = pop_keyboard_enhancement_flags();

    let _ = execute!(
        io::stdout(),
        DisableFocusChange,
        DisableBracketedPaste,
        DisableMouseCapture
    );
    let _ = crate::terminal_modes::clear_host_mouse_reporting(&mut io::stdout());
    #[cfg(windows)]
    if let Some(mode) = restore_windows_input_mode {
        restore_windows_input_mode_value(mode);
    }

    ratatui::restore();
    let _ = write_terminal_restore_postlude(&mut io::stdout(), reset_host_color_scheme_reports);

    #[cfg(windows)]
    if windows_vti_input_backend_enabled() && windows_win32_input_mode_enabled() {
        let _ = disable_windows_win32_input_mode(&mut io::stdout());
    }
}

#[cfg(not(windows))]
fn push_keyboard_enhancement_flags() -> io::Result<()> {
    execute!(
        io::stdout(),
        PushKeyboardEnhancementFlags(crate::input::ime_compatible_keyboard_enhancement_flags())
    )
}

#[cfg(windows)]
fn push_keyboard_enhancement_flags() -> io::Result<()> {
    Ok(())
}

#[cfg(not(windows))]
fn pop_keyboard_enhancement_flags() -> io::Result<()> {
    execute!(io::stdout(), PopKeyboardEnhancementFlags)
}

#[cfg(windows)]
fn pop_keyboard_enhancement_flags() -> io::Result<()> {
    Ok(())
}

#[cfg(windows)]
fn windows_win32_input_mode_enabled() -> bool {
    std::env::var("HERDR_WINDOWS_INPUT_PROBE")
        .map(|probe| probe.eq_ignore_ascii_case("win32"))
        .unwrap_or(true)
}

#[cfg(windows)]
fn enable_windows_win32_input_mode(writer: &mut impl std::io::Write) -> io::Result<()> {
    writer.write_all(b"\x1b[?9001h")?;
    writer.flush()
}

#[cfg(windows)]
fn disable_windows_win32_input_mode(writer: &mut impl std::io::Write) -> io::Result<()> {
    writer.write_all(b"\x1b[?9001l")?;
    writer.flush()
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        restore_terminal_state(
            self.reset_modify_other_keys,
            self.reset_host_color_scheme_reports,
            #[cfg(windows)]
            self.restore_windows_input_mode,
        );
    }
}

// ---------------------------------------------------------------------------
// Handshake
// ---------------------------------------------------------------------------

fn requested_render_encoding() -> RenderEncoding {
    match std::env::var("HERDR_RENDER_ENCODING").ok().as_deref() {
        Some("terminal-ansi" | "terminal_ansi" | "ansi") => RenderEncoding::TerminalAnsi,
        _ => RenderEncoding::SemanticFrame,
    }
}

#[cfg(unix)]
fn is_remote_client_process() -> bool {
    std::env::var(crate::remote::REMOTE_KEYBINDINGS_ENV_VAR).is_ok()
}

/// Time to wait for the server's Welcome reply during the handshake.
///
/// A local client talks to an already-connected server, so 5s is plenty. The
/// remote bridge client (`herdr --remote`) sits behind a fresh per-attach ssh
/// connection whose cold-connect (TCP + key exchange + auth) happens inside this
/// window; on a high-latency link that easily exceeds 5s, so it gets a far
/// larger budget. See issue #753.
const LOCAL_HANDSHAKE_READ_TIMEOUT: Duration = Duration::from_secs(5);
#[cfg(unix)]
const REMOTE_HANDSHAKE_READ_TIMEOUT: Duration = Duration::from_secs(60);

fn handshake_read_timeout() -> Duration {
    #[cfg(unix)]
    if is_remote_client_process() {
        return REMOTE_HANDSHAKE_READ_TIMEOUT;
    }
    LOCAL_HANDSHAKE_READ_TIMEOUT
}

fn requested_keybindings() -> ClientKeybindings {
    match std::env::var(crate::remote::REMOTE_KEYBINDINGS_ENV_VAR)
        .ok()
        .as_deref()
    {
        Some("local") => crate::config::Config::load()
            .config
            .local_keybindings_profile_toml()
            .map(|keys_toml| ClientKeybindings::Local { keys_toml })
            .unwrap_or(ClientKeybindings::Server),
        _ => ClientKeybindings::Server,
    }
}

#[cfg(windows)]
fn set_handshake_recv_timeout(
    stream: &LocalStream,
    timeout: Option<Duration>,
    context: &'static str,
) -> Result<(), ClientError> {
    match stream.set_recv_timeout(timeout) {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == io::ErrorKind::Unsupported => {
            debug!(err = %err, context, "client socket receive timeout unavailable");
            Ok(())
        }
        Err(err) => Err(ClientError::ConnectionFailed(err)),
    }
}

#[cfg(not(windows))]
fn set_handshake_recv_timeout(
    stream: &LocalStream,
    timeout: Option<Duration>,
    _context: &'static str,
) -> Result<(), ClientError> {
    stream
        .set_recv_timeout(timeout)
        .map_err(ClientError::ConnectionFailed)
}

/// Performs the client→server handshake.
///
/// Sends Hello with the terminal size and protocol version, reads the Welcome
/// response. Returns Ok(()) on success, or an error if the server rejects us.
#[allow(clippy::too_many_arguments)] // handshake parameter list mirrors the wire Hello fields.
fn do_handshake(
    stream: &mut LocalStream,
    cols: u16,
    rows: u16,
    cell_width_px: u32,
    cell_height_px: u32,
    requested_encoding: RenderEncoding,
    surface_mode: protocol::ClientSurfaceMode,
    keybindings: ClientKeybindings,
    direct_attach_requested: bool,
) -> Result<RenderEncoding, ClientError> {
    stream
        .set_nonblocking(false)
        .map_err(ClientError::ConnectionFailed)?;

    // Send Hello.
    let hello = build_hello_message(
        cols,
        rows,
        cell_width_px,
        cell_height_px,
        requested_encoding,
        surface_mode,
        keybindings,
        direct_attach_requested,
    );
    protocol::write_message(stream, &hello)
        .map_err(|e| ClientError::ConnectionFailed(io::Error::other(e.to_string())))?;

    // Read Welcome.
    set_handshake_recv_timeout(
        stream,
        Some(handshake_read_timeout()),
        "client handshake read timeout unavailable",
    )?;
    let welcome: ServerMessage = protocol::read_message(stream, MAX_FRAME_SIZE)?;
    set_handshake_recv_timeout(
        stream,
        None,
        "failed to clear client handshake read timeout",
    )?;

    match welcome {
        ServerMessage::Welcome {
            version,
            encoding,
            error,
        } => {
            if let Some(error) = error {
                return Err(ClientError::HandshakeRejected { version, error });
            }
            info!(version, ?encoding, "handshake succeeded");
            Ok(encoding)
        }
        _ => Err(ClientError::Protocol(protocol::FramingError::Io(
            io::Error::new(io::ErrorKind::InvalidData, "expected Welcome message"),
        ))),
    }
}

#[allow(clippy::too_many_arguments)] // handshake parameter list mirrors the wire Hello fields.
fn build_hello_message(
    cols: u16,
    rows: u16,
    cell_width_px: u32,
    cell_height_px: u32,
    requested_encoding: RenderEncoding,
    surface_mode: protocol::ClientSurfaceMode,
    keybindings: ClientKeybindings,
    direct_attach_requested: bool,
) -> ClientMessage {
    ClientMessage::Hello {
        version: PROTOCOL_VERSION,
        cols,
        rows,
        cell_width_px,
        cell_height_px,
        requested_encoding,
        surface_mode,
        keybindings,
        launch_mode: if direct_attach_requested {
            ClientLaunchMode::TerminalAttach
        } else {
            ClientLaunchMode::App
        },
    }
}

// ---------------------------------------------------------------------------
// Client event loop
// ---------------------------------------------------------------------------

/// Internal events for the client event loop.
enum ClientLoopEvent {
    /// Raw input bytes from stdin.
    #[cfg(unix)]
    StdinInput(Vec<u8>),
    /// Structured input events from platforms without Unix-style stdin bytes.
    #[cfg(windows)]
    StdinEvents(Vec<crate::protocol::ClientInputEvent>),
    /// Terminal resize detected.
    Resize(u16, u16, u32, u32),
    /// Server message received, tagged with the owning server stream.
    #[cfg(unix)]
    ServerMessage {
        server_id: supervisor::ServerId,
        message: ServerMessage,
    },
    /// Server message received.
    #[cfg(windows)]
    ServerMessage(ServerMessage),
    /// A subscribed sidebar-summary event arrived from one managed server.
    #[cfg(unix)]
    SupervisorSummaryChanged(supervisor::ServerId),
    /// A secondary server summary refresh completed off the UI loop.
    #[cfg(unix)]
    SupervisorSummaryFetched {
        server_id: supervisor::ServerId,
        result: Result<supervisor::ServerSummary, supervisor::ConnectionState>,
        elapsed: Duration,
    },
    /// The MAIN server's registry/settings/summary bundle completed off the UI
    /// loop. Fetched asynchronously because the main api socket is an ssh
    /// bridge under `herdr --remote`, where each request is a WAN round-trip.
    #[cfg(unix)]
    MainSupervisorRefreshFinished {
        snapshot: Box<supervisor::MainSupervisorSnapshot>,
        elapsed: Duration,
    },
    /// A sidebar-summary subscription worker ended and should be eligible to restart.
    #[cfg(unix)]
    SupervisorSummarySubscriptionEnded(supervisor::ServerId),
    /// A sidebar API request completed off the UI loop.
    #[cfg(unix)]
    SupervisorApiRequestFinished {
        server_id: supervisor::ServerId,
        refresh: ClientApiRefreshPolicy,
        result: Result<Box<crate::api::schema::SuccessResponse>, String>,
        elapsed: Duration,
    },
    /// A secondary server client stream connection attempt completed off the UI loop.
    #[cfg(unix)]
    SecondaryConnectionAttemptFinished {
        server_id: supervisor::ServerId,
        attempt: usize,
        result: Result<SecondaryConnectionAttempt, ClientError>,
        elapsed: Duration,
    },
    /// Add-remote validation and setup completed off the UI loop.
    #[cfg(unix)]
    AddRemoteFinished {
        result: Result<ClientAddRemoteSuccess, AddRemoteFailure>,
        elapsed: Duration,
    },
    /// item 3 (Area 5): a remote-management `remote.set_enabled`/`remote.remove` request finished
    /// off the UI loop. The handler branches on `action` to apply teardown / reconnect.
    #[cfg(unix)]
    RemoteManageRequestFinished {
        action: RemoteManageAction,
        remote_id: String,
        result: Result<(), String>,
        elapsed: Duration,
    },
    /// Server reader thread exited (connection lost).
    #[cfg(unix)]
    ServerDisconnected(supervisor::ServerId),
    /// Server reader thread exited (connection lost).
    #[cfg(windows)]
    ServerDisconnected,
    /// Timer tick.
    Timer,
}

#[cfg(unix)]
struct SummarySubscriptionEndGuard {
    server_id: supervisor::ServerId,
    event_tx: tokio::sync::mpsc::Sender<ClientLoopEvent>,
}

#[cfg(unix)]
impl Drop for SummarySubscriptionEndGuard {
    fn drop(&mut self) {
        let _ = self
            .event_tx
            .blocking_send(ClientLoopEvent::SupervisorSummarySubscriptionEnded(
                self.server_id.clone(),
            ));
    }
}

/// Runs the thin client: connects to the server, performs the handshake,
/// and enters the main event loop.
///
/// This is the entry point called from `main.rs` when running in client mode.
pub fn run_client() -> io::Result<()> {
    run_client_with_mode(
        requested_render_encoding(),
        None,
        None,
        "connecting to server",
    )
}

/// Runs a direct terminal attach client.
#[cfg(unix)]
pub fn run_terminal_attach(terminal_id: String, takeover: bool) -> io::Result<()> {
    run_client_with_mode(
        RenderEncoding::TerminalAnsi,
        Some((terminal_id, takeover)),
        Some(AttachEscapeState::default()),
        "attaching to terminal",
    )
}

/// Direct terminal attach is Unix raw-byte input only until Windows gets a semantic attach path.
#[cfg(windows)]
pub fn run_terminal_attach(_terminal_id: String, _takeover: bool) -> io::Result<()> {
    debug_assert!(!crate::platform::capabilities().direct_terminal_attach);
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "direct terminal attach is not supported on Windows yet",
    ))
}

/// Runs a read-only terminal session observer and prints one JSON envelope per frame.
pub fn run_terminal_session_observe(target: String, cols: u16, rows: u16) -> io::Result<()> {
    let mut stream =
        connect_terminal_session_stream(target.clone(), cols, rows, "observing terminal session")?;
    write_to_server(&mut stream, &ClientMessage::ObserveTerminal { target })?;
    write_terminal_session_output(stream)
}

/// Runs a writable terminal session controller.
pub fn run_terminal_session_control(
    target: String,
    takeover: bool,
    cols: u16,
    rows: u16,
) -> io::Result<()> {
    let mut stream = connect_terminal_session_stream(
        target.clone(),
        cols,
        rows,
        "controlling terminal session",
    )?;
    write_to_server(
        &mut stream,
        &ClientMessage::ControlTerminal { target, takeover },
    )?;

    let mut write_stream = stream.try_clone()?;
    let _input_thread = std::thread::spawn(move || {
        let stdin = io::stdin();
        for line in stdin.lock().lines() {
            let Ok(line) = line else {
                break;
            };
            if line.trim().is_empty() {
                continue;
            }
            match terminal_control_command_from_json(&line) {
                Ok(message) => {
                    let release = matches!(message, ClientMessage::Detach);
                    if write_to_server(&mut write_stream, &message).is_err() {
                        return;
                    }
                    if release {
                        return;
                    }
                }
                Err(err) => eprintln!("herdr: terminal session control input ignored: {err}"),
            }
        }
        let _ = write_to_server(&mut write_stream, &ClientMessage::Detach);
    });

    write_terminal_session_output(stream)
}

fn connect_terminal_session_stream(
    target: String,
    cols: u16,
    rows: u16,
    log_message: &'static str,
) -> io::Result<LocalStream> {
    init_logging();

    let socket_path = client_socket_path();
    crate::logging::startup("client");
    info!(path = %socket_path.display(), target = %target, cols, rows, "{log_message}");

    let mut stream = match crate::ipc::connect_local_stream(&socket_path) {
        Ok(stream) => stream,
        Err(err) => {
            eprintln!("herdr: {}", ClientError::ConnectionFailed(err));
            std::process::exit(1);
        }
    };

    match do_handshake(
        &mut stream,
        cols,
        rows,
        0,
        0,
        RenderEncoding::TerminalAnsi,
        protocol::ClientSurfaceMode::FullApp,
        requested_keybindings(),
        true,
    ) {
        Ok(RenderEncoding::TerminalAnsi) => {}
        Ok(encoding) => {
            eprintln!(
                "herdr: terminal session observe negotiated unsupported encoding {encoding:?}"
            );
            std::process::exit(1);
        }
        Err(err) => {
            eprintln!("herdr: {err}");
            std::process::exit(1);
        }
    }

    stream.set_nonblocking(false)?;
    Ok(stream)
}

fn write_terminal_session_output(mut stream: LocalStream) -> io::Result<()> {
    let mut stdout = io::stdout().lock();
    loop {
        match protocol::read_message(&mut stream, MAX_GRAPHICS_FRAME_SIZE) {
            Ok(ServerMessage::Terminal(frame)) => {
                let encoded = base64::engine::general_purpose::STANDARD.encode(&frame.bytes);
                let line = serde_json::json!({
                    "type": "terminal.frame",
                    "seq": frame.seq,
                    "encoding": "ansi",
                    "width": frame.width,
                    "height": frame.height,
                    "full": frame.full,
                    "bytes": encoded,
                });
                serde_json::to_writer(&mut stdout, &line)?;
                stdout.write_all(b"\n")?;
                stdout.flush()?;
            }
            Ok(ServerMessage::ServerShutdown { reason }) => {
                let line = serde_json::json!({
                    "type": "terminal.closed",
                    "reason": reason,
                });
                serde_json::to_writer(&mut stdout, &line)?;
                stdout.write_all(b"\n")?;
                stdout.flush()?;
                return Ok(());
            }
            Ok(ServerMessage::Graphics { .. }) => {}
            Ok(_) => {}
            Err(protocol::FramingError::UnexpectedEof) => return Ok(()),
            Err(err) => return Err(io::Error::other(err.to_string())),
        }
    }
}

#[derive(serde::Deserialize)]
#[serde(tag = "type")]
enum TerminalControlCommand {
    #[serde(rename = "terminal.input")]
    Input {
        text: Option<String>,
        bytes: Option<String>,
    },
    #[serde(rename = "terminal.resize")]
    Resize {
        cols: u16,
        rows: u16,
        #[serde(default)]
        cell_width_px: u32,
        #[serde(default)]
        cell_height_px: u32,
    },
    #[serde(rename = "terminal.scroll")]
    Scroll {
        direction: TerminalControlScrollDirection,
        lines: u16,
        #[serde(default)]
        source: TerminalControlScrollSource,
        #[serde(default)]
        column: Option<u16>,
        #[serde(default)]
        row: Option<u16>,
        #[serde(default)]
        modifiers: u8,
    },
    #[serde(rename = "terminal.release")]
    Release {},
}

#[derive(Clone, Copy, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
enum TerminalControlScrollDirection {
    Up,
    Down,
}

#[derive(Clone, Copy, Default, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
enum TerminalControlScrollSource {
    #[default]
    Wheel,
    PageKey,
}

fn terminal_control_command_from_json(raw: &str) -> Result<ClientMessage, String> {
    let command = serde_json::from_str::<TerminalControlCommand>(raw)
        .map_err(|err| format!("invalid json command: {err}"))?;
    match command {
        TerminalControlCommand::Input { text, bytes } => {
            let data = match (text, bytes) {
                (Some(_), Some(_)) => {
                    return Err("terminal.input accepts text or bytes, not both".into())
                }
                (Some(text), None) => text.into_bytes(),
                (None, Some(bytes)) => base64::engine::general_purpose::STANDARD
                    .decode(bytes)
                    .map_err(|err| format!("invalid terminal.input bytes: {err}"))?,
                (None, None) => Vec::new(),
            };
            Ok(ClientMessage::Input { data })
        }
        TerminalControlCommand::Resize {
            cols,
            rows,
            cell_width_px,
            cell_height_px,
        } => {
            if cols == 0 || rows == 0 {
                return Err("terminal.resize cols and rows must be greater than 0".into());
            }
            Ok(ClientMessage::Resize {
                cols,
                rows,
                cell_width_px,
                cell_height_px,
            })
        }
        TerminalControlCommand::Scroll {
            direction,
            lines,
            source,
            column,
            row,
            modifiers,
        } => {
            if lines == 0 {
                return Err("terminal.scroll lines must be greater than 0".into());
            }
            let direction = match direction {
                TerminalControlScrollDirection::Up => AttachScrollDirection::Up,
                TerminalControlScrollDirection::Down => AttachScrollDirection::Down,
            };
            let source = match source {
                TerminalControlScrollSource::Wheel => AttachScrollSource::Wheel,
                TerminalControlScrollSource::PageKey => AttachScrollSource::PageKey {
                    input: match direction {
                        AttachScrollDirection::Up => b"\x1b[5~".to_vec(),
                        AttachScrollDirection::Down => b"\x1b[6~".to_vec(),
                    },
                },
            };
            Ok(ClientMessage::AttachScroll {
                source,
                direction,
                lines,
                column,
                row,
                modifiers,
            })
        }
        TerminalControlCommand::Release {} => Ok(ClientMessage::Detach),
    }
}

fn run_client_with_mode(
    requested_encoding: RenderEncoding,
    attach_request: Option<(String, bool)>,
    attach_escape: Option<AttachEscapeState>,
    log_message: &'static str,
) -> io::Result<()> {
    init_logging();

    let loaded_config = crate::config::Config::load();
    crate::terminal_modes::clear_host_mouse_reporting(&mut io::stdout())?;
    let mouse_capture = loaded_config.config.ui.mouse_capture;
    let mouse_scroll_lines = loaded_config.config.ui.mouse_scroll_lines();
    let redraw_on_focus_gained = loaded_config.config.ui.redraw_on_focus_gained;
    let host_cursor = loaded_config.config.ui.host_cursor;
    let direct_attach_requested = attach_request.is_some();
    #[cfg(unix)]
    let remote_image_paste_key = client_remote_image_paste_key(&loaded_config.config);
    let kitty_graphics_enabled =
        loaded_config.config.experimental.kitty_graphics && !direct_attach_requested;

    let socket_path = client_socket_path();
    crate::logging::startup("client");
    info!(path = %socket_path.display(), "{log_message}");

    // Get the terminal geometry before handshake (before raw mode).
    let (cols, rows, cell_width_px, cell_height_px) =
        current_terminal_geometry(kitty_graphics_enabled);

    // Bootstrap the multi-server supervisor from the main API. On success the client owns the
    // unified sidebar and every server (main + secondaries) renders EmbeddedContent next to it;
    // on failure (or for direct attach) the client behaves exactly like the single-server client.
    // Ssh remote bridges are unix-only, so the whole mixed-server client is unix-only too.
    #[cfg(unix)]
    let supervisor_model = {
        let mut api = crate::api::client::ApiClient::local();
        match bootstrap_client_supervisor_model(direct_attach_requested, &mut api) {
            Ok(model) => model,
            Err(err) => {
                warn!(err = %err, "failed to bootstrap client supervisor from main API");
                None
            }
        }
    };
    #[cfg(unix)]
    if let Some(model) = &supervisor_model {
        debug!(
            secondary_servers = model.secondary_connection_plans().len(),
            workspace_rows = model.workspace_rows().len(),
            "client supervisor bootstrapped"
        );
    }

    #[cfg(unix)]
    let render_plan =
        client_render_plan(supervisor_model.as_ref(), requested_encoding, (cols, rows));
    #[cfg(unix)]
    let use_client_compositor = render_plan.use_client_compositor;
    #[cfg(unix)]
    let (hello_size, hello_encoding, hello_surface_mode) = (
        render_plan.server_size,
        render_plan.requested_encoding,
        render_plan.surface_mode,
    );
    #[cfg(windows)]
    let (hello_size, hello_encoding, hello_surface_mode) = (
        (cols, rows),
        requested_encoding,
        protocol::ClientSurfaceMode::FullApp,
    );

    let loop_options = ClientLoopOptions {
        sound_config: loaded_config.config.ui.sound,
        mouse_scroll_lines,
        redraw_on_focus_gained,
        host_cursor,
        kitty_graphics_enabled,
        #[cfg(unix)]
        mouse_capture_active: mouse_capture || use_client_compositor,
        #[cfg(windows)]
        mouse_capture_active: mouse_capture,
        #[cfg(unix)]
        remote_image_paste_key,
        host_size: (cols, rows),
        reported_size: hello_size,
        #[cfg(unix)]
        cell_size_px: (cell_width_px, cell_height_px),
        #[cfg(unix)]
        compositor: use_client_compositor.then(|| {
            let mut compositor = compositor::ClientCompositor::default();
            // The composited sidebar renders client-side: resolve the theme and the
            // collapsed-sidebar mode from the CLIENT's local config so it matches
            // the server-rendered look and collapse behavior.
            if let Ok(loaded) = crate::config::load_live_config() {
                compositor.set_palette(crate::app::client_palette_from_config(&loaded.config));
                compositor.set_collapsed_mode(loaded.config.ui.sidebar_collapsed_mode);
            }
            compositor
        }),
        #[cfg(unix)]
        supervisor_model,
    };

    // Try to connect to the server.
    let mut stream = match crate::ipc::connect_local_stream(&socket_path) {
        Ok(s) => s,
        Err(err) => {
            // Server unreachable — show clear error and exit.
            let client_err = ClientError::ConnectionFailed(err);
            eprintln!("herdr: {client_err}");
            std::process::exit(1);
        }
    };

    // Perform handshake while the stream is still in blocking mode.
    let negotiated_encoding = match do_handshake(
        &mut stream,
        hello_size.0,
        hello_size.1,
        cell_width_px,
        cell_height_px,
        hello_encoding,
        hello_surface_mode,
        requested_keybindings(),
        direct_attach_requested,
    ) {
        Ok(encoding) => encoding,
        Err(err) => {
            eprintln!("herdr: {err}");
            std::process::exit(1);
        }
    };

    if let Some((terminal_id, takeover)) = attach_request {
        let attach = ClientMessage::AttachTerminal {
            terminal_id,
            takeover,
        };
        if let Err(err) = write_to_server(&mut stream, &attach) {
            eprintln!("herdr: failed to request terminal attach: {err}");
            std::process::exit(1);
        }
    }

    // Now set up the terminal. This must happen AFTER the handshake succeeds,
    // so we don't leave the terminal in raw mode if the server rejects us.
    let direct_attach = attach_escape.is_some();
    let terminal_guard = if direct_attach {
        setup_direct_attach_terminal()
    } else {
        setup_terminal(loop_options.mouse_capture_active)
    }
    .map_err(|err| {
        eprintln!("herdr: failed to set up terminal: {err}");
        err
    })?;

    // Install a panic hook to restore the terminal on panic (same as monolithic).
    let panic_resets_modify_other_keys = terminal_guard.reset_modify_other_keys;
    let panic_resets_host_color_scheme_reports = terminal_guard.reset_host_color_scheme_reports;
    #[cfg(windows)]
    let panic_restore_windows_input_mode = terminal_guard.restore_windows_input_mode;
    let original_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        restore_terminal_state(
            panic_resets_modify_other_keys,
            panic_resets_host_color_scheme_reports,
            #[cfg(windows)]
            panic_restore_windows_input_mode,
        );
        original_hook(info);
    }));

    // Create the tokio runtime.
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(io::Error::other)?;

    let should_quit = Arc::new(AtomicBool::new(false));

    // Install Ctrl+C handler.
    let quit_flag = should_quit.clone();
    let _ = ctrlc::set_handler(move || {
        quit_flag.store(true, Ordering::Release);
    });

    let result = rt.block_on(async {
        run_client_loop(
            stream,
            should_quit,
            loop_options,
            negotiated_encoding,
            attach_escape,
        )
        .await
    });

    // Restore the terminal before printing any final status message.
    drop(terminal_guard);

    if let Err(err) = result {
        eprintln!("herdr: {err}");
        rt.shutdown_timeout(Duration::from_millis(100));
        crate::logging::shutdown("client");

        if matches!(
            err,
            ClientError::ServerShutdown {
                reason: Some(reason)
            } if reason == "detached"
        ) {
            return Ok(());
        }

        std::process::exit(1);
    }

    rt.shutdown_timeout(Duration::from_millis(100));
    crate::logging::shutdown("client");
    Ok(())
}

/// Bootstrap the client supervisor model from the main server's API, unless the client is a
/// direct terminal attach (which never owns a sidebar).
#[cfg(unix)]
fn bootstrap_supervisor_for_client(
    direct_attach_requested: bool,
    api: &mut impl supervisor::SupervisorApi,
) -> Result<Option<supervisor::ClientSupervisorModel>, String> {
    if direct_attach_requested {
        return Ok(None);
    }

    supervisor::bootstrap_from_main_api(api, main_display_name_for_client()).map(Some)
}

#[cfg(unix)]
fn bootstrap_client_supervisor_model(
    direct_attach_requested: bool,
    api: &mut impl supervisor::SupervisorApi,
) -> Result<Option<supervisor::ClientSupervisorModel>, String> {
    bootstrap_supervisor_for_client(direct_attach_requested, api)
}

/// The sidebar display name for the main server. A `herdr --remote` launcher exports the remote's
/// name; a plain local client falls back to "local".
#[cfg(unix)]
fn main_display_name_for_client() -> String {
    std::env::var(crate::remote::MAIN_DISPLAY_NAME_ENV_VAR)
        .ok()
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| "local".to_string())
}

#[cfg(unix)]
fn api_target_for_supervisor_server(
    model: &supervisor::ClientSupervisorModel,
    server_id: &supervisor::ServerId,
    ssh_bridges: &HashMap<supervisor::ServerId, crate::remote::RemoteBridge>,
) -> Option<crate::api::client::ConnectionTarget> {
    let target = model.server_connection_target(server_id)?;
    api_target_for_supervisor_target(server_id, &target, ssh_bridges)
}

#[cfg(unix)]
fn api_target_for_supervisor_target(
    server_id: &supervisor::ServerId,
    target: &supervisor::ServerConnectionTarget,
    ssh_bridges: &HashMap<supervisor::ServerId, crate::remote::RemoteBridge>,
) -> Option<crate::api::client::ConnectionTarget> {
    match target {
        supervisor::ServerConnectionTarget::Ssh { .. } => {
            ssh_bridges.get(server_id).map(|bridge| {
                crate::api::client::ConnectionTarget::SocketPath(
                    bridge.api_socket_path().to_path_buf(),
                )
            })
        }
        _ => api_target_for_connection_target(target),
    }
}

#[cfg(unix)]
fn api_target_for_connection_target(
    target: &supervisor::ServerConnectionTarget,
) -> Option<crate::api::client::ConnectionTarget> {
    match target {
        supervisor::ServerConnectionTarget::Main => {
            Some(crate::api::client::ConnectionTarget::LocalSession(None))
        }
        supervisor::ServerConnectionTarget::LocalSession(session) => Some(
            crate::api::client::ConnectionTarget::LocalSession(session.clone()),
        ),
        supervisor::ServerConnectionTarget::Ssh { .. } => None,
    }
}

#[cfg(unix)]
fn client_socket_path_for_connection_target(
    target: &supervisor::ServerConnectionTarget,
) -> Option<std::path::PathBuf> {
    match target {
        supervisor::ServerConnectionTarget::Main => Some(client_socket_path()),
        supervisor::ServerConnectionTarget::LocalSession(session) => {
            Some(crate::session::client_socket_path_for(session.as_deref()))
        }
        supervisor::ServerConnectionTarget::Ssh { .. } => None,
    }
}

#[cfg(all(unix, test))]
fn client_socket_path_for_supervisor_server(
    model: &supervisor::ClientSupervisorModel,
    server_id: &supervisor::ServerId,
    ssh_bridges: &HashMap<supervisor::ServerId, crate::remote::RemoteBridge>,
) -> Option<std::path::PathBuf> {
    let target = model.server_connection_target(server_id)?;
    match target {
        supervisor::ServerConnectionTarget::Ssh { .. } => ssh_bridges
            .get(server_id)
            .map(|bridge| bridge.client_socket_path().to_path_buf()),
        _ => client_socket_path_for_connection_target(&target),
    }
}

/// Connect + handshake one secondary's client stream for a reconnect/startup plan, off the UI
/// loop. Ssh plans reuse the existing bridge's client socket when it is still up; otherwise a
/// fresh bridge is started (never auto-restarting an incompatible server — reconnects can't
/// prompt).
#[cfg(unix)]
fn connect_secondary_client_stream_for_plan_detached(
    plan: supervisor::SecondaryConnectionPlan,
    server_size: (u16, u16),
    cell_width_px: u32,
    cell_height_px: u32,
    existing_ssh_client_socket: Option<std::path::PathBuf>,
) -> Result<SecondaryConnectionAttempt, ClientError> {
    let socket_path = match &plan.target {
        supervisor::ServerConnectionTarget::Ssh {
            destination,
            options,
        } => {
            if let Some(path) = existing_ssh_client_socket {
                path
            } else {
                let ssh_target =
                    crate::remote::SshTarget::new(destination.clone(), options.clone());
                // Reconnect path: can't prompt, so never auto-restart an incompatible server.
                let bridge = crate::remote::start_ssh_remote_bridge(ssh_target, false, None)
                    .map_err(ClientError::ConnectionFailed)?;
                let socket_path = bridge.client_socket_path().to_path_buf();
                return connect_secondary_client_stream(
                    &socket_path,
                    server_size,
                    cell_width_px,
                    cell_height_px,
                    plan.keybindings,
                )
                .map(|stream| SecondaryConnectionAttempt {
                    stream,
                    bridge: Some(bridge),
                });
            }
        }
        _ => client_socket_path_for_connection_target(&plan.target).ok_or_else(|| {
            ClientError::ConnectionFailed(io::Error::new(
                io::ErrorKind::InvalidInput,
                "secondary server has no client socket target",
            ))
        })?,
    };

    connect_secondary_client_stream(
        &socket_path,
        server_size,
        cell_width_px,
        cell_height_px,
        plan.keybindings,
    )
    .map(|stream| SecondaryConnectionAttempt {
        stream,
        bridge: None,
    })
}

/// Connect + handshake a secondary server's client stream as an embedded-content,
/// semantic-frame surface (the compositor renders the unified sidebar).
#[cfg(unix)]
fn connect_secondary_client_stream(
    socket_path: &std::path::Path,
    server_size: (u16, u16),
    cell_width_px: u32,
    cell_height_px: u32,
    keybindings: crate::remote_registry::RemoteKeybindingsSnapshot,
) -> Result<LocalStream, ClientError> {
    let mut stream =
        crate::ipc::connect_local_stream(socket_path).map_err(ClientError::ConnectionFailed)?;
    do_handshake(
        &mut stream,
        server_size.0,
        server_size.1,
        cell_width_px,
        cell_height_px,
        RenderEncoding::SemanticFrame,
        ClientSurfaceMode::EmbeddedContent,
        client_keybindings_from_snapshot(keybindings),
        false,
    )?;
    Ok(stream)
}

/// Wire a freshly handshaken secondary stream into the loop: a tagged reader thread (with the
/// per-server byte counter) plus a writer thread registered in `server_writes`.
#[cfg(unix)]
fn attach_secondary_client_stream(
    server_id: supervisor::ServerId,
    stream: LocalStream,
    rx_bytes: Arc<std::sync::atomic::AtomicU64>,
    event_tx: &tokio::sync::mpsc::Sender<ClientLoopEvent>,
    should_quit: &Arc<AtomicBool>,
    server_writes: &mut HashMap<supervisor::ServerId, ServerWriteHandle>,
) -> Result<(), ClientError> {
    let read_stream = stream.try_clone().map_err(ClientError::ConnectionFailed)?;
    let read_tx = event_tx.clone();
    let read_quit = should_quit.clone();
    let reader_server_id = server_id.clone();
    std::thread::spawn(move || {
        server_reader_thread(
            reader_server_id,
            read_stream,
            rx_bytes,
            read_tx,
            &read_quit,
            MAX_FRAME_SIZE,
        );
    });
    stream
        .set_nonblocking(false)
        .map_err(ClientError::ConnectionFailed)?;
    let write_handle = spawn_server_writer(server_id.clone(), stream, event_tx.clone());
    server_writes.insert(server_id, write_handle);
    Ok(())
}

/// Spawn the per-server writer thread. Writes are queued from the async loop and flushed on a
/// dedicated blocking thread so one slow/broken secondary socket can never stall the UI loop.
#[cfg(unix)]
fn spawn_server_writer(
    server_id: supervisor::ServerId,
    mut stream: LocalStream,
    event_tx: tokio::sync::mpsc::Sender<ClientLoopEvent>,
) -> ServerWriteHandle {
    let (tx, rx) = std::sync::mpsc::channel::<ClientMessage>();
    std::thread::spawn(move || {
        while let Ok(message) = rx.recv() {
            if let Err(err) = write_to_server(&mut stream, &message) {
                warn!(
                    server_id = ?server_id,
                    err = %err,
                    "server writer failed"
                );
                let _ = event_tx.blocking_send(ClientLoopEvent::ServerDisconnected(server_id));
                return;
            }
        }
    });
    ServerWriteHandle { tx }
}

#[cfg(unix)]
fn connection_state_from_client_error(err: &ClientError) -> supervisor::ConnectionState {
    match err {
        ClientError::HandshakeRejected { version, .. } => {
            supervisor::ConnectionState::ProtocolMismatch {
                server_protocol: Some(*version),
                client_protocol: PROTOCOL_VERSION,
            }
        }
        _ => supervisor::ConnectionState::Disconnected,
    }
}

#[cfg(unix)]
fn client_keybindings_from_snapshot(
    keybindings: crate::remote_registry::RemoteKeybindingsSnapshot,
) -> ClientKeybindings {
    match keybindings {
        crate::remote_registry::RemoteKeybindingsSnapshot::Server => ClientKeybindings::Server,
        crate::remote_registry::RemoteKeybindingsSnapshot::Local => crate::config::Config::load()
            .config
            .local_keybindings_profile_toml()
            .map(|keys_toml| ClientKeybindings::Local { keys_toml })
            .unwrap_or(ClientKeybindings::Server),
    }
}

#[cfg(all(unix, test))]
fn send_client_supervisor_request(
    model: &supervisor::ClientSupervisorModel,
    server_id: &supervisor::ServerId,
    request: crate::api::schema::Request,
    ssh_bridges: &HashMap<supervisor::ServerId, crate::remote::RemoteBridge>,
) -> Result<(), String> {
    let target = api_target_for_supervisor_server(model, server_id, ssh_bridges)
        .ok_or_else(|| format!("no API target for server {server_id:?}"))?;
    send_client_supervisor_request_to_target(target, request).map(|_| ())
}

#[cfg(unix)]
fn send_client_supervisor_request_to_target(
    target: crate::api::client::ConnectionTarget,
    request: crate::api::schema::Request,
) -> Result<crate::api::schema::SuccessResponse, String> {
    let api = crate::api::client::ApiClient::for_target(target);
    let value = api
        .request_value_with_timeout(&request, CLIENT_SUPERVISOR_API_TIMEOUT)
        .map_err(|err| err.to_string())?;
    crate::api::client::parse_response_value(value).map_err(|err| err.to_string())
}

/// Route a sidebar-originated API request to the owning server off the UI loop, emitting
/// `SupervisorApiRequestFinished` when it completes.
#[cfg(unix)]
fn spawn_client_supervisor_request(
    model: &supervisor::ClientSupervisorModel,
    server_id: supervisor::ServerId,
    refresh: ClientApiRefreshPolicy,
    request: crate::api::schema::Request,
    ssh_bridges: &HashMap<supervisor::ServerId, crate::remote::RemoteBridge>,
    event_tx: &tokio::sync::mpsc::Sender<ClientLoopEvent>,
) -> Result<(), String> {
    let target = api_target_for_supervisor_server(model, &server_id, ssh_bridges)
        .ok_or_else(|| format!("no API target for server {server_id:?}"))?;
    let event_tx = event_tx.clone();
    std::thread::spawn(move || {
        let started_at = Instant::now();
        let result = send_client_supervisor_request_to_target(target, request).map(Box::new);
        let elapsed = started_at.elapsed();
        let _ = event_tx.blocking_send(ClientLoopEvent::SupervisorApiRequestFinished {
            server_id,
            refresh,
            result,
            elapsed,
        });
    });
    Ok(())
}

#[cfg(unix)]
fn fps_for_frame_duration(duration: Duration) -> f64 {
    if duration.is_zero() {
        f64::INFINITY
    } else {
        1.0 / duration.as_secs_f64()
    }
}

#[cfg(unix)]
fn submit_remote_add_to_main_api(
    api: &mut impl supervisor::SupervisorApi,
    draft: supervisor::AddRemoteDraft,
) -> Result<crate::remote_registry::RemoteDefinitionSnapshot, String> {
    let response = api
        .request(crate::api::schema::Request {
            id: "client:remote-add".into(),
            method: crate::api::schema::Method::RemoteAdd(crate::api::schema::RemoteAddParams {
                name: draft.name,
                target: draft.target,
                keybindings: draft.keybindings,
            }),
        })
        .map_err(|err| add_remote_error_message(&err))?;
    match response.result {
        crate::api::schema::ResponseResult::RemoteAdded { remote } => Ok(remote),
        other => Err(format!("remote.add returned unexpected result: {other:?}")),
    }
}

#[cfg(unix)]
fn add_remote_error_message(error: &str) -> String {
    match error {
        "remote target already exists" => "remote already added".to_string(),
        "remote name already exists" => "name already used".to_string(),
        other => map_remote_bridge_error(other),
    }
}

/// Map raw ssh/bridge failures into short, actionable dialog text. The add-remote worker can fail
/// for very different reasons (host unreachable, ssh auth, missing/old herdr); a bare io error
/// string is not helpful in the small dialog status row.
#[cfg(unix)]
fn map_remote_bridge_error(error: &str) -> String {
    let lower = error.to_ascii_lowercase();
    if lower.contains("timed out") {
        "timed out reaching host — check the address and your ssh access".to_string()
    } else if lower.contains("connection refused")
        || lower.contains("could not resolve")
        || lower.contains("name or service not known")
        || lower.contains("no route to host")
    {
        "cannot reach host over ssh — check the address".to_string()
    } else if lower.contains("permission denied") || lower.contains("authentication") {
        "ssh authentication failed — set up key access to this host".to_string()
    } else if lower.contains("does not support live-handoff") || lower.contains("protocol") {
        "remote herdr is incompatible and can't be upgraded in place — update it and retry"
            .to_string()
    } else {
        error.to_string()
    }
}

#[cfg(unix)]
fn summary_refresh_subscription_request(id: impl Into<String>) -> crate::api::schema::Request {
    use crate::api::schema::Subscription;

    crate::api::schema::Request {
        id: id.into(),
        method: crate::api::schema::Method::EventsSubscribe(
            crate::api::schema::EventsSubscribeParams {
                subscriptions: vec![
                    Subscription::WorkspaceCreated {},
                    Subscription::WorkspaceUpdated {},
                    Subscription::WorkspaceRenamed {},
                    Subscription::WorkspaceClosed {},
                    Subscription::WorkspaceFocused {},
                    Subscription::TabCreated {},
                    Subscription::TabClosed {},
                    Subscription::TabFocused {},
                    Subscription::TabRenamed {},
                    Subscription::PaneCreated {},
                    Subscription::PaneClosed {},
                    Subscription::PaneFocused {},
                    Subscription::PaneExited {},
                    Subscription::PaneAgentDetected {},
                    Subscription::PaneAgentStatusChanged {
                        pane_id: None,
                        agent_status: None,
                    },
                ],
            },
        ),
    }
}

#[cfg(unix)]
fn start_missing_supervisor_summary_subscriptions(
    model: &supervisor::ClientSupervisorModel,
    subscribed_server_ids: &mut HashSet<supervisor::ServerId>,
    ssh_bridges: &HashMap<supervisor::ServerId, crate::remote::RemoteBridge>,
    event_tx: &tokio::sync::mpsc::Sender<ClientLoopEvent>,
    should_quit: &Arc<AtomicBool>,
) {
    for plan in model.summary_subscription_plans() {
        let Some(target) =
            api_target_for_supervisor_target(&plan.server_id, &plan.target, ssh_bridges)
        else {
            continue;
        };
        if !subscribed_server_ids.insert(plan.server_id.clone()) {
            continue;
        }
        spawn_supervisor_summary_subscription(plan.server_id, target, event_tx, should_quit);
    }
}

#[cfg(unix)]
fn spawn_supervisor_summary_subscription(
    server_id: supervisor::ServerId,
    target: crate::api::client::ConnectionTarget,
    event_tx: &tokio::sync::mpsc::Sender<ClientLoopEvent>,
    should_quit: &Arc<AtomicBool>,
) {
    let event_tx = event_tx.clone();
    let should_quit = should_quit.clone();
    std::thread::spawn(move || {
        let changed_event_tx = event_tx.clone();
        let _end_guard = SummarySubscriptionEndGuard {
            server_id: server_id.clone(),
            event_tx,
        };
        let client = crate::api::client::ApiClient::for_target(target);
        let request = summary_refresh_subscription_request(format!("client:summary:{server_id:?}"));
        let (ack, mut stream) =
            match client.subscribe_value(&request, Some(CLIENT_SUPERVISOR_API_TIMEOUT)) {
                Ok(value) => value,
                Err(err) => {
                    warn!(
                        server_id = ?server_id,
                        err = %err,
                        "failed to subscribe to supervisor summary events"
                    );
                    return;
                }
            };
        if let Err(err) = crate::api::client::parse_response_value(ack) {
            warn!(
                server_id = ?server_id,
                err = %err,
                "supervisor summary subscription was rejected"
            );
            return;
        }

        while !should_quit.load(Ordering::Acquire) {
            match stream.next_value() {
                Ok(Some(_event)) => {
                    if changed_event_tx
                        .blocking_send(ClientLoopEvent::SupervisorSummaryChanged(server_id.clone()))
                        .is_err()
                    {
                        return;
                    }
                }
                Ok(None) => return,
                Err(err) if api_client_error_is_timeout(&err) => continue,
                Err(err) => {
                    warn!(
                        server_id = ?server_id,
                        err = %err,
                        "supervisor summary subscription ended"
                    );
                    return;
                }
            }
        }
    });
}

#[cfg(unix)]
fn api_client_error_is_timeout(err: &crate::api::client::ApiClientError) -> bool {
    matches!(
        err,
        crate::api::client::ApiClientError::Io(io_err)
            if matches!(
                io_err.kind(),
                io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock
            )
    )
}

/// item 3 (Area 5): the kind of registry mutation a manage request performs. Carried back in
/// `RemoteManageRequestFinished` so the handler can branch teardown vs. reconnect.
#[cfg(unix)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RemoteManageAction {
    SetEnabled { enabled: bool },
    Delete,
}

/// item 3 (Area 5): build the `remote.set_enabled`/`remote.remove` request for a manage action.
#[cfg(unix)]
fn remote_manage_request(
    action: RemoteManageAction,
    remote_id: &str,
) -> crate::api::schema::Request {
    let method = match action {
        RemoteManageAction::SetEnabled { enabled } => crate::api::schema::Method::RemoteSetEnabled(
            crate::api::schema::RemoteSetEnabledParams {
                remote_id: remote_id.to_string(),
                enabled,
            },
        ),
        RemoteManageAction::Delete => {
            crate::api::schema::Method::RemoteRemove(crate::api::schema::RemoteRemoveParams {
                remote_id: remote_id.to_string(),
            })
        }
    };
    crate::api::schema::Request {
        id: "client:remote-manage".into(),
        method,
    }
}

/// item 3 (Area 5): spawn the `remote.set_enabled`/`remote.remove` request off the UI loop against
/// `ServerId::main()` (the local socket — no SSH bridge needed), then emit
/// `RemoteManageRequestFinished`. Modeled on `spawn_client_add_remote_submission`; it does NOT
/// reuse `spawn_client_supervisor_request` (which emits the unrelated `SupervisorApiRequestFinished`
/// and discards the response body), because the manage handler must branch on `action`.
#[cfg(unix)]
fn spawn_client_remote_manage_request(
    model: &supervisor::ClientSupervisorModel,
    action: RemoteManageAction,
    remote_id: String,
    ssh_bridges: &HashMap<supervisor::ServerId, crate::remote::RemoteBridge>,
    event_tx: &tokio::sync::mpsc::Sender<ClientLoopEvent>,
) {
    let main_id = supervisor::ServerId::main();
    let target = api_target_for_supervisor_server(model, &main_id, ssh_bridges);
    let request = remote_manage_request(action, &remote_id);
    let event_tx = event_tx.clone();
    std::thread::spawn(move || {
        let started_at = Instant::now();
        let result = match target {
            Some(target) => send_client_supervisor_request_to_target(target, request).map(|_| ()),
            None => Err("no API target for main server".to_string()),
        };
        let elapsed = started_at.elapsed();
        let _ = event_tx.blocking_send(ClientLoopEvent::RemoteManageRequestFinished {
            action,
            remote_id,
            result,
            elapsed,
        });
    });
}

#[cfg(unix)]
fn spawn_client_add_remote_submission(
    draft: supervisor::AddRemoteDraft,
    server_size: (u16, u16),
    cell_size_px: (u32, u32),
    event_tx: &tokio::sync::mpsc::Sender<ClientLoopEvent>,
    pending_add_remote: &mut bool,
) {
    if *pending_add_remote {
        return;
    }
    *pending_add_remote = true;
    let event_tx = event_tx.clone();
    std::thread::spawn(move || {
        let started_at = Instant::now();
        let result = prepare_client_add_remote_submission(draft, server_size, cell_size_px);
        let elapsed = started_at.elapsed();
        let _ = event_tx.blocking_send(ClientLoopEvent::AddRemoteFinished { result, elapsed });
    });
}

/// Outcome of a bounded remote op (see [`run_remote_op_with_timeout`]). Preserves the underlying
/// `io::Error` so callers can downcast typed signals (e.g. [`crate::remote::RestartConfirmNeeded`]).
#[cfg(unix)]
#[derive(Debug)]
enum RemoteOpError {
    TimedOut(Duration),
    WorkerGone,
    Failed(io::Error),
}

/// Why an add-remote submission did not succeed.
#[cfg(unix)]
#[derive(Debug)]
enum AddRemoteFailure {
    /// A plain message for the dialog's status row.
    Message(String),
    /// The remote runs an incompatible server that can't live-handoff; ask the user whether to
    /// restart it. `detail` is the prompt text; `destination` re-targets the retry.
    NeedsRestartConfirm { destination: String, detail: String },
}

#[cfg(unix)]
impl From<String> for AddRemoteFailure {
    fn from(message: String) -> Self {
        AddRemoteFailure::Message(message)
    }
}

/// Run a blocking remote operation on a helper thread, failing if it does not finish within
/// `timeout`. Bounds ssh bridge setup so the add-remote worker can never wedge the dialog on an
/// unreachable/slow/auth-prompting host (see [`ADD_REMOTE_BRIDGE_TIMEOUT`]).
#[cfg(unix)]
fn run_remote_op_with_timeout<T, F>(timeout: Duration, op: F) -> Result<T, RemoteOpError>
where
    T: Send + 'static,
    F: FnOnce() -> io::Result<T> + Send + 'static,
{
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(op());
    });
    match rx.recv_timeout(timeout) {
        Ok(Ok(value)) => Ok(value),
        Ok(Err(err)) => Err(RemoteOpError::Failed(err)),
        Err(std::sync::mpsc::RecvTimeoutError::Timeout) => Err(RemoteOpError::TimedOut(timeout)),
        Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => Err(RemoteOpError::WorkerGone),
    }
}

/// Turn a bridge-setup failure into a dialog-facing [`AddRemoteFailure`]. A
/// [`crate::remote::RestartConfirmNeeded`] signal becomes a y/N confirm; everything else becomes a
/// mapped error message.
#[cfg(unix)]
fn classify_add_remote_bridge_error(err: RemoteOpError) -> AddRemoteFailure {
    match err {
        RemoteOpError::TimedOut(timeout) => AddRemoteFailure::Message(format!(
            "failed to start ssh remote bridge: timed out after {}s connecting to the remote host",
            timeout.as_secs()
        )),
        RemoteOpError::WorkerGone => AddRemoteFailure::Message(
            "failed to start ssh remote bridge: remote connection worker exited unexpectedly"
                .to_string(),
        ),
        RemoteOpError::Failed(err) => match crate::remote::restart_confirm_needed(&err) {
            Some(confirm) => AddRemoteFailure::NeedsRestartConfirm {
                destination: confirm.destination.clone(),
                detail: confirm.to_string(),
            },
            None => AddRemoteFailure::Message(format!(
                "failed to start ssh remote bridge: {}",
                map_remote_bridge_error(&err.to_string())
            )),
        },
    }
}

#[cfg(unix)]
fn prepare_client_add_remote_submission(
    draft: supervisor::AddRemoteDraft,
    server_size: (u16, u16),
    cell_size_px: (u32, u32),
) -> Result<ClientAddRemoteSuccess, AddRemoteFailure> {
    let target = crate::remote_registry::RemoteTargetSnapshot::parse(&draft.target)
        .map_err(|err| err.message().to_string())?;
    reject_duplicate_main_target(&target)?;

    let keybindings = draft.keybindings;
    let restart_incompatible = draft.restart_incompatible;
    let (stream, bridge) = match &target {
        crate::remote_registry::RemoteTargetSnapshot::Local { session } => {
            validate_add_remote_target(
                crate::api::client::ConnectionTarget::LocalSession(session.clone()),
                |connection_target| {
                    let mut api = crate::api::client::ApiClient::for_target(connection_target);
                    supervisor::request_runtime_status(&mut api)
                },
            )?;
            let socket_path = crate::session::client_socket_path_for(session.as_deref());
            let stream = connect_secondary_client_stream(
                &socket_path,
                server_size,
                cell_size_px.0,
                cell_size_px.1,
                keybindings,
            )
            .map_err(|err| err.to_string())?;
            (stream, None)
        }
        crate::remote_registry::RemoteTargetSnapshot::Ssh { target, args } => {
            let ssh_target = crate::remote::SshTarget::new(target.clone(), args.clone());
            let bridge = run_remote_op_with_timeout(ADD_REMOTE_BRIDGE_TIMEOUT, move || {
                crate::remote::start_ssh_remote_bridge(ssh_target, restart_incompatible, None)
            })
            .map_err(classify_add_remote_bridge_error)?;
            validate_add_remote_target(
                crate::api::client::ConnectionTarget::SocketPath(
                    bridge.api_socket_path().to_path_buf(),
                ),
                |connection_target| {
                    let mut api = crate::api::client::ApiClient::for_target(connection_target);
                    supervisor::request_runtime_status(&mut api)
                },
            )?;
            let stream = connect_secondary_client_stream(
                bridge.client_socket_path(),
                server_size,
                cell_size_px.0,
                cell_size_px.1,
                keybindings,
            )
            .map_err(|err| err.to_string())?;
            (stream, Some(bridge))
        }
    };

    let mut main_api = crate::api::client::ApiClient::local();
    let remote = submit_remote_add_to_main_api(&mut main_api, draft)?;
    Ok(ClientAddRemoteSuccess {
        remote,
        stream,
        bridge,
    })
}

#[cfg(unix)]
fn reject_duplicate_main_target(
    target: &crate::remote_registry::RemoteTargetSnapshot,
) -> Result<(), String> {
    let Some(main_target) = main_server_target_snapshot() else {
        return Ok(());
    };
    if main_target.canonical_key() == target.canonical_key() {
        return Err("remote already added".to_string());
    }
    Ok(())
}

#[cfg(unix)]
fn main_server_target_snapshot() -> Option<crate::remote_registry::RemoteTargetSnapshot> {
    if let Ok(target) = std::env::var(crate::remote::MAIN_REMOTE_TARGET_ENV_VAR) {
        return crate::remote_registry::RemoteTargetSnapshot::parse(&target).ok();
    }

    Some(crate::remote_registry::RemoteTargetSnapshot::Local {
        session: crate::session::active_name(),
    })
}

#[cfg(unix)]
fn validate_add_remote_target(
    target: crate::api::client::ConnectionTarget,
    mut status_for_target: impl FnMut(
        crate::api::client::ConnectionTarget,
    ) -> Result<crate::api::RuntimeStatus, String>,
) -> Result<(), String> {
    let deadline = Instant::now() + ADD_REMOTE_TARGET_VALIDATE_TIMEOUT;
    loop {
        match status_for_target(target.clone()) {
            Ok(status) => {
                if status.protocol != Some(PROTOCOL_VERSION) {
                    return Err(format!(
                        "protocol mismatch: server protocol {:?}, client protocol {}",
                        status.protocol, PROTOCOL_VERSION
                    ));
                }
                return Ok(());
            }
            Err(err)
                if add_remote_target_status_error_is_transient(&err)
                    && Instant::now() < deadline =>
            {
                std::thread::sleep(ADD_REMOTE_TARGET_VALIDATE_RETRY_DELAY);
            }
            Err(err) => return Err(err),
        }
    }
}

#[cfg(unix)]
fn add_remote_target_status_error_is_transient(error: &str) -> bool {
    let error = error.to_ascii_lowercase();
    error.contains("resource temporarily unavailable")
        || error.contains("operation would block")
        || error.contains("timed out")
        || error.contains("connection refused")
        || error.contains("no such file or directory")
}

/// The main/registry/ui-settings refresh, fetched OFF the UI loop over one api
/// connection and posted back as [`ClientLoopEvent::MainSupervisorRefreshFinished`].
///
/// This must never run synchronously on the UI loop: the main api socket is
/// only a fast local socket for a locally-attached client. Under
/// `herdr --remote <host>` it is an ssh bridge, where every request is a fresh
/// bridge exec plus a WAN round-trip — a synchronous fetch froze input and
/// frame rendering for the whole bundle (the "super slow" remote attach).
///
/// Deduped through the SAME pending/queued sets the secondary refreshes use,
/// keyed by `ServerId::main()`: a change signal that lands while a fetch is in
/// flight queues exactly one rerun instead of stacking threads.
#[cfg(unix)]
fn start_main_supervisor_refresh(
    pending: &mut HashSet<supervisor::ServerId>,
    queued: &mut HashSet<supervisor::ServerId>,
    event_tx: &tokio::sync::mpsc::Sender<ClientLoopEvent>,
) {
    let main_id = supervisor::ServerId::main();
    if pending.contains(&main_id) {
        queued.insert(main_id);
        return;
    }
    pending.insert(main_id);
    let event_tx = event_tx.clone();
    std::thread::spawn(move || {
        let started_at = Instant::now();
        let mut api = crate::api::client::ApiClient::local();
        let snapshot = supervisor::fetch_main_supervisor_snapshot(&mut api);
        let elapsed = started_at.elapsed();
        let _ = event_tx.blocking_send(ClientLoopEvent::MainSupervisorRefreshFinished {
            snapshot: Box::new(snapshot),
            elapsed,
        });
    });
}

#[cfg(unix)]
fn refresh_client_supervisor_summaries(
    model: &mut supervisor::ClientSupervisorModel,
    ssh_bridges: &HashMap<supervisor::ServerId, crate::remote::RemoteBridge>,
    pending_summary_refresh_server_ids: &mut HashSet<supervisor::ServerId>,
    queued_summary_refresh_server_ids: &mut HashSet<supervisor::ServerId>,
    event_tx: &tokio::sync::mpsc::Sender<ClientLoopEvent>,
) {
    start_main_supervisor_refresh(
        pending_summary_refresh_server_ids,
        queued_summary_refresh_server_ids,
        event_tx,
    );
    let immediate_results = start_secondary_supervisor_summary_refreshes(
        model,
        ssh_bridges,
        pending_summary_refresh_server_ids,
        event_tx,
    );
    model.apply_secondary_summary_results(immediate_results);
}

#[cfg(unix)]
fn start_secondary_supervisor_summary_refreshes(
    model: &supervisor::ClientSupervisorModel,
    ssh_bridges: &HashMap<supervisor::ServerId, crate::remote::RemoteBridge>,
    pending_summary_refresh_server_ids: &mut HashSet<supervisor::ServerId>,
    event_tx: &tokio::sync::mpsc::Sender<ClientLoopEvent>,
) -> Vec<(
    supervisor::ServerId,
    Result<supervisor::ServerSummary, supervisor::ConnectionState>,
)> {
    let mut immediate_results = Vec::new();
    for plan in model.secondary_connection_plans() {
        if pending_summary_refresh_server_ids.contains(&plan.server_id) {
            continue;
        }
        let Some(target) =
            api_target_for_supervisor_target(&plan.server_id, &plan.target, ssh_bridges)
        else {
            immediate_results.push((plan.server_id, Err(supervisor::ConnectionState::Connecting)));
            continue;
        };
        let server_id = plan.server_id;
        pending_summary_refresh_server_ids.insert(server_id.clone());
        let event_tx = event_tx.clone();
        std::thread::spawn(move || {
            let started_at = Instant::now();
            let result = supervisor::fetch_server_summary_from_api_target(target);
            let elapsed = started_at.elapsed();
            let _ = event_tx.blocking_send(ClientLoopEvent::SupervisorSummaryFetched {
                server_id,
                result,
                elapsed,
            });
        });
    }
    immediate_results
}

/// item 6 (Area 6): a targeted single-server summary fetch. Mirrors the per-plan body of
/// `start_secondary_supervisor_summary_refreshes` for exactly ONE server id, so a focus / connect
/// / event-push refreshes only the changed server (not the whole fleet).
///
/// The main-server id is a no-op here: the local main refresh requires `&mut self` and is owned
/// by each caller (which already holds `&mut state.supervisor_model`). Secondary ids dedupe via
/// `pending` and spawn the fetch off the UI loop (NO blocking SSH/API call on the loop).
#[cfg(unix)]
fn start_single_secondary_summary_refresh(
    model: &supervisor::ClientSupervisorModel,
    server_id: &supervisor::ServerId,
    ssh_bridges: &HashMap<supervisor::ServerId, crate::remote::RemoteBridge>,
    pending: &mut HashSet<supervisor::ServerId>,
    queued: &mut HashSet<supervisor::ServerId>,
    event_tx: &tokio::sync::mpsc::Sender<ClientLoopEvent>,
) {
    // Main-server id: not an SSH-bridged secondary — the caller routes main
    // through `start_main_supervisor_refresh` instead.
    if *server_id == supervisor::ServerId::main() {
        return;
    }
    // Dedupe: a refresh for this server is already running off the UI loop.
    // Queue a rerun instead of dropping the signal — the in-flight fetch may
    // have read pre-change state, and silently dropping pushed the update to
    // the next poll tick (a visible sidebar lag on create/close).
    if pending.contains(server_id) {
        queued.insert(server_id.clone());
        return;
    }
    let Some(target) = model.server_connection_target(server_id) else {
        return;
    };
    let Some(api_target) = api_target_for_supervisor_target(server_id, &target, ssh_bridges) else {
        // The SSH bridge is not up yet; the connect/retry path owns bringing it up and the next
        // tick re-attempts. Do nothing this call.
        return;
    };
    pending.insert(server_id.clone());
    let server_id = server_id.clone();
    let event_tx = event_tx.clone();
    std::thread::spawn(move || {
        let started_at = Instant::now();
        // Ping-free: the targeted refresh only runs for connected servers, and
        // over an ssh bridge each request is a fresh remote exec.
        let result = supervisor::fetch_connected_server_summary_from_api_target(api_target);
        let elapsed = started_at.elapsed();
        let _ = event_tx.blocking_send(ClientLoopEvent::SupervisorSummaryFetched {
            server_id,
            result,
            elapsed,
        });
    });
}

#[cfg(unix)]
fn supervisor_summary_refresh_due(now: Instant, last_refresh: Instant) -> bool {
    now.duration_since(last_refresh) >= CLIENT_SUPERVISOR_REFRESH_INTERVAL
}

/// item 6 (Area 6): the adaptive secondary refresh schedule. Returns the connected secondary ids
/// whose per-server cadence is due at `now`. The active remote uses the fast 400ms cadence; all
/// other connected secondaries use the 2s background cadence. Main is ALWAYS excluded (the local
/// main refresh is owned by the 2s gate / the `&mut` callers). A server with no recorded
/// `last_summary_refresh` is treated as due immediately. This is a pure helper so the cadence is
/// unit-testable and the Timer body issues no inline blocking call.
#[cfg(unix)]
fn due_secondary_summary_refreshes(state: &ClientState, now: Instant) -> Vec<supervisor::ServerId> {
    let Some(model) = state.supervisor_model.as_ref() else {
        return Vec::new();
    };
    let active = model.active_server_id();
    model
        .summary_subscription_plans()
        .into_iter()
        .map(|plan| plan.server_id)
        .filter(|server_id| *server_id != supervisor::ServerId::main())
        .filter(|server_id| {
            let interval = if server_id == active {
                CLIENT_FOCUSED_SUMMARY_REFRESH_INTERVAL
            } else {
                CLIENT_SUPERVISOR_REFRESH_INTERVAL
            };
            match state.last_summary_refresh.get(server_id) {
                Some(last) => now.duration_since(*last) >= interval,
                None => true,
            }
        })
        .collect()
}

/// item 5: the select-loop wakeup deadline. With nothing animating we keep the existing 100ms
/// housekeeping cadence (idle behavior unchanged, zero recompose). While animating we wake at
/// whichever is sooner: the 100ms housekeeping tick or the next 80ms animation step. Kept on
/// std `Instant` for unit-testability; the call site converts to `tokio::time::Instant`.
#[cfg(unix)]
fn next_select_deadline(
    now: Instant,
    last_animation_tick: Instant,
    wants_animation: bool,
) -> Instant {
    let housekeeping = now + Duration::from_millis(100);
    if wants_animation {
        housekeeping.min(last_animation_tick + CLIENT_ANIMATION_INTERVAL)
    } else {
        housekeeping
    }
}

/// item 5: whether the gated animation step should advance the tick this Timer event. True only
/// when something is animating AND at least one full 80ms interval has elapsed since the last
/// advance — the `last_animation_tick` guard coalesces sub-80ms Timer storms to <=1 tick.
#[cfg(unix)]
fn should_advance_animation(
    wants_animation: bool,
    now: Instant,
    last_animation_tick: Instant,
) -> bool {
    wants_animation && now.duration_since(last_animation_tick) >= CLIENT_ANIMATION_INTERVAL
}

#[cfg(unix)]
fn secondary_retry_delay(attempt: usize) -> Duration {
    match attempt {
        0 => Duration::from_secs(1),
        1 => Duration::from_secs(2),
        2 => Duration::from_secs(5),
        _ => Duration::from_secs(15),
    }
}

/// item 3 (Area 5): apply the result of a finished `remote.set_enabled`/`remote.remove` request.
/// The registry refresh rides the synchronous-against-local-main path inside
/// `refresh_client_supervisor_summaries` (LOCAL socket, no SSH RTT — the same call
/// `AddRemoteFinished` makes), so it stays on the loop without violating the off-UI-loop SSH rule;
/// the off-thread part was only the `set_enabled`/`remove` request itself. On error it clears
/// `pending`. On success it refreshes the registry and then:
/// - re-enable → explicit `Connecting` (so the now-ungated plans pick it up next tick),
/// - disable-while-connected → teardown like `ServerDisconnected` + `Disconnected`,
/// - delete → `remove_secondary` + teardown.
#[cfg(unix)]
fn apply_remote_manage_request_finished(
    state: &mut ClientState,
    server_writes: &mut HashMap<supervisor::ServerId, ServerWriteHandle>,
    action: RemoteManageAction,
    remote_id: &str,
    result: Result<(), String>,
    event_tx: &tokio::sync::mpsc::Sender<ClientLoopEvent>,
) {
    let server_id = supervisor::ServerId::secondary(remote_id);
    if let Err(err) = result {
        warn!(remote_id = %remote_id, err = %err, "remote-manage request failed");
        if let Some(model) = &mut state.supervisor_model {
            model.clear_remote_manage_pending(remote_id);
        }
        return;
    }

    match action {
        RemoteManageAction::SetEnabled { enabled: true } => {
            if let Some(model) = &mut state.supervisor_model {
                refresh_client_supervisor_summaries(
                    model,
                    &state.ssh_bridges,
                    &mut state.pending_summary_refresh_server_ids,
                    &mut state.queued_summary_refresh_server_ids,
                    event_tx,
                );
                // re-enable MUST explicitly yield `Connecting` so the now-ungated
                // `unconnected_secondary_server_ids()` picks it up next tick
                // (`sync_remote_registry` never re-applies connection_state).
                let _ =
                    model.set_connection_state(&server_id, supervisor::ConnectionState::Connecting);
                model.clear_remote_manage_pending(remote_id);
            }
        }
        RemoteManageAction::SetEnabled { enabled: false } => {
            if let Some(model) = &mut state.supervisor_model {
                refresh_client_supervisor_summaries(
                    model,
                    &state.ssh_bridges,
                    &mut state.pending_summary_refresh_server_ids,
                    &mut state.queued_summary_refresh_server_ids,
                    event_tx,
                );
            }
            teardown_secondary_connection(state, server_writes, &server_id);
            if let Some(model) = &mut state.supervisor_model {
                let _ = model
                    .set_connection_state(&server_id, supervisor::ConnectionState::Disconnected);
                model.clear_remote_manage_pending(remote_id);
            }
        }
        RemoteManageAction::Delete => {
            teardown_secondary_connection(state, server_writes, &server_id);
            if let Some(model) = &mut state.supervisor_model {
                model.remove_secondary(&server_id);
                refresh_client_supervisor_summaries(
                    model,
                    &state.ssh_bridges,
                    &mut state.pending_summary_refresh_server_ids,
                    &mut state.queued_summary_refresh_server_ids,
                    event_tx,
                );
                model.clear_remote_manage_pending(remote_id);
            }
        }
    }
}

/// item 3 (Area 5): tear down a secondary's stream/bridge/poll state exactly like the
/// `ServerDisconnected` handler does (remove from `server_writes`, `frame_cache`,
/// `summary_subscription_server_ids`, `pending_summary_refresh_server_ids`,
/// `pending_secondary_connect_server_ids`, `ssh_bridges`). Unlike `ServerDisconnected` it does NOT
/// schedule a retry — the caller (disable / delete) wants the remote to stay down (the gated
/// producers exclude a disabled remote; a deleted remote is gone). Does NOT touch the model
/// `connection_state` (the caller sets it).
#[cfg(unix)]
fn teardown_secondary_connection(
    state: &mut ClientState,
    server_writes: &mut HashMap<supervisor::ServerId, ServerWriteHandle>,
    server_id: &supervisor::ServerId,
) {
    server_writes.remove(server_id);
    state.frame_cache.remove(server_id);
    state.summary_subscription_server_ids.remove(server_id);
    state.pending_summary_refresh_server_ids.remove(server_id);
    state.queued_summary_refresh_server_ids.remove(server_id);
    state.pending_secondary_connect_server_ids.remove(server_id);
    state.ssh_bridges.remove(server_id);
    state.secondary_retries.remove(server_id);
}

#[cfg(unix)]
fn schedule_secondary_retry(
    state: &mut ClientState,
    server_id: supervisor::ServerId,
    attempt: usize,
    now: Instant,
) {
    state.secondary_retries.insert(
        server_id,
        SecondaryRetryState {
            attempt,
            next_retry_at: now + secondary_retry_delay(attempt),
        },
    );
}

#[cfg(unix)]
fn schedule_missing_secondary_stream_retries(
    state: &mut ClientState,
    server_writes: &HashMap<supervisor::ServerId, ServerWriteHandle>,
    now: Instant,
) {
    let Some(model) = &state.supervisor_model else {
        return;
    };
    let connected_streams: HashSet<_> = server_writes.keys().cloned().collect();
    let retry_server_ids = model
        .secondary_server_ids_missing_client_stream(&connected_streams)
        .into_iter()
        .chain(model.unconnected_secondary_server_ids());
    for server_id in retry_server_ids {
        state
            .secondary_retries
            .entry(server_id.clone())
            .or_insert_with(|| SecondaryRetryState {
                attempt: 0,
                next_retry_at: now,
            });
    }
}

#[cfg(unix)]
fn handle_server_write_failure(
    state: &mut ClientState,
    server_writes: &mut HashMap<supervisor::ServerId, ServerWriteHandle>,
    server_id: supervisor::ServerId,
    error: io::Error,
    now: Instant,
) -> Result<(), ClientError> {
    if server_id == supervisor::ServerId::main() {
        return Err(ClientError::ConnectionLost(error));
    }

    warn!(
        server_id = ?server_id,
        err = %error,
        "secondary server write failed; marking it disconnected"
    );
    server_writes.remove(&server_id);
    state.frame_cache.remove(&server_id);
    state.summary_subscription_server_ids.remove(&server_id);
    state.pending_summary_refresh_server_ids.remove(&server_id);
    state
        .pending_secondary_connect_server_ids
        .remove(&server_id);
    state.ssh_bridges.remove(&server_id);
    if let Some(model) = &mut state.supervisor_model {
        let _ = model.set_connection_state(&server_id, supervisor::ConnectionState::Disconnected);
    }
    schedule_secondary_retry(state, server_id, 0, now);
    state.request_full_redraw();
    Ok(())
}

#[cfg(unix)]
fn retry_due_secondary_connections(
    state: &mut ClientState,
    now: Instant,
    event_tx: &tokio::sync::mpsc::Sender<ClientLoopEvent>,
    server_writes: &mut HashMap<supervisor::ServerId, ServerWriteHandle>,
) {
    let due: Vec<(supervisor::ServerId, usize)> = state
        .secondary_retries
        .iter()
        .filter(|(_, retry)| retry.next_retry_at <= now)
        .map(|(server_id, retry)| (server_id.clone(), retry.attempt))
        .collect();

    for (server_id, attempt) in due {
        if server_writes.contains_key(&server_id) {
            state.secondary_retries.remove(&server_id);
            continue;
        }
        if state
            .pending_secondary_connect_server_ids
            .contains(&server_id)
        {
            continue;
        }

        let plan = state.supervisor_model.as_ref().and_then(|model| {
            model
                .secondary_connection_plans()
                .into_iter()
                .find(|plan| plan.server_id == server_id)
        });
        let Some(plan) = plan else {
            state.secondary_retries.remove(&server_id);
            continue;
        };

        let existing_ssh_client_socket = state
            .ssh_bridges
            .get(&server_id)
            .map(|bridge| bridge.client_socket_path().to_path_buf());
        state
            .pending_secondary_connect_server_ids
            .insert(server_id.clone());
        spawn_secondary_connection_retry(
            server_id.clone(),
            attempt,
            plan,
            state.reported_size,
            state.cell_size_px.0,
            state.cell_size_px.1,
            existing_ssh_client_socket,
            event_tx,
        );
        if let Some(model) = &mut state.supervisor_model {
            let _ = model.set_connection_state(&server_id, supervisor::ConnectionState::Connecting);
        }
        state.request_full_redraw();
    }
}

#[cfg(unix)]
#[allow(clippy::too_many_arguments)] // mirrors the connect-plan fields threaded to the worker.
fn spawn_secondary_connection_retry(
    server_id: supervisor::ServerId,
    attempt: usize,
    plan: supervisor::SecondaryConnectionPlan,
    server_size: (u16, u16),
    cell_width_px: u32,
    cell_height_px: u32,
    existing_ssh_client_socket: Option<std::path::PathBuf>,
    event_tx: &tokio::sync::mpsc::Sender<ClientLoopEvent>,
) {
    let event_tx = event_tx.clone();
    std::thread::spawn(move || {
        let started_at = Instant::now();
        let result = connect_secondary_client_stream_for_plan_detached(
            plan,
            server_size,
            cell_width_px,
            cell_height_px,
            existing_ssh_client_socket,
        );
        let elapsed = started_at.elapsed();
        let _ = event_tx.blocking_send(ClientLoopEvent::SecondaryConnectionAttemptFinished {
            server_id,
            attempt,
            result,
            elapsed,
        });
    });
}

/// Response-first switching (issue #13): the composited frame always shows the ACTIVE server's
/// last-known content, regardless of which server the incoming frame came from.
#[cfg(unix)]
fn select_composited_render_frame<'a>(
    frames: &'a HashMap<supervisor::ServerId, protocol::FrameData>,
    active_server_id: &supervisor::ServerId,
    _incoming_server_id: &supervisor::ServerId,
) -> Option<&'a protocol::FrameData> {
    frames.get(active_server_id)
}

/// Recompose + blit from the cached frames (sidebar-only changes: hover, scroll, overlays,
/// animation, connection state). Paints the active server's last-known frame instantly; if none
/// has been received yet, a blank content frame repaints the new shell at once instead of holding
/// the previous server's screen.
#[cfg(unix)]
fn render_cached_composited_frame(state: &mut ClientState) {
    let composed = match (&state.compositor, &state.supervisor_model) {
        (Some(compositor), Some(model)) => {
            let active_server_id = model.active_server_id().clone();
            let active_frame = state
                .frame_cache
                .get(&active_server_id)
                .cloned()
                .unwrap_or_else(|| {
                    protocol::FrameData::blank(state.host_size.0, state.host_size.1)
                });
            compositor.compose_frame(
                model,
                &active_frame,
                state.host_size.0,
                state.host_size.1,
                Instant::now(),
            )
        }
        _ => return,
    };
    blit_client_frame_with_stats(state, composed);
}

/// Cache a server's freshly-received full frame and paint: composited mode composes the active
/// server's frame next to the client sidebar; single-server mode blits it directly. The cache
/// entry doubles as the per-server `FrameDelta` baseline (raw, exactly as received — the drawn
/// cursor never contaminates it). Shared by the `Frame` and (reconstructed) `FrameDelta` paths.
#[cfg(unix)]
fn render_incoming_server_frame(
    state: &mut ClientState,
    server_id: &supervisor::ServerId,
    frame_data: protocol::FrameData,
) {
    state
        .frame_cache
        .insert(server_id.clone(), frame_data.clone());
    let composed = match (&state.compositor, &state.supervisor_model) {
        (Some(compositor), Some(model)) => {
            let active_server_id = model.active_server_id().clone();
            let Some(active_frame) =
                select_composited_render_frame(&state.frame_cache, &active_server_id, server_id)
            else {
                // No frame received for the active server yet; keep the current paint until
                // its first frame (or a recompose) arrives.
                return;
            };
            Some(compositor.compose_frame(
                model,
                active_frame,
                state.host_size.0,
                state.host_size.1,
                Instant::now(),
            ))
        }
        _ => None,
    };
    match composed {
        Some(frame) => blit_client_frame_with_stats(state, frame),
        None => blit_client_frame_with_stats(state, frame_data),
    }
}

/// Blit one full frame to the host terminal via the shared single-server path, recording the
/// render duration for the 60fps budget diagnostics.
#[cfg(unix)]
fn blit_client_frame_with_stats(state: &mut ClientState, frame_data: protocol::FrameData) {
    let render_started_at = Instant::now();
    draw_semantic_frame(state, frame_data);
    record_client_frame_sample(state, render_started_at.elapsed());
}

/// Derive each server's recent downstream bytes/sec from the cumulative reader counters and push
/// it into the supervisor model for the host banner. Rate-limited to [`RX_RATE_SAMPLE_INTERVAL`].
#[cfg(unix)]
fn sample_download_rates(state: &mut ClientState, now: Instant) {
    if now.duration_since(state.last_rx_sample_at) < RX_RATE_SAMPLE_INTERVAL {
        return;
    }
    state.last_rx_sample_at = now;

    let snapshot = state.rx_counters.snapshot();
    let mut rates: Vec<(supervisor::ServerId, u64)> = Vec::new();
    for (server_id, bytes) in snapshot {
        if let Some((prev_bytes, prev_at)) = state.server_rx_sample.get(&server_id) {
            let dt = now.duration_since(*prev_at).as_secs_f64();
            if dt > 0.0 {
                let delta = bytes.saturating_sub(*prev_bytes);
                rates.push((server_id.clone(), (delta as f64 / dt) as u64));
            }
        }
        state.server_rx_sample.insert(server_id, (bytes, now));
    }

    if let Some(model) = state.supervisor_model.as_mut() {
        for (server_id, rate) in rates {
            model.set_server_download_bps(&server_id, rate);
        }
    }
}

#[cfg(unix)]
fn record_client_frame_sample(state: &mut ClientState, render_duration: Duration) {
    let sample = state.frame_stats.record_render_duration(render_duration);
    if sample.missed_sixty_fps_budget {
        debug!(
            render_ms = sample.render_duration.as_secs_f64() * 1000.0,
            render_fps = sample.render_fps,
            frame_budget_fps = fps_for_frame_duration(CLIENT_60FPS_FRAME_BUDGET),
            "client frame render missed 60fps budget"
        );
    }
}
/// The main client event loop.
///
/// Uses a threaded architecture:
/// - stdin reader thread → sends raw input bytes to main loop
/// - resize poller thread → sends resize events to main loop
/// - server reader thread(s) → read ServerMessages and send to main loop
/// - server writer thread(s) (unix) → flush queued ClientMessages per server
/// - main loop: coordinates input, output, and server communication
async fn run_client_loop(
    stream: LocalStream,
    should_quit: Arc<AtomicBool>,
    options: ClientLoopOptions,
    negotiated_encoding: RenderEncoding,
    attach_escape: Option<AttachEscapeState>,
) -> Result<(), ClientError> {
    #[cfg(windows)]
    let _ = (options.mouse_scroll_lines, options.host_size);
    let draw_host_cursor = attach_escape.is_none() && should_draw_host_cursor(options.host_cursor);
    #[cfg(unix)]
    let is_remote_client = is_remote_client_process();

    let mut state = ClientState {
        blit_encoder: render_ansi::BlitEncoder::new(),
        #[cfg(windows)]
        server_frame_baseline: None,
        mouse_capture_active: options.mouse_capture_active,
        reported_size: options.reported_size,
        #[cfg(unix)]
        host_size: options.host_size,
        #[cfg(unix)]
        cell_size_px: options.cell_size_px,
        sound_config: options.sound_config,
        kitty_graphics_enabled: options.kitty_graphics_enabled,
        attach_escape,
        #[cfg(unix)]
        mouse_scroll_lines: options.mouse_scroll_lines,
        #[cfg(unix)]
        remote_image_paste_key: options.remote_image_paste_key,
        redraw_on_focus_gained: options.redraw_on_focus_gained,
        draw_host_cursor,
        #[cfg(unix)]
        frame_stats: ClientFrameStats::default(),
        #[cfg(unix)]
        compositor: options.compositor,
        #[cfg(unix)]
        supervisor_model: options.supervisor_model,
        #[cfg(unix)]
        last_supervisor_summary_refresh: Instant::now(),
        #[cfg(unix)]
        frame_cache: HashMap::new(),
        #[cfg(unix)]
        rx_counters: RxByteCounters::default(),
        #[cfg(unix)]
        server_rx_sample: HashMap::new(),
        #[cfg(unix)]
        last_rx_sample_at: Instant::now(),
        #[cfg(unix)]
        ping_nonce: 0,
        #[cfg(unix)]
        pending_pings: HashMap::new(),
        #[cfg(unix)]
        last_ping_at: Instant::now(),
        #[cfg(unix)]
        summary_subscription_server_ids: HashSet::new(),
        #[cfg(unix)]
        pending_summary_refresh_server_ids: HashSet::new(),
        #[cfg(unix)]
        queued_summary_refresh_server_ids: HashSet::new(),
        #[cfg(unix)]
        pending_secondary_connect_server_ids: HashSet::new(),
        #[cfg(unix)]
        pending_add_remote: false,
        #[cfg(unix)]
        ssh_bridges: HashMap::new(),
        #[cfg(unix)]
        secondary_retries: HashMap::new(),
        #[cfg(unix)]
        last_animation_tick: Instant::now(),
        #[cfg(unix)]
        last_summary_refresh: HashMap::new(),
    };
    debug!(?negotiated_encoding, "client render encoding active");
    let host_mouse_capture_active = Arc::new(AtomicBool::new(state.mouse_capture_active));

    // Channel for events from the stdin, resize, and server reader threads.
    let (event_tx, mut event_rx) = tokio::sync::mpsc::channel::<ClientLoopEvent>(256);

    // Spawn the stdin reader thread.
    let will_query_host_terminal_theme =
        state.attach_escape.is_none() && should_query_host_terminal_theme();
    let stdin_quit = should_quit.clone();
    let stdin_tx = event_tx.clone();
    let stdin_mouse_capture_active = host_mouse_capture_active.clone();
    std::thread::spawn(move || {
        input::stdin_reader_loop(
            stdin_tx,
            &stdin_quit,
            will_query_host_terminal_theme,
            stdin_mouse_capture_active,
        );
    });

    if will_query_host_terminal_theme {
        query_host_terminal_theme();
    }

    // Spawn the resize poller thread.
    let resize_quit = should_quit.clone();
    let resize_tx = event_tx.clone();
    let kitty_graphics_enabled = state.kitty_graphics_enabled;
    #[cfg(unix)]
    let (initial_cols, initial_rows) = state.host_size;
    #[cfg(windows)]
    let (initial_cols, initial_rows) = state.reported_size;
    std::thread::spawn(move || {
        resize_poll_loop(
            resize_tx,
            initial_cols,
            initial_rows,
            kitty_graphics_enabled,
            &resize_quit,
        );
    });

    // Spawn the server reader thread (blocking reads from the socket).
    // Clone the stream's file descriptor so we can read from a blocking stream.
    let server_read_quit = should_quit.clone();
    let server_read_tx = event_tx.clone();
    let read_stream = stream.try_clone().map_err(ClientError::ConnectionFailed)?;
    #[cfg(unix)]
    let main_rx_bytes = state.rx_counters.counter(&supervisor::ServerId::main());
    std::thread::spawn(move || {
        let max_frame_size = if kitty_graphics_enabled {
            MAX_GRAPHICS_FRAME_SIZE
        } else {
            MAX_FRAME_SIZE
        };
        #[cfg(unix)]
        server_reader_thread(
            supervisor::ServerId::main(),
            read_stream,
            main_rx_bytes,
            server_read_tx,
            &server_read_quit,
            max_frame_size,
        );
        #[cfg(windows)]
        server_reader_thread(
            read_stream,
            server_read_tx,
            &server_read_quit,
            max_frame_size,
        );
    });

    // Use the original stream for writing. On unix, writes go through per-server writer
    // threads so a slow/broken socket never stalls the async loop; Windows keeps the direct
    // blocking write from the loop.
    #[cfg(unix)]
    let mut server_writes: HashMap<supervisor::ServerId, ServerWriteHandle> = HashMap::new();
    #[cfg(unix)]
    {
        stream
            .set_nonblocking(false)
            .map_err(ClientError::ConnectionFailed)?;
        server_writes.insert(
            supervisor::ServerId::main(),
            spawn_server_writer(supervisor::ServerId::main(), stream, event_tx.clone()),
        );

        // Kick off the secondary client-stream connections (off the UI loop) and the summary
        // event subscriptions for every managed server.
        schedule_missing_secondary_stream_retries(&mut state, &server_writes, Instant::now());
        retry_due_secondary_connections(&mut state, Instant::now(), &event_tx, &mut server_writes);
        if let Some(model) = &state.supervisor_model {
            start_missing_supervisor_summary_subscriptions(
                model,
                &mut state.summary_subscription_server_ids,
                &state.ssh_bridges,
                &event_tx,
                &should_quit,
            );
        }
    }
    #[cfg(windows)]
    let mut write_stream = stream;
    #[cfg(windows)]
    write_stream
        .set_nonblocking(false)
        .map_err(ClientError::ConnectionFailed)?;

    // This (foreground) client owns the prefix ASCII input-source switch; a no-op on non-macOS.
    use crate::platform::PrefixInputSource;
    let mut prefix_input_source = crate::platform::RealPrefixInputSource::default();

    // Main event loop.
    while !should_quit.load(Ordering::Acquire) {
        // item 5: wake sooner (80ms) when the sidebar is animating, else keep the 100ms
        // housekeeping cadence (idle behavior unchanged). The gate reads the cached model only
        // and performs no I/O; real input still pre-empts the deadline via `event_rx.recv()`.
        #[cfg(unix)]
        let event = {
            let wants_animation = state.compositor.is_some()
                && state
                    .supervisor_model
                    .as_ref()
                    .is_some_and(compositor::sidebar_wants_animation);
            let deadline =
                next_select_deadline(Instant::now(), state.last_animation_tick, wants_animation);
            tokio::select! {
                ev = event_rx.recv() => ev.unwrap_or(ClientLoopEvent::Timer),
                _ = tokio::time::sleep_until(tokio::time::Instant::from_std(deadline)) => ClientLoopEvent::Timer,
            }
        };
        #[cfg(windows)]
        let event = tokio::select! {
            ev = event_rx.recv() => ev.unwrap_or(ClientLoopEvent::Timer),
            _ = tokio::time::sleep(Duration::from_millis(100)) => ClientLoopEvent::Timer,
        };

        match event {
            #[cfg(unix)]
            ClientLoopEvent::StdinInput(data) => {
                let data = if let Some(attach_escape) = &mut state.attach_escape {
                    match attach_escape.filter_input(
                        data,
                        state.reported_size.1,
                        state.mouse_scroll_lines,
                    ) {
                        AttachInputAction::Forward(data) => data,
                        AttachInputAction::Scroll {
                            source,
                            direction,
                            lines,
                            column,
                            row,
                            modifiers,
                        } => {
                            let msg = ClientMessage::AttachScroll {
                                source,
                                direction,
                                lines,
                                column,
                                row,
                                modifiers,
                            };
                            if let Err(e) = queue_to_server_id(
                                &server_writes,
                                &supervisor::ServerId::main(),
                                msg,
                            ) {
                                return Err(ClientError::ConnectionLost(e));
                            }
                            continue;
                        }
                        AttachInputAction::Detach => {
                            let _ = queue_to_server_id(
                                &server_writes,
                                &supervisor::ServerId::main(),
                                ClientMessage::Detach,
                            );
                            return Ok(());
                        }
                        AttachInputAction::None => continue,
                    }
                } else {
                    let events = crate::raw_input::parse_raw_input_bytes_sync(&data);
                    if crate::raw_input::events_require_host_surface_redraw(
                        &events,
                        state.redraw_on_focus_gained,
                    ) {
                        state.request_full_redraw();
                    }
                    if crate::raw_input::events_require_host_terminal_theme_query(&events) {
                        query_host_terminal_theme();
                    }
                    if let (Some(compositor), Some(model)) =
                        (&mut state.compositor, &mut state.supervisor_model)
                    {
                        match dispatch_composited_input(data, compositor, model, state.host_size) {
                            ClientInputDispatch::Forward(data) => data,
                            ClientInputDispatch::ServerControl { server_id, message } => {
                                if let Err(e) =
                                    queue_to_server_id(&server_writes, &server_id, message)
                                {
                                    handle_server_write_failure(
                                        &mut state,
                                        &mut server_writes,
                                        server_id,
                                        e,
                                        Instant::now(),
                                    )?;
                                }
                                state.request_full_redraw();
                                render_cached_composited_frame(&mut state);
                                continue;
                            }
                            ClientInputDispatch::ApiRequest {
                                server_id,
                                refresh,
                                request,
                            } => {
                                if let Err(err) = spawn_client_supervisor_request(
                                    model,
                                    server_id.clone(),
                                    refresh,
                                    *request,
                                    &state.ssh_bridges,
                                    &event_tx,
                                ) {
                                    warn!(
                                        server_id = ?server_id,
                                        err = %err,
                                        "failed to start client sidebar request"
                                    );
                                }
                                state.request_full_redraw();
                                render_cached_composited_frame(&mut state);
                                continue;
                            }
                            ClientInputDispatch::AddRemote(draft) => {
                                model.set_add_remote_in_progress();
                                spawn_client_add_remote_submission(
                                    draft,
                                    state.reported_size,
                                    state.cell_size_px,
                                    &event_tx,
                                    &mut state.pending_add_remote,
                                );
                                state.request_full_redraw();
                                render_cached_composited_frame(&mut state);
                                continue;
                            }
                            // item 3 (Area 5): the model already set `overlay.pending` for this
                            // remote when it emitted the outcome (blocking re-issue while in
                            // flight). Spawn the registry mutation off the UI loop against the
                            // local main socket; the `RemoteManageRequestFinished` handler applies
                            // the registry refresh + teardown/reconnect.
                            ClientInputDispatch::SetRemoteEnabled { remote_id, enabled } => {
                                spawn_client_remote_manage_request(
                                    model,
                                    RemoteManageAction::SetEnabled { enabled },
                                    remote_id,
                                    &state.ssh_bridges,
                                    &event_tx,
                                );
                                state.request_full_redraw();
                                render_cached_composited_frame(&mut state);
                                continue;
                            }
                            ClientInputDispatch::DeleteRemote { remote_id } => {
                                spawn_client_remote_manage_request(
                                    model,
                                    RemoteManageAction::Delete,
                                    remote_id,
                                    &state.ssh_bridges,
                                    &event_tx,
                                );
                                state.request_full_redraw();
                                render_cached_composited_frame(&mut state);
                                continue;
                            }
                            ClientInputDispatch::Resize { cols, rows } => {
                                state.reported_size = (cols, rows);
                                let msg = ClientMessage::Resize {
                                    cols,
                                    rows,
                                    cell_width_px: state.cell_size_px.0,
                                    cell_height_px: state.cell_size_px.1,
                                };
                                let mut write_failures = Vec::new();
                                for (server_id, handle) in server_writes.iter() {
                                    if let Err(e) = queue_to_server(handle, msg.clone()) {
                                        write_failures.push((server_id.clone(), e));
                                    }
                                }
                                for (server_id, error) in write_failures {
                                    handle_server_write_failure(
                                        &mut state,
                                        &mut server_writes,
                                        server_id,
                                        error,
                                        Instant::now(),
                                    )?;
                                }
                                state.request_full_redraw();
                                render_cached_composited_frame(&mut state);
                                continue;
                            }
                            ClientInputDispatch::DetachAll => {
                                let detach = ClientMessage::Detach;
                                for handle in server_writes.values() {
                                    let _ = queue_to_server(handle, detach.clone());
                                }
                                return Ok(());
                            }
                            ClientInputDispatch::Redraw => {
                                state.request_full_redraw();
                                render_cached_composited_frame(&mut state);
                                continue;
                            }
                            ClientInputDispatch::Consumed => continue,
                        }
                    } else {
                        data
                    }
                };
                if should_bridge_clipboard_image_paste(
                    &data,
                    is_remote_client,
                    state.remote_image_paste_key,
                ) {
                    if let Some(image) = crate::platform::read_clipboard_image() {
                        if image.bytes.len() > MAX_CLIPBOARD_IMAGE_PAYLOAD {
                            warn!(
                                bytes = image.bytes.len(),
                                max = MAX_CLIPBOARD_IMAGE_PAYLOAD,
                                "local clipboard image is too large to bridge"
                            );
                            continue;
                        }
                        info!(
                            bytes = image.bytes.len(),
                            extension = image.extension,
                            "bridging local clipboard image paste to remote server"
                        );
                        let msg = ClientMessage::ClipboardImage {
                            extension: image.extension.to_owned(),
                            data: image.bytes,
                        };
                        let server_id = active_server_id(&state);
                        if let Err(e) = queue_to_server_id(&server_writes, &server_id, msg) {
                            handle_server_write_failure(
                                &mut state,
                                &mut server_writes,
                                server_id,
                                e,
                                Instant::now(),
                            )?;
                            render_cached_composited_frame(&mut state);
                        }
                        continue;
                    }
                    info!(
                        "clipboard image paste trigger received, but local clipboard has no image"
                    );
                }
                if let Some(image) = read_image_file_from_terminal_drop(&data, is_remote_client) {
                    info!(
                        bytes = image.bytes.len(),
                        extension = image.extension,
                        "bridging local image file drop to remote server"
                    );
                    let msg = ClientMessage::ClipboardImage {
                        extension: image.extension.to_owned(),
                        data: image.bytes,
                    };
                    let server_id = active_server_id(&state);
                    if let Err(e) = queue_to_server_id(&server_writes, &server_id, msg) {
                        handle_server_write_failure(
                            &mut state,
                            &mut server_writes,
                            server_id,
                            e,
                            Instant::now(),
                        )?;
                        render_cached_composited_frame(&mut state);
                    }
                    continue;
                }
                let msg = ClientMessage::Input { data };
                let server_id = active_server_id(&state);
                if let Err(e) = queue_to_server_id(&server_writes, &server_id, msg) {
                    handle_server_write_failure(
                        &mut state,
                        &mut server_writes,
                        server_id,
                        e,
                        Instant::now(),
                    )?;
                    render_cached_composited_frame(&mut state);
                }
            }
            #[cfg(windows)]
            ClientLoopEvent::StdinEvents(events) => {
                if state.attach_escape.is_some() {
                    continue;
                }
                let raw_events = events
                    .iter()
                    .map(crate::protocol::ClientInputEvent::to_raw_input_event)
                    .collect::<Vec<_>>();
                if crate::raw_input::events_require_host_surface_redraw(
                    &raw_events,
                    state.redraw_on_focus_gained,
                ) {
                    state.request_full_redraw();
                }
                let msg = ClientMessage::InputEvents { events };
                if let Err(e) = write_to_server(&mut write_stream, &msg) {
                    return Err(ClientError::ConnectionLost(e));
                }
            }
            ClientLoopEvent::Resize(new_cols, new_rows, cell_width_px, cell_height_px) => {
                #[cfg(unix)]
                {
                    state.host_size = (new_cols, new_rows);
                    state.cell_size_px = (cell_width_px, cell_height_px);
                    state.reported_size = state
                        .compositor
                        .as_ref()
                        .map(|compositor| compositor.content_size(new_cols, new_rows))
                        .unwrap_or((new_cols, new_rows));
                    let msg = ClientMessage::Resize {
                        cols: state.reported_size.0,
                        rows: state.reported_size.1,
                        cell_width_px,
                        cell_height_px,
                    };
                    // Every connected server renders at the shared content size, so the resize
                    // fans out (single-server mode has exactly the main stream here).
                    let mut write_failures = Vec::new();
                    for (server_id, handle) in server_writes.iter() {
                        if let Err(e) = queue_to_server(handle, msg.clone()) {
                            write_failures.push((server_id.clone(), e));
                        }
                    }
                    for (server_id, error) in write_failures {
                        handle_server_write_failure(
                            &mut state,
                            &mut server_writes,
                            server_id,
                            error,
                            Instant::now(),
                        )?;
                    }
                }
                #[cfg(windows)]
                {
                    state.reported_size = (new_cols, new_rows);
                    let msg = ClientMessage::Resize {
                        cols: new_cols,
                        rows: new_rows,
                        cell_width_px,
                        cell_height_px,
                    };
                    if let Err(e) = write_to_server(&mut write_stream, &msg) {
                        return Err(ClientError::ConnectionLost(e));
                    }
                }
            }
            #[cfg(unix)]
            ClientLoopEvent::ServerMessage { server_id, message } => match message {
                ServerMessage::Frame(frame_data) => {
                    render_incoming_server_frame(&mut state, &server_id, frame_data);
                }
                ServerMessage::FrameDelta(delta) => {
                    // Reconstruct the full frame from this server's cached baseline. On a
                    // baseline mismatch (e.g. just after a local resize) skip the delta; the
                    // server re-baselines with a full frame on any dimension change.
                    match state
                        .frame_cache
                        .get(&server_id)
                        .and_then(|prev| prev.with_delta(&delta))
                    {
                        Some(full) => render_incoming_server_frame(&mut state, &server_id, full),
                        None => {
                            debug!(
                                server_id = ?server_id,
                                "dropping frame delta without matching baseline"
                            );
                        }
                    }
                }
                ServerMessage::Pong { nonce } => {
                    // issue #13: true round-trip latency over the persistent stream (no per-ping
                    // connection/process-spawn overhead). Only the matching outstanding nonce
                    // counts; single-server clients never probe, so this stays inert there.
                    if let Some((pending_nonce, sent_at)) = state.pending_pings.remove(&server_id) {
                        if pending_nonce == nonce {
                            if let Some(model) = &mut state.supervisor_model {
                                let rtt =
                                    sent_at.elapsed().as_millis().min(u32::MAX as u128) as u32;
                                model.record_server_ping(&server_id, rtt);
                                state.request_full_redraw();
                            }
                        }
                    }
                }
                ServerMessage::Compressed(_) => {
                    // The reader thread inflates compressed payloads; reaching this
                    // arm means the payload failed to inflate.
                    warn!(server_id = ?server_id, "dropping server message that failed to inflate");
                }
                ServerMessage::Terminal(frame) => {
                    if server_id != active_server_id(&state) {
                        continue;
                    }
                    if state.kitty_graphics_enabled && contains_kitty_graphics_bytes(&frame.bytes) {
                        record_received_kitty_graphics(&frame.bytes);
                    }
                    let mut stdout = io::stdout();
                    let _ = stdout.write_all(&frame.bytes);
                    let _ = stdout.flush();
                }
                ServerMessage::Graphics { bytes } => {
                    if server_id != active_server_id(&state) {
                        continue;
                    }
                    if state.kitty_graphics_enabled {
                        record_received_kitty_graphics(&bytes);
                        let mut stdout = io::stdout();
                        let _ = stdout.write_all(&bytes);
                        let _ = stdout.flush();
                    }
                }
                ServerMessage::ServerShutdown { reason } => {
                    if server_id != supervisor::ServerId::main() {
                        // A stopped secondary must not kill the whole client: tear its stream
                        // state down, mark it disconnected, and let the retry machinery bring it
                        // back if it reappears.
                        teardown_secondary_connection(&mut state, &mut server_writes, &server_id);
                        if let Some(model) = &mut state.supervisor_model {
                            let _ = model.set_connection_state(
                                &server_id,
                                supervisor::ConnectionState::Disconnected,
                            );
                            state.request_full_redraw();
                        }
                        schedule_secondary_retry(&mut state, server_id, 0, Instant::now());
                        render_cached_composited_frame(&mut state);
                        continue;
                    }
                    return Err(ClientError::ServerShutdown { reason });
                }
                ServerMessage::Notify {
                    kind,
                    message,
                    body,
                } => {
                    handle_notify(kind, &message, body.as_deref(), &state.sound_config);
                }
                ServerMessage::Clipboard { data } => {
                    forward_clipboard(&data);
                    let _ = io::stdout().flush();
                }
                ServerMessage::WindowTitle { title } => {
                    if server_id != active_server_id(&state) {
                        continue;
                    }
                    write_window_title(title.as_deref());
                    let _ = io::stdout().flush();
                }
                ServerMessage::ReloadSoundConfig => {
                    reload_local_client_config(
                        &mut state.sound_config,
                        &mut state.redraw_on_focus_gained,
                        &mut state.draw_host_cursor,
                        &mut state.remote_image_paste_key,
                    );
                    // The composited sidebar's theme is client-resolved: pick up
                    // a changed [theme] on the same reload signal.
                    if let Some(compositor) = state.compositor.as_mut() {
                        if let Ok(loaded) = crate::config::load_live_config() {
                            compositor.set_palette(crate::app::client_palette_from_config(
                                &loaded.config,
                            ));
                        }
                    }
                }
                ServerMessage::MouseCapture { enabled } => {
                    if server_id != active_server_id(&state) {
                        continue;
                    }
                    // The client-owned sidebar always needs mouse capture; the server's request
                    // only wins in single-server mode.
                    let desired = desired_mouse_capture(enabled, state.compositor.is_some());
                    if desired != state.mouse_capture_active {
                        set_mouse_capture(desired).map_err(ClientError::ConnectionFailed)?;
                        state.mouse_capture_active = desired;
                        host_mouse_capture_active.store(desired, Ordering::Release);
                    }
                }
                ServerMessage::PrefixInputSource { active } => {
                    if server_id != active_server_id(&state) {
                        continue;
                    }
                    if active {
                        prefix_input_source.switch_to_ascii();
                    } else {
                        prefix_input_source.restore();
                    }
                }
                ServerMessage::Welcome { .. } => {
                    debug!("received unexpected Welcome in main loop");
                }
            },
            #[cfg(windows)]
            ClientLoopEvent::ServerMessage(msg) => match msg {
                ServerMessage::Frame(frame_data) => {
                    state.server_frame_baseline = Some(frame_data.clone());
                    draw_semantic_frame(&mut state, frame_data);
                }
                ServerMessage::FrameDelta(delta) => {
                    // Reconstruct the full frame from the cached baseline. On a
                    // baseline mismatch (e.g. just after a local resize) skip the
                    // delta; the server re-baselines with a full frame on any
                    // dimension change.
                    let Some(frame_data) = state
                        .server_frame_baseline
                        .as_ref()
                        .and_then(|prev| prev.with_delta(&delta))
                    else {
                        debug!("dropping frame delta without matching baseline");
                        continue;
                    };
                    state.server_frame_baseline = Some(frame_data.clone());
                    draw_semantic_frame(&mut state, frame_data);
                }
                ServerMessage::Pong { .. } => {
                    // Single-server clients do not probe latency; ignore.
                }
                ServerMessage::Compressed(_) => {
                    // The reader thread inflates compressed payloads; reaching this
                    // arm means the payload failed to inflate.
                    warn!("dropping server message that failed to inflate");
                }
                ServerMessage::Terminal(frame) => {
                    if state.kitty_graphics_enabled && contains_kitty_graphics_bytes(&frame.bytes) {
                        record_received_kitty_graphics(&frame.bytes);
                    }
                    let mut stdout = io::stdout();
                    let _ = stdout.write_all(&frame.bytes);
                    let _ = stdout.flush();
                }
                ServerMessage::Graphics { bytes } => {
                    if state.kitty_graphics_enabled {
                        record_received_kitty_graphics(&bytes);
                        let mut stdout = io::stdout();
                        let _ = stdout.write_all(&bytes);
                        let _ = stdout.flush();
                    }
                }
                ServerMessage::ServerShutdown { reason } => {
                    return Err(ClientError::ServerShutdown { reason });
                }
                ServerMessage::Notify {
                    kind,
                    message,
                    body,
                } => {
                    handle_notify(kind, &message, body.as_deref(), &state.sound_config);
                }
                ServerMessage::Clipboard { data } => {
                    forward_clipboard(&data);
                    let _ = io::stdout().flush();
                }
                ServerMessage::WindowTitle { title } => {
                    write_window_title(title.as_deref());
                    let _ = io::stdout().flush();
                }
                ServerMessage::ReloadSoundConfig => {
                    reload_local_client_config(
                        &mut state.sound_config,
                        &mut state.redraw_on_focus_gained,
                        &mut state.draw_host_cursor,
                    );
                }
                ServerMessage::MouseCapture { enabled } => {
                    let desired = enabled;
                    if desired != state.mouse_capture_active {
                        set_mouse_capture(desired).map_err(ClientError::ConnectionFailed)?;
                        if windows_vti_input_backend_enabled() {
                            let _ = enable_windows_virtual_terminal_input();
                        }
                        state.mouse_capture_active = desired;
                        host_mouse_capture_active.store(desired, Ordering::Release);
                    }
                }
                ServerMessage::PrefixInputSource { active } => {
                    if active {
                        prefix_input_source.switch_to_ascii();
                    } else {
                        prefix_input_source.restore();
                    }
                }
                ServerMessage::Welcome { .. } => {
                    debug!("received unexpected Welcome in main loop");
                }
            },
            #[cfg(unix)]
            ClientLoopEvent::SupervisorSummaryChanged(server_id) => {
                debug!(
                    server_id = ?server_id,
                    "supervisor summary event requested refresh"
                );
                // item 6 (Area 6): targeted event-push — refresh ONLY the changed server, not the
                // whole fleet. Both main and secondary ids spawn a single off-loop fetch; the
                // repaint happens when the fetched result arrives (no data changed yet here, and
                // the main fetch is a WAN round-trip bundle under `herdr --remote`).
                let now = Instant::now();
                if let Some(model) = &state.supervisor_model {
                    if server_id == supervisor::ServerId::main() {
                        start_main_supervisor_refresh(
                            &mut state.pending_summary_refresh_server_ids,
                            &mut state.queued_summary_refresh_server_ids,
                            &event_tx,
                        );
                    } else {
                        start_single_secondary_summary_refresh(
                            model,
                            &server_id,
                            &state.ssh_bridges,
                            &mut state.pending_summary_refresh_server_ids,
                            &mut state.queued_summary_refresh_server_ids,
                            &event_tx,
                        );
                    }
                    state.last_summary_refresh.insert(server_id.clone(), now);
                }
                schedule_missing_secondary_stream_retries(
                    &mut state,
                    &server_writes,
                    Instant::now(),
                );
                if let Some(model) = &state.supervisor_model {
                    start_missing_supervisor_summary_subscriptions(
                        model,
                        &mut state.summary_subscription_server_ids,
                        &state.ssh_bridges,
                        &event_tx,
                        &should_quit,
                    );
                }
            }
            #[cfg(unix)]
            ClientLoopEvent::SupervisorSummaryFetched {
                server_id,
                result,
                elapsed,
            } => {
                state.pending_summary_refresh_server_ids.remove(&server_id);
                // A change event arrived while this fetch was in flight: the fetch
                // may have read pre-change state, so rerun immediately instead of
                // waiting for the next poll tick.
                if state.queued_summary_refresh_server_ids.remove(&server_id) {
                    if let Some(model) = &state.supervisor_model {
                        start_single_secondary_summary_refresh(
                            model,
                            &server_id,
                            &state.ssh_bridges,
                            &mut state.pending_summary_refresh_server_ids,
                            &mut state.queued_summary_refresh_server_ids,
                            &event_tx,
                        );
                    }
                }
                // item 6 (Area 6): track the last successful poll for both the fast (active) and
                // slow (background) cadence classes so `due_secondary_summary_refreshes` measures
                // from the latest completion too (not only from the start recorded by the Timer).
                state
                    .last_summary_refresh
                    .insert(server_id.clone(), Instant::now());
                if elapsed > CLIENT_60FPS_FRAME_BUDGET {
                    debug!(
                        server_id = ?server_id,
                        elapsed_ms = elapsed.as_secs_f64() * 1000.0,
                        frame_budget_fps = fps_for_frame_duration(CLIENT_60FPS_FRAME_BUDGET),
                        "secondary supervisor summary completed off UI thread"
                    );
                }
                if let Some(model) = &mut state.supervisor_model {
                    model.apply_secondary_summary_results([(server_id.clone(), result)]);
                    state.request_full_redraw();
                }
                schedule_missing_secondary_stream_retries(
                    &mut state,
                    &server_writes,
                    Instant::now(),
                );
                if let Some(model) = &state.supervisor_model {
                    start_missing_supervisor_summary_subscriptions(
                        model,
                        &mut state.summary_subscription_server_ids,
                        &state.ssh_bridges,
                        &event_tx,
                        &should_quit,
                    );
                }
                render_cached_composited_frame(&mut state);
            }
            #[cfg(unix)]
            ClientLoopEvent::MainSupervisorRefreshFinished { snapshot, elapsed } => {
                let main_id = supervisor::ServerId::main();
                state.pending_summary_refresh_server_ids.remove(&main_id);
                // A change signal arrived while this fetch was in flight: the fetch may have
                // read pre-change state, so rerun immediately (same coalescing the secondary
                // path uses — at most one fetch in flight plus one queued rerun).
                if state.queued_summary_refresh_server_ids.remove(&main_id) {
                    start_main_supervisor_refresh(
                        &mut state.pending_summary_refresh_server_ids,
                        &mut state.queued_summary_refresh_server_ids,
                        &event_tx,
                    );
                }
                state.last_summary_refresh.insert(main_id, Instant::now());
                if elapsed > CLIENT_60FPS_FRAME_BUDGET {
                    debug!(
                        elapsed_ms = elapsed.as_secs_f64() * 1000.0,
                        frame_budget_fps = fps_for_frame_duration(CLIENT_60FPS_FRAME_BUDGET),
                        "main supervisor refresh completed off UI thread"
                    );
                }
                if let Some(model) = &mut state.supervisor_model {
                    model.apply_main_supervisor_snapshot(*snapshot);
                }
                schedule_missing_secondary_stream_retries(
                    &mut state,
                    &server_writes,
                    Instant::now(),
                );
                if let Some(model) = &state.supervisor_model {
                    start_missing_supervisor_summary_subscriptions(
                        model,
                        &mut state.summary_subscription_server_ids,
                        &state.ssh_bridges,
                        &event_tx,
                        &should_quit,
                    );
                }
                render_cached_composited_frame(&mut state);
            }
            #[cfg(unix)]
            ClientLoopEvent::SupervisorSummarySubscriptionEnded(server_id) => {
                state.summary_subscription_server_ids.remove(&server_id);
            }
            #[cfg(unix)]
            ClientLoopEvent::SupervisorApiRequestFinished {
                server_id,
                refresh,
                result,
                elapsed,
            } => {
                if elapsed > CLIENT_60FPS_FRAME_BUDGET {
                    debug!(
                        server_id = ?server_id,
                        elapsed_ms = elapsed.as_secs_f64() * 1000.0,
                        frame_budget_fps = fps_for_frame_duration(CLIENT_60FPS_FRAME_BUDGET),
                        "client sidebar API request completed off UI thread"
                    );
                }
                match result {
                    Ok(response) => {
                        // Optimistic echo: a created workspace is already fully
                        // described in the response — merge it into the sidebar
                        // NOW instead of waiting for the summary round-trip over
                        // the (possibly ssh-bridged) API. The refresh below still
                        // runs and reconciles authoritative state.
                        if let crate::api::schema::ResponseResult::WorkspaceCreated {
                            workspace,
                            ..
                        } = response.result
                        {
                            if let Some(model) = &mut state.supervisor_model {
                                model.apply_created_workspace(&server_id, workspace);
                                state.request_full_redraw();
                                render_cached_composited_frame(&mut state);
                            }
                        }
                        if refresh == ClientApiRefreshPolicy::Immediate {
                            let now = Instant::now();
                            if let Some(model) = &mut state.supervisor_model {
                                refresh_client_supervisor_summaries(
                                    model,
                                    &state.ssh_bridges,
                                    &mut state.pending_summary_refresh_server_ids,
                                    &mut state.queued_summary_refresh_server_ids,
                                    &event_tx,
                                );
                                state.last_supervisor_summary_refresh = now;
                                state.request_full_redraw();
                            }
                            schedule_missing_secondary_stream_retries(
                                &mut state,
                                &server_writes,
                                now,
                            );
                            if let Some(model) = &state.supervisor_model {
                                start_missing_supervisor_summary_subscriptions(
                                    model,
                                    &mut state.summary_subscription_server_ids,
                                    &state.ssh_bridges,
                                    &event_tx,
                                    &should_quit,
                                );
                            }
                        } else if refresh == ClientApiRefreshPolicy::ImmediateFocused {
                            // item 6 (Area 6): targeted single-server fetch for the focused server
                            // ONLY (not the whole fleet). Both main and secondary fetch OFF the UI
                            // loop — a focused main workspace produces server_id == main, and under
                            // `herdr --remote` the main api socket is an ssh bridge (WAN RTTs).
                            let now = Instant::now();
                            if let Some(model) = &state.supervisor_model {
                                if server_id == supervisor::ServerId::main() {
                                    start_main_supervisor_refresh(
                                        &mut state.pending_summary_refresh_server_ids,
                                        &mut state.queued_summary_refresh_server_ids,
                                        &event_tx,
                                    );
                                } else {
                                    start_single_secondary_summary_refresh(
                                        model,
                                        &server_id,
                                        &state.ssh_bridges,
                                        &mut state.pending_summary_refresh_server_ids,
                                        &mut state.queued_summary_refresh_server_ids,
                                        &event_tx,
                                    );
                                }
                                state.request_full_redraw();
                            }
                            state.last_summary_refresh.insert(server_id.clone(), now);
                        }
                    }
                    Err(err) => {
                        warn!(
                            server_id = ?server_id,
                            err = %err,
                            "failed to route client sidebar request"
                        );
                        // item 6 (Area 6): reconcile the optimistic highlight back to summary
                        // truth on the next refresh when the focus request itself failed.
                        if let Some(model) = &mut state.supervisor_model {
                            model.clear_optimistic_focus_on_failure(&server_id);
                        }
                    }
                }
                state.request_full_redraw();
                render_cached_composited_frame(&mut state);
            }
            #[cfg(unix)]
            ClientLoopEvent::SecondaryConnectionAttemptFinished {
                server_id,
                attempt,
                result,
                elapsed,
            } => {
                state
                    .pending_secondary_connect_server_ids
                    .remove(&server_id);
                if elapsed > CLIENT_60FPS_FRAME_BUDGET {
                    debug!(
                        server_id = ?server_id,
                        elapsed_ms = elapsed.as_secs_f64() * 1000.0,
                        frame_budget_fps = fps_for_frame_duration(CLIENT_60FPS_FRAME_BUDGET),
                        "secondary client connection attempt completed off UI thread"
                    );
                }
                match result {
                    Ok(connection) => {
                        if let Some(bridge) = connection.bridge {
                            state.ssh_bridges.insert(server_id.clone(), bridge);
                        }
                        let rx_bytes = state.rx_counters.counter(&server_id);
                        if let Err(err) = attach_secondary_client_stream(
                            server_id.clone(),
                            connection.stream,
                            rx_bytes,
                            &event_tx,
                            &should_quit,
                            &mut server_writes,
                        ) {
                            let next_attempt = attempt.saturating_add(1);
                            schedule_secondary_retry(
                                &mut state,
                                server_id.clone(),
                                next_attempt,
                                Instant::now(),
                            );
                            if let Some(model) = &mut state.supervisor_model {
                                let _ = model.set_connection_state(
                                    &server_id,
                                    connection_state_from_client_error(&err),
                                );
                            }
                            warn!(
                                server_id = ?server_id,
                                err = %err,
                                "failed to attach retried secondary client stream"
                            );
                            state.request_full_redraw();
                            render_cached_composited_frame(&mut state);
                            continue;
                        }

                        state.secondary_retries.remove(&server_id);
                        let now = Instant::now();
                        if let Some(model) = &mut state.supervisor_model {
                            let _ = model.set_connection_state(
                                &server_id,
                                supervisor::ConnectionState::Connected,
                            );
                            // item 6 (Area 6): prioritize the just-connected server. Neither
                            // `set_connection_state(.., Connected)` nor anything here sets
                            // `active_server_id`, so key off the handler's explicit `server_id`
                            // (NOT `active_server_id()`). Its summary is put in flight FIRST; the
                            // dedupe guard then prevents the whole-fleet fan-out below from
                            // double-spawning it.
                            start_single_secondary_summary_refresh(
                                model,
                                &server_id,
                                &state.ssh_bridges,
                                &mut state.pending_summary_refresh_server_ids,
                                &mut state.queued_summary_refresh_server_ids,
                                &event_tx,
                            );
                            state.last_summary_refresh.insert(server_id.clone(), now);
                            refresh_client_supervisor_summaries(
                                model,
                                &state.ssh_bridges,
                                &mut state.pending_summary_refresh_server_ids,
                                &mut state.queued_summary_refresh_server_ids,
                                &event_tx,
                            );
                            start_missing_supervisor_summary_subscriptions(
                                model,
                                &mut state.summary_subscription_server_ids,
                                &state.ssh_bridges,
                                &event_tx,
                                &should_quit,
                            );
                            state.last_supervisor_summary_refresh = now;
                        }
                    }
                    Err(err) => {
                        let connection_state = connection_state_from_client_error(&err);
                        if matches!(
                            connection_state,
                            supervisor::ConnectionState::ProtocolMismatch { .. }
                        ) {
                            state.secondary_retries.remove(&server_id);
                        } else {
                            let next_attempt = attempt.saturating_add(1);
                            schedule_secondary_retry(
                                &mut state,
                                server_id.clone(),
                                next_attempt,
                                Instant::now(),
                            );
                        }
                        state.ssh_bridges.remove(&server_id);
                        if let Some(model) = &mut state.supervisor_model {
                            let _ = model.set_connection_state(&server_id, connection_state);
                        }
                        warn!(
                            server_id = ?server_id,
                            err = %err,
                            "failed to retry secondary client connection"
                        );
                    }
                }
                state.request_full_redraw();
                render_cached_composited_frame(&mut state);
            }
            #[cfg(unix)]
            ClientLoopEvent::AddRemoteFinished { result, elapsed } => {
                state.pending_add_remote = false;
                if elapsed > CLIENT_60FPS_FRAME_BUDGET {
                    debug!(
                        elapsed_ms = elapsed.as_secs_f64() * 1000.0,
                        frame_budget_fps = fps_for_frame_duration(CLIENT_60FPS_FRAME_BUDGET),
                        "client add-remote submission completed off UI thread"
                    );
                }
                match result {
                    Ok(success) => {
                        // Clone the (Arc-backed) counters registry up front so we can resolve the
                        // new server's byte counter without re-borrowing `state` inside the model borrow.
                        let rx_counters = state.rx_counters.clone();
                        if let Some(model) = &mut state.supervisor_model {
                            let server_id = model.add_secondary(success.remote);
                            if let Some(bridge) = success.bridge {
                                state.ssh_bridges.insert(server_id.clone(), bridge);
                            }
                            let rx_bytes = rx_counters.counter(&server_id);
                            match attach_secondary_client_stream(
                                server_id.clone(),
                                success.stream,
                                rx_bytes,
                                &event_tx,
                                &should_quit,
                                &mut server_writes,
                            ) {
                                Ok(()) => {
                                    let _ = model.set_connection_state(
                                        &server_id,
                                        supervisor::ConnectionState::Connected,
                                    );
                                    model.finish_add_remote();
                                    let now = Instant::now();
                                    // item 6 (Area 6): prioritize the just-added server's summary
                                    // by the handler's explicit `server_id` (adding a remote does
                                    // not set `active_server_id`). Put it in flight FIRST; the
                                    // dedupe guard collapses the follow-on fan-out for this id.
                                    start_single_secondary_summary_refresh(
                                        model,
                                        &server_id,
                                        &state.ssh_bridges,
                                        &mut state.pending_summary_refresh_server_ids,
                                        &mut state.queued_summary_refresh_server_ids,
                                        &event_tx,
                                    );
                                    state.last_summary_refresh.insert(server_id.clone(), now);
                                    refresh_client_supervisor_summaries(
                                        model,
                                        &state.ssh_bridges,
                                        &mut state.pending_summary_refresh_server_ids,
                                        &mut state.queued_summary_refresh_server_ids,
                                        &event_tx,
                                    );
                                    start_missing_supervisor_summary_subscriptions(
                                        model,
                                        &mut state.summary_subscription_server_ids,
                                        &state.ssh_bridges,
                                        &event_tx,
                                        &should_quit,
                                    );
                                    state.last_supervisor_summary_refresh = now;
                                }
                                Err(err) => {
                                    let _ = model.set_connection_state(
                                        &server_id,
                                        connection_state_from_client_error(&err),
                                    );
                                    model.set_add_remote_error(err.to_string());
                                }
                            }
                        }
                    }
                    Err(AddRemoteFailure::Message(err)) => {
                        warn!(err = %err, "failed to add client remote");
                        if let Some(model) = &mut state.supervisor_model {
                            model.set_add_remote_error(err);
                        }
                    }
                    Err(AddRemoteFailure::NeedsRestartConfirm {
                        destination,
                        detail,
                    }) => {
                        warn!(
                            destination = %destination,
                            "add-remote blocked on an incompatible no-handoff server; prompting to restart"
                        );
                        if let Some(model) = &mut state.supervisor_model {
                            model.set_add_remote_restart_confirm(destination, detail);
                        }
                    }
                }
                state.request_full_redraw();
                render_cached_composited_frame(&mut state);
            }
            #[cfg(unix)]
            ClientLoopEvent::RemoteManageRequestFinished {
                action,
                remote_id,
                result,
                elapsed,
            } => {
                if elapsed > CLIENT_60FPS_FRAME_BUDGET {
                    debug!(
                        elapsed_ms = elapsed.as_secs_f64() * 1000.0,
                        frame_budget_fps = fps_for_frame_duration(CLIENT_60FPS_FRAME_BUDGET),
                        "client remote-manage request completed off UI thread"
                    );
                }
                apply_remote_manage_request_finished(
                    &mut state,
                    &mut server_writes,
                    action,
                    &remote_id,
                    result,
                    &event_tx,
                );
                state.request_full_redraw();
                render_cached_composited_frame(&mut state);
            }
            #[cfg(unix)]
            ClientLoopEvent::ServerDisconnected(server_id) => {
                if server_id == supervisor::ServerId::main() {
                    return Err(ClientError::ConnectionLost(io::Error::new(
                        io::ErrorKind::UnexpectedEof,
                        "server closed connection",
                    )));
                }
                teardown_secondary_connection(&mut state, &mut server_writes, &server_id);
                let mut reconnect_candidate = false;
                if let Some(model) = &mut state.supervisor_model {
                    let _ = model.set_connection_state(
                        &server_id,
                        supervisor::ConnectionState::Disconnected,
                    );
                    reconnect_candidate = model.is_reconnect_candidate(&server_id);
                    state.request_full_redraw();
                }
                // A disconnect for a server the user just removed or disabled is
                // the EXPECTED result of tearing its bridges down — scheduling a
                // retry here would resurrect them.
                if reconnect_candidate {
                    schedule_secondary_retry(&mut state, server_id, 0, Instant::now());
                }
                render_cached_composited_frame(&mut state);
            }
            #[cfg(windows)]
            ClientLoopEvent::ServerDisconnected => {
                return Err(ClientError::ConnectionLost(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "server closed connection",
                )));
            }
            ClientLoopEvent::Timer => {
                // Multi-server housekeeping: latency probes, throughput sampling, reconnect
                // backoff, the adaptive summary cadence, and the gated animation step. All of it
                // is inert in single-server mode (no compositor), preserving today's idle loop.
                #[cfg(unix)]
                if state.compositor.is_some() {
                    let now = Instant::now();
                    sample_download_rates(&mut state, now);
                    // issue #13: probe each connected server's latency over its persistent stream.
                    if now.duration_since(state.last_ping_at) >= SERVER_PING_INTERVAL {
                        state.last_ping_at = now;
                        for (server_id, handle) in &server_writes {
                            state.ping_nonce = state.ping_nonce.wrapping_add(1);
                            let nonce = state.ping_nonce;
                            if queue_to_server(handle, ClientMessage::Ping { nonce }).is_ok() {
                                state.pending_pings.insert(server_id.clone(), (nonce, now));
                            }
                        }
                    }
                    retry_due_secondary_connections(&mut state, now, &event_tx, &mut server_writes);

                    // item 6 (Area 6): adaptive secondary cadence (400ms active / 2s background).
                    // Each due secondary fetch goes through the spawn helper (off the UI loop); we
                    // record `last_summary_refresh[id]` on START so a slow SSH fetch does not stack
                    // (the `pending_summary_refresh_server_ids` guard also prevents duplicate
                    // workers). The Timer body issues NO inline blocking secondary API call.
                    let due = due_secondary_summary_refreshes(&state, now);
                    if !due.is_empty() {
                        if let Some(model) = &state.supervisor_model {
                            for server_id in &due {
                                start_single_secondary_summary_refresh(
                                    model,
                                    server_id,
                                    &state.ssh_bridges,
                                    &mut state.pending_summary_refresh_server_ids,
                                    &mut state.queued_summary_refresh_server_ids,
                                    &event_tx,
                                );
                                state.last_summary_refresh.insert(server_id.clone(), now);
                            }
                        }
                    }

                    let mut did_local_refresh = false;
                    if supervisor_summary_refresh_due(now, state.last_supervisor_summary_refresh) {
                        // The 2s gate drives ONLY the main registry/ui-settings/summary refresh,
                        // fetched OFF the UI loop (under `herdr --remote` the main api socket is
                        // an ssh bridge, so this bundle is WAN round-trips). The secondary fan-out
                        // is OMITTED — the per-secondary `due` loop above is the single source of
                        // secondary cadence.
                        if state.supervisor_model.is_some() {
                            start_main_supervisor_refresh(
                                &mut state.pending_summary_refresh_server_ids,
                                &mut state.queued_summary_refresh_server_ids,
                                &event_tx,
                            );
                            state.last_supervisor_summary_refresh = now;
                            state.request_full_redraw();
                        }
                        schedule_missing_secondary_stream_retries(&mut state, &server_writes, now);
                        if let Some(model) = &state.supervisor_model {
                            start_missing_supervisor_summary_subscriptions(
                                model,
                                &mut state.summary_subscription_server_ids,
                                &state.ssh_bridges,
                                &event_tx,
                                &should_quit,
                            );
                        }
                        did_local_refresh = true;
                    }
                    if !due.is_empty() || did_local_refresh {
                        render_cached_composited_frame(&mut state);
                    }

                    // item 5: gated, fully-local animation step. Advances the single client
                    // animation tick at the 80ms cadence and recomposes via the blit diff (NOT a
                    // full redraw). It calls ONLY advance_animation_tick +
                    // render_cached_composited_frame — never any SSH/API I/O. When nothing is
                    // animating, `wants` is false and the tick never advances (zero idle recompose).
                    let wants = state
                        .supervisor_model
                        .as_ref()
                        .is_some_and(compositor::sidebar_wants_animation);
                    if should_advance_animation(wants, now, state.last_animation_tick) {
                        if let Some(compositor) = state.compositor.as_mut() {
                            compositor.advance_animation_tick(CLIENT_ANIMATION_TICK_STEP);
                        }
                        state.last_animation_tick = now;
                        render_cached_composited_frame(&mut state);
                    }
                }
            }
        }
    }

    // Clean exit (Ctrl+C). Send Detach before closing.
    let detach = ClientMessage::Detach;
    #[cfg(unix)]
    for handle in server_writes.values() {
        let _ = queue_to_server(handle, detach.clone());
    }
    #[cfg(windows)]
    let _ = write_to_server(&mut write_stream, &detach);
    let _ = io::stdout().flush();

    Ok(())
}

// ---------------------------------------------------------------------------
// Per-server downstream byte counters (issue #13 host-banner throughput)
// ---------------------------------------------------------------------------

/// Shared cumulative downstream byte counters keyed by server, fed by the reader threads and
/// sampled on the UI loop to derive a bytes/sec rate for the host banner.
#[cfg(unix)]
#[derive(Clone, Default)]
struct RxByteCounters(
    Arc<std::sync::Mutex<HashMap<supervisor::ServerId, Arc<std::sync::atomic::AtomicU64>>>>,
);

#[cfg(unix)]
impl RxByteCounters {
    /// The (get-or-create) cumulative byte counter for one server.
    fn counter(&self, server_id: &supervisor::ServerId) -> Arc<std::sync::atomic::AtomicU64> {
        self.0
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .entry(server_id.clone())
            .or_default()
            .clone()
    }

    /// Snapshot of cumulative bytes received per server.
    fn snapshot(&self) -> Vec<(supervisor::ServerId, u64)> {
        self.0
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .iter()
            .map(|(id, bytes)| (id.clone(), bytes.load(std::sync::atomic::Ordering::Relaxed)))
            .collect()
    }
}

/// A `Read` wrapper that tallies bytes into a shared counter, so the reader thread can report
/// downstream throughput without changing the framing/protocol layer.
#[cfg(unix)]
struct CountingReader<R> {
    inner: R,
    counter: Arc<std::sync::atomic::AtomicU64>,
}

#[cfg(unix)]
impl<R: std::io::Read> std::io::Read for CountingReader<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let n = self.inner.read(buf)?;
        self.counter
            .fetch_add(n as u64, std::sync::atomic::Ordering::Relaxed);
        Ok(n)
    }
}

// ---------------------------------------------------------------------------
// Server reader thread
// ---------------------------------------------------------------------------

/// Blocking thread that reads ServerMessages from one server and sends them to the main event
/// loop tagged with the owning server id, tallying downstream bytes for the host banner.
#[cfg(unix)]
fn server_reader_thread(
    server_id: supervisor::ServerId,
    stream: LocalStream,
    rx_bytes: Arc<std::sync::atomic::AtomicU64>,
    event_tx: tokio::sync::mpsc::Sender<ClientLoopEvent>,
    should_quit: &Arc<AtomicBool>,
    max_frame_size: usize,
) {
    // Ensure the read stream is in blocking mode to avoid WouldBlock errors
    // from read_exact inside read_message. The stream should already be
    // blocking after handshake, but we enforce it here as a safety measure.
    if stream.set_nonblocking(false).is_err() {
        // If we can't set blocking mode, the stream is likely broken.
        let _ = event_tx.blocking_send(ClientLoopEvent::ServerDisconnected(server_id));
        return;
    }
    // Tally every downstream byte (issue #13) without touching the framing layer.
    let mut stream = CountingReader {
        inner: stream,
        counter: rx_bytes,
    };

    loop {
        if should_quit.load(Ordering::Acquire) {
            break;
        }

        match protocol::read_message(&mut stream, max_frame_size) {
            Ok(msg) => {
                // Inflate compressed payloads on the reader thread so the main
                // loop only ever sees plain messages.
                let msg = protocol::decompress_server_message(msg);
                if event_tx
                    .blocking_send(ClientLoopEvent::ServerMessage {
                        server_id: server_id.clone(),
                        message: msg,
                    })
                    .is_err()
                {
                    break; // Main loop gone.
                }
            }
            Err(protocol::FramingError::UnexpectedEof) => {
                // Server closed connection.
                let _ = event_tx.blocking_send(ClientLoopEvent::ServerDisconnected(server_id));
                break;
            }
            Err(protocol::FramingError::Io(err)) if err.kind() == io::ErrorKind::WouldBlock => {
                // Should not happen with blocking mode, but handle gracefully
                // in case the stream was set nonblocking by another clone.
                std::thread::sleep(Duration::from_millis(1));
                continue;
            }
            Err(err) => {
                warn!(err = %err, "server read error");
                let _ = event_tx.blocking_send(ClientLoopEvent::ServerDisconnected(server_id));
                break;
            }
        }
    }
}

/// Blocking thread that reads ServerMessages from the server and sends them
/// to the main event loop.
#[cfg(windows)]
fn server_reader_thread(
    mut stream: LocalStream,
    event_tx: tokio::sync::mpsc::Sender<ClientLoopEvent>,
    should_quit: &Arc<AtomicBool>,
    max_frame_size: usize,
) {
    // Ensure the read stream is in blocking mode to avoid WouldBlock errors
    // from read_exact inside read_message. The stream should already be
    // blocking after handshake, but we enforce it here as a safety measure.
    if stream.set_nonblocking(false).is_err() {
        // If we can't set blocking mode, the stream is likely broken.
        let _ = event_tx.blocking_send(ClientLoopEvent::ServerDisconnected);
        return;
    }

    loop {
        if should_quit.load(Ordering::Acquire) {
            break;
        }

        match protocol::read_message(&mut stream, max_frame_size) {
            Ok(msg) => {
                // Inflate compressed payloads on the reader thread so the main
                // loop only ever sees plain messages.
                let msg = protocol::decompress_server_message(msg);
                if event_tx
                    .blocking_send(ClientLoopEvent::ServerMessage(msg))
                    .is_err()
                {
                    break; // Main loop gone.
                }
            }
            Err(protocol::FramingError::UnexpectedEof) => {
                // Server closed connection.
                let _ = event_tx.blocking_send(ClientLoopEvent::ServerDisconnected);
                break;
            }
            Err(protocol::FramingError::Io(err)) if err.kind() == io::ErrorKind::WouldBlock => {
                // Should not happen with blocking mode, but handle gracefully
                // in case the stream was set nonblocking by another clone.
                std::thread::sleep(Duration::from_millis(1));
                continue;
            }
            Err(err) => {
                warn!(err = %err, "server read error");
                let _ = event_tx.blocking_send(ClientLoopEvent::ServerDisconnected);
                break;
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Write helpers
// ---------------------------------------------------------------------------

/// Writes a message to the server stream (blocking).
fn write_to_server(stream: &mut LocalStream, msg: &ClientMessage) -> io::Result<()> {
    protocol::write_message(stream, msg).map_err(|e| io::Error::other(e.to_string()))
}

/// The server owning the user's focus — input, clipboard bridging, and mouse-capture requests
/// route here. Single-server mode (no supervisor) is always the main server.
#[cfg(unix)]
fn active_server_id(state: &ClientState) -> supervisor::ServerId {
    state
        .supervisor_model
        .as_ref()
        .map(|model| model.active_server_id().clone())
        .unwrap_or_else(supervisor::ServerId::main)
}

#[cfg(unix)]
fn queue_to_server(handle: &ServerWriteHandle, msg: ClientMessage) -> io::Result<()> {
    handle
        .tx
        .send(msg)
        .map_err(|_| io::Error::new(io::ErrorKind::BrokenPipe, "server writer stopped"))
}

#[cfg(unix)]
fn queue_to_server_id(
    server_writes: &HashMap<supervisor::ServerId, ServerWriteHandle>,
    server_id: &supervisor::ServerId,
    msg: ClientMessage,
) -> io::Result<()> {
    let Some(handle) = server_writes.get(server_id) else {
        return Err(io::Error::new(
            io::ErrorKind::NotConnected,
            format!("server stream {server_id:?} is not connected"),
        ));
    };
    queue_to_server(handle, msg)
}

// ---------------------------------------------------------------------------
// Notifications
// ---------------------------------------------------------------------------

#[cfg(unix)]
fn client_remote_image_paste_key(
    config: &crate::config::Config,
) -> Option<(crossterm::event::KeyCode, crossterm::event::KeyModifiers)> {
    if !is_remote_client_process() {
        return None;
    }

    match config.remote_image_paste_key() {
        Ok(key) => key,
        Err(diagnostic) => {
            warn!(diagnostic = %diagnostic, "local remote image paste key config diagnostic");
            None
        }
    }
}

fn reload_local_client_config(
    sound_config: &mut crate::config::SoundConfig,
    redraw_on_focus_gained: &mut bool,
    draw_host_cursor: &mut bool,
    #[cfg(unix)] remote_image_paste_key: &mut Option<(
        crossterm::event::KeyCode,
        crossterm::event::KeyModifiers,
    )>,
) {
    match crate::config::load_live_config() {
        Ok(loaded) => {
            for diagnostic in loaded.config.ui.sound.diagnostics() {
                warn!(diagnostic = %diagnostic, "local sound config diagnostic");
            }
            #[cfg(unix)]
            let loaded_remote_image_paste_key = client_remote_image_paste_key(&loaded.config);
            *sound_config = loaded.config.ui.sound;
            *redraw_on_focus_gained = loaded.config.ui.redraw_on_focus_gained;
            *draw_host_cursor = should_draw_host_cursor(loaded.config.ui.host_cursor);
            #[cfg(unix)]
            {
                *remote_image_paste_key = loaded_remote_image_paste_key;
            }
            debug!("reloaded local client config");
        }
        Err(diagnostics) => {
            warn!(diagnostics = ?diagnostics, "failed to reload local client config; keeping current client config");
        }
    }
}

fn handle_notify(
    kind: NotifyKind,
    message: &str,
    body: Option<&str>,
    sound_config: &crate::config::SoundConfig,
) {
    handle_notify_with_notifiers(
        kind,
        message,
        body,
        sound_config,
        crate::terminal_notify::show_notification,
        crate::platform::show_desktop_notification,
    );
}

fn handle_notify_with_notifiers(
    kind: NotifyKind,
    message: &str,
    body: Option<&str>,
    sound_config: &crate::config::SoundConfig,
    mut show_terminal_notification: impl FnMut(&str, Option<&str>) -> io::Result<bool>,
    mut show_system_notification: impl FnMut(&str, Option<&str>) -> io::Result<bool>,
) {
    match kind {
        NotifyKind::Sound => {
            let Some(sound) = sound_from_notify_message(message) else {
                warn!(
                    message = message,
                    "received unknown sound notification from server"
                );
                return;
            };
            if sound_config.enabled {
                crate::sound::play(sound, sound_config);
            }
        }
        NotifyKind::Toast => {
            debug!(
                message = message,
                "received terminal toast notification from server"
            );
            if let Err(err) = show_terminal_notification(message, body) {
                warn!(err = %err, "failed to emit terminal notification");
            }
        }
        NotifyKind::SystemToast => {
            debug!(
                message = message,
                "received system toast notification from server"
            );
            if let Err(err) = show_system_notification(message, body) {
                warn!(err = %err, "failed to emit system notification");
            }
        }
    }
}

fn sound_from_notify_message(message: &str) -> Option<crate::sound::Sound> {
    match message {
        "agent done" => Some(crate::sound::Sound::Done),
        "agent attention" => Some(crate::sound::Sound::Request),
        _ => None,
    }
}

#[cfg(unix)]
fn should_bridge_clipboard_image_paste(
    data: &[u8],
    is_remote_client: bool,
    remote_image_paste_key: Option<(crossterm::event::KeyCode, crossterm::event::KeyModifiers)>,
) -> bool {
    if data == b"\x1b[200~\x1b[201~" {
        return is_remote_client;
    }

    let Some(remote_image_paste_key) = remote_image_paste_key else {
        return false;
    };

    let events = crate::raw_input::parse_raw_input_bytes_sync(data);
    matches!(
        events.as_slice(),
        [crate::raw_input::RawInputEvent::Key(key)]
            if key.kind == crossterm::event::KeyEventKind::Press
                && crate::config::terminal_key_matches_combo(*key, remote_image_paste_key)
    )
}

#[cfg(unix)]
fn read_image_file_from_terminal_drop(
    data: &[u8],
    is_remote_client: bool,
) -> Option<crate::platform::ClipboardImage> {
    let (path, extension) = image_path_from_terminal_drop(data, is_remote_client)?;
    let metadata = std::fs::metadata(&path).ok()?;
    if !metadata.is_file() {
        return None;
    }

    let file = std::fs::File::open(&path).ok()?;
    let bytes =
        match crate::platform::read_limited_reader(file, MAX_CLIPBOARD_IMAGE_PAYLOAD).ok()? {
            crate::platform::LimitedRead::Complete(bytes) => bytes,
            crate::platform::LimitedRead::Empty => return None,
            crate::platform::LimitedRead::Oversized => {
                warn!(
                    max = MAX_CLIPBOARD_IMAGE_PAYLOAD,
                    "local image file drop is too large to bridge"
                );
                return None;
            }
        };

    Some(crate::platform::ClipboardImage { bytes, extension })
}

#[cfg(unix)]
fn image_path_from_terminal_drop(
    data: &[u8],
    is_remote_client: bool,
) -> Option<(std::path::PathBuf, &'static str)> {
    if !is_remote_client {
        return None;
    }

    let bytes = bracketed_paste_payload(data).unwrap_or(data);
    let text = std::str::from_utf8(bytes).ok()?;
    let text = text.trim_end_matches(['\r', '\n']);
    if text.is_empty() || text.contains(['\r', '\n']) {
        return None;
    }

    let text = unescape_terminal_drop_path(strip_matching_path_quotes(text));
    let path = std::path::PathBuf::from(text);
    if !path.is_absolute() {
        return None;
    }

    let extension = recognized_image_extension(path.extension()?.to_str()?)?;
    Some((path, extension))
}

#[cfg(unix)]
fn bracketed_paste_payload(data: &[u8]) -> Option<&[u8]> {
    const START: &[u8] = b"\x1b[200~";
    const END: &[u8] = b"\x1b[201~";
    data.strip_prefix(START)?.strip_suffix(END)
}

#[cfg(unix)]
fn strip_matching_path_quotes(text: &str) -> &str {
    if text.len() < 2 {
        return text;
    }

    let bytes = text.as_bytes();
    match (bytes.first(), bytes.last()) {
        (Some(b'\''), Some(b'\'')) | (Some(b'"'), Some(b'"')) => &text[1..text.len() - 1],
        _ => text,
    }
}

#[cfg(unix)]
fn unescape_terminal_drop_path(text: &str) -> String {
    let mut unescaped = String::with_capacity(text.len());
    let mut chars = text.chars();
    while let Some(ch) = chars.next() {
        if ch == '\\' {
            if let Some(escaped) = chars.next() {
                unescaped.push(escaped);
            } else {
                unescaped.push(ch);
            }
        } else {
            unescaped.push(ch);
        }
    }
    unescaped
}

#[cfg(unix)]
fn recognized_image_extension(extension: &str) -> Option<&'static str> {
    if extension.eq_ignore_ascii_case("png") {
        Some("png")
    } else if extension.eq_ignore_ascii_case("jpg") || extension.eq_ignore_ascii_case("jpeg") {
        Some("jpg")
    } else if extension.eq_ignore_ascii_case("gif") {
        Some("gif")
    } else if extension.eq_ignore_ascii_case("webp") {
        Some("webp")
    } else if extension.eq_ignore_ascii_case("bmp") {
        Some("bmp")
    } else {
        None
    }
}

// ---------------------------------------------------------------------------
// Clipboard forwarding
// ---------------------------------------------------------------------------

/// Decode a clipboard payload forwarded by the server.
fn decode_clipboard_payload(data: &str) -> Option<Vec<u8>> {
    use base64::Engine;
    base64::engine::general_purpose::STANDARD.decode(data).ok()
}

/// Forwards a clipboard write from the server to the local client clipboard.
fn forward_clipboard(data: &str) {
    let Some(bytes) = decode_clipboard_payload(data) else {
        warn!("received invalid clipboard payload from server");
        return;
    };

    crate::selection::write_osc52_bytes(&bytes);
}

fn window_title_osc(title: Option<&str>) -> Vec<u8> {
    let title = title.unwrap_or("herdr");
    let safe_title = title
        .chars()
        .filter(|ch| !matches!(*ch, '\u{1b}' | '\u{7}' | '\u{9c}'))
        .collect::<String>();
    format!("\x1b]0;{safe_title}\x07").into_bytes()
}

fn write_window_title(title: Option<&str>) {
    let _ = io::stdout().write_all(&window_title_osc(title));
}

// ---------------------------------------------------------------------------
// Frame output
// ---------------------------------------------------------------------------

/// Encode and blit one full semantic frame to the host terminal, committing it
/// as the blit encoder's diff baseline. Shared by the full-frame and
/// delta-reconstruction paths.
fn draw_semantic_frame(state: &mut ClientState, frame_data: protocol::FrameData) {
    let frame_data = if state.draw_host_cursor {
        render_ansi::frame_with_drawn_cursor(frame_data)
    } else {
        frame_data
    };
    let encoded = if state.draw_host_cursor {
        state
            .blit_encoder
            .encode_with_suppressed_visible_cursor(&frame_data, false)
    } else {
        state.blit_encoder.encode(&frame_data, false)
    };
    let mut stdout = io::stdout();
    let graphics = if state.kitty_graphics_enabled {
        frame_data.graphics.as_slice()
    } else {
        &[]
    };
    let _ = write_encoded_frame_with_graphics(&mut stdout, &encoded.bytes, graphics);
    let _ = stdout.flush();
    state.blit_encoder.commit(frame_data, encoded);
}

fn write_encoded_frame_with_graphics(
    mut writer: impl io::Write,
    encoded: &[u8],
    graphics: &[u8],
) -> io::Result<()> {
    writer.write_all(encoded)?;
    if graphics.is_empty() {
        return Ok(());
    }

    record_received_kitty_graphics(graphics);
    writer.write_all(b"\x1b7")?;
    writer.write_all(graphics)?;
    writer.write_all(b"\x1b8")
}

fn contains_kitty_graphics_bytes(bytes: &[u8]) -> bool {
    bytes.windows(3).any(|window| window == b"\x1b_G")
}

fn record_received_kitty_graphics(bytes: &[u8]) {
    let ids = kitty_graphics_image_ids(bytes);
    if ids.is_empty() {
        return;
    }
    let set = RECEIVED_KITTY_GRAPHICS_IDS.get_or_init(|| Mutex::new(HashSet::new()));
    if let Ok(mut set) = set.lock() {
        set.extend(ids);
    }
}

fn clear_received_kitty_graphics(mut writer: impl io::Write) -> io::Result<()> {
    let Some(set) = RECEIVED_KITTY_GRAPHICS_IDS.get() else {
        return Ok(());
    };
    let Ok(mut set) = set.lock() else {
        return Ok(());
    };
    for id in set.drain() {
        write!(writer, "\x1b_Ga=d,d=I,i={id},q=2;\x1b\\")?;
    }
    writer.flush()
}

fn kitty_graphics_image_ids(bytes: &[u8]) -> Vec<u32> {
    let mut ids = Vec::new();
    let mut index = 0usize;
    while let Some(start) = find_subslice(&bytes[index..], b"\x1b_G") {
        let command_start = index + start + 3;
        let Some(end) = find_subslice(&bytes[command_start..], b"\x1b\\") else {
            break;
        };
        let command = &bytes[command_start..command_start + end];
        if let Some(id) = kitty_graphics_command_image_id(command) {
            ids.push(id);
        }
        index = command_start + end + 2;
    }
    ids
}

fn kitty_graphics_command_image_id(command: &[u8]) -> Option<u32> {
    let header_end = command
        .iter()
        .position(|byte| *byte == b';')
        .unwrap_or(command.len());
    for part in command[..header_end].split(|byte| *byte == b',') {
        let Some(value) = part.strip_prefix(b"i=") else {
            continue;
        };
        let text = std::str::from_utf8(value).ok()?;
        if let Ok(id) = text.parse::<u32>() {
            return Some(id);
        }
    }
    None
}

fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || needle.len() > haystack.len() {
        return None;
    }
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

// ---------------------------------------------------------------------------
// Resize polling
// ---------------------------------------------------------------------------

fn current_terminal_geometry(kitty_graphics_enabled: bool) -> (u16, u16, u32, u32) {
    let (cols, rows) = crossterm::terminal::size().unwrap_or((80, 24));
    if !kitty_graphics_enabled {
        return (cols, rows, 0, 0);
    }
    let Ok(size) = crossterm::terminal::window_size() else {
        return (cols, rows, 8, 16);
    };
    if size.columns == 0 || size.rows == 0 || size.width == 0 || size.height == 0 {
        return (cols, rows, 8, 16);
    }
    (
        cols,
        rows,
        (size.width as u32 / size.columns as u32).max(1),
        (size.height as u32 / size.rows as u32).max(1),
    )
}

/// Polls the terminal size and sends resize events when it changes.
fn resize_poll_loop(
    resize_tx: tokio::sync::mpsc::Sender<ClientLoopEvent>,
    initial_cols: u16,
    initial_rows: u16,
    kitty_graphics_enabled: bool,
    should_quit: &Arc<AtomicBool>,
) {
    let (_, _, initial_cell_width, initial_cell_height) =
        current_terminal_geometry(kitty_graphics_enabled);
    let mut last_size = (
        initial_cols,
        initial_rows,
        initial_cell_width,
        initial_cell_height,
    );
    while !should_quit.load(Ordering::Acquire) {
        std::thread::sleep(Duration::from_millis(100));
        let new_size = current_terminal_geometry(kitty_graphics_enabled);
        if new_size != last_size {
            last_size = new_size;
            if resize_tx
                .blocking_send(ClientLoopEvent::Resize(
                    new_size.0, new_size.1, new_size.2, new_size.3,
                ))
                .is_err()
            {
                break; // Main loop gone.
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Logging
// ---------------------------------------------------------------------------

/// Initialize logging for the client process.
fn query_host_terminal_theme() {
    let _ = write_host_terminal_theme_query(io::stdout());
}

fn should_query_host_terminal_theme() -> bool {
    !cfg!(windows)
}

fn write_host_terminal_theme_query(mut writer: impl io::Write) -> io::Result<()> {
    writer.write_all(crate::terminal_theme::HOST_COLOR_QUERY_SEQUENCE.as_bytes())?;
    writer.flush()
}

fn init_logging() {
    crate::logging::init_file_logging("herdr-client.log");
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::OsString;
    use std::sync::{Mutex, OnceLock};

    fn env_lock() -> &'static Mutex<()> {
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| Mutex::new(()))
    }

    fn restore_env_var(key: &str, value: Option<OsString>) {
        if let Some(value) = value {
            std::env::set_var(key, value);
        } else {
            std::env::remove_var(key);
        }
    }

    struct EnvVarGuard {
        key: &'static str,
        previous: Option<OsString>,
    }

    impl EnvVarGuard {
        fn set(key: &'static str, value: &str) -> Self {
            let previous = std::env::var_os(key);
            std::env::set_var(key, value);
            Self { key, previous }
        }
    }

    impl Drop for EnvVarGuard {
        fn drop(&mut self) {
            restore_env_var(self.key, self.previous.clone());
        }
    }

    #[test]
    fn windows_virtual_terminal_input_mode_sets_only_vti_bit() {
        assert_eq!(windows_virtual_terminal_input_mode(0x01f0), 0x03f0);
        assert_eq!(windows_virtual_terminal_input_mode(0x03f0), 0x03f0);
    }

    struct EnvVarsRemovedGuard {
        previous: Vec<(&'static str, Option<OsString>)>,
    }

    impl EnvVarsRemovedGuard {
        fn new(keys: &[&'static str]) -> Self {
            let previous: Vec<_> = keys
                .iter()
                .map(|key| (*key, std::env::var_os(key)))
                .collect();
            for key in keys {
                std::env::remove_var(key);
            }
            Self { previous }
        }
    }

    impl Drop for EnvVarsRemovedGuard {
        fn drop(&mut self) {
            for (key, value) in self.previous.clone() {
                restore_env_var(key, value);
            }
        }
    }

    #[test]
    fn host_cursor_policy_auto_uses_platform_default() {
        assert_eq!(
            should_draw_host_cursor(crate::config::HostCursorModeConfig::Auto),
            crate::platform::should_draw_host_cursor_by_default()
        );
    }

    #[test]
    fn host_cursor_policy_native_and_drawn_override_auto_detection() {
        let _guard = env_lock().lock().unwrap();
        let _env = EnvVarGuard::set("TERM_PROGRAM", "WezTerm");

        assert!(!should_draw_host_cursor(
            crate::config::HostCursorModeConfig::Native
        ));
        assert!(should_draw_host_cursor(
            crate::config::HostCursorModeConfig::Drawn
        ));
    }

    #[cfg(unix)]
    #[test]
    fn clipboard_image_paste_bridge_triggers_on_configured_key_and_empty_paste() {
        let ctrl_v = crate::config::parse_key_combo("ctrl+v").unwrap();
        assert!(should_bridge_clipboard_image_paste(
            &[0x16],
            true,
            Some(ctrl_v)
        ));
        assert!(should_bridge_clipboard_image_paste(
            b"\x1b[118;5u",
            true,
            Some(ctrl_v)
        ));
        assert!(should_bridge_clipboard_image_paste(
            b"\x1b[200~\x1b[201~",
            true,
            None
        ));
        assert!(!should_bridge_clipboard_image_paste(
            b"\x1b[200~\x1b[201~",
            false,
            Some(ctrl_v)
        ));
        assert!(!should_bridge_clipboard_image_paste(
            b"\x1b[200~text\x1b[201~",
            true,
            Some(ctrl_v)
        ));
        assert!(!should_bridge_clipboard_image_paste(&[0x16], true, None));
        assert!(!should_bridge_clipboard_image_paste(
            b"v",
            true,
            Some(ctrl_v)
        ));
    }

    #[cfg(unix)]
    struct TempImageFile {
        path: std::path::PathBuf,
    }

    #[cfg(unix)]
    impl TempImageFile {
        fn new(extension: &str, bytes: &[u8]) -> Self {
            Self::with_name_fragment("test", extension, bytes)
        }

        fn with_name_fragment(name_fragment: &str, extension: &str, bytes: &[u8]) -> Self {
            let nanos = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let path = std::env::temp_dir().join(format!(
                "herdr-client-drop-{name_fragment}-{}-{nanos}.{extension}",
                std::process::id()
            ));
            std::fs::write(&path, bytes).unwrap();
            Self { path }
        }
    }

    #[cfg(unix)]
    impl Drop for TempImageFile {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.path);
        }
    }

    #[cfg(unix)]
    #[test]
    fn remote_image_file_drop_bridge_reads_bracketed_absolute_image_path() {
        let file = TempImageFile::new("PNG", b"image-bytes");
        let input = format!("\x1b[200~{}\x1b[201~", file.path.display());

        let image = read_image_file_from_terminal_drop(input.as_bytes(), true).unwrap();

        assert_eq!(image.extension, "png");
        assert_eq!(image.bytes, b"image-bytes");
    }

    #[cfg(unix)]
    #[test]
    fn remote_image_file_drop_bridge_reads_plain_quoted_path_with_newline() {
        let file = TempImageFile::new("jpeg", b"jpeg-bytes");
        let input = format!("'{}'\n", file.path.display());

        let image = read_image_file_from_terminal_drop(input.as_bytes(), true).unwrap();

        assert_eq!(image.extension, "jpg");
        assert_eq!(image.bytes, b"jpeg-bytes");
    }

    #[cfg(unix)]
    #[test]
    fn remote_image_file_drop_bridge_unescapes_spaces_in_paths() {
        let file = TempImageFile::with_name_fragment("space test", "png", b"image-bytes");
        let escaped_path = file.path.display().to_string().replace(' ', "\\ ");

        let image = read_image_file_from_terminal_drop(escaped_path.as_bytes(), true).unwrap();

        assert_eq!(image.extension, "png");
        assert_eq!(image.bytes, b"image-bytes");
    }

    #[cfg(unix)]
    #[test]
    fn remote_image_file_drop_bridge_ignores_non_remote_and_non_image_input() {
        let file = TempImageFile::new("png", b"image-bytes");
        let path = file.path.display().to_string();

        assert!(read_image_file_from_terminal_drop(path.as_bytes(), false).is_none());
        assert!(read_image_file_from_terminal_drop(b"relative.png\n", true).is_none());
        assert!(read_image_file_from_terminal_drop(b"/tmp/file.txt\n", true).is_none());
        assert!(read_image_file_from_terminal_drop(
            format!("{}\nextra", file.path.display()).as_bytes(),
            true
        )
        .is_none());
    }

    #[test]
    fn graphics_bytes_are_written_after_blit_with_saved_cursor() {
        let mut output = Vec::new();
        write_encoded_frame_with_graphics(
            &mut output,
            b"\x1b[?2026htext\x1b[?2026lcursor",
            b"graphics",
        )
        .unwrap();

        assert_eq!(
            output,
            b"\x1b[?2026htext\x1b[?2026lcursor\x1b7graphics\x1b8"
        );
    }

    #[test]
    fn empty_graphics_writes_only_blit_frame() {
        let mut output = Vec::new();
        write_encoded_frame_with_graphics(&mut output, b"text", b"").unwrap();

        assert_eq!(output, b"text");
    }

    #[test]
    fn terminal_frame_kitty_detection_matches_apc_prefix() {
        assert!(contains_kitty_graphics_bytes(b"text\x1b_Ga=p;\x1b\\"));
        assert!(!contains_kitty_graphics_bytes(b"text\x1b[?2026h"));
    }

    #[test]
    fn kitty_graphics_image_id_parser_tracks_herdr_ids_only() {
        let ids = kitty_graphics_image_ids(
            b"text\x1b_Ga=t,t=d,f=32,s=1,v=1,i=10023,q=2;AAAA\x1b\\\x1b_Ga=p,i=10023,p=7;\x1b\\",
        );
        assert_eq!(ids, vec![10023, 10023]);
    }

    #[test]
    fn kitty_graphics_cleanup_deletes_tracked_images_not_all_images() {
        record_received_kitty_graphics(b"\x1b_Ga=t,i=123,q=2;AAAA\x1b\\");
        let mut output = Vec::new();
        clear_received_kitty_graphics(&mut output).unwrap();
        let text = String::from_utf8(output).unwrap();
        assert!(text.contains("a=d,d=I,i=123"));
        assert!(!text.contains("d=A"));
    }

    #[test]
    fn write_host_terminal_theme_query_emits_osc_queries() {
        let mut output = Vec::new();
        write_host_terminal_theme_query(&mut output).unwrap();
        assert_eq!(
            output,
            crate::terminal_theme::HOST_COLOR_QUERY_SEQUENCE.as_bytes()
        );
    }

    #[test]
    fn write_host_color_scheme_report_mode_emits_mode_sequences() {
        let mut output = Vec::new();
        write_host_color_scheme_report_mode(&mut output, true).unwrap();
        write_host_color_scheme_report_mode(&mut output, false).unwrap();

        let mut expected = Vec::new();
        expected.extend_from_slice(
            crate::terminal_theme::HOST_COLOR_SCHEME_REPORT_ENABLE_SEQUENCE.as_bytes(),
        );
        expected.extend_from_slice(
            crate::terminal_theme::HOST_COLOR_SCHEME_REPORT_DISABLE_SEQUENCE.as_bytes(),
        );
        assert_eq!(output, expected);
    }

    #[test]
    fn color_scheme_change_event_requests_host_theme_query() {
        let events = crate::raw_input::parse_raw_input_bytes_sync(b"\x1b[?997;1n");

        assert!(crate::raw_input::events_require_host_terminal_theme_query(
            &events
        ));
    }

    #[test]
    fn host_terminal_theme_query_is_disabled_on_windows() {
        assert_eq!(should_query_host_terminal_theme(), !cfg!(windows));
    }

    #[test]
    fn color_scheme_reports_are_enabled_only_for_full_clients() {
        assert_eq!(
            should_enable_host_color_scheme_reports(true),
            !cfg!(windows)
        );
        assert!(!should_enable_host_color_scheme_reports(false));
    }

    #[test]
    fn terminal_restore_postlude_restores_visible_default_cursor() {
        let mut output = Vec::new();
        write_terminal_restore_postlude(&mut output, false).unwrap();
        assert_eq!(output, b"\x1b[?25h\x1b[0 q");
    }

    #[test]
    fn terminal_restore_postlude_disables_color_scheme_reports_when_enabled() {
        let mut output = Vec::new();
        write_terminal_restore_postlude(&mut output, true).unwrap();

        let mut expected = Vec::new();
        expected.extend_from_slice(
            crate::terminal_theme::HOST_COLOR_SCHEME_REPORT_DISABLE_SEQUENCE.as_bytes(),
        );
        expected.extend_from_slice(b"\x1b[?25h\x1b[0 q");
        assert_eq!(output, expected);
    }

    #[cfg(unix)]
    #[test]
    fn attach_escape_detaches_on_prefix_q() {
        let mut escape = AttachEscapeState::default();
        assert!(matches!(
            escape.filter_input(vec![0x02], 24, 3),
            AttachInputAction::None
        ));
        assert!(matches!(
            escape.filter_input(vec![b'q'], 24, 3),
            AttachInputAction::Detach
        ));
    }

    #[cfg(unix)]
    #[test]
    fn attach_escape_sends_literal_prefix_on_double_prefix() {
        let mut escape = AttachEscapeState::default();
        assert!(matches!(
            escape.filter_input(vec![0x02], 24, 3),
            AttachInputAction::None
        ));
        match escape.filter_input(vec![0x02], 24, 3) {
            AttachInputAction::Forward(bytes) => assert_eq!(bytes, vec![0x02]),
            other => panic!("expected forwarded prefix, got {other:?}"),
        }
    }

    #[cfg(unix)]
    #[test]
    fn attach_escape_forwards_prefix_before_non_escape_key() {
        let mut escape = AttachEscapeState::default();
        assert!(matches!(
            escape.filter_input(vec![b'a', 0x02], 24, 3),
            AttachInputAction::Forward(bytes) if bytes == b"a"
        ));
        match escape.filter_input(vec![b'x'], 24, 3) {
            AttachInputAction::Forward(bytes) => assert_eq!(bytes, vec![0x02, b'x']),
            other => panic!("expected forwarded bytes, got {other:?}"),
        }
    }

    #[cfg(unix)]
    #[test]
    fn attach_escape_turns_wheel_into_scroll_action() {
        let mut escape = AttachEscapeState::default();
        match escape.filter_input(b"\x1b[<64;11;6M".to_vec(), 24, 7) {
            AttachInputAction::Scroll {
                source,
                direction,
                lines,
                column,
                row,
                ..
            } => {
                assert_eq!(source, AttachScrollSource::Wheel);
                assert_eq!(direction, AttachScrollDirection::Up);
                assert_eq!(lines, 7);
                assert_eq!(column, Some(10));
                assert_eq!(row, Some(5));
            }
            other => panic!("expected scroll action, got {other:?}"),
        }
    }

    #[cfg(unix)]
    #[test]
    fn attach_escape_swallows_non_wheel_mouse_reports() {
        let mut escape = AttachEscapeState::default();
        assert!(matches!(
            escape.filter_input(b"\x1b[<0;11;6M".to_vec(), 24, 7),
            AttachInputAction::None
        ));
    }

    #[cfg(unix)]
    #[test]
    fn attach_escape_turns_plain_page_keys_into_scroll_actions() {
        let mut escape = AttachEscapeState::default();
        match escape.filter_input(b"\x1b[5~".to_vec(), 12, 3) {
            AttachInputAction::Scroll {
                source,
                direction,
                lines,
                ..
            } => {
                assert_eq!(
                    source,
                    AttachScrollSource::PageKey {
                        input: b"\x1b[5~".to_vec()
                    }
                );
                assert_eq!(direction, AttachScrollDirection::Up);
                assert_eq!(lines, 11);
            }
            other => panic!("expected page-up scroll action, got {other:?}"),
        }

        match escape.filter_input(b"\x1b[6~".to_vec(), 12, 3) {
            AttachInputAction::Scroll {
                source,
                direction,
                lines,
                ..
            } => {
                assert_eq!(
                    source,
                    AttachScrollSource::PageKey {
                        input: b"\x1b[6~".to_vec()
                    }
                );
                assert_eq!(direction, AttachScrollDirection::Down);
                assert_eq!(lines, 11);
            }
            other => panic!("expected page-down scroll action, got {other:?}"),
        }
    }

    #[cfg(unix)]
    #[test]
    fn attach_escape_forwards_modified_page_key() {
        let mut escape = AttachEscapeState::default();
        match escape.filter_input(b"\x1b[5;5~".to_vec(), 12, 3) {
            AttachInputAction::Forward(bytes) => assert_eq!(bytes, b"\x1b[5;5~"),
            other => panic!("expected modified page key to forward, got {other:?}"),
        }
    }

    #[test]
    fn client_error_display_connection_failed() {
        let err = ClientError::ConnectionFailed(io::Error::new(
            io::ErrorKind::ConnectionRefused,
            "connection refused",
        ));
        let msg = err.to_string();
        assert!(
            msg.contains("failed to connect to server"),
            "should mention connection failure: {msg}"
        );
        assert!(
            msg.contains("herdr server"),
            "should suggest starting server: {msg}"
        );
    }

    #[test]
    fn client_error_display_handshake_rejected() {
        let err = ClientError::HandshakeRejected {
            version: 1,
            error: "incompatible".into(),
        };
        let msg = err.to_string();
        assert!(
            msg.contains("rejected handshake"),
            "should mention rejection: {msg}"
        );
        assert!(msg.contains("incompatible"), "should include error: {msg}");
    }

    #[test]
    fn client_error_display_server_shutdown() {
        let err = ClientError::ServerShutdown {
            reason: Some("maintenance".into()),
        };
        let msg = err.to_string();
        assert!(
            msg.contains("server shut down"),
            "should mention shutdown: {msg}"
        );
        assert!(msg.contains("maintenance"), "should include reason: {msg}");
    }

    #[test]
    fn client_error_display_server_shutdown_no_reason() {
        let err = ClientError::ServerShutdown { reason: None };
        let msg = err.to_string();
        assert!(
            msg.contains("server shut down"),
            "should mention shutdown: {msg}"
        );
    }

    #[test]
    fn client_error_display_detached_default_session_reattach_hint() {
        let _guard = env_lock().lock().unwrap();
        let _env = EnvVarsRemovedGuard::new(&[
            crate::remote::REATTACH_COMMAND_ENV_VAR,
            crate::session::SESSION_ENV_VAR,
        ]);
        let err = ClientError::ServerShutdown {
            reason: Some("detached".into()),
        };
        let msg = err.to_string();
        assert!(
            msg.contains("Run `herdr` to reattach"),
            "should suggest default reattach command: {msg}"
        );
    }

    #[test]
    fn client_error_display_detached_named_session_reattach_hint() {
        let _guard = env_lock().lock().unwrap();
        let _remote_env = EnvVarsRemovedGuard::new(&[crate::remote::REATTACH_COMMAND_ENV_VAR]);
        let _session_env = EnvVarGuard::set(crate::session::SESSION_ENV_VAR, "work");
        let err = ClientError::ServerShutdown {
            reason: Some("detached".into()),
        };
        let msg = err.to_string();
        assert!(
            msg.contains("Run `herdr session attach work` to reattach"),
            "should suggest named session reattach command: {msg}"
        );
    }

    #[test]
    fn client_error_display_detached_remote_reattach_hint_takes_precedence() {
        let _guard = env_lock().lock().unwrap();
        let _remote_env = EnvVarGuard::set(
            crate::remote::REATTACH_COMMAND_ENV_VAR,
            "herdr --remote host --session work",
        );
        let _session_env = EnvVarGuard::set(crate::session::SESSION_ENV_VAR, "work");
        let err = ClientError::ServerShutdown {
            reason: Some("detached".into()),
        };
        let msg = err.to_string();
        assert!(
            msg.contains("Run `herdr --remote host --session work` to reattach"),
            "should prefer remote reattach command: {msg}"
        );
    }

    #[test]
    fn client_error_display_connection_lost() {
        let _guard = env_lock().lock().unwrap();
        let _env = EnvVarsRemovedGuard::new(&[crate::remote::REATTACH_COMMAND_ENV_VAR]);
        let err =
            ClientError::ConnectionLost(io::Error::new(io::ErrorKind::BrokenPipe, "broken pipe"));
        let msg = err.to_string();
        assert!(
            msg.contains("lost connection to server"),
            "should mention lost connection: {msg}"
        );
    }

    #[test]
    fn client_error_display_remote_connection_lost_has_reattach_hint() {
        let _guard = env_lock().lock().unwrap();
        let _remote_env = EnvVarGuard::set(
            crate::remote::REATTACH_COMMAND_ENV_VAR,
            "herdr --remote host --session work",
        );
        let err =
            ClientError::ConnectionLost(io::Error::new(io::ErrorKind::BrokenPipe, "broken pipe"));
        let msg = err.to_string();
        assert!(
            msg.contains("lost connection to remote Herdr"),
            "should mention remote connection loss: {msg}"
        );
        assert!(
            msg.contains("panes may still be running"),
            "should explain possible persistence: {msg}"
        );
        assert!(
            msg.contains("Run `herdr --remote host --session work` to reattach"),
            "should show remote reattach command: {msg}"
        );
    }

    #[test]
    fn sound_from_notify_message_maps_done() {
        assert_eq!(
            sound_from_notify_message("agent done"),
            Some(crate::sound::Sound::Done)
        );
    }

    #[test]
    fn sound_from_notify_message_maps_attention() {
        assert_eq!(
            sound_from_notify_message("agent attention"),
            Some(crate::sound::Sound::Request)
        );
    }

    #[test]
    fn sound_from_notify_message_rejects_unknown_payloads() {
        assert_eq!(sound_from_notify_message("toast"), None);
    }

    #[test]
    fn reload_local_client_config_refreshes_local_client_presentation_state() {
        let _guard = crate::config::test_config_env_lock().lock().unwrap();
        let path = std::env::temp_dir().join(format!(
            "herdr-client-config-reload-{}-{}.toml",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::write(
            &path,
            "[ui]\nredraw_on_focus_gained = false\nhost_cursor = \"drawn\"\n",
        )
        .unwrap();
        let path_string = path.to_string_lossy().to_string();
        let _env = EnvVarGuard::set(crate::config::CONFIG_PATH_ENV_VAR, &path_string);
        let mut sound_config = crate::config::SoundConfig::default();
        let mut redraw_on_focus_gained = true;
        let mut draw_host_cursor = false;
        #[cfg(unix)]
        let mut remote_image_paste_key = None;

        reload_local_client_config(
            &mut sound_config,
            &mut redraw_on_focus_gained,
            &mut draw_host_cursor,
            #[cfg(unix)]
            &mut remote_image_paste_key,
        );

        assert!(!redraw_on_focus_gained);
        assert!(draw_host_cursor);
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn toast_notify_from_server_is_emitted_even_when_attach_config_was_off() {
        let sound_config = crate::config::SoundConfig::default();
        let mut emitted = None;

        handle_notify_with_notifiers(
            NotifyKind::Toast,
            "pi finished",
            Some("workspace 1"),
            &sound_config,
            |title, body| {
                emitted = Some((title.to_string(), body.map(str::to_string)));
                Ok(true)
            },
            |_, _| Ok(false),
        );

        assert_eq!(
            emitted,
            Some(("pi finished".to_string(), Some("workspace 1".to_string())))
        );
    }

    #[test]
    fn system_toast_notify_from_server_uses_system_notifier() {
        let sound_config = crate::config::SoundConfig::default();
        let mut emitted = None;

        handle_notify_with_notifiers(
            NotifyKind::SystemToast,
            "pi finished",
            Some("workspace 1"),
            &sound_config,
            |_, _| Ok(false),
            |title, body| {
                emitted = Some((title.to_string(), body.map(str::to_string)));
                Ok(true)
            },
        );

        assert_eq!(
            emitted,
            Some(("pi finished".to_string(), Some("workspace 1".to_string())))
        );
    }

    #[test]
    fn system_toast_notify_preserves_colon_in_title() {
        let sound_config = crate::config::SoundConfig::default();
        let mut emitted = None;

        handle_notify_with_notifiers(
            NotifyKind::SystemToast,
            "build: failed",
            Some("api workspace"),
            &sound_config,
            |_, _| Ok(false),
            |title, body| {
                emitted = Some((title.to_string(), body.map(str::to_string)));
                Ok(true)
            },
        );

        assert_eq!(
            emitted,
            Some((
                "build: failed".to_string(),
                Some("api workspace".to_string())
            ))
        );
    }

    #[test]
    fn decode_clipboard_payload_decodes_base64() {
        assert_eq!(decode_clipboard_payload("dGVzdA=="), Some(b"test".to_vec()));
    }

    #[test]
    fn decode_clipboard_payload_rejects_invalid_base64() {
        assert_eq!(decode_clipboard_payload("not-base64!!!"), None);
    }

    #[test]
    fn terminal_control_input_command_accepts_text() {
        let action =
            terminal_control_command_from_json(r#"{"type":"terminal.input","text":"hello"}"#)
                .unwrap();
        let ClientMessage::Input { data } = action else {
            panic!("expected input command");
        };
        assert_eq!(data, b"hello");
    }

    #[test]
    fn terminal_control_input_command_accepts_base64_bytes() {
        let action =
            terminal_control_command_from_json(r#"{"type":"terminal.input","bytes":"G1tB"}"#)
                .unwrap();
        let ClientMessage::Input { data } = action else {
            panic!("expected input command");
        };
        assert_eq!(data, b"\x1b[A");
    }

    #[test]
    fn terminal_control_resize_command_maps_to_client_resize() {
        let action = terminal_control_command_from_json(
            r#"{"type":"terminal.resize","cols":100,"rows":30,"cell_width_px":8,"cell_height_px":16}"#,
        )
        .unwrap();
        let ClientMessage::Resize {
            cols,
            rows,
            cell_width_px,
            cell_height_px,
        } = action
        else {
            panic!("expected resize command");
        };
        assert_eq!(
            (cols, rows, cell_width_px, cell_height_px),
            (100, 30, 8, 16)
        );
    }

    #[test]
    fn terminal_control_scroll_command_maps_to_attach_scroll() {
        let action = terminal_control_command_from_json(
            r#"{"type":"terminal.scroll","direction":"up","lines":3}"#,
        )
        .unwrap();
        let ClientMessage::AttachScroll {
            source,
            direction,
            lines,
            ..
        } = action
        else {
            panic!("expected scroll command");
        };
        assert_eq!(source, AttachScrollSource::Wheel);
        assert_eq!(direction, AttachScrollDirection::Up);
        assert_eq!(lines, 3);
    }

    #[test]
    fn forward_clipboard_uses_local_clipboard_path() {
        unsafe {
            std::env::set_var("SSH_CONNECTION", "1 2 3 4");
        }
        forward_clipboard("dGVzdA==");
        unsafe {
            std::env::remove_var("SSH_CONNECTION");
        }
    }

    #[test]
    fn window_title_osc_strips_terminators_and_defaults_to_herdr() {
        assert_eq!(
            window_title_osc(Some("herdr\x1b api\u{7}\u{9c}")),
            b"\x1b]0;herdr api\x07"
        );
        assert_eq!(window_title_osc(None), b"\x1b]0;herdr\x07");
    }

    /// Multi-server client wiring tests (unix-only, like the mixed-server client itself).
    #[cfg(unix)]
    mod multi_server {
        use super::*;
        use std::time::Instant;

        #[test]
        fn run_remote_op_with_timeout_returns_fast_success() {
            let value = run_remote_op_with_timeout(Duration::from_secs(5), || Ok(7u32)).unwrap();
            assert_eq!(value, 7);
        }

        #[test]
        fn run_remote_op_with_timeout_surfaces_inner_error() {
            let result: Result<(), RemoteOpError> =
                run_remote_op_with_timeout(Duration::from_secs(5), || {
                    Err(io::Error::other("boom"))
                });
            match result {
                Err(RemoteOpError::Failed(err)) => assert_eq!(err.to_string(), "boom"),
                other => panic!("expected Failed(boom), got {other:?}"),
            }
        }

        #[test]
        fn run_remote_op_with_timeout_fails_when_op_exceeds_deadline() {
            // The core anti-hang guarantee: a stuck remote op yields a timeout error, not a wedge.
            let result: Result<(), RemoteOpError> =
                run_remote_op_with_timeout(Duration::from_millis(50), || {
                    std::thread::sleep(Duration::from_secs(30));
                    Ok(())
                });
            assert!(
                matches!(result, Err(RemoteOpError::TimedOut(_))),
                "slow op must time out, got {result:?}"
            );
        }

        #[test]
        fn classify_bridge_error_turns_restart_signal_into_confirm() {
            let signal = crate::remote::RestartConfirmNeeded {
                destination: "macmini".to_string(),
                version: Some("0.5.10".to_string()),
                protocol: Some(6),
            };
            let failure =
                classify_add_remote_bridge_error(RemoteOpError::Failed(io::Error::other(signal)));
            match failure {
                AddRemoteFailure::NeedsRestartConfirm {
                    destination,
                    detail,
                } => {
                    assert_eq!(destination, "macmini");
                    assert!(detail.contains("protocol 6") && detail.contains("Restart"));
                }
                other => panic!("expected NeedsRestartConfirm, got {other:?}"),
            }
        }

        #[test]
        fn classify_bridge_error_maps_plain_failures_to_message() {
            let failure = classify_add_remote_bridge_error(RemoteOpError::Failed(
                io::Error::other("ssh: connect to host x port 22: Connection refused"),
            ));
            match failure {
                AddRemoteFailure::Message(msg) => assert!(msg.contains("cannot reach host")),
                other => panic!("expected Message, got {other:?}"),
            }
        }

        #[test]
        fn maps_common_bridge_failures_to_actionable_text() {
            assert!(map_remote_bridge_error(
                "operation timed out after 90s connecting to the remote host"
            )
            .contains("timed out"));
            assert!(
                map_remote_bridge_error("ssh: connect to host x port 22: Connection refused")
                    .contains("cannot reach host")
            );
            assert!(
                map_remote_bridge_error("Permission denied (publickey,password).")
                    .contains("authentication failed")
            );
            assert!(
                map_remote_bridge_error("ssh: Could not resolve hostname nope")
                    .contains("cannot reach host")
            );
            // An unmatched error passes through verbatim so we never hide unexpected detail.
            assert_eq!(
                map_remote_bridge_error("weird novel failure"),
                "weird novel failure"
            );
        }

        fn test_remote_definition(
            id: &str,
            name: &str,
        ) -> crate::remote_registry::RemoteDefinitionSnapshot {
            crate::remote_registry::RemoteDefinitionSnapshot {
                id: id.into(),
                name: name.into(),
                target: crate::remote_registry::RemoteTargetSnapshot::Local {
                    session: Some(id.into()),
                },
                session: None,
                keybindings: crate::remote_registry::RemoteKeybindingsSnapshot::Local,
                disabled: false,
            }
        }

        fn test_client_state_with_model(model: supervisor::ClientSupervisorModel) -> ClientState {
            ClientState {
                blit_encoder: render_ansi::BlitEncoder::new(),
                mouse_capture_active: false,
                reported_size: (80, 24),
                host_size: (80, 24),
                cell_size_px: (0, 0),
                sound_config: crate::config::SoundConfig::default(),
                kitty_graphics_enabled: false,
                attach_escape: None,
                mouse_scroll_lines: 3,
                remote_image_paste_key: None,
                redraw_on_focus_gained: false,
                draw_host_cursor: false,
                frame_stats: ClientFrameStats::default(),
                compositor: None,
                supervisor_model: Some(model),
                last_supervisor_summary_refresh: Instant::now(),
                frame_cache: HashMap::new(),
                rx_counters: RxByteCounters::default(),
                server_rx_sample: HashMap::new(),
                last_rx_sample_at: Instant::now(),
                ping_nonce: 0,
                pending_pings: HashMap::new(),
                last_ping_at: Instant::now(),
                summary_subscription_server_ids: HashSet::new(),
                pending_summary_refresh_server_ids: HashSet::new(),
                queued_summary_refresh_server_ids: HashSet::new(),
                pending_secondary_connect_server_ids: HashSet::new(),
                pending_add_remote: false,
                ssh_bridges: HashMap::new(),
                secondary_retries: HashMap::new(),
                last_animation_tick: Instant::now(),
                last_summary_refresh: HashMap::new(),
            }
        }

        #[test]
        fn hello_message_uses_requested_surface_mode() {
            let hello = build_hello_message(
                80,
                24,
                0,
                0,
                RenderEncoding::SemanticFrame,
                ClientSurfaceMode::EmbeddedContent,
                ClientKeybindings::Server,
                false,
            );

            match hello {
                ClientMessage::Hello {
                    surface_mode,
                    launch_mode,
                    ..
                } => {
                    assert_eq!(surface_mode, ClientSurfaceMode::EmbeddedContent);
                    assert_eq!(launch_mode, ClientLaunchMode::App);
                }
                other => panic!("expected hello, got {other:?}"),
            }
        }

        #[derive(Default)]
        struct BootstrapApi {
            requests: Vec<&'static str>,
            remotes: Vec<crate::remote_registry::RemoteDefinitionSnapshot>,
        }

        impl supervisor::SupervisorApi for BootstrapApi {
            fn request(
                &mut self,
                request: crate::api::schema::Request,
            ) -> Result<crate::api::schema::SuccessResponse, String> {
                let result = match request.method {
                    crate::api::schema::Method::RemoteList(_) => {
                        self.requests.push("remote.list");
                        crate::api::schema::ResponseResult::RemoteList {
                            remotes: self.remotes.clone(),
                        }
                    }
                    crate::api::schema::Method::WorkspaceList(_) => {
                        self.requests.push("workspace.list");
                        crate::api::schema::ResponseResult::WorkspaceList {
                            workspaces: Vec::new(),
                        }
                    }
                    crate::api::schema::Method::AgentList(_) => {
                        self.requests.push("agent.list");
                        crate::api::schema::ResponseResult::AgentList { agents: Vec::new() }
                    }
                    crate::api::schema::Method::ServerUiSettings(_) => {
                        self.requests.push("server.ui_settings");
                        crate::api::schema::ResponseResult::UiSettings {
                            settings: crate::api::schema::UiSettingsInfo::default(),
                        }
                    }
                    other => return Err(format!("unexpected method: {other:?}")),
                };

                Ok(crate::api::schema::SuccessResponse {
                    id: request.id,
                    result,
                })
            }
        }

        #[derive(Default)]
        struct RemoteAddApi {
            captured: Option<crate::api::schema::RemoteAddParams>,
        }

        impl supervisor::SupervisorApi for RemoteAddApi {
            fn request(
                &mut self,
                request: crate::api::schema::Request,
            ) -> Result<crate::api::schema::SuccessResponse, String> {
                match request.method {
                    crate::api::schema::Method::RemoteAdd(params) => {
                        self.captured = Some(params);
                        Ok(crate::api::schema::SuccessResponse {
                            id: request.id,
                            result: crate::api::schema::ResponseResult::RemoteAdded {
                                remote: crate::remote_registry::RemoteDefinitionSnapshot {
                                    id: "remote-1".into(),
                                    name: "dev".into(),
                                    target: crate::remote_registry::RemoteTargetSnapshot::Local {
                                        session: Some("dev".into()),
                                    },
                                    session: None,
                                    keybindings:
                                        crate::remote_registry::RemoteKeybindingsSnapshot::Local,
                                    disabled: false,
                                },
                            },
                        })
                    }
                    other => Err(format!("unexpected method: {other:?}")),
                }
            }
        }

        #[test]
        fn submit_remote_add_to_main_api_builds_remote_add_request() {
            let mut api = RemoteAddApi::default();

            let remote = submit_remote_add_to_main_api(
                &mut api,
                supervisor::AddRemoteDraft {
                    target: "local:dev".into(),
                    name: Some("dev".into()),
                    keybindings: crate::remote_registry::RemoteKeybindingsSnapshot::Local,
                    restart_incompatible: false,
                },
            )
            .unwrap();

            assert_eq!(remote.id, "remote-1");
            assert_eq!(
                api.captured,
                Some(crate::api::schema::RemoteAddParams {
                    name: Some("dev".into()),
                    target: "local:dev".into(),
                    keybindings: crate::remote_registry::RemoteKeybindingsSnapshot::Local,
                })
            );
        }

        #[test]
        fn add_remote_error_message_maps_registry_duplicates_for_modal() {
            assert_eq!(
                add_remote_error_message("remote target already exists"),
                "remote already added"
            );
            assert_eq!(
                add_remote_error_message("remote name already exists"),
                "name already used"
            );
            // An error that matches no bridge-failure heuristic passes through verbatim.
            assert_eq!(
                add_remote_error_message("some unmapped failure"),
                "some unmapped failure"
            );
        }

        #[test]
        fn validate_add_remote_target_rejects_local_protocol_mismatch() {
            let err = validate_add_remote_target(
                crate::api::client::ConnectionTarget::LocalSession(Some("dev".into())),
                |_| {
                    Ok(crate::api::RuntimeStatus {
                        version: Some("0.6.0".into()),
                        protocol: Some(crate::protocol::PROTOCOL_VERSION - 1),
                        capabilities: None,
                    })
                },
            )
            .unwrap_err();

            assert!(err.contains("protocol mismatch"));
            assert!(err.contains(&crate::protocol::PROTOCOL_VERSION.to_string()));
        }

        #[test]
        fn validate_add_remote_target_accepts_ssh_bridge_api_socket() {
            let err = validate_add_remote_target(
                crate::api::client::ConnectionTarget::SocketPath(std::path::PathBuf::from(
                    "/tmp/herdr-prod-api.sock",
                )),
                |_| {
                    Ok(crate::api::RuntimeStatus {
                        version: Some("0.6.0".into()),
                        protocol: Some(crate::protocol::PROTOCOL_VERSION),
                        capabilities: None,
                    })
                },
            );

            assert_eq!(err, Ok(()));
        }

        #[test]
        fn validate_add_remote_target_retries_transient_bridge_timeout() {
            let mut attempts = 0;

            let err = validate_add_remote_target(
                crate::api::client::ConnectionTarget::SocketPath(std::path::PathBuf::from(
                    "/tmp/herdr-prod-api.sock",
                )),
                |_| {
                    attempts += 1;
                    if attempts == 1 {
                        return Err("Resource temporarily unavailable (os error 35)".to_string());
                    }
                    Ok(crate::api::RuntimeStatus {
                        version: Some("0.6.0".into()),
                        protocol: Some(crate::protocol::PROTOCOL_VERSION),
                        capabilities: None,
                    })
                },
            );

            assert_eq!(err, Ok(()));
            assert_eq!(attempts, 2);
        }

        #[test]
        fn add_remote_target_rejects_active_local_session_as_duplicate_main() {
            let _guard = env_lock().lock().unwrap();
            let _session_env = EnvVarGuard::set(crate::session::SESSION_ENV_VAR, "dev");
            let _remote_env =
                EnvVarsRemovedGuard::new(&[crate::remote::MAIN_REMOTE_TARGET_ENV_VAR]);

            let target = crate::remote_registry::RemoteTargetSnapshot::Local {
                session: Some("dev".into()),
            };

            assert_eq!(
                reject_duplicate_main_target(&target),
                Err("remote already added".to_string())
            );
        }

        #[test]
        fn add_remote_target_rejects_main_remote_target_from_launch_env() {
            let _guard = env_lock().lock().unwrap();
            let _remote_env = EnvVarGuard::set(crate::remote::MAIN_REMOTE_TARGET_ENV_VAR, "iq-64");

            let target = crate::remote_registry::RemoteTargetSnapshot::Ssh {
                target: "iq-64".into(),
                args: Vec::new(),
            };

            assert_eq!(
                reject_duplicate_main_target(&target),
                Err("remote already added".to_string())
            );
        }

        #[test]
        fn summary_refresh_subscription_request_covers_sidebar_summary_events() {
            let request = summary_refresh_subscription_request("client:summary-events");

            assert_eq!(request.id, "client:summary-events");
            let crate::api::schema::Method::EventsSubscribe(params) = request.method else {
                panic!("expected events.subscribe request");
            };
            assert_eq!(
                params.subscriptions,
                vec![
                    crate::api::schema::Subscription::WorkspaceCreated {},
                    crate::api::schema::Subscription::WorkspaceUpdated {},
                    crate::api::schema::Subscription::WorkspaceRenamed {},
                    crate::api::schema::Subscription::WorkspaceClosed {},
                    crate::api::schema::Subscription::WorkspaceFocused {},
                    crate::api::schema::Subscription::TabCreated {},
                    crate::api::schema::Subscription::TabClosed {},
                    crate::api::schema::Subscription::TabFocused {},
                    crate::api::schema::Subscription::TabRenamed {},
                    crate::api::schema::Subscription::PaneCreated {},
                    crate::api::schema::Subscription::PaneClosed {},
                    crate::api::schema::Subscription::PaneFocused {},
                    crate::api::schema::Subscription::PaneExited {},
                    crate::api::schema::Subscription::PaneAgentDetected {},
                    crate::api::schema::Subscription::PaneAgentStatusChanged {
                        pane_id: None,
                        agent_status: None,
                    },
                ]
            );
        }

        #[test]
        fn full_app_client_bootstraps_supervisor_from_main_api() {
            let mut api = BootstrapApi::default();

            let model = bootstrap_supervisor_for_client(false, &mut api)
                .unwrap()
                .expect("full app client should bootstrap supervisor");

            assert_eq!(
                api.requests,
                vec![
                    "remote.list",
                    "workspace.list",
                    "agent.list",
                    "server.ui_settings"
                ]
            );
            assert!(model.secondary_connection_plans().is_empty());
        }

        #[test]
        fn remote_launch_display_name_labels_main_filter() {
            let _guard = env_lock().lock().unwrap();
            let _display_env = EnvVarGuard::set(crate::remote::MAIN_DISPLAY_NAME_ENV_VAR, "iq-64");
            let mut api = BootstrapApi::default();

            let mut model = bootstrap_supervisor_for_client(false, &mut api)
                .unwrap()
                .expect("remote client should bootstrap supervisor");
            model.cycle_filter();

            assert_eq!(model.filter_label(), "iq-64");
        }

        #[test]
        fn client_bootstrap_leaves_secondary_summaries_for_async_refresh() {
            let mut api = BootstrapApi {
                remotes: vec![test_remote_definition("remote-dev", "dev")],
                ..BootstrapApi::default()
            };

            let model = bootstrap_client_supervisor_model(false, &mut api)
                .unwrap()
                .expect("full app client should bootstrap supervisor");

            assert_eq!(
                api.requests,
                vec![
                    "remote.list",
                    "workspace.list",
                    "agent.list",
                    "server.ui_settings"
                ]
            );
            assert_eq!(model.secondary_connection_plans().len(), 1);
        }

        #[test]
        fn direct_attach_client_skips_supervisor_bootstrap() {
            let mut api = BootstrapApi::default();

            let model = bootstrap_supervisor_for_client(true, &mut api).unwrap();

            assert!(model.is_none());
            assert!(api.requests.is_empty());
        }

        #[test]
        fn client_render_plan_uses_embedded_content_when_supervisor_is_available() {
            let model = supervisor::ClientSupervisorModel::new("local");

            let plan = client_render_plan(Some(&model), RenderEncoding::TerminalAnsi, (80, 24));

            assert_eq!(plan.surface_mode, ClientSurfaceMode::EmbeddedContent);
            assert_eq!(plan.requested_encoding, RenderEncoding::SemanticFrame);
            assert_eq!(
                plan.server_size,
                (80 - compositor::DEFAULT_SIDEBAR_WIDTH, 24)
            );
            assert!(plan.use_client_compositor);
        }

        #[test]
        fn client_render_plan_uses_full_app_when_supervisor_is_unavailable() {
            let plan = client_render_plan(None, RenderEncoding::TerminalAnsi, (80, 24));

            assert_eq!(plan.surface_mode, ClientSurfaceMode::FullApp);
            assert_eq!(plan.requested_encoding, RenderEncoding::TerminalAnsi);
            assert_eq!(plan.server_size, (80, 24));
            assert!(!plan.use_client_compositor);
        }

        #[test]
        fn client_render_plan_uses_embedded_content_with_secondary_servers() {
            let mut model = supervisor::ClientSupervisorModel::new("local");
            model.add_secondary(test_remote_definition("remote-x", "x"));

            let plan = client_render_plan(Some(&model), RenderEncoding::TerminalAnsi, (80, 24));

            assert_eq!(plan.surface_mode, ClientSurfaceMode::EmbeddedContent);
            assert_eq!(plan.requested_encoding, RenderEncoding::SemanticFrame);
            assert_eq!(
                plan.server_size,
                (80 - compositor::DEFAULT_SIDEBAR_WIDTH, 24)
            );
            assert!(plan.use_client_compositor);
        }

        fn mixed_remote_model() -> (supervisor::ClientSupervisorModel, supervisor::ServerId) {
            let mut model = supervisor::ClientSupervisorModel::new("local");
            let remote_id = model.add_secondary(test_remote_definition("x", "x"));
            model
                .set_summary(
                    &supervisor::ServerId::main(),
                    supervisor::ServerSummary {
                        workspaces: vec![supervisor::WorkspaceSummary {
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
                    supervisor::ServerSummary {
                        workspaces: vec![supervisor::WorkspaceSummary {
                            workspace_id: "remote-api".into(),
                            label: "api".into(),
                            branch: Some("feature/api".into()),
                            focused: false,
                            ..Default::default()
                        }],
                        agents: vec![supervisor::AgentSummary {
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

        /// Sweep the whole host rect for the first position resolving to a hit target matching
        /// `predicate` (render == hit_test — geometry comes from the compositor itself, so the
        /// tests stay robust to sidebar layout changes).
        fn find_hit(
            compositor: &compositor::ClientCompositor,
            model: &supervisor::ClientSupervisorModel,
            host: (u16, u16),
            predicate: impl Fn(&compositor::SidebarHitTarget) -> bool,
        ) -> (u16, u16) {
            for y in 0..host.1 {
                for x in 0..host.0 {
                    if let Some(target) = compositor.hit_test(model, x, y, host.0, host.1) {
                        if predicate(&target) {
                            return (x, y);
                        }
                    }
                }
            }
            panic!("no position resolved to the expected hit target");
        }

        // ----- #23: workspace context menu mouse round-trips ------------------------------------

        fn down(button: MouseButton, column: u16, row: u16) -> MouseEvent {
            MouseEvent {
                kind: MouseEventKind::Down(button),
                column,
                row,
                modifiers: KeyModifiers::empty(),
            }
        }

        fn remote_workspace_position(
            compositor: &compositor::ClientCompositor,
            model: &supervisor::ClientSupervisorModel,
            remote_id: &supervisor::ServerId,
            host: (u16, u16),
        ) -> (u16, u16) {
            find_hit(compositor, model, host, |target| {
                matches!(
                    target,
                    compositor::SidebarHitTarget::Workspace { server_id, workspace_id }
                        if server_id == remote_id && workspace_id == "remote-api"
                )
            })
        }

        #[test]
        fn workspace_card_right_click_opens_context_menu() {
            let (mut model, remote_id) = mixed_remote_model();
            let mut compositor = compositor::ClientCompositor::new(26);
            let host = (60u16, 24u16);
            let (col, row) = remote_workspace_position(&compositor, &model, &remote_id, host);

            let dispatch = dispatch_composited_mouse_input(
                Vec::new(),
                &mut compositor,
                &mut model,
                host,
                &down(MouseButton::Right, col, row),
            );
            assert!(matches!(dispatch, ClientInputDispatch::Redraw));
            let menu = model
                .workspace_context_menu()
                .expect("right-click opened the context menu");
            assert_eq!(menu.server_id, remote_id);
            assert_eq!(menu.workspace_id, "remote-api");
            assert_eq!(menu.label, "api", "captured the current label");
        }

        #[test]
        fn context_menu_rename_then_submit_yields_workspace_rename_request() {
            let (mut model, remote_id) = mixed_remote_model();
            let mut compositor = compositor::ClientCompositor::new(26);
            let host = (60u16, 24u16);
            let (col, row) = remote_workspace_position(&compositor, &model, &remote_id, host);
            dispatch_composited_mouse_input(
                Vec::new(),
                &mut compositor,
                &mut model,
                host,
                &down(MouseButton::Right, col, row),
            );

            // click the "rename" row (index 0) -> rename overlay opens prefilled.
            let rename_hit = dispatch_sidebar_hit_target(
                compositor::SidebarHitTarget::WorkspaceContextMenuRow { index: 0 },
                &mut model,
                &down(MouseButton::Left, 1, 1),
            );
            assert!(matches!(rename_hit, ClientInputDispatch::Redraw));
            assert!(model.rename_workspace_form().is_some());

            // submit via the button -> workspace.rename ApiRequest to the OWNING server.
            let submit = dispatch_sidebar_hit_target(
                compositor::SidebarHitTarget::RenameWorkspaceSubmit,
                &mut model,
                &down(MouseButton::Left, 1, 1),
            );
            match submit {
                ClientInputDispatch::ApiRequest {
                    server_id,
                    request,
                    refresh,
                } => {
                    assert_eq!(server_id, remote_id);
                    assert_eq!(refresh, ClientApiRefreshPolicy::Immediate);
                    match request.method {
                        crate::api::schema::Method::WorkspaceRename(params) => {
                            assert_eq!(params.workspace_id, "remote-api");
                            assert_eq!(params.label, "api");
                        }
                        other => panic!("expected WorkspaceRename, got {other:?}"),
                    }
                }
                other => panic!("expected ApiRequest, got {other:?}"),
            }
        }

        #[test]
        fn context_menu_close_confirm_yields_workspace_close_request() {
            let (mut model, remote_id) = mixed_remote_model();
            let mut compositor = compositor::ClientCompositor::new(26);
            let host = (60u16, 24u16);
            let (col, row) = remote_workspace_position(&compositor, &model, &remote_id, host);
            dispatch_composited_mouse_input(
                Vec::new(),
                &mut compositor,
                &mut model,
                host,
                &down(MouseButton::Right, col, row),
            );

            // click the "close" row (index 1) -> confirm overlay opens.
            dispatch_sidebar_hit_target(
                compositor::SidebarHitTarget::WorkspaceContextMenuRow { index: 1 },
                &mut model,
                &down(MouseButton::Left, 1, 1),
            );
            assert!(model.confirm_close_workspace().is_some());

            // confirm -> workspace.close ApiRequest to the OWNING server.
            let confirm = dispatch_sidebar_hit_target(
                compositor::SidebarHitTarget::ConfirmCloseWorkspaceConfirm,
                &mut model,
                &down(MouseButton::Left, 1, 1),
            );
            match confirm {
                ClientInputDispatch::ApiRequest {
                    server_id,
                    request,
                    refresh,
                } => {
                    assert_eq!(server_id, remote_id);
                    assert_eq!(refresh, ClientApiRefreshPolicy::Immediate);
                    match request.method {
                        crate::api::schema::Method::WorkspaceClose(target) => {
                            assert_eq!(target.workspace_id, "remote-api");
                        }
                        other => panic!("expected WorkspaceClose, got {other:?}"),
                    }
                }
                other => panic!("expected ApiRequest, got {other:?}"),
            }
        }

        #[test]
        fn context_menu_close_cancel_dismisses_without_request() {
            let (mut model, remote_id) = mixed_remote_model();
            let mut compositor = compositor::ClientCompositor::new(26);
            let host = (60u16, 24u16);
            let (col, row) = remote_workspace_position(&compositor, &model, &remote_id, host);
            dispatch_composited_mouse_input(
                Vec::new(),
                &mut compositor,
                &mut model,
                host,
                &down(MouseButton::Right, col, row),
            );
            dispatch_sidebar_hit_target(
                compositor::SidebarHitTarget::WorkspaceContextMenuRow { index: 1 },
                &mut model,
                &down(MouseButton::Left, 1, 1),
            );
            let cancel = dispatch_sidebar_hit_target(
                compositor::SidebarHitTarget::ConfirmCloseWorkspaceCancel,
                &mut model,
                &down(MouseButton::Left, 1, 1),
            );
            assert!(matches!(cancel, ClientInputDispatch::Redraw));
            assert!(model.confirm_close_workspace().is_none());
        }

        fn mixed_remote_model_with_many_workspaces(
            main_count: usize,
            remote_count: usize,
        ) -> (supervisor::ClientSupervisorModel, supervisor::ServerId) {
            let mut model = supervisor::ClientSupervisorModel::new("local");
            let remote_id = model.add_secondary(test_remote_definition("x", "x"));

            let main_workspaces = (0..main_count)
                .map(|idx| supervisor::WorkspaceSummary {
                    workspace_id: format!("main-{idx}"),
                    label: format!("main-{idx}"),
                    branch: None,
                    focused: idx == 0,
                    worktree_key: None,
                    worktree_is_linked: false,
                })
                .collect();
            model
                .set_summary(
                    &supervisor::ServerId::main(),
                    supervisor::ServerSummary {
                        workspaces: main_workspaces,
                        agents: Vec::new(),
                    },
                )
                .unwrap();

            let remote_workspaces = (0..remote_count)
                .map(|idx| supervisor::WorkspaceSummary {
                    workspace_id: format!("remote-{idx}"),
                    label: format!("remote-{idx}"),
                    branch: None,
                    focused: false,
                    worktree_key: None,
                    worktree_is_linked: false,
                })
                .collect();
            model
                .set_summary(
                    &remote_id,
                    supervisor::ServerSummary {
                        workspaces: remote_workspaces,
                        agents: Vec::new(),
                    },
                )
                .unwrap();

            (model, remote_id)
        }

        /// SGR mouse-down (button left) at 0-based `(col, row)`. SGR coords are 1-based.
        fn sgr_left_down(col: u16, row: u16) -> Vec<u8> {
            format!("\x1b[<0;{};{}M", col + 1, row + 1).into_bytes()
        }

        #[test]
        fn composited_input_clicking_filter_cycles_sidebar_filter_without_forwarding() {
            let (mut model, _) = mixed_remote_model();
            let mut compositor = compositor::ClientCompositor::new(26);
            let host = (60, 16);
            let (col, row) = find_hit(&compositor, &model, host, |target| {
                matches!(target, compositor::SidebarHitTarget::Filter)
            });

            let dispatch = dispatch_composited_input(
                sgr_left_down(col, row),
                &mut compositor,
                &mut model,
                host,
            );

            assert_eq!(dispatch, ClientInputDispatch::Redraw);
            assert_eq!(
                model.filter(),
                &supervisor::ServerFilter::Server(supervisor::ServerId::main())
            );
        }

        #[test]
        fn composited_input_clicking_workspace_returns_owner_api_request() {
            let (mut model, remote_id) = mixed_remote_model();
            let mut compositor = compositor::ClientCompositor::new(26);
            // item 2 (C3): the host banner adds a row above the remote group, so render at a
            // taller sidebar and scan for the remote workspace row (render == hit_test).
            let host_size = (60, 24);
            let (col, row) = remote_workspace_position(&compositor, &model, &remote_id, host_size);

            let dispatch = dispatch_composited_input(
                sgr_left_down(col, row),
                &mut compositor,
                &mut model,
                host_size,
            );

            assert_eq!(
                dispatch,
                ClientInputDispatch::ApiRequest {
                    server_id: remote_id.clone(),
                    refresh: ClientApiRefreshPolicy::ImmediateFocused,
                    request: Box::new(crate::api::schema::Request {
                        id: "client:workspace-focus".into(),
                        method: crate::api::schema::Method::WorkspaceFocus(
                            crate::api::schema::WorkspaceTarget {
                                workspace_id: "remote-api".into(),
                            },
                        ),
                    }),
                }
            );
            assert_eq!(model.active_server_id(), &remote_id);
        }

        #[test]
        fn composited_input_scrolls_workspace_list_before_clicking_remote_workspace() {
            let (mut model, remote_id) = mixed_remote_model_with_many_workspaces(8, 2);
            let mut compositor = compositor::ClientCompositor::new(26);
            let host_size = (60, 12);

            let find_remote_first = |compositor: &compositor::ClientCompositor,
                                     model: &supervisor::ClientSupervisorModel|
             -> Option<(u16, u16)> {
                for row in 0..host_size.1 {
                    if matches!(
                        compositor.hit_test(model, 1, row, host_size.0, host_size.1),
                        Some(compositor::SidebarHitTarget::Workspace { server_id, workspace_id })
                            if server_id == remote_id && workspace_id == "remote-0"
                    ) {
                        return Some((1, row));
                    }
                }
                None
            };

            assert!(
                find_remote_first(&compositor, &model).is_none(),
                "remote workspaces should start below the visible workspace viewport"
            );

            // item 2 (C3): the host banner adds a row to the list, so scroll until the first
            // remote workspace row becomes hit-testable.
            let mut position = None;
            for _ in 0..24 {
                position = find_remote_first(&compositor, &model);
                if position.is_some() {
                    break;
                }
                assert_eq!(
                    dispatch_composited_input(
                        b"\x1b[<65;2;3M".to_vec(),
                        &mut compositor,
                        &mut model,
                        host_size,
                    ),
                    ClientInputDispatch::Redraw
                );
            }
            let (col, row) =
                position.expect("scrolling should reveal the first remote workspace row");

            let dispatch = dispatch_composited_input(
                sgr_left_down(col, row),
                &mut compositor,
                &mut model,
                host_size,
            );

            assert_eq!(
                dispatch,
                ClientInputDispatch::ApiRequest {
                    server_id: remote_id.clone(),
                    refresh: ClientApiRefreshPolicy::ImmediateFocused,
                    request: Box::new(crate::api::schema::Request {
                        id: "client:workspace-focus".into(),
                        method: crate::api::schema::Method::WorkspaceFocus(
                            crate::api::schema::WorkspaceTarget {
                                workspace_id: "remote-0".into(),
                            },
                        ),
                    }),
                }
            );
            assert_eq!(model.active_server_id(), &remote_id);
        }

        #[test]
        fn composited_input_clicking_agent_returns_owner_api_request() {
            let (mut model, remote_id) = mixed_remote_model();
            let mut compositor = compositor::ClientCompositor::new(26);
            let host = (60, 24);
            let (col, row) = find_hit(&compositor, &model, host, |target| {
                matches!(
                    target,
                    compositor::SidebarHitTarget::Agent { agent_id, .. }
                        if agent_id == "remote-agent"
                )
            });

            let dispatch = dispatch_composited_input(
                sgr_left_down(col, row),
                &mut compositor,
                &mut model,
                host,
            );

            assert_eq!(
                dispatch,
                ClientInputDispatch::ApiRequest {
                    server_id: remote_id.clone(),
                    refresh: ClientApiRefreshPolicy::ImmediateFocused,
                    request: Box::new(crate::api::schema::Request {
                        id: "client:agent-focus".into(),
                        method: crate::api::schema::Method::AgentFocus(
                            crate::api::schema::AgentTarget {
                                target: "remote-agent".into(),
                            },
                        ),
                    }),
                }
            );
            assert_eq!(model.active_server_id(), &remote_id);
        }

        // -------------------------------------------------------------------------------------
        // #24: client-side sidebar keyboard-navigation tests. These drive raw key bytes through
        // `dispatch_composited_input` and assert the SAME client actions the mouse path produces.
        // Each writes a temp config that enables the (default-unset) sidebar-nav bindings and
        // points `Config::load()` at it via `CONFIG_PATH_ENV_VAR`, guarded by the shared config
        // env lock so the bindings the interception layer resolves are deterministic.
        // -------------------------------------------------------------------------------------

        /// #24: run `body` with `Config::load()` pointed at a temp config containing `keys_toml`.
        fn with_client_keys_config<T>(keys_toml: &str, body: impl FnOnce() -> T) -> T {
            let _guard = crate::config::test_config_env_lock().lock().unwrap();
            let path = std::env::temp_dir().join(format!(
                "herdr-client-keys-{}-{}.toml",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_nanos()
            ));
            std::fs::write(&path, keys_toml).unwrap();
            let _env = EnvVarGuard::set(crate::config::CONFIG_PATH_ENV_VAR, path.to_str().unwrap());
            let result = body();
            std::fs::remove_file(&path).ok();
            result
        }

        /// #24: feed a single bare char keypress (no modifiers) through the composited input path.
        fn press_char(
            c: char,
            compositor: &mut compositor::ClientCompositor,
            model: &mut supervisor::ClientSupervisorModel,
        ) -> ClientInputDispatch {
            dispatch_composited_input(c.to_string().into_bytes(), compositor, model, (60, 16))
        }

        // A next-workspace key (bound direct to `alt+n`) routed through
        // `dispatch_composited_input` yields an ApiRequest focusing the NEXT workspace in the
        // aggregated list — which crosses the server boundary from main to the remote, switching
        // the active server.
        #[test]
        fn next_workspace_key_focuses_next_workspace_across_server_boundary() {
            with_client_keys_config("[keys]\nnext_workspace = \"alt+n\"\n", || {
                let (mut model, remote_id) = mixed_remote_model();
                let mut compositor = compositor::ClientCompositor::new(26);
                assert_eq!(model.active_server_id(), &supervisor::ServerId::main());

                let dispatch = dispatch_composited_input(
                    b"\x1bn".to_vec(),
                    &mut compositor,
                    &mut model,
                    (60, 16),
                );

                assert_eq!(
                    dispatch,
                    ClientInputDispatch::ApiRequest {
                        server_id: remote_id.clone(),
                        refresh: ClientApiRefreshPolicy::ImmediateFocused,
                        request: Box::new(crate::api::schema::Request {
                            id: "client:workspace-focus".into(),
                            method: crate::api::schema::Method::WorkspaceFocus(
                                crate::api::schema::WorkspaceTarget {
                                    workspace_id: "remote-api".into(),
                                },
                            ),
                        }),
                    }
                );
                assert_eq!(model.active_server_id(), &remote_id);
            });
        }

        // prev-workspace symmetric: from the first (main) workspace, stepping back wraps to the
        // last (remote) workspace, crossing the server boundary.
        #[test]
        fn prev_workspace_key_wraps_to_last_workspace() {
            with_client_keys_config("[keys]\nprevious_workspace = \"alt+p\"\n", || {
                let (mut model, remote_id) = mixed_remote_model();
                let mut compositor = compositor::ClientCompositor::new(26);

                let dispatch = dispatch_composited_input(
                    b"\x1bp".to_vec(),
                    &mut compositor,
                    &mut model,
                    (60, 16),
                );

                assert_eq!(
                    dispatch,
                    ClientInputDispatch::ApiRequest {
                        server_id: remote_id.clone(),
                        refresh: ClientApiRefreshPolicy::ImmediateFocused,
                        request: Box::new(crate::api::schema::Request {
                            id: "client:workspace-focus".into(),
                            method: crate::api::schema::Method::WorkspaceFocus(
                                crate::api::schema::WorkspaceTarget {
                                    workspace_id: "remote-api".into(),
                                },
                            ),
                        }),
                    }
                );
                assert_eq!(model.active_server_id(), &remote_id);
            });
        }

        // A new-workspace key opens the picker (multi-destination → Redraw, picker overlay set).
        #[test]
        fn new_workspace_key_opens_picker() {
            with_client_keys_config("[keys]\nnew_workspace = \"alt+m\"\n", || {
                let (mut model, _) = mixed_remote_model();
                let mut compositor = compositor::ClientCompositor::new(26);
                assert!(model.new_workspace_picker().is_none());

                let dispatch = dispatch_composited_input(
                    b"\x1bm".to_vec(),
                    &mut compositor,
                    &mut model,
                    (60, 16),
                );

                assert_eq!(dispatch, ClientInputDispatch::Redraw);
                assert!(model.new_workspace_picker().is_some());
            });
        }

        // A rename key opens the #23 rename overlay for the focused workspace.
        #[test]
        fn rename_workspace_key_opens_rename_overlay_for_focused_workspace() {
            with_client_keys_config("[keys]\nrename_workspace = \"alt+r\"\n", || {
                let (mut model, _) = mixed_remote_model();
                let mut compositor = compositor::ClientCompositor::new(26);
                assert!(model.rename_workspace_form().is_none());

                let dispatch = dispatch_composited_input(
                    b"\x1br".to_vec(),
                    &mut compositor,
                    &mut model,
                    (60, 16),
                );

                assert_eq!(dispatch, ClientInputDispatch::Redraw);
                let form = model
                    .rename_workspace_form()
                    .expect("rename overlay should be open");
                // main-herdr is the focused workspace in the fixture.
                assert_eq!(form.workspace_id, "main-herdr");
            });
        }

        // A close key opens the #23 confirm-close overlay for the focused workspace.
        #[test]
        fn close_workspace_key_opens_confirm_close_overlay_for_focused_workspace() {
            with_client_keys_config("[keys]\nclose_workspace = \"alt+d\"\n", || {
                let (mut model, _) = mixed_remote_model();
                let mut compositor = compositor::ClientCompositor::new(26);
                assert!(model.confirm_close_workspace().is_none());

                let dispatch = dispatch_composited_input(
                    b"\x1bd".to_vec(),
                    &mut compositor,
                    &mut model,
                    (60, 16),
                );

                assert_eq!(dispatch, ClientInputDispatch::Redraw);
                let confirm = model
                    .confirm_close_workspace()
                    .expect("confirm-close overlay should be open");
                assert_eq!(confirm.workspace_id, "main-herdr");
            });
        }

        // A collapse-toggle key flips `from_model`'s `app.sidebar_collapsed` AND dispatches a
        // Resize at the reclaimed content width — the collapsed rail is narrow (COLLAPSED_WIDTH),
        // so every connected server must re-render at the wider content.
        #[test]
        fn collapse_toggle_key_flips_sidebar_collapsed() {
            with_client_keys_config("[keys]\ntoggle_sidebar = \"alt+b\"\n", || {
                let (mut model, _) = mixed_remote_model();
                let mut compositor = compositor::ClientCompositor::new(26);
                assert!(!compositor.sidebar_collapsed_for_test());

                let dispatch = dispatch_composited_input(
                    b"\x1bb".to_vec(),
                    &mut compositor,
                    &mut model,
                    (60, 16),
                );
                assert_eq!(
                    dispatch,
                    ClientInputDispatch::Resize {
                        cols: 60 - crate::ui::COLLAPSED_WIDTH,
                        rows: 16
                    }
                );
                assert!(compositor.sidebar_collapsed_for_test());

                // Toggling back expands to the configured width and resizes again.
                let dispatch = dispatch_composited_input(
                    b"\x1bb".to_vec(),
                    &mut compositor,
                    &mut model,
                    (60, 16),
                );
                assert_eq!(dispatch, ClientInputDispatch::Resize { cols: 34, rows: 16 });
                assert!(!compositor.sidebar_collapsed_for_test());
            });
        }

        // SAFETY: a normal/unbound key (a plain letter, NOT in prefix mode) is NOT intercepted —
        // it is Forwarded unchanged so terminal/agent input is preserved.
        #[test]
        fn unbound_plain_key_is_forwarded_not_intercepted() {
            with_client_keys_config("[keys]\nnext_workspace = \"alt+n\"\n", || {
                let (mut model, _) = mixed_remote_model();
                let mut compositor = compositor::ClientCompositor::new(26);

                // 'x' matches no sidebar-nav binding and the prefix is not armed: Forward.
                assert_eq!(
                    press_char('x', &mut compositor, &mut model),
                    ClientInputDispatch::Forward(b"x".to_vec())
                );
                // Even a bare 'n' (the RHS of the alt+n direct binding) is NOT intercepted without
                // the alt modifier — the direct chord requires its modifier.
                assert_eq!(
                    press_char('n', &mut compositor, &mut model),
                    ClientInputDispatch::Forward(b"n".to_vec())
                );
            });
        }

        // Prefix-mode gating: with a prefix-bound next-workspace (`prefix+n`), a bare 'n' is
        // Forwarded when prefix is NOT armed, and intercepted only AFTER the prefix key (ctrl+b)
        // arms the mode.
        #[test]
        fn prefix_bound_key_intercepted_only_while_prefix_armed() {
            with_client_keys_config(
                "[keys]\nprefix = \"ctrl+b\"\nnext_workspace = \"prefix+n\"\n",
                || {
                    let (mut model, remote_id) = mixed_remote_model();
                    let mut compositor = compositor::ClientCompositor::new(26);

                    // Not armed: a bare 'n' is forwarded to the terminal (never hijacked).
                    assert_eq!(
                        press_char('n', &mut compositor, &mut model),
                        ClientInputDispatch::Forward(b"n".to_vec())
                    );
                    assert!(!compositor.prefix_armed());

                    // Press the prefix key (ctrl+b == 0x02): arms prefix mode, swallowed (Redraw).
                    assert_eq!(
                        dispatch_composited_input(
                            vec![0x02],
                            &mut compositor,
                            &mut model,
                            (60, 16),
                        ),
                        ClientInputDispatch::Redraw
                    );
                    assert!(compositor.prefix_armed());

                    // Now 'n' resolves the prefix-mode next-workspace action and disarms prefix
                    // mode.
                    let dispatch = press_char('n', &mut compositor, &mut model);
                    assert!(!compositor.prefix_armed());
                    assert_eq!(
                        dispatch,
                        ClientInputDispatch::ApiRequest {
                            server_id: remote_id.clone(),
                            refresh: ClientApiRefreshPolicy::ImmediateFocused,
                            request: Box::new(crate::api::schema::Request {
                                id: "client:workspace-focus".into(),
                                method: crate::api::schema::Method::WorkspaceFocus(
                                    crate::api::schema::WorkspaceTarget {
                                        workspace_id: "remote-api".into(),
                                    },
                                ),
                            }),
                        }
                    );
                },
            );
        }

        // A prefix chord with no client-side sidebar binding must NOT be swallowed: the stashed
        // prefix bytes plus the key are replayed to the server, so server-side prefix bindings
        // (splits, tabs, zoom, copy mode, …) keep working from the composited client.
        #[test]
        fn unmatched_prefix_chord_replays_prefix_and_key_to_server() {
            with_client_keys_config(
                "[keys]\nprefix = \"ctrl+b\"\nnext_workspace = \"prefix+n\"\n",
                || {
                    let (mut model, _) = mixed_remote_model();
                    let mut compositor = compositor::ClientCompositor::new(26);

                    // Arm prefix mode (ctrl+b == 0x02): swallowed while the chord is undecided.
                    assert_eq!(
                        dispatch_composited_input(
                            vec![0x02],
                            &mut compositor,
                            &mut model,
                            (60, 16),
                        ),
                        ClientInputDispatch::Redraw
                    );
                    assert!(compositor.prefix_armed());

                    // '%' matches no sidebar binding: the server owns this chord. Both the
                    // prefix byte and the key must arrive, in order.
                    assert_eq!(
                        press_char('%', &mut compositor, &mut model),
                        ClientInputDispatch::Forward(b"\x02%".to_vec())
                    );
                    assert!(!compositor.prefix_armed());
                },
            );
        }

        // Esc and a repeated prefix press cancel client prefix mode without leaking bytes to
        // the server, matching the server's own prefix-mode escape behavior.
        #[test]
        fn prefix_cancel_keys_are_swallowed() {
            with_client_keys_config("[keys]\nprefix = \"ctrl+b\"\n", || {
                let (mut model, _) = mixed_remote_model();
                let mut compositor = compositor::ClientCompositor::new(26);

                for cancel in [b"\x1b".to_vec(), vec![0x02]] {
                    assert_eq!(
                        dispatch_composited_input(
                            vec![0x02],
                            &mut compositor,
                            &mut model,
                            (60, 16),
                        ),
                        ClientInputDispatch::Redraw
                    );
                    assert!(compositor.prefix_armed());
                    assert_eq!(
                        dispatch_composited_input(cancel, &mut compositor, &mut model, (60, 16)),
                        ClientInputDispatch::Redraw
                    );
                    assert!(!compositor.prefix_armed());
                }
            });
        }

        // A dangling prefix followed by non-key input (paste burst) still reaches the server
        // with the prefix bytes replayed in front.
        #[test]
        fn dangling_prefix_replays_before_multi_event_input() {
            with_client_keys_config("[keys]\nprefix = \"ctrl+b\"\n", || {
                let (mut model, _) = mixed_remote_model();
                let mut compositor = compositor::ClientCompositor::new(26);

                assert_eq!(
                    dispatch_composited_input(vec![0x02], &mut compositor, &mut model, (60, 16)),
                    ClientInputDispatch::Redraw
                );
                assert!(compositor.prefix_armed());

                // A multi-key burst (e.g. paste) is not a single Key event: forward it with the
                // prefix bytes prepended.
                assert_eq!(
                    dispatch_composited_input(
                        b"hello".to_vec(),
                        &mut compositor,
                        &mut model,
                        (60, 16),
                    ),
                    ClientInputDispatch::Forward(b"\x02hello".to_vec())
                );
                assert!(!compositor.prefix_armed());
            });
        }

        // item 6 (Area 6): the focus dispatch emits ImmediateFocused (a server-switching focus).
        #[test]
        fn focus_dispatch_uses_immediate_focused_policy() {
            let (mut model, remote_id) = mixed_remote_model();
            let mouse = MouseEvent {
                kind: MouseEventKind::Down(MouseButton::Left),
                column: 0,
                row: 0,
                modifiers: KeyModifiers::NONE,
            };

            let workspace_dispatch = dispatch_sidebar_hit_target(
                compositor::SidebarHitTarget::Workspace {
                    server_id: remote_id.clone(),
                    workspace_id: "remote-api".into(),
                },
                &mut model,
                &mouse,
            );
            assert!(matches!(
                workspace_dispatch,
                ClientInputDispatch::ApiRequest {
                    refresh: ClientApiRefreshPolicy::ImmediateFocused,
                    ..
                }
            ));

            // A second model so the agent focus is also a server switch (active starts at main).
            let (mut model, remote_id) = mixed_remote_model();
            let agent_dispatch = dispatch_sidebar_hit_target(
                compositor::SidebarHitTarget::Agent {
                    server_id: remote_id.clone(),
                    agent_id: "remote-agent".into(),
                },
                &mut model,
                &mouse,
            );
            assert!(matches!(
                agent_dispatch,
                ClientInputDispatch::ApiRequest {
                    refresh: ClientApiRefreshPolicy::ImmediateFocused,
                    ..
                }
            ));
        }

        #[test]
        fn single_secondary_summary_refresh_dedupes_pending() {
            let (model, remote_id) = mixed_remote_model();
            let ssh_bridges = HashMap::new();
            let (event_tx, mut event_rx) = tokio::sync::mpsc::channel(8);
            let mut pending = HashSet::new();
            let mut queued = HashSet::new();
            pending.insert(remote_id.clone());

            start_single_secondary_summary_refresh(
                &model,
                &remote_id,
                &ssh_bridges,
                &mut pending,
                &mut queued,
                &event_tx,
            );

            // Already pending: no second worker spawned, pending unchanged — but the
            // signal is QUEUED for a rerun when the in-flight fetch completes.
            assert_eq!(pending.len(), 1);
            assert!(pending.contains(&remote_id));
            assert!(queued.contains(&remote_id));
            assert!(event_rx.try_recv().is_err());
        }

        #[test]
        fn main_supervisor_refresh_dedupes_pending_and_queues_rerun() {
            // A change signal that lands while a main fetch is already in flight must
            // queue exactly one rerun instead of spawning a second fetch thread —
            // under `herdr --remote` each main fetch is a WAN round-trip bundle.
            let (event_tx, mut event_rx) = tokio::sync::mpsc::channel(8);
            let mut pending = HashSet::new();
            let mut queued = HashSet::new();
            pending.insert(supervisor::ServerId::main());

            start_main_supervisor_refresh(&mut pending, &mut queued, &event_tx);

            assert_eq!(pending.len(), 1);
            assert!(pending.contains(&supervisor::ServerId::main()));
            assert!(queued.contains(&supervisor::ServerId::main()));
            assert!(event_rx.try_recv().is_err(), "no second fetch spawned");
        }

        #[test]
        fn single_secondary_summary_refresh_skips_main_id() {
            let (model, _remote_id) = mixed_remote_model();
            let ssh_bridges = HashMap::new();
            let (event_tx, mut event_rx) = tokio::sync::mpsc::channel(8);
            let mut pending = HashSet::new();
            let mut queued = HashSet::new();

            start_single_secondary_summary_refresh(
                &model,
                &supervisor::ServerId::main(),
                &ssh_bridges,
                &mut pending,
                &mut queued,
                &event_tx,
            );

            // Main id is a no-op inside the helper: nothing spawned, pending stays empty.
            assert!(pending.is_empty());
            assert!(event_rx.try_recv().is_err());
        }

        #[test]
        fn single_secondary_summary_refresh_targets_one_server() {
            let mut model = supervisor::ClientSupervisorModel::new("local");
            let id_a = model.add_secondary(test_remote_definition("a", "a"));
            let id_b = model.add_secondary(test_remote_definition("b", "b"));
            let ssh_bridges = HashMap::new();
            let (event_tx, mut event_rx) = tokio::sync::mpsc::channel(8);
            let mut pending = HashSet::new();
            let mut queued = HashSet::new();

            start_single_secondary_summary_refresh(
                &model,
                &id_b,
                &ssh_bridges,
                &mut pending,
                &mut queued,
                &event_tx,
            );

            // Only id_b is in flight (a single id in, a single fetch out — targeted, not fleet).
            assert!(pending.contains(&id_b));
            assert!(!pending.contains(&id_a));
            assert_eq!(pending.len(), 1);

            // The worker thread enqueues exactly one SupervisorSummaryFetched for id_b.
            let event = event_rx.blocking_recv().unwrap();
            match event {
                ClientLoopEvent::SupervisorSummaryFetched { server_id, .. } => {
                    assert_eq!(server_id, id_b);
                }
                _ => panic!("expected a single SupervisorSummaryFetched for id_b"),
            }
            assert!(event_rx.try_recv().is_err());
        }

        #[test]
        fn due_secondary_summary_refreshes_uses_fast_cadence_for_active() {
            let mut model = supervisor::ClientSupervisorModel::new("local");
            let active = model.add_secondary(test_remote_definition("active", "active"));
            let background = model.add_secondary(test_remote_definition("bg", "bg"));
            model.set_active_server(active.clone()).unwrap();
            let mut state = test_client_state_with_model(model);

            let now = Instant::now();
            let stale = now - Duration::from_millis(500);
            state.last_summary_refresh.insert(active.clone(), stale);
            state.last_summary_refresh.insert(background.clone(), stale);

            let due = due_secondary_summary_refreshes(&state, now);
            // Active remote (500ms old) is due at the 400ms fast cadence; the background remote
            // (500ms old) is NOT due at the 2s background cadence.
            assert!(due.contains(&active));
            assert!(!due.contains(&background));
        }

        #[test]
        fn due_secondary_summary_refreshes_returns_background_after_slow_interval() {
            let mut model = supervisor::ClientSupervisorModel::new("local");
            let active = model.add_secondary(test_remote_definition("active", "active"));
            let background = model.add_secondary(test_remote_definition("bg", "bg"));
            model.set_active_server(active.clone()).unwrap();
            let mut state = test_client_state_with_model(model);

            let now = Instant::now();
            // Background just past the 2s background interval; active just refreshed (not due).
            state
                .last_summary_refresh
                .insert(active.clone(), now - Duration::from_millis(10));
            state.last_summary_refresh.insert(
                background.clone(),
                now - CLIENT_SUPERVISOR_REFRESH_INTERVAL - Duration::from_millis(1),
            );

            let due = due_secondary_summary_refreshes(&state, now);
            assert!(due.contains(&background));
            assert!(!due.contains(&active));
            // Main never appears in the result.
            assert!(!due.contains(&supervisor::ServerId::main()));
        }

        #[test]
        fn timer_issues_no_inline_blocking_secondary_fetch() {
            // Structural guard: feeding due secondaries into the spawn helper enqueues a
            // background SupervisorSummaryFetched (worker thread) and returns within the 60fps
            // budget — proving no synchronous SSH call is on the loop.
            let mut model = supervisor::ClientSupervisorModel::new("local");
            let remote_id = model.add_secondary(test_remote_definition("slow", "slow"));
            let mut state = test_client_state_with_model(model);
            // Force the secondary due immediately (no prior refresh).
            let now = Instant::now();
            let ssh_bridges = HashMap::new();
            let (event_tx, mut event_rx) = tokio::sync::mpsc::channel(8);

            let due = due_secondary_summary_refreshes(&state, now);
            assert!(due.contains(&remote_id));

            let started_at = Instant::now();
            if let Some(model) = &state.supervisor_model {
                for server_id in &due {
                    start_single_secondary_summary_refresh(
                        model,
                        server_id,
                        &ssh_bridges,
                        &mut state.pending_summary_refresh_server_ids,
                        &mut state.queued_summary_refresh_server_ids,
                        &event_tx,
                    );
                    state.last_summary_refresh.insert(server_id.clone(), now);
                }
            }
            let elapsed = started_at.elapsed();

            assert!(
                elapsed <= CLIENT_60FPS_FRAME_BUDGET,
                "the Timer's secondary fan-out blocked the UI thread for {elapsed:?}, about {:.1} fps",
                fps_for_frame_duration(elapsed)
            );
            assert!(state
                .pending_summary_refresh_server_ids
                .contains(&remote_id));
            // The fetch happens on the worker thread (off the loop): the event arrives later.
            let event = event_rx.blocking_recv().unwrap();
            assert!(matches!(
                event,
                ClientLoopEvent::SupervisorSummaryFetched { server_id, .. } if server_id == remote_id
            ));
        }

        #[test]
        fn supervisor_summary_changed_refreshes_only_that_server() {
            // The SupervisorSummaryChanged handler routes a secondary id through the
            // single-server helper (the targeted event-push), never the whole-fleet refresh — so
            // only the changed server lands in `pending`.
            let mut model = supervisor::ClientSupervisorModel::new("local");
            let changed = model.add_secondary(test_remote_definition("changed", "changed"));
            let other = model.add_secondary(test_remote_definition("other", "other"));
            let ssh_bridges = HashMap::new();
            let (event_tx, mut event_rx) = tokio::sync::mpsc::channel(8);
            let mut pending = HashSet::new();
            let mut queued = HashSet::new();

            // Mirror the handler's secondary branch exactly.
            start_single_secondary_summary_refresh(
                &model,
                &changed,
                &ssh_bridges,
                &mut pending,
                &mut queued,
                &event_tx,
            );

            assert!(pending.contains(&changed));
            assert!(!pending.contains(&other));
            assert!(!pending.contains(&supervisor::ServerId::main()));

            let event = event_rx.blocking_recv().unwrap();
            assert!(matches!(
                event,
                ClientLoopEvent::SupervisorSummaryFetched { server_id, .. } if server_id == changed
            ));
            assert!(event_rx.try_recv().is_err());
        }

        #[test]
        fn connect_prioritizes_connected_server_refresh() {
            // On connect, the just-connected server's summary is put in flight by the handler's
            // EXPLICIT server_id (connecting does NOT change active_server_id, which stays at
            // main). So prioritization keys off the connected id, not active_server_id().
            let mut model = supervisor::ClientSupervisorModel::new("local");
            let connected = model.add_secondary(test_remote_definition("connected", "connected"));
            // active_server_id remains main after a connect (set_connection_state(.., Connected)
            // does not touch it) — assert that so the test pins the "not active_server_id" rule.
            assert_eq!(model.active_server_id(), &supervisor::ServerId::main());
            let ssh_bridges = HashMap::new();
            let (event_tx, mut event_rx) = tokio::sync::mpsc::channel(8);
            let mut pending = HashSet::new();
            let mut queued = HashSet::new();

            // Mirror the connect handler's prioritized single-server fetch.
            start_single_secondary_summary_refresh(
                &model,
                &connected,
                &ssh_bridges,
                &mut pending,
                &mut queued,
                &event_tx,
            );

            assert!(pending.contains(&connected));
            let event = event_rx.blocking_recv().unwrap();
            assert!(matches!(
                event,
                ClientLoopEvent::SupervisorSummaryFetched { server_id, .. } if server_id == connected
            ));
        }

        #[test]
        fn composited_input_translates_content_mouse_to_embedded_viewport() {
            let (mut model, _) = mixed_remote_model();
            let mut compositor = compositor::ClientCompositor::new(26);

            let dispatch = dispatch_composited_input(
                b"\x1b[<0;28;3M".to_vec(),
                &mut compositor,
                &mut model,
                (60, 16),
            );

            assert_eq!(
                dispatch,
                ClientInputDispatch::Forward(b"\x1b[<0;2;3M".to_vec())
            );
        }

        // item 7 (Area 4): an SGR no-button motion report (`\x1b[<35;col;rowM`, drag-bit set,
        // button code 3) parses to `MouseEventKind::Moved`. 1-based escape over 0-based (col,row).
        fn moved_bytes(col: u16, row: u16) -> Vec<u8> {
            format!("\x1b[<35;{};{}M", col + 1, row + 1).into_bytes()
        }

        #[test]
        fn composited_moved_sets_hover_and_redraws_then_coalesces() {
            let (mut model, remote_id) = mixed_remote_model();
            let mut compositor = compositor::ClientCompositor::new(26);
            let host = (60u16, 24u16);
            let (col, row) = remote_workspace_position(&compositor, &model, &remote_id, host);

            // first motion over the row → Redraw (hover changed from None).
            assert_eq!(
                dispatch_composited_input(moved_bytes(col, row), &mut compositor, &mut model, host),
                ClientInputDispatch::Redraw
            );
            assert!(matches!(
                compositor.hover(),
                Some(crate::app::state::SidebarHoverTarget::Workspace { .. })
            ));
            // a second identical motion → Consumed (change-detection coalescing, zero redraw).
            assert_eq!(
                dispatch_composited_input(moved_bytes(col, row), &mut compositor, &mut model, host),
                ClientInputDispatch::Consumed
            );
        }

        #[test]
        fn composited_moved_off_sidebar_clears_hover_once() {
            let (mut model, remote_id) = mixed_remote_model();
            let mut compositor = compositor::ClientCompositor::new(26);
            let host = (60u16, 24u16);
            let (col, row) = remote_workspace_position(&compositor, &model, &remote_id, host);

            // establish a sidebar hover.
            assert_eq!(
                dispatch_composited_input(moved_bytes(col, row), &mut compositor, &mut model, host),
                ClientInputDispatch::Redraw
            );
            // motion into the content area clears the hover → exactly one Redraw.
            assert_eq!(
                dispatch_composited_input(moved_bytes(40, 3), &mut compositor, &mut model, host),
                ClientInputDispatch::Redraw
            );
            assert_eq!(compositor.hover(), None);
            // a second content motion (no prior hover) is NOT intercepted: it falls through to
            // translate_content_mouse_input, which maps Moved → the original bytes (Forward).
            assert_eq!(
                dispatch_composited_input(moved_bytes(40, 3), &mut compositor, &mut model, host),
                ClientInputDispatch::Forward(moved_bytes(40, 3))
            );
        }

        #[test]
        fn hover_never_produces_server_traffic() {
            // a client Moved over a workspace OR agent row only ever returns Redraw/Consumed —
            // never ApiRequest/ServerControl/AddRemote/SetRemoteEnabled/DeleteRemote.
            let (mut model, _remote_id) = mixed_remote_model();
            let mut compositor = compositor::ClientCompositor::new(26);
            let host = (60u16, 24u16);

            // sweep every sidebar row with a motion; none may produce traffic.
            for row in 0..host.1 {
                let dispatch = dispatch_composited_input(
                    moved_bytes(1, row),
                    &mut compositor,
                    &mut model,
                    host,
                );
                assert!(
                    matches!(
                        dispatch,
                        ClientInputDispatch::Redraw | ClientInputDispatch::Consumed
                    ),
                    "hover motion produced non-hover dispatch {dispatch:?} at row {row}"
                );
            }
            assert_eq!(model.active_server_id(), &supervisor::ServerId::main());
        }

        #[test]
        fn composited_input_clicking_new_with_single_destination_returns_create_request() {
            let (mut model, _) = mixed_remote_model();
            model.set_filter(supervisor::ServerFilter::Server(
                supervisor::ServerId::main(),
            ));
            let mut compositor = compositor::ClientCompositor::new(26);
            let host = (60, 16);
            let (col, row) = find_hit(&compositor, &model, host, |target| {
                matches!(target, compositor::SidebarHitTarget::New)
            });

            let dispatch = dispatch_composited_input(
                sgr_left_down(col, row),
                &mut compositor,
                &mut model,
                host,
            );

            assert_eq!(
                dispatch,
                ClientInputDispatch::ApiRequest {
                    server_id: supervisor::ServerId::main(),
                    refresh: ClientApiRefreshPolicy::Immediate,
                    request: Box::new(crate::api::schema::Request {
                        id: "client:workspace-create".into(),
                        method: crate::api::schema::Method::WorkspaceCreate(
                            crate::api::schema::WorkspaceCreateParams {
                                cwd: None,
                                focus: true,
                                label: None,
                                env: std::collections::HashMap::new(),
                            },
                        ),
                    }),
                }
            );
        }

        #[test]
        fn composited_input_clicking_new_with_multiple_destinations_opens_picker() {
            let (mut model, _) = mixed_remote_model();
            let mut compositor = compositor::ClientCompositor::new(26);
            let host = (60, 16);
            let (col, row) = find_hit(&compositor, &model, host, |target| {
                matches!(target, compositor::SidebarHitTarget::New)
            });

            let dispatch = dispatch_composited_input(
                sgr_left_down(col, row),
                &mut compositor,
                &mut model,
                host,
            );

            assert_eq!(dispatch, ClientInputDispatch::Redraw);
            assert_eq!(
                model
                    .new_workspace_picker_destinations()
                    .map(|items| items.len()),
                Some(2)
            );
        }

        #[test]
        fn composited_input_clicking_picker_destination_returns_create_request() {
            let (mut model, remote_id) = mixed_remote_model();
            model.open_new_workspace_picker();
            let mut compositor = compositor::ClientCompositor::new(26);

            // item 1: click the FOOTER-ANCHORED remote destination row (index 1), using the same
            // shared geometry + anchor_area the renderer/hit-test use (the popup floats over the
            // live content at the sidebar footer, not centered).
            let anchor = compositor.overlay_anchor_area(&model, 60, 20);
            let inner = crate::ui::new_workspace_picker_inner_rect(anchor, 2).expect("modal fits");
            let row1 = crate::ui::new_workspace_picker_row_rect(inner, 1);
            assert!(row1.y > 0);

            let dispatch = dispatch_composited_input(
                sgr_left_down(row1.x, row1.y),
                &mut compositor,
                &mut model,
                (60, 20),
            );

            assert_eq!(
                dispatch,
                ClientInputDispatch::ApiRequest {
                    server_id: remote_id.clone(),
                    refresh: ClientApiRefreshPolicy::Immediate,
                    request: Box::new(crate::api::schema::Request {
                        id: "client:workspace-create".into(),
                        method: crate::api::schema::Method::WorkspaceCreate(
                            crate::api::schema::WorkspaceCreateParams {
                                cwd: None,
                                focus: true,
                                label: None,
                                env: std::collections::HashMap::new(),
                            },
                        ),
                    }),
                }
            );
            assert_eq!(model.active_server_id(), &remote_id);
            assert_eq!(model.new_workspace_picker_destinations(), None);
        }

        #[test]
        fn composited_input_picker_keyboard_navigates_and_confirms() {
            let (mut model, remote_id) = mixed_remote_model();
            model.open_new_workspace_picker();
            let mut compositor = compositor::ClientCompositor::new(26);

            // ↓ moves the highlight onto the remote (index 1).
            let nav = dispatch_composited_input(
                b"\x1b[B".to_vec(),
                &mut compositor,
                &mut model,
                (60, 16),
            );
            assert_eq!(nav, ClientInputDispatch::Redraw);
            assert_eq!(model.new_workspace_picker().map(|p| p.selected), Some(1));

            // Enter confirms the highlighted destination → create on the remote.
            let confirm =
                dispatch_composited_input(b"\r".to_vec(), &mut compositor, &mut model, (60, 16));
            assert_eq!(
                confirm,
                ClientInputDispatch::ApiRequest {
                    server_id: remote_id.clone(),
                    refresh: ClientApiRefreshPolicy::Immediate,
                    request: Box::new(crate::api::schema::Request {
                        id: "client:workspace-create".into(),
                        method: crate::api::schema::Method::WorkspaceCreate(
                            crate::api::schema::WorkspaceCreateParams {
                                cwd: None,
                                focus: true,
                                label: None,
                                env: std::collections::HashMap::new(),
                            },
                        ),
                    }),
                }
            );
            assert_eq!(model.active_server_id(), &remote_id);
            assert!(model.new_workspace_picker().is_none());
        }

        #[test]
        fn composited_input_picker_esc_closes() {
            let (mut model, _) = mixed_remote_model();
            model.open_new_workspace_picker();
            let mut compositor = compositor::ClientCompositor::new(26);

            let dispatch =
                dispatch_composited_input(b"\x1b".to_vec(), &mut compositor, &mut model, (60, 16));

            assert_eq!(dispatch, ClientInputDispatch::Redraw);
            assert!(model.new_workspace_picker().is_none());
        }

        #[test]
        fn composited_input_clicking_menu_opens_client_global_menu() {
            let (mut model, _) = mixed_remote_model();
            let mut compositor = compositor::ClientCompositor::new(26);
            let host = (60, 16);
            let (col, row) = find_hit(&compositor, &model, host, |target| {
                matches!(target, compositor::SidebarHitTarget::Menu)
            });

            let dispatch = dispatch_composited_input(
                sgr_left_down(col, row),
                &mut compositor,
                &mut model,
                host,
            );

            assert_eq!(dispatch, ClientInputDispatch::Redraw);
            assert_eq!(model.client_global_menu_highlighted(), Some(0));
        }

        /// The position of the open client global menu's item `index` (render == hit_test).
        fn global_menu_item_position(
            compositor: &compositor::ClientCompositor,
            model: &supervisor::ClientSupervisorModel,
            host: (u16, u16),
            index: usize,
        ) -> (u16, u16) {
            find_hit(compositor, model, host, |target| {
                matches!(
                    target,
                    compositor::SidebarHitTarget::ClientGlobalMenuItem { index: hit }
                        if *hit == index
                )
            })
        }

        #[test]
        fn composited_moved_over_open_global_menu_moves_highlight() {
            // item 7: motion over the open client menu moves the highlight to the hovered row
            // (mirrors the monolithic host) and repaints; identical motion coalesces; motion off
            // the menu leaves the highlight put. The menu stays open throughout.
            let (mut model, _) = mixed_remote_model();
            model.open_client_global_menu();
            assert_eq!(model.client_global_menu_highlighted(), Some(0));
            let mut compositor = compositor::ClientCompositor::new(26);
            let host = (60u16, 16u16);
            let (col1, row1) = global_menu_item_position(&compositor, &model, host, 1);
            let (col2, row2) = global_menu_item_position(&compositor, &model, host, 2);

            // motion onto menu row index 1 moves the highlight 0 → 1 and repaints.
            assert_eq!(
                dispatch_composited_input(
                    moved_bytes(col1, row1),
                    &mut compositor,
                    &mut model,
                    host
                ),
                ClientInputDispatch::Redraw
            );
            assert_eq!(model.client_global_menu_highlighted(), Some(1));
            // a second identical motion is coalesced (no change) → Consumed.
            assert_eq!(
                dispatch_composited_input(
                    moved_bytes(col1, row1),
                    &mut compositor,
                    &mut model,
                    host
                ),
                ClientInputDispatch::Consumed
            );
            // motion onto row index 2 moves the highlight 1 → 2.
            assert_eq!(
                dispatch_composited_input(
                    moved_bytes(col2, row2),
                    &mut compositor,
                    &mut model,
                    host
                ),
                ClientInputDispatch::Redraw
            );
            assert_eq!(model.client_global_menu_highlighted(), Some(2));
            // motion off the right-anchored menu (far-left column) leaves the highlight put.
            assert_eq!(
                dispatch_composited_input(moved_bytes(1, row1), &mut compositor, &mut model, host),
                ClientInputDispatch::Consumed
            );
            assert_eq!(model.client_global_menu_highlighted(), Some(2));
        }

        #[test]
        fn composited_input_clicking_client_global_menu_dispatches_server_actions() {
            let (mut model, _) = mixed_remote_model();
            model.open_client_global_menu();
            let mut compositor = compositor::ClientCompositor::new(26);
            let host = (60, 16);

            let click_item = |index: usize,
                              compositor: &mut compositor::ClientCompositor,
                              model: &mut supervisor::ClientSupervisorModel|
             -> ClientInputDispatch {
                let (col, row) = global_menu_item_position(compositor, model, host, index);
                dispatch_composited_input(sgr_left_down(col, row), compositor, model, host)
            };

            let settings = click_item(0, &mut compositor, &mut model);
            assert_eq!(
                settings,
                ClientInputDispatch::ServerControl {
                    server_id: supervisor::ServerId::main(),
                    message: ClientMessage::OpenSettings,
                }
            );

            model.open_client_global_menu();
            let keybinds = click_item(1, &mut compositor, &mut model);
            assert_eq!(
                keybinds,
                ClientInputDispatch::ServerControl {
                    server_id: supervisor::ServerId::main(),
                    message: ClientMessage::OpenKeybindHelp,
                }
            );

            model.open_client_global_menu();
            let reload = click_item(2, &mut compositor, &mut model);
            assert_eq!(
                reload,
                ClientInputDispatch::ApiRequest {
                    server_id: supervisor::ServerId::main(),
                    refresh: ClientApiRefreshPolicy::Immediate,
                    request: Box::new(crate::api::schema::Request {
                        id: "client:reload-config".into(),
                        method: crate::api::schema::Method::ServerReloadConfig(
                            crate::api::schema::EmptyParams::default(),
                        ),
                    }),
                }
            );

            model.open_client_global_menu();
            let detach = click_item(3, &mut compositor, &mut model);
            assert_eq!(detach, ClientInputDispatch::DetachAll);
        }

        #[test]
        fn composited_global_menu_settings_targets_and_activates_main_when_remote_is_active() {
            let (mut model, remote_id) = mixed_remote_model();
            model
                .focus_workspace_route(&remote_id, "remote-api")
                .api_request("client:workspace-focus")
                .unwrap();
            assert_eq!(model.active_server_id(), &remote_id);

            model.open_client_global_menu();
            let mut compositor = compositor::ClientCompositor::new(26);
            let host = (60, 16);
            let (col, row) = global_menu_item_position(&compositor, &model, host, 0);
            let dispatch = dispatch_composited_input(
                sgr_left_down(col, row),
                &mut compositor,
                &mut model,
                host,
            );

            assert_eq!(
                dispatch,
                ClientInputDispatch::ServerControl {
                    server_id: supervisor::ServerId::main(),
                    message: ClientMessage::OpenSettings,
                }
            );
            assert_eq!(model.active_server_id(), &supervisor::ServerId::main());
        }

        #[test]
        fn composited_input_dragging_sidebar_divider_resizes_content() {
            let (mut model, _) = mixed_remote_model();
            let mut compositor = compositor::ClientCompositor::new(26);

            assert_eq!(
                dispatch_composited_input(
                    b"\x1b[<0;26;5M".to_vec(),
                    &mut compositor,
                    &mut model,
                    (80, 24),
                ),
                ClientInputDispatch::Redraw
            );
            assert_eq!(
                dispatch_composited_input(
                    b"\x1b[<32;31;5M".to_vec(),
                    &mut compositor,
                    &mut model,
                    (80, 24),
                ),
                ClientInputDispatch::Resize { cols: 49, rows: 24 }
            );
            assert_eq!(compositor.sidebar_width(), 31);
        }

        #[test]
        fn composited_client_keeps_mouse_capture_enabled_for_sidebar() {
            assert!(desired_mouse_capture(false, true));
            assert!(desired_mouse_capture(true, true));
            assert!(desired_mouse_capture(true, false));
            assert!(!desired_mouse_capture(false, false));
        }

        #[test]
        fn composited_input_add_remote_form_submits_draft() {
            let (mut model, _) = mixed_remote_model();
            model.open_add_remote_form();
            let mut compositor = compositor::ClientCompositor::new(12);

            assert_eq!(
                dispatch_composited_input(
                    b"local:dev".to_vec(),
                    &mut compositor,
                    &mut model,
                    (24, 8)
                ),
                ClientInputDispatch::Redraw
            );
            assert_eq!(
                dispatch_composited_input(b"\tdev".to_vec(), &mut compositor, &mut model, (24, 8)),
                ClientInputDispatch::Redraw
            );

            let dispatch =
                dispatch_composited_input(b"\r".to_vec(), &mut compositor, &mut model, (24, 8));

            assert_eq!(
                dispatch,
                ClientInputDispatch::AddRemote(supervisor::AddRemoteDraft {
                    target: "local:dev".into(),
                    name: Some("dev".into()),
                    keybindings: crate::remote_registry::RemoteKeybindingsSnapshot::Local,
                    restart_incompatible: false,
                })
            );
        }

        #[test]
        fn api_target_for_supervisor_server_maps_main_and_local_secondary() {
            let (model, remote_id) = mixed_remote_model();

            assert_eq!(
                api_target_for_supervisor_server(
                    &model,
                    &supervisor::ServerId::main(),
                    &HashMap::new()
                ),
                Some(crate::api::client::ConnectionTarget::LocalSession(None))
            );
            assert_eq!(
                api_target_for_supervisor_server(&model, &remote_id, &HashMap::new()),
                Some(crate::api::client::ConnectionTarget::LocalSession(Some(
                    "x".into()
                )))
            );
        }

        #[test]
        fn supervisor_targets_map_ssh_secondary_through_bridge_sockets() {
            let mut model = supervisor::ClientSupervisorModel::new("local");
            let remote_id = model.add_secondary(crate::remote_registry::RemoteDefinitionSnapshot {
                id: "remote-prod".into(),
                name: "prod".into(),
                target: crate::remote_registry::RemoteTargetSnapshot::Ssh {
                    target: "prod.example.com".into(),
                    args: Vec::new(),
                },
                session: None,
                keybindings: crate::remote_registry::RemoteKeybindingsSnapshot::Local,
                disabled: false,
            });
            let api_socket = std::path::PathBuf::from("/tmp/herdr-prod-api.sock");
            let client_socket = std::path::PathBuf::from("/tmp/herdr-prod-client.sock");
            let bridge = crate::remote::RemoteBridge::from_socket_paths_for_test(
                client_socket.clone(),
                api_socket.clone(),
            );
            let ssh_bridges = HashMap::from([(remote_id.clone(), bridge)]);

            assert_eq!(
                api_target_for_supervisor_server(&model, &remote_id, &ssh_bridges),
                Some(crate::api::client::ConnectionTarget::SocketPath(api_socket))
            );
            assert_eq!(
                client_socket_path_for_supervisor_server(&model, &remote_id, &ssh_bridges),
                Some(client_socket)
            );
        }

        #[test]
        fn client_supervisor_request_allows_ssh_bridge_latency() {
            let socket_dir = std::env::temp_dir().join(format!(
                "herdr-delayed-api-{}-{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|duration| duration.as_nanos())
                    .unwrap_or(0)
            ));
            std::fs::create_dir_all(&socket_dir).unwrap();
            let api_socket = socket_dir.join("api.sock");
            let client_socket = socket_dir.join("client.sock");
            let listener = std::os::unix::net::UnixListener::bind(&api_socket).unwrap();

            let api_thread = std::thread::spawn(move || {
                let (mut stream, _addr) = listener.accept().unwrap();
                let mut reader = std::io::BufReader::new(stream.try_clone().unwrap());
                let mut request_line = String::new();
                std::io::BufRead::read_line(&mut reader, &mut request_line).unwrap();
                assert!(request_line.contains("\"ping\""));
                std::thread::sleep(Duration::from_millis(750));
                let _ = writeln!(
                    stream,
                    "{{\"id\":\"delayed\",\"result\":{{\"type\":\"pong\",\"version\":\"0.6.4\",\"protocol\":{}}}}}",
                    PROTOCOL_VERSION
                );
            });

            let mut model = supervisor::ClientSupervisorModel::new("local");
            let remote_id = model.add_secondary(crate::remote_registry::RemoteDefinitionSnapshot {
                id: "remote-prod".into(),
                name: "prod".into(),
                target: crate::remote_registry::RemoteTargetSnapshot::Ssh {
                    target: "prod.example.com".into(),
                    args: Vec::new(),
                },
                session: None,
                keybindings: crate::remote_registry::RemoteKeybindingsSnapshot::Local,
                disabled: false,
            });
            let bridge =
                crate::remote::RemoteBridge::from_socket_paths_for_test(client_socket, api_socket);
            let ssh_bridges = HashMap::from([(remote_id.clone(), bridge)]);
            let request = crate::api::schema::Request {
                id: "delayed".into(),
                method: crate::api::schema::Method::Ping(crate::api::schema::PingParams::default()),
            };

            let result = send_client_supervisor_request(&model, &remote_id, request, &ssh_bridges);

            api_thread.join().unwrap();
            std::fs::remove_dir_all(&socket_dir).unwrap();
            assert!(
                result.is_ok(),
                "SSH bridge API requests should tolerate sub-second remote latency: {result:?}"
            );
        }

        #[test]
        fn secondary_summary_refresh_returns_within_sixty_fps_budget_when_remote_is_slow() {
            let socket_dir = std::path::PathBuf::from("/tmp").join(format!(
                "hsum-{}-{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|duration| duration.as_nanos())
                    .unwrap_or(0)
            ));
            std::fs::create_dir_all(&socket_dir).unwrap();
            let api_socket = socket_dir.join("api.sock");
            let client_socket = socket_dir.join("client.sock");
            let listener = std::os::unix::net::UnixListener::bind(&api_socket).unwrap();

            let api_thread = std::thread::spawn(move || {
                let (mut stream, _addr) = listener.accept().unwrap();
                let mut reader = std::io::BufReader::new(stream.try_clone().unwrap());
                let mut request_line = String::new();
                std::io::BufRead::read_line(&mut reader, &mut request_line).unwrap();
                assert!(request_line.contains("\"ping\""));
                std::thread::sleep(Duration::from_millis(750));
                let _ = writeln!(
                    stream,
                    "{{\"id\":\"client-supervisor:status\",\"result\":{{\"type\":\"pong\",\"version\":\"0.6.4\",\"protocol\":{}}}}}",
                    PROTOCOL_VERSION
                );
            });

            let mut model = supervisor::ClientSupervisorModel::new("local");
            let remote_id = model.add_secondary(crate::remote_registry::RemoteDefinitionSnapshot {
                id: "remote-prod".into(),
                name: "prod".into(),
                target: crate::remote_registry::RemoteTargetSnapshot::Ssh {
                    target: "prod.example.com".into(),
                    args: Vec::new(),
                },
                session: None,
                keybindings: crate::remote_registry::RemoteKeybindingsSnapshot::Local,
                disabled: false,
            });
            let bridge =
                crate::remote::RemoteBridge::from_socket_paths_for_test(client_socket, api_socket);
            let ssh_bridges = HashMap::from([(remote_id.clone(), bridge)]);
            let (event_tx, mut event_rx) = tokio::sync::mpsc::channel(8);
            let mut pending = HashSet::new();

            let started_at = Instant::now();
            start_secondary_supervisor_summary_refreshes(
                &model,
                &ssh_bridges,
                &mut pending,
                &event_tx,
            );
            let elapsed = started_at.elapsed();

            assert!(
                elapsed <= CLIENT_60FPS_FRAME_BUDGET,
                "starting a slow remote summary refresh blocked the UI thread for {elapsed:?}, about {:.1} fps",
                fps_for_frame_duration(elapsed)
            );
            assert!(pending.contains(&remote_id));
            assert!(event_rx.try_recv().is_err());

            let event = event_rx.blocking_recv().unwrap();
            match event {
                ClientLoopEvent::SupervisorSummaryFetched { server_id, .. } => {
                    assert_eq!(server_id, remote_id);
                }
                _ => panic!("expected async summary result"),
            }

            api_thread.join().unwrap();
            std::fs::remove_dir_all(&socket_dir).unwrap();
        }

        #[test]
        fn client_supervisor_api_request_returns_within_sixty_fps_budget_when_remote_is_slow() {
            let socket_dir = std::path::PathBuf::from("/tmp").join(format!(
                "hact-{}-{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|duration| duration.as_nanos())
                    .unwrap_or(0)
            ));
            std::fs::create_dir_all(&socket_dir).unwrap();
            let api_socket = socket_dir.join("api.sock");
            let client_socket = socket_dir.join("client.sock");
            let listener = std::os::unix::net::UnixListener::bind(&api_socket).unwrap();

            let api_thread = std::thread::spawn(move || {
                let (mut stream, _addr) = listener.accept().unwrap();
                let mut reader = std::io::BufReader::new(stream.try_clone().unwrap());
                let mut request_line = String::new();
                std::io::BufRead::read_line(&mut reader, &mut request_line).unwrap();
                assert!(request_line.contains("\"workspace.focus\""));
                std::thread::sleep(Duration::from_millis(750));
                let _ = writeln!(
                    stream,
                    "{{\"id\":\"client:workspace-focus\",\"result\":{{\"type\":\"ok\"}}}}"
                );
            });

            let mut model = supervisor::ClientSupervisorModel::new("local");
            let remote_id = model.add_secondary(crate::remote_registry::RemoteDefinitionSnapshot {
                id: "remote-prod".into(),
                name: "prod".into(),
                target: crate::remote_registry::RemoteTargetSnapshot::Ssh {
                    target: "prod.example.com".into(),
                    args: Vec::new(),
                },
                session: None,
                keybindings: crate::remote_registry::RemoteKeybindingsSnapshot::Local,
                disabled: false,
            });
            let bridge =
                crate::remote::RemoteBridge::from_socket_paths_for_test(client_socket, api_socket);
            let ssh_bridges = HashMap::from([(remote_id.clone(), bridge)]);
            let (event_tx, mut event_rx) = tokio::sync::mpsc::channel(8);
            let request = crate::api::schema::Request {
                id: "client:workspace-focus".into(),
                method: crate::api::schema::Method::WorkspaceFocus(
                    crate::api::schema::WorkspaceTarget {
                        workspace_id: "remote-api".into(),
                    },
                ),
            };

            let started_at = Instant::now();
            let result = spawn_client_supervisor_request(
                &model,
                remote_id.clone(),
                ClientApiRefreshPolicy::Deferred,
                request,
                &ssh_bridges,
                &event_tx,
            );
            let elapsed = started_at.elapsed();

            assert!(result.is_ok());
            assert!(
                elapsed <= CLIENT_60FPS_FRAME_BUDGET,
                "starting a slow remote API action blocked the UI thread for {elapsed:?}, about {:.1} fps",
                fps_for_frame_duration(elapsed)
            );
            assert!(event_rx.try_recv().is_err());

            let event = event_rx.blocking_recv().unwrap();
            match event {
                ClientLoopEvent::SupervisorApiRequestFinished {
                    server_id, result, ..
                } => {
                    assert_eq!(server_id, remote_id);
                    assert!(result.is_ok());
                }
                _ => panic!("expected async API result"),
            }

            api_thread.join().unwrap();
            std::fs::remove_dir_all(&socket_dir).unwrap();
        }

        #[test]
        fn secondary_connection_retry_returns_within_sixty_fps_budget_when_handshake_is_slow() {
            let _guard = env_lock().lock().unwrap();
            let config_home = std::path::PathBuf::from("/tmp").join(format!(
                "hcfg-{}-{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|duration| duration.as_nanos())
                    .unwrap_or(0)
            ));
            let _config_env = EnvVarGuard::set("XDG_CONFIG_HOME", config_home.to_str().unwrap());
            let client_socket = crate::session::client_socket_path_for(Some("slow"));
            std::fs::create_dir_all(client_socket.parent().unwrap()).unwrap();
            let listener = std::os::unix::net::UnixListener::bind(&client_socket).unwrap();

            let server_thread = std::thread::spawn(move || {
                let (mut stream, _addr) = listener.accept().unwrap();
                std::thread::sleep(Duration::from_millis(750));
                protocol::write_message(
                    &mut stream,
                    &ServerMessage::Welcome {
                        version: PROTOCOL_VERSION,
                        encoding: RenderEncoding::SemanticFrame,
                        error: None,
                    },
                )
                .unwrap();
                std::thread::sleep(Duration::from_millis(250));
            });

            let mut model = supervisor::ClientSupervisorModel::new("local");
            let remote_id = model.add_secondary(test_remote_definition("slow", "slow"));
            let mut state = test_client_state_with_model(model);
            let now = Instant::now();
            state.secondary_retries.insert(
                remote_id.clone(),
                SecondaryRetryState {
                    attempt: 0,
                    next_retry_at: now,
                },
            );
            let (event_tx, mut event_rx) = tokio::sync::mpsc::channel(8);
            let mut server_writes = HashMap::new();

            let started_at = Instant::now();
            retry_due_secondary_connections(&mut state, now, &event_tx, &mut server_writes);
            let elapsed = started_at.elapsed();

            assert!(
                elapsed <= CLIENT_60FPS_FRAME_BUDGET,
                "starting a slow secondary reconnect blocked the UI thread for {elapsed:?}, about {:.1} fps",
                fps_for_frame_duration(elapsed)
            );
            assert!(state
                .pending_secondary_connect_server_ids
                .contains(&remote_id));
            assert!(event_rx.try_recv().is_err());

            let event = event_rx.blocking_recv().unwrap();
            match event {
                ClientLoopEvent::SecondaryConnectionAttemptFinished {
                    server_id, result, ..
                } => {
                    assert_eq!(server_id, remote_id);
                    if let Err(err) = &result {
                        panic!(
                            "secondary reconnect should complete after the delayed handshake: {err:?}"
                        );
                    }
                }
                _ => panic!("expected async secondary connection result"),
            }

            server_thread.join().unwrap();
            std::fs::remove_file(client_socket).ok();
            std::fs::remove_dir_all(config_home).ok();
        }

        #[test]
        fn add_remote_submission_returns_within_sixty_fps_budget_when_remote_is_slow() {
            let _guard = env_lock().lock().unwrap();
            let config_home = std::path::PathBuf::from("/tmp").join(format!(
                "hadd-{}-{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|duration| duration.as_nanos())
                    .unwrap_or(0)
            ));
            let _config_env = EnvVarGuard::set("XDG_CONFIG_HOME", config_home.to_str().unwrap());
            let _session_env = EnvVarsRemovedGuard::new(&[
                crate::session::SESSION_ENV_VAR,
                crate::api::SOCKET_PATH_ENV_VAR,
                crate::server::socket_paths::CLIENT_SOCKET_PATH_ENV_VAR,
                crate::remote::MAIN_REMOTE_TARGET_ENV_VAR,
            ]);
            let session_api_socket = crate::session::api_socket_path_for(Some("slowadd"));
            let session_client_socket = crate::session::client_socket_path_for(Some("slowadd"));
            let main_api_socket = crate::api::socket_path();
            std::fs::create_dir_all(session_api_socket.parent().unwrap()).unwrap();
            std::fs::create_dir_all(main_api_socket.parent().unwrap()).unwrap();
            let session_api_listener =
                std::os::unix::net::UnixListener::bind(&session_api_socket).unwrap();
            let session_client_listener =
                std::os::unix::net::UnixListener::bind(&session_client_socket).unwrap();
            let main_api_listener =
                std::os::unix::net::UnixListener::bind(&main_api_socket).unwrap();

            let session_api_thread = std::thread::spawn(move || {
                let (mut stream, _addr) = session_api_listener.accept().unwrap();
                let mut reader = std::io::BufReader::new(stream.try_clone().unwrap());
                let mut request_line = String::new();
                std::io::BufRead::read_line(&mut reader, &mut request_line).unwrap();
                assert!(request_line.contains("\"ping\""));
                std::thread::sleep(Duration::from_millis(750));
                let _ = writeln!(
                    stream,
                    "{{\"id\":\"client-supervisor:status\",\"result\":{{\"type\":\"pong\",\"version\":\"0.6.4\",\"protocol\":{}}}}}",
                    PROTOCOL_VERSION
                );
            });
            let session_client_thread = std::thread::spawn(move || {
                let (mut stream, _addr) = session_client_listener.accept().unwrap();
                let _hello: ClientMessage =
                    protocol::read_message(&mut stream, MAX_FRAME_SIZE).unwrap();
                protocol::write_message(
                    &mut stream,
                    &ServerMessage::Welcome {
                        version: PROTOCOL_VERSION,
                        encoding: RenderEncoding::SemanticFrame,
                        error: None,
                    },
                )
                .unwrap();
                std::thread::sleep(Duration::from_millis(250));
            });
            let main_api_thread = std::thread::spawn(move || {
                let (mut stream, _addr) = main_api_listener.accept().unwrap();
                let mut reader = std::io::BufReader::new(stream.try_clone().unwrap());
                let mut request_line = String::new();
                std::io::BufRead::read_line(&mut reader, &mut request_line).unwrap();
                assert!(request_line.contains("\"remote.add\""));
                let _ = writeln!(
                    stream,
                    "{{\"id\":\"client:remote-add\",\"result\":{{\"type\":\"remote_added\",\"remote\":{{\"id\":\"remote-slowadd\",\"name\":\"slowadd\",\"target\":{{\"type\":\"local\",\"session\":\"slowadd\"}},\"keybindings\":\"local\"}}}}}}"
                );
            });

            let draft = supervisor::AddRemoteDraft {
                target: "local:slowadd".into(),
                name: Some("slowadd".into()),
                keybindings: crate::remote_registry::RemoteKeybindingsSnapshot::Local,
                restart_incompatible: false,
            };
            let (event_tx, mut event_rx) = tokio::sync::mpsc::channel(8);
            let mut pending_add_remote = false;

            let started_at = Instant::now();
            spawn_client_add_remote_submission(
                draft,
                (80, 24),
                (0, 0),
                &event_tx,
                &mut pending_add_remote,
            );
            let elapsed = started_at.elapsed();

            assert!(pending_add_remote);
            assert!(
                elapsed <= CLIENT_60FPS_FRAME_BUDGET,
                "starting a slow add-remote submission blocked the UI thread for {elapsed:?}, about {:.1} fps",
                fps_for_frame_duration(elapsed)
            );
            assert!(event_rx.try_recv().is_err());

            let event = event_rx.blocking_recv().unwrap();
            match event {
                ClientLoopEvent::AddRemoteFinished { result, .. } => match result {
                    Ok(_) => {}
                    Err(err) => panic!("add-remote should succeed: {err:?}"),
                },
                _ => panic!("expected async add-remote result"),
            }

            session_api_thread.join().unwrap();
            session_client_thread.join().unwrap();
            main_api_thread.join().unwrap();
            std::fs::remove_dir_all(config_home).ok();
        }

        #[test]
        fn server_writer_queue_returns_within_sixty_fps_budget_when_socket_write_is_slow() {
            // A connected local-socket pair (interprocess streams), like
            // `server::client_transport`'s test helper.
            let path = std::env::temp_dir().join(format!(
                "hwrite-{}-{}.sock",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|duration| duration.as_nanos())
                    .unwrap_or(0)
            ));
            let _ = std::fs::remove_file(&path);
            use interprocess::local_socket::traits::Listener as _;
            let listener = crate::ipc::bind_local_listener(&path).unwrap();
            let client_stream = crate::ipc::connect_local_stream(&path).unwrap();
            let server_stream = listener.accept().unwrap();

            let (event_tx, _event_rx) = tokio::sync::mpsc::channel(8);
            let handle = spawn_server_writer(supervisor::ServerId::main(), client_stream, event_tx);
            let large_message = ClientMessage::ClipboardImage {
                extension: "png".into(),
                data: vec![7; MAX_CLIPBOARD_IMAGE_PAYLOAD],
            };

            queue_to_server(&handle, large_message).unwrap();
            std::thread::sleep(Duration::from_millis(20));

            let started_at = Instant::now();
            queue_to_server(
                &handle,
                ClientMessage::Input {
                    data: b"x".to_vec(),
                },
            )
            .unwrap();
            let elapsed = started_at.elapsed();

            assert!(
                elapsed <= CLIENT_60FPS_FRAME_BUDGET,
                "queueing while a server writer is blocked took {elapsed:?}, about {:.1} fps",
                fps_for_frame_duration(elapsed)
            );
            drop(server_stream);
            let _ = std::fs::remove_file(&path);
        }

        #[test]
        fn frame_stats_calculate_render_fps_from_frame_duration() {
            let mut stats = ClientFrameStats::default();
            let sample = stats.record_render_duration(Duration::from_micros(16_667));

            assert!((sample.render_fps - 60.0).abs() < 0.1);
            assert_eq!(sample.render_duration, Duration::from_micros(16_667));
            assert!(!sample.missed_sixty_fps_budget);

            let slow = stats.record_render_duration(Duration::from_millis(25));
            assert!(slow.render_fps < 60.0);
            assert!(slow.missed_sixty_fps_budget);
        }

        #[test]
        fn frame_stats_use_stable_fps_for_zero_duration_frames() {
            let mut stats = ClientFrameStats::default();
            let sample = stats.record_render_duration(Duration::ZERO);

            assert_eq!(sample.render_fps, f64::INFINITY);
            assert!(!sample.missed_sixty_fps_budget);
        }

        #[test]
        fn supervisor_summary_refresh_due_uses_two_second_interval() {
            let start = Instant::now();

            assert!(!supervisor_summary_refresh_due(
                start + Duration::from_millis(1999),
                start
            ));
            assert!(supervisor_summary_refresh_due(
                start + Duration::from_secs(2),
                start
            ));
        }

        #[test]
        fn secondary_retry_delay_uses_conservative_backoff_schedule() {
            assert_eq!(secondary_retry_delay(0), Duration::from_secs(1));
            assert_eq!(secondary_retry_delay(1), Duration::from_secs(2));
            assert_eq!(secondary_retry_delay(2), Duration::from_secs(5));
            assert_eq!(secondary_retry_delay(3), Duration::from_secs(15));
            assert_eq!(secondary_retry_delay(8), Duration::from_secs(15));
        }

        #[test]
        fn client_socket_path_for_connection_target_maps_local_sessions_only() {
            let _guard = env_lock().lock().unwrap();
            let _env = EnvVarsRemovedGuard::new(&[
                crate::server::socket_paths::CLIENT_SOCKET_PATH_ENV_VAR,
                crate::session::SESSION_ENV_VAR,
            ]);
            let named = client_socket_path_for_connection_target(
                &supervisor::ServerConnectionTarget::LocalSession(Some("work".into())),
            )
            .unwrap();
            assert!(named.ends_with("sessions/work/herdr-client.sock"));

            let default =
                client_socket_path_for_connection_target(&supervisor::ServerConnectionTarget::Main)
                    .unwrap();
            assert!(default.ends_with("herdr-client.sock"));

            assert_eq!(
                client_socket_path_for_connection_target(
                    &supervisor::ServerConnectionTarget::Ssh {
                        destination: "host".into(),
                        options: Vec::new(),
                    }
                ),
                None
            );
        }

        fn test_frame(width: u16) -> protocol::FrameData {
            protocol::FrameData {
                cells: Vec::new(),
                width,
                height: 1,
                cursor: None,
                hyperlinks: Vec::new(),
                graphics: Vec::new(),
            }
        }

        #[test]
        fn select_composited_render_frame_requires_active_server_cache() {
            let main = supervisor::ServerId::main();
            let remote = supervisor::ServerId::secondary("remote-x");
            let mut frames = std::collections::HashMap::new();
            frames.insert(main.clone(), test_frame(10));
            frames.insert(remote.clone(), test_frame(20));

            assert_eq!(
                select_composited_render_frame(&frames, &remote, &main)
                    .unwrap()
                    .width,
                20
            );

            let missing = supervisor::ServerId::secondary("missing");
            assert_eq!(
                select_composited_render_frame(&frames, &missing, &main),
                None
            );
        }

        #[test]
        fn secondary_write_failure_disconnects_server_without_failing_client() {
            let now = Instant::now();
            let mut model = supervisor::ClientSupervisorModel::new("local");
            let remote_id = model.add_secondary(test_remote_definition("remote-x", "x"));
            model.set_active_server(remote_id.clone()).unwrap();
            let mut state = test_client_state_with_model(model);
            state.frame_cache.insert(remote_id.clone(), test_frame(8));
            state
                .summary_subscription_server_ids
                .insert(remote_id.clone());
            let mut server_writes = HashMap::new();

            let result = handle_server_write_failure(
                &mut state,
                &mut server_writes,
                remote_id.clone(),
                io::Error::new(io::ErrorKind::BrokenPipe, "secondary closed"),
                now,
            );

            assert!(result.is_ok());
            assert!(!state.frame_cache.contains_key(&remote_id));
            assert!(!state.summary_subscription_server_ids.contains(&remote_id));
            assert_eq!(
                state.supervisor_model.as_ref().unwrap().active_server_id(),
                &supervisor::ServerId::main()
            );
            assert_eq!(
                state
                    .secondary_retries
                    .get(&remote_id)
                    .map(|retry| retry.next_retry_at),
                Some(now + secondary_retry_delay(0))
            );
        }

        #[test]
        fn main_write_failure_still_fails_client() {
            let mut state =
                test_client_state_with_model(supervisor::ClientSupervisorModel::new("local"));
            let mut server_writes = HashMap::new();

            let result = handle_server_write_failure(
                &mut state,
                &mut server_writes,
                supervisor::ServerId::main(),
                io::Error::new(io::ErrorKind::BrokenPipe, "main closed"),
                Instant::now(),
            );

            assert!(matches!(result, Err(ClientError::ConnectionLost(_))));
            assert!(state.secondary_retries.is_empty());
        }

        #[test]
        fn schedule_missing_secondary_stream_retries_includes_new_connecting_servers() {
            let now = Instant::now();
            let mut model = supervisor::ClientSupervisorModel::new("local");
            model.sync_remote_registry(vec![test_remote_definition("remote-x", "x")]);
            let remote_id = supervisor::ServerId::secondary("remote-x");
            let mut state = test_client_state_with_model(model);
            let server_writes = HashMap::new();

            schedule_missing_secondary_stream_retries(&mut state, &server_writes, now);

            assert_eq!(
                state
                    .secondary_retries
                    .get(&remote_id)
                    .map(|retry| retry.next_retry_at),
                Some(now)
            );
        }

        #[test]
        fn schedule_missing_secondary_stream_retries_includes_connected_server_without_stream() {
            let now = Instant::now();
            let mut model = supervisor::ClientSupervisorModel::new("local");
            let remote_id = model.add_secondary(test_remote_definition("remote-x", "x"));
            let mut state = test_client_state_with_model(model);
            let server_writes = HashMap::new();

            schedule_missing_secondary_stream_retries(&mut state, &server_writes, now);

            assert_eq!(
                state
                    .secondary_retries
                    .get(&remote_id)
                    .map(|retry| retry.next_retry_at),
                Some(now)
            );
        }

        #[test]
        fn summary_subscription_end_guard_sends_ended_event_on_drop() {
            let server_id = supervisor::ServerId::secondary("remote-x");
            let (tx, mut rx) = tokio::sync::mpsc::channel(1);

            drop(SummarySubscriptionEndGuard {
                server_id: server_id.clone(),
                event_tx: tx,
            });

            let event = rx.blocking_recv().expect("subscription ended event");
            assert!(matches!(
                event,
                ClientLoopEvent::SupervisorSummarySubscriptionEnded(id) if id == server_id
            ));
        }

        // --- item 5: client agent animation --------------------------------------------------

        /// Mixed model with one main workspace and one remote workspace whose single agent has
        /// the given `status`. Both servers connect by default, so a "working" agent makes
        /// `sidebar_wants_animation` true.
        fn animation_model(
            status: &str,
        ) -> (supervisor::ClientSupervisorModel, supervisor::ServerId) {
            let mut model = supervisor::ClientSupervisorModel::new("local");
            let remote_id = model.add_secondary(test_remote_definition("remote-x", "x"));
            model
                .set_summary(
                    &supervisor::ServerId::main(),
                    supervisor::ServerSummary {
                        workspaces: vec![supervisor::WorkspaceSummary {
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
                    supervisor::ServerSummary {
                        workspaces: vec![supervisor::WorkspaceSummary {
                            workspace_id: "remote-api".into(),
                            label: "api".into(),
                            branch: Some("feature/api".into()),
                            focused: false,
                            ..Default::default()
                        }],
                        agents: vec![supervisor::AgentSummary {
                            agent_id: "remote-agent".into(),
                            workspace_id: "remote-api".into(),
                            label: "claude".into(),
                            status: status.into(),
                            focused: false,
                        }],
                    },
                )
                .unwrap();
            (model, remote_id)
        }

        #[test]
        fn client_animation_cadence_matches_visible_step_rate() {
            // Cadence is fixed by the contract: 80ms / step 8.
            assert_eq!(CLIENT_ANIMATION_INTERVAL, Duration::from_millis(80));
            assert_eq!(CLIENT_ANIMATION_TICK_STEP, 8);

            // `spinner_frame` maps SPINNERS[(tick/8) % len], so step 8 advances exactly one
            // visible spinner frame per interval.
            let visible_steps_per_interval = CLIENT_ANIMATION_TICK_STEP / 8;
            assert_eq!(visible_steps_per_interval, 1);
            let visible_step_period = CLIENT_ANIMATION_INTERVAL / visible_steps_per_interval;
            assert_eq!(visible_step_period, Duration::from_millis(80));

            // Within the 64..=128ms band bounded by the server (16ms * 8 = 128ms visible period)
            // and headless (128ms / (8/8) = 128ms visible period).
            assert!(visible_step_period >= Duration::from_millis(64));
            assert!(visible_step_period <= Duration::from_millis(128));
        }

        #[test]
        fn next_select_deadline_picks_min_when_active() {
            let now = Instant::now();
            let last = now - Duration::from_millis(30);

            // Active: min(now + 100ms housekeeping, last + 80ms animation). last + 80ms =
            // now + 50ms is sooner, so it wins.
            let active = next_select_deadline(now, last, true);
            assert_eq!(active, last + CLIENT_ANIMATION_INTERVAL);
            assert_eq!(active, now + Duration::from_millis(50));

            // Inactive: always the 100ms housekeeping deadline (idle behavior unchanged).
            let idle = next_select_deadline(now, last, false);
            assert_eq!(idle, now + Duration::from_millis(100));

            // Active but the animation deadline is further out than housekeeping → housekeeping
            // wins. last + 80ms must exceed now + 100ms, i.e. last is >20ms in the future.
            let far_last = now + Duration::from_millis(50);
            let active_far = next_select_deadline(now, far_last, true);
            assert_eq!(active_far, now + Duration::from_millis(100));
            assert!(far_last + CLIENT_ANIMATION_INTERVAL > now + Duration::from_millis(100));
        }

        #[test]
        fn two_timers_within_interval_advance_tick_once() {
            let t0 = Instant::now();
            // First Timer at t0: at least one interval since `last` → advance.
            assert!(should_advance_animation(
                true,
                t0,
                t0 - CLIENT_ANIMATION_INTERVAL
            ));
            // Second Timer 40ms later (< 80ms since the just-recorded t0) → no advance
            // (coalesced).
            assert!(!should_advance_animation(
                true,
                t0 + Duration::from_millis(40),
                t0
            ));
            // A Timer a full interval later → advance again.
            assert!(should_advance_animation(
                true,
                t0 + CLIENT_ANIMATION_INTERVAL,
                t0
            ));
        }

        #[test]
        fn no_tick_advance_when_idle() {
            // With no working agent — and the host banner animation forced Static so the banner
            // does not gate animation (item 2/C3) — the gate is false, so the animation step
            // never runs regardless of elapsed time → the tick stays put.
            let (mut idle_model, _) = animation_model("idle");
            let mut ui_settings = idle_model.ui_settings().clone();
            ui_settings.sidebar_host.animation = crate::config::HostBannerAnimation::Static;
            idle_model.set_ui_settings(ui_settings);
            assert!(!compositor::sidebar_wants_animation(&idle_model));
            let wants = compositor::sidebar_wants_animation(&idle_model);
            let t0 = Instant::now();
            assert!(!should_advance_animation(
                wants,
                t0 + Duration::from_secs(10),
                t0
            ));

            // And with no advance the compositor tick stays unchanged.
            let compositor = compositor::ClientCompositor::new(26);
            assert_eq!(compositor.animation_tick(), 0);
        }

        #[test]
        fn animation_step_performs_no_io() {
            // Replicate the exact component sequence of the Timer animation step and assert it
            // touches NONE of the off-UI-loop pending sets that the SSH/API helpers populate
            // (no SSH/API I/O on the UI loop).
            let (model, _) = animation_model("working");
            let mut state = test_client_state_with_model(model);
            state.compositor = Some(compositor::ClientCompositor::new(26));

            assert!(state.pending_summary_refresh_server_ids.is_empty());
            assert!(state.pending_secondary_connect_server_ids.is_empty());
            assert!(state.summary_subscription_server_ids.is_empty());

            let now = Instant::now();
            let wants = state.compositor.is_some()
                && state
                    .supervisor_model
                    .as_ref()
                    .is_some_and(compositor::sidebar_wants_animation);
            assert!(wants);
            if should_advance_animation(
                wants,
                now,
                state.last_animation_tick - CLIENT_ANIMATION_INTERVAL,
            ) {
                if let Some(compositor) = state.compositor.as_mut() {
                    compositor.advance_animation_tick(CLIENT_ANIMATION_TICK_STEP);
                }
                state.last_animation_tick = now;
                render_cached_composited_frame(&mut state);
            }

            // The tick advanced...
            assert_eq!(
                state.compositor.as_ref().unwrap().animation_tick(),
                CLIENT_ANIMATION_TICK_STEP
            );
            // ...but no SSH/API refresh or connect work was scheduled.
            assert!(state.pending_summary_refresh_server_ids.is_empty());
            assert!(state.pending_secondary_connect_server_ids.is_empty());
            assert!(state.summary_subscription_server_ids.is_empty());
        }

        // ----- item 3 (Area 5): manage loop wiring (off-UI-loop) --------------------------------

        /// A disabled secondary with a DUE retry entry (as `ServerDisconnected`'s unconditional
        /// `schedule_secondary_retry` would leave) is dropped by `retry_due_secondary_connections`
        /// before any reconnect, because the gated `secondary_connection_plans()` yields no plan.
        #[test]
        fn disabled_server_retry_entry_dropped_before_reconnect() {
            let mut model = supervisor::ClientSupervisorModel::new("local");
            // a single DISABLED secondary.
            model.sync_remote_registry(vec![{
                let mut def = test_remote_definition("r1", "alpha");
                def.disabled = true;
                def
            }]);
            let server_id = supervisor::ServerId::secondary("r1");

            let mut state = test_client_state_with_model(model);
            let now = Instant::now();
            state.secondary_retries.insert(
                server_id.clone(),
                SecondaryRetryState {
                    attempt: 0,
                    next_retry_at: now,
                },
            );

            let (event_tx, mut event_rx) = tokio::sync::mpsc::channel(8);
            let mut server_writes = HashMap::new();
            retry_due_secondary_connections(&mut state, now, &event_tx, &mut server_writes);

            // the gated plan yields nothing → the entry is removed, no connect attempt spawned.
            assert!(!state.secondary_retries.contains_key(&server_id));
            assert!(!state
                .pending_secondary_connect_server_ids
                .contains(&server_id));
            assert!(event_rx.try_recv().is_err());
        }

        /// `SetRemoteEnabled`/`DeleteRemote` dispatch targets `ServerId::main()` off the UI loop —
        /// the spawn helper returns within the frame budget and does not block on the API call.
        #[test]
        fn set_enabled_dispatch_spawns_main_request() {
            let _guard = env_lock().lock().unwrap();
            // point the local socket at a guaranteed-missing path so the spawned thread fails
            // fast.
            let _sock = EnvVarGuard::set(
                crate::api::SOCKET_PATH_ENV_VAR,
                "/tmp/herdr-nonexistent-manage-test.sock",
            );
            let (model, _) = mixed_remote_model();
            let (event_tx, mut event_rx) = tokio::sync::mpsc::channel(8);

            let started = Instant::now();
            spawn_client_remote_manage_request(
                &model,
                RemoteManageAction::SetEnabled { enabled: false },
                "x".into(),
                &HashMap::new(),
                &event_tx,
            );
            let elapsed = started.elapsed();
            // the spawn helper returns essentially immediately — the API round-trip happens on
            // the spawned thread, NOT inline on the UI loop.
            assert!(
                elapsed <= CLIENT_60FPS_FRAME_BUDGET,
                "spawning a manage request blocked the UI thread for {elapsed:?}"
            );

            // the request DOES complete off-thread and emits the finished event addressed to x.
            let event = event_rx.blocking_recv().unwrap();
            match event {
                ClientLoopEvent::RemoteManageRequestFinished {
                    action, remote_id, ..
                } => {
                    assert_eq!(action, RemoteManageAction::SetEnabled { enabled: false });
                    assert_eq!(remote_id, "x");
                }
                _ => panic!("expected RemoteManageRequestFinished"),
            }
        }

        /// Building the manage request targets the right API method.
        #[test]
        fn remote_manage_request_builds_set_enabled_and_remove() {
            let set = remote_manage_request(RemoteManageAction::SetEnabled { enabled: true }, "r1");
            match set.method {
                crate::api::schema::Method::RemoteSetEnabled(params) => {
                    assert_eq!(params.remote_id, "r1");
                    assert!(params.enabled);
                }
                other => panic!("expected remote.set_enabled, got {other:?}"),
            }
            let del = remote_manage_request(RemoteManageAction::Delete, "r1");
            match del.method {
                crate::api::schema::Method::RemoteRemove(params) => {
                    assert_eq!(params.remote_id, "r1");
                }
                other => panic!("expected remote.remove, got {other:?}"),
            }
        }

        /// The re-enable handler sets the server `connection_state == Connecting` so the gated
        /// plans pick it up on the next tick (`sync_remote_registry` never re-applies state).
        #[test]
        fn re_enable_yields_connecting() {
            let _guard = env_lock().lock().unwrap();
            let _sock = EnvVarGuard::set(
                crate::api::SOCKET_PATH_ENV_VAR,
                "/tmp/herdr-nonexistent-manage-test.sock",
            );
            let mut model = supervisor::ClientSupervisorModel::new("local");
            let server_id = model.add_secondary({
                let mut def = test_remote_definition("r1", "alpha");
                def.disabled = true;
                def
            });
            model
                .set_connection_state(&server_id, supervisor::ConnectionState::Disconnected)
                .unwrap();
            let mut state = test_client_state_with_model(model);
            let (event_tx, _rx) = tokio::sync::mpsc::channel(8);
            let mut server_writes = HashMap::new();

            apply_remote_manage_request_finished(
                &mut state,
                &mut server_writes,
                RemoteManageAction::SetEnabled { enabled: true },
                "r1",
                Ok(()),
                &event_tx,
            );

            let server = state
                .supervisor_model
                .as_ref()
                .unwrap()
                .server_for_test(&server_id)
                .unwrap();
            assert_eq!(
                server.connection_state,
                supervisor::ConnectionState::Connecting
            );
        }

        /// Disabling a currently-connected remote tears down its stream/bridge/subscription state
        /// and sets `Disconnected`.
        #[test]
        fn disable_while_connected_tears_down() {
            let _guard = env_lock().lock().unwrap();
            let _sock = EnvVarGuard::set(
                crate::api::SOCKET_PATH_ENV_VAR,
                "/tmp/herdr-nonexistent-manage-test.sock",
            );
            let mut model = supervisor::ClientSupervisorModel::new("local");
            let server_id = model.add_secondary(test_remote_definition("r1", "alpha"));
            model
                .set_connection_state(&server_id, supervisor::ConnectionState::Connected)
                .unwrap();
            let mut state = test_client_state_with_model(model);
            // seed live stream/bridge/subscription/pending state for the server.
            state
                .summary_subscription_server_ids
                .insert(server_id.clone());
            state
                .pending_summary_refresh_server_ids
                .insert(server_id.clone());
            state
                .pending_secondary_connect_server_ids
                .insert(server_id.clone());
            let (event_tx, _rx) = tokio::sync::mpsc::channel(8);
            let mut server_writes = HashMap::new();

            apply_remote_manage_request_finished(
                &mut state,
                &mut server_writes,
                RemoteManageAction::SetEnabled { enabled: false },
                "r1",
                Ok(()),
                &event_tx,
            );

            assert!(!state.summary_subscription_server_ids.contains(&server_id));
            assert!(!state
                .pending_summary_refresh_server_ids
                .contains(&server_id));
            assert!(!state
                .pending_secondary_connect_server_ids
                .contains(&server_id));
            assert!(!state.ssh_bridges.contains_key(&server_id));
            let server = state
                .supervisor_model
                .as_ref()
                .unwrap()
                .server_for_test(&server_id)
                .unwrap();
            assert_eq!(
                server.connection_state,
                supervisor::ConnectionState::Disconnected
            );
        }

        /// Deleting a remote removes the secondary from the model, tears down, and clears the
        /// overlay confirm/pending markers.
        #[test]
        fn delete_removes_secondary_and_clears_overlay() {
            let _guard = env_lock().lock().unwrap();
            let _sock = EnvVarGuard::set(
                crate::api::SOCKET_PATH_ENV_VAR,
                "/tmp/herdr-nonexistent-manage-test.sock",
            );
            let mut model = supervisor::ClientSupervisorModel::new("local");
            let server_id = model.add_secondary(test_remote_definition("r1", "alpha"));
            model.open_remote_manage_overlay();
            // enter delete-confirm + mark pending for r1 (as the dispatch would).
            model.begin_remote_manage_delete();
            assert_eq!(
                model
                    .remote_manage_overlay()
                    .unwrap()
                    .confirm_delete
                    .as_deref(),
                Some("r1")
            );
            let mut state = test_client_state_with_model(model);
            let (event_tx, _rx) = tokio::sync::mpsc::channel(8);
            let mut server_writes = HashMap::new();

            apply_remote_manage_request_finished(
                &mut state,
                &mut server_writes,
                RemoteManageAction::Delete,
                "r1",
                Ok(()),
                &event_tx,
            );

            let model = state.supervisor_model.as_ref().unwrap();
            assert!(
                model.server_for_test(&server_id).is_none(),
                "secondary removed from model"
            );
            let overlay = model.remote_manage_overlay().unwrap();
            assert!(overlay.confirm_delete.is_none());
            assert!(overlay.pending.is_none());
        }
    }
}
