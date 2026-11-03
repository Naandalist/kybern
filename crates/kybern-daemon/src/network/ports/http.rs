//! The viewing side of a forward that parses HTTP (ADE-126): the plain `http://localhost:<port>` listener, when the
//! browser speaks HTTP/1.x first, and the Fast listener ([`super::fast`]) both send each request over the forward's one
//! HTTP/2 connection to the target (mode `H`), instead of copying bytes on a socket per browser connection.
//!
//! That one place gives a forward what a raw socket cannot:
//! * the target compresses text as Fast mode always did (whole `200` bodies the browser accepts gzip for);
//! * this machine counts each response's bytes, so a slow page shows as slow ([`super::load`]);
//! * responses are kept and revalidated in one batch per page load ([`super::cache`]).
//!
//! A connection whose first bytes are not an HTTP/1.x request (a database, SSH), one where the server speaks first, a
//! target without ready sockets (`ports.direct.v2`) and the WebSocket transport all stay byte-for-byte TCP. WebSocket
//! upgrades (hot reload) get their own raw socket to the target.

use std::convert::Infallible;
use std::sync::Arc;
use std::time::Duration;

use http_body_util::BodyExt;
use hyper::body::{Bytes, Incoming};
use hyper::header::{HeaderMap, HeaderValue};
use hyper::{Request, Response, StatusCode};
use hyper_util::rt::{TokioIo, TokioTimer};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

use super::cache::{self, Lookup};
use super::direct::{self, MODE_RAW, ProxyBody, Route, status_response, strip_hop_headers};
use super::{Counted, Forward};
use crate::network::Network;

const TARGET: &str = "kybern::network::ports";
/// How long the plain listener waits for the browser's first bytes before it asks the target too (a server-first
/// protocol speaks then).
const SNIFF_WAIT: Duration = Duration::from_millis(250);
/// A batch may take this long before the requests it has not answered yet ask on their own.
const BATCH_TIMEOUT: Duration = Duration::from_secs(30);
/// What one batch answer may bring at most.
const MAX_BATCH_ANSWER: usize = 96 * 1024 * 1024;

/// What the first bytes of a connection say.
pub(super) enum Sniffed {
    /// An HTTP/1.x request: serve it here.
    Http,
    /// Anything else: copy bytes, on the upstream socket already opened when the server spoke first.
    Raw(Option<TcpStream>),
}

const METHODS: [&[u8]; 9] = [b"GET ", b"POST ", b"PUT ", b"DELETE ", b"HEAD ", b"OPTIONS ", b"PATCH ", b"TRACE ", b"PRI "];

/// `Some(true)` for the start of an HTTP/1.x request line (never `CONNECT`, never HTTP/2's preface), `Some(false)` for
/// anything else, `None` while too few bytes arrived to tell.
fn looks_http(bytes: &[u8]) -> Option<bool> {
    let mut undecided = false;
    for method in METHODS {
        if bytes.len() >= method.len() {
            if bytes.starts_with(method) {
                return Some(method != b"PRI ");
            }
        } else if method.starts_with(bytes) {
            undecided = true;
        }
    }
    if undecided { None } else { Some(false) }
}

/// Decide how to carry a browser connection. Waits for its first bytes; after [`SNIFF_WAIT`] it opens the raw socket to
/// the target as well, and whichever side speaks first decides.
pub(super) async fn sniff(network: &Network, route: &Route, socket: &TcpStream) -> Sniffed {
    let mut buffer = [0u8; 16];
    let decide = |read: std::io::Result<usize>, buffer: &[u8]| match read {
        Ok(0) | Err(_) => Some(false),
        Ok(read) => looks_http(&buffer[..read]),
    };
    let mut upstream: Option<TcpStream> = None;
    let started = std::time::Instant::now();
    loop {
        let read = match &upstream {
            None => match tokio::time::timeout(SNIFF_WAIT, socket.peek(&mut buffer)).await {
                Ok(read) => read,
                Err(_) => {
                    upstream = route.open(network, MODE_RAW).await;
                    if upstream.is_none() {
                        return Sniffed::Raw(None);
                    }
                    continue;
                }
            },
            Some(opened) => tokio::select! {
                read = socket.peek(&mut buffer) => read,
                _ = opened.readable() => return Sniffed::Raw(upstream),
            },
        };
        match decide(read, &buffer) {
            Some(true) => return Sniffed::Http,
            Some(false) => return Sniffed::Raw(upstream),
            // A few bytes of what could be a method: wait for the rest, briefly.
            None if started.elapsed() < Duration::from_secs(2) => tokio::time::sleep(Duration::from_millis(10)).await,
            None => return Sniffed::Raw(upstream),
        }
    }
}

/// Serve one browser connection of the plain listener as HTTP/1.1.
pub(super) async fn serve_plain(network: Arc<Network>, forward: Arc<Forward>, socket: TcpStream) {
    let service = {
        let (network, forward) = (network.clone(), forward.clone());
        hyper::service::service_fn(move |request: Request<Incoming>| {
            let (network, forward) = (network.clone(), forward.clone());
            async move { Ok::<_, Infallible>(exchange(&network, &forward, request, "http").await) }
        })
    };
    let mut builder = hyper::server::conn::http1::Builder::new();
    builder.timer(TokioTimer::new()).keep_alive(true).preserve_header_case(true);
    let connection = builder.serve_connection(TokioIo::new(socket), service).with_upgrades();
    tokio::select! {
        result = connection => if let Err(error) = result { tracing::debug!(target: TARGET, ?error, "a forwarded HTTP connection ended"); },
        _ = forward.cancel.cancelled() => {},
    }
}

fn is_upgrade(request: &Request<Incoming>) -> bool {
    request.version() <= hyper::Version::HTTP_11 && request.headers().contains_key(hyper::header::UPGRADE)
}

/// The host the browser asked for: the `:authority` of HTTP/2, the `Host` header of HTTP/1.1.
fn authority(request: &Request<Incoming>) -> Option<String> {
    request
        .uri()
        .authority()
        .map(|authority| authority.to_string())
        .or_else(|| request.headers().get(hyper::header::HOST).and_then(|host| host.to_str().ok()).map(str::to_string))
}

/// One request of a page, from the plain (`scheme` `http`) or the Fast (`https`) listener.
pub(super) async fn exchange(
    network: &Arc<Network>,
    forward: &Arc<Forward>,
    request: Request<Incoming>,
    scheme: &'static str,
) -> Response<ProxyBody> {
    // The route of the session open now: after the target restarts this prepares the forward again.
    let Some(route) = super::current_route(network, forward).await.filter(|route| route.ready_sockets()) else {
        return status_response(StatusCode::BAD_GATEWAY);
    };
    if is_upgrade(&request) {
        return upgrade(network, forward, &route, request).await;
    }
    let Some(host) = authority(&request) else { return status_response(StatusCode::BAD_REQUEST) };
    let (mut parts, body) = request.into_parts();
    let path = parts.uri.path_and_query().map(|pq| pq.as_str().to_string()).unwrap_or_else(|| "/".into());
    let Ok(uri) = format!("https://{host}{path}").parse::<hyper::Uri>() else { return status_response(StatusCode::BAD_REQUEST) };
    // A page never speaks for Kybern: its own `x-kybern-*` headers go no further.
    direct::take_kybern_headers(&mut parts.headers);
    strip_hop_headers(&mut parts.headers);
    parts.headers.remove(hyper::header::HOST);
    if scheme == "http" && !parts.headers.contains_key("x-forwarded-proto") {
        parts.headers.insert("x-forwarded-proto", HeaderValue::from_static("http"));
    }
    // What the browser sent, with its host: the cache's key and variants, and its conditional request.
    let mut browser = parts.headers.clone();
    if let Ok(value) = HeaderValue::from_str(&host) {
        browser.insert(hyper::header::HOST, value);
    }
    let navigation = cache::is_navigation(&parts.method, &browser);
    if navigation {
        // A new page: its load counts from here, and the old page's requests no longer keep a load open.
        forward.load.navigated();
    }
    let meter = Metering::begin(network, forward);
    let caching = forward.caches();
    if caching && navigation {
        start_batch(network, forward, &route, &host);
    }
    // A write may change what the server answers for anything of its host (RFC 9111 §4.4): before it leaves and again
    // when its answer arrives, so a read that crossed it is not kept as current either.
    let writes =
        caching && !matches!(parts.method, hyper::Method::GET | hyper::Method::HEAD | hyper::Method::OPTIONS | hyper::Method::TRACE);
    if writes {
        forward.cache.written(&host);
    }
    let key = caching.then(|| cache::request_key(&parts.method, &browser, &path)).flatten();
    let mut stale = None;
    let mut route = route;
    if let Some(key) = &key {
        forward.cache.wait_for_batch(key).await;
        // The wait may have been long enough for the route to change (a member's route taken, or given up).
        if let Some(now) = super::current_route(network, forward).await.filter(|route| route.ready_sockets()) {
            route = now;
        }
        match forward.cache.lookup(key, &browser) {
            Lookup::Fresh(entry) => {
                meter.cached(entry.body.len());
                return cache::respond(&entry, &browser);
            }
            Lookup::Stale(entry) => {
                parts.headers.remove(hyper::header::IF_NONE_MATCH);
                parts.headers.remove(hyper::header::IF_MODIFIED_SINCE);
                if let Some(tag) = entry.upstream_etag().and_then(|tag| HeaderValue::from_str(tag).ok()) {
                    parts.headers.insert(hyper::header::IF_NONE_MATCH, tag);
                }
                if let Some(at) = entry.last_modified().and_then(|at| HeaderValue::from_str(at).ok()) {
                    parts.headers.insert(hyper::header::IF_MODIFIED_SINCE, at);
                }
                stale = Some(entry);
            }
            Lookup::Miss => {}
        }
    }
    if caching {
        parts.headers.insert(cache::CACHE_HEADER, HeaderValue::from_static("1"));
    }
    parts.uri = uri;
    parts.version = hyper::Version::HTTP_2;
    let sent = std::time::Instant::now();
    let response = match send(network, forward, &route, Request::from_parts(parts, body.boxed())).await {
        Some(response) => response,
        None => return status_response(StatusCode::BAD_GATEWAY),
    };
    if writes {
        forward.cache.written(&host);
    }
    let (mut parts, body) = response.into_parts();
    strip_hop_headers(&mut parts.headers);
    if let Some(entry) = stale {
        if parts.status == StatusCode::NOT_MODIFIED {
            forward.cache.confirm(&entry, sent);
            meter.cached(entry.body.len());
            return cache::respond(&entry, &browser);
        }
        forward.cache.remove(&entry);
    }
    let tee = key.and_then(|key| {
        let vary = cache::storable(&parts)?;
        Some(Tee { key, request: browser, status: parts.status, headers: parts.headers.clone(), vary, buffer: Vec::new(), sent })
    });
    // Kybern's own header stays here; the browser sees the response as the dev server's.
    parts.headers.remove(cache::ORIGINAL_ETAG_HEADER);
    // An event stream (or any response that never ends) is not part of a page load.
    let streaming = parts
        .headers
        .get(hyper::header::CONTENT_TYPE)
        .and_then(|kind| kind.to_str().ok())
        .is_some_and(|kind| kind.to_ascii_lowercase().starts_with("text/event-stream"));
    let meter = (!streaming).then_some(meter);
    Response::from_parts(parts, Tracked { inner: body.boxed(), meter, tee, forward: forward.clone() }.boxed())
}

/// Send a request over the forward's HTTP/2 connection, opening a fresh connection once when the held one is gone.
async fn send(
    network: &Arc<Network>,
    forward: &Arc<Forward>,
    route: &Arc<Route>,
    request: Request<ProxyBody>,
) -> Option<Response<Incoming>> {
    let mut sender = match forward.tunnel.sender(network, route, &forward.cancel).await {
        Some(sender) => sender,
        // A member's route that stopped answering: back to the direct one.
        None if route.hop.is_some() => {
            let direct = super::fall_back(network, forward, route)?;
            forward.tunnel.sender(network, &direct, &forward.cancel).await?
        }
        None => return None,
    };
    if sender.ready().await.is_err() {
        sender = forward.tunnel.sender(network, route, &forward.cancel).await?;
    }
    match sender.send_request(request).await {
        Ok(response) => Some(response),
        Err(error) => {
            tracing::debug!(target: TARGET, ?error, "a forwarded request failed");
            None
        }
    }
}

/// Revalidate everything the cache keeps for `host`, in one request. Each record of the answer is applied as it arrives,
/// letting the page's requests that wait for that entry go on; a timeout keeps what already arrived.
fn start_batch(network: &Arc<Network>, forward: &Arc<Forward>, route: &Arc<Route>, host: &str) {
    let Some(mut run) = forward.cache.begin_batch(host) else { return };
    let (network, forward, route, host) = (network.clone(), forward.clone(), route.clone(), host.to_string());
    tokio::spawn(async move {
        let started = std::time::Instant::now();
        let meter = Metering::begin(&network, &forward);
        let budget = cache::batch_budget(forward.load.last_rate());
        let mut answered = 0usize;
        let read = async {
            let request = cache::batch_request(&run.entries, &host, budget)?;
            let response = send(&network, &forward, &route, request).await?;
            if response.status() != StatusCode::OK {
                return None;
            }
            let mut body = response.into_body();
            let mut reader = cache::BatchReader::default();
            let mut size = 0usize;
            while let Some(frame) = body.frame().await {
                let Ok(data) = frame.ok()?.into_data() else { continue };
                size += data.len();
                if size > MAX_BATCH_ANSWER {
                    return None;
                }
                meter.received(data.len());
                for outcome in reader.feed(&data)? {
                    answered += 1;
                    forward.cache.apply(&mut run, outcome);
                }
            }
            Some(reader.finished())
        };
        let complete = tokio::time::timeout(BATCH_TIMEOUT, read).await.ok().flatten().unwrap_or(false);
        let entries = run.entries.len();
        forward.cache.end_batch(run, complete);
        tracing::info!(target: TARGET, machine = %forward.machine_id, entries, answered, complete, budget, ms = started.elapsed().as_millis() as u64, "revalidated a forward's cache in one batch");
    });
}

/// A request counted in the forward's load, ended when dropped.
struct Metering {
    forward: Arc<Forward>,
    id: u64,
}

impl Metering {
    fn begin(network: &Arc<Network>, forward: &Arc<Forward>) -> Self {
        let id = forward.load.begin();
        super::watch_load(network, forward);
        Self { forward: forward.clone(), id }
    }

    fn received(&self, bytes: usize) {
        self.forward.load.received(self.id, bytes);
    }

    fn cached(&self, bytes: usize) {
        self.forward.load.cached(bytes);
    }
}

impl Drop for Metering {
    fn drop(&mut self) {
        self.forward.load.end(self.id);
    }
}

/// Room left in what all forwards may hold at once for responses still arriving that the cache will keep (128 MiB).
static BUFFER_ROOM: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(128 * 1024 * 1024);

/// A response being kept in the cache as it streams.
struct Tee {
    key: String,
    request: HeaderMap,
    status: StatusCode,
    headers: HeaderMap,
    vary: Vec<String>,
    buffer: Vec<u8>,
    sent: std::time::Instant,
}

impl Tee {
    /// Add a piece, within [`cache::MAX_ENTRY`] and [`BUFFER_ROOM`]; false when it no longer fits.
    fn push(&mut self, data: &[u8]) -> bool {
        if self.buffer.len() + data.len() > cache::MAX_ENTRY || !cache::take_from(&BUFFER_ROOM, data.len()) {
            return false;
        }
        self.buffer.extend_from_slice(data);
        true
    }
}

impl Drop for Tee {
    fn drop(&mut self) {
        BUFFER_ROOM.fetch_add(self.buffer.len(), std::sync::atomic::Ordering::AcqRel);
    }
}

/// A response body on its way to the browser: counted, and kept when it may be.
struct Tracked {
    inner: ProxyBody,
    meter: Option<Metering>,
    tee: Option<Tee>,
    forward: Arc<Forward>,
}

impl Tracked {
    /// The whole body went by: keep it, and end the request's part in the load.
    fn finish(&mut self) {
        if let Some(mut tee) = self.tee.take() {
            let (mut parts, ()) = Response::new(()).into_parts();
            parts.status = tee.status;
            parts.headers = std::mem::take(&mut tee.headers);
            let body = Bytes::from(std::mem::take(&mut tee.buffer));
            BUFFER_ROOM.fetch_add(body.len(), std::sync::atomic::Ordering::AcqRel);
            let (key, vary) = (std::mem::take(&mut tee.key), std::mem::take(&mut tee.vary));
            self.forward.cache.store(key, &tee.request, parts, vary, body, tee.sent);
        }
        self.meter = None;
    }
}

impl hyper::body::Body for Tracked {
    type Data = Bytes;
    type Error = hyper::Error;

    fn poll_frame(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Result<hyper::body::Frame<Bytes>, hyper::Error>>> {
        use std::task::Poll;
        let polled = std::pin::Pin::new(&mut self.inner).poll_frame(cx);
        match &polled {
            Poll::Ready(Some(Ok(frame))) => {
                if let Some(data) = frame.data_ref() {
                    let len = data.len();
                    if let Some(meter) = &self.meter {
                        meter.received(len);
                    }
                    if self.tee.as_mut().is_some_and(|tee| !tee.push(data)) {
                        self.tee = None;
                    }
                }
            }
            Poll::Ready(Some(Err(_))) => {
                self.tee = None;
                self.meter = None;
            }
            Poll::Ready(None) => self.finish(),
            Poll::Pending => {}
        }
        // The server stops polling once the body says it has ended, so the last frame may be the last call.
        if matches!(polled, Poll::Ready(Some(Ok(_)))) && self.inner.is_end_stream() {
            self.finish();
        }
        polled
    }

    fn is_end_stream(&self) -> bool {
        self.inner.is_end_stream()
    }

    fn size_hint(&self) -> hyper::body::SizeHint {
        self.inner.size_hint()
    }
}

/// A WebSocket (or other HTTP/1.1 upgrade): its own raw socket to the target, the request written as HTTP/1.1, then the
/// two upgraded connections joined. A message from the dev server on it (hot reload) makes the forward's cache stale.
async fn upgrade(
    network: &Arc<Network>,
    forward: &Arc<Forward>,
    route: &Arc<Route>,
    mut request: Request<Incoming>,
) -> Response<ProxyBody> {
    let Some(socket) = route.open(network, MODE_RAW).await else { return status_response(StatusCode::BAD_GATEWAY) };
    let Ok((mut sender, connection)) = hyper::client::conn::http1::handshake::<_, http_body_util::Empty<Bytes>>(TokioIo::new(socket)).await
    else {
        return status_response(StatusCode::BAD_GATEWAY);
    };
    tokio::spawn(connection.with_upgrades());
    let mut outbound = Request::builder()
        .method(request.method().clone())
        .uri(request.uri().path_and_query().map(|pq| pq.as_str()).unwrap_or("/"))
        .version(hyper::Version::HTTP_11);
    for (name, value) in request.headers() {
        if name != hyper::header::PROXY_AUTHORIZATION && !name.as_str().starts_with("x-kybern-") {
            outbound = outbound.header(name, value);
        }
    }
    let Ok(outbound) = outbound.body(http_body_util::Empty::new()) else { return status_response(StatusCode::BAD_REQUEST) };
    let browser_side = hyper::upgrade::on(&mut request);
    let mut response = match sender.send_request(outbound).await {
        Ok(response) => response,
        Err(_) => return status_response(StatusCode::BAD_GATEWAY),
    };
    if response.status() == StatusCode::SWITCHING_PROTOCOLS {
        let target_side = hyper::upgrade::on(&mut response);
        let watched = forward.caches().then(|| forward.clone());
        let cancel = forward.cancel.clone();
        let count = Counted::new(forward);
        tokio::spawn(async move {
            let _count = count;
            let (Ok(browser), Ok(target)) = tokio::join!(browser_side, target_side) else { return };
            let (browser, target) = (TokioIo::new(browser), TokioIo::new(target));
            tokio::select! {
                _ = join_watching(browser, target, watched) => {},
                _ = cancel.cancelled() => {},
            }
        });
    }
    let (parts, body) = response.into_parts();
    Response::from_parts(parts, body.boxed())
}

/// Join an upgraded connection, reading the dev server's WebSocket frames on the way: any data message after the first
/// (Vite's `connected`) is a hot-reload event, and the forward's cache must ask again before it answers.
async fn join_watching<B, T>(browser: B, target: T, watched: Option<Arc<Forward>>)
where
    B: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
    T: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    let (mut browser_read, mut browser_write) = tokio::io::split(browser);
    let (mut target_read, mut target_write) = tokio::io::split(target);
    let up = async {
        let _ = tokio::io::copy(&mut browser_read, &mut target_write).await;
        let _ = target_write.shutdown().await;
    };
    let down = async {
        let mut frames = FrameWatch::default();
        let mut buffer = vec![0u8; 16 * 1024];
        loop {
            let read = match target_read.read(&mut buffer).await {
                Ok(0) | Err(_) => break,
                Ok(read) => read,
            };
            if let Some(forward) = &watched
                && frames.feed(&buffer[..read])
            {
                forward.cache.invalidate();
            }
            if browser_write.write_all(&buffer[..read]).await.is_err() {
                break;
            }
        }
        let _ = browser_write.shutdown().await;
    };
    tokio::join!(up, down);
}

/// Reads server-to-client WebSocket frame headers (unmasked) as bytes go by, to notice data messages.
#[derive(Default)]
struct FrameWatch {
    header: Vec<u8>,
    /// Payload bytes of the current frame still to skip.
    skip: u64,
    messages: u32,
}

impl FrameWatch {
    /// Feed bytes; true when a data message other than the first started in them.
    fn feed(&mut self, mut bytes: &[u8]) -> bool {
        let mut event = false;
        while !bytes.is_empty() {
            if self.skip > 0 {
                let take = (self.skip.min(bytes.len() as u64)) as usize;
                self.skip -= take as u64;
                bytes = &bytes[take..];
                continue;
            }
            self.header.push(bytes[0]);
            bytes = &bytes[1..];
            let Some((length, header_len)) = Self::parse(&self.header) else {
                if self.header.len() > 14 {
                    // Not frames as expected: treat what follows as events.
                    self.header.clear();
                    return true;
                }
                continue;
            };
            if self.header.len() < header_len {
                continue;
            }
            let opcode = self.header[0] & 0x0f;
            self.header.clear();
            self.skip = length;
            // Text, binary and continuation frames carry data; 0x8-0xA are control frames.
            if opcode < 0x8 && opcode != 0x0 {
                self.messages += 1;
                if self.messages > 1 {
                    event = true;
                }
            }
        }
        event
    }

    /// The payload length and full header length once enough header bytes are known.
    fn parse(header: &[u8]) -> Option<(u64, usize)> {
        let second = *header.get(1)?;
        let masked = second & 0x80 != 0;
        let mask = if masked { 4 } else { 0 };
        match second & 0x7f {
            126 => {
                let bytes = header.get(2..4)?;
                Some((u16::from_be_bytes([bytes[0], bytes[1]]) as u64, 4 + mask))
            }
            127 => {
                let bytes = header.get(2..10)?;
                Some((u64::from_be_bytes(bytes.try_into().ok()?), 10 + mask))
            }
            short => Some((short as u64, 2 + mask)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_an_http1_request_line_is_served_as_http() {
        assert_eq!(looks_http(b"GET / HTTP/1.1\r\n"), Some(true));
        assert_eq!(looks_http(b"OPTIONS * HTTP/1.1"), Some(true));
        assert_eq!(looks_http(b"GE"), None, "too short to tell");
        assert_eq!(looks_http(b"PRI * HTTP/2.0"), Some(false), "HTTP/2 prior knowledge stays raw");
        assert_eq!(looks_http(b"CONNECT a:1 HTTP/1.1"), Some(false));
        assert_eq!(looks_http(b"SSH-2.0-OpenSSH"), Some(false));
        assert_eq!(looks_http(&[0x16, 0x03, 0x01]), Some(false), "a TLS hello");
        assert_eq!(looks_http(&[0, 0, 0, 8, 4, 210, 22, 47]), Some(false), "a Postgres startup");
    }

    fn frame(opcode: u8, payload: &[u8]) -> Vec<u8> {
        let mut out = vec![0x80 | opcode];
        match payload.len() {
            n if n < 126 => out.push(n as u8),
            n if n < 65536 => {
                out.push(126);
                out.extend_from_slice(&(n as u16).to_be_bytes());
            }
            n => {
                out.push(127);
                out.extend_from_slice(&(n as u64).to_be_bytes());
            }
        }
        out.extend_from_slice(payload);
        out
    }

    #[test]
    fn a_hot_reload_message_is_noticed_after_the_greeting_whatever_the_chunking() {
        let mut watch = FrameWatch::default();
        assert!(!watch.feed(&frame(0x1, br#"{"type":"connected"}"#)), "the greeting is not an event");
        assert!(!watch.feed(&frame(0x9, b"")), "a ping is not");
        let update = frame(0x1, &[b'x'; 70_000]);
        let mut seen = false;
        for piece in update.chunks(7) {
            seen |= watch.feed(piece);
        }
        assert!(seen, "a large update split across reads");
        assert!(watch.feed(&frame(0x2, b"bin")));
    }
}
