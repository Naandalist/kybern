//! The viewing side's cache of a forward's dev server responses (ADE-126).
//!
//! A dev server sends most of a page with `Cache-Control: no-cache` and a validator, so even a browser that keeps them
//! asks again for each one on a reload: one round trip to the other machine per request, six at a time. Kybern's web
//! views often start with no cache at all. This cache keeps the responses the forward already carried, keyed by host,
//! path and the request headers they `Vary` on, with their validators.
//!
//! * A page load (a navigation request) revalidates every kept response of its host with **one** request over the
//!   forward's HTTP/2 connection ([`REVALIDATE_HEADER`]): the target checks each validator against its dev server over
//!   loopback, in parallel, and streams back fresh, changed (with the new body, compressed) or gone, quick answers first.
//!   A request of the page waits only for its own record, not the whole answer.
//! * A kept response is answered here without asking the target only when
//!   - this page load's batch confirmed it, within [`BATCH_FRESH`], and the request is a subresource the page loads
//!     (`Sec-Fetch-Dest` of a script, style, image, font or worker; never a `fetch` or XHR, whose destination is empty);
//!   - or the response's own `max-age` still covers it and neither side asked for a checked answer. `no-cache`,
//!     `max-age=0` and `must-revalidate` responses are revalidated every time outside a batch.
//!
//!   Anything else is revalidated on its own: a `304` costs a round trip but no body.
//! * A hot-reload message on the forward's WebSockets makes everything stale at once, and any request other than `GET`,
//!   `HEAD`, `OPTIONS` or `TRACE` makes everything of its host stale (RFC 9111 §4.4 asks for its URL; a dev server's API
//!   changes more than one).
//! * Only whole `200` answers to plain `GET`s with an `ETag` or `Last-Modified` are kept: never with `Set-Cookie`,
//!   `Cache-Control: private` or `no-store`, `Vary: *`, a range, an `Authorization` header or an event stream. The
//!   request's `Cookie` is always a variant, an absent one included, so a page kept for one session (or for none) is never
//!   answered to another. One response is at most [`MAX_ENTRY`] as it travelled, a forward keeps at most [`MAX_FORWARD`]
//!   and all forwards [`MAX_TOTAL`], least recently used first out. It lives in memory and ends with the forward.
//! * Only a target that lists `ports.cache.v1` is cached for: it keeps the dev server's own `ETag` beside the weak one a
//!   compressed body gets ([`ORIGINAL_ETAG_HEADER`]), so revalidation sends exactly what the dev server issued.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use futures::StreamExt;
use http_body_util::{BodyExt, Full, Limited};
use hyper::body::{Bytes, Incoming};
use hyper::header::{HeaderMap, HeaderName, HeaderValue};
use hyper::{Request, Response, StatusCode};
use serde::{Deserialize, Serialize};

use super::direct::{self, ProxyBody, status_response, strip_hop_headers};

const TARGET: &str = "kybern::network::ports";

/// Viewing side → target: the viewing machine keeps a cache of this forward's responses.
pub const CACHE_HEADER: &str = "x-kybern-cache";
/// Target → viewing side, beside a weakened `ETag`: the dev server's own tag.
pub const ORIGINAL_ETAG_HEADER: &str = "x-kybern-etag";
/// Viewing side → target: this request is a batched revalidation, not a request for the dev server.
pub const REVALIDATE_HEADER: &str = "x-kybern-revalidate";

/// A response this page load's batch confirmed is answered here, for a subresource of the page, for this long.
pub const BATCH_FRESH: Duration = Duration::from_secs(120);
pub const MAX_ENTRY: usize = 32 * 1024 * 1024;
pub const MAX_FORWARD: usize = 96 * 1024 * 1024;
pub const MAX_TOTAL: usize = 256 * 1024 * 1024;
/// Responses one batch revalidates at most.
const MAX_BATCH: usize = 2000;
/// Changed bodies one batch answer carries at most; the rest are reported gone and fetched as usual. The viewing machine
/// asks for what its link carries in [`BUDGET_TIME`], between [`MIN_BATCH_BODIES`] and this.
const MAX_BATCH_BODIES: usize = 4 * 1024 * 1024;
const MIN_BATCH_BODIES: usize = 256 * 1024;
const BUDGET_TIME: Duration = Duration::from_secs(2);
/// One changed body a batch carries at most, before compression.
const MAX_BATCH_BODY: usize = 512 * 1024;
/// How long the target waits for the dev server on one entry of a batch.
const CHECK_TIMEOUT: Duration = Duration::from_secs(5);
/// Checks a target runs against its dev server at once.
const BATCH_CONCURRENCY: usize = 16;
/// How long a page request waits for a batch before it asks on its own.
const BATCH_WAIT: Duration = Duration::from_secs(30);
/// Two navigations this close together share one batch.
const BATCH_SPACING: Duration = Duration::from_secs(1);
/// The request headers a revalidation repeats (besides those the response varies on).
const REPLAYED: [&str; 7] = ["host", "accept", "accept-encoding", "accept-language", "user-agent", "cookie", "x-forwarded-proto"];

/// What every forward's cache holds together.
static TOTAL: AtomicUsize = AtomicUsize::new(0);

pub struct Entry {
    key: String,
    /// The `Host` the request named: what a write to that host makes stale.
    host: String,
    /// The request headers the response varies on, as this request had them (lowercase names).
    vary: Vec<(String, Option<String>)>,
    /// The request headers a revalidation sends again.
    replay: Vec<(String, String)>,
    status: StatusCode,
    headers: HeaderMap,
    pub body: Bytes,
    /// The tag the dev server issued (what a revalidation sends), and the one the browser saw.
    upstream_etag: Option<String>,
    etag: Option<String>,
    last_modified: Option<String>,
    /// How long the response itself says it stays fresh after it was validated (`max-age` less `Age`); `None` when it
    /// must be revalidated every time.
    own_fresh: Option<Duration>,
    validated_at: Mutex<Instant>,
    /// The page load ([`State::generation`]) whose batch last confirmed it; 0 for none.
    checked_in: AtomicU64,
    used: AtomicU64,
}

impl Entry {
    /// The tag a revalidation sends: the dev server's own.
    pub fn upstream_etag(&self) -> Option<&str> {
        self.upstream_etag.as_deref().or(self.etag.as_deref())
    }

    pub fn last_modified(&self) -> Option<&str> {
        self.last_modified.as_deref()
    }

    fn size(&self) -> usize {
        self.body.len() + self.key.len() + 512
    }

    fn validated_at(&self) -> Instant {
        *self.validated_at.lock().unwrap_or_else(|p| p.into_inner())
    }

    fn matches(&self, headers: &HeaderMap) -> bool {
        self.vary.iter().all(|(name, value)| header_text(headers, name).as_deref() == value.as_deref())
    }
}

/// How long a response says it stays fresh: its `max-age` less its `Age`. `None` with `no-cache`, `must-revalidate`,
/// `no-store`, `max-age=0` or no `max-age` at all: such a response is revalidated every time outside a page load's batch.
fn own_freshness(headers: &HeaderMap) -> Option<Duration> {
    if ["no-cache", "must-revalidate", "no-store", "proxy-revalidate"]
        .iter()
        .any(|d| has_directive(headers, hyper::header::CACHE_CONTROL, d))
    {
        return None;
    }
    let max_age = headers
        .get_all(hyper::header::CACHE_CONTROL)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|value| value.split(','))
        .find_map(|part| {
            let (name, value) = part.split_once('=')?;
            name.trim().eq_ignore_ascii_case("max-age").then(|| value.trim().trim_matches('"').parse::<u64>().ok()).flatten()
        })?;
    let age = header_text(headers, "age").and_then(|age| age.trim().parse::<u64>().ok()).unwrap_or(0);
    Some(Duration::from_secs(max_age.saturating_sub(age))).filter(|fresh| !fresh.is_zero())
}

/// A request for something the page itself loads (a script, style, image, font or worker), not data it fetches: only
/// those are answered from what the page load's batch confirmed. A `fetch` or XHR has an empty destination.
fn is_subresource(headers: &HeaderMap) -> bool {
    header_text(headers, "sec-fetch-dest").is_some_and(|dest| {
        matches!(dest.trim().to_ascii_lowercase().as_str(), "script" | "style" | "image" | "font" | "worker" | "sharedworker")
    })
}

fn header_text(headers: &HeaderMap, name: &str) -> Option<String> {
    let values: Vec<&str> = headers.get_all(name).iter().filter_map(|value| value.to_str().ok()).collect();
    (!values.is_empty()).then(|| values.join(", "))
}

pub enum Lookup {
    /// Confirmed by this page load's batch, or fresh by its own `max-age`: answer from here.
    Fresh(Arc<Entry>),
    /// Kept, but it must be asked about: ask with its validators.
    Stale(Arc<Entry>),
    Miss,
}

/// The batch that runs: the keys it has not answered yet (a key may have several variants), and a counter that moves on
/// each record so a waiting request looks again.
struct Batch {
    pending: HashMap<String, usize>,
    progress: tokio::sync::watch::Sender<u64>,
}

#[derive(Default)]
struct State {
    entries: HashMap<String, Vec<Arc<Entry>>>,
    size: usize,
    invalidated_at: Option<Instant>,
    /// When a write (a request other than `GET`, `HEAD`, `OPTIONS` or `TRACE`) to each host last went by.
    written_at: HashMap<String, Instant>,
    batch: Option<Batch>,
    last_batch: Option<Instant>,
    /// Page loads that started a batch of their own; what a batch confirms counts for its page load only.
    generation: u64,
}

/// A batch in progress, owned by the task that sends it: which entries it asks about and which were answered.
pub struct BatchRun {
    pub entries: Vec<Arc<Entry>>,
    answered: Vec<bool>,
    generation: u64,
    sent: Instant,
}

/// One forward's cache.
#[derive(Default)]
pub struct Cache {
    state: Mutex<State>,
    clock: AtomicU64,
    /// Bytes answered from here and bytes that crossed the network, over the forward's life.
    pub served: AtomicU64,
}

impl Drop for Cache {
    fn drop(&mut self) {
        let size = self.lock().size;
        TOTAL.fetch_sub(size, Ordering::Relaxed);
    }
}

/// The key of a request this cache may answer: host and path, for a plain `GET` with nothing that makes it personal or
/// partial.
pub fn request_key(method: &hyper::Method, headers: &HeaderMap, path: &str) -> Option<String> {
    if method != hyper::Method::GET
        || headers.contains_key(hyper::header::AUTHORIZATION)
        || headers.contains_key(hyper::header::RANGE)
        || headers.contains_key(hyper::header::UPGRADE)
        || has_directive(headers, hyper::header::CACHE_CONTROL, "no-store")
    {
        return None;
    }
    let host = header_text(headers, "host").unwrap_or_default();
    Some(format!("{host}{path}"))
}

fn has_directive(headers: &HeaderMap, name: HeaderName, directive: &str) -> bool {
    headers
        .get_all(name)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|value| value.split(','))
        .any(|part| part.trim().split('=').next().is_some_and(|name| name.trim().eq_ignore_ascii_case(directive)))
}

/// The request asks for an answer the server has checked: `Cache-Control: no-cache` or `max-age=0`, or `Pragma: no-cache`.
fn wants_checked(headers: &HeaderMap) -> bool {
    has_directive(headers, hyper::header::CACHE_CONTROL, "no-cache")
        || has_directive(headers, hyper::header::PRAGMA, "no-cache")
        || headers
            .get_all(hyper::header::CACHE_CONTROL)
            .iter()
            .filter_map(|value| value.to_str().ok())
            .flat_map(|value| value.split(','))
            .any(|part| part.trim().eq_ignore_ascii_case("max-age=0"))
}

/// Whether a response may be kept, and the request headers it varies on.
pub fn storable(parts: &hyper::http::response::Parts) -> Option<Vec<String>> {
    let headers = &parts.headers;
    if parts.status != StatusCode::OK
        || !(headers.contains_key(hyper::header::ETAG) || headers.contains_key(hyper::header::LAST_MODIFIED))
        || headers.contains_key(hyper::header::SET_COOKIE)
        || headers.contains_key(hyper::header::CONTENT_RANGE)
        || has_directive(headers, hyper::header::CACHE_CONTROL, "no-store")
        || has_directive(headers, hyper::header::CACHE_CONTROL, "private")
    {
        return None;
    }
    let kind = header_text(headers, "content-type").unwrap_or_default().to_ascii_lowercase();
    if kind.starts_with("text/event-stream") {
        return None;
    }
    if header_text(headers, "content-length").and_then(|n| n.trim().parse::<usize>().ok()).is_some_and(|n| n > MAX_ENTRY) {
        return None;
    }
    let mut vary: Vec<String> = headers
        .get_all(hyper::header::VARY)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|value| value.split(','))
        .map(|name| name.trim().to_ascii_lowercase())
        .filter(|name| !name.is_empty())
        .collect();
    if vary.iter().any(|name| name == "*") {
        return None;
    }
    vary.sort();
    vary.dedup();
    Some(vary)
}

/// Whether the browser's conditional request matches the kept response (`If-None-Match` compares weakly).
fn browser_has(entry: &Entry, headers: &HeaderMap) -> bool {
    let weak = |tag: &str| tag.trim().trim_start_matches("W/").to_string();
    if let Some(asked) = header_text(headers, "if-none-match") {
        let Some(tag) = entry.etag.as_deref() else { return false };
        return asked.split(',').any(|candidate| candidate.trim() == "*" || weak(candidate) == weak(tag));
    }
    match (header_text(headers, "if-modified-since"), entry.last_modified.as_deref()) {
        (Some(since), Some(modified)) => since.trim() == modified.trim(),
        _ => false,
    }
}

/// The answer to `request_headers` from a kept response: `304` when the browser already has it.
pub fn respond(entry: &Entry, request_headers: &HeaderMap) -> Response<ProxyBody> {
    if browser_has(entry, request_headers) {
        let mut response = status_response(StatusCode::NOT_MODIFIED);
        for name in [hyper::header::ETAG, hyper::header::CACHE_CONTROL, hyper::header::LAST_MODIFIED, hyper::header::VARY] {
            for value in entry.headers.get_all(&name) {
                response.headers_mut().append(name.clone(), value.clone());
            }
        }
        return response;
    }
    let mut response = Response::new(Full::new(entry.body.clone()).map_err(|never| match never {}).boxed());
    *response.status_mut() = entry.status;
    *response.headers_mut() = entry.headers.clone();
    response.headers_mut().insert(hyper::header::CONTENT_LENGTH, HeaderValue::from(entry.body.len()));
    response
}

impl Cache {
    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// Whether `entry` may be answered here for a request with `headers`: validated after the last hot-reload message and
    /// the last write to its host, and then either confirmed by this page load's batch (for a subresource of the page) or
    /// fresh by its own `max-age` (unless the request asks for a checked answer).
    fn fresh(&self, state: &State, entry: &Entry, headers: &HeaderMap) -> bool {
        let at = entry.validated_at();
        let current = state.invalidated_at.is_none_or(|invalidated| at > invalidated)
            && state.written_at.get(&entry.host).is_none_or(|written| at > *written);
        if !current {
            return false;
        }
        let checked = entry.checked_in.load(Ordering::Acquire);
        let by_batch = checked != 0 && checked == state.generation && at.elapsed() < BATCH_FRESH && is_subresource(headers);
        let by_itself = !wants_checked(headers) && entry.own_fresh.is_some_and(|fresh| at.elapsed() < fresh);
        by_batch || by_itself
    }

    /// The kept response for a request, and whether it may be answered without asking the target.
    pub fn lookup(&self, key: &str, headers: &HeaderMap) -> Lookup {
        let state = self.lock();
        let Some(entry) = state.entries.get(key).and_then(|variants| variants.iter().find(|entry| entry.matches(headers))) else {
            return Lookup::Miss;
        };
        entry.used.store(self.clock.fetch_add(1, Ordering::Relaxed), Ordering::Relaxed);
        if self.fresh(&state, entry, headers) { Lookup::Fresh(entry.clone()) } else { Lookup::Stale(entry.clone()) }
    }

    /// A request that may change what the server answers (`POST`, `PUT`, `PATCH`, `DELETE`, …) went to `host`, or its
    /// answer arrived: everything kept for that host is asked about again before it is used.
    pub fn written(&self, host: &str) {
        let mut state = self.lock();
        if state.written_at.len() > 64 {
            state.written_at.clear();
            state.invalidated_at = Some(Instant::now());
        }
        state.written_at.insert(host.to_string(), Instant::now());
    }

    /// The dev server confirmed `entry` (a `304`) for a request sent at `sent`.
    pub fn confirm(&self, entry: &Entry, sent: Instant) {
        let mut at = entry.validated_at.lock().unwrap_or_else(|p| p.into_inner());
        *at = (*at).max(sent);
    }

    /// Keep a response. `request` are the browser's request headers, `parts` the response as it travelled (headers that
    /// belong to Kybern are taken out here), `sent` when the request left: the body is what the server had then, so a
    /// hot-reload message after that point makes it stale even when the body arrives later.
    pub fn store(
        &self,
        key: String,
        request: &HeaderMap,
        mut parts: hyper::http::response::Parts,
        mut vary: Vec<String>,
        body: Bytes,
        sent: Instant,
    ) {
        if body.len() > MAX_ENTRY {
            return;
        }
        // A cookie is part of what the browser asked for even when the server does not say it varies on it: a page for
        // one session is never answered to another, and one kept with no cookie (signed out) never to a browser with one.
        if !vary.iter().any(|name| name == "cookie") {
            vary.push("cookie".into());
        }
        let upstream_etag = parts
            .headers
            .remove(ORIGINAL_ETAG_HEADER)
            .or_else(|| parts.headers.get(hyper::header::ETAG).cloned())
            .and_then(|tag| tag.to_str().ok().map(str::to_string));
        parts.headers.remove(hyper::header::CONTENT_LENGTH);
        let mut replay: Vec<(String, String)> = REPLAYED
            .iter()
            .chain(vary.iter().map(String::as_str).collect::<Vec<_>>().iter())
            .filter_map(|name| header_text(request, name).map(|value| (name.to_string(), value)))
            .collect();
        replay.sort();
        replay.dedup();
        let entry = Arc::new(Entry {
            vary: vary.iter().map(|name| (name.clone(), header_text(request, name))).collect(),
            replay,
            status: parts.status,
            etag: header_text(&parts.headers, "etag"),
            last_modified: header_text(&parts.headers, "last-modified"),
            own_fresh: own_freshness(&parts.headers),
            upstream_etag,
            headers: parts.headers,
            body,
            host: header_text(request, "host").unwrap_or_default(),
            key: key.clone(),
            validated_at: Mutex::new(sent),
            checked_in: AtomicU64::new(0),
            used: AtomicU64::new(self.clock.fetch_add(1, Ordering::Relaxed)),
        });
        self.insert(entry);
    }

    fn insert(&self, entry: Arc<Entry>) {
        let size = entry.size();
        let mut state = self.lock();
        Self::remove_variant(&mut state, &entry.key, |held| held.vary == entry.vary);
        while state.size + size > MAX_FORWARD || (TOTAL.load(Ordering::Relaxed) + size > MAX_TOTAL && state.size > 0) {
            let oldest = state
                .entries
                .values()
                .flatten()
                .min_by_key(|held| held.used.load(Ordering::Relaxed))
                .map(|held| (held.key.clone(), held.vary.clone()));
            let Some((key, vary)) = oldest else { break };
            Self::remove_variant(&mut state, &key, |held| held.vary == vary);
        }
        if state.size + size > MAX_FORWARD || TOTAL.load(Ordering::Relaxed) + size > MAX_TOTAL {
            return;
        }
        state.size += size;
        TOTAL.fetch_add(size, Ordering::Relaxed);
        state.entries.entry(entry.key.clone()).or_default().push(entry);
    }

    fn remove_variant(state: &mut State, key: &str, which: impl Fn(&Entry) -> bool) {
        let Some(variants) = state.entries.get_mut(key) else { return };
        let mut freed = 0;
        variants.retain(|held| {
            let drop = which(held);
            if drop {
                freed += held.size();
            }
            !drop
        });
        if variants.is_empty() {
            state.entries.remove(key);
        }
        state.size -= freed;
        TOTAL.fetch_sub(freed, Ordering::Relaxed);
    }

    pub fn remove(&self, entry: &Entry) {
        let mut state = self.lock();
        Self::remove_variant(&mut state, &entry.key, |held| std::ptr::eq(held, entry));
    }

    /// A hot-reload message: whatever is kept is asked about again before it is used.
    pub fn invalidate(&self) {
        self.lock().invalidated_at = Some(Instant::now());
    }

    pub fn len(&self) -> usize {
        self.lock().entries.values().map(Vec::len).sum()
    }

    pub fn bytes(&self) -> usize {
        self.lock().size
    }

    /// Start a batch for a page load on `host`: the kept responses of that host to revalidate, or `None` when there are
    /// none or a batch runs or just ran (two navigations that close together share one batch and one page load).
    pub fn begin_batch(&self, host: &str) -> Option<BatchRun> {
        let mut state = self.lock();
        if state.batch.is_some() || state.last_batch.is_some_and(|at| at.elapsed() < BATCH_SPACING) {
            return None;
        }
        // A new page load: what an earlier batch confirmed no longer counts for it.
        state.generation += 1;
        state.last_batch = Some(Instant::now());
        // The plain and the Fast listener are different hosts with their own entries; a page loads from one of them.
        let mut entries: Vec<Arc<Entry>> = state
            .entries
            .values()
            .flatten()
            .filter(|entry| entry.key.strip_prefix(host).is_some_and(|path| path.starts_with('/')))
            .cloned()
            .collect();
        if entries.is_empty() {
            return None;
        }
        entries.sort_by_key(|entry| std::cmp::Reverse(entry.used.load(Ordering::Relaxed)));
        entries.truncate(MAX_BATCH);
        let mut pending = HashMap::new();
        for entry in &entries {
            *pending.entry(entry.key.clone()).or_insert(0) += 1;
        }
        state.batch = Some(Batch { pending, progress: tokio::sync::watch::Sender::new(0) });
        let answered = vec![false; entries.len()];
        Some(BatchRun { entries, answered, generation: state.generation, sent: Instant::now() })
    }

    /// Apply one record of a batch's answer as it arrives, and let the requests waiting for that entry go on.
    pub fn apply(&self, run: &mut BatchRun, outcome: Outcome) {
        let Some(entry) = run.entries.get(outcome.index).cloned() else { return };
        if std::mem::replace(&mut run.answered[outcome.index], true) {
            return;
        }
        // What the batch confirmed was true when it left.
        let now = run.sent;
        match outcome.state {
            Answer::Fresh => {
                let mut at = entry.validated_at.lock().unwrap_or_else(|p| p.into_inner());
                *at = (*at).max(now);
                entry.checked_in.store(run.generation, Ordering::Release);
            }
            Answer::Changed { mut headers, body } => {
                let etag = header_text(&headers, "etag");
                let upstream = headers.remove(ORIGINAL_ETAG_HEADER).and_then(|tag| tag.to_str().ok().map(str::to_string));
                let changed = Arc::new(Entry {
                    key: entry.key.clone(),
                    host: entry.host.clone(),
                    vary: entry.vary.clone(),
                    replay: entry.replay.clone(),
                    status: StatusCode::OK,
                    upstream_etag: upstream.or_else(|| etag.clone()),
                    etag,
                    last_modified: header_text(&headers, "last-modified"),
                    own_fresh: own_freshness(&headers),
                    headers,
                    body,
                    validated_at: Mutex::new(now),
                    checked_in: AtomicU64::new(run.generation),
                    used: AtomicU64::new(entry.used.load(Ordering::Relaxed)),
                });
                self.remove(&entry);
                self.insert(changed);
            }
            Answer::Gone => self.remove(&entry),
        }
        let mut state = self.lock();
        if let Some(batch) = &mut state.batch {
            if let Some(left) = batch.pending.get_mut(&entry.key) {
                *left = left.saturating_sub(1);
                if *left == 0 {
                    batch.pending.remove(&entry.key);
                }
            }
            batch.progress.send_modify(|n| *n += 1);
        }
    }

    /// End a batch. `complete` when its whole answer arrived: an entry it left out is then dropped. After a failure or a
    /// timeout what already arrived stays applied, and the rest is asked about one by one.
    pub fn end_batch(&self, run: BatchRun, complete: bool) {
        if complete {
            for (entry, answered) in run.entries.iter().zip(&run.answered) {
                if !answered {
                    self.remove(entry);
                }
            }
        }
        let mut state = self.lock();
        if let Some(batch) = state.batch.take() {
            batch.progress.send_modify(|n| *n += 1);
        }
    }

    /// Wait, if a batch runs and has not answered `key` yet, until it has (or ends, or [`BATCH_WAIT`] passes).
    pub async fn wait_for_batch(&self, key: &str) {
        let deadline = tokio::time::Instant::now() + BATCH_WAIT;
        loop {
            // Subscribed under the lock that `apply` and `end_batch` take, so no record is missed between the check and the
            // wait.
            let mut progress = {
                let state = self.lock();
                match &state.batch {
                    Some(batch) if batch.pending.contains_key(key) => batch.progress.subscribe(),
                    _ => return,
                }
            };
            match tokio::time::timeout_at(deadline, progress.changed()).await {
                Ok(Ok(())) => continue,
                _ => return,
            }
        }
    }
}

/// How many changed bytes a batch may carry: what the forward's link carried in [`BUDGET_TIME`] at its last measured
/// rate, within [`MIN_BATCH_BODIES`] and [`MAX_BATCH_BODIES`]; the most when nothing was measured yet.
pub fn batch_budget(bytes_per_second: Option<u64>) -> usize {
    match bytes_per_second {
        Some(rate) => ((rate as f64 * BUDGET_TIME.as_secs_f64()) as usize).clamp(MIN_BATCH_BODIES, MAX_BATCH_BODIES),
        None => MAX_BATCH_BODIES,
    }
}

/// Whether a request is a page load (a document or frame navigation), which starts a batch.
pub fn is_navigation(method: &hyper::Method, headers: &HeaderMap) -> bool {
    if method != hyper::Method::GET {
        return false;
    }
    match (header_text(headers, "sec-fetch-mode"), header_text(headers, "sec-fetch-dest")) {
        (Some(mode), _) => mode.eq_ignore_ascii_case("navigate"),
        (None, Some(dest)) => matches!(dest.to_ascii_lowercase().as_str(), "document" | "iframe" | "frame"),
        (None, None) => header_text(headers, "accept").is_some_and(|accept| accept.to_ascii_lowercase().starts_with("text/html")),
    }
}

// ---- the batch on the wire ----

#[derive(Serialize, Deserialize)]
struct BatchRequest {
    entries: Vec<BatchEntry>,
    /// Changed bytes the answer may carry ([`batch_budget`]); at most [`MAX_BATCH_BODIES`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    budget: Option<usize>,
}

#[derive(Serialize, Deserialize)]
struct BatchEntry {
    path: String,
    headers: Vec<(String, String)>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    etag: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    last_modified: Option<String>,
}

#[derive(Serialize, Deserialize)]
struct RecordHead {
    i: usize,
    /// `fresh`, `changed` (the new headers and a body follow) or `gone`.
    state: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    headers: Vec<(String, String)>,
}

pub enum Answer {
    Fresh,
    Changed { headers: HeaderMap, body: Bytes },
    Gone,
}

pub struct Outcome {
    pub index: usize,
    pub state: Answer,
}

/// The path of a key (`host` + path).
fn path_of(entry: &Entry) -> &str {
    let host = entry.replay.iter().find(|(name, _)| name == "host").map(|(_, host)| host.len()).unwrap_or(0);
    &entry.key[host.min(entry.key.len())..]
}

/// The request a batch sends over the forward's HTTP/2 connection. `authority` is any host of the forward.
pub fn batch_request(entries: &[Arc<Entry>], authority: &str, budget: usize) -> Option<Request<ProxyBody>> {
    let body = BatchRequest {
        budget: Some(budget),
        entries: entries
            .iter()
            .map(|entry| BatchEntry {
                path: path_of(entry).to_string(),
                headers: entry.replay.clone(),
                etag: entry.upstream_etag.clone().or_else(|| entry.etag.clone()),
                last_modified: entry.last_modified.clone(),
            })
            .collect(),
    };
    let body = serde_json::to_vec(&body).ok()?;
    Request::post(format!("https://{authority}/"))
        .version(hyper::Version::HTTP_2)
        .header(REVALIDATE_HEADER, "1")
        .header(hyper::header::CONTENT_TYPE, "application/json")
        .body(Full::new(Bytes::from(body)).map_err(|never| match never {}).boxed())
        .ok()
}

/// Reads a batch answer as it streams: length-prefixed JSON heads, each `changed` one followed by a length-prefixed
/// body. Each record is handed out as soon as it is whole.
#[derive(Default)]
pub struct BatchReader {
    buffer: Vec<u8>,
}

impl BatchReader {
    /// Add bytes; the records they complete, or `None` when the answer is malformed.
    pub fn feed(&mut self, bytes: &[u8]) -> Option<Vec<Outcome>> {
        self.buffer.extend_from_slice(bytes);
        let mut outcomes = Vec::new();
        let mut used = 0;
        loop {
            let rest = &self.buffer[used..];
            let Some((head, head_len)) = Self::part(rest)? else { break };
            let head: RecordHead = serde_json::from_slice(head).ok()?;
            let mut taken = head_len;
            let state = match head.state.as_str() {
                "fresh" => Answer::Fresh,
                "changed" => {
                    let Some((body, body_len)) = Self::part(&rest[taken..])? else { break };
                    taken += body_len;
                    let mut headers = HeaderMap::new();
                    for (name, value) in head.headers {
                        if let (Ok(name), Ok(value)) = (HeaderName::from_bytes(name.as_bytes()), HeaderValue::from_str(&value)) {
                            headers.append(name, value);
                        }
                    }
                    Answer::Changed { headers, body: Bytes::copy_from_slice(body) }
                }
                _ => Answer::Gone,
            };
            used += taken;
            outcomes.push(Outcome { index: head.i, state });
        }
        self.buffer.drain(..used);
        Some(outcomes)
    }

    /// Nothing is left half read: the answer ended on a record boundary.
    pub fn finished(&self) -> bool {
        self.buffer.is_empty()
    }

    /// One length-prefixed part and the bytes it takes: `Some(None)` while incomplete, `None` when it cannot be one.
    fn part(bytes: &[u8]) -> Option<Option<(&[u8], usize)>> {
        let Some(prefix) = bytes.get(..4) else { return Some(None) };
        let len = u32::from_be_bytes(prefix.try_into().ok()?) as usize;
        if len > MAX_ENTRY {
            return None;
        }
        Some(bytes.get(4..4 + len).map(|part| (part, 4 + len)))
    }
}

#[cfg(test)]
fn parse_batch(bytes: &[u8]) -> Option<Vec<Outcome>> {
    let mut reader = BatchReader::default();
    let outcomes = reader.feed(bytes)?;
    reader.finished().then_some(outcomes)
}

fn record(head: &RecordHead, body: Option<&[u8]>) -> Bytes {
    let mut out = Vec::new();
    push_record(&mut out, head, body);
    Bytes::from(out)
}

fn push_record(out: &mut Vec<u8>, head: &RecordHead, body: Option<&[u8]>) {
    let head = serde_json::to_vec(head).unwrap_or_default();
    out.extend_from_slice(&(head.len() as u32).to_be_bytes());
    out.extend_from_slice(&head);
    if let Some(body) = body {
        out.extend_from_slice(&(body.len() as u32).to_be_bytes());
        out.extend_from_slice(body);
    }
}

// ---- the target: one batch, many loopback checks ----

/// Answer a batched revalidation from the viewing machine: each entry is asked of the dev server with its validators, and
/// each record is sent as soon as its check ends. Fresh and gone records go ahead of changed bodies still waiting to be
/// sent, so on a slow link the page's unchanged requests are let go first.
pub(super) async fn serve_revalidate(upstream: SocketAddr, request: Request<Incoming>) -> Response<ProxyBody> {
    let authority = request.uri().authority().map(|authority| authority.to_string());
    let Ok(body) = Limited::new(request.into_body(), 8 * 1024 * 1024).collect().await else {
        return status_response(StatusCode::PAYLOAD_TOO_LARGE);
    };
    let Ok(batch) = serde_json::from_slice::<BatchRequest>(&body.to_bytes()) else {
        return status_response(StatusCode::BAD_REQUEST);
    };
    let started = Instant::now();
    // Its own small pool: the forward's pool carries the browser's request bodies, these have none.
    let mut connector = hyper_util::client::legacy::connect::HttpConnector::new();
    connector.set_nodelay(true);
    connector.set_connect_timeout(Some(Duration::from_secs(3)));
    let client: CheckClient = hyper_util::client::legacy::Client::builder(hyper_util::rt::TokioExecutor::new())
        .pool_max_idle_per_host(BATCH_CONCURRENCY)
        .build(connector);
    let budget = Arc::new(AtomicUsize::new(batch.budget.unwrap_or(MAX_BATCH_BODIES).min(MAX_BATCH_BODIES)));
    let count = batch.entries.len().min(MAX_BATCH);
    let (quick_tx, quick) = tokio::sync::mpsc::unbounded_channel::<Bytes>();
    let (changed_tx, changed) = tokio::sync::mpsc::unbounded_channel::<Bytes>();
    tokio::spawn(async move {
        let checks = futures::stream::iter(batch.entries.into_iter().take(MAX_BATCH).enumerate())
            .map(|(i, entry)| {
                let (client, authority, budget) = (client.clone(), authority.clone(), budget.clone());
                async move { (i, check_one(&client, upstream, authority.as_deref(), entry, &budget).await) }
            })
            .buffer_unordered(BATCH_CONCURRENCY);
        futures::pin_mut!(checks);
        let (mut fresh, mut changed_count) = (0, 0);
        while let Some((i, result)) = checks.next().await {
            let sent = match result {
                Checked::Fresh => {
                    fresh += 1;
                    quick_tx.send(record(&RecordHead { i, state: "fresh".into(), headers: Vec::new() }, None))
                }
                Checked::Changed(headers, body) => {
                    changed_count += 1;
                    changed_tx.send(record(&RecordHead { i, state: "changed".into(), headers }, Some(&body)))
                }
                Checked::Gone => quick_tx.send(record(&RecordHead { i, state: "gone".into(), headers: Vec::new() }, None)),
            };
            // The viewing machine stopped reading: the rest is not wanted.
            if sent.is_err() {
                break;
            }
        }
        tracing::debug!(target: TARGET, count, fresh, changed = changed_count, ms = started.elapsed().as_millis() as u64, "revalidated a forward's cache");
    });
    let records = futures::stream::unfold((quick, changed), |(mut quick, mut changed)| async move {
        let next = tokio::select! {
            biased;
            Some(record) = quick.recv() => record,
            Some(record) = changed.recv() => record,
            else => return None,
        };
        Some((Ok::<_, hyper::Error>(hyper::body::Frame::data(next)), (quick, changed)))
    });
    let mut response = Response::new(BodyExt::boxed(http_body_util::StreamBody::new(records)));
    response.headers_mut().insert(hyper::header::CONTENT_TYPE, HeaderValue::from_static("application/octet-stream"));
    response
}

/// Take `amount` from `left` when that much is left, in one step however many tasks take at once.
pub(super) fn take_from(left: &AtomicUsize, amount: usize) -> bool {
    let mut current = left.load(Ordering::Acquire);
    loop {
        let Some(next) = current.checked_sub(amount) else { return false };
        match left.compare_exchange_weak(current, next, Ordering::AcqRel, Ordering::Acquire) {
            Ok(_) => return true,
            Err(seen) => current = seen,
        }
    }
}

enum Checked {
    Fresh,
    Changed(Vec<(String, String)>, Vec<u8>),
    Gone,
}

type CheckClient = hyper_util::client::legacy::Client<hyper_util::client::legacy::connect::HttpConnector, http_body_util::Empty<Bytes>>;

async fn check_one(
    client: &CheckClient,
    upstream: SocketAddr,
    authority: Option<&str>,
    entry: BatchEntry,
    budget: &AtomicUsize,
) -> Checked {
    if !entry.path.starts_with('/') {
        return Checked::Gone;
    }
    let Ok(uri) = format!("http://{upstream}{}", entry.path).parse::<hyper::Uri>() else { return Checked::Gone };
    let mut headers = HeaderMap::new();
    for (name, value) in &entry.headers {
        if let (Ok(name), Ok(value)) = (HeaderName::from_bytes(name.as_bytes()), HeaderValue::from_str(value)) {
            headers.append(name, value);
        }
    }
    strip_hop_headers(&mut headers);
    direct::take_kybern_headers(&mut headers);
    direct::set_dev_host(&mut headers, authority);
    if let Some(value) = entry.etag.as_deref().and_then(|tag| HeaderValue::from_str(tag).ok()) {
        headers.insert(hyper::header::IF_NONE_MATCH, value);
    }
    if let Some(value) = entry.last_modified.as_deref().and_then(|at| HeaderValue::from_str(at).ok()) {
        headers.insert(hyper::header::IF_MODIFIED_SINCE, value);
    }
    let gzip_ok = direct::accepts_gzip(&headers);
    let mut request = Request::new(http_body_util::Empty::<Bytes>::new());
    *request.uri_mut() = uri;
    *request.headers_mut() = headers;
    // A dev server that holds a request (Vite while it optimizes dependencies) must not hold the whole batch: that
    // response is reported gone and the browser asks for it as usual.
    let Ok(Ok(response)) = tokio::time::timeout(CHECK_TIMEOUT, client.request(request)).await else { return Checked::Gone };
    if response.status() == StatusCode::NOT_MODIFIED {
        return Checked::Fresh;
    }
    let (mut parts, body) = response.into_parts();
    if storable(&parts).is_none() {
        return Checked::Gone;
    }
    // Only small changed bodies travel in the batch, so its answer stays quick on a slow link and the page's requests
    // are not held behind a large one; a large change is fetched by the browser and streams as usual.
    let Ok(Ok(body)) = tokio::time::timeout(CHECK_TIMEOUT, Limited::new(body, MAX_BATCH_BODY).collect()).await else {
        return Checked::Gone;
    };
    let mut body = body.to_bytes().to_vec();
    strip_hop_headers(&mut parts.headers);
    if gzip_ok && direct::compressible(&parts) {
        let Some(zipped) = direct::gzip_all(&body) else { return Checked::Gone };
        direct::mark_gzipped(&mut parts.headers, true);
        body = zipped;
    }
    parts.headers.remove(hyper::header::CONTENT_LENGTH);
    // A changed body counts against the batch's budget; past it the viewing machine fetches it as usual.
    if !take_from(budget, body.len()) {
        return Checked::Gone;
    }
    let headers = parts
        .headers
        .iter()
        .filter_map(|(name, value)| value.to_str().ok().map(|value| (name.as_str().to_string(), value.to_string())))
        .collect();
    Checked::Changed(headers, body)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parts(status: u16, headers: &[(&str, &str)]) -> hyper::http::response::Parts {
        let mut builder = Response::builder().status(status);
        for (name, value) in headers {
            builder = builder.header(*name, *value);
        }
        builder.body(()).unwrap().into_parts().0
    }

    fn request(headers: &[(&str, &str)]) -> HeaderMap {
        let mut map = HeaderMap::new();
        for (name, value) in headers {
            map.append(HeaderName::from_bytes(name.as_bytes()).unwrap(), HeaderValue::from_str(value).unwrap());
        }
        map
    }

    const SCRIPT: (&str, &str) = ("sec-fetch-dest", "script");

    #[test]
    fn only_whole_validated_shareable_answers_are_kept() {
        let tag = ("etag", "W/\"1\"");
        assert_eq!(
            storable(&parts(200, &[tag, ("vary", "Accept-Encoding, Origin")])),
            Some(vec!["accept-encoding".into(), "origin".into()])
        );
        assert!(storable(&parts(200, &[("last-modified", "Tue, 03 Nov 2026 00:00:00 GMT")])).is_some());
        assert!(storable(&parts(200, &[])).is_none(), "no validator");
        assert!(storable(&parts(206, &[tag])).is_none());
        assert!(storable(&parts(304, &[tag])).is_none());
        assert!(storable(&parts(200, &[tag, ("set-cookie", "a=b")])).is_none());
        assert!(storable(&parts(200, &[tag, ("cache-control", "private, max-age=10")])).is_none());
        assert!(storable(&parts(200, &[tag, ("cache-control", "no-store")])).is_none());
        assert!(storable(&parts(200, &[tag, ("vary", "*")])).is_none());
        assert!(storable(&parts(200, &[tag, ("content-range", "bytes 0-1/2")])).is_none());
        assert!(storable(&parts(200, &[tag, ("content-type", "text/event-stream")])).is_none());
        // Kept so its body need not cross again, but asked about on every use outside a batch.
        assert!(storable(&parts(200, &[tag, ("cache-control", "no-cache")])).is_some());

        let get = hyper::Method::GET;
        assert_eq!(request_key(&get, &request(&[("host", "localhost:3000")]), "/a.js?v=1").as_deref(), Some("localhost:3000/a.js?v=1"));
        assert!(request_key(&hyper::Method::POST, &request(&[]), "/").is_none());
        assert!(request_key(&get, &request(&[("authorization", "Bearer x")]), "/").is_none());
        assert!(request_key(&get, &request(&[("range", "bytes=0-1")]), "/").is_none());
        assert!(request_key(&get, &request(&[("cache-control", "no-store")]), "/").is_none());
    }

    #[test]
    fn a_response_is_fresh_by_itself_only_for_its_own_max_age() {
        let fresh = |headers: &[(&str, &str)]| own_freshness(&parts(200, headers).headers);
        assert_eq!(fresh(&[("cache-control", "public, max-age=31536000, immutable")]), Some(Duration::from_secs(31_536_000)));
        assert_eq!(fresh(&[("cache-control", "max-age=60"), ("age", "20")]), Some(Duration::from_secs(40)));
        assert_eq!(fresh(&[("cache-control", "max-age=60"), ("age", "90")]), None, "already older than its life");
        assert_eq!(fresh(&[]), None, "no max-age: no promise");
        assert_eq!(fresh(&[("cache-control", "no-cache")]), None);
        assert_eq!(fresh(&[("cache-control", "max-age=0")]), None);
        assert_eq!(fresh(&[("cache-control", "max-age=600, must-revalidate")]), None, "revalidated every time");
        assert_eq!(fresh(&[("cache-control", "no-cache, max-age=600")]), None);
        assert!(is_subresource(&request(&[SCRIPT])));
        for dest in ["style", "image", "font", "worker"] {
            assert!(is_subresource(&request(&[("sec-fetch-dest", dest)])), "{dest}");
        }
        assert!(!is_subresource(&request(&[("sec-fetch-dest", "empty")])), "fetch and XHR");
        assert!(!is_subresource(&request(&[("sec-fetch-dest", "document")])));
        assert!(!is_subresource(&HeaderMap::new()), "a client that does not say");
    }

    fn keep(cache: &Cache, key: &str, request_headers: &[(&str, &str)], response: &[(&str, &str)], body: &str) {
        let parts = parts(200, response);
        let vary = storable(&parts).unwrap();
        cache.store(key.into(), &request(request_headers), parts, vary, Bytes::from(body.to_string()), Instant::now());
    }

    fn kept(lookup: Lookup) -> bool {
        !matches!(lookup, Lookup::Miss)
    }

    #[test]
    fn a_kept_answer_matches_its_variant_and_the_browser_gets_a_304_for_what_it_has() {
        let cache = Cache::default();
        let gzip = [("host", "localhost:3000"), ("accept-encoding", "gzip")];
        keep(&cache, "localhost:3000/a.js", &gzip, &[("etag", "W/\"1\""), ("x-kybern-etag", "\"1\""), ("vary", "accept-encoding")], "zip");
        let Lookup::Stale(entry) = cache.lookup("localhost:3000/a.js", &request(&gzip)) else { panic!("kept, but asked about") };
        assert_eq!(entry.upstream_etag.as_deref(), Some("\"1\""), "revalidation sends the dev server's own tag");
        assert!(!entry.headers.contains_key(ORIGINAL_ETAG_HEADER), "the browser never sees Kybern's header");
        assert!(matches!(cache.lookup("localhost:3000/a.js", &request(&[("accept-encoding", "br")])), Lookup::Miss), "another variant");
        let full = respond(&entry, &request(&gzip));
        assert_eq!(full.status(), 200);
        assert_eq!(full.headers()["content-length"], "3");
        assert_eq!(respond(&entry, &request(&[("if-none-match", "\"1\"")])).status(), 304, "weak comparison");
        assert_eq!(respond(&entry, &request(&[("if-none-match", "\"2\"")])).status(), 200);
    }

    #[test]
    fn data_is_never_answered_from_here_without_asking_and_a_write_makes_its_host_stale() {
        // Express answers `res.json()` with a weak ETag and nothing else: GET, then POST, then GET must see the new list.
        let cache = Cache::default();
        let api = [("host", "h"), ("sec-fetch-dest", "empty")];
        keep(&cache, "h/api/items", &api, &[("etag", "W/\"a\""), ("content-type", "application/json")], "[1]");
        assert!(matches!(cache.lookup("h/api/items", &request(&api)), Lookup::Stale(_)), "a fetch is always asked about");
        // Even a batch's confirmation does not answer a fetch.
        let mut run = cache.begin_batch("h").unwrap();
        cache.apply(&mut run, Outcome { index: 0, state: Answer::Fresh });
        cache.end_batch(run, true);
        assert!(matches!(cache.lookup("h/api/items", &request(&api)), Lookup::Stale(_)));

        // A response fresh by its own max-age is answered here until a write to its host.
        keep(&cache, "h/logo.svg", &[("host", "h")], &[("etag", "\"l\""), ("cache-control", "max-age=600")], "<svg/>");
        keep(&cache, "other/logo.svg", &[("host", "other")], &[("etag", "\"l\""), ("cache-control", "max-age=600")], "<svg/>");
        assert!(matches!(cache.lookup("h/logo.svg", &request(&[("host", "h")])), Lookup::Fresh(_)));
        assert!(
            matches!(cache.lookup("h/logo.svg", &request(&[("host", "h"), ("cache-control", "no-cache")])), Lookup::Stale(_)),
            "a hard reload asks"
        );
        std::thread::sleep(Duration::from_millis(2));
        cache.written("h");
        assert!(matches!(cache.lookup("h/logo.svg", &request(&[("host", "h")])), Lookup::Stale(_)), "after a POST to the host");
        assert!(matches!(cache.lookup("other/logo.svg", &request(&[("host", "other")])), Lookup::Fresh(_)), "another host is left alone");
        let Lookup::Stale(entry) = cache.lookup("h/logo.svg", &request(&[("host", "h")])) else { unreachable!() };
        cache.confirm(&entry, Instant::now());
        assert!(matches!(cache.lookup("h/logo.svg", &request(&[("host", "h")])), Lookup::Fresh(_)), "confirmed after the write");
        cache.invalidate();
        assert!(matches!(cache.lookup("h/logo.svg", &request(&[("host", "h")])), Lookup::Stale(_)), "a hot reload makes it stale");
    }

    #[test]
    fn a_no_cache_module_is_answered_here_only_for_the_page_load_whose_batch_confirmed_it() {
        let cache = Cache::default();
        let module = [("host", "h"), SCRIPT];
        keep(&cache, "h/m.js", &module, &[("etag", "W/\"1\""), ("cache-control", "no-cache")], "m");
        assert!(matches!(cache.lookup("h/m.js", &request(&module)), Lookup::Stale(_)), "no-cache is asked about every time");
        let mut run = cache.begin_batch("h").unwrap();
        cache.apply(&mut run, Outcome { index: 0, state: Answer::Fresh });
        cache.end_batch(run, true);
        assert!(matches!(cache.lookup("h/m.js", &request(&module)), Lookup::Fresh(_)), "this load's batch checked it");
        assert!(
            matches!(cache.lookup("h/m.js", &request(&[("host", "h"), SCRIPT, ("cache-control", "no-cache")])), Lookup::Fresh(_)),
            "a hard reload's batch is its check"
        );
        assert!(matches!(cache.lookup("h/m.js", &request(&[("host", "h")])), Lookup::Stale(_)), "not for a request that is no subresource");
        // The next page load starts its own batch; until it confirms the module, the earlier confirmation does not count.
        cache.lock().last_batch = Some(Instant::now() - BATCH_SPACING);
        let run = cache.begin_batch("h").unwrap();
        assert!(matches!(cache.lookup("h/m.js", &request(&module)), Lookup::Stale(_)), "a new page load");
        cache.end_batch(run, false);
        assert!(kept(cache.lookup("h/m.js", &request(&module))), "an interrupted batch drops nothing");
    }

    #[test]
    fn the_cache_stays_within_its_size_dropping_the_least_recently_used() {
        let cache = Cache::default();
        // Four of these are just over what one forward keeps.
        let big = "x".repeat(MAX_FORWARD / 4);
        for name in ["a", "b", "c"] {
            keep(&cache, name, &[], &[("etag", "\"1\"")], &big);
        }
        assert!(kept(cache.lookup("a", &HeaderMap::new())), "a is used again");
        keep(&cache, "d", &[], &[("etag", "\"1\"")], &big);
        assert!(cache.bytes() <= MAX_FORWARD);
        assert!(matches!(cache.lookup("b", &HeaderMap::new()), Lookup::Miss), "the oldest unused one went");
        assert!(kept(cache.lookup("a", &HeaderMap::new())));
        keep(&cache, "huge", &[], &[("etag", "\"1\"")], &"x".repeat(MAX_ENTRY + 1));
        assert!(matches!(cache.lookup("huge", &HeaderMap::new()), Lookup::Miss));
        let before = TOTAL.load(Ordering::Relaxed);
        let held = cache.bytes();
        drop(cache);
        assert!(TOTAL.load(Ordering::Relaxed) <= before - held + MAX_FORWARD, "a closed forward gives its bytes back");
    }

    #[test]
    fn a_batch_answer_refreshes_replaces_and_drops_entries_record_by_record() {
        let cache = Arc::new(Cache::default());
        for name in ["/same", "/new", "/gone", "/silent"] {
            keep(&cache, &format!("h{name}"), &[("host", "h"), SCRIPT], &[("etag", "\"1\"")], name);
        }
        keep(&cache, "other/x", &[("host", "other")], &[("etag", "\"1\"")], "x");
        cache.invalidate();
        assert!(cache.begin_batch("nothing-here").is_none(), "no entries of that host");
        cache.lock().last_batch = None;
        let mut run = cache.begin_batch("h").unwrap();
        assert_eq!(run.entries.len(), 4, "only the page's own host");
        assert!(cache.begin_batch("h").is_none(), "one batch at a time");
        let index = |key: &str| run.entries.iter().position(|entry| entry.key == key).unwrap();
        assert_eq!(path_of(&run.entries[index("h/new")]), "/new");
        let mut wire = Vec::new();
        push_record(&mut wire, &RecordHead { i: index("h/same"), state: "fresh".into(), headers: vec![] }, None);
        push_record(
            &mut wire,
            &RecordHead { i: index("h/new"), state: "changed".into(), headers: vec![("etag".into(), "\"2\"".into())] },
            Some(b"fresh body"),
        );
        push_record(&mut wire, &RecordHead { i: index("h/gone"), state: "gone".into(), headers: vec![] }, None);
        // The answer arrives in pieces; each record is applied as soon as it is whole.
        let headers = request(&[("host", "h"), SCRIPT]);
        let mut reader = BatchReader::default();
        let (first, rest) = wire.split_at(9);
        assert!(reader.feed(first).unwrap().is_empty(), "the first record is not whole yet");
        let mut outcomes = reader.feed(&rest[..rest.len() - 3]).unwrap();
        assert_eq!(outcomes.len(), 2, "fresh and changed are whole");
        for outcome in outcomes.drain(..) {
            cache.apply(&mut run, outcome);
        }
        assert!(matches!(cache.lookup("h/same", &headers), Lookup::Fresh(_)), "released before the answer ended");
        let Lookup::Fresh(new) = cache.lookup("h/new", &headers) else { panic!("the changed body is kept") };
        assert_eq!((&new.body[..], new.etag.as_deref()), (&b"fresh body"[..], Some("\"2\"")));
        assert!(!reader.finished());
        for outcome in reader.feed(&rest[rest.len() - 3..]).unwrap() {
            cache.apply(&mut run, outcome);
        }
        assert!(reader.finished());
        cache.end_batch(run, true);
        assert!(matches!(cache.lookup("h/gone", &headers), Lookup::Miss));
        assert!(matches!(cache.lookup("h/silent", &headers), Lookup::Miss), "an entry the answer left out is dropped");
        assert!(matches!(cache.lookup("other/x", &request(&[("host", "other")])), Lookup::Stale(_)), "another host's entry is left alone");
        assert!(parse_batch(&[0, 0, 0, 9, b'{']).is_none(), "a cut answer is refused");
        assert!(BatchReader::default().feed(&[0xff, 0xff, 0xff, 0xff]).is_none(), "an impossible length");
    }

    #[test]
    fn a_page_request_waits_only_for_its_own_record() {
        tokio::runtime::Builder::new_current_thread().enable_time().build().unwrap().block_on(async {
            let cache = Arc::new(Cache::default());
            for name in ["/a", "/b"] {
                keep(&cache, &format!("h{name}"), &[("host", "h"), SCRIPT], &[("etag", "\"1\"")], name);
            }
            let mut run = cache.begin_batch("h").unwrap();
            let index = |key: &str| run.entries.iter().position(|entry| entry.key == key).unwrap();
            let (a, b) = (index("h/a"), index("h/b"));
            let waiting = {
                let cache = cache.clone();
                tokio::spawn(async move { cache.wait_for_batch("h/a").await })
            };
            let other = {
                let cache = cache.clone();
                tokio::spawn(async move { cache.wait_for_batch("h/b").await })
            };
            tokio::task::yield_now().await;
            cache.apply(&mut run, Outcome { index: a, state: Answer::Fresh });
            tokio::time::timeout(Duration::from_secs(2), waiting).await.expect("released by its record").unwrap();
            assert!(!other.is_finished(), "the other request still waits");
            // A batch that times out keeps what arrived and lets the rest ask on their own.
            cache.end_batch(run, false);
            tokio::time::timeout(Duration::from_secs(2), other).await.expect("released when the batch ends").unwrap();
            let headers = request(&[("host", "h"), SCRIPT]);
            assert!(matches!(cache.lookup("h/a", &headers), Lookup::Fresh(_)));
            assert!(matches!(cache.lookup("h/b", &headers), Lookup::Stale(_)), "kept, asked about on its own");
            let _ = b;
        });
    }

    #[test]
    fn a_batch_carries_what_the_link_can_bring_in_a_couple_of_seconds() {
        assert_eq!(batch_budget(None), MAX_BATCH_BODIES);
        assert_eq!(batch_budget(Some(86_000)), MIN_BATCH_BODIES, "86 KB/s");
        assert_eq!(batch_budget(Some(1_000_000)), 2_000_000);
        assert_eq!(batch_budget(Some(50_000_000)), MAX_BATCH_BODIES);
    }

    #[test]
    fn a_page_kept_for_one_session_is_never_answered_to_another() {
        let cache = Cache::default();
        let signed_in = [("host", "h"), ("cookie", "session=alice")];
        keep(&cache, "h/dashboard", &signed_in, &[("etag", "W/\"1\"")], "alice's dashboard");
        assert!(kept(cache.lookup("h/dashboard", &request(&signed_in))));
        assert!(matches!(cache.lookup("h/dashboard", &request(&[("host", "h")])), Lookup::Miss), "signed out");
        assert!(matches!(cache.lookup("h/dashboard", &request(&[("host", "h"), ("cookie", "session=bob")])), Lookup::Miss));
        // Kept signed out, never answered to a browser that signed in meanwhile.
        keep(&cache, "h/home", &[("host", "h")], &[("etag", "W/\"1\""), ("cache-control", "max-age=60")], "signed-out home");
        assert!(matches!(cache.lookup("h/home", &request(&[("host", "h")])), Lookup::Fresh(_)));
        assert!(matches!(cache.lookup("h/home", &request(&signed_in)), Lookup::Miss), "signed in");
    }

    #[test]
    fn a_hot_update_during_a_slow_answer_leaves_that_answer_stale() {
        let cache = Cache::default();
        let sent = Instant::now();
        std::thread::sleep(Duration::from_millis(5));
        cache.invalidate();
        let parts = parts(200, &[("etag", "\"1\""), ("cache-control", "max-age=600")]);
        let vary = storable(&parts).unwrap();
        cache.store("h/a.js".into(), &HeaderMap::new(), parts, vary, Bytes::from_static(b"old"), sent);
        assert!(matches!(cache.lookup("h/a.js", &HeaderMap::new()), Lookup::Stale(_)), "the body is from before the edit");
    }

    #[test]
    fn a_budget_is_taken_whole_or_not_at_all() {
        let left = AtomicUsize::new(10);
        assert!(take_from(&left, 6));
        assert!(!take_from(&left, 6), "only 4 left");
        assert!(take_from(&left, 4));
        assert_eq!(left.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn page_loads_are_recognized() {
        let get = hyper::Method::GET;
        assert!(is_navigation(&get, &request(&[("sec-fetch-mode", "navigate")])));
        assert!(!is_navigation(&get, &request(&[("sec-fetch-mode", "cors"), ("accept", "text/html")])));
        assert!(is_navigation(&get, &request(&[("sec-fetch-dest", "iframe")])));
        assert!(is_navigation(&get, &request(&[("accept", "text/html,application/xhtml+xml")])));
        assert!(!is_navigation(&get, &request(&[("accept", "*/*")])));
        assert!(!is_navigation(&hyper::Method::POST, &request(&[("sec-fetch-mode", "navigate")])));
    }
}
