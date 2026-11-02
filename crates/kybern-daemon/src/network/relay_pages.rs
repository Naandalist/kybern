//! Relay page tickets (ADE-99 §1.4): a phone's web view cannot send `X-Kybern-Relay-Auth`, so a page of machine B that
//! is reachable only through relay member M (a preview, a visual frame, an artifact preview) cannot load through
//! `/relay/{machine_id}/http/…`. M mints a short-lived, page-scoped ticket instead and accepts it in the URL:
//! `ANY /relay/{machine_id}/page/{ticket}/{*path}` ([`super::relay::serve_page`]).
//!
//! A ticket binds one device token, one target machine and one path: a **subtree** (`/preview-proxy/<id>/`,
//! `/preview-files/<id>/`) or a **document** (`/visual-frame/<id>`, `/artifact-preview/<id>`). It follows the rules of
//! the preview tickets (UUIDv4, sliding 30 minutes, hard cap 12 hours, at most 64, oldest evicted first) in its own store.
//!
//! A page's streams belong to its ticket: they count under `page:<ticket>` with their own small cap
//! ([`MAX_STREAMS_PER_PAGE`]), so a page cannot use up its device's relay streams, and they end when the ticket is
//! closed, revoked, evicted or expires ([`PageGrant::cancel`], [`PageGrant::deadline`]).

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use kybern_protocol::*;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use super::relay::route_check;
use crate::auth::{Principal, PrincipalKind};
use crate::previews::tickets::{HARD_CAP, MAX_TICKETS, SLIDING_EXPIRY};
use crate::state::AppState;

/// Streams one page ticket may hold open at once (requests and hot-reload sockets together).
pub const MAX_STREAMS_PER_PAGE: usize = 32;

/// The header B reads a page's outside prefix from (`/relay/<machine>/page/<ticket>`), so a sandboxed page's
/// policy points at the address the web view loaded. B trusts it only beside [`LOCAL_MARK_HEADER`].
pub const PREFIX_HEADER: &str = "x-kybern-page-prefix";
/// Set by B on the local request it makes for a relayed stream, with a value only this process knows. A request a
/// client sends to B directly cannot carry it, so a forged [`PREFIX_HEADER`] is ignored there.
pub const LOCAL_MARK_HEADER: &str = "x-kybern-relay-local";

/// A live ticket as one request sees it.
#[derive(Debug, Clone)]
pub struct PageGrant {
    pub info: PageInfo,
    /// Fires when the ticket is closed, revoked, evicted or found expired: its open streams stop.
    pub cancel: CancellationToken,
    /// The hard cap: no stream of the ticket outlives it.
    pub deadline: Instant,
}

/// What a ticket is bound to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PageInfo {
    /// The device token that minted it. A revoked token stops the page at once.
    pub token_id: Uuid,
    /// The machine the page is on.
    pub target: String,
    /// A subtree: the prefix, ending in `/`. A document: its exact path.
    pub bound: String,
    pub document: bool,
}

impl PageInfo {
    /// Whether a normalized request path (no query) is the page or inside it.
    pub fn admits(&self, path: &str) -> bool {
        if self.document { path == self.bound } else { path.starts_with(&self.bound) || format!("{path}/") == self.bound }
    }
}

struct Entry {
    info: PageInfo,
    created: Instant,
    last_used: Instant,
    cancel: CancellationToken,
}

/// The relay page tickets of this machine.
pub struct RelayPages {
    entries: Mutex<HashMap<String, Entry>>,
    sliding: Duration,
    hard_cap: Duration,
    max: usize,
    local_mark: String,
}

impl Default for RelayPages {
    fn default() -> Self {
        Self::with_limits(SLIDING_EXPIRY, HARD_CAP, MAX_TICKETS)
    }
}

impl RelayPages {
    pub fn with_limits(sliding: Duration, hard_cap: Duration, max: usize) -> Self {
        Self { entries: Mutex::default(), sliding, hard_cap, max, local_mark: Uuid::new_v4().simple().to_string() }
    }

    /// The value of [`LOCAL_MARK_HEADER`] on this process's own relayed requests.
    pub fn local_mark(&self) -> &str {
        &self.local_mark
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<String, Entry>> {
        self.entries.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn live(&self, entry: &Entry, now: Instant) -> bool {
        now.saturating_duration_since(entry.last_used) < self.sliding && now.saturating_duration_since(entry.created) < self.hard_cap
    }

    pub fn mint(&self, info: PageInfo) -> String {
        self.mint_at(info, Instant::now())
    }

    pub fn mint_at(&self, info: PageInfo, now: Instant) -> String {
        let mut entries = self.lock();
        entries.retain(|_, entry| {
            let live = self.live(entry, now);
            if !live {
                entry.cancel.cancel();
            }
            live
        });
        while entries.len() >= self.max {
            let Some(oldest) = entries.iter().min_by_key(|(_, entry)| entry.created).map(|(id, _)| id.clone()) else { break };
            if let Some(entry) = entries.remove(&oldest) {
                entry.cancel.cancel();
            }
        }
        let ticket = Uuid::new_v4().to_string();
        entries.insert(ticket.clone(), Entry { info, created: now, last_used: now, cancel: CancellationToken::new() });
        ticket
    }

    /// Look a ticket up and slide its expiry. An expired ticket is removed.
    pub fn lookup(&self, ticket: &str) -> Option<PageGrant> {
        self.lookup_at(ticket, Instant::now())
    }

    pub fn lookup_at(&self, ticket: &str, now: Instant) -> Option<PageGrant> {
        let mut entries = self.lock();
        let live = entries.get(ticket).map(|entry| self.live(entry, now))?;
        if !live {
            if let Some(entry) = entries.remove(ticket) {
                entry.cancel.cancel();
            }
            return None;
        }
        let entry = entries.get_mut(ticket)?;
        entry.last_used = now;
        Some(PageGrant { info: entry.info.clone(), cancel: entry.cancel.clone(), deadline: entry.created + self.hard_cap })
    }

    /// Close a ticket for the token that minted it. Idempotent; a ticket of another token behaves as if it did not exist.
    pub fn close(&self, ticket: &str, token_id: Uuid) -> bool {
        let mut entries = self.lock();
        if entries.get(ticket).is_some_and(|entry| entry.info.token_id == token_id) {
            if let Some(entry) = entries.remove(ticket) {
                entry.cancel.cancel();
            }
            return true;
        }
        false
    }

    /// Drop every ticket a device token minted (device revocation). Returns the count.
    pub fn revoke_principal(&self, token_id: Uuid) -> usize {
        let mut entries = self.lock();
        let before = entries.len();
        entries.retain(|_, entry| {
            let keep = entry.info.token_id != token_id;
            if !keep {
                entry.cancel.cancel();
            }
            keep
        });
        before - entries.len()
    }

    #[cfg(test)]
    pub fn len(&self) -> usize {
        self.lock().len()
    }
}

fn valid_id(id: &str) -> bool {
    (1..=128).contains(&id.len()) && id.bytes().all(|byte| byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-')
}

/// A path segment after percent-decoding must be neither a dot segment nor smuggle a separator.
fn plain_segment(segment: &str) -> bool {
    let decoded = super::relay::percent_decoded(segment);
    decoded != "." && decoded != ".." && !decoded.contains(['/', '\\']) && !decoded.chars().any(char::is_control)
}

/// What a page path binds: `(bound, document)`, or `None` for a path no page ticket may cover.
pub fn parse_page_path(path: &str) -> Option<(String, bool)> {
    if path.chars().any(|c| c == '\\' || c == '#' || c == ' ' || c.is_control()) {
        return None;
    }
    let (route, _query) = path.split_once('?').unwrap_or((path, ""));
    let rest = route.strip_prefix('/')?;
    let segments: Vec<&str> = rest.split('/').collect();
    let (root, id) = (*segments.first()?, *segments.get(1)?);
    if !valid_id(id) {
        return None;
    }
    match root {
        "preview-proxy" | "preview-files" => {
            segments[2..].iter().all(|segment| plain_segment(segment)).then(|| (format!("/{root}/{id}/"), false))
        }
        "visual-frame" | "artifact-preview" => (segments.len() == 2).then(|| (format!("/{root}/{id}"), true)),
        _ => None,
    }
}

fn mint_error(status: axum::http::StatusCode, message: &str) -> RpcError {
    use axum::http::StatusCode;
    match status {
        StatusCode::NOT_FOUND => RpcError::new(codes::NOT_FOUND, message),
        StatusCode::BAD_REQUEST => RpcError::invalid_params(message),
        _ => RpcError::new(codes::NOT_AVAILABLE, message),
    }
}

/// `network.relay_page.open`.
pub fn open(state: &AppState, principal: &Principal, params: NetworkRelayPageOpenParams) -> Result<NetworkRelayPageOpenResult, RpcError> {
    if !matches!(principal.kind, PrincipalKind::Device) {
        return Err(RpcError::new(codes::FORBIDDEN, "only a device can open a relay page"));
    }
    let Some((bound, document)) = parse_page_path(&params.path) else {
        return Err(RpcError::invalid_params(
            "path is /preview-proxy/<id>/…, /preview-files/<id>/…, /visual-frame/<id> or /artifact-preview/<id>",
        ));
    };
    route_check(state, &params.machine_id).map_err(|(status, message)| mint_error(status, message))?;
    let ticket = state.network.pages.mint(PageInfo {
        token_id: principal.token_id,
        target: params.machine_id.clone(),
        bound: bound.clone(),
        document,
    });
    let root = format!("/relay/{}/page/{ticket}", params.machine_id);
    Ok(NetworkRelayPageOpenResult { path: format!("{root}{}", params.path), scope: format!("{root}{bound}"), ticket })
}

/// `network.relay_page.close`.
pub fn close(state: &AppState, principal: &Principal, ticket: &str) {
    state.network.pages.close(ticket, principal.token_id);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn info(token_id: Uuid, bound: &str, document: bool) -> PageInfo {
        PageInfo { token_id, target: "b".into(), bound: bound.into(), document }
    }

    #[test]
    fn only_ticket_pages_can_be_bound_and_hostile_paths_are_refused() {
        assert_eq!(parse_page_path("/preview-proxy/abc/"), Some(("/preview-proxy/abc/".into(), false)));
        assert_eq!(parse_page_path("/preview-proxy/abc"), Some(("/preview-proxy/abc/".into(), false)));
        assert_eq!(parse_page_path("/preview-files/a-b_C/dir/x.html?v=1"), Some(("/preview-files/a-b_C/".into(), false)));
        assert_eq!(parse_page_path("/visual-frame/abc?theme=dark"), Some(("/visual-frame/abc".into(), true)));
        assert_eq!(parse_page_path("/artifact-preview/abc"), Some(("/artifact-preview/abc".into(), true)));
        let too_long = format!("/preview-proxy/{}/", "a".repeat(129));
        let longest = format!("/preview-proxy/{}/", "a".repeat(128));
        assert!(parse_page_path(&too_long).is_none());
        assert!(parse_page_path(&longest).is_some());
        for bad in [
            "",
            "/",
            "preview-proxy/abc/",
            "/preview-proxy/",
            "/preview-proxy//x",
            "/preview-proxy/a.b/",
            "/preview-proxy/ab%2fc/",
            "/preview-proxy/abc/../other/",
            "/preview-proxy/abc/%2e%2e/other/",
            "/preview-proxy/abc/.%2E/other/",
            "/preview-proxy/abc/a%2f..%2fb",
            "/preview-proxy/abc/a\\b",
            "/preview-proxy/abc/#frag",
            "/preview-proxy/abc/ x",
            "/visual-frame/abc/extra",
            "/visual-frame/abc/",
            "/artifact-preview/a%2eb",
            "/ws",
            "/network/session",
            "/relay/x/ws",
            "/api/threads",
            "/Preview-Proxy/abc/",
        ] {
            assert!(parse_page_path(bad).is_none(), "{bad:?} is refused");
        }
    }

    #[test]
    fn a_subtree_admits_its_own_paths_on_a_segment_boundary_and_a_document_only_itself() {
        let tree = info(Uuid::nil(), "/preview-proxy/X/", false);
        assert!(tree.admits("/preview-proxy/X/"));
        assert!(tree.admits("/preview-proxy/X/assets/app.js"));
        assert!(tree.admits("/preview-proxy/X"));
        assert!(!tree.admits("/preview-proxy/XY/"));
        assert!(!tree.admits("/preview-proxy/XY"));
        assert!(!tree.admits("/preview-files/X/"));
        assert!(!tree.admits("/ws"));
        let document = info(Uuid::nil(), "/visual-frame/X", true);
        assert!(document.admits("/visual-frame/X"));
        assert!(!document.admits("/visual-frame/X/sub"));
        assert!(!document.admits("/visual-frame/XY"));
        assert!(!document.admits("/visual-frame"));
    }

    #[test]
    fn tickets_slide_expire_and_evict_like_preview_tickets() {
        let pages = RelayPages::with_limits(Duration::from_secs(60), Duration::from_secs(300), 2);
        let token = Uuid::now_v7();
        let start = Instant::now();
        let a = pages.mint_at(info(token, "/preview-proxy/a/", false), start);
        assert!(pages.lookup_at(&a, start + Duration::from_secs(50)).is_some(), "live");
        assert!(pages.lookup_at(&a, start + Duration::from_secs(100)).is_some(), "a use slides the expiry");
        assert!(pages.lookup_at(&a, start + Duration::from_secs(200)).is_none(), "idle past the window");
        assert!(pages.lookup_at(&a, start + Duration::from_secs(201)).is_none(), "and removed");
        let b = pages.mint_at(info(token, "/preview-proxy/b/", false), start);
        for step in 1..=5 {
            let _ = pages.lookup_at(&b, start + Duration::from_secs(step * 55));
        }
        assert!(pages.lookup_at(&b, start + Duration::from_secs(330)).is_none(), "the hard cap holds against sliding");
        // At most two: the oldest goes.
        let now = Instant::now();
        let first = pages.mint_at(info(token, "/preview-proxy/1/", false), now);
        let second = pages.mint_at(info(token, "/preview-proxy/2/", false), now + Duration::from_millis(1));
        let third = pages.mint_at(info(token, "/preview-proxy/3/", false), now + Duration::from_millis(2));
        assert_eq!(pages.len(), 2);
        assert!(pages.lookup_at(&first, now + Duration::from_millis(3)).is_none());
        assert!(pages.lookup_at(&second, now + Duration::from_millis(3)).is_some());
        assert!(pages.lookup_at(&third, now + Duration::from_millis(3)).is_some());
    }

    #[test]
    fn a_tickets_streams_are_told_to_stop_when_it_is_closed_revoked_evicted_or_expired() {
        let pages = RelayPages::with_limits(Duration::from_secs(100), Duration::from_secs(300), 2);
        let (owner, start) = (Uuid::new_v4(), Instant::now());
        let closed = pages.mint_at(info(owner, "/preview-proxy/a/", false), start);
        let grant = pages.lookup_at(&closed, start).unwrap();
        assert_eq!(grant.deadline, start + Duration::from_secs(300), "the hard cap bounds every stream");
        assert!(!grant.cancel.is_cancelled());
        assert!(pages.close(&closed, owner));
        assert!(grant.cancel.is_cancelled(), "closed");

        let revoked = pages.mint_at(info(owner, "/preview-proxy/b/", false), start);
        let grant = pages.lookup_at(&revoked, start).unwrap();
        pages.revoke_principal(owner);
        assert!(grant.cancel.is_cancelled(), "revoked");

        let expired = pages.mint_at(info(owner, "/preview-proxy/c/", false), start);
        let grant = pages.lookup_at(&expired, start).unwrap();
        assert!(pages.lookup_at(&expired, start + Duration::from_secs(150)).is_none());
        assert!(grant.cancel.is_cancelled(), "expired");

        let first = pages.mint_at(info(owner, "/preview-proxy/d/", false), start);
        let grant = pages.lookup_at(&first, start).unwrap();
        pages.mint_at(info(owner, "/preview-proxy/e/", false), start + Duration::from_millis(1));
        pages.mint_at(info(owner, "/preview-proxy/f/", false), start + Duration::from_millis(2));
        assert!(grant.cancel.is_cancelled(), "evicted");
    }

    #[test]
    fn closing_is_idempotent_and_only_for_the_minting_token_and_revocation_clears_a_devices_pages() {
        let pages = RelayPages::default();
        let (owner, other) = (Uuid::now_v7(), Uuid::now_v7());
        let mine = pages.mint(info(owner, "/preview-proxy/a/", false));
        let also = pages.mint(info(owner, "/visual-frame/a", true));
        let theirs = pages.mint(info(other, "/preview-proxy/b/", false));
        assert!(!pages.close(&mine, other), "another token cannot close it");
        assert!(pages.lookup(&mine).is_some(), "and it is still there");
        assert!(pages.close(&mine, owner));
        assert!(!pages.close(&mine, owner), "closing twice is fine");
        assert!(!pages.close("nope", owner));
        assert_eq!(pages.revoke_principal(owner), 1);
        assert!(pages.lookup(&also).is_none());
        assert!(pages.lookup(&theirs).is_some(), "other devices keep theirs");
    }
}
