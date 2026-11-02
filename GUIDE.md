# Kybern guide

Everything past the basics in the [README](README.md): the command line,
permissions, each agent's settings, delegation and the project Coordinator,
accounts, and connecting to other machines. For Pi's model and thinking
controls, steering and permission modes, see the
[Pi integration guide](crates/kybern-drivers/PI.md).

## Command line

The `kybern` command talks to the same daemon as the app. Turn it on in
**Settings → About → Terminal command → Install** (the desktop app keeps it up
to date), or install it on its own with the remote installer in the README.

```sh
kybern providers                                # which agents are installed
kybern new -p . "explain this repo in three lines"
kybern threads
kybern send <thread-id> "now write the README"
kybern watch                                    # live events for every thread
kybern ls <project-id> src                      # browse project files
kybern terminal run <thread-id> "git status"
```

Approvals show up inline in `new` and `send`; answer with `y`, `a` (always) or `n`.

| Platform | What **Install** does |
| --- | --- |
| macOS | Links `kybern` into `~/.local/bin` when that is on your PATH, otherwise into `/usr/local/bin` after an administrator prompt. |
| Windows | Adds Kybern's `bin` folder to your user Path. Open a new terminal afterwards. |
| Linux | Links `kybern` into `~/.local/bin`: from `/usr/lib/kybern-gpuix/` for the `.deb` and `.rpm`, or from the copy the AppImage keeps in `~/.local/share/kybern-gpuix/bin` and refreshes on each update. |

If an older `kybern` (for example one from `cargo install` in `~/.cargo/bin`)
comes first on your PATH, the same row shows it and offers to remove it.

## Resume a conversation started elsewhere

Choose **Resume session** beside **New thread** in the sidebar. You can also
use `/resume` or `/sessions` in the composer, the project menu, or the command
palette. Search by title, folder, or native session ID; filter by
harness and project. Choosing a conversation imports its available message and
tool history and continues the same native session on your next message.
Choosing it again opens its existing Kybern thread, including archived threads.

```sh
kybern sessions --provider claude-code
kybern sessions --provider codex --query "session picker"
kybern resume --provider codex <native-session-id>
kybern send <returned-thread-id> "Continue from where we left off"
```

All six harnesses are supported: Claude Code, Codex, OpenCode, pi, OMP, and
Cursor. Discovery uses the selected environment’s installed harness and
configured profile. New Cursor chats use the official local SDK. SDK session IDs
start with `cursor-sdk:`; older CLI chats continue through ACP. Editor-only
and cloud conversations are not included. The original working folder must
still exist. Kybern uses it directly and does not create or switch a worktree.

Importing history does not execute old tools, recreate checkpoints, or charge
historical usage to Kybern. History imports are limited to 64 MB. If a harness
refuses continuation because another process owns the session, close that
process and retry. Saved sessions remain native to their original harness.

## Full access

Full access never shows a permission card. Each harness starts in its own
no-approval mode:

| Harness | Full access launch |
| --- | --- |
| Claude Code | `--permission-mode bypassPermissions --allow-dangerously-skip-permissions` |
| Codex | approval policy `never`, sandbox `danger-full-access` |
| OpenCode | one `*: allow` permission rule |
| Pi / Oh My Pi | yolo approvals |
| Cursor SDK | sandbox and automatic review off |

A harness can still ask in that mode. Claude Code, for example, keeps its
built-in safety checks, such as `rm` on a possibly empty `$VAR` path. Kybern
allows those requests itself and records each one in the transcript as
"Allowed by Full access". Questions are never answered for you: AskUserQuestion,
forms and MCP elicitations, Pi dialogs, plan approval (ExitPlanMode) and
`kybern_request_secret` still wait for your reply.

Switching an idle conversation to Full access resumes Claude Code and Oh My Pi
with the new launch mode; Codex, Pi, Cursor and OpenCode 2 switch in place. An
OpenCode 1 session keeps its launch rules, so Kybern allows whatever it still
asks. During
a running turn the switch applies when the turn ends. Until then, Kybern allows
the command that is waiting and every later permission request.

## OpenCode versions

Kybern drives OpenCode 1.10 or later and OpenCode 2.2 or later; it reads
`opencode --version` and speaks each version's own server API. OpenCode 2.0
and 2.1 predate the server API Kybern uses, and a future OpenCode 3 is not
supported yet; the provider list says so instead of failing when a chat
starts. Run `opencode upgrade` to update.

Each Kybern mode becomes an ordered permission list on the session (the last
matching rule wins):

| Mode | OpenCode 1 rules | OpenCode 2 rules |
| --- | --- | --- |
| Supervised | ask `*`; allow read, glob, grep, list, todowrite, question, skill, task, lsp | ask `*`; allow read, glob, grep, list, question, skill, subagent |
| Accept edits | allow `*`; ask bash, webfetch, websearch, external_directory, doom_loop | allow `*`; ask shell, webfetch, websearch, external_directory |
| Auto | allow `*`; ask external_directory, doom_loop | allow `*`; ask external_directory |
| Full access | allow `*` | allow `*` |

OpenCode 2 renamed `bash` to `shell` and `task` to `subagent`, and has no
`doom_loop` or `lsp` permission. It allows paths outside the project by
default, so Kybern adds the `external_directory` ask rule to keep OpenCode 1
behaviour. OpenCode 2 applies a new mode to the running session. The Kybern
guide and a Coordinator's role reach OpenCode 2 as a session instruction entry
instead of a system prompt sent with every message, and its questions arrive as
forms, which Kybern shows as the usual question card. Effort controls are not
offered for either version.

## Cursor SDK

New Cursor chats use exact-pinned `@cursor/sdk` 1.0.35 in an isolated Node helper.
Node.js 22.13 or newer and npm must be installed on the daemon’s machine.
Set it up in **Settings → Agent providers → Cursor** (**Install SDK**, then
**Sign in…**), or choose the dimmed Cursor icon in the model picker to go
there. The daemon does the work, so this also sets up a remote machine; the
sign-in page opens on the computer you are using. From a terminal:

```sh
kybern cursor install
kybern cursor login
kybern cursor status
```

Browser login uses Cursor’s official SDK authentication flow, separate from the
Cursor app and CLI sign-in. Alternatively set `CURSOR_API_KEY` in the Cursor provider’s environment.
`kybern cursor logout` clears the SDK login; an environment key still takes
precedence. Discovery never downloads packages or starts a login flow.

Following T3’s SDK integration, Cursor has two permission policies:

| Kybern mode | Cursor SDK behavior |
| --- | --- |
| Auto-review (`auto`) | Sandbox enabled, automatic review enabled; no human approval prompts |
| Full access (`full-access`) | Sandbox disabled, automatic review disabled |

The SDK does not expose interactive approvals. Ask for approval and Approve edits
are unavailable for new SDK chats; explicit requests fail rather than pretending
they are enforced. Existing ACP chats retain their original permission controls.
New threads and SDK history imports default to Auto-review unless the configured
default is Full access. Cursor’s sandbox policy is distinct from Kybern’s own
per-app computer-use consent, which still applies.

The SDK driver streams text, reasoning, tool calls, native child tasks, final
answers, and token usage; supports images, model variants, cancellation, resume,
`/compress`, and authenticated Kybern MCP tools. Native child tasks are read-only:
the SDK does not expose child steering, stop, or adoption controls. Fork, rollback,
structured question responses, mid-turn steering, independent effort settings,
and authoritative local billing totals are not advertised. Pick a model variant
for Cursor-specific model parameters.

The installer uses the committed npm lockfile with lifecycle scripts disabled,
and stores the package under `~/.kybern/cache/cursor-sdk/1.0.35`. Updating it means
updating Kybern and running `kybern cursor install` again. For isolated tests,
set `KYBERN_CURSOR_SDK_DIR`, `KYBERN_CURSOR_AUTH_FILE`, and
`KYBERN_CURSOR_STATE_DIR`; `KYBERN_CURSOR_NODE` selects the Node executable.
The old provider binary setting applies only to legacy ACP chats.

Implementation and validation details: [Cursor SDK driver](crates/kybern-drivers/src/cursor/README.md).

## OMP profiles

Open **Settings → Agents → OMP profiles** to select a default or a project
profile. Choose **Use environment**, **OMP default** (the unnamed profile), or
**Named profile**. Expand **Project overrides** to choose a profile for a
registered project; **Use default above** removes its override. Worktrees use
the registered project's profile.
Profiles apply when a chat first starts; resumed chats retain their original
profile even after defaults change or the daemon releases an idle process.
Older chats acquire that binding on their first start with this version.

The daemon's `settings.json` also supports:

```json
{
  "providers": {
    "omp": {
      "env": { "OMP_PROFILE": "personal" },
      "project_profiles": { "/absolute/registered/project": "work" }
    }
  }
}
```

`OMP_PROFILE` is resolved from the project override, provider environment, then
the daemon environment. `PI_PROFILE` is used only when `OMP_PROFILE` is absent.
Profile names follow OMP's
lowercase name rules. Discovery, model lists, skills and saved-session imports
use the selected project/profile. To resume from the CLI, use the same context:

```sh
kybern sessions --provider omp --project /absolute/registered/project
kybern resume --provider omp --project /absolute/registered/project <session-id>
```

## Mermaid diagrams

Desktop renders fenced `mermaid` blocks as diagrams. **Source** switches to the
original definition; **Preview** returns to the diagram. **Expand diagram** opens
a larger view, and **Copy code** copies the definition. Incomplete streaming
blocks remain source until settled, and invalid or oversized diagrams retain a
readable source fallback. Diagram rendering is lazy, uses strict Mermaid
security, and releases its rendering document after idle. Finished images stay
visible; switching chats releases their image URLs.

## Connectors, plugins, and Claude artifacts

Open **Settings → Integrations** on desktop, or **Connectors and plugins** in a
mobile conversation’s menu. Claude Code uses its native plugin catalog, scopes,
installation commands, and MCP sign-in. Codex uses its app-server catalog,
plugin actions, and account connector setup pages. Provider restrictions remain
in effect; plugins that require Codex’s installation interstitial must be
installed there. New Claude plugin installs use the native user scope. Existing
project/local scopes are preserved. Start a fresh agent session after changing
plugins. Connected Codex apps and enabled Claude plugin skills also appear in
the composer’s capability picker.

In a Claude conversation, ask for an artifact, or open **Artifacts** in the
desktop dock or mobile conversation menu. You can preview an HTML, Markdown,
or SVG file and inspect its source. Kybern supplies local previews, not Claude’s
hosted Artifact publishing tool. **Check publishing options** asks Claude to
check its current tools first; a missing tool must not be described as something
a retry will restore. Existing successful publication receipts retain their
Claude URL and update action. Requests remain ordinary Claude turns with their
normal approval handling, and publication requires a successful tool receipt.

**Share and versions** opens the hosted Claude page. Claude owns hosting,
sharing permissions, version selection, and live account connectors; Kybern
does not clone those account controls or use an undocumented publishing API.
Local HTML/SVG previews have an isolated origin and cannot access the daemon’s
credentials or fetch network resources. Use the hosted page for connector-backed
content and remote dependencies. Sources are limited to 1 MB inside the thread’s
workspace. Native publication also requires a Claude account and CLI version
that supports [Claude Code artifacts](https://code.claude.com/docs/en/artifacts).

```sh
kybern integrations list . --provider claude-code
kybern integrations change . plugin-name@marketplace enable --scope project
kybern integrations login <thread-id> <mcp-server-name>
kybern artifacts list <thread-id>
kybern artifacts read <thread-id> artifacts/dashboard.html
```

These additive RPCs require a daemon containing this feature. Updating only the
phone’s JavaScript does not add the corresponding daemon support.

## Working with other threads

Start an ordinary chat and ask for help: “Have Codex implement this and Claude
review it.” The agent delegates with Kybern's own tools. Each helper appears in
the **Agents** panel beside the conversation and as a child conversation in the
sidebar. Open a helper to follow its work or give it direction, then use **Main
thread** to return. Each helper is a real, resumable conversation and can use a
different installed harness.

Use **@** in the desktop composer, or add a conversation from the mobile
composer, to reference an earlier thread. Agents can also search and read past
conversations, including other projects on the connected daemon. Reading a
reference does not start a turn in that thread. Asking an agent to contact it
sends an attributed question; a busy recipient receives queued work and replies
return to the requesting conversation.
Sends respect permission boundaries: an isolated worker cannot wake a thread
in the main checkout and should return its work through the thread that started it.

New children start from the source thread's current Git commit unless the agent
specifies another revision. Editing workers use isolated Git worktrees, and
parent uncommitted changes are not copied into a child. Research and review
workers can also run in projects without Git. Review a worker's changes before
integrating them into another workspace. Settings > Agents sets how many
delegated agents one thread can run at once (four by default), how many a
project's Coordinator can run at once (six by default), and how deep delegation
can nest (two levels by default).

### The project Coordinator

Each project can have one **Coordinator**: a persistent, resumable chat that
plans work, hands it to other agents, and reviews what comes back. It is an
ordinary thread with a lead role, so it delegates with the same tools as any
other thread and its helpers appear in **Agents**. Use **Create coordinator**
in the sidebar (or on a project on mobile) to open it. On desktop the draft
composer has the usual harness, model, effort and permission controls; your
first message becomes the project's **brief** and is sent as its first turn.
Leaving the draft creates nothing. Archiving the Coordinator frees the slot for
a new one.

For each message the Coordinator answers in the conversation, starts a new
child, or sends the work to an existing child, and says which and why. It does
not edit files or run mutating commands; every edit goes to a child. If the
project has no setup note, it first delegates research on how to build, run and
test the project and saves the result as the **setup note**. Children get
pointers to notes, files and commits instead of pasted context, and are told to
stop and say exactly what is missing rather than mock or guess.

Claude Code, OpenCode and Pi/OMP enforce the read-only role by restricting the
Coordinator's native tools to read-only inspection. Codex and Cursor cannot
select tools, so their Coordinator follows its instructions only; the header
then shows **Edits not blocked on Codex** (or Cursor).

The project's shared space is made of project notes:

- **Brief**: a pinned note you write and edit. Agents read it but cannot change it.
- **Setup**: a pinned note only the Coordinator writes.
- **Memory**: ordinary project notes. Agents fully edit only notes they created
  and append to the others.

The Coordinator and every thread delegated from it start with the brief and an
index of the project's notes. If you change the brief later, the Coordinator
gets the new text with its next message. Turn on **Give every thread in this
project the brief** (`brief_in_all_threads`) to give every new thread in the
project the same brief and index.

A new Coordinator sets itself up. Kybern starts it with a short interview in
rounds (question cards where the harness has them), while a research agent
writes the setup note. Your first message in Draft becomes your first answer.
The interview ends with two cards: the brief, which you can reword before
saving, and the Coordinator settings it proposes, which you can untick row by
row. Use **Redo setup** in the Project tab to run it again.

Coordinator settings live in **Settings › Delegation** as defaults, and each
project can override them in its Project tab:

- **Before starting work**: **Act**, **Propose large** (small fixes start right
  away, larger or protocol work gets a plan first) or **Propose all**.
- **Agents work in**: this checkout or their own worktree.
- **Models for each kind of work**: harness, model and effort for research,
  implementation, review, design and test. The Coordinator may pick another,
  and the agent's row then shows its reason.
- **Push and open pull requests without asking**.

The Coordinator has no shell. It lands work with `kybern_git`: status, log,
diff, branches, commits and merging an agent's worktree branch need no
approval; pushing and opening a pull request ask unless the setting above
allows them; merging a pull request always asks. Agents never commit; the
Coordinator reviews their changes and commits them.

When the Coordinator is idle, or after a failed turn, use the normal harness
picker in its composer to change harness, model, or effort. Kybern keeps the
same conversation and notes; the selected harness begins with a fresh provider
session with the same Coordinator instructions. Finish or stop a running turn
before switching.

Coordinators created before the Coordinator became a lead thread were archived
when the daemon upgraded. Their latest project brief was saved as the pinned
brief note, their overview and setup log became memory notes, and their worker
threads stay available as history.

The desktop right sidebar starts empty. Use **+** to add the panels you need
and **×** to close them; Kybern remembers your choices for each environment.
Conversation shortcuts also open their corresponding panel.

The CLI exposes the Coordinator and read-only history:

```sh
kybern coordinator get <project-id>
kybern coordinator open <project-id> --provider claude-code --message "We build Kybern; small PRs"
kybern coordinator setup <project-id>
kybern coordinator switch-harness <project-id> --provider codex --model gpt-5.6-sol
kybern history search --all-projects --query authentication
kybern history read <thread-id> --limit 50
```

`coordinator get` prints the Coordinator thread, or `null`. `coordinator open`
is idempotent: a project has at most one Coordinator, so it returns the existing
thread with `"created": false`. `--message` and `--message-file` (`-` reads
standard input) give the opening message: a project without a brief starts the
setup interview with it. `coordinator setup` runs the interview again. `history read` returns bounded pages; continue with `--before-seq`,
`--message-seq` and `--text-offset`.

## Asking for secrets

Agents never ask for API keys in chat. `kybern_request_secret` shows a card in
the conversation with the label and reason the agent gave and a masked field.
**Save securely** writes the value to a git-ignored file in the thread's
workspace (`.env` by default) as the variable the agent named, replacing the
existing value and keeping every other line. **Decline** tells the agent no. The
agent only learns whether it was saved; the value is never returned to it and is
never stored in Kybern's history, which keeps `[redacted]` in its place. If the
file is in a Git project and not ignored, the request fails with a note to add
it to `.gitignore` first.

## Background behaviour

The daemon outlives the app, so it trims what it keeps alive once work
finishes. Agent processes are resumed from the provider's own session on the
next message, so releasing one loses no conversation. Limits live under
`background` in `settings.json` (also in Settings > General > Background);
every value is in minutes and `0` turns that limit off.

| Setting | Default | What it does |
| --- | --- | --- |
| `session_idle_minutes` | 10 | Close an idle thread's agent process after this long. Running, awaiting-approval, and background-task threads are never touched. |
| `max_idle_sessions` | 4 | Most idle agent processes kept warm; the least recently used go first. |
| `terminal_idle_minutes` | 60 | Close shells with no window attached and nothing in the foreground. |
| `daemon_idle_exit_minutes` | 0 (off) | Exit the daemon after nothing has needed it. Only for daemons the desktop app starts on demand; the CLI and remote clients do not restart an exited daemon. |

`kybern activity` shows what the daemon is holding open right now: clients,
agent processes (live and idle), terminals, queued follow-ups, and when it
would exit if idle exit is on.

### Keep this machine reachable when Kybern is closed

While a machine is in a machine network with Connect to your other machines
on, its daemon registers a per-user login item so it starts when you sign in
and your other machines reach it with no app window open: a Task Scheduler
task on Windows (`\Kybern\kybernd`, run headless), a LaunchAgent on macOS
(`~/Library/LaunchAgents/dev.kybern.kybernd.login.plist`), and a
`systemd --user` service on Linux (`kybern-login.service`, or an XDG
autostart entry without a user systemd). Nothing needs administrator rights.
The item is removed when the machine leaves or is removed from the network,
the network is turned off, or you turn the setting off in Settings → Machines
or with `kybern machines reachable off`. Such a daemon is never stopped by
the idle exit. The setting is `network.keep_reachable` (default on) and is
per machine, like the rest of the `network` section.

After the computer wakes from sleep, or when Tailscale shows a machine coming
back, the daemon retries its machines at once instead of waiting out its
reconnect backoff. `kybern machines doctor` reports whether the login item is
registered, and on Linux a Kybern service unit that lacks `OOMPolicy=continue`
(without it, one agent running out of memory stops the daemon and every other
agent; the daemon adds a drop-in for older units when it starts).

## Accounts and continuing conversations

Add accounts in Settings → Agent providers → Accounts. Each named account uses
its harness's own sign-in and session storage. Create a new directory or reference
an existing native account directory, then finish sign-in in the provided terminal.
The default account keeps using the regular CLI configuration.

Choose a global account for each harness, optionally override it for a project,
and use the conversation's account picker for a thread override. Threads that
follow defaults resolve the account on their next message. Running turns and
queued messages retain their admitted account; background services keep their
original owner. Model favorites and recent choices belong to each account.

Changing an account or harness keeps the same Kybern conversation, draft, tasks,
files, and checkout. A compatible native session resumes with the conversation
it missed. Otherwise Kybern opens a native session with bounded, attributed
conversation context and marks the transition. Native child processes, hidden
reasoning, and tool state cannot be transferred between harnesses or accounts.
When a confirmed 5-hour or weekly limit interrupts a task, **Switch and continue**
lets you choose another account explicitly; accounts never rotate automatically.

```sh
kybern accounts create --provider claude-code Work
kybern accounts list --provider claude-code
kybern accounts sign-in --provider claude-code <account-id>
kybern accounts default --provider claude-code <account-id>
```

## Reviews and visual replies

Open a pull request from the Pull requests page or its linked conversation's
Review dock. Review files, checks, and comments, keep drafts across navigation,
and submit comments, approvals, or requested changes explicitly. Checkout, merge,
and close require deliberate actions. Inline drafts stay tied to the commit you
reviewed; a changed head must be reviewed before sending. **Send to agent** uses
selected findings and preserves its repair conversation when a retry is needed.

Ordinary thread worktrees can be inspected and removed from their thread menu.
Active work, shared ownership, terminals, external processes, and ignored files
are protected. Confirmed source recovery retains tracked and untracked files;
continuing the conversation recreates its own worktree. Automatic cleanup is
off by default and applies only to eligible archived worktrees.

Every harness receives `kybern_html_preview` and `kybern_html_publish`. Preview
returns a screenshot and console diagnostics; the first request installs a pinned
headless browser. Publication needs no browser and stores a durable inline visual,
embedding supported absolute local image references. Pages run scripts in an
isolated sandbox, follow the app theme, and pause when hidden. Readers can expand
the visual, inspect its source, or save HTML. This release adds the desktop UI;
mobile feature parity follows separately.

```sh
kybern visuals preview <thread-id> visual.html --screenshot preview.png
kybern visuals publish <thread-id> visual.html --title "Scenario comparison"
kybern recover-omp-answer <thread-id> <turn-id>
```

The OMP recovery command appends a correction only when the original native
session matches the saved text, reasoning, and tool order exactly. It preserves
the original event log and separates progress from the exact final answer.


## Connect to another machine over Tailscale

With Tailscale installed and connected on the VPS and your desktop or phone,
the quickest route is to let the daemon listen on its Tailscale address:

```sh
kybern pair --tailscale
```

This opens a listener on the VPS's Tailscale IP next to the loopback one,
remembers the choice in `settings.json` (`access.tailscale`), and prints a QR
code the Kybern mobile app can scan. The desktop app offers the same switch,
**Reachable over Tailscale**, in **Pair a device**. Traffic stays inside the
tailnet's WireGuard tunnel; nothing is exposed on the public interface.

For a browser-friendly HTTPS address instead, configure
[Tailscale Serve](https://tailscale.com/docs/reference/tailscale-cli/serve)
on the VPS to forward to Kybern:

```sh
tailscale serve --bg http://127.0.0.1:4173
kybernd --port 4173 --pair
```

`kybernd --pair` starts the daemon and prints:

- A six-digit pairing code, valid once for ten minutes.
- A QR code of the invitation when a reachable address was detected.
- Detected addresses, including a matching Tailscale HTTPS proxy when configured.
- A complete invitation for each address, containing the code and environment identity.

In the desktop app, choose **Switch environment → Add environment**, enter a
name, and paste the complete invitation into **Address or pairing invitation**.
You can also enter its address and code separately. Keep the daemon running.
Create a separate code for each client.

If the daemon is already running, generate a new invitation over SSH with:

```sh
kybern pair
```

Do not start a second daemon against the same data directory. If you installed
only `kybernd`, its `--pair` option is for startup; use the optional CLI to pair
more devices while work stays running.

Discovery checks active network interfaces and the installed Tailscale CLI.
It prefers a matching HTTPS Serve proxy, and lists direct Tailscale, private,
and public interface addresses only when the daemon listens on them. It never
opens a firewall, enables Serve, or guesses that a public NAT address accepts
inbound connections. With the default loopback listener and no proxy, it prints
local-only addresses and tells you to run `kybern pair --tailscale` or
configure a proxy or SSH tunnel.

## Connect through an SSH tunnel or another HTTPS proxy

For an SSH tunnel, start the daemon on the VPS:

```sh
kybernd --port 4173 --pair
```

On your desktop, keep this tunnel running:

```sh
ssh -N -L 14173:127.0.0.1:4173 user@your-vps
```

Use **http://127.0.0.1:14173** and the code printed on the VPS. The tunnel's
local port belongs to your desktop, so use that address instead of the VPS's
printed loopback address. A phone needs its own reachable route; it cannot use
a tunnel running only on your desktop.

For an existing HTTPS reverse proxy, supply its public address explicitly:

```sh
kybernd --port 4173 --advertise-url https://kybern.example.com --pair
```

This advertises the URL; it does not configure the proxy or TLS.
