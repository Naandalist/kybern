# Changelog

## 0.21.6

<!-- kybern-release-title: Faster typing on Windows, Oh My Pi without sign-in -->
<!-- kybern-release-summary: Typing on Windows no longer lags or shows a timeout error, Oh My Pi works with whatever providers you set up in it, and the mobile app reaches your computer again after an update moves it to a new port. -->

- On Windows, typing no longer stops for seconds or shows "Timed out after 2 seconds waiting for a window update". Each keystroke used to wait several times for the window to finish drawing, which costs a whole frame while an answer streams in; a busy moment longer than 2 seconds also threw that error. Keys now land without waiting, and a busy window catches up instead of failing.
- Oh My Pi, OpenCode and pi no longer show a sign-in. They use the providers you set up in them (Google Antigravity, Claude, ChatGPT, an API key…), so Kybern lists their models and never asks you to sign in. Oh My Pi signed in only through Google Antigravity used to look signed out, so first-run setup offered Claude Code as the only ready agent. Settings → Accounts no longer lists these agents, unless you added an extra profile for one before.
- The mobile app reaches your computer again when it moves to a new port, for example after an update. It used to keep retrying the old address, showed the computer as Offline, and Home showed "Open connections" without your spaces. It now tries the computer's other addresses, checks each one is really your computer, and keeps the one that works. Home stays usable while any of your machines answers.
- Manage machines on the mobile app no longer opens a blank screen or closes the app when one of your machines is offline.
- A daemon that is shutting down closes its network ports first, so a shutdown that hangs can no longer keep the newer daemon off its usual port.

## 0.21.5

<!-- kybern-release-title: Tidier previews -->
<!-- kybern-release-summary: Closing a Browser tab closes its page and the port Kybern forwarded for it, and dev servers on Windows show up in the servers list. -->

- Closing a Browser tab now closes its page. A new tab starts at the servers list, and a port Kybern forwarded for that page stops about 10 seconds later. Ports you forward yourself stay open.
- Dev servers on a Windows machine show up under Servers on all machines. A Vite or Node server there often listens only on the IPv6 loopback, which Kybern did not check, so it could be opened but was never listed.

## 0.21.4

<!-- kybern-release-title: Fewer password prompts, every machine up to date -->
<!-- kybern-release-summary: macOS stops asking for your keychain password after every update, Update all machines now covers computers running the desktop app, and your other machines reconnect on their own, and previews from other machines cope with slow connections. -->

- On macOS, updating Kybern no longer asks for your login keychain password every time. Your machine logins now live in one Keychain item, read by a small helper that stays the same from release to release. The first time 0.21.4 opens, macOS may ask a few times, once for each machine login you had saved, naming "kybern-keychain"; choose Always Allow each time. After that it stops asking. macOS may also ask once more for notification permission. If you go back to 0.21.3 after this update, pair your machines again.
- Update all machines and Keep all machines on the same version now update computers that run the desktop app, not only the ones that run just the daemon. When Kybern is open there, it installs the update and restarts by itself once nothing is running. If someone used that window in the last couple of minutes, it asks first: Restart now or Later. An update that follows another machine also waits until that computer's Kybern terminals are closed. When only the daemon runs, it updates the installed app itself. A Linux .deb or .rpm install shows what to run instead.
- When another machine stops accepting this computer's saved login, Kybern asks for a new one by itself. Before, the machine stayed offline in Settings, its threads failed to open, and only pairing again helped.
- Opening a thread on another machine connects to it first instead of saying "This machine is offline", and Settings → Machines shows a machine the network reports online as Online, with "connecting…" while this computer reaches it.
- An old saved entry for this same computer no longer shows up as a second, unpaired "This Mac".
- A machine removed from your network while it was offline learns of it the next time it reaches another machine, then leaves the network and stops starting Kybern at login.
- A computer that wakes from sleep reconnects to your other machines again. Before, one connection attempt cut off by sleep or a network change could stop it from reconnecting to any machine until Kybern's daemon restarted. Settings → Machines and the machine picker now say "this computer's link is reconnecting" when the app reaches a machine but this computer's daemon does not yet.
- Previews of a dev server on another machine handle slow connections better. A page that is still loading over a slow link says so, with how much is left, instead of staying blank. Text is compressed on every forwarded page, not only in Fast mode. A reload asks the other machine in one round which files changed and fetches only those. When another of your machines reaches both ends much faster than the direct route, the preview goes through it and the address bar says which one; that machine has to prove who it relays for before any page data passes.

## 0.21.3

<!-- kybern-release-title: Up-to-date status for your other machines -->
<!-- kybern-release-summary: Agents on another machine show when they finish, machines that come back online show as online, and images in replies load from any folder. -->

- Agents you delegate to another machine show Done, Failed or Stopped as soon as they finish. Before, their rows could say "Working" for hours, because the app had no reliable way to hear that an agent on another machine had ended.
- The sidebar shows agents started on another machine after it was offline for a while. Before, it could keep the list from before the machine went away, with newer agents missing.
- A machine that comes back online shows as Online in the machine menu without restarting the app, and "last seen" stays current while a machine is connected. Kybern now reconnects to a machine as soon as its daemon reports it online.
- Images in agent replies load from any folder on the machine, including other checkouts, worktrees and folders a finished worktree left behind. Kybern still checks that the file is a real image under 50 MB, and only your paired devices can read it. Files on network shares and network drives are not loaded, nor are system device folders such as `/dev`. A disk that stops answering gives up after 15 seconds.
- Images keep loading after a disk cleanup removes Kybern's image cache. Before, every image in a reply said "Unable to load the image" until the app restarted.

## 0.21.2

<!-- kybern-release-title: Fast previews from your other machines -->
<!-- kybern-release-summary: Dev servers on another machine preview several times faster, machines stay reachable with Kybern closed, and saved Antigravity conversations import. -->

- Previews of a dev server on another machine load much faster. Forwards keep connections ready, so opening a page no longer waits on extra round trips. On slower routes (over about 40 ms), previews switch to Fast mode: HTTP/2 over `https://localhost`, with every request sharing one connection and text compressed on the way. In our tests, a 360-module Vite app over a 213 ms route loaded in under 6 s in Kybern's window, down from 19 s. Hot reload, WebSockets and server-sent events keep working. The Fast chip in the address bar switches a preview back to plain HTTP, for apps that expect `http://localhost` in redirects or API calls.
- Fast mode uses a certificate made for your install that only Kybern's own web views accept. It is never added to your system's trust store.
- A thread on another machine opens its own localhost server through the same fast forward.
- Forwards recover by themselves when the other machine's daemon restarts. Before, they failed until they sat idle for 30 minutes.
- Machines in your network stay reachable when Kybern is closed. While a machine is in a network, Kybern starts in the background when you sign in: a scheduled task on Windows, a LaunchAgent on macOS and a systemd user service on Linux. Turn it off in Settings → Machines → Keep this machine reachable when Kybern is closed, or with `kybern machines reachable off`.
- A machine that wakes from sleep reconnects to the others within seconds, instead of waiting out its retry delay.
- On Linux, an agent or build that runs out of memory no longer stops Kybern's daemon and every conversation with it. Existing installs are fixed on the next start, and `kybern machines doctor` reports one that isn't. A conversation cut off by a daemon stop now says so, instead of showing a raw diagnostic line.
- Saved Antigravity conversations show up in the first-run scan and in session import, grouped by project, with the same tool cards as a live conversation. Kybern only reads Antigravity's files.
- Long links and keys in a message wrap inside the bubble, instead of running past its edge and leaving empty space below.
- The Running, Failed and Passed filter on the Actions page, and the dock's project switcher, keep their highlight on the selected option when you resize the pane.
- After a few messages, Kybern asks once whether you'd star it on GitHub. If the GitHub CLI is signed in, Star on GitHub stars it right away; otherwise it opens the page. If you have already starred Kybern, you won't see it, and closing it hides it for good.

## 0.21.1

<!-- kybern-release-title: Clearer model picker and agent settings -->
<!-- kybern-release-summary: Antigravity's Gemini models get an effort slider, the model picker fits every agent, and Settings says what each agent actually needs. -->

- Antigravity lists Gemini 3.8 Flash once, with a Low, Medium and High effort slider, instead of one model per level. Threads that picked a level before keep working and follow the slider.
- The model picker fits every agent beside a selected account's name, so the Antigravity tab no longer slides under the reload button. With more agents than fit, the strip scrolls.
- Choosing an agent that isn't set up in the model picker opens its setup in Settings, and an account that needs signing in opens the sign-in sheet. Before, nothing happened.
- Settings → Agent providers says what is true for each agent: where it runs from, when its update was last checked, or why it can't run. Cursor shows an older installed SDK as an update with an Update SDK button, rather than as not installed. "Update now" is now "Check for updates", and appears only where Kybern can run the agent's updater. The list follows a running update on its own, so the Refresh row is gone.
- Kybern tools for Antigravity are on by default for new installs. Turning them off still removes Kybern's entry from Antigravity's MCP servers.

## 0.21.0

<!-- kybern-release-title: Antigravity, and computer use on Windows and Linux -->
<!-- kybern-release-summary: Run Google's Antigravity as an agent, let agents use apps on Windows and Linux, and reach every machine in your network from the mobile app. -->

- Antigravity joins Kybern as an agent. Kybern runs Google's `agy` CLI, streams its replies and tool calls, resumes conversations, lists its models and shows its usage limits. Antigravity runs only in Full access, and the composer says so. Sign in with a Gemini API key, or with Google after a notice: Google says using Antigravity through other apps on a personal plan may get the account suspended. Turn on Kybern tools in Settings → Agent providers → Antigravity to give its conversations notes, tasks, delegation and computer use. Onboarding offers it with the official install command.
- Agents can use apps on Windows and Linux through agent-desktop, which Kybern installs and checks for you. On a Mac, agent-desktop is an alternative to CuaDriver: choose it in Settings → Computer use → Driver. Linux supports GNOME on X11 and Wayland; KDE and other desktops are best effort and need assistive technologies turned on. Conversations on a machine without computer use can use the apps on another machine in your network. Codex gets Kybern's computer tools on Linux, where OpenAI's plugin isn't available.
- Claude Code conversations identify as Kybern, instead of using the label that others measured using up the 5-hour limit about twice as fast for the same tokens.
- The mobile app reaches every machine in your network. Machines that are only reachable through a relay now connect, along with their images, attachments, uploads, previews, visuals and artifacts. The mobile update needs your machines on 0.21.0.
- On mobile, Settings → Machines can rename and pause machines, check the network, update all machines, retry or cancel queued messages, and show each machine's specs and agents. Threads say which machine they run on, and offer Fork to and Continue on another machine, including setting up the project there.
- The mobile app connects to machines other than the one it's paired with. Since 0.20.0 it couldn't: those machines always showed as offline. If the paired computer isn't answering, another of your saved computers can show your network instead.
- The iPad sidebar is compact again, with smaller rows, filter chips and Settings in the footer.

## 0.20.6

<!-- kybern-release-title: A tidier Pull requests header -->
<!-- kybern-release-summary: The Pull requests and Actions headers keep their tabs readable in a narrow list, and Kybern's release builds move to GitHub's macOS 15 machines. -->

- In a narrow list column, the project and repository pickers no longer cover the Actions tab. Below the default width they become a folder button that opens the same menu, sized and spaced like the buttons beside it, and the list column's minimum width is now 304 pt.
- The run-workflow button's triangle is balanced with the icons beside it.
- Kybern's Apple Silicon builds use GitHub's macOS 15 machines, ahead of macOS 14's retirement.
- An Indonesian README with a language switcher, thanks to @gianabif14. The Android APK links in both READMEs point to the GitHub release.

## 0.20.5

<!-- kybern-release-title: GitHub Actions in Kybern -->
<!-- kybern-release-summary: Follow and control your GitHub Actions runs from the Pull requests page, open dev servers on other machines faster, start Cursor threads faster, and choose a Stable or Dev update track. -->

- The Pull requests page has an Actions tab for the repository's GitHub Actions runs, grouped by branch with their pull requests. A run shows its job graph with the failure path highlighted, and each job's steps beside its log. Rerun all or failed jobs, rerun one job, cancel, or start a workflow with its inputs, and send a failed job to an agent with Fix with agent. Kybern lets you know when a run you started, or one on your current branch, finishes.
- Dev servers on another machine open much faster: port forwarding uses direct Tailscale connections when both machines support them, and keeps the existing tunnel for older or relayed machines.
- Pasting an image into a thread on another machine works, as do attachment previews, Space logos, thread images and note moves for that machine.
- Artifact previews, live visuals and Preview for a thread on another machine load that machine's pages, including Open in browser.
- Delegated agents of a thread on another machine nest under it in the sidebar and open on the right machine.
- Cursor threads start much faster: the next turn and new conversations reach the first reply in about 2 seconds instead of 8–25 seconds.
- Cursor's long-context models, such as Claude at 1M and Grok at 500k, start instead of failing with “AI Model Not Found”. Kybern sends only the settings you chose, and a rejected selection says which setting to change.
- Choose an update track in Settings → About: Stable, or Dev for early builds. The choice is shared across your machines, new installs start on Stable, and switching back to Stable never installs an older version.
- Fixes a macOS freeze where Kybern stopped responding with a spinning cursor.
- Thread history reads answers that contain only thinking instead of failing on them.
- Toasts and the update announcement sit in the window's bottom-right corner instead of floating above the composer.
- A redesigned README with a quick install matrix. Thanks to @gianabif14, Kybern's first outside contributor.

## 0.20.4

<!-- kybern-release-title: Spaces and previews across your machines -->
<!-- kybern-release-summary: Keep Spaces in sync across your network, open dev servers from any connected machine, and move smoothly between pull request tabs. -->

- Spaces now sync across connected machines, including names, colors, logos, order and project placement. Existing Spaces combine into one shared rail; offline changes catch up on reconnect, and new clones inherit their repository's Space. Update each machine to enable syncing.
- Preview discovers dev servers across all connected machines, shows their machine names and puts the current project's servers first. Click a remote server to forward and open it automatically. Recents remember the original machine and offer Retry when it is offline.
- Open other pull requests from the horizontal tab's + menu using search or a GitHub URL for a known project, or choose Open in tab from the pull request list. Grouped repositories and remote projects keep their own PR identities.
- Pull request tabs scroll and switch files much more smoothly. Each tab restores its selected view, file and exact reading position, including after its cached data expires.
- Fixes a race that could fail a queued delegation just as its repository finished copying to another machine.

## 0.20.3

<!-- kybern-release-title: Open remote terminals with the mouse -->
<!-- kybern-release-summary: The New terminal on… submenu stays open when clicked, so you can select another machine and open its terminal in the current thread. -->

- Fixes the “New terminal on…” machine submenu closing immediately after a mouse click. You can select a machine and open its terminal in the current thread without using the keyboard workaround.

## 0.20.2

<!-- kybern-release-title: Machine setup finishes without an extra retry -->
<!-- kybern-release-summary: After adding a project to another machine, Kybern waits for the refreshed project list before selecting it, avoiding an unnecessary Retry selection error. -->

- Fixes successful project setup showing “The project list has not refreshed yet” when another refresh was already running. The composer now waits for the updated project list and selects the destination automatically, preserving your draft.

## 0.20.1

<!-- kybern-release-title: Set up your machines with fewer steps -->
<!-- kybern-release-summary: Connect trusted machines automatically and set up missing projects directly from the composer, with clear progress and retries. -->

- Turning on machine networking immediately refreshes connected machines and login broker choices, without restarting the desktop.
- Automatically connect machines through saved pairing or verified SSH access while preserving machines you explicitly turned off. Install and connect offers verified Kybern installation on supported macOS and Linux machines.
- Set up a missing repository or incomplete project group from the composer on one or all missing machines. Choose destination folders and env files, follow each machine's progress, and retry failures without repeating successful work.
- Platform logos, status badges and a source-to-destination diagram make setup easier to scan. Queued work, drafts and destination agent accounts stay intact through setup and retries.

## 0.20.0

<!-- kybern-release-title: Your machines, together in Kybern -->
<!-- kybern-release-summary: Connect your machines over Tailscale, see their projects and chats together, and send work to the machine you choose. Turn it on in Settings → Machines. -->

- Connect your machines over Tailscale and see their projects, chats, notes and tasks together. Networking stays off until you turn it on; a one-time introduction explains it, and Settings → Machines lets you add, pause, remove and choose permissions for each machine.
- Pick a machine when starting a chat, delegate an agent to another machine, or fork and continue a conversation there. Work for an offline machine waits in a queue, and remote agents show their machine and live progress beside the original conversation.
- Add a repository or a group project to another machine by cloning or sending it directly. Partial groups offer only the missing repositories, preserve existing branches and edits, and resume work that was waiting for the project. Choose which env files to transfer.
- Open terminals and preview dev servers on other machines from the desktop app. Closing a remote terminal tab closes its shell, Windows terminals report when their shell exits, and rapid terminal input arrives in order.
- Send files between machines and choose when to send, paste or automatically share clipboard text. Automatic sharing filters secrets and password-manager content.
- Compare user skills and plugins across machines in Settings. Review a copy before applying it, confirm replacements, and queue copies for an offline machine. Plugin copying uses each supported harness's installer; local sources and unsupported configurations stay visible with a reason they cannot be copied.
- Choose a machine to lend supported agent logins to the others, and apply supported settings across machines. Provider keys are stored encrypted; removing a machine or turning networking off ends its borrowed sessions.
- Preserve saved connections and account settings when migrating to the machine network, including when returning to an older app version.
- Includes the recent macOS fixes for stalled drawing and unresponsive typing, plus the refreshed Windows WebView2 installer.

## 0.19.5

<!-- kybern-release-title: Typing and clicks stay responsive -->
<!-- kybern-release-summary: Fixes macOS UI freezes where sidebar highlights, clicks and composer typing stopped updating until you switched desktops. -->

- Fixes macOS UI freezes where sidebar highlights, clicks and composer typing stopped updating until you switched desktops. Waiting for a new frame no longer blocks the UI thread, and the current view draws as soon as a frame is available.

## 0.19.4

- The desktop app recovers automatically when its animation-frame subscription stalls, fixing chats that would not open and a thinking shimmer that stayed until the window was minimized and restored.

## 0.19.3

<!-- kybern-release-title: The window keeps responding when drawing stalls -->
<!-- kybern-release-summary: When macOS stops handing Kybern new frames to draw into, the window stays responsive and recovers instead of freezing until you move or minimize it. -->

- Fixes a freeze where the whole window stopped updating until you dragged it, minimized it or switched desktops. When macOS held on to every frame Kybern draws into, Kybern waited about a second for each new frame, so typing and clicks stalled with it. It now skips those draws, asks macOS to release the frames, and draws the current view as soon as one is free.
- Each such episode is written to `gpuix-frames.log` in the Kybern data folder as `DrawableStarved`, with how it ended. `KYBERN_DRAWABLE_GATE=0` turns this off.

## 0.19.2

<!-- kybern-release-title: Recovers when the window stops drawing -->
<!-- kybern-release-summary: When a Kybern window stops updating, it now recovers on its own within a moment instead of waiting for you to move or minimize it. -->

- When a Kybern window stops updating (it used to come back only after you dragged it, minimized it or switched desktops), the app now notices within a quarter second and recovers on its own. It reattaches the window to its screen's frame timer, restarting or replacing the timer if needed, and redraws a frame that was dropped.
- Each recovery is written to `gpuix-frames.log` in the Kybern data folder, so a freeze that still happens can be traced. `KYBERN_FRAME_WATCHDOG=0` turns the watchdog off.

## 0.19.1

<!-- kybern-release-title: Terminal, branches and images fixed -->
<!-- kybern-release-summary: The terminal takes clicks, typing and drag selection reliably, the branch picker lists remote branches with search, and images in chats load on Android. -->

- The terminal tab focuses on any click, so typing reaches the shell every time, and dragging selects across lines. Fast typing no longer arrives out of order or gets dropped.
- The branch picker for a new chat lists remote branches, fetches new ones from the remote when it opens, and has a search field. Starting a chat on `origin/feature` in the checkout makes a local branch that tracks it.
- Tree lines (tool groups, runs, subagents in the sidebar) show on 1x displays such as external monitors, where only the last corner used to appear.
- In the image viewer, the zoom, copy, download and open buttons work instead of closing the image. Expanded Read results no longer show the image's base64 text, which made image-heavy threads slow.
- A click anywhere on a search field focuses it, including its magnifier icon (the pull request list search).
- Computer use recovers by itself when CuaDriver ends its connection's session, instead of failing until restarted and reporting missing permissions.

## 0.19.0

<!-- kybern-release-title: A clearer pull request page -->
<!-- kybern-release-summary: Pull requests now show one clear verdict with the next step, a list grouped by what needs you, a full file tree, and one timeline of everything that happened. -->

- Pull requests show one verdict at the top of the sidebar, such as "Ready to merge", "Waiting on 6 checks" or "2 blockers", with the next step and the one button that does it: Squash and merge, Merge when ready (auto-merge), Update branch, Resolve in conversation, or Ready for review. Running checks are no longer reported as failing. Merge when ready only appears while GitHub is waiting on required checks, so it never merges at once, and merging past unfinished or failing checks asks first.
- The sidebar stays in place while you read. Checks are grouped by workflow with required marks, shard progress and run time, failures first; each failing check and each change request has an agent button. Reviewers and assignees show their GitHub avatars and review state.
- The pull request list is grouped into Needs attention, Awaiting review, Approved and Drafts, and each row says why. In a group project, the filter lists member repositories, and repository chips switch to one of them.
- The Changes tab lists every file in a full-height tree (or a flat list) with a filter, loads all pages in the background, and never cuts off line counts.
- The Timeline tab is one history in time order: comments, reviews with their inline comments, commits and force pushes, with filters.
- The header keeps Review and a "⋯" menu; the description reads at a comfortable width.
- New `kybern pr merge` options (`--method`, `--admin`, `--auto`, `--cancel-auto`) and `kybern pr update-branch` / `kybern pr ready`.
- The retired Tauri desktop app is gone from the repository and the release; Tauri installs stay on 0.18.0, so install Kybern GPUIX to keep updating.

## 0.18.0

<!-- kybern-release-title: OpenCode 2 support -->
<!-- kybern-release-summary: Kybern now drives OpenCode 2 alongside OpenCode 1, including permission prompts, compaction, fork and resume, and importing your past OpenCode sessions. -->

- Kybern works with OpenCode 2. It detects the installed version and keeps OpenCode 1 working; on OpenCode 2 you can also change a running session's permission mode and steer a running turn. Versions it can't drive yet show a reason in the agent list instead of failing when a chat starts.
- Import past OpenCode 2 sessions from onboarding or `kybern sessions import`.
- The GPUIX welcome changes steps without flashing and opens straight into the app.

## 0.17.0

<!-- kybern-release-title: A welcome that sets up your agents and brings in your past work -->
<!-- kybern-release-summary: New installs of Kybern GPUIX open on a short welcome that finds your coding agents, imports your past chats by project, and sets your theme, window style and approval mode, then opens straight into the app. -->

- New installs of Kybern GPUIX open on a welcome that finds your coding agents and helps you install or sign in to them. Every signed-in agent works side by side; you pick which one new chats start with.
- Bring in your past work: Kybern reads your Claude Code, Codex, Cursor, Oh My Pi and pi history, groups it by project, and imports the latest 50 chats, every chat, or just the projects. Imported chats keep their dates, and anything skipped says why.
- Choose your theme, a window style (Solid, Translucent sidebar or Full translucency) and when agents ask for approval before you start.
- `kybern sessions scan` and `kybern sessions import` find and import past agent sessions from the command line.

## 0.16.1

<!-- kybern-release-title: Kybern on Windows finds Codex and your other agents -->
<!-- kybern-release-summary: Kybern on Windows now finds agent CLIs installed with npm, Scoop, pnpm, Volta or nvm-windows, even when the app started before you installed them. -->

- On Windows, Kybern now finds Codex, OpenCode, pi and other agents installed with npm, Scoop, pnpm, Volta or nvm-windows, plus Node.js for Cursor. It reads the same Path a new terminal gets, so agents no longer show as "Not installed" when your terminal runs them fine.

## 0.16.0

<!-- kybern-release-title: Kybern GPUIX on Windows and Linux, and a Coordinator that keeps its facts straight -->
<!-- kybern-release-summary: Install Kybern GPUIX on Windows and Linux with the same looks and features as on the Mac, and keep long Coordinator projects accurate with context refreshes, change reports and dated notes. -->

- Install Kybern GPUIX on Windows 10 and 11 (per-user installer) and on Linux (AppImage, .deb and .rpm), with the same looks and features as on the Mac. It updates itself on every platform.
- See interactive visuals, artifacts, Mermaid diagrams and your dev server's preview inside the app on Windows (WebView2) and Linux (a bundled, sandboxed WebKit) instead of in the browser. On Linux, pages that can't be sandboxed, such as inside a Snap or a container, open in your browser.
- Use native window controls: Windows caption buttons with Snap Layouts, Aero snap and the window menu, and Linux title-bar buttons in your desktop's order and side. Kybern reopens where you left it, on the same monitor.
- Use Ctrl shortcuts on Windows and Linux, with "Ctrl+…" in menus and tooltips. In the terminal, Ctrl and Alt go to the shell and copy, paste and find move to Ctrl+Shift+C/V/F. AltGr types its character on European keyboard layouts.
- Get the Mac's glass on Windows 11: Mica behind the window in Kybern's light or dark theme, and frosted panels on Windows and Linux. Kybern follows Transparency effects, contrast themes and Animation effects while it runs.
- Read the same text everywhere: Segoe UI Variable on Windows 11 and Inter elsewhere, with every weight, JetBrains Mono for code, and colour emoji such as ❤️ and ⚠️.
- Get notifications when an agent finishes or needs you on Windows and Linux; clicking one opens its thread.
- Keep long Coordinator projects accurate: when the Coordinator's context fills up, it writes a checked handoff (goals, decisions, removals, your preferences) and continues in a fresh session in the same thread. Read or correct the handoff, and refresh by hand with **Refresh context**. Set the budget in Settings › Delegation or per project.
- Tell the Coordinator what changed: each turn starts with the commits, deletions and renames since its last turn, so it stops suggesting code its own agents removed. It checks code before making claims about it.
- Give the Coordinator a project map built by Kybern (layout, entry points, build and test commands, recent removals), shown in a Map tab next to Notes.
- Keep project notes as dated history: notes have categories (Preference, Decision, Convention, Gotcha), claims cite commits, notes whose files changed are flagged stale, and **Tidy notes** offers a cleanup for you to approve.
- Expand a visual that's cut off with **Show more**, or open it with **Full screen** (⇧⌘↵). A question stays answerable in the full-screen view.
- See and open the pull requests of every repository in a group project.
- Fix the Kybern desktop app replacing a newer running daemon with its own older one on restart.
- Fix the daemon crashing on Windows while it installed the preview browser.

## 0.15.1

<!-- kybern-release-title: A redesigned Kybern mobile, and agent results that arrive on time -->
<!-- kybern-release-summary: The mobile app is rebuilt to feel native and catches up with desktop; queued agent results no longer wait behind background tasks, and files agents link in dot-folders open. -->

- Get agent results as soon as a turn ends: results from delegated agents, queued messages and messages from other threads no longer wait while a dev server or monitor keeps running in the background.
- Open the files agents link in folders like `.kybern-artifacts/` and `.screenshots/`, in file tabs and in Preview. Files Kybern keeps closed, such as `.git` and `.env`, now say why instead of "File not found".
- Scroll the repository list when you create a group project from a folder with many repositories, in Kybern GPUIX.
- Use a redesigned Kybern mobile (0.1.2): native menus and sheets on iOS and opaque themed ones on Android, menus that grow out of their buttons with liquid motion, and bars that fade into the screen edge.
- Follow work on mobile the way desktop shows it: one-line steps, running processes in one row, an Agents sheet, inline visuals, request and question cards with countdowns, held messages you can deliver or dismiss, and steering for Claude Code and Oh My Pi.
- Switch computers and spaces from the mobile home screen, filter Notes and Tasks by space and project, see threads grouped by project with "Needs input" badges, and use space logos.
- Do more from your phone: pull requests, a task board with batch send, quick file edits, terminal search, dev-server previews, plan usage limits, accounts, computer use status and every Settings page.
- See the new Kybern icon on Android, shaped by your launcher, with dark and themed variants, and the Icon Composer icon on iOS.

## 0.15.0

<!-- kybern-release-title: Work across several repositories in one chat, in Kybern GPUIX and on mobile -->
<!-- kybern-release-summary: Group repositories into one project so an agent can change them together, start new chats the moment you press Enter, and keep your default space safe from deletion. -->

- Work across several repositories in one chat: add a **Group of repositories** from the Projects **+** menu, pick the parent folder and tick the repositories it holds (nothing is ticked for you). Agents work in the group folder, are told which repositories they're in and where each one's instructions live, and commit in each repository separately.
- See every repository a group chat touched: Changes and the edited-files card list files under one header per repository, the branch chip shows `main +2` with each repository's branch in its menu, and **Create N pull requests** opens one pull request in every changed GitHub repository, each linking to the others.
- Run group chats in worktrees: each repository gets its own worktree on the chat's branch, and a chip picks which repositories the chat may change. Kybern copies the group's AGENTS.md, CLAUDE.md, `.claude` and anything you choose under **Also copy into worktrees**, and warns you when a repository left out of the chat changes anyway. Rewind restores every repository together or none of them.
- Share instructions across a group: Kybern offers to create an AGENTS.md and a CLAUDE.md that imports it in the group folder, and **Repositories…** on the project adds or removes repositories later.
- Create and use groups on mobile too: add a group, edit its repositories, pick base branches and repositories for a chat, and review changes and pull requests grouped by repository.
- Start a new chat the moment you press Enter: the thread opens and your message rises into it at once, even while a worktree is still being prepared. If the chat can't start, the message stays with the reason, **Retry** and **Edit**, and retrying never makes a second thread. Same on mobile.
- Rename a project from its right-click menu or the palette (**Rename project…**). Existing worktrees keep working after a rename.
- Keep your default space: it can't be deleted any more (it can still be renamed and recoloured), so deleting your only other space no longer makes every space seem to disappear. **Rename space…** in the palette works even when you have one space.
- Say what to do when a worktree chat starts in a folder without git, and when a message goes to an archived chat.

## 0.14.0

<!-- kybern-release-title: Agents ask for what they need, right in the chat, in Kybern GPUIX -->
<!-- kybern-release-summary: Inline visuals rest as pictures and can ask you to choose, agents ask you to sign in, enter a password, point at something or grant a permission in one card, and pasted keys stay out of the chat. -->

- Read inline visuals as part of the chat: an agent's page now rests as a picture under the reply, with its title and actions on a caption line, and scrolls and fades like any message. Click it, or a button or toggle in it, and it comes alive in place; Esc, Done or scrolling away turns it back into a picture. Tall visuals stop at 480 pt with **Show more**, and **Open in tab** keeps what you changed.
- Answer an agent through its visual: when it shows options ("pick a layout", "tune these, then apply"), choose in the page and press **Send**, skip it, or just type a reply. The agent picks up from your answer.
- Sign in without leaving the chat: when an agent needs gh, vercel, supabase, npm, gcloud, fly, wrangler or eas, a card signs you in through the browser and the agent carries on. A **Sign in** chip also appears on a command that failed because you weren't signed in.
- Enter sudo, SSH and git passwords in a private card that shows exactly which command is asking. Only the real sudo, ssh or git can raise it, Kybern never stores or answers it for you, and an SSH host key always waits for your yes or no. The card goes away if the command gives up first. Commands that need to ask you something else open in a terminal tab.
- Point instead of describing: an agent can ask you to click an element in the Preview or a visual, pick a file or folder, drop an image or capture a window. ⌥-click an element yourself to add it to your message.
- Keep pasted keys out of the chat: paste an API key into the composer and Kybern offers to store it in your project's git-ignored env file, leaving a `$NAME` chip the agent can use.
- Let agents keep going while you're away: a question can carry a default and a calm countdown, which pauses while you're looking at it. Set the longest wait, or turn it off, in Settings.
- See which agent needs you: a question shows **Needs input**, apart from approvals, on its thread and on the thread that delegated it, and the notification names the agent. The delegating agent hears about it too and can answer the question for you; the record says who answered.
- Answer questions in a cleaner card: the question is the title, the first option is ready for the keyboard, previews sit beside the options (or under the focused one in a narrow window), **Something else…** becomes a text field in place, and one button sends. Questions render Markdown, and **Attach** adds an image, a file or a window capture to your answer. Every request card now lines up on the same edges.
- Keep typing when a question arrives: it waits behind a small hint until you pause, and your draft is still there afterwards.
- Grant macOS permissions from one card: when an app needs the camera, screen recording or another privacy permission, the card opens the right pane in System Settings and continues once it's granted.
- Type in the composer after clicking in a visual or the Preview: the keyboard comes back on its own, and Esc in a page hands it back too. No more silent beeps.
- Fix the whole window ignoring clicks after closing an image opened from a file chip with its X button.
- Scroll tall images to the bottom in the image viewer and image tabs; they used to cut off part way down.
- Keep delegated agents in the sidebar after you reopen Kybern while they're still working.
- Fix Claude receiving multi-select answers mangled when an option contained a comma, questions going dead after Kybern restarts (they now offer **Reply in chat**), Cursor questions not showing, and cards giving up after 5 minutes of waiting in Claude.
- Fix the note title, the `@`, `$` and `/` menu, dragged tabs and tasks, and the update card staying opaque on glass, and an empty code chip appearing where a line wraps.

## 0.13.0

<!-- kybern-release-title: Edit files in tabs and start chats from a new home in Kybern GPUIX -->
<!-- kybern-release-summary: Edit files right in their tabs, start new chats from a redesigned screen that picks up recent threads and open tasks, and use split views whose title bars respond again. -->

- Edit files right in their tabs: type straight into any file with live syntax colours, find with ⌘F, Tab and Shift-Tab indenting and paired brackets and quotes. ⌘S saves, an unsaved tab shows a dot and asks before it closes, and when an agent changes a file you're editing, a bar offers **Reload** or **Keep mine**. Markdown previews show your unsaved text. Files over 2 MB and binary files open read-only.
- Open a file in Cursor, Zed, Xcode or another editor on your Mac from the tab's ⋯ menu, at the line you're on; ⌘⇧O opens it in your default editor, which you can set in Settings.
- Read long lines in file tabs without scrolling sideways: code wraps like the chat does, with line numbers kept on each line. Turn it off with **Wrap lines** in the tab's ⋯ menu or in Settings.
- Start new chats from a redesigned screen: **Let's work in** over the project name, which is the project picker, with the composer in the middle of the window. Under it, **Continue** lists your three most recent threads in the project and **Up next** its top open tasks; click a task to send it to an agent with your instructions. When you send, your message rises into the new thread as the composer moves into place.
- Click everything in a split view's title bar again: tabs, **+**, Hand off and each pane's ⋯ menu had stopped responding. ⌘⇧W closes the focused pane, and ⌘W on a split pane's chat tab closes that pane instead of the window. Hand off also responds again in a single pane.
- Stay in your space: Home shows the current space's threads instead of jumping to the last space you were in, and switching spaces keeps you on the page you're on.
- Send several tasks to an agent from one floating composer: a chip lists the selected tasks (remove any of them there) and another picks separate runs or one run.
- Open file links in chats reliably: links go to the right line, links an agent wrote from a subfolder still open, and pasted images open in the viewer.
- Rename notes created inside a thread.
- Fix the Explorer search field overlapping the file tree in long projects, and the sidebar's New thread and filter buttons sitting 2 pt low.

## 0.12.1

<!-- kybern-release-title: Kybern GPUIX stays light through long agent runs -->
<!-- kybern-release-summary: Kybern GPUIX no longer grows with every agent reply: a long session now stays around 300 MB instead of climbing past 700 MB. -->

- Kybern GPUIX no longer grows by tens of MB with every agent reply. It now ships React's production build, which also does less work while a reply streams. In a replay of a real orchestration session, memory levels off around 300 MB instead of climbing past 400 MB within three replies.

## 0.12.0

<!-- kybern-release-title: Tabs, new controls and real glass in Kybern GPUIX -->
<!-- kybern-release-summary: Open files, diffs, terminals, the browser and more as tabs inside a thread, answer agents from clearer approval cards, follow their work in compact rows, and use redesigned controls on a glass window that now really blurs, with far less memory used by images. -->

- Open files, diffs, terminals, the browser, Review, pull requests, notes, tasks, visuals, artifacts and diagrams as **tabs in the thread header**, next to the chat. A single click in Explorer or Changes opens a preview tab (italic) that the next click replaces. Tabs are kept per thread across restarts. ⌘1 goes to the chat, ⌘2–⌘9 to a tab, ⌃Tab cycles, ⌘W closes a tab and ⌘⇧T reopens one.
- Pin a subagent or a delegated agent as a tab from the Agents panel or its row in the chat to watch it live; a delegated agent's tab has its own composer.
- Split tabs as well as threads: ⌘\ and ⌘⇧\ move the active tab into a new group to the right or below, up to four groups, each with its own tabs. ⌘⌥ and an arrow key moves between groups. The dock keeps the lists you open things from.
- Redesign the buttons and controls: one calm, tinted main action per card or dialog instead of a white pill, clear secondary and ghost buttons, disabled buttons that look disabled, one checkbox, radio, segmented control, chip and text field style, and select buttons that show names instead of ids.
- On a glass window, menus, dialogs, sheets and the palette now blur what's behind them, and the strip above the composer, code blocks and your messages follow the glass instead of staying solid.
- Remove the dark band above the composer; text scrolls under it and blurs the same way every time.
- Fade long tab titles instead of cutting them with "…", and separate thread tabs with thin bars.
- Answer agents from clearer request cards: every choice is a numbered row (no more **⋯** menu), the full command shows with each chained step on its own line, **Allow … for this session** says exactly what it allows, and **Decline** lets you tell the agent what to do instead. Press a number, use ↑ ↓ and Return, or Esc to cancel the turn. Every kind of request — commands, edits, plans, app access, computer use, notes and tasks, permissions, secrets and questions — uses the same layout.
- Follow agent work in compact rows: everything an agent does between two messages folds into one line such as "Edited 1 file · ran 2 commands · read 3 files", with failures in red and the current step while it works. Open it to see each step as a file, command or search chip; click a file to open it as a tab. New rows slide in smoothly as the agent works.
- Send a queued follow-up into the running turn with **Steer now** on its row, for agents that accept steering (Claude Code, Codex, Pi and Oh My Pi).
- Mark what's already attached in the **@** and **$** pickers; pick an item again to remove it, and attaching the same file twice no longer adds it twice. Deleting a chip in the composer no longer deletes the one next to it.
- Zoom into images: tall screenshots open fitted to their width and scroll, double-click switches between fit and 100%, and ⌘+, ⌘−, ⌘0, ⌘-scroll and dragging zoom and pan, in the viewer and in image tabs.
- Use far less memory for images in replies and attachments: they're decoded at the size they're shown and the viewer opens the original, so a long session's peak went from about 700 MB to about 300 MB. Kybern GPUIX also writes a small memory log (`memory.jsonl`, a line a minute) so memory spikes can be traced; `KYBERN_MEMORY_LOG=0` turns it off.

## 0.11.1

<!-- kybern-release-title: Caps Lock types capitals in Kybern GPUIX -->
<!-- kybern-release-summary: Typing with Caps Lock on in Kybern GPUIX no longer comes out in mixed case. -->

- Type capitals with Caps Lock on in Kybern GPUIX. Some letters in the composer (c, v, x, y, z), keys typed quickly after them, and text in the notes editor came out lowercase, and the terminal received lowercase letters.

## 0.11.0

<!-- kybern-release-title: Spaces and glass in Kybern GPUIX -->
<!-- kybern-release-summary: Group projects into Spaces, give Kybern GPUIX a frosted glass window, load long histories in one scroll, and get faster CI and a GPUIX that now builds for Linux and Windows. -->

- Add **Spaces** to Kybern GPUIX: group projects into spaces from the rail, each with an image or an icon on a colour. Drag a project onto a space or use **Move to space**, switch with ⌃1–⌃9, and the sidebar, Home, Tasks, Notes, Pull requests and the palette follow the space you're in. Existing projects start in **Personal**; with one space the rail shows only **+**.
- Make Kybern GPUIX translucent: **Settings › Appearance › Window** sets Solid or Translucent per theme, with **Sidebar only** as the default. **Advanced** adds sidebar, content and surface opacity and the desktop blur. On whole-window glass the composer, menus and floating buttons are frosted and cards are raised, and fades melt into the glass instead of ending in a band. Reduce transparency and Increase contrast keep the window solid.
- Load older history in whole turns: threads where agents delegated work now scroll back to their first message in a couple of gestures instead of stopping on a page that showed nothing new, and each page loads more than 10 times faster while the thread is running.
- Put the task page's **Tell an agent** field and its composer in one floating card, with workspace and branch as chips, as in the desktop app.
- Keep **+** buttons and task checkboxes visible without hovering, and select a board card with its checkbox (shift-click for a range) instead of opening the task.
- Keep the last reply above the composer at any composer height, cap the composer at 40% of the window, and drop the empty band above question cards. Images open and close faster.
- Open linked Markdown files rendered, with **Show source**.
- Move Kybern GPUIX to GPUIX 0.11 with one-step Tab focus, and check new GPUIX releases weekly.
- Build the patched GPUIX app for Linux and Windows in a first spike; the plan for both betas is in `apps/gpuix/PORTING-PLATFORMS.md`.
- Run pull request checks in about 5 minutes instead of 11, skip Rust and WebKit checks on GPUIX-only changes, and cache release builds so releases publish sooner.

## 0.10.3

<!-- kybern-release-title: Kybern GPUIX for Intel Macs -->
<!-- kybern-release-summary: Kybern GPUIX now installs on Intel Macs too, and releases build faster. -->

- Add **Kybern GPUIX** for Intel Macs. The installer picks the right build for your Mac, and the current desktop app now offers it on Intel Macs as well: `curl -fsSL https://github.com/Tresnanda/kybern/releases/latest/download/kybern-gpuix-mac-install.sh | sh`.
- Load Kybern GPUIX's notification and web view helpers on macOS 13 and 14; they previously required macOS 15.
- Build the daemon and CLI once per release and reuse them in every desktop app, so releases publish about 15 minutes sooner.

## 0.10.2

<!-- kybern-release-title: Kybern GPUIX comes alive -->
<!-- kybern-release-summary: Smooth 60 fps motion across Kybern GPUIX, a tidier composer tray, images that morph into their preview, Full access that never asks, and faster, steadier sends and checkpoints. -->

- Animate Kybern GPUIX throughout: sidebar, transcript, composer, menus, dock and pages now move smoothly. Loading indicators run on Core Animation at display rate instead of about 5 fps, so an agent that is working costs about as much as an idle window, and hover colours fade instead of snapping.
- Merge the cards stacked above the composer into one tray with sections for subagents, background tasks, helpers and queued messages, with status icons and live times. Queued rows show what you queued instead of "Queued follow-up".
- Draw tree lines between parents and children in the sidebar, the "Worked for" groups, the composer tray and the dock's Agents tab.
- Morph images into their preview and back when you open them, whether they're in a reply, your own message or the composer.
- Show your message the moment you press Enter. A send that fails stays in the chat with **Retry** and **Edit**, and a follow-up never overtakes the message before it.
- Never ask for permission in **Full access**. Harness safety prompts are allowed and noted in the chat; questions, plans and secret requests still wait for you.
- Render long replies in full, with tables, code blocks and images intact. Images in hidden folders such as `.screenshots` now show, and so do images elsewhere on your Mac when the reply names their exact path.
- Turn inline code that names a file in the workspace into a link, underline a multi-word link as one, and show the agent's logo on "Sent to" rows.
- Fix a crash when picking a skill from the `$` menu, and a picked skill, note, task or thread that showed as plain text until you typed again.
- End a turn as soon as the agent's reply is done instead of waiting for the checkpoint. Checkpoints reuse their previous snapshot and skip untracked files of 100 MB or more, so they take a fraction of a second on large repos. Sends no longer wait behind other threads' worktree work.
- Fix a slow memory leak in Kybern GPUIX and release memory while the window is in the background.

## 0.10.1

<!-- kybern-release-title: Kybern GPUIX fixes from daily use -->
<!-- kybern-release-summary: Sidebar hover and clicks behave, New chat opens a free chat, and the usage popover, delegated messages and dock icon match the desktop app. -->

- Keep hover on sidebar rows in Kybern GPUIX. A row redrawn under a resting pointer lost its hover, and clicking a project could pick it up for reordering; a press now becomes a drag only after it moves past the row's edge or more than a few pixels.
- Open a free chat from **New chat** in Kybern GPUIX, as in the desktop app, instead of a draft in the most recent project. The project menu under the composer is now **Chat location** and lists Free chat above your projects.
- Rebuild the context ring's popover in Kybern GPUIX to match the desktop app: prompt cache, context window with a meter, and account limits with meters, reset times and pace. It opens on hover and stays open while the pointer is inside it.
- Shorten long delegated tasks, questions and results in Kybern GPUIX behind **Show more**, show the sender's mark inline in their chip, and keep a message's Copy and Save to note buttons visible while you move onto them.
- Show the task kind icon in Kybern GPUIX's background-task strip, and switch its dock icon to the dark and tinted versions with the system appearance.

## 0.10.0

<!-- kybern-release-title: Kybern GPUIX, a native desktop app -->
<!-- kybern-release-summary: A new native desktop app built on GPUI ships next to the existing one, using about 60% less memory and far less CPU, with a faster, steadier daemon underneath. -->

- Add **Kybern GPUIX**, a native desktop app for Apple silicon Macs built on Zed's GPUI. It does everything the current desktop app does, renders without a web view, and uses about 60% less memory and 20–40% less CPU while scrolling, idling at zero frames. It installs next to the current app as its own app with its own updates: `curl -fsSL https://github.com/Tresnanda/kybern/releases/latest/download/kybern-gpuix-mac-install.sh | sh`. The current desktop app keeps working and updating as before.
- Offer Kybern GPUIX once in the current desktop app after it updates: one click downloads it, checks its signature with the updater key and installs it into Applications. Settings › About keeps an Install or Open button for it.
- Stop an agent's message to another thread from interrupting that thread's running turn. Messages from Kybern itself now queue unless they are explicitly sent to steer, so delegated agents are no longer cancelled when a sibling finishes.
- Start faster: the sidebar's projects arrive before Git is probed, saved threads hydrate alongside the lists, and activity, account and quota reads are no longer repeated or blocked behind each other. A large-history daemon answers `threads.list` in milliseconds instead of seconds.
- Show a `cat > file <<EOF` command as an edit of that file instead of a read of `<<EOF`.

## 0.9.1

<!-- kybern-release-title: A Coordinator that sets itself up -->
<!-- kybern-release-summary: A new Coordinator interviews you to set up its brief and settings, lands work with its own git tool, and follows per-project model defaults. -->

- Let a new Coordinator set itself up. It interviews you in a few rounds, with question cards where the harness has them, while a research agent writes the setup note. Your first message in Draft becomes your first answer instead of the brief. The interview ends with a brief you can reword before saving and the settings it proposes, which you can untick row by row. **Redo setup** in the Project tab runs it again.
- Add Coordinator settings: whether to act or propose before starting work, where agents work, a harness and model for each kind of work, and whether pushing and opening pull requests need your approval. Set defaults in Settings › Delegation and override them per project in the Project tab. When the Coordinator picks a different model, the agent's row shows why.
- Give the Coordinator a git tool instead of a shell: branches, commits and merging an agent's worktree branch run directly; pushing and opening a pull request ask unless you allow them; merging a pull request always asks. Agents still never commit.
- Let the Coordinator ask question cards and use skills, and turn a typed `/skill` anywhere in a message into the skill instead of plain text.
- Keep plan usage rings after the daemon restarts. A ring waiting on a sign-in refresh or a failed read stays visible, dimmed, with when it was last updated.
- Show the last known models to agents choosing a harness when a model list is slow to load.

## 0.9.0

<!-- kybern-release-title: A Coordinator that delegates, and secrets kept out of chat -->
<!-- kybern-release-summary: The project Coordinator now plans and hands work to agents, keeps a shared brief and memory in Notes, and agents can ask for API keys without seeing them. -->

- Rebuild the project Coordinator as a lead chat. It answers quick questions, hands work to agents on any harness, shows them in the Agents tab and never edits files itself. Claude Code, OpenCode, Pi and Oh My Pi block its edits; on Codex and Cursor the header says edits are not blocked. A new Project tab holds your brief, the Coordinator's setup note and the memory notes agents write, with a switch to give every chat in the project the brief. Settings has a limit for active agents per Coordinator.
- Remove the old collaboration groups, assignments and Collaboration pane. Upgrading archives old coordinator chats and their helpers and keeps their brief, setup and project overview as project notes.
- Let agents ask for an API key or token with `kybern_request_secret`. You enter it in a private card on desktop or mobile, Kybern writes it to a git-ignored env file in the workspace, and the agent only learns the variable name. Masked answers are never stored, including Codex secret questions, which were stored in plain text before.
- Keep inline visual replies where they were published while a turn is still running, and stop them bouncing as work rows appear. Pages are no longer clipped at the bottom, stop re-rendering their theme on unrelated changes, and use thin scrollbars that match the app.
- Tell agents to put findings in the conversation, prefer inline pages for substantial reports, and write separate report files only when asked.
- The CLI replaces `kybern collaboration` with `kybern coordinator get|open|switch-harness` and `kybern history search|read`.

## 0.8.1

<!-- kybern-release-title: Switch accounts and expand previews directly -->
<!-- kybern-release-summary: Select CLI Claude directly in the model picker, expand interactive previews over the chat, and share reusable skills across accounts. -->

- Switch back to CLI Claude directly in the model picker, even when another account is the project or global default. Keep account controls available while settings refresh and recover from failed catalog reads.
- Expand an inline interactive preview over the chat without opening the sidebar. Keep its page state and reading position, and return focus to the expand button when it closes.
- Share reusable skills and installed plugin files across accounts for the same provider. Preserve conflicting local files in backups, keep credentials, sessions and connections separate, and discover skills for the account selected in the chat. Remote plugins and services can still require per-account installation or sign-in.
- Detect repositories initialized after a project was added, so Git and worktree controls become available without removing and adding the project again.

## 0.8.0

<!-- kybern-release-title: Preview anything, sign in once -->
<!-- kybern-release-summary: Open mockups and dev servers in an in-app browser, add accounts in one browser step, and review pull requests in a cleaner workspace. -->

- Preview local HTML files and dev servers in the right dock. The address bar takes URLs or file paths, with back, forward, reload, a loading bar, device sizes with rotate and drag handles, and a floating mini player that follows you across threads. The empty state lists local servers and recent pages. Agents can open a preview beside the chat with `kybern_preview_open`. Files outside the project ask once per folder, remote dev servers preview through the daemon, and public sites open in your browser.
- Add an account in one sheet: pick the agent, finish signing in in your browser, and Kybern fills in the email and plan. Paste-code and device-code fallbacks cover remote connections. Settings has a new Accounts tab with colors, usage, default, rename, sign out and remove, and the rail usage rings open an accounts popover. The default account is now called the CLI account.
- Choose the account inside the model picker with one tab per account; the composer trigger shows the account name and a small color dot only when a thread uses a named account. The prompt cache timer and plan usage now share one status glyph.
- Show each Cursor model once. Context, Fast and effort are controls in the picker: ⚡ toggles Fast, effort has a slider with reset, and other options are menus that disable combinations Cursor doesn't offer. GPT, Codex, Kimi and GLM reasoning levels use the same effort slider. Model rows have ⌘1–⌘9 shortcuts.
- Rework pull requests: the PR list is the only sidebar, rows show author avatar, checks and line counts, and each PR has Summary, Changes and Timeline tabs with merge status, reviews and a checks ring. Write inline comments under the line, review from a popover, and send findings to an agent from the top bar.
- Make inline visual replies borderless with an Open in panel button, size them before they paint so nothing jumps, and open them in a Preview panel with rendered and source views instead of a dialog.
- Let one chat claim several tasks at once. The thread header lists the tasks it runs, and claiming a task no longer reopens tasks already handed over.
- Remove the ring around harness marks on subagent badges.

## 0.7.1

<!-- kybern-release-title: A clearer pull request workspace -->
<!-- kybern-release-summary: Keep the PR list beside your review, read changes and discussion separately, and keep agent timers readable. -->

- Keep the pull request list beside the selected review in wide windows, with Overview, Changes and Conversation tabs. Description, checks and reviewers appear in the overview; changed files have their own rail. Narrow windows keep the back action, search and selected-row focus. Review drafts and verified checkout before agent repairs are preserved.
- Keep subagent elapsed times on one line, including long-running tasks. Long titles leave room for status and controls; background status moves to its own line when larger text or a narrow pane needs more space.
- Make inline HTML preview and publication available to Pi, Oh My Pi and dedicated project coordinators. Agents can choose visual replies when they help, without a special prompt.
- Refresh Kybern's guidance when resuming or importing a Cursor conversation, so it describes the currently available tools.

## 0.7.0

<!-- kybern-release-title: Keep working across accounts and agents -->
<!-- kybern-release-summary: Switch accounts in the same conversation, review pull requests in Kybern, and let agents reply with interactive visuals. -->

- Add named accounts for every harness, with global defaults, project overrides, and a choice for each conversation. Switching keeps the conversation, drafts, files, tasks, and checkout together. Compatible native sessions resume; a fresh session receives saved conversation context. When a confirmed 5-hour or weekly limit interrupts work, choose another account and continue explicitly.
- Switch harnesses in the same conversation. Queued messages keep the harness and account selected when they were queued, and background services keep their original owner and controls.
- Message a running Claude subagent from its normal composer. Messages wait for that child's next tool call and show confirmed delivery. If it finishes first, the original message stays available with an explicit Send to parent action. Native subagent rows use their harness logos.
- Review pull requests in Kybern's full page or right dock, with files, checks, review comments, saved drafts, inline comments, checkout, and explicit review and merge actions. Send selected findings to an agent without losing the review.
- Preview and publish interactive HTML replies from every harness. Visuals appear inline, follow the app theme, expand for closer inspection, and support source viewing and HTML export. Published local images remain available after their source files are removed.
- Clean up old thread worktrees with an inspection and recovery path. Active work and shared checkouts are protected; automatic cleanup is opt-in and keeps unmerged branches.
- Apply Claude Full access changes by restarting and resuming the native session when idle, or offer Stop and apply now while work is running. Ordinary Enter sends a new message when only a Claude background service remains.
- Group Cursor model choices that differ only by effort, while keeping context-size and other model variants distinct.
- Fix Oh My Pi progress text appearing in the final answer. An explicit recovery command can repair an existing affected answer when its original native history matches exactly.

## 0.6.3

<!-- kybern-release-title: Agents that delegate and talk to each other -->
<!-- kybern-release-summary: Any chat can hand work to agents on other harnesses, in the same branch or their own worktree, and chats can message each other. -->

- Let any chat delegate work to other agents, on any installed harness and model. Children work in the same checkout and branch by default, or in their own worktree started from your uncommitted changes. Their results come back together in one message, and Stop or archive takes the children with it.
- See every agent a chat started in the Agents tab: delegated agents, each harness's own subagents and older helper threads, with status, branch or shared checkout, files touched, conflicts and results. Several agents started in one turn fold into one card.
- Let chats message each other. A question gets an answer back automatically, a message can reach a running chat right away where the harness allows it, and a message to a chat with broader permissions waits for you to deliver or dismiss it. Mention a chat with @ and its agent knows it can write to it.
- Agents in a shared checkout record the files they edit and are warned when they edit files another agent owns. Agents in their own worktree commit on their own branch, and worktrees are cleaned up once their work is merged.
- Set how many agents one chat can run at once and how deep delegation goes in Settings → Agent providers → Delegation.
- A turn cut off by a Kybern restart now ends as interrupted instead of failed, and messages queued behind it still go out.
- Open each harness's own subagents as read-only child threads, with a live status bar, grouped launch rows and sidebar nesting.
- Go back and forward between views with the title-bar arrows, ⌘[ and ⌘], or mouse buttons. The plain `d` theme shortcut is gone; use ⌘K → Toggle theme.
- Select several tasks to change their status or priority, delete them, or send them to agents in one go. Each task page has a Send to agent field.
- Notes and tasks mentioned in messages render as chips with their live titles, and every agent now learns how to use Kybern's tools (Settings → Threads → Tell agents about Kybern).
- The `kybern` command now ships with the app. Add it to your PATH from Settings → About, and set up Cursor from Settings without the terminal.
- Fix Codex showing as Not installed when it's installed through nvm and the app starts before your shell is ready.

## 0.6.2

<!-- kybern-release-title: Agents can use your notes and tasks -->
<!-- kybern-release-summary: Agents can read your notes and tasks, file follow-up tasks and take a task on, and you can start a task's run from the real composer. -->

- Let agents search and read your notes and tasks, write notes, file tasks and claim a task they're working on. They only fully edit what an agent created, never close a task, and ask first unless the chat has Full access or Auto. Each write shows a card with Open and Undo.
- Start a task's run from the real composer on the task page, with every agent, model, effort and permission choice, skills, mentions and attachments. Follow-ups to a running task can carry images and files too.
- Mention notes and tasks in any chat. The @ menu now has All, Threads, Notes, Tasks, Files and Plugins tabs, and @ works without a project.
- Add Notes and Tasks panels to the right dock, and drop the empty dock from the Notes and Tasks pages.
- Quick-added tasks save when you click away. In the Tasks sidebar, Projects can collapse while pinned projects stay visible.
- Paste or drop images into task descriptions. Images in pasted content are uploaded instead of stored inline, and task descriptions can be up to 512 KB.
- Projects added from the CLI, mobile or another window now appear right away, and the plan usage rings in the rail have more room.

## 0.6.1

<!-- kybern-release-title: Projects in Notes and Tasks, images in notes -->
<!-- kybern-release-summary: Pick a project before you write a note or task, paste images into notes, and keep Claude plan usage current. -->

- Choose a project in Tasks and Notes. Every project appears in the Tasks sidebar, and a task's project dot on a list row or board card opens the project picker. New note can start a note in any project. A "+" in both places adds a project without leaving the page.
- Paste, drop or insert images into notes. They're stored on the machine that keeps the note.
- Fix Claude plan usage staying stale on Macs that still have an old Claude credentials file. When usage can't be read, the usage card now says why and what brings it back. A limit that reset since its last reading no longer shows as 100% left.
- Notes use the whole window, without the chat sidebar, and the note header has more room.
- Fix a scrollbar flashing while moving between plan usage rings.

## 0.6.0

<!-- kybern-release-title: Notes, Tasks and live plan usage -->
<!-- kybern-release-summary: Write notes, turn them into tasks an agent can pick up, and see how much of your Claude, Codex and Cursor plans is left, right in the app rail. -->

- Add Notes: global, project and thread notes, with a gallery and a focused writing page. Mention a note to give an agent its contents. Existing thread notes move over automatically, and deleted notes stay in Recently deleted for 30 days.
- Add Tasks: a list and board with Linear-style statuses and drag and drop. Send to agent starts a conversation from a task with your notes as context, and the task moves to Running and then Needs review on its own. Checklist lines in a note can become tasks.
- See how much of your Claude Code, Codex and Cursor plans is left in the app rail. Hover a ring for each limit, when it resets, and whether you're on pace to run out before then. Limits now refresh within a minute and after every turn instead of going stale, and Cursor's plan usage appears for the first time.
- Run new Cursor conversations on Cursor's official SDK, with streaming, sub-agents, model variants and resume. New chats offer Auto-review or Full access. Set it up with `kybern cursor install` and `kybern cursor login`; it needs Node.js 22.13 or later. Existing Cursor chats keep working as before.
- Find models faster in large catalogs: one searchable picker with recent and starred models, sections by backend, and an effort slider. The composer names the backend when two models share a name.
- Redesign the window: Home, Notes, Tasks, Pull requests and Usage sit in an app rail beside one rounded workspace.
- Fix Oh My Pi background commands staying "running" after they finished, and report timeouts and failed exits as failed.
- Fix conversation titles and commit messages silently using Codex when Claude Code was selected.

## 0.5.9

<!-- kybern-release-title: Stay signed in to Claude and Codex -->
<!-- kybern-release-summary: Kybern no longer cuts off Claude Code or Codex while they save a refreshed login, which signed you out about once a day. -->

- Fix having to sign in to Claude Code again almost every day. Kybern stopped its background Claude processes, which read the model list and usage limits, the moment they answered. Sometimes that was while Claude was still saving the login it had just refreshed, and the login was lost. Kybern now lets them finish and exit on their own.
- Stop agents gracefully on macOS, Linux and Windows, so Codex, which refreshes its ChatGPT login the same way, keeps its sign-in too.
- Leave the daily login refresh to your conversations. While the login is about to expire, the model list and the Usage page use their saved values instead of starting Claude in the background.

## 0.5.8

<!-- kybern-release-title: Claude can publish artifacts -->
<!-- kybern-release-summary: Claude agents in Kybern can create and update claude.ai artifacts, and finished reasoning stops showing as still thinking. -->

- Let Claude agents create and update claude.ai artifacts and read their comments. Claude Code keeps these tools off when an app runs it the way Kybern does, so Kybern now turns them on. They need a claude.ai sign-in and are still subject to your organization's policy.
- Stop showing reasoning as still thinking once the agent moves on to its answer or a tool call, on desktop and mobile.

## 0.5.7

<!-- kybern-release-title: Agents remember how to use your apps -->
<!-- kybern-release-summary: When an app needs a trick, agents save a short note, and the next conversation starts from it. See and edit the notes in Settings → Computer use. -->

- Let agents keep notes on how to use each app. When an app needs a non-obvious approach, like WhatsApp's message box ignoring background typing, the agent saves a short note, and later conversations see it the first time they use that app. Review, edit or delete notes in Settings → Computer use → App notes, or with `kybern computer notes`.
- Get WhatsApp messages and mentions right the first time with a built-in note on how its message box works.
- Fix apps opened moments earlier being treated as unknown during computer use.

## 0.5.6

<!-- kybern-release-title: Computer use is about four times faster -->
<!-- kybern-release-summary: Agents click and type in your apps about four times faster, and waste fewer steps in browsers and after screenshots. Update CuaDriver in Settings → Computer use to get the speedup. -->

- Make computer use about four times faster. A click now takes about a quarter of a second instead of over one, because Kybern shortens CuaDriver's one-second wait after every action. Kybern now installs CuaDriver 0.30.1; Settings → Computer use offers the update when you have an older one.
- Start computer use sooner: mentioning @Computer gets CuaDriver ready while the agent reads your request.
- Help agents finish browser tasks: they now open pages by URL instead of clicking page content, which often ignores background clicks.
- Fix screenshots missing from `act` results when the agent skipped the window report, and element references failing after a screenshot.
- Let agents open apps inside other apps, such as DeviceHub inside Xcode, and explain when a window is on another Space.
- Let @Computer work in a chat that started before computer use was turned on. If computer use is off, the agent says so instead of trying shell commands.

## 0.5.5

<!-- kybern-release-title: OMP works again -->
<!-- kybern-release-summary: OMP threads send normally again with OMP 18.3, instead of stopping with an error about collaboration tools. -->

- Fix OMP threads failing with "OMP did not load Kybern's configured collaboration tools" after updating to OMP 18.3. OMP now hides tools from extensions behind on-demand lookups by default; Kybern's tools stay in its normal tool list.

## 0.5.4

<!-- kybern-release-title: Computer use stays light on memory -->
<!-- kybern-release-summary: CuaDriver now uses about 25 MB instead of hundreds of megabytes or more, and stops on its own when agents are done with it. -->

- Keep CuaDriver's memory use to about 25 MB during computer use. Its on-screen agent cursor could hold over a gigabyte of screen images, so it is now off by default; turn on Show the agent’s cursor in Settings → Computer use to bring it back.
- Stop CuaDriver 10 minutes after agents last use it, and when Kybern quits, instead of leaving it running in the background. A CuaDriver another app started is left alone.

## 0.5.3

<!-- kybern-release-title: Computer use is twice as fast -->
<!-- kybern-release-summary: Agents click through apps on your Mac about twice as fast, the live view costs less while it watches, and quitting an app no longer reports an error. -->

- Make each computer-use click about twice as fast. The agent's cursor still shows where it acts, but no longer waits out a long glide before every click.
- Take the live view's pictures between steps without re-reading the app's controls, so watching an agent work adds less time to each step.
- Report a window as closed when the agent quits its app, instead of an error about a window that could not be read.

## 0.5.2

<!-- kybern-release-title: Computer use stays connected -->
<!-- kybern-release-summary: Computer use keeps working after a quiet stretch, steps that visibly worked are reported as working, and the live view shows each step as a clear caption. -->

- Keep computer use working after it has been idle for a while. Agents were refused with "session has ended" until Kybern restarted.
- Report a step as done when the app visibly changed, instead of marking every step unconfirmed and suggesting a foreground retry that was not needed.
- Show the current step in the live view as a caption on the picture, with the target in bold, shortcuts as keys such as ⌘S, and a check when the agent finishes.

## 0.5.1

<!-- kybern-release-title: Let agents use apps on your Mac -->
<!-- kybern-release-summary: Type @Computer and Claude, OpenCode, Cursor, or Pi can read and use the apps on your Mac, with your approval and a live view of each step. Claude model names load reliably on slow machines, and the sidebar can reorder projects and filter threads. -->

- Let Claude, OpenCode, Cursor, and Pi agents use apps on your Mac. Turn it on in Settings → Computer use, which installs CuaDriver and walks you through granting access, then type @Computer in a chat. You approve each app the first time, a floating live view shows what the agent sees, and password managers, Keychain, and System Settings are always off limits. Codex keeps its own computer-use plugin.
- Show @ and $ mentions in the composer as icons and labels, the same as in sent messages, with undo, redo, and plain-text copy and paste.
- Read Claude models from Claude Code's own model list and keep it on disk, so the picker shows full names like Claude Opus 5.5 even on slow or busy machines, and running threads keep the friendly name. Model descriptions now appear under each name on desktop and mobile.
- Reorder projects by dragging their headers, or with Move up and Move down in the project menu. The order is saved per environment.
- Filter sidebar threads to pinned, working, or a single agent from the button next to Add project.
- Open commands, About, Reload agents, and agent settings from a new help menu in the sidebar footer.

## 0.5.0

<!-- kybern-release-title: Kybern uses far less memory -->
<!-- kybern-release-summary: The desktop app holds much less RAM: hidden windows release their transcripts, long tool results and attachment thumbnails stay small, and idle screens and startup no longer keep extra graphics layers. -->

- Release a window's transcript, highlighting and Markdown caches when the window is minimized, fully covered, or hidden, and restore reading position, drafts, attachments, queued prompts, approvals, and terminals when it comes back.
- Keep open tool results light: very long outputs mount only the lines on screen, and copying the whole result still copies all of it exactly.
- Keep only the 16 most recent per-turn change summaries loaded in an open thread; older turns reload theirs when you scroll back to them.
- Decode attachment thumbnails at thumbnail size instead of full resolution. Opening an attachment still shows the original image.
- Stop idle screens from holding extra graphics memory: the split-pane drop preview now appears only while you drag a thread, and sidebar titles and the project list no longer keep their own layers.
- Show the sidebar and home screen already in place when the app launches, and show connection spinners only when connecting takes a moment, which lowers the memory spike at startup. Switching projects keeps its motion.

## 0.4.17

<!-- kybern-release-title: Point to attachments right in your prompt -->
<!-- kybern-release-summary: Mention a pasted image as @image1 to place it exactly where your prompt refers to it, model names stay readable during a session, and mid-turn messages get proper spacing. -->

- Mention pasted or attached files inline as @image1 or @file1. Each attachment shows its label, the @ menu lists attachments first, and the agent receives each mentioned file right where the prompt refers to it.
- Keep the friendly model name, like Claude Opus 5.5, after a Claude session starts instead of switching to the raw model id, and stop adding that id to the picker as a custom model.
- Give messages sent while the agent is working the same spacing as other messages, so they no longer sit flush against the agent's reply.

## 0.4.16

<!-- kybern-release-title: Claude models show their version -->
<!-- kybern-release-summary: The model picker shows the concrete Claude model an alias resolves to, like Claude Opus 5, read from the agent itself. -->

- Show the concrete Claude model behind each alias in the picker, so Opus reads as Claude Opus 5 and Fable as Claude Fable 5.1. The version is read from Claude Code at no cost and cached per version, and stays accurate as new models ship.

## 0.4.15

<!-- kybern-release-title: New models show up without a restart -->
<!-- kybern-release-summary: The model picker refreshes when you open it and gains a reload control, and Claude model names stay accurate as new versions ship. -->

- Refresh the agent model catalog when the model picker opens, and add a Reload models action, so newly released models appear without relaunching the app.
- Add a Reload agents control to the sidebar footer to re-probe installed agents and their models on demand.
- Name Claude models mechanically from the agent instead of a fixed table, so an alias like Opus stays version-accurate as new models ship.

## 0.4.14

<!-- kybern-release-title: Project switching from free chats -->
<!-- kybern-release-summary: Move a new free-chat draft into an existing or newly added project directly from the composer. -->

- Turn the Free chat composer chip into an anchored project picker with clear selection, matching row geometry, and an Add project action.

## 0.4.13

<!-- kybern-release-title: Free chats and clearer conversation context -->
<!-- kybern-release-summary: Start chats without a project, keep drafts visible, see message timing, and steer agents while background work continues. -->

- Start project-free chats from New chat and find them in a collapsible Recents section beneath Projects, with an isolated neutral workspace instead of repository controls.
- Keep unsent work visible with pencil indicators for new-chat and thread drafts, and show the remaining provider prompt-cache window in the composer.
- Group conversations by date and show message timestamps so earlier prompts and responses are easier to place in time.
- Send new instructions while supported harnesses continue background tasks, including Claude Code continuation support and parity checks for other providers.

## 0.4.11

<!-- kybern-release-title: Clearer chat controls -->
<!-- kybern-release-summary: Clear stale notifications and see terminal activity, helper harnesses, and submitted answers more clearly. -->

- Add Dismiss all to clear stale notifications, including errors and pending requests, while allowing new attention events to appear.
- Clarify submitted answers, terminal commands, and background activity with dedicated icons, and show overlapping harness logos for helper threads.
- Refine thread menu spacing and composer input sizing, and remove the overlapping fades when opening Settings.

## 0.4.10

<!-- kybern-release-title: Sidebar blur restored -->
<!-- kybern-release-summary: Restore the frosted macOS sidebar when full translucency is turned off. -->

- Restore native sidebar blur in normal appearance mode so windows behind Kybern no longer show through sharply. Full-content translucency remains optional.
- Correct the regression-test fixture that blocked the unpublished 0.4.9 release. Theme startup and translucency toggles remain covered by release checks.

## 0.4.9

<!-- kybern-release-title: Sidebar blur restored -->
<!-- kybern-release-summary: Restore the frosted macOS sidebar when full translucency is turned off. -->

- Restore native sidebar blur in normal appearance mode so windows behind Kybern no longer show through sharply. Full-content translucency remains optional.
- Add a release regression check covering theme startup and translucency toggles.

## 0.4.8

<!-- kybern-release-title: Quieter helpers, reliable follow-ups -->
<!-- kybern-release-summary: Helper threads stay out of notifications, and follow-up assignments wait until the helper is ready. -->

- Keep helper completions, failures, approvals, and questions out of the notification bell, unread dots, toasts, and system alerts. Details and approval controls remain available inside the helper thread and Work panel.
- Queue follow-up assignments while a helper is busy, and separate queued, working, and attention counts in the Work panel.
- Restore macOS release packaging when optional Apple certificate settings are empty. This release includes the fixes from the unpublished 0.4.7 build.

## 0.4.7

<!-- kybern-release-title: Quieter helpers, reliable follow-ups -->
<!-- kybern-release-summary: Helper threads stay out of notifications, and follow-up assignments wait until the helper is ready. -->

- Keep helper completions, failures, approvals, and questions out of the notification bell, unread dots, toasts, and system alerts. Details and approval controls remain available inside the helper thread and Work panel.
- Queue follow-up assignments while a helper is busy instead of incorrectly marking them as needing attention, then dispatch once the helper is available.
- Separate queued, working, and attention counts in the Work panel so stalled tasks no longer inflate the active count.

## 0.4.6

<!-- kybern-release-title: Lighter chats, clearer controls -->
<!-- kybern-release-summary: Long chats and tool-heavy turns use less peak memory, with clearer notifications, reviewed answers, and steadier agent updates. -->

- Reduce memory spikes in long chats and tool-heavy turns while preserving exact saved results, formatting, and useful motion. Large completed results load on demand, and unnecessary graphics layers and historical-result reads are avoided.
- Answer multiple agent questions one step at a time, move backward to edit, and review all answers before submitting.
- Refine the environment menu, add icon-only copy and download actions to image previews, and preserve pasted-image thumbnails when switching threads.
- Keep narrow composers contained and split chats readable, limit the message navigation rail to user prompts, and make translucent command, file, and skill pickers easier to read.
- Use smaller notification dots: blue for completed, yellow for pending or blocked, and red for errors. Right-click the bell to dismiss completed notices; successful helper completions no longer duplicate notifications.
- Add a new-thread control when the sidebar is collapsed, align active sidebar shapes with hover states, and improve unread completion indicators.
- Deliver collaboration updates without stale duplicate wakeups, and keep completed or reused helper agents' activity accurate across harnesses. Fixes #34 and #35.

## 0.4.5

<!-- kybern-release-title: Steady window controls -->
<!-- kybern-release-summary: The macOS traffic lights settle centered on the toolbar as the window opens, without needing a resize. -->

- The macOS traffic lights settle centered on the toolbar as the window opens, instead of drifting up after the first paint and needing a window resize to correct.

## 0.4.4

<!-- kybern-release-title: Unified settings chrome, steady window controls -->
<!-- kybern-release-summary: Settings share the thread sidebar's chrome and the macOS window controls stay aligned with the toolbar. -->

- Settings share the thread's chrome: the same sidebar width and drag handle, row density, icon set, and a flush content surface instead of an inset card.
- The macOS traffic lights stay centered on the toolbar instead of drifting upward after the window finishes opening.
- The sidebar gains a notification center; the bell reflects live status and filters the sidebar instead of opening a popover.
- Composer picker rows are single-line, and the send button, edit glyph, and top chrome use refreshed Hugeicons.

## 0.4.3

<!-- kybern-release-title: Less retained work, steadier history -->
<!-- kybern-release-summary: Large saved results load when opened, long chats retain less daemon memory, and refreshed icons and usage views keep the workspace clear. -->

- Saved tool results load when opened. Results shared across panes stay available while visible, and closing details releases their mounted content without losing exact output or reconnect recovery.
- The daemon avoids unnecessary transcript and notification copies, omits irrelevant history events from transcript reconstruction, and returns unused allocator pages sooner on supported platforms.
- Long histories start with a correctly sized viewport, avoiding an initial full-history mount and preserving the live edge while older content is released.
- Refreshed Hugeicons, integration controls, and usage details improve workspace consistency. Account limits stay with their environment when switching connections.
- Release publication now requires successful CI for the exact main-branch release commit and successful desktop bundles for every supported platform.

## 0.4.2

<!-- kybern-release-title: A clearer place for your preferences -->
<!-- kybern-release-summary: Explore dedicated settings, clearer usage insights, and a sidebar that keeps helper chats connected to their parent. -->

- Settings opens as a dedicated screen with search and grouped navigation. Notifications and background activity have their own pages, and returning keeps your workspace and focus.
- Preview light, dark, and system themes with a simple selection outline. Screen transitions respect keyboard navigation and reduced motion.
- Explore usage by date, agent, model, or day, with token and reported-cost summaries. Loading, empty results, and failures have clear recovery actions.
- Follow helper chats with indentation and connecting guides beneath their parent in the desktop sidebar.
- Command, file, and skill suggestions now match the composer width, and settings icons align with their labels.
- Cursor can discover the repository's project Agent Skills.

## 0.4.1

<!-- kybern-release-title: Lighter, steadier conversations -->
<!-- kybern-release-summary: Coordinators research before they act and can be deleted, helper results arrive once, and long chats use far less memory. -->

- Project coordinators research the project before implementing. The first send keeps your brief, delegates research, and asks for a reviewed overview before accepting edit or integration assignments; setup progress and your corrections survive restarts.
- Delete a coordinator once its work is inactive. Deletion archives it, keeps worker history, files, and knowledge, and frees the project to start a fresh coordinator.
- Helper results are delivered once. Reading a collaboration message now acknowledges it, so an agent no longer receives the same result again as a queued follow-up. This applies to Codex, Claude Code, OpenCode, Pi, OMP, Cursor, and coordinators.
- Spawned conversations collapse under their parent on desktop, phone, and iPad. Queued updates and delivered results show the sender and formatted content on demand, keeping routing details out of the reading flow.
- Long conversations, dense tool activity, and large code blocks use far less memory while scrolling. The desktop stress workload's peak dropped from about 875 MiB to about 390 MiB, streaming allocates less per token, and an idle conversation settles near 130 MiB.
- The transcript's bottom fade no longer masks the whole scroller on opaque windows, and icons only promote while they animate.

## 0.4.0

<!-- kybern-release-title: Bring your agents together -->
<!-- kybern-release-summary: Delegate across harnesses, keep project knowledge with a coordinator, and follow the work from your chats. -->

- Ask agents to read earlier conversations, contact other threads, and delegate work across installed harnesses. Follow their child chats and results from the conversation.
- Create a project coordinator from the familiar chat composer. It plans work, manages workers, and maintains shared project knowledge; switch its harness when it is idle.
- Choose the tools you want in the right sidebar with **+**. New workspaces start with the panel empty and remember your choices.
- See what changed when a desktop update arrives, with a dismissible release card and full release details. Install from **Update and restart** at the bottom of the sidebar, with download progress and retry after a failed installation.
- Preview Mermaid diagrams in conversations. Switch between preview and source, expand a diagram, or copy its definition; incomplete and invalid diagrams keep a readable source fallback.
- Choose a default OMP profile or override it per project in **Settings → Agents**. Worktrees inherit the project profile, while existing chats keep the profile they started with.
- Browse large conversation histories with lower memory use, while preserving selection, focus, row controls, and deliberate loading of earlier messages.
- Keep the composer and stacked panels readable when translucency or accessibility settings disable blur.

## 0.3.6

- Recover stuck agent turns when an interrupt is acknowledged without a final result or the provider stops responding. After a five-second cancellation deadline, close the stuck session and allow the conversation to resume.
- Settle outstanding tasks and approvals during forced stops, preserve recorded usage, and prevent late events or stale stop requests from affecting newer responses.

## 0.2.5

- Keep Claude background agents and processes in the same running turn across follow-up work, and prevent premature or duplicate completion notifications.
- Keep agent images in chronological work order until the turn completes. Show generated images with the final response and inspection screenshots inside their tool results.
- Open image previews from composer attachments and sent messages, with focus restored after closing.
- Preserve Shift+Enter newlines in sent messages.
- Refine the environment picker with aligned rows, clearer status, and a new-window action beside each environment that leaves the current window unchanged.
- Reuse release build caches from main, cross-compile Intel Mac builds on faster ARM runners, and avoid duplicate artifact uploads.

## 0.2.4

- Resume conversations started in Claude Code, Codex, OpenCode, pi, OMP, or Cursor CLI. Open **Resume session** beside **New thread**, from a project's menu, or with `/resume` and `/sessions`.
- Find saved conversations by title, project, folder, or session ID with an **All agents** filter. Import available message and tool history, preserve the native session, and reopen existing Kybern threads without duplicates.
- Keep Claude background completions and follow-up tools in their original turn. Automatically continue when native background work finishes and replace provisional waiting messages with the final answer.
- Preserve continuation state, usage, and history across reloads. Add regression checks for all six session formats, atomic imports, keyboard navigation, and light/dark and narrow native layouts.

## 0.2.3

- Reduce retained conversation memory with bounded inactive caches, compact background updates, and cache release when switching environments. Preserve drafts, pending controls, split panes, and live updates during history reloads.
- Bound event, terminal, and WebSocket buffers by bytes, with durable event replay and terminal scrollback recovery for slow clients.
- Release idle Markdown and highlighting workers and cap provider discovery cache entries.
- Show compact, deferred image thumbnails and load full-resolution originals only when opened.
- Fix table wrapping and horizontal overflow in narrow conversations.
- Add Open in new window to environment menus with independent environment selection per window.
- Add native memory comparisons and regression coverage for rendering, image previews, cache eviction, and recovery.

## 0.2.2

- Count time asleep toward idle agent expiry, so overdue processes close on the first cleanup sweep after waking.
- Stop opening threads, reconnecting, and subscribing to events from extending idle agent lifetimes. Conversations still resume on the next message.
- Preserve active work and approval waits, and recheck activity before closing a selected idle process.

## 0.2.1

- Redesign agent question forms with clearer hierarchy, multiline answers, quieter choices, and responsive action layouts.
- Fix context usage warnings turning near-black instead of amber at 80–94% usage.
- Open local image links in an authenticated in-app preview and provide useful recovery guidance for blocked image paths.
- Give agents thread-specific artifact guidance and document preview placement in the repository instructions.
- Preserve the desktop performance findings and visual-quality requirements in AGENTS.md, CLAUDE.md, and the performance guide.
- Add native WebKit regression checks for question forms and artifact previews.

## 0.2.0

- Virtualize long conversation histories, expanded tool groups, and agent activity to keep mounted interface work bounded.
- Move large Markdown parsing off the interface thread and reuse completed blocks as text streams, preserving GFM, links, footnotes, code controls, and exact final output.
- Navigate every message through a virtualized rail with keyboard support and bounded geometry reads.
- Preserve reading position, following, selections, focus, wrapping, and expansion state while virtual rows mount and unmount.
- Fix sidebar context menus and submenus appearing transparent without blur in Translucent app mode.
- Add M1 native WebKit measurements and regression checks for large histories, growing Markdown, navigation, tool expansion, and menu materials.

## 0.1.9

- Make cards, controls, message bubbles, menus, and dialogs consistently translucent in Translucent app mode.
- Improve long-thread scrolling by preserving settled turn identities and removing collapsed work details from the rendered page.
- Move syntax highlighting off the interface thread, bound queued work and cached results, and pace highlighting while code streams.
- Preserve code-block wrapping and highlighted content while surrounding Markdown updates.
- Reduce streaming Markdown updates and cache message-navigation previews to avoid repeatedly reading historical messages.
- Add native WebKit rendering regression coverage using the production content security policy.

## 0.1.8

- Fix Translucent app mode remaining opaque outside the sidebar on macOS.
- Add native WebKit regression coverage for window materials, settings, floating surfaces, and accessibility fallbacks.

## 0.1.7

- Reduce running-indicator repaint work and pause looping indicators when they are hidden or offscreen.
- Fix full-app translucency being hidden by an opaque content wrapper, and avoid stacking an extra tint in split panes.
- Redesign the context-usage popover with clearer token counts, usage meters, separate account limits, and keyboard access.
- Keep native ResizeObserver layout diagnostics out of fatal error banners and defer resize work to the next frame.
- Replace raw interface-error banners with a compact notice with expandable technical details, copy, and dismiss actions.
- Rebuild the mobile client around native tabs and Liquid Glass surfaces, with updated connection, thread, approval, and settings screens.

## 0.1.6

- Start drafting immediately while harness discovery refreshes, with cached harness choices on subsequent launches.
- Show context usage in the composer, with token details, supported account usage windows, and reset times on hover or focus.
- Add native manual compaction and discovered slash commands where supported by each harness.
- Add full-app translucency in Appearance, including floating surfaces and the terminal, with accessibility fallbacks.
- Fix composer wrapping, caret scrolling, and spacing alignment.
- Close owned harness processes reliably during session release and failed startup; preserve conversation history and provide safe reconnect guidance for Codex writer conflicts.
- Prevent Codex from appearing as its own subagent after resume, and exclude old false entries from activity views.
- Render Codex asynchronous questions as answerable forms. Keep unanswered forms across reloads and deliver submitted answers while the agent works.

Account limits and commands depend on the installed harness protocol. Kybern does not infer missing quotas or expose unsupported terminal-only commands.
