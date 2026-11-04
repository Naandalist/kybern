//! The clipboard between machines (ADE-86 M4 / U2, design §8.2).
//!
//! * **Explicit.** `clipboard.set` / `clipboard.get` read or write this machine's clipboard, or another
//!   machine's through the session (`peer.clipboard.set` / `peer.clipboard.get`, `machine:clipboard`).
//!   Text only, up to 1 MiB. A machine with no desktop session (a server, a daemon in a service session) keeps
//!   what it was sent in a Kybern buffer for 10 minutes, and a paste reads that buffer.
//! * **Universal clipboard**, off by default. A machine takes part when its owner turned on
//!   `allow.<machine>.clipboard_auto` (Settings → Machines → "Share clipboard automatically"). While it and at
//!   least one other online member have, [`run`] polls this machine's clipboard once a second and pushes each
//!   change that passes the secret filter ([`super::clipboard_filter`]) to the opted-in members as
//!   `peer.clipboard.auto`, sealed to the receiver's sealing key. Skips are counted by reason, never logged with
//!   content.
//! * **Backends** are the system's own tools, found at run time: `pbcopy` / `pbpaste` (and `osascript` for the
//!   concealed markers) on macOS, PowerShell's `Set-Clipboard` / `Get-Clipboard` on Windows (the daemon must
//!   run in the user's interactive session; it does under `schtasks` in session 2; the universal clipboard reads the
//!   text and the password-manager formats natively, in one snapshot), and `wl-copy` /
//!   `wl-paste`, else `xclip`, on Linux. Without one, the Kybern buffer is the clipboard.

use std::process::Stdio;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use base64::Engine;
use base64::engine::general_purpose::STANDARD as B64;
use kybern_protocol::methods::*;
use kybern_protocol::process::NoWindow;
use kybern_protocol::*;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tokio::io::AsyncWriteExt;
use tokio::process::Command;

use super::clipboard_filter::{Markers, SkipCounter, Verdict, classify};
use super::peers::PeerLink;
use crate::orchestrator::remote_delegation::CallFailure;
use crate::state::AppState;

const TARGET: &str = "kybern::network::clipboard";
const TOOL_TIMEOUT: Duration = Duration::from_secs(5);
const POLL_EVERY: Duration = Duration::from_secs(1);
const SEAL_CONTEXT: &str = "kybern-clipboard-v1";

/// What stands in for the operating system's clipboard.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Backend {
    /// The system's tools.
    Os,
    /// Only the Kybern buffer (a machine with no desktop).
    BufferOnly,
    /// An in-memory clipboard, for tests.
    Memory,
}

pub struct Clipboard {
    backend: Mutex<Backend>,
    buffer: Mutex<Option<(Instant, String)>>,
    memory: Mutex<Option<String>>,
    memory_markers: Mutex<Markers>,
    last_seen: AtomicU64,
    last_applied: AtomicU64,
    pub skips: SkipCounter,
}

impl Default for Clipboard {
    fn default() -> Self {
        Self {
            backend: Mutex::new(Backend::Os),
            buffer: Mutex::new(None),
            memory: Mutex::new(None),
            memory_markers: Mutex::new(Markers::default()),
            last_seen: AtomicU64::new(0),
            last_applied: AtomicU64::new(0),
            skips: SkipCounter::default(),
        }
    }
}

fn digest(text: &str) -> u64 {
    let hash = Sha256::digest(text.as_bytes());
    u64::from_be_bytes(hash[..8].try_into().expect("8 bytes")) | 1
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Run a tool, optionally feeding its stdin, and return its stdout. `None` when the tool is not installed.
async fn tool(program: &str, args: &[&str], input: Option<&str>) -> std::io::Result<Option<Vec<u8>>> {
    let mut command = Command::new(program).no_window();
    command.args(args).kill_on_drop(true);
    command.stdin(if input.is_some() { Stdio::piped() } else { Stdio::null() }).stdout(Stdio::piped()).stderr(Stdio::null());
    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    if let Some(input) = input
        && let Some(mut stdin) = child.stdin.take()
    {
        stdin.write_all(input.as_bytes()).await?;
        drop(stdin);
    }
    match tokio::time::timeout(TOOL_TIMEOUT, child.wait_with_output()).await {
        Ok(Ok(output)) if output.status.success() => Ok(Some(output.stdout)),
        Ok(Ok(output)) => Err(std::io::Error::other(format!("{program} exited with {}", output.status))),
        Ok(Err(error)) => Err(error),
        Err(_) => Err(std::io::Error::other(format!("{program} did not answer in time"))),
    }
}

impl Clipboard {
    /// Use only the Kybern buffer, whatever the system offers.
    pub fn use_buffer_only(&self) {
        *lock(&self.backend) = Backend::BufferOnly;
    }

    /// An in-memory clipboard in place of the system's (tests).
    #[cfg(test)]
    pub fn use_memory(&self) {
        *lock(&self.backend) = Backend::Memory;
    }

    /// The in-memory clipboard's text, to read or write as the user's own copy would (tests).
    #[cfg(test)]
    pub fn memory_for_test(&self) -> std::sync::MutexGuard<'_, Option<String>> {
        lock(&self.memory)
    }

    /// Forget what was applied, so the next look treats the clipboard as the user's own copy (tests).
    #[cfg(test)]
    pub fn forget_applied(&self) {
        self.last_applied.store(0, Ordering::Relaxed);
        self.last_seen.store(0, Ordering::Relaxed);
    }

    #[cfg(test)]
    pub fn set_memory_markers(&self, markers: Markers) {
        *lock(&self.memory_markers) = markers;
    }

    fn backend(&self) -> Backend {
        *lock(&self.backend)
    }

    /// The system clipboard's text; `Ok(None)` when this machine has none to read.
    async fn system_get(&self) -> std::io::Result<Option<String>> {
        let bytes = match self.backend() {
            Backend::BufferOnly => return Ok(None),
            Backend::Memory => return Ok(Some(lock(&self.memory).clone().unwrap_or_default())),
            Backend::Os => {
                if cfg!(target_os = "macos") {
                    tool("pbpaste", &[], None).await?
                } else if cfg!(windows) {
                    let script =
                        "[Console]::OutputEncoding=[Text.Encoding]::UTF8; $t = Get-Clipboard -Raw; if ($t) { [Console]::Out.Write($t) }";
                    tool("powershell", &["-NoProfile", "-NonInteractive", "-Command", script], None).await?
                } else if std::env::var_os("WAYLAND_DISPLAY").is_some() {
                    tool("wl-paste", &["--no-newline"], None).await?
                } else if std::env::var_os("DISPLAY").is_some() {
                    tool("xclip", &["-selection", "clipboard", "-o"], None).await?
                } else {
                    None
                }
            }
        };
        Ok(bytes.map(|bytes| String::from_utf8_lossy(&bytes).into_owned()))
    }

    /// Write the system clipboard; `Ok(false)` when this machine has none.
    async fn system_set(&self, text: &str) -> std::io::Result<bool> {
        let done = match self.backend() {
            Backend::BufferOnly => None,
            Backend::Memory => {
                *lock(&self.memory) = Some(text.to_string());
                Some(Vec::new())
            }
            Backend::Os => {
                if cfg!(target_os = "macos") {
                    tool("pbcopy", &[], Some(text)).await?
                } else if cfg!(windows) {
                    let script = "[Console]::InputEncoding=[Text.Encoding]::UTF8; $t = [Console]::In.ReadToEnd(); Set-Clipboard -Value $t";
                    tool("powershell", &["-NoProfile", "-NonInteractive", "-Command", script], Some(text)).await?
                } else if std::env::var_os("WAYLAND_DISPLAY").is_some() {
                    tool("wl-copy", &[], Some(text)).await?
                } else if std::env::var_os("DISPLAY").is_some() {
                    tool("xclip", &["-selection", "clipboard", "-i"], Some(text)).await?
                } else {
                    None
                }
            }
        };
        Ok(done.is_some())
    }

    /// The system clipboard's text with the markers its source set, read together. On Windows both come from one
    /// opened clipboard, so the markers belong to the text; a clipboard that cannot be read that way is an error, never
    /// a text without markers. Elsewhere the text is read, then the markers.
    async fn system_snapshot(&self) -> std::io::Result<Option<(String, Markers)>> {
        #[cfg(windows)]
        if self.backend() == Backend::Os {
            return tokio::task::spawn_blocking(windows_clipboard::snapshot).await.map_err(std::io::Error::other)?;
        }
        let Some(text) = self.system_get().await? else { return Ok(None) };
        Ok(Some((text, self.system_markers().await)))
    }

    /// The markers the source set on what is on the clipboard now (concealed by a password manager, transient).
    async fn system_markers(&self) -> Markers {
        let listing = match self.backend() {
            Backend::Memory => return *lock(&self.memory_markers),
            Backend::BufferOnly => return Markers::default(),
            Backend::Os => {
                if cfg!(target_os = "macos") {
                    tool("osascript", &["-e", "clipboard info"], None).await
                } else if std::env::var_os("WAYLAND_DISPLAY").is_some() && !cfg!(windows) {
                    tool("wl-paste", &["--list-types"], None).await
                } else if std::env::var_os("DISPLAY").is_some() && !cfg!(windows) {
                    tool("xclip", &["-selection", "clipboard", "-t", "TARGETS", "-o"], None).await
                } else {
                    Ok(None)
                }
            }
        };
        let Ok(Some(listing)) = listing else { return Markers::default() };
        let listing = String::from_utf8_lossy(&listing);
        let mut markers = Markers::default();
        for name in [
            "org.nspasteboard.ConcealedType",
            "org.nspasteboard.TransientType",
            "org.nspasteboard.AutoGeneratedType",
            "x-kde-passwordManagerHint",
        ] {
            if listing.contains(name) {
                markers.note_format(name);
            }
        }
        markers
    }

    fn buffer_put(&self, text: &str) {
        *lock(&self.buffer) = Some((Instant::now(), text.to_string()));
    }

    fn buffer_get(&self) -> Option<String> {
        let mut buffer = lock(&self.buffer);
        let keep = Duration::from_secs(CLIPBOARD_BUFFER_MINUTES as u64 * 60);
        if buffer.as_ref().is_some_and(|(at, _)| at.elapsed() > keep) {
            *buffer = None;
        }
        buffer.as_ref().map(|(_, text)| text.clone())
    }

    /// Put `text` on this machine's clipboard: the system's, else the buffer.
    pub async fn set(&self, text: &str) -> Result<ClipboardStore, RpcError> {
        if text.len() > MAX_CLIPBOARD_BYTES {
            return Err(RpcError::invalid_params("The clipboard text is limited to 1 MiB."));
        }
        match self.system_set(text).await {
            Ok(true) => {
                self.last_applied.store(digest(text), Ordering::Relaxed);
                Ok(ClipboardStore::System)
            }
            Ok(false) => {
                self.buffer_put(text);
                Ok(ClipboardStore::Buffer)
            }
            // The system clipboard is there but refused (no desktop session): keep the text in the buffer instead.
            Err(error) => {
                tracing::debug!(target: TARGET, %error, "the system clipboard could not be written; using the Kybern buffer");
                self.buffer_put(text);
                Ok(ClipboardStore::Buffer)
            }
        }
    }

    /// Read this machine's clipboard: the system's text, else what the buffer holds.
    pub async fn get(&self) -> Result<ClipboardGetResult, RpcError> {
        match self.system_get().await {
            Ok(Some(text)) if !text.is_empty() => {
                if text.len() > MAX_CLIPBOARD_BYTES {
                    return Err(RpcError::invalid_params("The clipboard holds more than 1 MiB of text."));
                }
                return Ok(ClipboardGetResult { text, source: ClipboardStore::System });
            }
            Ok(_) => {}
            Err(error) => tracing::debug!(target: TARGET, %error, "the system clipboard could not be read"),
        }
        match self.buffer_get() {
            Some(text) => Ok(ClipboardGetResult { text, source: ClipboardStore::Buffer }),
            None => Err(RpcError::new(codes::NOT_FOUND, "The clipboard has no text.")),
        }
    }

    /// Where an explicit send lands here.
    pub fn store(&self) -> ClipboardStore {
        if self.backend() == Backend::BufferOnly { ClipboardStore::Buffer } else { ClipboardStore::System }
    }
}

// ---- the client methods ----

fn failure(name: &str, failure: CallFailure) -> RpcError {
    match failure {
        CallFailure::Offline(reason) => {
            RpcError::new(codes::NOT_AVAILABLE, reason.clone()).with_problem(&crate::problem::machine_offline(name, reason))
        }
        CallFailure::Gone(reason) => RpcError::new(codes::NOT_FOUND, reason),
        CallFailure::Remote(error) if error.code == codes::FORBIDDEN => RpcError::new(
            codes::FORBIDDEN,
            format!("{name} does not let this machine use its clipboard. Turn Clipboard on for it in Settings → Machines on {name}."),
        ),
        CallFailure::Remote(error) if error.code == codes::METHOD_NOT_FOUND => {
            RpcError::new(codes::NOT_AVAILABLE, format!("{name} needs a newer Kybern to share its clipboard."))
        }
        CallFailure::Remote(error) => RpcError::new(error.code, format!("{name}: {}", error.message)),
    }
}

/// `Some((id, name))` for another machine of the network; `None` for this one.
fn other_machine(state: &AppState, machine: Option<&str>) -> Result<Option<(String, String)>, RpcError> {
    let Some(query) = machine.map(str::trim).filter(|query| !query.is_empty()) else { return Ok(None) };
    let hub = state.orchestrator.remote_hub().ok_or_else(|| RpcError::new(codes::NOT_AVAILABLE, "the machine network is not available"))?;
    if query.eq_ignore_ascii_case(&hub.self_id()) || query.eq_ignore_ascii_case(&hub.self_name()) {
        return Ok(None);
    }
    let target = hub.resolve(query).map_err(|message| RpcError::new(codes::NOT_FOUND, message))?;
    Ok(Some((target.id, target.name)))
}

pub async fn handle_set(state: &AppState, params: ClipboardSetParams) -> Result<ClipboardSetResult, RpcError> {
    let bytes = params.text.len() as u64;
    match other_machine(state, params.machine.as_deref())? {
        None => Ok(ClipboardSetResult { stored: state.network.clipboard.set(&params.text).await?, bytes }),
        Some((id, name)) => {
            let hub = state.orchestrator.remote_hub().expect("checked by other_machine");
            let sent = PeerClipboardSetParams { text: params.text };
            let value = hub.call(&id, PeerClipboardSet::NAME, serde_json::to_value(sent).map_err(RpcError::internal)?).await;
            let value = value.map_err(|error| failure(&name, error))?;
            serde_json::from_value(value).map_err(RpcError::internal)
        }
    }
}

pub async fn handle_get(state: &AppState, params: ClipboardGetParams) -> Result<ClipboardGetResult, RpcError> {
    match other_machine(state, params.machine.as_deref())? {
        None => state.network.clipboard.get().await,
        Some((id, name)) => {
            let hub = state.orchestrator.remote_hub().expect("checked by other_machine");
            let value = hub.call(&id, PeerClipboardGet::NAME, json!({})).await.map_err(|error| failure(&name, error))?;
            serde_json::from_value(value).map_err(RpcError::internal)
        }
    }
}

pub fn status(state: &AppState) -> ClipboardStatus {
    ClipboardStatus { backend: state.network.clipboard.store(), auto: auto_enabled(state), skipped: state.network.clipboard.skips.total() }
}

// ---- the peer methods ----

pub async fn peer_set(state: &AppState, params: PeerClipboardSetParams) -> Result<ClipboardSetResult, RpcError> {
    let bytes = params.text.len() as u64;
    Ok(ClipboardSetResult { stored: state.network.clipboard.set(&params.text).await?, bytes })
}

pub async fn peer_get(state: &AppState) -> Result<ClipboardGetResult, RpcError> {
    state.network.clipboard.get().await
}

fn seal_context(from: &str, to: &str) -> Vec<u8> {
    format!("{SEAL_CONTEXT}\0{from}\0{to}").into_bytes()
}

/// `peer.clipboard.auto`: a change another member shared. It is applied without being shared again.
pub async fn peer_auto(state: &AppState, link: &PeerLink, params: PeerClipboardAutoParams) -> Result<(), RpcError> {
    let membership = state.network.membership().ok_or_else(|| RpcError::new(codes::NOT_AVAILABLE, "no network"))?;
    let sealed = B64.decode(params.sealed.as_bytes()).map_err(|_| RpcError::invalid_params("sealed is not base64"))?;
    let plain = crate::secret_store::open(&membership.keys(), &seal_context(&link.machine_id, &membership.machine_id()), &sealed)
        .map_err(|_| RpcError::new(codes::FORBIDDEN, "that clipboard change was not sealed for this machine"))?;
    let text = String::from_utf8(plain).map_err(|_| RpcError::invalid_params("the clipboard change is not text"))?;
    if text.len() > MAX_CLIPBOARD_BYTES {
        return Err(RpcError::invalid_params("The clipboard text is limited to 1 MiB."));
    }
    let clipboard = &state.network.clipboard;
    clipboard.last_applied.store(digest(&text), Ordering::Relaxed);
    clipboard.last_seen.store(digest(&text), Ordering::Relaxed);
    clipboard.set(&text).await?;
    Ok(())
}

// ---- the universal clipboard ----

/// This machine's owner turned on "Share clipboard automatically".
fn auto_enabled(state: &AppState) -> bool {
    let Some(membership) = state.network.membership() else { return false };
    membership.projection().allowed(&membership.machine_id()).get("clipboard_auto").copied().unwrap_or(false)
}

/// The members to push to: online, not paused, and opted in themselves.
fn auto_peers(state: &AppState) -> Vec<(String, String)> {
    let Some(membership) = state.network.membership() else { return Vec::new() };
    let me = membership.machine_id();
    let projection = membership.projection();
    projection
        .members()
        .filter(|(id, entry)| **id != me && entry.removed.is_none() && !entry.paused)
        .filter(|(id, _)| projection.allowed(id).get("clipboard_auto").copied().unwrap_or(false))
        .filter(|(id, _)| state.network.peers.status(id).online)
        .map(|(id, entry)| (id.clone(), entry.descriptor.sealing_key.clone()))
        .collect()
}

/// Watch this machine's clipboard while it takes part, and share what passes the filter.
pub async fn run(state: AppState) {
    loop {
        tokio::select! {
            _ = state.shutdown.cancelled() => return,
            _ = tokio::time::sleep(POLL_EVERY) => {}
        }
        tick(&state).await;
    }
}

/// One look at the clipboard. Public inside the crate so a test can drive it.
pub(crate) async fn tick(state: &AppState) {
    if !state.network.is_enabled() || !auto_enabled(state) {
        return;
    }
    let peers = auto_peers(state);
    // Nobody to share with: the clipboard is not even read.
    if peers.is_empty() {
        return;
    }
    let clipboard = &state.network.clipboard;
    let Ok(Some((text, markers))) = clipboard.system_snapshot().await else { return };
    if text.is_empty() || text.len() > MAX_CLIPBOARD_BYTES {
        return;
    }
    let hash = digest(&text);
    if clipboard.last_seen.swap(hash, Ordering::Relaxed) == hash || clipboard.last_applied.load(Ordering::Relaxed) == hash {
        return;
    }
    let verdict = classify(&text, markers);
    if let Verdict::Secret(reason) = verdict {
        clipboard.skips.record(verdict);
        tracing::debug!(target: TARGET, reason = reason.as_str(), "not sharing a clipboard change");
        return;
    }
    let Some(membership) = state.network.membership() else { return };
    let me = membership.machine_id();
    for (id, sealing_key) in peers {
        let Some(link) = state.network.peers.link(&id) else { continue };
        let Some(public) = B64.decode(sealing_key.as_bytes()).ok().and_then(|raw| <[u8; 32]>::try_from(raw).ok()) else { continue };
        let Ok(sealed) = crate::secret_store::seal(&public, &seal_context(&me, &id), text.as_bytes()) else { continue };
        let params = PeerClipboardAutoParams { sealed: B64.encode(sealed) };
        if let Err(error) = link.call::<PeerClipboardAuto>(params).await {
            tracing::debug!(target: TARGET, machine = %id, code = error.code, "a member did not take a clipboard change");
        }
    }
}

#[allow(dead_code)]
fn _value(_: Value) {}

/// Windows' clipboard formats that carry a DWORD: `0` keeps the content out of clipboard history and the cloud
/// clipboard, and so out of sharing.
const WINDOWS_FLAG_FORMATS: [&str; 2] = ["CanIncludeInClipboardHistory", "CanUploadToCloudClipboard"];

/// Markers from the registered formats a Windows source offered, with the value of each flag format when it could be
/// read. A flag that is present but unreadable counts as set to `0`.
#[cfg_attr(not(windows), allow(dead_code))]
fn windows_markers(formats: &[(String, Option<u32>)]) -> Markers {
    let mut markers = Markers::default();
    for (name, value) in formats {
        markers.note_format(name);
        if WINDOWS_FLAG_FORMATS.contains(&name.as_str()) {
            markers.note_history_allowed(value.unwrap_or(0));
        }
    }
    markers
}

/// The Windows clipboard read natively, so the text and the password-manager markers are one snapshot.
#[cfg(windows)]
mod windows_clipboard {
    use super::{Markers, WINDOWS_FLAG_FORMATS, windows_markers};
    use windows_sys::Win32::Foundation::{GetLastError, HANDLE, SetLastError};
    use windows_sys::Win32::System::DataExchange::{
        CloseClipboard, EnumClipboardFormats, GetClipboardData, GetClipboardFormatNameW, OpenClipboard,
    };
    use windows_sys::Win32::System::Memory::{GlobalLock, GlobalSize, GlobalUnlock};

    const CF_UNICODETEXT: u32 = 13;
    /// Registered formats are numbered from here; the ones below are the standard formats.
    const FIRST_REGISTERED: u32 = 0xC000;

    /// Closes the clipboard when dropped.
    struct Opened;

    impl Drop for Opened {
        fn drop(&mut self) {
            unsafe { CloseClipboard() };
        }
    }

    /// The bytes of a clipboard handle, while the clipboard is open.
    fn bytes_of(handle: HANDLE) -> Option<Vec<u8>> {
        if handle.is_null() {
            return None;
        }
        unsafe {
            let size = GlobalSize(handle);
            let data = GlobalLock(handle) as *const u8;
            if data.is_null() {
                return None;
            }
            let bytes = std::slice::from_raw_parts(data, size).to_vec();
            GlobalUnlock(handle);
            Some(bytes)
        }
    }

    /// The text and the markers on the clipboard now; `Ok(None)` when it holds no text.
    pub fn snapshot() -> std::io::Result<Option<(String, Markers)>> {
        // Another program may hold the clipboard for a moment.
        let mut attempts = 0;
        while unsafe { OpenClipboard(std::ptr::null_mut()) } == 0 {
            attempts += 1;
            if attempts == 10 {
                return Err(std::io::Error::other("the clipboard is held by another program"));
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        let _opened = Opened;
        let mut formats: Vec<(String, Option<u32>)> = Vec::new();
        let mut has_text = false;
        let mut format = 0;
        loop {
            // The end of the list and a failure both return 0; only a failure sets the last error.
            unsafe { SetLastError(0) };
            format = unsafe { EnumClipboardFormats(format) };
            if format == 0 {
                if unsafe { GetLastError() } != 0 {
                    return Err(std::io::Error::other("the clipboard's formats could not be listed"));
                }
                break;
            }
            if format == CF_UNICODETEXT {
                has_text = true;
            }
            if format < FIRST_REGISTERED {
                continue;
            }
            let mut name = [0u16; 256];
            let len = unsafe { GetClipboardFormatNameW(format, name.as_mut_ptr(), name.len() as i32) };
            if len <= 0 {
                continue;
            }
            let name = String::from_utf16_lossy(&name[..len as usize]);
            let value = if WINDOWS_FLAG_FORMATS.contains(&name.as_str()) {
                bytes_of(unsafe { GetClipboardData(format) }).and_then(|bytes| Some(u32::from_le_bytes(bytes.get(..4)?.try_into().ok()?)))
            } else {
                None
            };
            formats.push((name, value));
        }
        if !has_text {
            return Ok(None);
        }
        let bytes = bytes_of(unsafe { GetClipboardData(CF_UNICODETEXT) })
            .ok_or_else(|| std::io::Error::other("the clipboard's text could not be read"))?;
        let (pairs, _) = bytes.as_chunks::<2>();
        let wide: Vec<u16> = pairs.iter().map(|pair| u16::from_le_bytes(*pair)).take_while(|unit| *unit != 0).collect();
        Ok(Some((String::from_utf16_lossy(&wide), windows_markers(&formats))))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn windows_password_manager_formats_keep_a_copy_from_being_shared() {
        let short_password = "Tr0ub4dor";
        assert_eq!(classify(short_password, windows_markers(&[])), Verdict::Shareable, "nothing in the text gives it away");
        for formats in [
            vec![("ExcludeClipboardContentFromMonitorProcessing".to_string(), None)],
            vec![("CanIncludeInClipboardHistory".to_string(), Some(0))],
            vec![("CanUploadToCloudClipboard".to_string(), Some(0))],
            // Present but unreadable: kept out too.
            vec![("CanIncludeInClipboardHistory".to_string(), None)],
        ] {
            assert!(matches!(classify(short_password, windows_markers(&formats)), Verdict::Secret(_)), "{formats:?}");
        }
        let allowed = [("CanIncludeInClipboardHistory".to_string(), Some(1)), ("HTML Format".to_string(), None)];
        assert_eq!(classify(short_password, windows_markers(&allowed)), Verdict::Shareable);
    }

    #[tokio::test]
    async fn a_machine_with_no_desktop_keeps_the_text_in_a_buffer_for_ten_minutes() {
        let clipboard = Clipboard::default();
        clipboard.use_buffer_only();
        assert!(clipboard.get().await.is_err());
        assert_eq!(clipboard.set("hello").await.unwrap(), ClipboardStore::Buffer);
        let got = clipboard.get().await.unwrap();
        assert_eq!((got.text.as_str(), got.source), ("hello", ClipboardStore::Buffer));
        // Ten minutes later it is gone.
        *lock(&clipboard.buffer) = Some((Instant::now() - Duration::from_secs(11 * 60), "old".into()));
        assert!(clipboard.get().await.is_err());
        // The cap is a megabyte.
        assert!(clipboard.set(&"x".repeat(MAX_CLIPBOARD_BYTES + 1)).await.is_err());
        assert!(clipboard.set(&"x".repeat(MAX_CLIPBOARD_BYTES)).await.is_ok());
    }

    #[tokio::test]
    async fn the_system_clipboard_is_used_when_there_is_one_and_the_buffer_backs_it_up() {
        let clipboard = Clipboard::default();
        clipboard.use_memory();
        assert_eq!(clipboard.set("from elsewhere").await.unwrap(), ClipboardStore::System);
        let got = clipboard.get().await.unwrap();
        assert_eq!((got.text.as_str(), got.source), ("from elsewhere", ClipboardStore::System));
        // What a send put there is not shared on again by the watcher.
        assert_eq!(clipboard.last_applied.load(Ordering::Relaxed), digest("from elsewhere"));
    }
}
