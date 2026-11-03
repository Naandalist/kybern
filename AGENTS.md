# kybern

Rust workspace: a daemon (`kybernd`) that drives coding agents, the Kybern
GPUIX desktop client (React on GPUI through Bun, on macOS, Windows and Linux),
a CLI, and an Expo mobile client. The Tauri desktop app that preceded GPUIX
was removed after 0.18.0 (issue #113). Read `docs/architecture.md`
for how the pieces fit and `docs/design.md` for the desktop design rules
before touching UI.

## Layout

| Path | What |
| --- | --- |
| `crates/kybern-protocol` | Wire types. Every RPC method lives in `methods.rs` with its scope. `kybern-schema` bin dumps JSON Schema. `process.rs` is the one `NoWindow` helper (Windows `CREATE_NO_WINDOW`) that daemon, driver and git children use; wrap background `Command::new(..)` with `.no_window()`, never the terminal PTY. Its `write_executable` writes a program the process starts next (every test fixture script): a plain write can leave the file busy (Linux ETXTBSY) for a sibling thread's forked child. |
| `crates/kybern-store` | SQLite. Migrations are the array in `schema.rs`; append a new entry, never edit an old one. `projection.rs` folds events into transcripts. |
| `crates/kybern-git` | Snapshots, diffs, worktrees via the `git` CLI. |
| `crates/kybern-drivers` | One module per agent: `claude`, `codex`, `opencode`, `pi` (also omp), `cursor`, `antigravity` (`agy`: one process per turn, Full access only; Kybern tools through the one global `kybern` MCP entry (`kybern mcp-bridge`, `antigravity_mcp.rs`, setting `antigravity.kybern_tools`); spec in `docs/design/ade-111-antigravity.md`; the daemon rules are in `orchestrator/antigravity_rules.rs`). All implement `AgentDriver` + `AgentSession` from `lib.rs`. `sessions/` reads saved native sessions without running anything; `SessionReader` (`AgentDriver::open_session_reader`) keeps one helper process (Codex app-server, `opencode serve`, Cursor sidecar) open across many list/read calls for scans and bulk imports. |
| `crates/kybern` | The shipped package: `kybernd` and `kybern` binaries as thin wrappers over the two library crates below. |
| `crates/kybern-daemon` | `lib.rs` (axum), `ws.rs` (auth, subscriptions), `rpc.rs` (dispatch), `orchestrator.rs` (threads, turns, approvals, checkpoints; `orchestrator/agent_items.rs` is the agents' Notes and Tasks tools: agents fully edit only what they created, never close tasks, ask `kybern_notes_tasks` consent outside Full access/Auto, and file at most 10 tasks a turn; `orchestrator/lead.rs` makes a project's Coordinator an Orchestrator V2 lead thread: lead instructions, read-only tool restrictions, brief and notes index, harness switch; `orchestrator/secret_request.rs` and `secrets.rs` are the `kybern_request_secret` tool and the redacted write of its answer to a git-ignored env file; `secrets.target` and `secrets.store` reuse the file rules for a key pasted into the composer, refuse tracked files and can add the file to `.gitignore`), `pick.rs` and `orchestrator/pick_request.rs` (`kybern_request_pick`: a card asking the user to point at a Preview or visual element, file, folder, image or window; the answer is re-bounded into plain fields), `chips.rs` (`kybern://secret/…` and `kybern://anchor/…` composer chips expand in the provider's copy only), `terminal.rs`, `files.rs`, `github.rs`, `http.rs`, `access.rs`, `settings.rs`, `cursor_setup.rs` (Cursor SDK install and browser sign-in for clients), `session_import.rs` (first-run `sessions.scan_projects` / `sessions.import_all`: scans every harness account concurrently, groups sessions by git root, imports under that root's project with the session's own `updated_at`; one `SessionReader` per harness account per job, jobs outlive the client and count as daemon activity; notifications `sessions.scan.changed`, `sessions.import.progress`, `sessions.import.finished`), `usage.rs` (account plan limits: one cache fed by turn-free live reads and turn reports, pushed as `usage.limits.changed`), `computer/` (computer use), `network/` (the machine network, design in `docs/design/machine-network.md`: `keys`/`ops`/`membership` are identity and the signed membership set; `tailscale` is the CLI/fake seam, `KYBERN_TAILSCALE_FAKE=<fixture.json>` plays a tailnet; `well_known` is the signed identity endpoint, `session` the handshake and grants, `peers` the dial rule, heartbeat and backoff worker, `relay` the multiplexed relay; `join` the offers (auto, found, code, migrate, merge) and the discovery pass that joins same-user machines, `removal` removal and its side effects, `manage` pause/rename/policy/switch/rejoin, `client_grant` the one-pairing-every-machine tokens and device revoke fan-out, `status` and `doctor` what `network.status`/`network.doctor` report, and `kybern machines …` is `crates/kybern-cli/src/machines.rs`; the in-process multi-daemon harness is `network/harness.rs`, process tests are `tests/network/`), `login_service/` ("Keep this machine reachable", ADE-119: the per-user login item a networked daemon registers (Task Scheduler via `conhost --headless`, LaunchAgent, systemd user unit or XDG autostart) behind the `ServiceManager` seam, which is inert outside a real `kybernd`; `network/wake_watch.rs` cuts the reconnect backoff short after sleep and tailnet changes), `agent_guide.rs` (the "Working in Kybern" guide every session receives, rendered from its tools and byte-stable for prompt caching; wording changes bump `GUIDE_VERSION` and the golden file; the `tell_agents_about_kybern` setting turns it off). |
| `crates/kybern-daemon/src/previews/` | In-app browser preview. `tickets.rs` (in-memory, sliding 30 min, 12 h cap, 64 max, revoked with the thread/device), `files.rs` (`/preview-files/{ticket}/{*path}`: canonical root, no dotfiles or symlink escapes, nosniff, sandbox CSP without allow-same-origin, `bridge.js` injected into HTML), `mod.rs` (`previews.open`: classification, roots, persisted folder grants that can never cover `/`, home, system folders; `is_local_or_private_host`), `probe.rs`, `discovery.rs` (lsof, /proc or common ports, 2 s shared cache), `proxy.rs` (`/preview-proxy/{ticket}` with WebSocket, loopback and private addresses only; strips X-Frame-Options and `frame-ancestors`, forwards the app's own cookies). GPUIX's `src/preview/relay.ts` gives relayed pages their own loopback origin. |
| `crates/kybern-client` | Async JSON-RPC client used by the CLI and the daemon. |
| `crates/kybern-cli` | The `kybern` CLI (`lib.rs`). Also the integration harness. |
| `apps/gpuix` | Kybern GPUIX, the desktop app (see below). Ships for macOS, Windows (NSIS) and Linux (AppImage, .deb, .rpm) from the same release as the daemon. |
| `apps/mobile` | Expo SDK 57 Android/iOS companion (pnpm 11, exact pins). Expo Router, Ink theme, streamed conversations, approvals, files, terminals, and tasks. See `apps/mobile/README.md`. |
| `packages/kybern-client` | Shared TypeScript transport, address/pairing helpers, wire types, transcript projection, and composer/provider-input helpers. GPUIX and mobile import these sources; its tests (`tests/`) run in CI's `gpuix` job. |

The old GPUI client is on the `gpui` branch. Do not port its views back.

### Kybern GPUIX (`apps/gpuix`)

| Path | What |
| --- | --- |
| `src/` | The app: `main.tsx` (boot), `App.tsx`, `state.ts`, `shared/` (theme math, formatting, accounts, notes/tasks models and transcript helpers that came from the Tauri app; imported as `@/…` or by relative path, tests in `shared/tests`), one folder per surface (`transcript/`, `composer/`, `sidebar/`, `nav/`, `dock/`, `notes/`, `tasks/`, `pulls/`, `settings/`, `split/`, `tabs/`, `terminal/`, …). `kit/` holds the UI primitives (read `kit/README.md`), `icons.tsx` the icon set. |
| `src/platform/`, `src/shell/`, `src/native/` | Per-OS services and window shell: shortcut keys (`platform/keys.ts`: ⌘ on macOS, Ctrl elsewhere), fonts, OS display settings, window chrome, notifications, updates, and wrappers over the addon. |
| `native/` | The patched GPUIX addon. `FORK` pins the forks, `update.sh` builds it into `native/build`, and `README.md` (long; search it) documents islands, cached views, input and every native change. `*-check.ts` / `*-lab.tsx` are the on-screen checks; `check-harness.ts` runs them. `wpe/` builds the Linux web view's WebKit; `webview-*` are the per-OS web view hosts. |
| `packaging/`, `scripts/package-*.ts` | Per-OS packages, installers and their checks (`packaging/README.md`). |
| `scripts/run-platform-checks.ts` | The Linux/Windows check runner the `GPUIX platforms` workflow calls. |
| `PORTING-PLATFORMS.md`, `WEBVIEW-PLATFORMS.md` | The Windows/Linux port: status per work package, known gaps, CI and hardware findings, and the record of the Tauri retirement. |

## Build and run

```sh
cargo build -p kybern                                    # seconds; kybernd + kybern
./target/debug/kybernd --data-dir /tmp/kyb --port 4199   # scratch daemon
./target/debug/kybern --data-dir /tmp/kyb providers

cd apps/gpuix
pnpm install --frozen-lockfile
native/update.sh                      # build the patched addon pinned in native/FORK (minutes)
KYBERN_DATA_DIR=/tmp/kyb pnpm dev     # GPUIX against the scratch daemon; omit for ~/.kybern
pnpm screenshot out.png               # background launch and a PNG of the window
```

In a separate checkout or worktree, explicitly set `CARGO_TARGET_DIR` to that
checkout's own `target` directory for Cargo commands. An inherited
value can point at the running app's checkout and overwrite its daemon even
when the sources and test data are isolated.

Use a scratch `--data-dir` for testing so `~/.kybern` stays clean. The app
connects to the daemon named by `KYBERN_DATA_DIR` (port and token files) and
spawns `kybernd` from its own directory or `PATH` when none is reachable.
`KYBERN_NO_ACTIVATE=1` keeps the window from stealing focus.

Without `native/build`, GPUIX falls back to the stock npm addon and loses the
patched features; `KYBERN_GPUIX_NATIVE=npm` forces that. Daemons outlive
desktop windows. For an app-managed local endpoint, the desktop restarts a
daemon whose version is incompatible or whose binary is older than the staged
binary. Explicit `KYBERN_URL` endpoints remain externally managed and are never
stopped by the app.

### Disk usage and cleanup

Disk space is a development budget. Do not leave large build directories in
every checkout or worktree. On September 19, 2026, unused Rust build output and
plugin staging alone required a 41 GiB cleanup.

- Before a native build, dependency installation, simulator download, or large
  test run, check free space on the workspace volume and measure the relevant
  build/cache directories. Check again after heavy work. Below **30 GiB free**,
  pause new disk-heavy work, reclaim this task's unused generated files, and
  recheck. If that is insufficient, report the blocker rather than filling
  the disk. Do not skip required validation to meet the budget.
- Reuse one `CARGO_TARGET_DIR` inside the current checkout. Never create a new
  target directory per command, copy an existing `target/` into a worktree, or
  point another checkout at the running app's target directory. Use
  `CARGO_INCREMENTAL=0` for disposable worktree builds and one-off validation;
  retain incremental compilation only for an actively iterated checkout.
- Run the checks required for the change; avoid redundant full workspace
  builds, release builds, native exports, or installations. Research/review
  worktrees should not install dependencies or compile unless needed. If a
  task adds **10 GiB** or its target directory exceeds **15 GiB**, inspect and
  remove obsolete task artifacts before starting another heavy run. These are
  cleanup checkpoints, not permission to interrupt a running build.
- Before finishing work in a temporary checkout/worktree, remove its generated
  Rust targets, task-created native DerivedData, disposable dependency installs,
  scratch test data, and redundant exports after preserving deliverables and
  validation evidence. Track custom output paths, including `.cargo/target`.
  Cleanup of this task's unused generated artifacts is part of the task and
  does not require a separate confirmation. Keep source changes and worktree
  registrations intact; never delete a whole worktree merely to reclaim space.
- Before deleting build output, check running builds, daemons, and open files.
  Stop only scratch processes owned by this task. Never run `cargo clean`,
  sweep, or remove output used by an active desktop, daemon, build, or another
  task. Preserve the user's installed daemon and normal `~/.kybern` data.
- Keep build outputs, dependency trees, scratch data, and recordings out of
  commits and handoff copies. Do not create full repository backups or duplicate
  dependency caches for routine validation. Measure relevant agent snapshot or
  staging growth when investigating disk pressure; do not automatically delete
  chat history, recovery snapshots, or backups.
- Reuse installed mobile runtimes and devices. The user tests on **iPadOS 27.0**;
  preserve its device support. Do not install extra simulator runtimes without
  a task requirement or erase simulator app data, signing assets, or release
  archives as routine cache cleanup.
- For disk-heavy tasks, report free space before/after and any large generated
  directories retained with the reason. File deletion may leave blocks held
  by APFS snapshots; verify actual free space (`diskutil info /`, "Container
  Free Space") instead of claiming the directory size was reclaimed. Do not thin
  Time Machine snapshots as routine build cleanup.
- Rust builds go through kache (`rustc-wrapper` in `~/.cargo/config.toml`), a
  shared build cache in `~/Library/Caches/kache` capped at 10 GiB by
  `~/.config/kache/config.toml`. `kache stats` shows it, and `kache gc
  --max-age 2d` trims it. Remove a finished worktree's own `target/`; the
  cache keeps what other builds can reuse.

## GPUIX native forks

The addon is built from two forks: `Tresnanda/gpuix` (the N-API addon, with a
`zed` submodule) and `Tresnanda/zed` (GPUI). `apps/gpuix/native/FORK` pins both
(`commit=`, `zed=`); `update.sh` builds exactly that pin. Read "Changing the
native module" in `apps/gpuix/native/README.md` first.

- A native change is a commit on the forks plus a new pin in the same Kybern
  commit as the app change that needs it. `kybern` is the shipped line and
  `next` the integration line; parallel work uses its own branch (`wp3`,
  `winhw2`, …) and only the integrating agent merges into `next`, then
  fast-forwards `kybern`. Never force-push either.
- Merging fork branches: fetch before using `origin/next` as a parent (an
  explicit-SHA push does not update remote-tracking refs), bump the gpuix
  `zed` gitlink to the merged zed commit, and check `git diff --stat` of every
  hand-built commit before pushing. A clean text merge can still break the
  build: after any merge that touches `Cargo.toml`, check for duplicated
  `[...]` tables.
- A Metal change needs its WGSL and HLSL counterparts in the same commit. A
  struct shared with the shaders must keep the same layout on all three.
- Code that only exists on some platforms needs a `cfg` gate in Rust or a
  `process.platform` check in TypeScript. The macOS build is the one most often
  broken by Windows/Linux work; build it before pinning.

## Cross-platform checks

- `.github/workflows/gpuix-platforms.yml` runs the GPUIX on-screen checks on
  Windows (Direct3D 11 on WARP), Linux X11, Wayland and Ubuntu 24.04. Dispatch
  it on a pushed branch: `gh workflow run gpuix-platforms.yml --ref <branch>
  -f checks=all` (or a comma list). A full run takes 35–40 minutes. The runners
  have no GPU, and some checks are listed as known gaps.
- The Linux web view's WPE WebKit build (`gpuix-wpe.yml`) takes about two hours
  cold. It is cached by the hash of `native/wpe/wpe-version.sh`, `build-wpe.sh`
  and the patches. GitHub caches are branch-scoped: a tag can only reuse main's
  cache. Changing a WPE patch costs one cold build per branch that runs it.
- `ci.yml` runs GPUIX tests on Linux, where Ctrl is the primary shortcut key.
  Tests written for macOS shortcuts set `setKeyPlatform('darwin')` from
  `src/platform/keys.ts`; off-macOS cases set their platform themselves.
- PR CI runs `cargo check --workspace --all-targets` on `windows-latest`
  when Rust inputs change, on PRs and ordinary main pushes; release version
  bumps and manual full CI runs always include it. Warnings fail the
  check, and test code is checked without running Windows tests. Full Windows
  builds run in `GPUIX platforms` and in the release. Grep new daemon code for
  Unix-only APIs (`os::unix`, `libc::`, `/proc/`) without a `cfg` gate before tagging.
- Real hardware: the user's Windows 11 PC is `ssh satyamas` (RTX 3060 and AMD
  iGPU, WSL Ubuntu with a 22.04 chroot for release-floor Linux builds). Use it
  for real-GPU checks and as a fast dev build box, one lane at a time; PRs,
  CI and releases stay on GitHub, and it is never a CI runner. Never touch the
  user's own files, conda or shell profiles there, and never send OS input
  unless Kybern's own window is in front.

## Mobile app

- Read `apps/mobile/README.md` before working on the companion. Reuse
  `src/ui/primitives.tsx`, `src/ui/theme.tsx`, and existing feature components.
  Icons need both an SF Symbol and a Material Symbol in `src/ui/icons.ts`.
- Android uses custom themed components and liquid-gooey-style motion, not stock
  native Android controls. Menus must have opaque readable surfaces. iOS may use
  native Liquid Glass. Preserve this platform distinction when adding UI.
- `app/` contains routes; `src/features/` holds conversation/workspace UI;
  `src/state/runtime.ts` owns secure connections, subscriptions, and hydration.
  Keep the shared TypeScript client and Rust wire types synchronized.
- Preserve settled transcript identities, bounded history, paced streaming,
  exact final text, and readable formatting. Mobile performance notes live in
  `apps/mobile/perf/`; desktop checks do not exercise React Native.
- Use `pnpm start:go --lan` for SDK 57-compatible Expo Go, or `pnpm start --lan`
  for an installed development build. `pnpm ios` / `pnpm android` compile the
  native app. The `preview` EAS profile produces an APK without Metro.
- EAS belongs to personal account `treshnanda`, project `kybern-mobile`.
  Preserve the explicit owner and project ID; do not move builds to `beme-mobile`.
  Run EAS commands from `apps/mobile`. Root `.easignore` must retain both the
  mobile app and `packages/kybern-client`, excluding local artifacts and caches.
- Native config is generated. Keep `withSceneLifecycle.js` until Expo supplies
  the iOS 27 scene delegate. `withAndroidLocalNetworking.js` permits direct
  LAN/Tailscale HTTP/WebSocket connections in release APKs; test release config,
  since Expo Go and debug builds can hide native configuration errors.
- Native sockets use the existing trusted Kybern origin and authorization
  header through `nativeSocket.ts`. Preserve identity verification and the
  browser ticket flow. Save redeemed pairing credentials before connecting so
  a failed socket handshake does not consume the only recovery path.
- Test with a scratch daemon. Cleanup may stop scratch daemons and Metro and
  remove generated build caches, but preserve the user's normal `~/.kybern`
  data and installed daemon needed by their desktop/mobile sessions.

## Computer use

- `crates/kybern-daemon/src/computer/` exposes `kybern_computer_*` native tools to
  every harness except Codex, which keeps OpenAI's bundled plugin. The daemon
  asks per-app consent itself (`kybern_computer_use` approvals) because harness
  approval of Kybern tools is inconsistent.
- CuaDriver is installed separately into `/Applications/CuaDriver.app` and owns
  the Accessibility and Screen Recording grants. Kybern never bundles it; the
  pinned installer and minimum version live in `computer/driver.rs` and
  `computer/setup.rs`. `kybern computer doctor` explains each requirement.
- Kybern starts CuaDriver's daemon itself (`open --env … serve`), with
  `--no-overlay` unless `computer_use.show_cursor` is on; its cursor overlay
  held 0.9–1.6 GB in live runs. The daemon reads its speed settings from its
  own environment: the post-action window watch (1000 ms by default, most of
  each click) is 150 ms. Kybern stops a daemon it launched when idle and on
  exit, and leaves one it found running. Measure the daemon with `footprint`,
  not RSS, and per-call timings with `RUST_LOG=kybern::computer=debug`.
- `@Computer` is a `Mention` with path `kybern://computer`. The daemon lists
  it in `skills.list` and expands it into an instruction only in the copy a
  provider receives. An idle session spawned without the tools is resumed
  with them at turn start; otherwise the agent is told the tools are missing.
  `computer.frame` feeds the desktop live view; frames are captured only
  while a client polls and are never persisted.
- Claude Code defers MCP tools, so `kybern_computer_help` is rarely read. Put
  must-know guidance in the @Computer instruction or tool descriptions, and
  app-specific quirks seen in real runs in `quirk_note` (shown once per session).
- App notes (`computer/notes.rs`) are what agents learned about one app, saved
  with `kybern_computer_note` as `<data dir>/computer-notes/<bundle id>.md` and
  shown with the quirk note the first time a session uses the app. They reach
  later prompts, so they stay small (1500 characters), are framed as hints, and
  are editable in Settings → Computer use and `kybern computer notes`.
- Keep the contract cheap: accessibility text by default, screenshots only on
  request, batched `act` steps that return a diff. Run the real-app check with
  `cargo test -p kybern-daemon computer::live -- --ignored --nocapture`.

## Release versions

Daemon, CLI and GPUIX share the workspace version and
`vX.Y.Z` release pipeline. Dev releases (`vX.Y.Z-dev.N`, GitHub prereleases with
`latest-dev.json` on the rolling `dev-feed` release) reach installs on the Dev
update track first; stable versions are promoted from them (RELEASING.md,
"Update tracks"). The mobile app has its own `expo.version`; EAS manages native build
numbers remotely. A push to `main` is not a release. The desktop release script
does not version or build mobile. See `RELEASING.md`.
Do not push mobile version tags through the broad daemon release trigger without
first separating the workflows. Breaking protocol changes require a coordinated
`PROTOCOL_VERSION` change; additive features may need a newer daemon too.

Release order:
1. Bump versions and the CHANGELOG in the PR.
2. Merge it.
3. Wait for the full CI suite on that exact main commit; version bumps run it
   automatically. Otherwise dispatch `gh workflow run ci.yml --ref main` while
   main points at the candidate. The release gate (`scripts/check-release-ci.mjs`)
   rejects filtered runs with skipped jobs and checks for a different commit.
4. Wait for main's WPE run if the recipe changed.
5. Push an annotated `vX.Y.Z` tag.

Afterwards, check that `latest.json` lists the `-gpuix` keys for macOS,
`windows-x86_64` and `linux-x86_64`. Before a mobile update, compare
`eas update`'s runtime with the installed builds (`eas build:list`).

## Tests and checks

```sh
cargo test --workspace                                    # unit + snapshot + live driver tests
cargo nextest run --workspace                             # the same tests the way CI runs them, one process each
cargo test -p kybern-drivers --test live_drivers          # runs real agent CLIs when installed, skips otherwise
cargo fmt --all --check && cargo clippy --workspace -- -D warnings

cd apps/gpuix && pnpm typecheck && bun test --path-ignore-patterns 'native/upstream/**'
node --experimental-strip-types --test packages/kybern-client/tests/*.test.mjs

cd apps/mobile
pnpm typecheck && pnpm test
pnpm exec expo-doctor
pnpm exec expo export --platform all
```

Protocol changes break `crates/kybern-protocol/tests/snapshots`. That is the
point: review the diff, then `INSTA_UPDATE=always cargo test -p kybern-protocol`.
CI runs fmt, clippy and the Rust tests on ubuntu and macos, checks every Rust
target on Windows without running tests, and runs the GPUIX, shared-client and
release checks on ubuntu (see Cross-platform checks for the GPUIX on-screen checks).
The Rust tests run under cargo-nextest (`.config/nextest.toml`), each in its own
process; the workspace has no doctests, which nextest skips, so the first one
needs `cargo test --workspace --doc` added to `ci.yml`. The whole CI run must
stay under 10 minutes; measure the jobs on a real run when you add slow tests.
A test that fails at random on CI is quarantined, not rerun until green: the CI
profile retries a failed test once, and one that then passes keeps the job
green but is listed under "Flaky tests" in the job summary. On pushes to
`main`, `scripts/report-flaky-tests.mjs` also files a `flaky` issue titled after
it, or comments on and reopens the existing one (at most five a run; pull
requests and branches only get the summary). That issue still has to be fixed
or the test removed; make tests wait on events or conditions, not sleeps or
clocks. A test that fails both attempts fails CI.
Mobile checks and EAS builds are currently manual; run the mobile checks for
mobile or shared-client changes. Native configuration changes need a new build.
EAS Update uses preview/production channels and fingerprint runtime compatibility.
Use the corresponding `--environment` when publishing and verify the runtime
matches the intended native build. Keep transcript pages sequence-bounded and
replay events received during hydration; preserve row identity and reading position.

## Conventions

- Adding an RPC method: struct pair + `method!` in `methods.rs`, add it to
  `registry!`, add it to `bin/schema.rs`, handle it in `daemon/src/rpc.rs`,
  add a CLI subcommand, mirror the types and the `Methods` map entry in
  `packages/kybern-client/src/types.ts` (every client imports it). Scope is
  enforced in `ws.rs` from the registry.
- Adding a driver: implement the traits in `drivers/src/lib.rs`, register in
  `registry.rs`, add a live test in `tests/live_drivers.rs`, document the
  permission-mode mapping in `docs/architecture.md`. Speak the agent's own
  protocol; do not add a shared abstraction layer. Follow
  `docs/harness-parity.md` for spawned-agent and background-process lifecycle,
  controls, recovery, and UI parity.
- Provider subagents: every runtime task of kind `Agent` gets a read-only child
  thread (`Thread.subagent`, store migration 17, `orchestrator/subagents.rs`),
  created and refreshed from the task snapshot and fed with the subagent's own
  prompt, messages, reasoning and tool calls under its own turn. Drivers tag that
  output with an `EventOrigin::Agent` origin, a tool `parent_id`, or
  `DriverEvent::SubagentOnly` and send the launch prompt as
  `DriverEvent::SubagentPrompt`; child prose never enters the parent log, and
  child tool calls nest in the parent only where a harness already did that
  (Claude, Cursor). Sends, steering and edits to a subagent thread are rejected;
  `tasks.stop`, `tasks.background` and `threads.interrupt` accept its id.
  `threads.list` omits subagent threads unless `include_subagents` or
  `parent_thread_id` is set, and clients must keep them out of normal lists
  (`isSubagentThread` in `packages/kybern-client`). Archiving a thread archives
  its subagent threads.
  Rules for grouping, phases, copy and sidebar visibility live in
  `packages/kybern-client/src/subagents.ts`; a subagent is an ordinary,
  read-only thread page in the clients.
- Events are append-only. New behavior means a new `EventPayload` variant plus
  handling in `store/projection.rs` and the shared transcript projection in
  `packages/kybern-client` (which GPUIX and mobile use). Clients ignore
  unknown kinds.
- Thread anchors for rewind are recorded on `TurnCompleted` (`TurnAnchors`).
- `serde_json` has `preserve_order` on for deterministic schemas. Keep it.
- Copy: sentence case, verb-first buttons, errors say what to do next.

## Desktop UI rules

GPUIX (`apps/gpuix`):

- Reuse `src/kit` (read `kit/README.md`) and the shared surfaces before writing
  new styles. Colors and materials come from `src/theme.ts`, which builds on
  the theme math in `src/shared/lib/kit/theme`. Keep the look the user approved: change only what was
  asked.
- Shortcuts go through `src/platform/keys.ts` (`isPrimary`, `SHORTCUT`), and
  labels are written in macOS glyphs, which the kit converts to "Ctrl+…" off
  macOS. A focused terminal keeps Ctrl and Alt for the shell, and AltGr
  (Ctrl+Alt with a composed character) must type its character.
- Read `apps/gpuix/native/README.md` (islands, cached views, wrapper layout
  cost) before changing transcript rows, lists or anything per frame.

- Dependencies are exact-pinned; `.npmrc` sets `save-exact` and
  `ignore-scripts`. Check socket.dev before adding a package.

## Performance and visual quality

Responsiveness and visual quality are joint acceptance criteria. Preserve the
kit's hierarchy, materials, typography, and useful motion while reducing work.
Before changing transcripts, Markdown, state subscriptions, navigation rails,
window materials, or animations, read `apps/gpuix/native/README.md` and the
GPUIX reports in `apps/gpuix/perf/` for the affected path. The retired Tauri
app's measured WebKit regressions and fixes are kept in
[its performance guide](apps/gpuix/perf/tauri/README.md); its principles still
apply, its WebKit specifics do not.

- Keep settled turn objects, row keys, Markdown component types, and highlighted
  DOM stable. A streaming update should invalidate the affected content only.
- Bound mounted history and large expanded work through the existing virtual
  rows. Unmount closed details after their exit; preserve selection, focus,
  wrapping, expansion, reading position, and follow behavior across remounts.
- Keep large Markdown parsing and syntax highlighting in the existing workers.
  Retain bounded queues/caches, cancellation, prefix progress, final catch-up,
  and readable failure fallbacks. Assert actual formatted output as well as text.
- Derive navigation from transcript data, cache bounded previews, and coalesce
  geometry reads once per frame. Offscreen messages must remain reachable.
- Reuse paced streaming and visibility-aware motion. Pause loops offscreen, in
  hidden windows, and in inactive panes; prefer opacity/transform over repainting
  colors. Keep typing and interaction feedback immediate.
- Use one intentional blur layer for a floating surface. Keep structural
  wrappers compatible with translucency, avoid duplicate pane tints, and verify
  light/dark, opaque, reduced-transparency, and increased-contrast states.
- Measure the failing workload before and after; verify appearance alongside
  timing. Run the relevant GPUIX on-screen checks plus the normal checks. A readable fallback, empty list, lost content, or
  removed visual effect does not count as an optimization.

Report CPU, commit time, frame intervals, input probes, and energy separately;
include the workload, runtime, hardware, and limitations with measurements.

## Verifying UI

GPUIX has an automation client: the on-screen checks in `apps/gpuix/native`
and `apps/gpuix/scripts` drive a real window through `check-harness.ts`, and
`pnpm screenshot` captures one. Use a scratch daemon, keep one window per
worktree, and screenshot only the app window. A locked or sleeping Mac screen
stops frames: wheel scrolls never land and captures fail.

## Delegated agents and branches

- A delegated worktree starts with a "kybern delegation base" commit that
  snapshots the parent's uncommitted and untracked files. Never push it: the
  repository is public. Build any branch you push (CI spikes included) from a
  public commit plus your own commits, and check `git log <base>..<branch>`
  first. Delete throwaway `spike/*` branches when done.
- Stage explicit paths; never `git add -A`.
- Wait for CI and long builds with a blocking command (`gh run watch <id>
  --exit-status`, or a loop) and end your turn only with the result. A
  delegated agent whose turn ends on "waiting…" is marked finished.
- In zsh, `$var:r`, `$var:c` and similar are modifiers, so write
  `"${sha}:refs/heads/x"` and `"${sha}:path"`. Unbraced, they silently push
  the wrong ref or read an empty file.

## Sharing image previews

Kybern shows a local image a reply references from any folder: a path relative
to the conversation's working folder, an absolute path or a `~/` path, in a
separate checkout or worktree too (the daemon's `/threads/{id}/image` route
serves any readable PNG, JPEG, GIF, WebP or AVIF file, judged by its content).
There is no need to copy a preview into the conversation's folder; reference
it where it is and verify it exists first.

Use `![Description](<path.png>)` for an inline preview. Keep generated
previews out of commits unless the user asks to include them. Follow the
thread-specific artifact guidance when Kybern provides the exact folder.

## Data directory

`~/.kybern/`: `state.sqlite`, `settings.json`, `keybindings.json`, `themes/`,
`daemon.token` (0600), `daemon.port`, `worktrees/`, `assets/`, `cache/` (harness model
catalogs, safe to delete).

### Branded image backgrounds

For Kybern promotional images, screenshot presentations, and illustrated feature
cards, reuse `assets/release-background.png` as the background.
It is the same artwork as `/Users/mymac/Downloads/Frame 7 (1).png`, already used
by the update notice. Prefer the repository asset so visuals remain reproducible
and consistent with the website. Keep functional UI previews and content readable.
