# Machine network transports

Peer sessions keep machine identity, signed membership, short-lived grants and per-machine
policy on one authenticated WebSocket. Port forwarding additionally uses a separate TCP path
when both peers support `ports.direct.v1` and have a direct Tailscale route.

`ports.forward` binds IPv4 loopback locally. With no explicit local port it tries the remote
port and then each higher port. The returned `local_port` is the address the Browser uses.
`--local-port 0` still requests an ephemeral port. No test connection or HTTP page probe runs
before opening a forwarded page. Proxy tickets recognize a bound forward, and each proxied HTTP
request or WebSocket upgrade connects to the upstream once instead of probing and discarding a socket.

`peer.ports.prepare` authorizes the destination without connecting to it. The serving daemon
binds an ephemeral listener exclusively to its Tailscale IP and returns its port and a random
256-bit token. The client derives the IP from the member's recorded Tailscale node. Each TCP
connection sends 64 token bytes, then waits for one success byte before application traffic.
The receiver verifies the token hash, source node with Tailscale whois, live member/session/grant
and current `machine:ports` permission, then connects only to its own IPv4/IPv6 loopback.
Daemon API ports are refused. Direct data listeners bind only to Tailscale, so loopback
forwards cannot reach them (and their port numbers can coexist with unrelated loopback services).

Application bytes use independent TCP sockets and bidirectional copy, preserving upgrades,
half-closes, TLS and non-HTTP protocols. Listener and connection counts are bounded. Stop,
shutdown, membership or policy revocation, session closure and the existing idle timeout
cancel listeners and active sockets. Different clients get independent listeners and tokens.
`peer.ports.release` is scoped to the caller's own forward id, including after ports permission
has been revoked.

Peers lacking the capability or new method, off-tailnet/code-only peers, unavailable Tailscale
listeners and failed direct connections use the existing `peer.ports.open/data/close` transport.
The old stream framing, credit window and authorization are unchanged. No protocol-version
bump is needed. With older peers, destination refusals arrive on the first real connection.

## Reproduce the local benchmark

Use a checkout-local target and scratch data only. The fixture starts real daemon states and
peer sessions on a fake tailnet mapped to loopback; it does not start agent CLIs or use normal
Kybern data. It creates a direct TCP forward and a second forward with a simulated 0.20.x
presence to exercise the original WebSocket transport. `forwards.json` records all three ports
and the elapsed time for opening the direct forward.

```sh
node scripts/bench-port-forward.mjs /tmp/kybern-port-benchmark
pnpm --dir /tmp/kybern-port-benchmark --store-dir /tmp/kybern-port-benchmark/.store install --ignore-scripts
node /tmp/kybern-port-benchmark/server.mjs
```

In another terminal, from the repository root:

```sh
CARGO_TARGET_DIR="$PWD/target" CARGO_INCREMENTAL=0 \
KYBERN_PORT_BENCHMARK_ROOT=/tmp/kybern-port-benchmark \
cargo test -p kybern-daemon port_forward_benchmark_fixture -- --ignored --nocapture
```

After `forwards.json` appears, run in Ego Lite (the ego-browser skill documents its setup):

```sh
ego-browser nodejs -e "await (await import('file://$PWD/scripts/bench-port-forward-browser.mjs')).benchmark('/tmp/kybern-port-benchmark')"
```

The page imports 300 JavaScript modules with 2 KiB payloads. Measurements are medians of
12 cache-disabled browser navigations after a warmup per route, with route order rotated each
round, and 30 HMR updates per route at 100 ms spacing. Vite transformations are warm. HMR
measures the browser's POST that changes a watched source file, the WebSocket update, module
fetch and evaluation. `browser-results.json` retains samples; the fixture exits when `done`
appears. Pass `true` as the second benchmark argument to invalidate Vite's transform cache
before each measured navigation. This excludes Vite process startup and server configuration.

## Measure across machines

Do not compile on a serving machine low on disk. Install the candidate daemon built elsewhere,
and use scratch daemon data on both machines. Join the scratch pair with `machines code` and
`machines join <code> <address>`; enable ports permission on the server. Keep production data
and daemons running normally.

Copy only `scripts/bench-port-forward.mjs` to the serving machine. Reuse its installed Vite
dependencies when disk is below the project budget; install the scratch dependency only after
freeing enough space. Otherwise run the first three commands above there. Vite binds `127.0.0.1:58173`; it needs no `--host` option. On the client:

```sh
kybern --data-dir /absolute/client-scratch --json ports forward instabox 58173
```

Use the returned port in `http://localhost:<local_port>/`. For a direct reference to the same
loopback-only server, use an independent SSH forward to the server (or compare with a separate
Vite instance bound to its Tailscale IP if testing native Tailscale TCP). For the old-transport
baseline, repeat with an otherwise identical scratch 0.20.x serving daemon. Stop the existing
forward before switching serving daemons. Record RTT, direct-vs-DERP Tailscale route, CPU,
server/browser versions, cache boundary, sample counts, first-load and HMR medians and tails.
The raw loopback benchmark does not establish latency or throughput across a real tailnet.

Clean scratch apps, installs, daemon data and task build output after preserving results.

## Fast forwards (ADE-116)

What makes a forwarded preview slow is round trips, not bytes, until the page is large: a browser
opens at most six HTTP/1.1 connections to an origin and sends one request at a time on each, so a
Vite page of a few hundred modules waits one round trip to the serving machine for every six.
Measured on a 361-module Vite app (Chrome, 2026-11-03): 16.3 s cold over a 213 ms DERP route and
2.1 s over 26 ms, against 0.28 s locally.

- **Ready sockets** (`ports.direct.v2`, `ports/direct.rs`): the target answers the token as soon as
  the caller is authorized and the socket waits for a mode byte, so the caller keeps four
  authorized sockets per forward (`Route::keep_warm`, refreshed every 45 s while the forward was
  used in the last 10 minutes). A browser connection starts without the two-round-trip handshake.
  A forward prepares itself again when the session it was prepared on ends (the target restarted):
  the target's per-forward listener lives with that session.
- **Fast mode** (`ports/fast.rs`, `PortsForwardParams::https`): the forward also listens on
  `https://localhost:<https_port>` with HTTP/2 and Kybern's per-install certificate
  (`<data dir>/network/preview-tls/`, `ports/tls.rs`). Every request of the page travels over one
  HTTP/2 connection to the target (mode `H`), which sends them on to the dev server over pooled
  HTTP/1.1 with the browser's `Host`, and gzips uncompressed text when the browser accepts it
  (whole `200` bodies only: never a `206`, a `Content-Range` or `Cache-Control: no-transform`; a
  strong `ETag` becomes weak). Compressing responses that mix secrets with reflected input is what
  BREACH exploits; the risk is low here, since only the user's own dev server is served this way,
  inside WireGuard, to a web view on the user's machine. WebSocket upgrades (hot reload) use HTTP/1.1 on the same origin, each on a raw ready socket.
  Kybern's web views accept exactly that certificate and no other. Which origins and documents
  accept it depends on the platform:
  - macOS: WKWebView checks the leaf's SHA-256 in each server-trust challenge against the
    preview's own origin (scheme, host and port) (`native/webview.m`).
  - Windows: WebView2 checks the PEM and the exact origin in `ServerCertificateErrorDetected`
    (`webview-win/src/policy.rs`). `ALWAYS_ALLOW` is the only way to accept it, and WebView2 then
    remembers host and certificate, not the port. That memory lives in the document's own InPrivate
    profile, so it can't reach other documents.
  - Linux: WPE accepts a certificate for a host name, not a port
    (`webkit_network_session_allow_tls_certificate_for_host`, `webview-linux.c`). Only a pinned
    Fast preview adds it, for its own origin's host only (`localhost` or `127.0.0.1`). After that,
    every web view of the app accepts that certificate on that host, on any port, until the app
    restarts. That includes other previews, visuals and the renderer. A separate ephemeral network
    session per preview does not change this, because the exception lives in the shared WPE
    network process (the `webview` on-screen check showed it). Making it exact would need a WPE
    patch.

  On Windows the preview's own page, and on Linux any document, could also reach another loopback
  port that presents the same certificate. Only Kybern's own Fast forwards hold its private key,
  which is kept in the data folder, so what that exposes is other Kybern forwards on the same
  machine. The certificate is never added to a system trust store, so any other browser refuses it.
- The desktop turns Fast mode on when the forward's measured round trip (`rtt_ms`) is above 40 ms,
  and remembers a choice made with the **Fast** chip in the address bar or **Fast loading
  (HTTP/2)** in the page menu (`apps/gpuix/src/preview/fast.ts`).
- **Known risk:** the page's origin becomes `https://localhost:<port>`. A page that hard-codes
  `http://localhost:3000` for its API, an OAuth redirect URI or a cookie scoped to the HTTP origin
  sees mixed content or a mismatch. One click on the chip goes back to plain HTTP.
- Between two current machines, relay and port streams over the session send binary frames and
  use a window of up to 4 MiB (`relay.frames.v2`); limits queue for up to 10 s before refusing, and
  the listeners and session sockets set `TCP_NODELAY`. The windows of one session share 16 MiB
  (`LINK_WINDOW_BUDGET`): a few streams get 4 MiB each, more get less, never under 256 KiB. Stream
  data and closes wait in the session's own 4 MiB data lane (`peers::BulkLane`) for as long as a
  slow route needs, and the socket writer sends control frames (requests, answers, credit) first,
  so a backlog of data never trips the 30 s send limit that ends a session or delays an RPC.

Measure with real browsers: the fixtures kept with the ADE-116 report (`.kybern-artifacts/ade-116/`)
drive headless Chrome over CDP (`--ignore-certificate-errors-spki-list` pins the same leaf) and
read WKWebView timings from a beacon the test app posts. A dev server's own speed matters: a
2-core VPS serves the 365 requests of that page in 0.8 s on its own loopback.

## Slow links (ADE-126)

Measured 2026-11-03: a TanStack Start page in Vite dev mode on instabox makes 212 requests for about 31 MB of
unminified JavaScript (one prebundled icon package is 21.4 MB) and hides its content until it hydrates. The Mac in Bali
reached instabox over a direct Tailscale path at 86 KB/s, so the page showed only its footer for minutes, while
instabox → kdi-vps ran at 778 KB/s and kdi-vps → the Mac at about 2.5 MB/s.

- **HTTP on the plain listener** (`ports/http.rs`): on a `ports.direct.v2` route, a browser connection whose first bytes
  are an HTTP/1.x request line is served by hyper here, and each request goes over the forward's one HTTP/2 connection
  to the target (mode `H`, as in Fast mode). Anything else stays a byte-for-byte socket: a connection whose first bytes
  are not a request line (TLS, SSH, Postgres, HTTP/2 prior knowledge, `CONNECT`), and one where the server speaks first.
  When the browser has sent nothing for 250 ms the raw socket to the target is opened too, and whichever side speaks
  first decides. WebSocket upgrades get their own raw socket. The plain listener sends `X-Forwarded-Proto: http`.
- **Compression everywhere Kybern parses HTTP**: the target gzips text for the plain listener as it did for Fast mode
  (whole `200` bodies only, never with `Content-Range` or `no-transform`, a strong `ETag` weakened, `Accept-Encoding`
  q-values honoured, images and other binary types left alone). The WebSocket stream transport (`peer.ports.open`) and
  raw sockets carry bytes Kybern does not parse; compressing them would need the stream to be parsed on both ends (or a
  general-purpose stream compressor, which gains little on already-compressed traffic and costs CPU on every byte), so
  they stay as they are.
- **Load progress** (`ports/load.rs`): each response's bytes are counted as they arrive. A load starts with each
  navigation (what the old page still has open drops out) or the first request after 1.5 s without one, and ends 1.5 s
  after the last. A request open 5 s with no bytes for as long (a long poll) or open 20 s at under 512 B/s (a stream of
  events) is left out for good. `PortForward.loading` carries the load (received and cached bytes, the previous load's
  total as an estimate, the rate over 3 s) only while one of its requests has been open 2 s and still counts, so a fast
  link or quick polling shows nothing. `ports.changed` repeats it at most every 3 s, after 2 s, and only while it looks
  slow: under 500 KB/s, more than 10 s left by the estimate, or 10 s gone without one (`load::looks_slow`). The Browser
  applies the same rule and shows "Loading over a slow connection — 4.2 of 31 MB · 90 KB/s" under its chrome after 3 s
  with bytes arriving (`apps/gpuix/src/preview/slow.ts`); only the first words are a live region.
- **Viewing-side cache** (`ports/cache.rs`, `ports.cache.v1`): whole `200` answers to plain `GET`s with an `ETag` or
  `Last-Modified` are kept per forward, keyed by host, path and the request headers they `Vary` on. A request's
  `Cookie` always counts as a variant, an absent one included, so a page kept for one session (or for none) is never
  answered to another. Never kept: with `Set-Cookie`, `private`, `no-store`, `Vary: *`, a range, `Authorization`, or an
  event stream. A navigation request starts one batched revalidation of that host's entries (`x-kybern-revalidate` on
  the HTTP/2 connection): the target asks its dev server about each over loopback, 16 at a time with 5 s each, and
  streams back fresh, changed (a new body of at most 512 KiB, compressed) or gone (a larger change is fetched by the
  browser as usual), each record as its check ends and fresh or gone ones ahead of changed bodies. The changed bytes
  one batch carries are what the forward's last load moved in 2 s, between 256 KiB and 4 MiB. Each of the page's
  requests waits only for its own record; a batch that times out keeps what arrived. A kept response is answered
  locally only when this page load's batch confirmed it (within 2 minutes) and the request is a subresource
  (`Sec-Fetch-Dest` script, style, image, font or worker; never a `fetch` or XHR), or when its own `max-age` still
  covers it and the request does not ask for a checked answer. `no-cache`, `max-age=0` and `must-revalidate` answers
  are revalidated every time outside a batch. Validation counts from when the request left. A data message on the
  forward's WebSocket after the first (Vite's `connected`) marks everything stale, and a request other than `GET`,
  `HEAD`, `OPTIONS` or `TRACE` marks everything of its host stale, before it leaves and when its answer arrives. Bounds: 32 MiB per response, 96 MiB per
  forward, 256 MiB for all and 128 MiB being buffered, least recently used first out; in memory only, gone with the
  forward. The target keeps the dev server's own tag beside a weakened one (`x-kybern-etag`, only for a viewing
  machine that sent `x-kybern-cache`), and strips every `x-kybern-*` request header before the dev server; the viewing
  side strips a page's own.
- **Routes through a member** (`ports/hop.rs`, `ports.route.v1`). The viewing machine V names a member M at the
  target T (`peer.ports.via`); T answers with a token for that route only, which works only from M's node and only
  while M is a current, unpaused member with a session that may use T's ports itself, so the forward's own token never
  passes through M. Dropping M, or M losing any of that, ends every connection T let in through it, ready ones too.
  V then asks M to relay (`peer.ports.hop`, needs `machine:relay`); M resolves T from membership and asks T to confirm
  that the port is the listener of V's forward and that V named M (`peer.ports.hop_check`), so a hop never reaches any
  other port of T. M checks its own token and V's node on each connection.
  M sees the route token pass, so V does not rely on it to know who answers: after the token V sends a 32-byte nonce,
  and T answers with an Ed25519 signature by its membership key over the nonce, the forward id, its listener port and
  the three machine ids. V checks it against T's key in membership before it sends any page bytes, so M can neither
  answer in T's place nor replay an answer. The bytes are not encrypted end to end beyond WireGuard on each leg, so M
  could still read or change them: M is a member of the user's own network granted `machine:relay`. TLS pinned to T
  would close that at the cost of a TLS layer on every relayed socket; it is not done.
  V measures routes only when a load looks slow or after a load ends, never while a fast page loads. It probes the
  direct route first (mode `P`: the target sends at most 2 MiB of filler for at most 5 s, the caller reads for 2.5 s);
  at 1 MB/s or more no member is asked. Otherwise it probes up to three candidates, twice, one at a time; the
  page's own bytes during the direct probe count toward the direct route. A member is taken only when every round is
  at least twice as fast and above 64 KB/s. New connections and the next HTTP/2 connection use it; open ones finish
  where they are. Candidates have a session open, list `ports.route.v1` and are not paused; the target must list it
  too. Unchosen routes are released at M and dropped at T. The choice is kept per target for 15 minutes (a new forward
  takes it at once, and nothing is measured again until then); a member
  that stops answering sends the forward back to the direct route for 2 minutes. The Browser shows
  "on instabox via kdi-vps".