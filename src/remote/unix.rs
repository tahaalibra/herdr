//! Remote thin-client launcher over SSH command stdio.

use std::collections::BTreeMap;
use std::fs::{self, File};
use std::io::{self, IsTerminal, Write as _};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

use serde::Deserialize;
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

const BRIDGE_ACCEPT_POLL: Duration = Duration::from_millis(50);
const BRIDGE_SOCKET_PERMISSION_MODE: u32 = 0o600;
const REMOTE_SERVER_SHUTDOWN_CONFIRM_TIMEOUT: Duration = Duration::from_secs(5);
const REMOTE_SERVER_SHUTDOWN_POLL_INTERVAL: Duration = Duration::from_millis(100);
const CURRENT_PROTOCOL: u32 = crate::protocol::PROTOCOL_VERSION;
const STABLE_UPDATE_MANIFEST_URL: &str = "https://herdr.dev/latest.json";
const PREVIEW_UPDATE_MANIFEST_URL: &str = "https://herdr.dev/preview.json";
const REMOTE_BINARY_ENV_VAR: &str = "HERDR_REMOTE_BINARY";
const REMOTE_BRIDGE_PROBE_ENV_VAR: &str = "HERDR_REMOTE_BRIDGE_PROBE";
const SSH_CONTROL_SOCKET_NAME: &str = "ctl";
pub(crate) const REATTACH_COMMAND_ENV_VAR: &str = "HERDR_REATTACH_COMMAND";
pub(crate) const MAIN_DISPLAY_NAME_ENV_VAR: &str = "HERDR_MAIN_DISPLAY_NAME";
pub(crate) const MAIN_REMOTE_TARGET_ENV_VAR: &str = "HERDR_MAIN_REMOTE_TARGET";

pub(crate) const REMOTE_KEYBINDINGS_ENV_VAR: &str = "HERDR_REMOTE_KEYBINDINGS";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RemoteKeybindings {
    Local,
    Server,
}

impl RemoteKeybindings {
    fn parse(value: &str) -> Result<Self, String> {
        match value {
            "local" => Ok(Self::Local),
            "server" => Ok(Self::Server),
            _ => Err("--remote-keybindings must be 'local' or 'server'".to_string()),
        }
    }

    fn as_str(self) -> &'static str {
        match self {
            Self::Local => "local",
            Self::Server => "server",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RemoteLaunch {
    pub(crate) target: String,
    pub(crate) keybindings: RemoteKeybindings,
    pub(crate) live_handoff: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RemoteBridgeKind {
    Client,
    Api,
}

impl RemoteBridgeKind {
    fn subcommand(self) -> &'static str {
        match self {
            Self::Client => "remote-client-bridge",
            Self::Api => "remote-api-bridge",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct RemoteBridgePaths {
    client_socket: PathBuf,
    api_socket: PathBuf,
}

pub(crate) struct RemoteBridge {
    client_socket: PathBuf,
    api_socket: PathBuf,
    _client_bridge: Option<SshStdioBridge>,
    _api_bridge: Option<SshStdioBridge>,
}

impl RemoteBridge {
    pub(crate) fn client_socket_path(&self) -> &Path {
        &self.client_socket
    }

    pub(crate) fn api_socket_path(&self) -> &Path {
        &self.api_socket
    }

    #[cfg(test)]
    pub(crate) fn from_socket_paths_for_test(client_socket: PathBuf, api_socket: PathBuf) -> Self {
        Self {
            client_socket,
            api_socket,
            _client_bridge: None,
            _api_bridge: None,
        }
    }
}

pub(crate) fn extract_remote_args(
    args: &[String],
) -> Result<(Vec<String>, Option<RemoteLaunch>), String> {
    let mut cleaned = Vec::with_capacity(args.len());
    if let Some(program) = args.first() {
        cleaned.push(program.clone());
    }

    let mut remote_target = None;
    let mut keybindings = RemoteKeybindings::Local;
    let mut keybindings_seen = false;
    let mut live_handoff = false;
    let mut index = 1;
    while index < args.len() {
        let arg = &args[index];
        if arg == "--" {
            cleaned.extend_from_slice(&args[index..]);
            break;
        }
        if arg == "--handoff" {
            live_handoff = true;
            index += 1;
            continue;
        }
        if arg == "--remote" {
            if remote_target.is_some() {
                return Err("--remote can only be specified once".to_string());
            }
            let Some(value) = args.get(index + 1) else {
                return Err("missing value for --remote".to_string());
            };
            remote_target = Some(validate_remote_target(value)?.to_owned());
            index += 2;
            continue;
        }
        if let Some(value) = arg.strip_prefix("--remote=") {
            if remote_target.is_some() {
                return Err("--remote can only be specified once".to_string());
            }
            remote_target = Some(validate_remote_target(value)?.to_owned());
            index += 1;
            continue;
        }
        if arg == "--remote-keybindings" {
            if keybindings_seen {
                return Err("--remote-keybindings can only be specified once".to_string());
            }
            let Some(value) = args.get(index + 1) else {
                return Err("missing value for --remote-keybindings".to_string());
            };
            keybindings = RemoteKeybindings::parse(value)?;
            keybindings_seen = true;
            index += 2;
            continue;
        }
        if let Some(value) = arg.strip_prefix("--remote-keybindings=") {
            if keybindings_seen {
                return Err("--remote-keybindings can only be specified once".to_string());
            }
            keybindings = RemoteKeybindings::parse(value)?;
            keybindings_seen = true;
            index += 1;
            continue;
        }

        cleaned.push(arg.clone());
        index += 1;
    }

    let remote = remote_target.map(|target| RemoteLaunch {
        target,
        keybindings,
        live_handoff,
    });
    if remote.is_none() && keybindings_seen {
        return Err("--remote-keybindings requires --remote".to_string());
    }
    if remote.is_none() && live_handoff {
        cleaned.push("--handoff".to_string());
    }

    Ok((cleaned, remote))
}

fn validate_remote_target(target: &str) -> Result<&str, String> {
    if target.is_empty() {
        return Err("missing value for --remote".to_string());
    }
    if target.starts_with('-') {
        return Err("--remote target must not start with '-'".to_string());
    }
    Ok(target)
}

pub(crate) fn run_remote(remote: RemoteLaunch) -> io::Result<()> {
    let session_name = crate::session::active_name()
        .unwrap_or_else(|| crate::session::DEFAULT_SESSION_NAME.to_string());
    let program = std::env::args()
        .next()
        .unwrap_or_else(|| "herdr".to_string());
    let reattach_command = reattach_command(
        &program,
        &remote.target,
        &session_name,
        remote.keybindings,
        remote.live_handoff,
    );
    // The CLI `--remote <host>` path is always a bare destination (leading-`-` is rejected by
    // `validate_remote_target`), so there are no extra ssh options to carry.
    let ssh_target = SshTarget::bare(&remote.target);
    let manage_ssh_config = crate::config::Config::load()
        .config
        .remote
        .manage_ssh_config;
    let remote_ssh = RemoteSsh::new(ssh_target, manage_ssh_config);
    let prepared_remote = prepare_remote_herdr(
        &remote_ssh,
        remote.live_handoff,
        RemotePrepPolicy::Interactive,
    )?;
    ensure_remote_server_ready(
        &remote_ssh,
        &prepared_remote.remote_herdr,
        prepared_remote.installed_or_replaced,
        prepared_remote.stop_after_install_approved,
        remote.live_handoff,
        RemotePrepPolicy::Interactive,
    )?;

    let bridge = start_ssh_remote_bridge_with_prepared(
        remote_ssh,
        &session_name,
        prepared_remote.remote_herdr,
    )?;

    run_client_process(
        bridge.client_socket_path(),
        bridge.api_socket_path(),
        &reattach_command,
        remote.keybindings,
        &remote.target,
    )
}

/// A resolved ssh connection: the destination plus any user-supplied ssh options that must
/// precede it (e.g. `-L`, `-J`, `-p`, `-o`). The destination alone is the dedup / socket-path /
/// display key; the options are emitted on every ssh invocation so port-forwards and jump hosts
/// from a full ssh add-remote spec actually take effect.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SshTarget {
    destination: String,
    options: Vec<String>,
}

impl SshTarget {
    pub(crate) fn new(destination: impl Into<String>, options: Vec<String>) -> Self {
        Self {
            destination: destination.into(),
            options,
        }
    }

    /// A bare destination with no extra ssh options (the `herdr --remote <host>` CLI path).
    pub(crate) fn bare(destination: impl Into<String>) -> Self {
        Self::new(destination, Vec::new())
    }

    pub(crate) fn destination(&self) -> &str {
        &self.destination
    }
}

/// How `prepare_remote_herdr` / `ensure_remote_server_ready` resolve the install + restart
/// decisions on a remote host.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RemotePrepPolicy {
    /// `herdr --remote` from a shell: prompt on a TTY, refuse without one.
    Interactive,
    /// The in-client add-remote worker: never read stdin (the TUI owns it in raw mode, so any
    /// `read_line` would hang invisibly). Auto-approve installing herdr on a fresh host, prefer
    /// live-handoff for an out-of-date running server, and refuse to silently hard-stop a remote
    /// server that cannot hand off — unless `restart_incompatible` is set, which means the user
    /// explicitly approved stopping an incompatible no-handoff server.
    NonInteractive { restart_incompatible: bool },
}

/// Typed signal (wrapped in an `io::Error`) that a non-interactive attach hit an incompatible
/// remote server that cannot live-handoff. The client downcasts this to show a y/N restart prompt
/// instead of a dead-end error, then retries with `restart_incompatible = true`.
#[derive(Debug, Clone)]
pub(crate) struct RestartConfirmNeeded {
    pub(crate) destination: String,
    pub(crate) version: Option<String>,
    pub(crate) protocol: Option<u32>,
}

impl std::fmt::Display for RestartConfirmNeeded {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{} runs an older herdr (v{} protocol {}) that can't live-handoff. Restart it with the updated herdr? This interrupts its running panes.",
            self.destination,
            version_label(self.version.as_deref()),
            protocol_label(self.protocol)
        )
    }
}

impl std::error::Error for RestartConfirmNeeded {}

/// If `err` carries a [`RestartConfirmNeeded`] signal, borrow it.
pub(crate) fn restart_confirm_needed(err: &io::Error) -> Option<&RestartConfirmNeeded> {
    err.get_ref()
        .and_then(|inner| inner.downcast_ref::<RestartConfirmNeeded>())
}

/// Start (or reuse) the ssh bridge for one remote so the client can attach its
/// client + API streams. The returned bridge must be kept alive for the
/// connection's lifetime.
pub(crate) fn start_ssh_remote_bridge(
    target: SshTarget,
    restart_incompatible: bool,
    session_name: Option<&str>,
) -> io::Result<RemoteBridge> {
    let session_name = session_name.unwrap_or(crate::session::DEFAULT_SESSION_NAME);
    // Client-driven attach: never block on stdin, and prefer live-handoff so an out-of-date
    // remote server is upgraded without killing its panes.
    let policy = RemotePrepPolicy::NonInteractive {
        restart_incompatible,
    };
    let manage_ssh_config = crate::config::Config::load()
        .config
        .remote
        .manage_ssh_config;
    let remote_ssh = RemoteSsh::new(target, manage_ssh_config);
    let prepared_remote = prepare_remote_herdr(&remote_ssh, true, policy)?;
    ensure_remote_server_ready(
        &remote_ssh,
        &prepared_remote.remote_herdr,
        prepared_remote.installed_or_replaced,
        prepared_remote.stop_after_install_approved,
        true,
        policy,
    )?;
    start_ssh_remote_bridge_with_prepared(remote_ssh, session_name, prepared_remote.remote_herdr)
}

fn start_ssh_remote_bridge_with_prepared(
    remote_ssh: RemoteSsh,
    session_name: &str,
    remote_herdr: RemoteHerdr,
) -> io::Result<RemoteBridge> {
    let paths = remote_bridge_socket_paths(remote_ssh.target(), session_name);
    // The bridges own the `RemoteSsh` so its managed ssh config (and control master) stays
    // alive for as long as any bridge thread can still spawn ssh connections.
    let ssh = Arc::new(remote_ssh);
    let client_bridge = SshStdioBridge::start(
        Arc::clone(&ssh),
        remote_herdr.clone(),
        paths.client_socket.clone(),
        session_name.to_string(),
        RemoteBridgeKind::Client,
    )?;
    let api_bridge = SshStdioBridge::start(
        ssh,
        remote_herdr,
        paths.api_socket.clone(),
        session_name.to_string(),
        RemoteBridgeKind::Api,
    )?;

    Ok(RemoteBridge {
        client_socket: paths.client_socket,
        api_socket: paths.api_socket,
        _client_bridge: Some(client_bridge),
        _api_bridge: Some(api_bridge),
    })
}

pub(crate) fn run_remote_client_bridge() -> io::Result<()> {
    if remote_bridge_probe_requested() {
        return Ok(());
    }

    ensure_remote_server_running()?;

    let socket_path = crate::server::socket_paths::client_socket_path();
    bridge_stdio_to_socket(&socket_path, "client")
}

pub(crate) fn run_remote_api_bridge() -> io::Result<()> {
    if remote_bridge_probe_requested() {
        return Ok(());
    }

    ensure_remote_server_running()?;

    let socket_path = crate::api::socket_path();
    bridge_stdio_to_socket(&socket_path, "API")
}

fn remote_bridge_probe_requested() -> bool {
    std::env::var_os(REMOTE_BRIDGE_PROBE_ENV_VAR).is_some()
}

fn bridge_stdio_to_socket(socket_path: &Path, label: &str) -> io::Result<()> {
    let stream = UnixStream::connect(socket_path).map_err(|err| {
        io::Error::new(
            err.kind(),
            format!(
                "failed to connect to remote Herdr {label} socket {}: {err}",
                socket_path.display()
            ),
        )
    })?;

    let mut stdout = io::stdout().lock();
    let mut socket_to_stdout = stream.try_clone()?;
    let mut stdin_to_socket = stream;

    let _upload = thread::spawn(move || {
        let mut stdin = io::stdin();
        let _ = copy_flush(&mut stdin, &mut stdin_to_socket);
        let _ = stdin_to_socket.shutdown(std::net::Shutdown::Write);
    });

    copy_flush(&mut socket_to_stdout, &mut stdout).map(|_| ())
}

fn ensure_remote_server_running() -> io::Result<()> {
    let socket_path = crate::server::socket_paths::client_socket_path();
    if crate::server::autodetect::is_server_listening() {
        let status = crate::api::read_runtime_status_at(
            &crate::api::socket_path(),
            Duration::from_millis(500),
        )?
        .ok_or_else(|| io::Error::other("remote server status API is unavailable"))?;
        if status.protocol == Some(CURRENT_PROTOCOL) {
            return Ok(());
        }
        return Err(io::Error::other(
            "remote herdr server must restart before this bridge can attach; rerun `herdr --remote` from an interactive terminal to approve stopping it",
        ));
    }

    crate::server::autodetect::spawn_server_daemon()?;
    crate::server::autodetect::wait_for_server_socket(&socket_path, Duration::from_secs(5))
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct RemotePlatform {
    os: &'static str,
    arch: &'static str,
}

impl RemotePlatform {
    fn from_uname(os: &str, arch: &str) -> Option<Self> {
        let os = match os.trim() {
            "Linux" => "linux",
            "Darwin" => "macos",
            _ => return None,
        };
        let arch = match arch.trim() {
            "x86_64" | "amd64" => "x86_64",
            "aarch64" | "arm64" => "aarch64",
            _ => return None,
        };
        Some(Self { os, arch })
    }

    fn local() -> Self {
        let os = if cfg!(target_os = "linux") {
            "linux"
        } else if cfg!(target_os = "macos") {
            "macos"
        } else {
            "unknown"
        };

        let arch = if cfg!(target_arch = "x86_64") {
            "x86_64"
        } else if cfg!(target_arch = "aarch64") {
            "aarch64"
        } else {
            "unknown"
        };

        Self { os, arch }
    }

    fn asset_key(&self) -> String {
        format!("{}-{}", self.os, self.arch)
    }
}

#[derive(Debug, Clone)]
struct RemoteHerdr {
    install_suffix: String,
    shell_path: String,
    platform: RemotePlatform,
}

impl RemoteHerdr {
    fn for_platform(platform: RemotePlatform) -> Self {
        Self::for_install_suffix(platform, ".local/bin/herdr".to_string())
    }

    fn for_install_suffix(platform: RemotePlatform, install_suffix: String) -> Self {
        let shell_path = format!("\"$HOME/{install_suffix}\"");
        Self {
            install_suffix,
            shell_path,
            platform,
        }
    }

    fn with_shell_path(mut self, shell_path: String) -> Self {
        self.shell_path = shell_path;
        self
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
enum RemoteAssetRef {
    Url(String),
    Object { url: String, sha256: Option<String> },
}

impl RemoteAssetRef {
    fn url(&self) -> &str {
        match self {
            Self::Url(url) => url,
            Self::Object { url, .. } => url,
        }
    }

    fn sha256(&self) -> Option<&str> {
        match self {
            Self::Url(_) => None,
            Self::Object { sha256, .. } => {
                sha256.as_deref().filter(|value| !value.trim().is_empty())
            }
        }
    }
}

#[derive(Deserialize)]
struct RemoteUpdateManifest {
    version: String,
    protocol: Option<u32>,
    assets: BTreeMap<String, RemoteAssetRef>,
    #[serde(default, deserialize_with = "deserialize_remote_manifest_releases")]
    releases: BTreeMap<String, RemoteReleaseMetadata>,
}

#[derive(Deserialize)]
struct RemoteReleaseMetadata {
    protocol: Option<u32>,
    #[serde(default)]
    assets: BTreeMap<String, RemoteAssetRef>,
}

#[derive(Deserialize)]
struct RemotePreviewManifest {
    build_id: String,
    protocol: u32,
    assets: BTreeMap<String, RemoteAssetRef>,
    #[serde(default)]
    builds: BTreeMap<String, RemotePreviewBuildMetadata>,
}

#[derive(Deserialize)]
struct RemotePreviewBuildMetadata {
    protocol: u32,
    assets: BTreeMap<String, RemoteAssetRef>,
}

fn deserialize_remote_manifest_releases<'de, D>(
    deserializer: D,
) -> Result<BTreeMap<String, RemoteReleaseMetadata>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = Option::<serde_json::Value>::deserialize(deserializer)?;
    Ok(match value {
        Some(serde_json::Value::Object(object)) => object
            .into_iter()
            .filter_map(|(version, release)| {
                serde_json::from_value::<RemoteReleaseMetadata>(release)
                    .ok()
                    .map(|metadata| (version, metadata))
            })
            .collect(),
        _ => BTreeMap::new(),
    })
}

impl RemoteUpdateManifest {
    fn release_for_version(&self, version: &str) -> Option<RemoteManifestReleaseRef<'_>> {
        if self.version.trim_start_matches('v') == version {
            return Some(RemoteManifestReleaseRef {
                protocol: self.protocol,
                assets: &self.assets,
            });
        }

        self.releases.get(version).and_then(|release| {
            (!release.assets.is_empty()).then_some(RemoteManifestReleaseRef {
                protocol: release.protocol,
                assets: &release.assets,
            })
        })
    }
}

#[derive(Clone, Copy)]
struct RemoteManifestReleaseRef<'a> {
    protocol: Option<u32>,
    assets: &'a BTreeMap<String, RemoteAssetRef>,
}

fn current_version() -> String {
    crate::build_info::version()
}

fn current_channel() -> &'static str {
    crate::build_info::channel()
}

struct InstallSource {
    path: PathBuf,
    temporary_dir: Option<PathBuf>,
}

struct RemoteReleaseAsset {
    url: String,
    sha256: Option<String>,
}

struct PreparedRemoteHerdr {
    remote_herdr: RemoteHerdr,
    installed_or_replaced: bool,
    stop_after_install_approved: bool,
}

#[derive(Clone)]
struct ManagedSshOptions {
    config_path: PathBuf,
    control_path: PathBuf,
}

struct ManagedSshConfig {
    options: ManagedSshOptions,
}

impl Drop for ManagedSshConfig {
    fn drop(&mut self) {
        if let Some(dir) = self.options.config_path.parent() {
            let _ = fs::remove_dir_all(dir);
        }
    }
}

struct RemoteSsh {
    target: SshTarget,
    managed_config: Option<ManagedSshConfig>,
}

impl RemoteSsh {
    fn new(target: SshTarget, manage_ssh_config: bool) -> Self {
        let managed_config = if manage_ssh_config {
            write_managed_ssh_config()
                .inspect_err(|err| {
                    tracing::debug!(%err, "could not write managed ssh config; using plain ssh");
                })
                .ok()
        } else {
            None
        };

        Self {
            target,
            managed_config,
        }
    }

    fn target(&self) -> &str {
        self.target.destination()
    }

    fn options(&self) -> Option<&ManagedSshOptions> {
        self.managed_config.as_ref().map(|config| &config.options)
    }

    /// Build `ssh <managed-config...> <user options...> -T <destination>`. `-T` (disable
    /// pseudo-tty) is inserted before the destination unless the user already supplied it; the
    /// herdr payload is always a trailing positional appended by callers so it runs on the
    /// remote rather than being parsed as an ssh option.
    fn command(&self) -> Command {
        let mut command = self.base_command();
        if !self.target.options.iter().any(|opt| opt == "-T") {
            command.arg("-T");
        }
        command.arg(self.target.destination());
        command
    }

    fn base_command(&self) -> Command {
        let mut command = Command::new("ssh");
        apply_managed_ssh_options(&mut command, self.options());
        // Bound the connect phase so an unreachable host fails fast instead of stalling for the
        // OS TCP timeout. Skip if the user already pinned a ConnectTimeout in their own options.
        if !self
            .target
            .options
            .iter()
            .any(|opt| opt.contains("ConnectTimeout"))
        {
            command.arg("-o").arg("ConnectTimeout=10");
        }
        command.args(&self.target.options);
        command
    }

    fn sh_output(&self, script: &str) -> io::Result<Output> {
        let mut child = self
            .command()
            .arg("/bin/sh -s")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()?;

        let write_result = if let Some(mut stdin) = child.stdin.take() {
            stdin.write_all(script.as_bytes())
        } else {
            Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "ssh bootstrap stdin missing",
            ))
        };
        let output = child.wait_with_output()?;
        write_result?;
        Ok(output)
    }

    fn user_shell_output(&self, command: &str) -> io::Result<Output> {
        self.command().arg(command).output()
    }

    fn install_herdr(&self, remote_herdr: &RemoteHerdr, source_path: &Path) -> io::Result<()> {
        let output = self.sh_output(&remote_install_prepare_script(remote_herdr))?;
        if !output.status.success() {
            return Err(command_failed("remote install preparation failed", &output));
        }
        let (tmp_path, dest_path) = parse_remote_install_paths(&output.stdout)?;

        let mut child = self
            .command()
            .arg(remote_install_stream_command(&tmp_path))
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::inherit())
            .spawn()
            .map_err(|err| {
                io::Error::new(err.kind(), format!("failed to start ssh install: {err}"))
            })?;

        let mut source = File::open(source_path)?;
        let copy_result = if let Some(mut stdin) = child.stdin.take() {
            io::copy(&mut source, &mut stdin).map(|_| ())
        } else {
            Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "ssh install stdin missing",
            ))
        };
        let status = child.wait()?;
        copy_result?;

        if status.success() {
            let output = self.sh_output(&remote_install_commit_script(&tmp_path, &dest_path))?;
            if output.status.success() {
                Ok(())
            } else {
                Err(command_failed("remote install commit failed", &output))
            }
        } else {
            Err(io::Error::other(format!(
                "remote install exited with {status}"
            )))
        }
    }
}

fn remote_install_prepare_script(remote_herdr: &RemoteHerdr) -> String {
    format!(
        r#"set -eu
dest="$HOME/{install_suffix}"
dir="${{dest%/*}}"
mkdir -p "$dir"
tmp="${{dest}}.tmp.$$"
printf '%s\0%s\0' "$tmp" "$dest"
"#,
        install_suffix = remote_herdr.install_suffix
    )
}

fn parse_remote_install_paths(stdout: &[u8]) -> io::Result<(String, String)> {
    let mut parts = stdout.split(|byte| *byte == 0);
    let tmp_path = parts.next().unwrap_or_default();
    let dest_path = parts.next().unwrap_or_default();
    if tmp_path.is_empty() || dest_path.is_empty() {
        return Err(io::Error::other(
            "remote install preparation did not return destination paths",
        ));
    }
    let tmp_path = String::from_utf8(tmp_path.to_vec()).map_err(|err| {
        io::Error::other(format!(
            "remote install temporary path is not valid UTF-8: {err}"
        ))
    })?;
    let dest_path = String::from_utf8(dest_path.to_vec()).map_err(|err| {
        io::Error::other(format!(
            "remote install destination path is not valid UTF-8: {err}"
        ))
    })?;
    Ok((tmp_path, dest_path))
}

fn remote_install_stream_command(tmp_path: &str) -> String {
    format!("tee {}", shell_quote(tmp_path))
}

fn remote_install_commit_script(tmp_path: &str, dest_path: &str) -> String {
    format!(
        "set -eu\nchmod 755 {tmp_path}\nmv {tmp_path} {dest_path}\n",
        tmp_path = shell_quote(tmp_path),
        dest_path = shell_quote(dest_path)
    )
}

impl Drop for RemoteSsh {
    fn drop(&mut self) {
        if self.managed_config.is_none() {
            return;
        }

        let _ = self
            .base_command()
            .arg("-O")
            .arg("exit")
            .arg("-o")
            .arg("BatchMode=yes")
            .arg(self.target.destination())
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
}

fn apply_managed_ssh_options(command: &mut Command, options: Option<&ManagedSshOptions>) {
    let Some(options) = options else {
        return;
    };

    command
        .arg("-F")
        .arg(&options.config_path)
        .arg("-S")
        .arg(&options.control_path)
        .arg("-o")
        .arg("ControlMaster=auto")
        .arg("-o")
        // Bounded, not "yes": a master orphaned by a killed client must
        // self-expire once its last session closes instead of living forever.
        .arg("ControlPersist=60");
}

impl InstallSource {
    fn persistent(path: PathBuf) -> Self {
        Self {
            path,
            temporary_dir: None,
        }
    }

    fn temporary(path: PathBuf, temporary_dir: PathBuf) -> Self {
        Self {
            path,
            temporary_dir: Some(temporary_dir),
        }
    }

    fn cleanup(&self) {
        if let Some(dir) = &self.temporary_dir {
            let _ = fs::remove_dir_all(dir);
        }
    }
}

fn prepare_remote_herdr(
    ssh: &RemoteSsh,
    live_handoff_enabled: bool,
    policy: RemotePrepPolicy,
) -> io::Result<PreparedRemoteHerdr> {
    let platform = detect_remote_platform(ssh)?;
    let remote_herdr = RemoteHerdr::for_platform(platform);
    let override_binary = remote_binary_override_path()?;
    let remote_binary_candidates = remote_binary_candidates(ssh, &remote_herdr)?;
    let exe_name_remote_herdr = remote_herdr_from_current_exe_name(&remote_herdr.platform);

    if override_binary.is_none() {
        for candidate in remote_binary_candidates
            .iter()
            .chain(exe_name_remote_herdr.as_ref())
        {
            if remote_binary_matches(ssh, candidate).unwrap_or(false) {
                return Ok(PreparedRemoteHerdr {
                    remote_herdr: candidate.clone(),
                    installed_or_replaced: false,
                    stop_after_install_approved: false,
                });
            }
        }
        if remote_binary_matches(ssh, &remote_herdr)? {
            return Ok(PreparedRemoteHerdr {
                remote_herdr,
                installed_or_replaced: false,
                stop_after_install_approved: false,
            });
        }
    }

    let mut stop_after_install_approved = false;
    if let Some(status_probe_herdr) = remote_binary_candidates.first().or_else(|| {
        remote_binary_exists(ssh, &remote_herdr)
            .ok()
            .and_then(|exists| exists.then_some(&remote_herdr))
    }) {
        stop_after_install_approved = confirm_remote_install_with_running_server(
            ssh,
            status_probe_herdr,
            live_handoff_enabled,
            policy,
        )?;
    }
    confirm_remote_install(
        ssh.target(),
        &remote_herdr,
        &install_source_description(&remote_herdr.platform, override_binary.as_deref()),
        policy,
    )?;
    let source = resolve_install_source(&remote_herdr.platform, override_binary)?;
    let install_result = ssh.install_herdr(&remote_herdr, &source.path);
    source.cleanup();
    install_result?;

    match check_remote_binary(ssh, &remote_herdr)? {
        RemoteBinaryCheck::Compatible => {}
        other => {
            return Err(io::Error::other(
                other.install_failure_message(&remote_herdr.shell_path),
            ))
        }
    }
    warn_if_remote_bin_not_on_path(ssh)?;

    Ok(PreparedRemoteHerdr {
        remote_herdr,
        installed_or_replaced: true,
        stop_after_install_approved,
    })
}

fn detect_remote_platform(ssh: &RemoteSsh) -> io::Result<RemotePlatform> {
    let output = ssh.sh_output("uname -s\nuname -m\n")?;
    if !output.status.success() {
        return Err(command_failed("remote platform detection failed", &output));
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let mut lines = stdout.lines();
    let os = lines.next().unwrap_or_default();
    let arch = lines.next().unwrap_or_default();
    RemotePlatform::from_uname(os, arch).ok_or_else(|| {
        io::Error::other(format!(
            "unsupported remote platform: {} {}",
            os.trim(),
            arch.trim()
        ))
    })
}

fn remote_binary_candidates(
    ssh: &RemoteSsh,
    remote_herdr: &RemoteHerdr,
) -> io::Result<Vec<RemoteHerdr>> {
    let mut candidates = Vec::new();

    if let Some(path_candidate) = remote_binary_on_path_any(ssh, remote_herdr)? {
        push_if_new_remote_binary_candidate(&mut candidates, path_candidate);
    }

    let output = ssh.sh_output(&known_remote_binary_candidate_script(
        &remote_herdr.platform,
    ))?;
    if !output.status.success() {
        return Err(command_failed("remote binary discovery failed", &output));
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    for candidate in remote_herdrs_from_path_discovery(remote_herdr, &stdout) {
        push_if_new_remote_binary_candidate(&mut candidates, candidate);
    }

    Ok(candidates)
}

fn push_if_new_remote_binary_candidate(candidates: &mut Vec<RemoteHerdr>, candidate: RemoteHerdr) {
    if !candidates
        .iter()
        .any(|existing| existing.shell_path == candidate.shell_path)
    {
        candidates.push(candidate);
    }
}

fn known_remote_binary_candidate_script(platform: &RemotePlatform) -> String {
    let mut script = String::from(
        r#"home=${HOME:-}
user=${USER:-}
version="#,
    );
    script.push_str(&shell_quote(&current_version()));
    script.push_str(
        r#"
emit() {
    path=$1
    if [ -n "$path" ] && [ -x "$path" ]; then
        printf '%s\n' "$path"
    fi
}
if [ -n "$home" ]; then
    emit "$home/.local/bin/herdr"
fi
"#,
    );
    if platform.os == "macos" {
        script.push_str(
            r#"    emit "/opt/homebrew/bin/herdr"
    emit "/usr/local/bin/herdr"
"#,
        );
    } else if platform.os == "linux" {
        script.push_str(
            r#"    emit "/home/linuxbrew/.linuxbrew/bin/herdr"
"#,
        );
    }
    script.push_str(
        r#"if [ -n "$home" ]; then
    emit "$home/.local/share/mise/installs/herdr/$version/bin/herdr"
    emit "$home/.local/share/mise/installs/github-ogulcancelik-herdr/$version/herdr"
    emit "$home/.nix-profile/bin/herdr"
fi
if [ -n "$user" ]; then
    emit "/etc/profiles/per-user/$user/bin/herdr"
fi
emit "/nix/var/nix/profiles/default/bin/herdr"
emit "/run/current-system/sw/bin/herdr"
"#,
    );

    script
}

fn remote_binary_on_path_any(
    ssh: &RemoteSsh,
    remote_herdr: &RemoteHerdr,
) -> io::Result<Option<RemoteHerdr>> {
    let output = ssh.user_shell_output("command -v herdr")?;
    if !output.status.success() {
        return Ok(None);
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    Ok(remote_herdr_from_path_discovery(remote_herdr, &stdout))
}

fn remote_herdrs_from_path_discovery(remote_herdr: &RemoteHerdr, stdout: &str) -> Vec<RemoteHerdr> {
    stdout
        .lines()
        .filter_map(|path| remote_herdr_from_path(remote_herdr, path))
        .collect()
}

fn remote_herdr_from_path_discovery(
    remote_herdr: &RemoteHerdr,
    stdout: &str,
) -> Option<RemoteHerdr> {
    stdout
        .lines()
        .find_map(|path| remote_herdr_from_path(remote_herdr, path))
}

fn remote_herdr_from_path(remote_herdr: &RemoteHerdr, path: &str) -> Option<RemoteHerdr> {
    let path = path.trim();
    if !path.starts_with('/') {
        return None;
    }
    if is_mise_shim_path(path) {
        return None;
    }
    Some(remote_herdr.clone().with_shell_path(shell_quote(path)))
}

fn is_mise_shim_path(path: &str) -> bool {
    path.ends_with("/mise/shims/herdr")
}

fn remote_herdr_from_current_exe_name(platform: &RemotePlatform) -> Option<RemoteHerdr> {
    let exe = std::env::current_exe().ok()?;
    let name = exe.file_name()?.to_str()?;
    remote_herdr_from_exe_name(platform.clone(), name)
}

fn remote_herdr_from_exe_name(platform: RemotePlatform, name: &str) -> Option<RemoteHerdr> {
    if name == "herdr"
        || name.is_empty()
        || !name
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '.' | '_' | '-'))
    {
        return None;
    }

    Some(RemoteHerdr::for_install_suffix(
        platform,
        format!(".local/bin/{name}"),
    ))
}

fn remote_binary_matches(ssh: &RemoteSsh, remote_herdr: &RemoteHerdr) -> io::Result<bool> {
    let command = remote_binary_match_command(remote_herdr);
    let output = ssh.sh_output(&command)?;
    if !output.status.success() {
        return Ok(false);
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let mut lines = stdout.lines();
    let version = lines.next().unwrap_or_default().trim();
    let status = lines.next().unwrap_or_default();
    Ok(version == format!("herdr {}", current_version())
        && parse_client_status_json(status)
            .map(|status| status.protocol == CURRENT_PROTOCOL)
            .unwrap_or(false))
}

fn remote_binary_match_command(remote_herdr: &RemoteHerdr) -> String {
    format!(
        "test -x {0} && {0} --version && {0} status client --json && {1}=1 {0} remote-client-bridge && {1}=1 {0} remote-api-bridge",
        remote_herdr.shell_path, REMOTE_BRIDGE_PROBE_ENV_VAR
    )
}

/// Why a freshly-installed remote herdr is not usable by this client. Unlike the boolean
/// `remote_binary_matches`, this distinguishes the failure so the user gets a truthful, actionable
/// message instead of the old catch-all "did not report version" (which lied when the version
/// actually matched but the protocol/bridge support did not).
#[derive(Debug, Clone, PartialEq, Eq)]
enum RemoteBinaryCheck {
    Compatible,
    NotExecutable,
    /// Ran, but reported a different herdr version than this client.
    VersionMismatch {
        reported: String,
    },
    /// Version matched, but the wire protocol differs (e.g. an older same-version release asset).
    ProtocolMismatch {
        reported: Option<u32>,
    },
    /// Version + protocol look right, but the binary lacks the remote-bridge subcommands.
    MissingBridgeSupport,
    /// Probe output could not be understood (binary did not respond to --version/status).
    Unintelligible,
}

/// A diagnostic probe that reports each capability on its own marker line (instead of the
/// short-circuiting `&&` chain in `remote_binary_match_command`), so we can tell *which* check
/// failed. The script always exits 0 and emits exactly one terminal marker per stage.
fn remote_binary_diagnose_command(remote_herdr: &RemoteHerdr) -> String {
    format!(
        "P={0}\n\
         if ! test -x \"$P\"; then echo HERDR_PROBE_NOT_EXECUTABLE; exit 0; fi\n\
         v=$(\"$P\" --version 2>/dev/null) || {{ echo HERDR_PROBE_NO_VERSION; exit 0; }}\n\
         echo \"HERDR_PROBE_VERSION $v\"\n\
         s=$(\"$P\" status client --json 2>/dev/null) || {{ echo HERDR_PROBE_NO_STATUS; exit 0; }}\n\
         echo \"HERDR_PROBE_STATUS $s\"\n\
         if {1}=1 \"$P\" remote-client-bridge >/dev/null 2>&1 && {1}=1 \"$P\" remote-api-bridge >/dev/null 2>&1; then echo HERDR_PROBE_BRIDGE_OK; else echo HERDR_PROBE_BRIDGE_MISSING; fi\n",
        remote_herdr.shell_path, REMOTE_BRIDGE_PROBE_ENV_VAR
    )
}

fn interpret_remote_binary_probe(stdout: &str) -> RemoteBinaryCheck {
    let mut version = None;
    let mut status = None;
    let mut bridge_ok = None;
    for line in stdout.lines() {
        let line = line.trim();
        if line == "HERDR_PROBE_NOT_EXECUTABLE" {
            return RemoteBinaryCheck::NotExecutable;
        } else if line == "HERDR_PROBE_NO_VERSION" || line == "HERDR_PROBE_NO_STATUS" {
            return RemoteBinaryCheck::Unintelligible;
        } else if let Some(rest) = line.strip_prefix("HERDR_PROBE_VERSION ") {
            version = Some(rest.trim().to_string());
        } else if let Some(rest) = line.strip_prefix("HERDR_PROBE_STATUS ") {
            status = Some(rest.trim().to_string());
        } else if line == "HERDR_PROBE_BRIDGE_OK" {
            bridge_ok = Some(true);
        } else if line == "HERDR_PROBE_BRIDGE_MISSING" {
            bridge_ok = Some(false);
        }
    }

    let Some(version) = version else {
        return RemoteBinaryCheck::Unintelligible;
    };
    if version != format!("herdr {}", current_version()) {
        return RemoteBinaryCheck::VersionMismatch { reported: version };
    }
    let protocol = status
        .as_deref()
        .and_then(parse_client_status_json)
        .map(|status| status.protocol);
    if protocol != Some(CURRENT_PROTOCOL) {
        return RemoteBinaryCheck::ProtocolMismatch { reported: protocol };
    }
    if bridge_ok != Some(true) {
        return RemoteBinaryCheck::MissingBridgeSupport;
    }
    RemoteBinaryCheck::Compatible
}

impl RemoteBinaryCheck {
    /// Message for the case where we just installed a binary but it is not usable. Points the user
    /// at `HERDR_REMOTE_BINARY` for the common cross-platform / dev-build cause.
    fn install_failure_message(&self, shell_path: &str) -> String {
        let current_version = current_version();
        let seed = format!(
            "Build herdr for the remote platform and set {REMOTE_BINARY_ENV_VAR}=<path>, or install a matching herdr on the remote host manually."
        );
        match self {
            RemoteBinaryCheck::Compatible => {
                format!("remote herdr at {shell_path} is compatible")
            }
            RemoteBinaryCheck::NotExecutable => format!(
                "installed remote herdr at {shell_path}, but it is not executable on the remote host (most likely a wrong-architecture binary). {seed}"
            ),
            RemoteBinaryCheck::VersionMismatch { reported } => format!(
                "installed remote herdr at {shell_path}, but it reports `{reported}`, not version {current_version}. {seed}"
            ),
            RemoteBinaryCheck::ProtocolMismatch { reported } => format!(
                "installed remote herdr at {shell_path} runs protocol {}, but this client needs protocol {CURRENT_PROTOCOL}. The version matched, so this is an older {current_version} build (e.g. the published release asset for the remote platform predates protocol {CURRENT_PROTOCOL}). {seed}",
                protocol_label(*reported)
            ),
            RemoteBinaryCheck::MissingBridgeSupport => format!(
                "installed remote herdr at {shell_path} does not support the remote-bridge subcommands this client needs (an older {current_version} build). {seed}"
            ),
            RemoteBinaryCheck::Unintelligible => format!(
                "installed remote herdr at {shell_path}, but it did not respond to --version/status probes. {seed}"
            ),
        }
    }
}

/// Diagnose the installed remote binary, distinguishing version/protocol/bridge failures. Used on
/// the post-install error path; the hot path still uses the cheaper boolean `remote_binary_matches`.
fn check_remote_binary(
    ssh: &RemoteSsh,
    remote_herdr: &RemoteHerdr,
) -> io::Result<RemoteBinaryCheck> {
    let output = ssh.sh_output(&remote_binary_diagnose_command(remote_herdr))?;
    if !output.status.success() {
        return Ok(RemoteBinaryCheck::Unintelligible);
    }
    Ok(interpret_remote_binary_probe(&String::from_utf8_lossy(
        &output.stdout,
    )))
}

fn remote_binary_exists(ssh: &RemoteSsh, remote_herdr: &RemoteHerdr) -> io::Result<bool> {
    let command = format!("test -x {}", remote_herdr.shell_path);
    Ok(ssh.sh_output(&command)?.status.success())
}

fn remote_binary_override_path() -> io::Result<Option<PathBuf>> {
    let Some(value) = std::env::var_os(REMOTE_BINARY_ENV_VAR) else {
        return Ok(None);
    };
    if value.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("{REMOTE_BINARY_ENV_VAR} must not be empty"),
        ));
    }

    let path = PathBuf::from(value);
    let metadata = fs::metadata(&path).map_err(|err| {
        io::Error::new(
            err.kind(),
            format!(
                "failed to inspect {REMOTE_BINARY_ENV_VAR} path {}: {err}",
                path.display()
            ),
        )
    })?;
    if !metadata.is_file() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "{REMOTE_BINARY_ENV_VAR} path is not a file: {}",
                path.display()
            ),
        ));
    }

    Ok(Some(path))
}

fn install_source_description(platform: &RemotePlatform, override_binary: Option<&Path>) -> String {
    install_source_description_for(
        platform,
        override_binary,
        local_binary_can_seed_remote(platform),
    )
}

fn install_source_description_for(
    platform: &RemotePlatform,
    override_binary: Option<&Path>,
    local_binary_can_seed_remote: bool,
) -> String {
    if let Some(path) = override_binary {
        return format!("{REMOTE_BINARY_ENV_VAR} ({})", path.display());
    }

    if local_binary_can_seed_remote {
        "the current local herdr binary".to_string()
    } else {
        format!(
            "the {} {} asset for {}",
            current_version(),
            current_channel(),
            platform.asset_key()
        )
    }
}

fn resolve_install_source(
    platform: &RemotePlatform,
    override_binary: Option<PathBuf>,
) -> io::Result<InstallSource> {
    if let Some(path) = override_binary {
        return Ok(InstallSource::persistent(path));
    }

    if *platform == RemotePlatform::local() {
        let path = std::env::current_exe()?;
        if !crate::update::is_package_manager_managed_exe_path(&path) {
            return Ok(InstallSource::persistent(path));
        }
    }

    download_release_asset(platform)
}

fn local_binary_can_seed_remote(platform: &RemotePlatform) -> bool {
    if *platform != RemotePlatform::local() {
        return false;
    }

    std::env::current_exe()
        .map(|path| !crate::update::is_package_manager_managed_exe_path(&path))
        .unwrap_or(false)
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum RemoteServerStatus {
    Running {
        version: Option<String>,
        protocol: Option<u32>,
        live_handoff: bool,
        detached_server_daemon: bool,
    },
    NotRunning,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RemoteServerRestartReason {
    ProtocolMismatch,
    DaemonDetachMissing,
    BinaryUpdated,
    VersionMismatch,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RemoteInstallRunningServerPlan {
    KeepRunning,
    LiveHandoff,
    StopRequired(RemoteServerRestartReason),
}

fn ensure_remote_server_ready(
    ssh: &RemoteSsh,
    remote_herdr: &RemoteHerdr,
    remote_binary_changed: bool,
    stop_after_install_approved: bool,
    live_handoff_enabled: bool,
    policy: RemotePrepPolicy,
) -> io::Result<()> {
    let status = remote_server_status(ssh, remote_herdr)?;
    let RemoteServerStatus::Running {
        version,
        protocol,
        live_handoff,
        detached_server_daemon,
    } = status
    else {
        return Ok(());
    };

    let Some(reason) = remote_server_restart_reason(
        version.as_deref(),
        protocol,
        detached_server_daemon,
        remote_binary_changed,
    ) else {
        return Ok(());
    };

    // Non-interactive (client) attach: decide without prompting and never hard-stop.
    if let RemotePrepPolicy::NonInteractive {
        restart_incompatible,
    } = policy
    {
        match non_interactive_server_action(reason, live_handoff_enabled, live_handoff) {
            NonInteractiveServerAction::AttachExisting => return Ok(()),
            NonInteractiveServerAction::LiveHandoff => {
                return match live_handoff_remote_server(ssh, remote_herdr) {
                    Ok(()) => Ok(()),
                    // A failed handoff for a protocol mismatch leaves us unable to attach; surface
                    // it rather than killing panes. For a compatible server, fall back to attaching.
                    Err(err) if reason == RemoteServerRestartReason::ProtocolMismatch => Err(err),
                    Err(err) => {
                        eprintln!(
                            "remote live handoff failed: {err}; attaching to the running server."
                        );
                        Ok(())
                    }
                };
            }
            NonInteractiveServerAction::ProtocolStuck => {
                // The remote runs an incompatible server that can't live-handoff. Stopping it would
                // interrupt its panes, so we never do that silently — unless the user explicitly
                // approved it via the add-remote y/N. Otherwise we surface a typed signal the
                // client turns into that prompt.
                if restart_incompatible {
                    stop_remote_server(ssh, remote_herdr)?;
                    return Ok(());
                }
                return Err(io::Error::other(RestartConfirmNeeded {
                    destination: ssh.target().to_string(),
                    version: version.clone(),
                    protocol,
                }));
            }
        }
    }

    if live_handoff_enabled && live_handoff {
        match live_handoff_remote_server(ssh, remote_herdr) {
            Ok(()) => return Ok(()),
            Err(err) => {
                eprintln!("remote live handoff failed: {err}");
                eprintln!("falling back to remote server restart.");
            }
        }
    }

    if stop_after_install_approved {
        stop_remote_server(ssh, remote_herdr)?;
        return Ok(());
    }

    if confirm_remote_server_stop(ssh.target(), version.as_deref(), protocol, reason)? {
        stop_remote_server(ssh, remote_herdr)?;
    }
    Ok(())
}

fn remote_server_restart_reason(
    version: Option<&str>,
    protocol: Option<u32>,
    detached_server_daemon: bool,
    remote_binary_changed: bool,
) -> Option<RemoteServerRestartReason> {
    if protocol != Some(CURRENT_PROTOCOL) {
        return Some(RemoteServerRestartReason::ProtocolMismatch);
    }
    if !detached_server_daemon {
        return Some(RemoteServerRestartReason::DaemonDetachMissing);
    }
    if version != Some(current_version().as_str()) {
        return Some(RemoteServerRestartReason::VersionMismatch);
    }
    if remote_binary_changed {
        return Some(RemoteServerRestartReason::BinaryUpdated);
    }
    None
}

/// What a non-interactive (client-driven) attach should do with an out-of-date running remote
/// server, given the restart reason and whether live-handoff is possible. It never hard-stops:
/// a protocol mismatch that cannot hand off is reported as an error rather than killing panes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum NonInteractiveServerAction {
    /// Attach to the running server unchanged (protocol is compatible).
    AttachExisting,
    /// Live-handoff to the prepared server (preserves panes), then attach.
    LiveHandoff,
    /// Cannot attach: protocol mismatch and live-handoff is unavailable.
    ProtocolStuck,
}

fn non_interactive_server_action(
    reason: RemoteServerRestartReason,
    live_handoff_enabled: bool,
    live_handoff_supported: bool,
) -> NonInteractiveServerAction {
    let can_handoff = live_handoff_enabled && live_handoff_supported;
    match reason {
        RemoteServerRestartReason::ProtocolMismatch => {
            if can_handoff {
                NonInteractiveServerAction::LiveHandoff
            } else {
                NonInteractiveServerAction::ProtocolStuck
            }
        }
        // Protocol is compatible: prefer a pane-preserving handoff to pick up the new binary, but
        // attaching to the running server is always a safe fallback (no hard restart).
        RemoteServerRestartReason::DaemonDetachMissing
        | RemoteServerRestartReason::BinaryUpdated
        | RemoteServerRestartReason::VersionMismatch => {
            if can_handoff {
                NonInteractiveServerAction::LiveHandoff
            } else {
                NonInteractiveServerAction::AttachExisting
            }
        }
    }
}

fn confirm_remote_install_with_running_server(
    ssh: &RemoteSsh,
    remote_herdr: &RemoteHerdr,
    live_handoff_enabled: bool,
    policy: RemotePrepPolicy,
) -> io::Result<bool> {
    // Non-interactive (client) attach auto-approves replacing the binary; the running server is
    // reconciled later in `ensure_remote_server_ready` (live-handoff when possible).
    if matches!(policy, RemotePrepPolicy::NonInteractive { .. }) {
        return Ok(false);
    }
    let target = ssh.target();
    let status = match remote_server_status(ssh, remote_herdr) {
        Ok(status) => status,
        Err(err) => {
            if !io::stdin().is_terminal() {
                return Err(io::Error::other(format!(
                    "could not inspect the running remote herdr server on {target} before installing: {err}; run from an interactive terminal to approve updating the remote binary"
                )));
            }
            eprintln!(
                "could not inspect the running remote herdr server on {target} before installing: {err}"
            );
            eprint!("continue installing the remote herdr binary? [y/N] ");
            io::stderr().flush()?;

            let mut answer = String::new();
            io::stdin().read_line(&mut answer)?;
            let answer = answer.trim().to_ascii_lowercase();
            if answer != "y" && answer != "yes" {
                return Err(io::Error::new(
                    io::ErrorKind::Interrupted,
                    "remote herdr install cancelled",
                ));
            }
            return Ok(false);
        }
    };
    let RemoteServerStatus::Running {
        version,
        protocol,
        live_handoff,
        detached_server_daemon,
    } = &status
    else {
        return Ok(false);
    };
    let plan = remote_install_running_server_plan(
        version.as_deref(),
        *protocol,
        *detached_server_daemon,
        true,
        *live_handoff,
        live_handoff_enabled,
    );

    if plan == RemoteInstallRunningServerPlan::KeepRunning {
        if io::stdin().is_terminal() {
            eprintln!("remote herdr server on {target} is already compatible:");
            eprintln!("  server: v{}", version_label(version.as_deref()));
            eprintln!(
                "Herdr will install {} without stopping the running remote server.",
                current_version()
            );
        }
        return Ok(false);
    }

    if !io::stdin().is_terminal() {
        match plan {
            RemoteInstallRunningServerPlan::LiveHandoff => return Ok(false),
            RemoteInstallRunningServerPlan::StopRequired(_) => {
                return Err(io::Error::other(format!(
                    "remote herdr server on {target} is running v{}; run from an interactive terminal to approve stopping it for the update",
                    version_label(version.as_deref())
                )));
            }
            RemoteInstallRunningServerPlan::KeepRunning => return Ok(false),
        }
    }

    if plan == RemoteInstallRunningServerPlan::LiveHandoff {
        eprintln!("remote herdr server on {target} is currently running:");
        eprintln!("  server: v{}", version_label(version.as_deref()));
        eprintln!(
            "Herdr will install {} and hand off live pane processes to the prepared server.",
            current_version()
        );
        return Ok(false);
    }

    eprintln!("remote herdr server on {target} is currently running:");
    eprintln!("  server: v{}", version_label(version.as_deref()));
    eprintln!(
        "To complete the remote update, Herdr must stop the running remote server after installing."
    );
    eprintln!("This stops active remote pane processes, including shells, dev servers, and tests.");
    eprintln!();
    eprint!(
        "Install {} and stop the remote server now? [y/N] ",
        current_version()
    );
    io::stderr().flush()?;

    let mut answer = String::new();
    io::stdin().read_line(&mut answer)?;
    let answer = answer.trim().to_ascii_lowercase();
    if answer != "y" && answer != "yes" {
        return Err(io::Error::new(
            io::ErrorKind::Interrupted,
            "remote herdr install cancelled",
        ));
    }

    Ok(true)
}

fn remote_install_running_server_plan(
    version: Option<&str>,
    protocol: Option<u32>,
    detached_server_daemon: bool,
    remote_binary_changed: bool,
    live_handoff: bool,
    live_handoff_enabled: bool,
) -> RemoteInstallRunningServerPlan {
    let Some(reason) = remote_server_restart_reason(
        version,
        protocol,
        detached_server_daemon,
        remote_binary_changed,
    ) else {
        return RemoteInstallRunningServerPlan::KeepRunning;
    };

    if live_handoff_enabled && live_handoff {
        return RemoteInstallRunningServerPlan::LiveHandoff;
    }

    RemoteInstallRunningServerPlan::StopRequired(reason)
}

fn remote_server_status(
    ssh: &RemoteSsh,
    remote_herdr: &RemoteHerdr,
) -> io::Result<RemoteServerStatus> {
    let command = format!("{} status server --json", remote_herdr.shell_path);
    let output = ssh.sh_output(&command)?;
    if !output.status.success() {
        return Err(command_failed("remote server status failed", &output));
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    parse_remote_server_status_json(stdout.trim())
}

#[derive(Debug, Deserialize)]
struct RemoteClientStatusJson {
    protocol: u32,
}

#[derive(Debug, Deserialize)]
struct RemoteServerStatusJson {
    running: bool,
    version: Option<String>,
    protocol: Option<u32>,
    capabilities: Option<RemoteServerCapabilitiesJson>,
}

#[derive(Debug, Deserialize)]
struct RemoteServerCapabilitiesJson {
    live_handoff: bool,
    #[serde(default)]
    detached_server_daemon: bool,
}

fn parse_client_status_json(status: &str) -> Option<RemoteClientStatusJson> {
    serde_json::from_str(status).ok()
}

fn parse_remote_server_status_json(status: &str) -> io::Result<RemoteServerStatus> {
    let parsed: RemoteServerStatusJson = serde_json::from_str(status).map_err(|err| {
        io::Error::other(format!(
            "could not parse remote server status JSON from `{status}`: {err}"
        ))
    })?;
    if !parsed.running {
        return Ok(RemoteServerStatus::NotRunning);
    }

    let capabilities = parsed.capabilities;

    Ok(RemoteServerStatus::Running {
        version: parsed.version,
        protocol: parsed.protocol,
        live_handoff: capabilities
            .as_ref()
            .is_some_and(|capabilities| capabilities.live_handoff),
        detached_server_daemon: capabilities
            .as_ref()
            .is_some_and(|capabilities| capabilities.detached_server_daemon),
    })
}

fn confirm_remote_server_stop(
    target: &str,
    version: Option<&str>,
    _protocol: Option<u32>,
    reason: RemoteServerRestartReason,
) -> io::Result<bool> {
    if !io::stdin().is_terminal() {
        if reason == RemoteServerRestartReason::ProtocolMismatch {
            return Err(io::Error::other(format!(
                "remote herdr server on {target} must stop before this client can attach; run from an interactive terminal to approve stopping it"
            )));
        }

        eprintln!(
            "remote herdr server on {target} is still running v{}; it will use {} after it restarts.",
            version_label(version),
            current_version()
        );
        return Ok(false);
    }

    eprintln!("remote herdr server on {target} is currently running:");
    eprintln!("  server: v{}", version_label(version));
    eprintln!("  prepared binary: {}", current_version());
    eprintln!();

    match reason {
        RemoteServerRestartReason::ProtocolMismatch => {
            eprintln!("the remote server must stop before this client can attach.");
        }
        RemoteServerRestartReason::DaemonDetachMissing => {
            eprintln!(
                "the remote server was started by a herdr build that may not survive SSH connection loss. restart it so network drops disconnect only this client."
            );
        }
        RemoteServerRestartReason::BinaryUpdated => {
            eprintln!(
                "the remote herdr binary was installed or replaced. restart the remote server so it uses the prepared binary."
            );
        }
        RemoteServerRestartReason::VersionMismatch => {
            eprintln!(
                "the remote server is still running a different herdr version. restart it so it uses the prepared binary."
            );
        }
    }

    let prompt = if reason == RemoteServerRestartReason::ProtocolMismatch {
        "stop the remote server and continue attaching? [Y/n] "
    } else {
        "restart the remote server now? [y/N] "
    };
    eprint!("{prompt}");
    io::stderr().flush()?;

    let mut answer = String::new();
    io::stdin().read_line(&mut answer)?;
    let answer = answer.trim().to_ascii_lowercase();
    if answer == "y" || answer == "yes" {
        return Ok(true);
    }
    if answer.is_empty() && reason == RemoteServerRestartReason::ProtocolMismatch {
        return Ok(true);
    }
    if reason == RemoteServerRestartReason::ProtocolMismatch {
        return Err(io::Error::new(
            io::ErrorKind::Interrupted,
            "remote herdr server stop cancelled",
        ));
    }

    Ok(false)
}

fn live_handoff_remote_server(ssh: &RemoteSsh, remote_herdr: &RemoteHerdr) -> io::Result<()> {
    let command = format!(
        "{} server live-handoff --import-exe {} --expected-protocol {} --expected-version {}",
        remote_herdr.shell_path,
        remote_herdr.shell_path,
        CURRENT_PROTOCOL,
        current_version()
    );
    let output = ssh.sh_output(&command)?;
    if !output.status.success() {
        return Err(command_failed("remote server live handoff failed", &output));
    }

    eprintln!(
        "handed off the remote herdr server on {}; reconnecting to the prepared server.",
        ssh.target()
    );
    Ok(())
}

fn stop_remote_server(ssh: &RemoteSsh, remote_herdr: &RemoteHerdr) -> io::Result<()> {
    let command = format!("{} server stop", remote_herdr.shell_path);
    let output = ssh.sh_output(&command)?;
    if !output.status.success() {
        return Err(command_failed("remote server stop failed", &output));
    }

    wait_for_remote_server_shutdown(ssh, remote_herdr)?;
    eprintln!(
        "stopped the remote herdr server on {}; it will restart when the remote client bridge attaches.",
        ssh.target()
    );
    Ok(())
}

fn wait_for_remote_server_shutdown(ssh: &RemoteSsh, remote_herdr: &RemoteHerdr) -> io::Result<()> {
    let deadline = Instant::now() + REMOTE_SERVER_SHUTDOWN_CONFIRM_TIMEOUT;
    loop {
        if remote_server_status(ssh, remote_herdr)? == RemoteServerStatus::NotRunning {
            return Ok(());
        }
        if Instant::now() >= deadline {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                format!(
                    "shutdown was requested, but the old remote herdr server on {target} is still responding after {} seconds",
                    REMOTE_SERVER_SHUTDOWN_CONFIRM_TIMEOUT.as_secs(),
                    target = ssh.target()
                ),
            ));
        }
        thread::sleep(REMOTE_SERVER_SHUTDOWN_POLL_INTERVAL);
    }
}

fn version_label(version: Option<&str>) -> &str {
    version.unwrap_or("unknown")
}

fn protocol_label(protocol: Option<u32>) -> String {
    protocol
        .map(|value| value.to_string())
        .unwrap_or_else(|| "unknown".to_string())
}

fn warn_if_remote_bin_not_on_path(ssh: &RemoteSsh) -> io::Result<()> {
    let output = ssh.user_shell_output("command -v herdr")?;
    if output.status.success()
        && remote_shell_resolves_managed_install(&String::from_utf8_lossy(&output.stdout))
    {
        return Ok(());
    }

    eprintln!(
        "herdr: installed remote binary to ~/.local/bin/herdr, but the remote shell does not resolve `herdr` to that path"
    );
    Ok(())
}

fn remote_shell_resolves_managed_install(stdout: &str) -> bool {
    stdout
        .lines()
        .next()
        .map(str::trim)
        .is_some_and(|path| path.ends_with("/.local/bin/herdr"))
}

fn download_release_asset(platform: &RemotePlatform) -> io::Result<InstallSource> {
    let asset_key = platform.asset_key();
    let asset = remote_release_asset(&asset_key)?;

    let dir = private_download_dir(&asset_key)?;
    let path = dir.join("herdr.tmp");
    let status = Command::new("curl")
        .args(["-sfL", "--max-time", "120", "-o"])
        .arg(&path)
        .arg(&asset.url)
        .status()
        .map_err(|err| io::Error::new(err.kind(), format!("download failed: {err}")))?;
    if !status.success() {
        let _ = fs::remove_dir_all(&dir);
        return Err(io::Error::other("download failed"));
    }
    if let Some(expected) = &asset.sha256 {
        if let Err(err) = crate::checksum::verify_sha256(&path, expected) {
            let _ = fs::remove_dir_all(&dir);
            return Err(io::Error::new(
                err.kind(),
                format!("downloaded remote asset checksum verification failed: {err}"),
            ));
        }
    }

    Ok(InstallSource::temporary(path, dir))
}

fn fetch_remote_manifest(url: &str) -> io::Result<Vec<u8>> {
    let output = Command::new("curl")
        .args([
            "-sfL",
            "--retry",
            "3",
            "--connect-timeout",
            "10",
            "--max-time",
            "20",
            url,
        ])
        .output()
        .map_err(|err| io::Error::new(err.kind(), format!("curl failed: {err}")))?;
    if !output.status.success() {
        return Err(command_failed("failed to fetch update manifest", &output));
    }
    Ok(output.stdout)
}

fn remote_asset_info(asset: &RemoteAssetRef) -> RemoteReleaseAsset {
    RemoteReleaseAsset {
        url: asset.url().to_string(),
        sha256: asset.sha256().map(str::to_string),
    }
}

fn preview_assets_for_build<'a>(
    manifest: &'a RemotePreviewManifest,
    build_id: &str,
) -> io::Result<(u32, &'a BTreeMap<String, RemoteAssetRef>)> {
    if manifest.build_id == build_id {
        return Ok((manifest.protocol, &manifest.assets));
    }
    let build = manifest.builds.get(build_id).ok_or_else(|| {
        io::Error::other(format!(
            "preview manifest no longer includes build {build_id}; run `herdr update` locally or set {REMOTE_BINARY_ENV_VAR}=target/release/herdr"
        ))
    })?;
    Ok((build.protocol, &build.assets))
}

fn remote_release_asset(asset_key: &str) -> io::Result<RemoteReleaseAsset> {
    if crate::build_info::is_preview() {
        let build_id = crate::build_info::build_id().ok_or_else(|| {
            io::Error::other("preview client has no build id; set HERDR_REMOTE_BINARY or install Herdr on the remote manually")
        })?;
        let manifest_bytes = fetch_remote_manifest(PREVIEW_UPDATE_MANIFEST_URL)?;
        let manifest: RemotePreviewManifest =
            serde_json::from_slice(&manifest_bytes).map_err(|err| {
                io::Error::other(format!("failed to parse preview manifest JSON: {err}"))
            })?;
        let (protocol, assets) = preview_assets_for_build(&manifest, build_id)?;
        if protocol != CURRENT_PROTOCOL {
            return Err(io::Error::other(format!(
                "preview manifest has build {build_id} protocol {protocol}, but this client needs protocol {CURRENT_PROTOCOL}; set {REMOTE_BINARY_ENV_VAR}=target/release/herdr or install a matching Herdr on the remote host manually"
            )));
        }
        return assets.get(asset_key).map(remote_asset_info).ok_or_else(|| {
            io::Error::other(format!(
                "no {asset_key} binary in the preview manifest for build {build_id}"
            ))
        });
    }

    let current_version = current_version();
    let manifest_bytes = fetch_remote_manifest(STABLE_UPDATE_MANIFEST_URL)?;
    let manifest: RemoteUpdateManifest = serde_json::from_slice(&manifest_bytes)
        .map_err(|err| io::Error::other(format!("failed to parse update manifest JSON: {err}")))?;
    let release = manifest.release_for_version(&current_version).ok_or_else(|| {
        io::Error::other(format!(
            "release manifest does not include herdr {current_version}; build herdr for {} or install it there manually",
            asset_key
        ))
    })?;
    if let Some(protocol) = release.protocol {
        if protocol != CURRENT_PROTOCOL {
            return Err(io::Error::other(format!(
                "release manifest has herdr {current_version} protocol {protocol}, but this client needs protocol {CURRENT_PROTOCOL}; set {REMOTE_BINARY_ENV_VAR}=target/release/herdr or install a matching herdr on the remote host manually"
            )));
        }
    }
    release
        .assets
        .get(asset_key)
        .map(remote_asset_info)
        .ok_or_else(|| {
            io::Error::other(format!(
                "no {asset_key} binary in the release manifest for herdr {current_version}"
            ))
        })
}

fn private_download_dir(asset_key: &str) -> io::Result<PathBuf> {
    let base = std::env::temp_dir();
    for attempt in 0..100 {
        let dir = base.join(format!(
            "herdr-remote-{}-{}-{attempt}",
            std::process::id(),
            asset_key
        ));
        match fs::create_dir(&dir) {
            Ok(()) => return Ok(dir),
            Err(err) if err.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(err) => return Err(err),
        }
    }

    Err(io::Error::new(
        io::ErrorKind::AlreadyExists,
        "failed to create private herdr remote download directory",
    ))
}

fn confirm_remote_install(
    target: &str,
    remote_herdr: &RemoteHerdr,
    source_description: &str,
    policy: RemotePrepPolicy,
) -> io::Result<()> {
    // A fresh ssh-reachable host auto-installs herdr with no prompt on the client-driven path.
    if matches!(policy, RemotePrepPolicy::NonInteractive { .. }) {
        return Ok(());
    }
    if !io::stdin().is_terminal() {
        return Err(io::Error::other(format!(
            "matching remote herdr {} is not installed at {}; run from an interactive terminal to approve installation",
            current_version(),
            remote_herdr.shell_path
        )));
    }

    eprintln!(
        "matching herdr {} is not installed on {target} for {}.",
        current_version(),
        remote_herdr.platform.asset_key()
    );
    eprint!(
        "Install {} to {}? [Y/n] ",
        source_description, remote_herdr.shell_path
    );
    io::stderr().flush()?;

    let mut answer = String::new();
    io::stdin().read_line(&mut answer)?;
    let answer = answer.trim().to_ascii_lowercase();
    if answer == "n" || answer == "no" {
        return Err(io::Error::new(
            io::ErrorKind::Interrupted,
            "remote herdr installation cancelled",
        ));
    }

    Ok(())
}

fn remote_bridge_command(
    remote_herdr: &RemoteHerdr,
    session_name: &str,
    kind: RemoteBridgeKind,
) -> String {
    let mut command = format!("exec {}", remote_herdr.shell_path);
    if session_name != crate::session::DEFAULT_SESSION_NAME {
        command.push_str(" --session ");
        command.push_str(&shell_quote(session_name));
    }
    command.push(' ');
    command.push_str(kind.subcommand());
    command
}

fn reattach_command(
    program: &str,
    target: &str,
    session_name: &str,
    keybindings: RemoteKeybindings,
    live_handoff: bool,
) -> String {
    let program = if program.is_empty() { "herdr" } else { program };
    let mut command = format!("{} --remote {}", shell_quote(program), shell_quote(target));
    if keybindings != RemoteKeybindings::Local {
        command.push_str(" --remote-keybindings ");
        command.push_str(keybindings.as_str());
    }
    if live_handoff {
        command.push_str(" --handoff");
    }
    if session_name != crate::session::DEFAULT_SESSION_NAME {
        command.push_str(" --session ");
        command.push_str(&shell_quote(session_name));
    }
    command
}

fn shell_quote(value: &str) -> String {
    if !value.is_empty()
        && value.chars().all(|ch| {
            ch.is_ascii_alphanumeric()
                || matches!(
                    ch,
                    '@' | '%' | '_' | '+' | '=' | ':' | ',' | '.' | '/' | '-'
                )
        })
    {
        return value.to_string();
    }

    format!("'{}'", value.replace('\'', "'\\''"))
}

fn command_failed(context: &str, output: &Output) -> io::Error {
    let stderr = String::from_utf8_lossy(&output.stderr);
    let stderr = stderr.trim();
    if stderr.is_empty() {
        io::Error::other(format!("{context}: {}", output.status))
    } else {
        io::Error::other(format!("{context}: {stderr}"))
    }
}

struct SshStdioBridge {
    local_socket: PathBuf,
    should_stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
    /// PIDs of the live per-connection ssh children. Killed on drop: an
    /// established bridge pipe otherwise outlives remote removal/disable —
    /// the local ssh child and the remote `remote-*-bridge` process both
    /// linger until the client exits, because nothing else closes the pipe.
    live_children: Arc<std::sync::Mutex<Vec<u32>>>,
}

fn spawn_bridge_worker(
    stream: UnixStream,
    run: impl FnOnce(UnixStream) -> io::Result<()> + Send + 'static,
) {
    let _ = thread::spawn(move || {
        if let Err(err) = run(stream) {
            eprintln!("herdr: remote bridge failed: {err}");
        }
    });
}

impl SshStdioBridge {
    fn start(
        ssh: Arc<RemoteSsh>,
        remote_herdr: RemoteHerdr,
        local_socket: PathBuf,
        session_name: String,
        kind: RemoteBridgeKind,
    ) -> io::Result<Self> {
        let _ = std::fs::remove_file(&local_socket);
        let listener = UnixListener::bind(&local_socket)?;
        crate::ipc::restrict_socket_permissions(&local_socket, BRIDGE_SOCKET_PERMISSION_MODE)?;
        listener.set_nonblocking(true)?;

        let should_stop = Arc::new(AtomicBool::new(false));
        let thread_stop = Arc::clone(&should_stop);
        let live_children = Arc::new(std::sync::Mutex::new(Vec::new()));
        let thread_children = Arc::clone(&live_children);
        let thread = thread::spawn(move || {
            while !thread_stop.load(Ordering::Acquire) {
                match listener.accept() {
                    Ok((stream, _addr)) => {
                        if let Err(err) = stream.set_nonblocking(false) {
                            eprintln!(
                                "herdr: remote bridge failed to prepare client socket: {err}"
                            );
                            continue;
                        }
                        let worker_ssh = Arc::clone(&ssh);
                        let worker_remote_herdr = remote_herdr.clone();
                        let worker_session_name = session_name.clone();
                        let worker_children = Arc::clone(&thread_children);
                        spawn_bridge_worker(stream, move |stream| {
                            bridge_connection(
                                stream,
                                &worker_ssh,
                                &worker_remote_herdr,
                                &worker_session_name,
                                kind,
                                &worker_children,
                            )
                        });
                    }
                    Err(err) if err.kind() == io::ErrorKind::WouldBlock => {
                        thread::sleep(BRIDGE_ACCEPT_POLL);
                    }
                    Err(err) => {
                        eprintln!("herdr: remote bridge listener failed: {err}");
                        break;
                    }
                }
            }
        });

        Ok(Self {
            local_socket,
            should_stop,
            thread: Some(thread),
            live_children,
        })
    }
}

impl Drop for SshStdioBridge {
    fn drop(&mut self) {
        self.should_stop.store(true, Ordering::Release);
        let _ = std::fs::remove_file(&self.local_socket);
        // Terminate the live per-connection ssh children so their remote
        // `remote-*-bridge` counterparts exit too; the workers observe the
        // child exit, close their streams, and unwind.
        terminate_tracked_children(&self.live_children);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// SIGTERM every tracked child pid. Factored out of `SshStdioBridge::drop`
/// so the kill path is unit-testable with a real spawned child.
fn terminate_tracked_children(children: &std::sync::Mutex<Vec<u32>>) {
    let Ok(children) = children.lock() else {
        return;
    };
    for pid in children.iter() {
        // SAFETY: plain kill(2) on a pid we spawned and still track.
        unsafe {
            libc::kill(*pid as libc::pid_t, libc::SIGTERM);
        }
    }
}

/// Creates a fresh user-only (`0700`) directory for the generated ssh config
/// and control socket, returning its path.
///
/// Using a private directory created with fail-if-exists semantics — rather
/// than a predictable file in the world-writable temp dir — stops a local user
/// from pre-planting a symlink or world-writable file that herdr would write
/// and `ssh -F` would then read.
fn private_ssh_config_dir() -> io::Result<PathBuf> {
    use std::os::unix::fs::DirBuilderExt;

    let mut bases = vec![std::env::temp_dir()];
    let short_tmp = PathBuf::from("/tmp");
    if bases.first() != Some(&short_tmp) {
        bases.push(short_tmp);
    }

    let mut last_error = None;
    for base in bases {
        for attempt in 0..100 {
            let dir = base.join(format!("herdr-ssh-{}-{attempt}", std::process::id()));
            if !fits_unix_socket_path(&dir.join(SSH_CONTROL_SOCKET_NAME)) {
                continue;
            }
            match fs::DirBuilder::new().mode(0o700).create(&dir) {
                Ok(()) => return Ok(dir),
                Err(err) if err.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(err) => {
                    last_error = Some(err);
                    break;
                }
            }
        }
    }

    Err(last_error.unwrap_or_else(|| {
        io::Error::new(
            io::ErrorKind::AlreadyExists,
            "failed to create private herdr ssh config directory",
        )
    }))
}

/// Quotes a path for an ssh_config `Include` so a path containing spaces (or
/// glob metacharacters) is treated as one literal token instead of being split
/// or expanded by ssh — otherwise the user's config might not be Included and
/// herdr's fallback would wrongly take effect.
fn ssh_config_quote(path: &str) -> String {
    format!("\"{path}\"")
}

/// Builds a temporary ssh config for remote attach commands without overriding
/// the user's own settings, returning its path.
///
/// The file `Include`s the user's real ssh config first, so ssh's
/// first-value-wins rule keeps any `ServerAlive*` the user set there (including
/// an explicit `0` to disable it). Herdr's keepalive values apply only when
/// the user has none.
fn write_managed_ssh_config() -> io::Result<ManagedSshConfig> {
    use std::os::unix::fs::OpenOptionsExt;

    let dir = private_ssh_config_dir()?;
    let path = dir.join("config");
    let control_path = dir.join(SSH_CONTROL_SOCKET_NAME);

    let mut contents = String::new();
    if let Some(home) = std::env::var_os("HOME") {
        let user_config = PathBuf::from(home).join(".ssh").join("config");
        if user_config.is_file() {
            contents.push_str(&format!(
                "Include {}\n",
                ssh_config_quote(&user_config.to_string_lossy())
            ));
        }
    }
    if Path::new("/etc/ssh/ssh_config").is_file() {
        contents.push_str("Include /etc/ssh/ssh_config\n");
    }
    contents.push_str("Host *\n");
    contents.push_str("  ServerAliveInterval 15\n");
    contents.push_str("  ServerAliveCountMax 4\n");

    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(BRIDGE_SOCKET_PERMISSION_MODE)
        .open(&path)?;
    file.write_all(contents.as_bytes())?;
    Ok(ManagedSshConfig {
        options: ManagedSshOptions {
            config_path: path,
            control_path,
        },
    })
}

fn bridge_connection(
    stream: UnixStream,
    ssh: &RemoteSsh,
    remote_herdr: &RemoteHerdr,
    session_name: &str,
    kind: RemoteBridgeKind,
    live_children: &std::sync::Mutex<Vec<u32>>,
) -> io::Result<()> {
    let mut command = ssh.command();
    command.arg(remote_bridge_command(remote_herdr, session_name, kind));
    command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        // Never inherit the bridge ssh's stderr: this runs inside the raw-mode TUI client, so any
        // ssh chatter (host-key notices, multiplexing notes, transient warnings) would corrupt /
        // spam the screen. A genuine bridge failure surfaces as a dropped stream → reconnect, and
        // connection-setup errors are already reported by the detect/install phase.
        .stderr(Stdio::null());

    let mut child = command
        .spawn()
        .map_err(|err| io::Error::new(err.kind(), format!("failed to start ssh bridge: {err}")))?;
    let child_pid = child.id();
    if let Ok(mut children) = live_children.lock() {
        children.push(child_pid);
    }
    let mut child_stdin = child
        .stdin
        .take()
        .ok_or_else(|| io::Error::new(io::ErrorKind::BrokenPipe, "ssh bridge stdin missing"))?;
    let mut child_stdout = child
        .stdout
        .take()
        .ok_or_else(|| io::Error::new(io::ErrorKind::BrokenPipe, "ssh bridge stdout missing"))?;
    let mut stream_to_child = stream.try_clone()?;
    let mut child_to_stream = stream;

    let upload = thread::spawn(move || {
        let _ = copy_flush(&mut stream_to_child, &mut child_stdin);
    });
    let download = thread::spawn(move || {
        let _ = copy_flush(&mut child_stdout, &mut child_to_stream);
        let _ = child_to_stream.shutdown(std::net::Shutdown::Write);
    });

    let status = child.wait();
    if let Ok(mut children) = live_children.lock() {
        children.retain(|pid| *pid != child_pid);
    }
    let status = status?;
    let _ = upload.join();
    let _ = download.join();

    if status.success() {
        Ok(())
    } else {
        Err(io::Error::new(
            io::ErrorKind::ConnectionAborted,
            format!("ssh bridge exited with {status}"),
        ))
    }
}

fn copy_flush<R: io::Read, W: io::Write>(reader: &mut R, writer: &mut W) -> io::Result<u64> {
    let mut buffer = [0_u8; 16 * 1024];
    let mut total = 0;

    loop {
        let bytes_read = match reader.read(&mut buffer) {
            Ok(0) => return Ok(total),
            Ok(bytes_read) => bytes_read,
            Err(err) if err.kind() == io::ErrorKind::Interrupted => continue,
            Err(err) => return Err(err),
        };

        writer.write_all(&buffer[..bytes_read])?;
        writer.flush()?;
        total += bytes_read as u64;
    }
}

fn run_client_process(
    local_client_socket: &Path,
    local_api_socket: &Path,
    reattach_command: &str,
    keybindings: RemoteKeybindings,
    main_remote_target: &str,
) -> io::Result<()> {
    let exe = std::env::current_exe()?;
    let status = remote_client_command(
        &exe,
        local_client_socket,
        local_api_socket,
        reattach_command,
        keybindings,
        main_remote_target,
    )
    .stdin(Stdio::inherit())
    .stdout(Stdio::inherit())
    .stderr(Stdio::inherit())
    .status()?;

    if status.success() {
        Ok(())
    } else {
        Err(io::Error::new(
            io::ErrorKind::Interrupted,
            format!("remote client exited with {status}"),
        ))
    }
}

fn remote_client_command(
    exe: &Path,
    local_client_socket: &Path,
    local_api_socket: &Path,
    reattach_command: &str,
    keybindings: RemoteKeybindings,
    main_remote_target: &str,
) -> Command {
    let mut command = Command::new(exe);
    command
        .arg("client")
        .env(
            crate::server::socket_paths::CLIENT_SOCKET_PATH_ENV_VAR,
            local_client_socket,
        )
        .env(crate::api::SOCKET_PATH_ENV_VAR, local_api_socket)
        .env("HERDR_RENDER_ENCODING", "terminal-ansi")
        .env(REATTACH_COMMAND_ENV_VAR, reattach_command)
        .env(MAIN_DISPLAY_NAME_ENV_VAR, main_remote_target)
        .env(MAIN_REMOTE_TARGET_ENV_VAR, main_remote_target)
        .env(REMOTE_KEYBINDINGS_ENV_VAR, keybindings.as_str());
    command
}

/// Bytes `derive_client_socket_from_api_socket` inserts before `.sock`
/// ("-client"), reserved when sizing the api socket name so the DERIVED client
/// socket path also stays under the sun_path ceiling.
const DERIVED_CLIENT_SUFFIX_RESERVE: usize = "-client".len();

fn local_forward_api_socket_path(target: &str, session_name: &str) -> PathBuf {
    let pid = std::process::id();
    let target_clean = sanitize_path_component(target);
    let session_clean = sanitize_path_component(session_name);

    let tmpdir = std::env::temp_dir();
    let readable = tmpdir.join(format!(
        "herdr-remote-{pid}-{target_clean}-{session_clean}-api.sock"
    ));
    if fits_unix_socket_path_with_reserve(&readable, DERIVED_CLIENT_SUFFIX_RESERVE) {
        return readable;
    }

    // macOS' per-user TMPDIR (~49 chars under /var/folders/...) can push the
    // readable name past sun_path's 104-byte ceiling. Fall back to a hashed
    // short name in TMPDIR, then to /tmp as a last resort when TMPDIR itself
    // is longer than the budget. The hash covers the full unsanitized
    // target/session so uniqueness does not depend on the prefix truncation;
    // the prefix is kept only for debuggability.
    let target_prefix: String = target_clean.chars().take(8).collect();
    let hash = short_socket_hash(target, session_name, "api");
    let short_name = format!("herdr-r-{pid}-{target_prefix}-api.{hash}.sock");
    let short_in_tmp = tmpdir.join(&short_name);
    if fits_unix_socket_path_with_reserve(&short_in_tmp, DERIVED_CLIENT_SUFFIX_RESERVE) {
        return short_in_tmp;
    }
    PathBuf::from("/tmp").join(short_name)
}

fn remote_bridge_socket_paths(target: &str, session_name: &str) -> RemoteBridgePaths {
    let api_socket = local_forward_api_socket_path(target, session_name);
    // The spawned remote client is launched with BOTH socket env overrides set,
    // and the resolution contract makes the api override win: the client
    // DERIVES its client socket from `HERDR_SOCKET_PATH` and ignores
    // `HERDR_CLIENT_SOCKET_PATH`. Bind the client bridge listener at exactly
    // that derived path so the client's resolution lands on a bound socket
    // (binding anything else fails the attach with ENOENT).
    let client_socket =
        crate::server::socket_paths::derive_client_socket_from_api_socket(&api_socket);
    RemoteBridgePaths {
        client_socket,
        api_socket,
    }
}

fn fits_unix_socket_path(path: &Path) -> bool {
    fits_unix_socket_path_with_reserve(path, 0)
}

fn fits_unix_socket_path_with_reserve(path: &Path, reserve: usize) -> bool {
    use std::os::unix::ffi::OsStrExt;
    // sun_path is byte-limited: 104 bytes on macOS, 108 on Linux. Reserve
    // 1 byte for the trailing NUL and use the smaller cap for portability.
    const MAX: usize = 103;
    path.as_os_str().as_bytes().len() + reserve <= MAX
}

fn short_socket_hash(target: &str, session: &str, kind: &str) -> String {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};
    let mut hasher = DefaultHasher::new();
    target.hash(&mut hasher);
    0u8.hash(&mut hasher);
    session.hash(&mut hasher);
    0u8.hash(&mut hasher);
    kind.hash(&mut hasher);
    format!("{:016x}", hasher.finish())
}

fn sanitize_path_component(input: &str) -> String {
    let sanitized: String = input
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || matches!(ch, '.' | '_' | '-') {
                ch
            } else {
                '-'
            }
        })
        .collect();

    sanitized.trim_matches('-').chars().take(32).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn terminate_tracked_children_kills_live_child() {
        // A tracked child must die on bridge drop; otherwise removal/disable
        // leaks the local ssh and the remote bridge process until client exit.
        let mut child = std::process::Command::new("/bin/sleep")
            .arg("30")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn sleep");
        let children = std::sync::Mutex::new(vec![child.id()]);

        terminate_tracked_children(&children);

        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            match child.try_wait().expect("try_wait") {
                Some(status) => {
                    assert!(!status.success(), "sleep should be terminated, not exit 0");
                    break;
                }
                None if Instant::now() < deadline => {
                    thread::sleep(Duration::from_millis(20));
                }
                None => panic!("tracked child survived terminate_tracked_children"),
            }
        }
    }

    fn plain_remote_ssh(target: SshTarget) -> RemoteSsh {
        RemoteSsh {
            target,
            managed_config: None,
        }
    }

    fn ssh_argv(target: &SshTarget, remote_command: &str) -> Vec<String> {
        let ssh = plain_remote_ssh(target.clone());
        let mut command = ssh.command();
        command.arg(remote_command);
        command
            .get_args()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect()
    }

    #[test]
    fn ssh_target_command_inserts_dash_t_before_bare_destination() {
        assert_eq!(
            ssh_argv(&SshTarget::bare("iq-64"), "uname -s"),
            ["-o", "ConnectTimeout=10", "-T", "iq-64", "uname -s"]
        );
    }

    #[test]
    fn ssh_target_command_emits_options_before_destination() {
        let target = SshTarget::new(
            "iq-64",
            vec![
                "-L".into(),
                "9000:localhost:9000".into(),
                "-J".into(),
                "jump".into(),
            ],
        );
        assert_eq!(
            ssh_argv(&target, "uname -s"),
            [
                "-o",
                "ConnectTimeout=10",
                "-L",
                "9000:localhost:9000",
                "-J",
                "jump",
                "-T",
                "iq-64",
                "uname -s"
            ]
        );
    }

    #[test]
    fn ssh_target_command_does_not_duplicate_user_supplied_dash_t() {
        let target = SshTarget::new("iq-64", vec!["-T".into()]);
        assert_eq!(
            ssh_argv(&target, "x"),
            ["-o", "ConnectTimeout=10", "-T", "iq-64", "x"]
        );
    }

    #[test]
    fn ssh_target_command_respects_user_connect_timeout() {
        let target = SshTarget::new("iq-64", vec!["-o".into(), "ConnectTimeout=3".into()]);
        assert_eq!(
            ssh_argv(&target, "x"),
            ["-o", "ConnectTimeout=3", "-T", "iq-64", "x"]
        );
    }

    fn probe_lines(version: &str, protocol: u32, bridge_ok: bool) -> String {
        format!(
            "HERDR_PROBE_VERSION {version}\nHERDR_PROBE_STATUS {{\"protocol\":{protocol}}}\n{}\n",
            if bridge_ok {
                "HERDR_PROBE_BRIDGE_OK"
            } else {
                "HERDR_PROBE_BRIDGE_MISSING"
            }
        )
    }

    #[test]
    fn probe_reports_compatible_for_matching_binary() {
        let stdout = probe_lines(
            &format!("herdr {}", current_version()),
            CURRENT_PROTOCOL,
            true,
        );
        assert_eq!(
            interpret_remote_binary_probe(&stdout),
            RemoteBinaryCheck::Compatible
        );
    }

    #[test]
    fn probe_reports_protocol_mismatch_when_version_matches_but_protocol_is_old() {
        // The case where the version matched, but the installed asset spoke an older protocol.
        let stdout = probe_lines(&format!("herdr {}", current_version()), 6, true);
        assert_eq!(
            interpret_remote_binary_probe(&stdout),
            RemoteBinaryCheck::ProtocolMismatch { reported: Some(6) }
        );
    }

    #[test]
    fn probe_reports_missing_bridge_when_subcommands_absent() {
        let stdout = probe_lines(
            &format!("herdr {}", current_version()),
            CURRENT_PROTOCOL,
            false,
        );
        assert_eq!(
            interpret_remote_binary_probe(&stdout),
            RemoteBinaryCheck::MissingBridgeSupport
        );
    }

    #[test]
    fn probe_reports_version_mismatch_before_protocol() {
        let stdout = probe_lines("herdr 0.5.10", 6, true);
        assert_eq!(
            interpret_remote_binary_probe(&stdout),
            RemoteBinaryCheck::VersionMismatch {
                reported: "herdr 0.5.10".to_string()
            }
        );
    }

    #[test]
    fn probe_reports_not_executable_and_unintelligible() {
        assert_eq!(
            interpret_remote_binary_probe("HERDR_PROBE_NOT_EXECUTABLE\n"),
            RemoteBinaryCheck::NotExecutable
        );
        assert_eq!(
            interpret_remote_binary_probe("HERDR_PROBE_NO_VERSION\n"),
            RemoteBinaryCheck::Unintelligible
        );
        assert_eq!(
            interpret_remote_binary_probe(""),
            RemoteBinaryCheck::Unintelligible
        );
    }

    #[test]
    fn protocol_mismatch_install_message_is_actionable_and_not_about_version() {
        let msg = RemoteBinaryCheck::ProtocolMismatch { reported: Some(6) }
            .install_failure_message("$HOME/.local/bin/herdr");
        assert!(msg.contains("protocol 6"));
        assert!(msg.contains("HERDR_REMOTE_BINARY"));
        assert!(
            !msg.contains("did not report version"),
            "protocol mismatch must not be reported as a version problem: {msg}"
        );
    }

    #[test]
    fn non_interactive_attaches_to_protocol_compatible_running_server_without_handoff() {
        // Version/binary/daemon differs but protocol matches: attach to the running server, no
        // restart.
        for reason in [
            RemoteServerRestartReason::VersionMismatch,
            RemoteServerRestartReason::BinaryUpdated,
            RemoteServerRestartReason::DaemonDetachMissing,
        ] {
            assert_eq!(
                non_interactive_server_action(reason, false, false),
                NonInteractiveServerAction::AttachExisting
            );
            // Even if handoff is enabled, an unsupported server still attaches as-is.
            assert_eq!(
                non_interactive_server_action(reason, true, false),
                NonInteractiveServerAction::AttachExisting
            );
        }
    }

    #[test]
    fn non_interactive_prefers_live_handoff_when_available() {
        for reason in [
            RemoteServerRestartReason::ProtocolMismatch,
            RemoteServerRestartReason::VersionMismatch,
            RemoteServerRestartReason::BinaryUpdated,
            RemoteServerRestartReason::DaemonDetachMissing,
        ] {
            assert_eq!(
                non_interactive_server_action(reason, true, true),
                NonInteractiveServerAction::LiveHandoff
            );
        }
    }

    #[test]
    fn non_interactive_protocol_mismatch_without_handoff_is_stuck_not_hard_stopped() {
        // The key safety property: a protocol mismatch we cannot hand off is reported as stuck,
        // never resolved by hard-stopping (which would kill the remote server's panes).
        assert_eq!(
            non_interactive_server_action(RemoteServerRestartReason::ProtocolMismatch, true, false),
            NonInteractiveServerAction::ProtocolStuck
        );
        assert_eq!(
            non_interactive_server_action(RemoteServerRestartReason::ProtocolMismatch, false, true),
            NonInteractiveServerAction::ProtocolStuck
        );
    }

    #[test]
    fn restart_confirm_needed_is_downcast_from_io_error() {
        let err = io::Error::other(RestartConfirmNeeded {
            destination: "iq-64".to_string(),
            version: Some("0.6.0".to_string()),
            protocol: Some(6),
        });

        let needed = restart_confirm_needed(&err).expect("typed restart signal");
        assert_eq!(needed.destination, "iq-64");
        assert_eq!(needed.version.as_deref(), Some("0.6.0"));
        assert_eq!(needed.protocol, Some(6));
        let message = needed.to_string();
        assert!(message.contains("iq-64"), "got {message}");
        assert!(message.contains("protocol 6"), "got {message}");

        assert!(restart_confirm_needed(&io::Error::other("plain")).is_none());
    }

    #[test]
    fn bridge_socket_is_user_only() {
        use std::os::unix::fs::PermissionsExt;

        let socket = std::env::temp_dir().join(format!(
            "herdr-bridge-permissions-test-{}.sock",
            std::process::id()
        ));
        let remote_herdr = RemoteHerdr::for_platform(RemotePlatform {
            os: "linux",
            arch: "x86_64",
        });
        let bridge = SshStdioBridge::start(
            Arc::new(plain_remote_ssh(SshTarget::bare("example"))),
            remote_herdr,
            socket.clone(),
            "default".to_string(),
            RemoteBridgeKind::Client,
        )
        .expect("start bridge listener");

        let mode = std::fs::metadata(&socket).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, BRIDGE_SOCKET_PERMISSION_MODE);

        drop(bridge);
        let _ = std::fs::remove_file(socket);
    }

    #[test]
    fn bridge_worker_returns_before_connection_finishes() {
        let (stream, _peer) = UnixStream::pair().unwrap();
        let (finished_tx, finished_rx) = std::sync::mpsc::channel();

        let start = Instant::now();
        spawn_bridge_worker(stream, move |_| {
            thread::sleep(Duration::from_millis(200));
            finished_tx.send(()).unwrap();
            Ok(())
        });

        assert!(start.elapsed() < Duration::from_millis(50));
        assert!(finished_rx.recv_timeout(Duration::from_millis(50)).is_err());
    }

    #[test]
    fn remote_bridge_exposes_socket_paths() {
        let bridge = RemoteBridge::from_socket_paths_for_test(
            PathBuf::from("/tmp/herdr-client.sock"),
            PathBuf::from("/tmp/herdr-api.sock"),
        );

        assert_eq!(
            bridge.client_socket_path(),
            Path::new("/tmp/herdr-client.sock")
        );
        assert_eq!(bridge.api_socket_path(), Path::new("/tmp/herdr-api.sock"));
    }

    #[test]
    fn managed_ssh_config_includes_user_config_then_fallback() {
        use std::os::unix::fs::PermissionsExt;

        let managed_config = write_managed_ssh_config().expect("write managed config");
        let path = managed_config.options.config_path.clone();
        let control_path = managed_config.options.control_path.clone();
        let contents = std::fs::read_to_string(&path).expect("read keepalive config");

        // herdr's fallback transport settings are present...
        assert!(
            contents.contains("Host *"),
            "config should add a Host * fallback block: {contents}"
        );
        assert!(
            contents.contains("ServerAliveInterval 15"),
            "config should set the keepalive interval: {contents}"
        );
        assert!(
            contents.contains("ServerAliveCountMax 4"),
            "config should set the keepalive count: {contents}"
        );
        assert!(!contents.contains("ControlMaster"));
        assert!(!contents.contains("ControlPersist"));
        assert!(!contents.contains("ControlPath"));
        // ...and any user config is Included (quoted) BEFORE it so
        // first-value-wins keeps the user's own settings.
        if let Some(home) = std::env::var_os("HOME") {
            let user_config = PathBuf::from(home).join(".ssh").join("config");
            if user_config.is_file() {
                let include = format!(
                    "Include {}",
                    ssh_config_quote(&user_config.to_string_lossy())
                );
                let include_at = contents.find(&include).expect("user config Included");
                let fallback_at = contents.find("Host *").expect("fallback present");
                assert!(
                    include_at < fallback_at,
                    "user config must be Included before herdr's fallback: {contents}"
                );
            }
        }

        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(
            mode, BRIDGE_SOCKET_PERMISSION_MODE,
            "keepalive config must be user-only"
        );
        // The config lives in a private 0700 dir, not a predictable temp path.
        let dir = path.parent().expect("config has a parent dir");
        let dir_mode = std::fs::metadata(dir).unwrap().permissions().mode() & 0o777;
        assert_eq!(dir_mode, 0o700, "ssh config dir must be user-only");
        assert!(
            fits_unix_socket_path(&control_path),
            "control socket path must fit portable Unix socket limits"
        );

        drop(managed_config);
    }

    #[test]
    fn ssh_config_quote_wraps_path_with_spaces() {
        assert_eq!(
            ssh_config_quote("/home/a b/.ssh/config"),
            "\"/home/a b/.ssh/config\""
        );
    }

    #[test]
    fn remote_ssh_command_uses_managed_config_when_present() {
        let managed_config = write_managed_ssh_config().expect("write managed config");
        let config_path = managed_config.options.config_path.clone();
        let control_path = managed_config.options.control_path.clone();
        let ssh = RemoteSsh {
            target: SshTarget::bare("example"),
            managed_config: Some(managed_config),
        };

        let command = ssh.command();
        let args = command
            .get_args()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect::<Vec<_>>();

        assert_eq!(
            args,
            vec![
                "-F".to_string(),
                config_path.to_string_lossy().into_owned(),
                "-S".to_string(),
                control_path.to_string_lossy().into_owned(),
                "-o".to_string(),
                "ControlMaster=auto".to_string(),
                "-o".to_string(),
                "ControlPersist=60".to_string(),
                "-o".to_string(),
                "ConnectTimeout=10".to_string(),
                "-T".to_string(),
                "example".to_string(),
            ]
        );
    }

    #[test]
    fn remote_ssh_command_is_plain_without_managed_config() {
        let ssh = plain_remote_ssh(SshTarget::bare("example"));

        let command = ssh.command();
        let args = command
            .get_args()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect::<Vec<_>>();

        assert_eq!(
            args,
            vec![
                "-o".to_string(),
                "ConnectTimeout=10".to_string(),
                "-T".to_string(),
                "example".to_string(),
            ]
        );
    }

    #[test]
    fn remote_install_stream_command_avoids_shell_c_wrapper() {
        let command = remote_install_stream_command("/home/a b/.local/bin/herdr.tmp.123");

        assert_eq!(command, "tee '/home/a b/.local/bin/herdr.tmp.123'");
    }

    #[test]
    fn remote_install_prepare_and_commit_scripts_quote_paths() {
        let remote_herdr = RemoteHerdr::for_platform(RemotePlatform {
            os: "linux",
            arch: "x86_64",
        });
        let prepare = remote_install_prepare_script(&remote_herdr);

        assert!(prepare.contains("mkdir -p \"$dir\""));
        assert!(prepare.contains("printf '%s\\0%s\\0' \"$tmp\" \"$dest\""));
        assert_eq!(
            parse_remote_install_paths(b"/home/a b/herdr.tmp.42\0/home/a b/herdr\0").unwrap(),
            (
                "/home/a b/herdr.tmp.42".to_string(),
                "/home/a b/herdr".to_string()
            )
        );
        assert_eq!(
            parse_remote_install_paths(b"/home/a b\n/herdr.tmp.42\0/home/a b\n/herdr\0").unwrap(),
            (
                "/home/a b\n/herdr.tmp.42".to_string(),
                "/home/a b\n/herdr".to_string()
            )
        );
        assert_eq!(
            remote_install_commit_script("/home/a b/herdr.tmp.42", "/home/a b/herdr"),
            "set -eu\nchmod 755 '/home/a b/herdr.tmp.42'\nmv '/home/a b/herdr.tmp.42' '/home/a b/herdr'\n"
        );
    }

    #[test]
    fn extract_remote_args_removes_space_form() {
        let args = vec![
            "herdr".into(),
            "--remote".into(),
            "dev".into(),
            "--help".into(),
        ];
        let (cleaned, remote) = extract_remote_args(&args).unwrap();
        assert_eq!(cleaned, vec!["herdr", "--help"]);
        let remote = remote.unwrap();
        assert_eq!(remote.target, "dev");
        assert_eq!(remote.keybindings, RemoteKeybindings::Local);
    }

    #[test]
    fn extract_remote_args_removes_equals_form() {
        let args = vec!["herdr".into(), "--remote=user@host".into()];
        let (cleaned, remote) = extract_remote_args(&args).unwrap();
        assert_eq!(cleaned, vec!["herdr"]);
        let remote = remote.unwrap();
        assert_eq!(remote.target, "user@host");
        assert_eq!(remote.keybindings, RemoteKeybindings::Local);
    }

    #[test]
    fn extract_remote_args_accepts_remote_keybindings_server() {
        let args = vec![
            "herdr".into(),
            "--remote".into(),
            "dev".into(),
            "--remote-keybindings=server".into(),
        ];
        let (cleaned, remote) = extract_remote_args(&args).unwrap();
        assert_eq!(cleaned, vec!["herdr"]);
        let remote = remote.unwrap();
        assert_eq!(remote.target, "dev");
        assert_eq!(remote.keybindings, RemoteKeybindings::Server);
    }

    #[test]
    fn extract_remote_args_accepts_remote_keybindings_space_form() {
        let args = vec![
            "herdr".into(),
            "--remote=dev".into(),
            "--remote-keybindings".into(),
            "server".into(),
        ];
        let (cleaned, remote) = extract_remote_args(&args).unwrap();
        assert_eq!(cleaned, vec!["herdr"]);
        assert_eq!(remote.unwrap().keybindings, RemoteKeybindings::Server);
    }

    #[test]
    fn extract_remote_args_accepts_explicit_handoff() {
        let args = vec!["herdr".into(), "--remote=dev".into(), "--handoff".into()];

        let (cleaned, remote) = extract_remote_args(&args).unwrap();

        assert_eq!(cleaned, vec!["herdr"]);
        let remote = remote.unwrap();
        assert_eq!(remote.target, "dev");
        assert!(remote.live_handoff);
    }

    #[test]
    fn extract_remote_args_preserves_child_remote_options_after_separator() {
        let args = vec![
            "herdr".into(),
            "agent".into(),
            "start".into(),
            "repro".into(),
            "--".into(),
            "child".into(),
            "--remote".into(),
            "dev".into(),
            "--remote-keybindings=server".into(),
            "--handoff".into(),
        ];

        let (cleaned, remote) = extract_remote_args(&args).unwrap();

        assert_eq!(cleaned, args);
        assert!(remote.is_none());
    }

    #[test]
    fn extract_remote_args_preserves_handoff_without_remote() {
        let args = vec!["herdr".into(), "update".into(), "--handoff".into()];

        let (cleaned, remote) = extract_remote_args(&args).unwrap();

        assert_eq!(cleaned, args);
        assert!(remote.is_none());
    }

    #[test]
    fn extract_remote_args_rejects_remote_keybindings_without_remote() {
        let args = vec!["herdr".into(), "--remote-keybindings=server".into()];
        let err = extract_remote_args(&args).unwrap_err();
        assert_eq!(err, "--remote-keybindings requires --remote");
    }

    #[test]
    fn extract_remote_args_rejects_duplicate_remote_keybindings() {
        let args = vec![
            "herdr".into(),
            "--remote=dev".into(),
            "--remote-keybindings=local".into(),
            "--remote-keybindings=server".into(),
        ];
        let err = extract_remote_args(&args).unwrap_err();
        assert_eq!(err, "--remote-keybindings can only be specified once");
    }

    #[test]
    fn extract_remote_args_requires_value() {
        let args = vec!["herdr".into(), "--remote".into()];
        let err = extract_remote_args(&args).unwrap_err();
        assert_eq!(err, "missing value for --remote");
    }

    #[test]
    fn extract_remote_args_rejects_empty_value() {
        let args = vec!["herdr".into(), "--remote=".into()];
        let err = extract_remote_args(&args).unwrap_err();
        assert_eq!(err, "missing value for --remote");
    }

    #[test]
    fn extract_remote_args_rejects_duplicate_values() {
        let args = vec![
            "herdr".into(),
            "--remote=dev".into(),
            "--remote=prod".into(),
        ];
        let err = extract_remote_args(&args).unwrap_err();
        assert_eq!(err, "--remote can only be specified once");
    }

    #[test]
    fn extract_remote_args_rejects_option_like_target() {
        let args = vec!["herdr".into(), "--remote".into(), "-oProxyCommand=x".into()];
        let err = extract_remote_args(&args).unwrap_err();
        assert_eq!(err, "--remote target must not start with '-'");
    }

    #[test]
    fn sanitize_path_component_removes_shell_sensitive_chars() {
        assert_eq!(sanitize_path_component("user@host:22"), "user-host-22");
    }

    #[test]
    fn remote_platform_maps_uname_values() {
        assert_eq!(
            RemotePlatform::from_uname("Linux", "amd64")
                .unwrap()
                .asset_key(),
            "linux-x86_64"
        );
        assert_eq!(
            RemotePlatform::from_uname("Darwin", "arm64")
                .unwrap()
                .asset_key(),
            "macos-aarch64"
        );
        assert!(RemotePlatform::from_uname("FreeBSD", "x86_64").is_none());
    }

    #[test]
    fn reattach_command_includes_remote_and_session() {
        assert_eq!(
            reattach_command(
                "target/release/herdr",
                "user@host",
                "work",
                RemoteKeybindings::Local,
                false,
            ),
            "target/release/herdr --remote user@host --session work"
        );
        assert_eq!(
            reattach_command(
                "herdr",
                "host name",
                crate::session::DEFAULT_SESSION_NAME,
                RemoteKeybindings::Local,
                false,
            ),
            "herdr --remote 'host name'"
        );
        assert_eq!(
            reattach_command(
                "herdr",
                "host",
                crate::session::DEFAULT_SESSION_NAME,
                RemoteKeybindings::Server,
                false,
            ),
            "herdr --remote host --remote-keybindings server"
        );
        assert_eq!(
            reattach_command(
                "herdr",
                "host",
                crate::session::DEFAULT_SESSION_NAME,
                RemoteKeybindings::Local,
                true,
            ),
            "herdr --remote host --handoff"
        );
    }

    #[test]
    fn remote_client_command_sets_main_target_metadata_env() {
        let command = remote_client_command(
            Path::new("/tmp/herdr"),
            Path::new("/tmp/herdr-client.sock"),
            Path::new("/tmp/herdr-api.sock"),
            "herdr --remote iq-64",
            RemoteKeybindings::Local,
            "iq-64",
        );
        let envs: BTreeMap<String, Option<String>> = command
            .get_envs()
            .map(|(key, value)| {
                (
                    key.to_string_lossy().to_string(),
                    value.map(|value| value.to_string_lossy().to_string()),
                )
            })
            .collect();

        assert_eq!(
            envs.get(MAIN_DISPLAY_NAME_ENV_VAR),
            Some(&Some("iq-64".to_string()))
        );
        assert_eq!(
            envs.get(MAIN_REMOTE_TARGET_ENV_VAR),
            Some(&Some("iq-64".to_string()))
        );
        assert_eq!(
            envs.get(crate::api::SOCKET_PATH_ENV_VAR),
            Some(&Some("/tmp/herdr-api.sock".to_string()))
        );
    }

    #[test]
    fn remote_bridge_command_uses_installed_binary() {
        let remote_herdr = RemoteHerdr::for_platform(RemotePlatform {
            os: "linux",
            arch: "x86_64",
        });
        assert_eq!(
            remote_bridge_command(
                &remote_herdr,
                crate::session::DEFAULT_SESSION_NAME,
                RemoteBridgeKind::Client,
            ),
            "exec \"$HOME/.local/bin/herdr\" remote-client-bridge"
        );
        assert_eq!(
            remote_bridge_command(
                &remote_herdr,
                crate::session::DEFAULT_SESSION_NAME,
                RemoteBridgeKind::Api,
            ),
            "exec \"$HOME/.local/bin/herdr\" remote-api-bridge"
        );
    }

    #[test]
    fn remote_binary_match_command_requires_bridge_probe() {
        let remote_herdr = RemoteHerdr::for_platform(RemotePlatform {
            os: "macos",
            arch: "aarch64",
        });

        assert_eq!(
            remote_binary_match_command(&remote_herdr),
            "test -x \"$HOME/.local/bin/herdr\" && \"$HOME/.local/bin/herdr\" --version && \"$HOME/.local/bin/herdr\" status client --json && HERDR_REMOTE_BRIDGE_PROBE=1 \"$HOME/.local/bin/herdr\" remote-client-bridge && HERDR_REMOTE_BRIDGE_PROBE=1 \"$HOME/.local/bin/herdr\" remote-api-bridge"
        );
    }

    #[test]
    fn remote_herdr_from_exe_name_uses_commit_labeled_binary() {
        let platform = RemotePlatform {
            os: "macos",
            arch: "aarch64",
        };
        let remote_herdr =
            remote_herdr_from_exe_name(platform, "herdr-39986ed").expect("commit binary");

        assert_eq!(
            remote_bridge_command(
                &remote_herdr,
                crate::session::DEFAULT_SESSION_NAME,
                RemoteBridgeKind::Api,
            ),
            "exec \"$HOME/.local/bin/herdr-39986ed\" remote-api-bridge"
        );
    }

    #[test]
    fn remote_herdr_from_exe_name_skips_plain_binary_name() {
        let platform = RemotePlatform {
            os: "macos",
            arch: "aarch64",
        };

        assert!(remote_herdr_from_exe_name(platform.clone(), "herdr").is_none());
        assert!(remote_herdr_from_exe_name(platform.clone(), "").is_none());
        assert!(remote_herdr_from_exe_name(platform, "herdr dev").is_none());
    }

    #[test]
    fn remote_path_discovery_uses_path_binary() {
        let remote_herdr = RemoteHerdr::for_platform(RemotePlatform {
            os: "linux",
            arch: "x86_64",
        });
        let remote_herdr = remote_herdr_from_path_discovery(&remote_herdr, "/usr/bin/herdr\n")
            .expect("path binary");

        assert_eq!(
            remote_bridge_command(
                &remote_herdr,
                crate::session::DEFAULT_SESSION_NAME,
                RemoteBridgeKind::Client,
            ),
            "exec /usr/bin/herdr remote-client-bridge"
        );
    }

    #[test]
    fn remote_path_discovery_quotes_discovered_binary() {
        let remote_herdr = RemoteHerdr::for_platform(RemotePlatform {
            os: "linux",
            arch: "x86_64",
        });
        let remote_herdr =
            remote_herdr_from_path_discovery(&remote_herdr, "/opt/herdr bin/herdr\n")
                .expect("path binary");

        assert_eq!(
            remote_bridge_command(
                &remote_herdr,
                crate::session::DEFAULT_SESSION_NAME,
                RemoteBridgeKind::Client,
            ),
            "exec '/opt/herdr bin/herdr' remote-client-bridge"
        );
    }

    #[test]
    fn remote_path_discovery_uses_macos_path_binary() {
        let remote_herdr = RemoteHerdr::for_platform(RemotePlatform {
            os: "macos",
            arch: "aarch64",
        });
        let remote_herdr =
            remote_herdr_from_path_discovery(&remote_herdr, "/opt/homebrew/bin/herdr\n")
                .expect("path binary");

        assert_eq!(
            remote_bridge_command(
                &remote_herdr,
                crate::session::DEFAULT_SESSION_NAME,
                RemoteBridgeKind::Client,
            ),
            "exec /opt/homebrew/bin/herdr remote-client-bridge"
        );
        assert_eq!(remote_herdr.platform.asset_key(), "macos-aarch64");
    }

    #[test]
    fn remote_path_discovery_reads_multiple_absolute_paths() {
        let remote_herdr = RemoteHerdr::for_platform(RemotePlatform {
            os: "linux",
            arch: "x86_64",
        });
        let candidates = remote_herdrs_from_path_discovery(
            &remote_herdr,
            "/usr/bin/herdr\nbin/herdr\n /opt/herdr bin/herdr\n",
        );

        assert_eq!(candidates.len(), 2);
        assert_eq!(candidates[0].shell_path, "/usr/bin/herdr");
        assert_eq!(candidates[1].shell_path, "'/opt/herdr bin/herdr'");
    }

    #[test]
    fn remote_path_discovery_ignores_mise_shims() {
        let remote_herdr = RemoteHerdr::for_platform(RemotePlatform {
            os: "linux",
            arch: "x86_64",
        });
        let candidates = remote_herdrs_from_path_discovery(
            &remote_herdr,
            "/home/can/.local/share/mise/shims/herdr\n/home/can/.local/share/mise/installs/herdr/0.7.1/bin/herdr\n",
        );

        assert_eq!(candidates.len(), 1);
        assert_eq!(
            candidates[0].shell_path,
            "/home/can/.local/share/mise/installs/herdr/0.7.1/bin/herdr"
        );
    }

    #[test]
    fn known_remote_binary_candidate_script_includes_mise_and_nix_paths() {
        let script = known_remote_binary_candidate_script(&RemotePlatform {
            os: "linux",
            arch: "x86_64",
        });

        assert!(script.contains("emit \"$home/.local/bin/herdr\""));
        assert!(!script.contains("mise/shims/herdr"));
        assert!(script.contains(&format!("version={}", shell_quote(&current_version()))));
        assert!(
            script.contains("emit \"$home/.local/share/mise/installs/herdr/$version/bin/herdr\"")
        );
        assert!(script.contains(
            "emit \"$home/.local/share/mise/installs/github-ogulcancelik-herdr/$version/herdr\""
        ));
        assert!(script.contains("emit \"$home/.nix-profile/bin/herdr\""));
        assert!(script.contains("emit \"/etc/profiles/per-user/$user/bin/herdr\""));
        assert!(script.contains("emit \"/run/current-system/sw/bin/herdr\""));
        assert!(script.contains("emit \"/home/linuxbrew/.linuxbrew/bin/herdr\""));
        assert!(!script.contains("emit \"/opt/homebrew/bin/herdr\""));
    }

    #[test]
    fn known_remote_binary_candidate_script_includes_macos_homebrew_paths() {
        let script = known_remote_binary_candidate_script(&RemotePlatform {
            os: "macos",
            arch: "aarch64",
        });

        assert!(script.contains("emit \"/opt/homebrew/bin/herdr\""));
        assert!(script.contains("emit \"/usr/local/bin/herdr\""));
        assert!(!script.contains("emit \"/home/linuxbrew/.linuxbrew/bin/herdr\""));
    }

    #[test]
    fn remote_path_discovery_quotes_single_quotes_in_discovered_binary() {
        let remote_herdr = RemoteHerdr::for_platform(RemotePlatform {
            os: "linux",
            arch: "x86_64",
        });
        let remote_herdr =
            remote_herdr_from_path_discovery(&remote_herdr, "/opt/herdr's/bin/herdr\n")
                .expect("path binary");

        assert_eq!(
            remote_bridge_command(
                &remote_herdr,
                crate::session::DEFAULT_SESSION_NAME,
                RemoteBridgeKind::Client,
            ),
            "exec '/opt/herdr'\\''s/bin/herdr' remote-client-bridge"
        );
    }

    #[test]
    fn remote_path_discovery_ignores_relative_paths() {
        let remote_herdr = RemoteHerdr::for_platform(RemotePlatform {
            os: "linux",
            arch: "x86_64",
        });
        let remote_herdr = remote_herdr_from_path_discovery(&remote_herdr, "bin/herdr\n");

        assert!(remote_herdr.is_none());
    }

    #[test]
    fn remote_path_discovery_ignores_empty_output() {
        let remote_herdr = RemoteHerdr::for_platform(RemotePlatform {
            os: "linux",
            arch: "x86_64",
        });
        let remote_herdr = remote_herdr_from_path_discovery(&remote_herdr, "\n");

        assert!(remote_herdr.is_none());
    }

    #[test]
    fn remote_shell_path_warning_accepts_managed_install() {
        assert!(remote_shell_resolves_managed_install(
            "/home/can/.local/bin/herdr\n"
        ));
        assert!(remote_shell_resolves_managed_install(
            "/Users/can/.local/bin/herdr\n"
        ));
        assert!(!remote_shell_resolves_managed_install(
            "/usr/local/bin/herdr\n"
        ));
        assert!(!remote_shell_resolves_managed_install(""));
    }

    #[test]
    fn parse_client_status_json_reads_protocol() {
        assert_eq!(
            parse_client_status_json(r#"{"version":"x","protocol":8,"binary":"/bin/herdr"}"#)
                .map(|status| status.protocol),
            Some(8)
        );
        assert!(parse_client_status_json(r#"{"protocol":"unknown"}"#).is_none());
    }

    #[test]
    fn parse_remote_server_status_json_reads_running_server() {
        assert_eq!(
            parse_remote_server_status_json(
                r#"{"status":"running","running":true,"version":"0.6.0","protocol":8,"capabilities":{"live_handoff":true,"detached_server_daemon":true}}"#
            )
            .unwrap(),
            RemoteServerStatus::Running {
                version: Some("0.6.0".into()),
                protocol: Some(8),
                live_handoff: true,
                detached_server_daemon: true
            }
        );
    }

    #[test]
    fn parse_remote_server_status_json_treats_missing_capability_as_old_server() {
        assert_eq!(
            parse_remote_server_status_json(
                r#"{"status":"running","running":true,"version":"0.6.0","protocol":8}"#
            )
            .unwrap(),
            RemoteServerStatus::Running {
                version: Some("0.6.0".into()),
                protocol: Some(8),
                live_handoff: false,
                detached_server_daemon: false
            }
        );
    }

    #[test]
    fn parse_remote_server_status_json_reads_stopped_server() {
        assert_eq!(
            parse_remote_server_status_json(
                r#"{"status":"not_running","running":false,"version":null,"protocol":null}"#
            )
            .unwrap(),
            RemoteServerStatus::NotRunning
        );
    }

    #[test]
    fn remote_update_manifest_uses_root_assets_for_latest_version() {
        let manifest: RemoteUpdateManifest = serde_json::from_str(
            r#"{
                "version": "1.2.3",
                "assets": {
                    "linux-x86_64": "https://example.com/latest"
                },
                "releases": {
                    "1.2.3": {
                        "assets": {
                            "linux-x86_64": "https://example.com/archive"
                        }
                    }
                }
            }"#,
        )
        .unwrap();

        assert_eq!(
            manifest
                .release_for_version("1.2.3")
                .and_then(|release| release.assets.get("linux-x86_64"))
                .map(RemoteAssetRef::url),
            Some("https://example.com/latest")
        );
    }

    #[test]
    fn remote_update_manifest_reads_archived_release_assets() {
        let manifest: RemoteUpdateManifest = serde_json::from_str(
            r#"{
                "version": "1.2.4",
                "assets": {
                    "linux-x86_64": "https://example.com/latest"
                },
                "releases": {
                    "1.2.3": {
                        "notes": "ignored",
                        "assets": {
                            "linux-x86_64": "https://example.com/archive"
                        }
                    }
                }
            }"#,
        )
        .unwrap();

        assert_eq!(
            manifest
                .release_for_version("1.2.3")
                .and_then(|release| release.assets.get("linux-x86_64"))
                .map(RemoteAssetRef::url),
            Some("https://example.com/archive")
        );
    }

    #[test]
    fn remote_update_manifest_uses_archived_release_protocol() {
        let manifest: RemoteUpdateManifest = serde_json::from_str(
            r#"{
                "version": "1.2.4",
                "protocol": 42,
                "assets": {
                    "linux-x86_64": "https://example.com/latest"
                },
                "releases": {
                    "1.2.3": {
                        "notes": "ignored",
                        "protocol": 41,
                        "assets": {
                            "linux-x86_64": "https://example.com/archive"
                        }
                    }
                }
            }"#,
        )
        .unwrap();

        assert_eq!(
            manifest
                .release_for_version("1.2.3")
                .and_then(|release| release.protocol),
            Some(41)
        );
    }

    #[test]
    fn remote_update_manifest_does_not_inherit_latest_protocol_for_archived_assets() {
        let manifest: RemoteUpdateManifest = serde_json::from_str(
            r#"{
                "version": "1.2.4",
                "protocol": 42,
                "assets": {
                    "linux-x86_64": "https://example.com/latest"
                },
                "releases": {
                    "1.2.3": {
                        "notes": "ignored",
                        "assets": {
                            "linux-x86_64": "https://example.com/archive"
                        }
                    }
                }
            }"#,
        )
        .unwrap();

        assert_eq!(
            manifest
                .release_for_version("1.2.3")
                .and_then(|release| release.protocol),
            None
        );
    }

    #[test]
    fn remote_preview_manifest_falls_back_to_archived_exact_build_assets() {
        let manifest: RemotePreviewManifest = serde_json::from_str(
            r#"{
                "build_id": "2026-06-06-new",
                "protocol": 12,
                "assets": {
                    "linux-x86_64": {
                        "url": "https://example.com/new",
                        "sha256": "new"
                    }
                },
                "builds": {
                    "2026-06-02-old": {
                        "protocol": 11,
                        "assets": {
                            "linux-x86_64": {
                                "url": "https://example.com/old",
                                "sha256": "old"
                            }
                        }
                    }
                }
            }"#,
        )
        .unwrap();

        let (protocol, assets) =
            preview_assets_for_build(&manifest, "2026-06-02-old").expect("archived build");
        let asset = assets.get("linux-x86_64").expect("asset");
        assert_eq!(protocol, 11);
        assert_eq!(asset.url(), "https://example.com/old");
        assert_eq!(asset.sha256(), Some("old"));
    }

    #[test]
    fn remote_server_restart_reason_requires_stop_for_protocol_mismatch() {
        assert_eq!(
            remote_server_restart_reason(Some(&current_version()), Some(0), true, false),
            Some(RemoteServerRestartReason::ProtocolMismatch)
        );
    }

    #[test]
    fn remote_server_restart_reason_allows_unchanged_compatible_server() {
        assert_eq!(
            remote_server_restart_reason(
                Some(&current_version()),
                Some(CURRENT_PROTOCOL),
                true,
                false
            ),
            None
        );
    }

    #[test]
    fn remote_server_restart_reason_requires_restart_for_old_daemon() {
        assert_eq!(
            remote_server_restart_reason(
                Some(&current_version()),
                Some(CURRENT_PROTOCOL),
                false,
                false
            ),
            Some(RemoteServerRestartReason::DaemonDetachMissing)
        );
    }

    #[test]
    fn remote_server_restart_reason_requires_restart_after_helper_update() {
        assert_eq!(
            remote_server_restart_reason(
                Some(&current_version()),
                Some(CURRENT_PROTOCOL),
                true,
                true
            ),
            Some(RemoteServerRestartReason::BinaryUpdated)
        );
    }

    #[test]
    fn remote_server_restart_reason_offers_restart_for_version_mismatch() {
        assert_eq!(
            remote_server_restart_reason(Some("0.0.0"), Some(CURRENT_PROTOCOL), true, false),
            Some(RemoteServerRestartReason::VersionMismatch)
        );
        assert_eq!(
            remote_server_restart_reason(None, Some(CURRENT_PROTOCOL), true, false),
            Some(RemoteServerRestartReason::VersionMismatch)
        );
    }

    #[test]
    fn remote_server_restart_reason_allows_current_server() {
        assert_eq!(
            remote_server_restart_reason(
                Some(&current_version()),
                Some(CURRENT_PROTOCOL),
                true,
                false
            ),
            None
        );
    }

    #[test]
    fn remote_install_plan_keeps_compatible_running_server() {
        assert_eq!(
            remote_install_running_server_plan(
                Some(&current_version()),
                Some(CURRENT_PROTOCOL),
                true,
                false,
                false,
                false
            ),
            RemoteInstallRunningServerPlan::KeepRunning
        );
    }

    #[test]
    fn remote_install_plan_requires_stop_for_old_daemon() {
        assert_eq!(
            remote_install_running_server_plan(
                Some(&current_version()),
                Some(CURRENT_PROTOCOL),
                false,
                true,
                false,
                false
            ),
            RemoteInstallRunningServerPlan::StopRequired(
                RemoteServerRestartReason::DaemonDetachMissing
            )
        );
    }

    #[test]
    fn remote_install_plan_requires_stop_after_helper_update() {
        assert_eq!(
            remote_install_running_server_plan(
                Some(&current_version()),
                Some(CURRENT_PROTOCOL),
                true,
                true,
                false,
                false
            ),
            RemoteInstallRunningServerPlan::StopRequired(RemoteServerRestartReason::BinaryUpdated)
        );
    }

    #[test]
    fn remote_install_plan_requires_stop_for_incompatible_running_server() {
        assert_eq!(
            remote_install_running_server_plan(
                Some("0.0.0"),
                Some(CURRENT_PROTOCOL),
                true,
                true,
                false,
                false
            ),
            RemoteInstallRunningServerPlan::StopRequired(
                RemoteServerRestartReason::VersionMismatch
            )
        );
    }

    #[test]
    fn remote_install_plan_uses_live_handoff_for_incompatible_running_server() {
        assert_eq!(
            remote_install_running_server_plan(
                Some("0.0.0"),
                Some(CURRENT_PROTOCOL),
                true,
                true,
                true,
                true
            ),
            RemoteInstallRunningServerPlan::LiveHandoff
        );
    }

    #[test]
    fn install_source_description_uses_override_binary() {
        let platform = RemotePlatform {
            os: "linux",
            arch: "aarch64",
        };
        assert_eq!(
            install_source_description_for(&platform, Some(Path::new("/tmp/herdr-aarch64")), false),
            "HERDR_REMOTE_BINARY (/tmp/herdr-aarch64)"
        );
    }

    #[test]
    fn install_source_description_uses_local_binary_when_allowed() {
        let platform = RemotePlatform::local();

        assert_eq!(
            install_source_description_for(&platform, None, true),
            "the current local herdr binary"
        );
    }

    #[test]
    fn install_source_description_uses_release_asset_when_local_binary_cannot_seed_remote() {
        let platform = RemotePlatform::local();

        assert_eq!(
            install_source_description_for(&platform, None, false),
            format!(
                "the {} {} asset for {}",
                current_version(),
                current_channel(),
                platform.asset_key()
            )
        );
    }

    #[test]
    fn resolve_install_source_uses_override_binary_without_temporary_cleanup() {
        let platform = RemotePlatform {
            os: "linux",
            arch: "aarch64",
        };
        let source = resolve_install_source(&platform, Some(PathBuf::from("/tmp/herdr-aarch64")))
            .expect("override source");
        assert_eq!(source.path, PathBuf::from("/tmp/herdr-aarch64"));
        assert!(source.temporary_dir.is_none());
    }

    fn remote_env_lock() -> &'static std::sync::Mutex<()> {
        static LOCK: std::sync::OnceLock<std::sync::Mutex<()>> = std::sync::OnceLock::new();
        LOCK.get_or_init(|| std::sync::Mutex::new(()))
    }

    fn socket_path_byte_len(path: &Path) -> usize {
        use std::os::unix::ffi::OsStrExt;
        path.as_os_str().as_bytes().len()
    }

    #[test]
    fn local_forward_api_socket_path_uses_readable_name_when_it_fits() {
        let _guard = remote_env_lock().lock().unwrap();
        // Short target + session leave plenty of room — keep the human-
        // readable form so the socket path stays grep-friendly.
        let path = local_forward_api_socket_path("dev", "default");
        let filename = path
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("")
            .to_string();
        assert!(
            filename.starts_with("herdr-remote-"),
            "expected readable name, got {filename}"
        );
        assert!(filename.contains("-dev-default-api."), "got {filename}");
        assert!(
            fits_unix_socket_path(&path),
            "socket path too long: {} ({} bytes)",
            path.display(),
            socket_path_byte_len(&path)
        );
    }

    #[test]
    fn remote_bridge_socket_paths_are_distinct_for_client_and_api() {
        let _guard = remote_env_lock().lock().unwrap();
        let paths = remote_bridge_socket_paths("prod.example.com", "default");

        assert_ne!(paths.client_socket, paths.api_socket);
        assert!(paths
            .client_socket
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("")
            .contains("-client."));
        assert!(paths
            .api_socket
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("")
            .contains("-api."));
        assert!(fits_unix_socket_path(&paths.client_socket));
        assert!(fits_unix_socket_path(&paths.api_socket));
    }

    #[test]
    fn remote_bridge_client_socket_matches_spawned_client_resolution() {
        let _guard = remote_env_lock().lock().unwrap();
        // Regression: the spawned remote client is launched with BOTH
        // `HERDR_SOCKET_PATH` (api) and `HERDR_CLIENT_SOCKET_PATH` set, and the
        // resolution contract derives the client socket from the api override.
        // The bridge must bind its client listener at exactly that derived
        // path, or the client fails the attach with ENOENT on a socket that
        // was never bound (`...-api-client.sock` vs a bound `...-client.sock`).
        let paths = remote_bridge_socket_paths("devbox", "default");
        let resolved = crate::server::socket_paths::client_socket_path_from_overrides(
            paths.api_socket.to_str(),
            paths.client_socket.to_str(),
        );
        assert_eq!(resolved, paths.client_socket);
    }

    #[test]
    fn local_forward_api_socket_path_fits_in_sun_path_with_derived_client() {
        let _guard = remote_env_lock().lock().unwrap();
        // Worst case for the readable form: macOS-style 49-char TMPDIR +
        // max-length sanitized components. Should fall back to the hashed
        // short name, which fits under TMPDIR — and the DERIVED client socket
        // (api stem + "-client") must fit too.
        let target = "longish-host.example.com";
        let session = "a-fairly-long-session-name-here";
        let api_path = local_forward_api_socket_path(target, session);
        assert!(
            fits_unix_socket_path(&api_path),
            "api socket path too long for sun_path: {} ({} bytes)",
            api_path.display(),
            socket_path_byte_len(&api_path)
        );
        let client_path =
            crate::server::socket_paths::derive_client_socket_from_api_socket(&api_path);
        assert!(
            fits_unix_socket_path(&client_path),
            "derived client socket path too long for sun_path: {} ({} bytes)",
            client_path.display(),
            socket_path_byte_len(&client_path)
        );
    }

    #[test]
    fn local_forward_api_socket_path_falls_back_to_tmp_when_dir_is_long() {
        let _guard = remote_env_lock().lock().unwrap();
        // Force a TMPDIR long enough that even the hashed short name cannot
        // fit inside it. The fallback should drop to /tmp.
        let prior = std::env::var_os("TMPDIR");
        let long_dir = std::env::temp_dir().join("a".repeat(80));
        let _ = fs::create_dir_all(&long_dir);
        std::env::set_var("TMPDIR", &long_dir);

        let path = local_forward_api_socket_path("longish-host.example.com", "default");
        let fits = fits_unix_socket_path(&path);
        let parent = path.parent().map(Path::to_path_buf);
        let filename = path
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("")
            .to_string();

        match prior {
            Some(v) => std::env::set_var("TMPDIR", v),
            None => std::env::remove_var("TMPDIR"),
        }
        let _ = fs::remove_dir_all(&long_dir);

        assert!(fits, "fallback path still overflows: {}", path.display());
        assert_eq!(parent.as_deref(), Some(Path::new("/tmp")));
        assert!(
            filename.starts_with("herdr-r-"),
            "expected hashed fallback, got {filename}"
        );
    }

    #[test]
    fn install_source_cleanup_removes_temporary_directory() {
        let dir = std::env::temp_dir().join(format!(
            "herdr-install-source-cleanup-test-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir(&dir).expect("create temp dir");
        let path = dir.join("herdr.tmp");
        fs::write(&path, b"test").expect("write temp file");

        InstallSource::temporary(path, dir.clone()).cleanup();

        assert!(!dir.exists());
    }
}
