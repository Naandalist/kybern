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
