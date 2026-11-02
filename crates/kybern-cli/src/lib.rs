mod accounts;
mod continue_on;
mod coordinator;
mod credentials;
mod fork;
mod machines;
mod mcp_bridge;
mod outbox;
mod plugin_sync;
mod render;
mod requests;
mod skill_sync;
mod spaces;
mod utilities;

use std::path::PathBuf;

use anyhow::{Context, Result, anyhow};
use clap::{Parser, Subcommand};
use kybern_protocol::methods::*;
use kybern_protocol::*;

use kybern_client::{Client, Endpoint};

#[derive(Parser)]
#[command(name = "kybern", version, about = "Command-line client for the kybern daemon")]
struct Cli {
    /// Daemon WebSocket URL, e.g. ws://127.0.0.1:4173/ws
    #[arg(long, global = true)]
    url: Option<String>,
    /// Bearer token. Defaults to ~/.kybern/daemon.token
    #[arg(long, global = true)]
    token: Option<String>,
    /// Data dir to read token/port from.
    #[arg(long, global = true)]
    data_dir: Option<PathBuf>,
    /// Emit raw JSON instead of formatted output.
    #[arg(long, global = true)]
    json: bool,
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum IntegrationsCmd {
    List {
        project: String,
        #[arg(long, default_value = "claude-code")]
        provider: ProviderKind,
    },
    Change {
        project: String,
        id: String,
        #[arg(value_parser = ["install", "uninstall", "enable", "disable", "update"])]
        action: String,
        #[arg(long, default_value = "claude-code")]
        provider: ProviderKind,
        #[arg(long, default_value = "plugin", value_parser = ["plugin", "connector"])]
        kind: String,
        #[arg(long)]
        scope: Option<String>,
    },
    /// Start Claude connector sign-in in a thread terminal.
    Login { thread: String, name: String },
}

#[derive(Subcommand)]
enum ArtifactsCmd {
    List {
        thread: String,
        #[arg(long)]
        before_seq: Option<i64>,
        #[arg(long, default_value_t = 30)]
        limit: u32,
    },
    Read {
        thread: String,
        path: String,
    },
    /// Issue a single-use preview ticket (expires after 60 seconds).
    Preview {
        thread: String,
        path: String,
    },
}

#[derive(Subcommand)]
enum VisualsCmd {
    /// Publish self-contained HTML from a file as a durable inline reply.
    Publish {
        thread: String,
        path: std::path::PathBuf,
        #[arg(long)]
        title: String,
        #[arg(long, default_value_t = 480)]
        height: u32,
    },
    /// Render a screenshot and diagnostics using Kybern's own preview browser.
    Preview {
        thread: String,
        path: std::path::PathBuf,
        #[arg(long, default_value_t = 728)]
        width: u32,
        #[arg(long, default_value = "dark", value_parser = ["dark", "light"])]
        appearance: String,
        #[arg(long)]
        screenshot: std::path::PathBuf,
    },
    Read {
        thread: String,
        visual: uuid::Uuid,
    },
    Frame {
        thread: String,
        visual: uuid::Uuid,
    },
    Revoke {
        thread: String,
        ticket: String,
    },
    /// Draw a visual's transcript picture (transparent, 2x). Prints its key and the HTTP path of the PNG.
    Snapshot {
        thread: String,
        visual: uuid::Uuid,
        #[arg(long, default_value_t = 736)]
        width: u32,
        #[arg(long, default_value = "dark")]
        appearance: String,
    },
}

#[derive(Subcommand)]
enum Cmd {
    /// Install or sign in to the official Cursor SDK on this machine.
    Cursor {
        #[command(subcommand)]
        cmd: CursorCmd,
    },
    /// Open, show or move a project's Coordinator, the lead thread that plans and delegates.
    Coordinator {
        #[command(subcommand)]
        cmd: coordinator::CoordinatorCmd,
    },
    /// Find threads and read their saved messages without waking them.
    History {
        #[command(subcommand)]
        cmd: coordinator::HistoryCmd,
    },
    /// The helper behind SUDO_ASKPASS, SSH_ASKPASS and GIT_ASKPASS in agent sessions: shows the prompt as a private card and prints the answer.
    #[command(hide = true)]
    Askpass {
        /// The prompt, as the asking program passes it.
        prompt: Vec<String>,
    },
    /// The stdio MCP server Antigravity starts to reach Kybern's tools. Inert outside a Kybern session.
    #[command(hide = true)]
    McpBridge,
    /// See or press the request cards agents summon: sign-ins, permissions, questions with a deadline.
    Requests {
        #[command(subcommand)]
        cmd: requests::RequestsCmd,
    },
    /// The system credential store for Kybern's own entries (the secret goes through standard input and output).
    #[command(hide = true)]
    Credentials {
        #[command(subcommand)]
        cmd: credentials::CredentialsCmd,
    },
    /// Show daemon info.
    Info,
    /// Show what the daemon is holding open: clients, agent processes, terminals, queued work.
    Activity,
    /// Stop the local daemon gracefully.
    StopDaemon,
    /// List providers and their availability.
    Providers {
        /// Resolve project-scoped provider settings for this project id or path.
        #[arg(long)]
        project: Option<String>,
        /// Bypass the daemon's short-lived provider catalog cache.
        #[arg(long)]
        refresh: bool,
    },
    /// Manage native-isolated named accounts.
    Accounts {
        #[command(subcommand)]
        cmd: AccountsCmd,
    },
    /// Inspect or choose the next-message target of an existing conversation.
    Target {
        thread: ThreadId,
        #[arg(long)]
        provider: Option<ProviderKind>,
        #[arg(long)]
        account: Option<String>,
        #[arg(long)]
        model: Option<String>,
        #[arg(long)]
        effort: Option<String>,
        #[arg(long)]
        inherit: bool,
    },
    /// Continue an interrupted usage-limited task on an explicitly selected account.
    SwitchContinue {
        thread: ThreadId,
        #[arg(long)]
        provider: ProviderKind,
        #[arg(long)]
        account: String,
        #[arg(long)]
        message_id: Option<MessageId>,
    },
    /// Stop the turn and apply a pending permission change now.
    ApplyPermissions { thread: ThreadId },
    /// List saved conversations from an agent harness, or find and import them all (`scan`, `import`).
    #[command(args_conflicts_with_subcommands = true)]
    Sessions {
        #[command(subcommand)]
        cmd: Option<SessionsCmd>,
        #[arg(long)]
        query: Option<String>,
        #[arg(long)]
        provider: Option<ProviderKind>,
        #[arg(long)]
        project: Option<String>,
        #[arg(long)]
        cursor: Option<String>,
    },
    /// Continue a saved harness session in Kybern, importing its history.
    Resume {
        #[arg(long)]
        provider: ProviderKind,
        /// Select the project/profile used to list this saved session.
        #[arg(long)]
        project: Option<String>,
        session_id: String,
    },
    /// Show harness update results, or request an update when idle.
    HarnessUpdates {
        #[arg(long)]
        run: Option<ProviderKind>,
    },
    /// Show the daemon's own update state, check the release feed, or install the newest version when idle.
    DaemonUpdate {
        /// Ask the release feed for the newest version.
        #[arg(long)]
        check: bool,
        /// Install the newest version once nothing is running, then restart the daemon.
        #[arg(long)]
        run: bool,
    },
    /// Manage projects.
    #[command(alias = "project")]
    Projects {
        #[command(subcommand)]
        cmd: ProjectsCmd,
    },
    /// Group projects into spaces.
    Spaces {
        #[command(subcommand)]
        cmd: spaces::SpacesCmd,
    },
    /// Your network of machines: list, add, remove, and check them.
    Machines {
        #[command(subcommand)]
        cmd: machines::MachinesCmd,
    },
    /// Give another machine a copy of a thread's work: a hand-off note, its branch and your uncommitted changes.
    Fork(fork::ForkArgs),
    /// Move a thread to another machine: its work and conversation go there and this thread becomes read-only.
    Continue(continue_on::ContinueArgs),
    /// Give up a move that could not be confirmed, so the thread is yours again.
    ContinueAbandon(continue_on::ContinueAbandonArgs),
    /// Work waiting for machines that are offline.
    Outbox {
        #[command(subcommand)]
        cmd: outbox::OutboxCmd,
    },
    /// List threads.
    Threads {
        #[arg(long)]
        project: Option<String>,
        /// Include archived threads in the list (they are hidden by default).
        #[arg(long)]
        archived: bool,
    },
    /// Create a thread and send the first message, streaming the turn.
    New {
        /// Project id, or a path (added if missing).
        #[arg(long, short)]
        project: String,
        #[arg(long, default_value = "claude-code")]
        provider: String,
        #[arg(long)]
        model: Option<String>,
        #[arg(long)]
        effort: Option<String>,
        #[arg(long, value_parser = parse_mode)]
        mode: Option<PermissionMode>,
        #[arg(long)]
        worktree: bool,
        /// Branch to start from: the worktree forks from it, or the checkout switches to it.
        #[arg(long)]
        branch: Option<String>,
        /// Do not stream; return the thread id immediately.
        #[arg(long)]
        detach: bool,
        prompt: Vec<String>,
    },
    /// Send a message to an existing thread and stream the turn.
    Send {
        thread: String,
        #[arg(long)]
        detach: bool,
        prompt: Vec<String>,
    },
    /// Steer the currently running turn using the provider's native input control.
    Steer { thread: String, prompt: Vec<String> },
    /// Queue a message for an active Claude native subagent's next tool call.
    SubagentSend {
        thread: String,
        #[arg(long)]
        message_id: Option<uuid::Uuid>,
        prompt: Vec<String>,
    },
    /// Read durable native child delivery states.
    SubagentMessages { thread: String },
    /// Explicitly send an undelivered child message to its root parent.
    SubagentSendToParent { thread: String, message_id: uuid::Uuid },
    /// Read or update a thread's personal notes.
    Notes {
        thread: String,
        /// Replace the notes with this UTF-8 file (an empty file clears them).
        #[arg(long)]
        file: Option<PathBuf>,
    },
    /// Write and find notes: global pages, project pages and thread notes.
    Note {
        #[command(subcommand)]
        cmd: NoteCmd,
    },
    /// Plan work as tasks and send them to an agent.
    Task {
        #[command(subcommand)]
        cmd: TaskCmd,
    },
    /// Print a thread's transcript.
    Show {
        thread: String,
        /// Show only the newest N entries; JSON output includes an older-page cursor.
        #[arg(long)]
        limit: Option<u32>,
        #[arg(long, requires = "limit")]
        before_seq: Option<i64>,
        #[arg(long)]
        through_seq: Option<i64>,
    },
    /// Recover a historical OMP answer from exact retained native block boundaries.
    RecoverOmpAnswer { thread: String, turn: String },
    /// Read one saved tool result at an optional historical snapshot.
    ToolOutput {
        thread: String,
        tool_call_id: String,
        #[arg(long)]
        start_seq: Option<i64>,
        #[arg(long)]
        through_seq: Option<i64>,
    },
    /// Manage durable follow-ups on this environment.
    Queue {
        #[command(subcommand)]
        cmd: QueueCmd,
    },
    /// List, deliver or dismiss messages other threads sent (held ones wait for you).
    Messages {
        #[command(subcommand)]
        cmd: MessagesCmd,
    },
    /// Manage threads that agents delegated work to.
    Delegations {
        #[command(subcommand)]
        cmd: DelegationsCmd,
    },
    /// Follow live events for one thread or all threads.
    Watch {
        thread: Option<String>,
        /// Replay from this seq first (0 = full history).
        #[arg(long)]
        after: Option<i64>,
    },
    /// Interrupt the running turn.
    Interrupt { thread: String },
    /// Compact context using the harness native operation.
    Compact { thread: String },
    /// Answer a non-blocking question; provide one answer per question in order.
    Answer {
        thread: String,
        request_id: String,
        #[arg(required = true)]
        answers: Vec<String>,
    },
    /// Release an idle agent process; preserve its conversation for resume.
    Release { thread: String },
    /// Inspect or control provider-owned agents and background processes.
    Tasks {
        thread: String,
        #[command(subcommand)]
        cmd: Option<TasksCmd>,
    },
    /// List or answer pending approvals.
    Approvals {
        #[command(subcommand)]
        cmd: Option<ApprovalsCmd>,
    },
    /// Archive a thread.
    Archive { thread: String },
    /// Inspect or remove an ordinary conversation’s managed worktree.
    Worktree {
        thread: String,
        #[arg(long)]
        remove: bool,
        #[arg(long)]
        force: bool,
        #[arg(long)]
        delete_branch: bool,
    },
    /// List git checkpoints for a thread.
    Checkpoints { thread: String },
    /// Show the diff for a thread (whole thread) or one turn.
    Diff {
        thread: String,
        #[arg(long)]
        turn: Option<String>,
        /// Only list files, no patch.
        #[arg(long)]
        stat: bool,
    },
    /// Restore the working tree to the state before a turn.
    Revert { thread: String, turn: String },
    /// Show or edit settings.
    Settings {
        #[command(subcommand)]
        cmd: Option<SettingsCmd>,
    },
    /// Set up and check computer use (CuaDriver or agent-desktop).
    Computer {
        #[command(subcommand)]
        cmd: Option<ComputerCmd>,
    },
    /// Save a secret to a git-ignored env file in a thread's workspace; the value is read from stdin.
    SecretStore {
        thread: String,
        /// The variable to set, e.g. STRIPE_SECRET_KEY.
        key: String,
        /// Env file relative to the workspace.
        #[arg(long, default_value = ".env")]
        file: String,
        /// Add the file to the .gitignore beside it when git does not ignore it yet.
        #[arg(long)]
        add_to_gitignore: bool,
    },
    /// Token usage and cost.
    Usage {
        #[arg(long, value_parser = ["provider", "model", "day", "thread"], default_value = "provider")]
        by: String,
        /// Only turns in the last N days.
        #[arg(long)]
        days: Option<i64>,
        /// Show current plan limits (5-hour / weekly) per provider instead.
        #[arg(long)]
        limits: bool,
    },
    /// Pair another device: prints a one-time code and the endpoints to use.
    Pair {
        #[arg(long)]
        label: Option<String>,
        /// Address reachable from the receiving device (e.g. an HTTPS proxy or SSH tunnel).
        #[arg(long)]
        address: Option<String>,
        /// Also listen on this machine's Tailscale address (remembered across restarts),
        /// so devices on the tailnet can scan the invitation and connect directly.
        #[arg(long)]
        tailscale: bool,
    },
    /// List or revoke access tokens.
    Tokens {
        #[command(subcommand)]
        cmd: Option<TokensCmd>,
    },
    /// Git status for a thread's working directory.
    Git { thread: String },
    /// List a project's local branches, most recently committed first.
    Branches { project: String },
    /// Find files in a project by fuzzy path match, or send some to another machine.
    #[command(args_conflicts_with_subcommands = true)]
    Files {
        #[command(subcommand)]
        cmd: Option<utilities::FilesCmd>,
        /// A project id.
        project: Option<String>,
        #[arg(default_value = "")]
        query: String,
    },
    /// Forward ports of your other machines to this one.
    Ports {
        #[command(subcommand)]
        cmd: utilities::PortsCmd,
    },
    /// Manage provider-owned plugins and connectors.
    Integrations {
        #[command(subcommand)]
        cmd: IntegrationsCmd,
    },
    /// Find and check web pages for the in-app browser preview.
    Preview {
        #[command(subcommand)]
        cmd: PreviewCmd,
    },
    /// Preview and publish durable inline interactive HTML replies.
    Visuals {
        #[command(subcommand)]
        cmd: VisualsCmd,
    },
    /// Inspect native Claude artifact receipts and local source files.
    Artifacts {
        #[command(subcommand)]
        cmd: ArtifactsCmd,
    },
    /// List skills available to an agent in a project, or your user skills on every machine.
    #[command(args_conflicts_with_subcommands = true)]
    /// Compare and explicitly copy user plugins between machines.
    Plugins {
        #[command(subcommand)]
        cmd: plugin_sync::PluginsCmd,
    },
    Skills {
        #[command(subcommand)]
        cmd: Option<skill_sync::SkillsCmd>,
        project: Option<String>,
        #[arg(long, default_value = "codex")]
        provider: ProviderKind,
    },
    /// List one directory of a project (relative path, empty for the root)
    Ls {
        project: String,
        #[arg(default_value = "")]
        path: String,
    },
    /// Print a project file
    Cat { project: String, path: String },
    /// Read a linked file relative to a conversation workspace.
    ThreadCat { thread: String, path: String },
    /// Replace an existing project file's contents.
    Write {
        project: String,
        path: String,
        /// A file to read the new contents from; `-` (the default) is stdin.
        #[arg(long, default_value = "-")]
        from: String,
        /// Fail instead of overwriting when the file is no longer at this version.
        #[arg(long)]
        expected_version: Option<String>,
    },
    /// Replace an existing file's contents in a conversation workspace.
    ThreadWrite {
        thread: String,
        path: String,
        /// A file to read the new contents from; `-` (the default) is stdin.
        #[arg(long, default_value = "-")]
        from: String,
        #[arg(long)]
        expected_version: Option<String>,
    },
    /// Show which file references name a file in a conversation workspace.
    ThreadResolve {
        thread: String,
        #[arg(required = true)]
        paths: Vec<String>,
    },
    /// Commit everything in a thread's working directory.
    Commit {
        thread: String,
        #[arg(long, short)]
        message: Option<String>,
    },
    /// Pull requests via the GitHub CLI.
    Pr {
        #[command(subcommand)]
        cmd: PrCmd,
    },
    /// GitHub Actions workflow runs via the GitHub CLI.
    Actions {
        #[command(subcommand)]
        cmd: ActionsCmd,
    },
    /// Kybern's own GitHub repository, as the daemon's GitHub CLI account sees it.
    Github {
        #[command(subcommand)]
        cmd: GithubCmd,
    },
    /// Terminals owned by the daemon.
    Terminal {
        #[command(subcommand)]
        cmd: TerminalCmd,
    },
    /// Call any RPC method with raw JSON params.
    Call { method: String, params: Option<String> },
}

#[derive(Subcommand)]
enum SessionsCmd {
    /// Find saved sessions in every installed harness, grouped by repository.
    Scan {
        /// Only these harnesses (repeatable).
        #[arg(long)]
        provider: Vec<ProviderKind>,
        /// Only repositories with enough recent sessions to suggest.
        #[arg(long)]
        suggested: bool,
    },
    /// Import the saved sessions of one or more repositories into Kybern, without running anything.
    Import {
        /// Repository folders, as shown by `kybern sessions scan`.
        #[arg(required = true)]
        roots: Vec<String>,
        /// Only these harnesses (repeatable).
        #[arg(long)]
        provider: Vec<ProviderKind>,
        /// Most recent sessions to import per repository.
        #[arg(long, default_value_t = 50, conflicts_with = "all")]
        limit: u32,
        /// Import every session instead of the most recent ones.
        #[arg(long)]
        all: bool,
        /// Import into this existing project (name, path or id); with one root only.
        #[arg(long)]
        project: Option<String>,
        /// Reuse the session list of a finished scan.
        #[arg(long)]
        scan_id: Option<String>,
        /// Start the job and print its id without waiting.
        #[arg(long)]
        detach: bool,
    },
    /// Show an import job.
    Job { job_id: String },
    /// Stop an import job after the session being read.
    Cancel { job_id: String },
}

#[derive(Subcommand)]
enum NoteCmd {
    /// List notes, newest first with pinned notes on top.
    List {
        /// Show Recently deleted notes instead.
        #[arg(long)]
        deleted: bool,
        /// Machine name or id, or `all` for every online machine. Default: this one.
        #[arg(long)]
        machine: Option<String>,
    },
    /// Print a note as markdown.
    Show { id: String },
    /// Create a global note, or a project note with --project.
    New {
        /// Project name, path or id.
        #[arg(long)]
        project: Option<String>,
        #[arg(long)]
        title: Option<String>,
        /// Read the markdown body from this file (`-` reads stdin).
        #[arg(long)]
        file: Option<String>,
        /// What kind of memory this is: preference, decision, convention or gotcha.
        #[arg(long, value_parser = parse_note_category)]
        category: Option<NoteCategory>,
    },
    /// Replace a note's body from a file (`-` reads stdin), and optionally its title.
    Edit {
        id: String,
        /// Read the new markdown body from this file (`-` reads stdin).
        #[arg(long)]
        file: Option<String>,
        #[arg(long)]
        title: Option<String>,
        /// Set the note's category: preference, decision, convention or gotcha.
        #[arg(long, value_parser = parse_note_category)]
        category: Option<NoteCategory>,
    },
    /// Show the project's "Tidy notes" offer (notes whose cited files changed), snooze it, or send it to the Coordinator.
    Tidy {
        /// Project name, path or id.
        project: String,
        /// "Not now": hide the offer for 7 days.
        #[arg(long, conflicts_with = "start")]
        not_now: bool,
        /// Ask the project's Coordinator to check the flagged notes and propose changes.
        #[arg(long)]
        start: bool,
    },
    /// Move a note to Recently deleted.
    Delete { id: String },
    /// Bring a deleted note back.
    Restore { id: String },
    /// Find notes whose title or text contains the words.
    Search {
        query: Vec<String>,
        /// Machine name or id, or `all` for every online machine. Default: this one.
        #[arg(long)]
        machine: Option<String>,
    },
}

#[derive(Subcommand)]
enum TaskCmd {
    /// List tasks by status. Done and canceled tasks are hidden unless asked for.
    List {
        /// Only this status: inbox, todo, running, needs-review, done or canceled.
        #[arg(long, value_parser = parse_task_status)]
        status: Option<TaskStatus>,
        /// Only this project's tasks (name, path or id).
        #[arg(long)]
        project: Option<String>,
        /// Include done and canceled tasks.
        #[arg(long, short)]
        all: bool,
        /// Machine name or id, or `all` for every online machine. Default: this one.
        #[arg(long)]
        machine: Option<String>,
    },
    /// Print a task, its linked notes and its runs.
    Show {
        /// Key such as ADE-14, or an id.
        task: String,
    },
    /// Create a task. Without --project it is a global task.
    New {
        title: String,
        /// Project name, path or id.
        #[arg(long)]
        project: Option<String>,
        /// Read the markdown description from this file (`-` reads stdin).
        #[arg(long)]
        file: Option<String>,
        /// inbox (default), todo, done or canceled.
        #[arg(long, value_parser = parse_task_status)]
        status: Option<TaskStatus>,
        /// 0 none, 1 urgent, 2 high, 3 medium, 4 low.
        #[arg(long)]
        priority: Option<u8>,
    },
    /// Change a task's title, description, status or priority.
    Edit {
        task: String,
        #[arg(long)]
        title: Option<String>,
        /// Read the new markdown description from this file (`-` reads stdin).
        #[arg(long)]
        file: Option<String>,
        /// inbox, todo, done or canceled.
        #[arg(long, value_parser = parse_task_status)]
        status: Option<TaskStatus>,
        #[arg(long)]
        priority: Option<u8>,
    },
    /// Mark a task done.
    Done { task: String },
    /// Send text to the task's agent: now when its run is idle, queued when busy, or saved for the next run.
    Followup { task: String, text: Vec<String> },
    /// Start an agent on a task, in the background. Follow it with `kybern task show`.
    Send {
        task: String,
        /// Agent to run; defaults to the default provider in settings.
        #[arg(long)]
        provider: Option<String>,
        #[arg(long)]
        model: Option<String>,
        #[arg(long)]
        effort: Option<String>,
        #[arg(long, value_parser = parse_mode)]
        mode: Option<PermissionMode>,
        /// Run in a new git worktree.
        #[arg(long)]
        worktree: bool,
        /// Branch to start from.
        #[arg(long)]
        branch: Option<String>,
        /// Project to run in. Required for a global task.
        #[arg(long)]
        project: Option<String>,
        /// Prompt text; defaults to the task's title and description.
        prompt: Vec<String>,
    },
    /// Start an agent on several tasks at once: a thread each, or one thread for all with --combined.
    #[command(name = "send-batch")]
    SendBatch {
        /// Task keys or ids. Tasks that already have a run in progress are skipped.
        #[arg(required = true)]
        tasks: Vec<String>,
        /// One run for every task instead of a run each.
        #[arg(long)]
        combined: bool,
        /// Prompt text sent with every task's reference.
        #[arg(long, default_value = "Work on the attached tasks.")]
        prompt: String,
        /// Agent to run; defaults to the default provider in settings.
        #[arg(long)]
        provider: Option<String>,
        #[arg(long)]
        model: Option<String>,
        #[arg(long)]
        effort: Option<String>,
        #[arg(long, value_parser = parse_mode)]
        mode: Option<PermissionMode>,
        /// Run in a new git worktree.
        #[arg(long)]
        worktree: bool,
        /// Branch to start from.
        #[arg(long)]
        branch: Option<String>,
        /// Project to run global tasks in.
        #[arg(long)]
        project: Option<String>,
    },
}

#[derive(Subcommand)]
enum MessagesCmd {
    /// List messages sent to or from a thread, oldest first.
    List {
        thread: String,
        /// Only these states: held, queued, steered, delivered, answered, dismissed, failed.
        #[arg(long = "state", value_parser = parse_message_state)]
        states: Vec<ThreadMessageState>,
    },
    /// Deliver a held message to its thread.
    Deliver { message: String },
    /// Dismiss a held message without delivering it.
    Dismiss { message: String },
}

#[derive(Subcommand)]
enum DelegationsCmd {
    /// Remove a delegated child's worktree (and its branch when merged).
    RemoveWorktree {
        thread: String,
        /// Required when the worktree has uncommitted changes or its branch is unmerged.
        #[arg(long)]
        force: bool,
    },
}

fn parse_message_state(s: &str) -> Result<ThreadMessageState, String> {
    serde_json::from_value(serde_json::Value::String(s.to_string()))
        .map_err(|_| format!("unknown message state `{s}`; use held, queued, steered, delivered, answered, dismissed or failed"))
}

#[derive(Subcommand)]
enum QueueCmd {
    List { thread: Option<String> },
    Add { thread: String, prompt: Vec<String> },
    Edit { thread: String, id: String, prompt: Vec<String> },
    Remove { thread: String, id: String },
}

#[derive(Subcommand)]
enum AccountsCmd {
    /// List accounts with identity and sign-in state. The CLI account is the
    /// one the agent's own command line uses.
    List {
        #[arg(long)]
        provider: ProviderKind,
    },
    /// Sign in to a new account (or again to --account) in the browser.
    Login {
        #[arg(long)]
        provider: ProviderKind,
        /// Sign in again to this existing account instead of adding one.
        #[arg(long)]
        account: Option<String>,
        #[arg(long, value_enum, default_value = "browser")]
        mode: accounts::LoginModeArg,
        /// Use an existing native folder for a new account.
        #[arg(long)]
        directory: Option<String>,
        /// omp only: the upstream provider id (for example `anthropic`).
        #[arg(long)]
        upstream: Option<String>,
    },
    Rename {
        #[arg(long)]
        provider: ProviderKind,
        account: String,
        name: String,
    },
    /// Set an account color: blue, green, purple, pink, teal or amber.
    Color {
        #[arg(long)]
        provider: ProviderKind,
        account: String,
        color: String,
    },
    SignOut {
        #[arg(long)]
        provider: ProviderKind,
        account: String,
    },
    Remove {
        #[arg(long)]
        provider: ProviderKind,
        account: String,
    },
    Create {
        #[arg(long)]
        provider: ProviderKind,
        name: String,
        #[arg(long)]
        directory: Option<String>,
    },
    SignIn {
        #[arg(long)]
        provider: ProviderKind,
        account: String,
    },
    Usage {
        #[arg(long)]
        provider: ProviderKind,
        account: String,
    },
    Catalog {
        #[arg(long)]
        provider: ProviderKind,
        account: String,
        #[arg(long)]
        project: Option<String>,
        #[arg(long)]
        refresh: bool,
    },
    Default {
        #[arg(long)]
        provider: ProviderKind,
        account: String,
        #[arg(long)]
        project: Option<String>,
    },
}

#[derive(Subcommand)]
enum ProjectsCmd {
    List {
        /// Return the stored project index without refreshing Git status.
        #[arg(long)]
        cached: bool,
        /// Only the projects in this space (an id or name).
        #[arg(long)]
        space: Option<String>,
    },
    /// Browse directories on the connected environment.
    Browse {
        path: Option<String>,
    },
    Add {
        path: PathBuf,
        #[arg(long)]
        name: Option<String>,
        /// The space for the new project (an id or name). Defaults to the first space.
        #[arg(long)]
        space: Option<String>,
        /// Add a group of repositories: PATH is the parent folder, each --member a repository inside it.
        #[arg(long)]
        group: bool,
        /// A member repository of the group, relative to PATH ("." for the folder itself). Repeat for each.
        #[arg(long = "member", requires = "group")]
        members: Vec<String>,
    },
    /// List the git repositories inside a folder, as candidates for a group.
    Scan {
        path: PathBuf,
        /// Also check these folders (relative to PATH or absolute), like "Add folder…".
        #[arg(long)]
        include: Vec<String>,
        /// Folder levels to search (default 2, at most 4).
        #[arg(long)]
        depth: Option<u8>,
    },
    /// Show a group's repositories, or replace them.
    Members {
        /// A project id, name or path.
        project: String,
        /// Replace the members with this comma-separated list of paths.
        #[arg(long, value_delimiter = ',')]
        set: Option<Vec<String>>,
        /// Replace the top-level entries copied into new worktrees (comma-separated; "" clears).
        #[arg(long, value_delimiter = ',')]
        copy: Option<Vec<String>>,
    },
    /// Show the project map: repositories, layout, commands, busy areas and recent removals, stamped with HEAD.
    Map {
        /// A project id, name or path.
        project: String,
    },
    /// Create the group folder's missing shared AGENTS.md and CLAUDE.md.
    Instructions {
        /// A project id, name or path.
        project: String,
        /// Hide the offer in the apps instead of creating the files.
        #[arg(long)]
        dismiss: bool,
    },
    /// Move a project to another space.
    Move {
        /// A project id, name or path.
        project: String,
        /// A space id or name.
        space: String,
    },
    /// Add a project of this machine to another machine of the network: it clones there, or this machine sends it.
    Provision {
        /// A project id, name or path.
        project: String,
        /// The other machine's name or id.
        machine: String,
        /// The folder on that machine (`~` is its home). Omit for the one it proposes.
        #[arg(long)]
        path: Option<String>,
        /// An env file to copy, relative to the project. Repeat for each; omit to copy every detected one.
        #[arg(long = "env")]
        env_files: Vec<String>,
        /// Copy no env files.
        #[arg(long, conflicts_with = "env_files")]
        no_env: bool,
        /// Only show what would happen: the proposed folder and the env files.
        #[arg(long)]
        plan: bool,
    },
    /// Show where adding a project to another machine stands.
    ProvisionStatus {
        operation_id: uuid::Uuid,
    },
    Remove {
        id: String,
    },
}

#[derive(Subcommand)]
enum TokensCmd {
    List,
    Revoke { id: String },
}

/// Run a pull request action that needs the current head: read it, then act on exactly that head.
async fn pr_simple_action(
    client: &Client,
    project: &str,
    number: u64,
    repo: Option<String>,
    action: PrActionKind,
    merge_method: Option<String>,
    admin: bool,
) -> Result<()> {
    let project_id = resolve_project(client, project, false).await?;
    let detail = client.call::<PrDetail>(PrDetailParams { project_id, number, repo: repo.clone() }).await?;
    client
        .call::<PrAction>(PrActionParams {
            project_id,
            number,
            action,
            body: String::new(),
            inline_comments: Vec::new(),
            head_sha: detail.head_sha,
            thread_id: None,
            for_repair: false,
            repo,
            merge_method,
            admin,
        })
        .await?;
    Ok(())
}

#[derive(Subcommand)]
enum ActionsCmd {
    /// List workflow runs on any branch, newest first.
    Runs {
        project: String,
        /// A workflow id or file name (ci.yml).
        #[arg(long)]
        workflow: Option<String>,
        #[arg(long)]
        branch: Option<String>,
        /// A status or conclusion: in_progress, failure, success, …
        #[arg(long)]
        status: Option<String>,
        #[arg(long, default_value = "1")]
        page: u32,
        /// Member repository of a group project.
        #[arg(long)]
        repo: Option<String>,
    },
    /// A run with its jobs and steps, and the jobs its workflow file declares.
    View {
        project: String,
        run: u64,
        /// An earlier attempt.
        #[arg(long)]
        attempt: Option<u32>,
        #[arg(long)]
        repo: Option<String>,
    },
    /// Print one page of a finished job's log (--excerpt: the lines around its errors).
    Log {
        project: String,
        job: u64,
        #[arg(long, default_value = "0")]
        offset: u32,
        #[arg(long, default_value = "500")]
        limit: u32,
        #[arg(long)]
        excerpt: bool,
        #[arg(long)]
        repo: Option<String>,
    },
    /// List the repository's workflows.
    Workflows {
        project: String,
        #[arg(long)]
        repo: Option<String>,
    },
    /// Run a run's jobs again (--failed: only the failed ones; --job: one job).
    Rerun {
        project: String,
        run: u64,
        #[arg(long, conflicts_with = "job")]
        failed: bool,
        #[arg(long)]
        job: Option<u64>,
        #[arg(long)]
        repo: Option<String>,
    },
    /// Cancel a run.
    Cancel {
        project: String,
        run: u64,
        #[arg(long)]
        repo: Option<String>,
    },
    /// Start a workflow_dispatch workflow: `-f checks=all` per input.
    Dispatch {
        project: String,
        workflow: String,
        #[arg(long = "ref")]
        git_ref: String,
        #[arg(short = 'f', long = "field")]
        fields: Vec<String>,
        #[arg(long)]
        repo: Option<String>,
    },
}

#[derive(Subcommand)]
enum GithubCmd {
    /// Whether the signed-in GitHub account starred Tresnanda/kybern.
    Starred,
    /// Star Tresnanda/kybern as the signed-in GitHub account.
    Star,
}

#[derive(Subcommand)]
enum PrCmd {
    /// Read pull request description, checks and requested reviewers.
    View {
        project: String,
        number: u64,
        /// Member repository of a group project.
        #[arg(long)]
        repo: Option<String>,
    },
    /// Read one bounded page of files, comments, reviews or review_comments.
    Page {
        project: String,
        number: u64,
        kind: String,
        #[arg(long, default_value = "1")]
        page: u32,
        /// Member repository of a group project.
        #[arg(long)]
        repo: Option<String>,
        /// `timeline` only: the next_cursor of the previous page.
        #[arg(long)]
        cursor: Option<String>,
    },
    /// Merge a pull request (squash unless --method says otherwise). --auto turns on
    /// GitHub auto-merge, --cancel-auto turns it off, --admin bypasses branch protection.
    Merge {
        project: String,
        number: u64,
        /// squash, merge or rebase.
        #[arg(long)]
        method: Option<String>,
        #[arg(long, conflicts_with_all = ["auto", "cancel_auto"])]
        admin: bool,
        #[arg(long, conflicts_with = "cancel_auto")]
        auto: bool,
        #[arg(long)]
        cancel_auto: bool,
        /// Member repository of a group project.
        #[arg(long)]
        repo: Option<String>,
    },
    /// Merge the base branch into the pull request's branch.
    UpdateBranch {
        project: String,
        number: u64,
        /// Member repository of a group project.
        #[arg(long)]
        repo: Option<String>,
    },
    /// Take a draft pull request out of draft.
    Ready {
        project: String,
        number: u64,
        /// Member repository of a group project.
        #[arg(long)]
        repo: Option<String>,
    },
    /// Explicitly comment, approve, request_changes, checkout, merge or close.
    /// Read structured PrActionParams from a JSON file (or stdin with -).
    Action { file: String },
    /// Create a pull request from a thread's branch (commits and pushes first).
    Create {
        thread: String,
        #[arg(long)]
        title: Option<String>,
        #[arg(long)]
        body: Option<String>,
        #[arg(long)]
        base: Option<String>,
        #[arg(long)]
        draft: bool,
    },
    /// List pull requests for a project.
    List {
        project: String,
        #[arg(long, default_value = "open")]
        state: String,
    },
}

#[derive(Subcommand)]
enum SettingsCmd {
    /// Print settings as JSON.
    Show,
    /// Replace settings from a JSON file (or stdin with `-`).
    Set { file: String },
}

#[derive(Subcommand)]
enum PreviewCmd {
    /// List local web servers that can be previewed.
    Servers {
        /// Mark servers running from this thread's folder.
        #[arg(long)]
        thread: Option<String>,
    },
    /// Check whether a local or private-network page answers and can be framed.
    Probe {
        /// Page address, such as http://localhost:5173.
        #[arg(value_name = "URL")]
        address: String,
    },
    /// Open a page or HTML file for a thread and print the path to load.
    Open {
        /// Thread id.
        thread: String,
        /// URL, file path, or host:port.
        target: String,
        /// Allow previewing files from the target's folder (saved in settings).
        #[arg(long)]
        allow_folder: bool,
    },
    /// Open a loopback dev server without requiring a thread on this machine.
    Server {
        /// Server address, such as http://localhost:5173.
        url: String,
        /// Serve through a device-scoped proxy ticket.
        #[arg(long)]
        proxy: bool,
    },
    /// List the folders outside projects that previews may serve.
    Folders {
        /// Remove this folder from the list.
        #[arg(long, value_name = "PATH")]
        remove: Option<String>,
    },
}

/// The driver a machine's computer use runs on, in words for the person.
fn computer_driver_name(status: &ComputerStatus) -> &'static str {
    match status.driver {
        Some(ComputerDriver::CuaDriver) => "CuaDriver",
        Some(ComputerDriver::AgentDesktop) | None => "agent-desktop",
    }
}

fn computer_grant_hint(status: &ComputerStatus) -> String {
    match (status.driver, status.platform.as_deref()) {
        (Some(ComputerDriver::CuaDriver), _) => "Follow CuaDriver's prompts to allow Accessibility and Screen Recording.".into(),
        (_, Some("macos")) => {
            "Follow the prompts to allow Accessibility and Screen Recording for Kybern, then run `kybern computer doctor`.".into()
        }
        _ => {
            format!("{} needs no permission prompts here; `kybern computer doctor` lists what else to check.", computer_driver_name(status))
        }
    }
}

#[derive(Subcommand)]
enum ComputerCmd {
    /// Print computer-use status as JSON.
    Status,
    /// Check each requirement and say how to fix what is missing.
    Doctor,
    /// Install or update the driver this machine uses (CuaDriver or agent-desktop).
    Install,
    /// Start the Accessibility and Screen Recording prompts (macOS).
    Grant,
    /// Save what a thread's agent last saw while using an app.
    Frame {
        thread: String,
        /// JPEG destination.
        #[arg(long, default_value = "frame.jpg")]
        out: String,
    },
    /// List what agents learned about apps, or show, replace or clear one app's notes.
    Notes {
        /// App name or app id (a bundle id, exe name or desktop-entry id).
        app: Option<String>,
        /// Replace the notes with this UTF-8 file (`-` reads stdin).
        #[arg(long, conflicts_with = "clear", requires = "app")]
        set: Option<String>,
        /// Delete the notes.
        #[arg(long, requires = "app")]
        clear: bool,
    },
}

#[derive(Subcommand)]
enum TerminalCmd {
    /// List terminals.
    List,
    /// Create a terminal in a thread's directory (or --cwd) and print its id.
    Create {
        #[arg(long)]
        thread: Option<String>,
        #[arg(long)]
        cwd: Option<String>,
    },
    /// Send a line of input to a terminal.
    Send {
        terminal: String,
        input: Vec<String>,
    },
    /// Stream a terminal's output to stdout (replays scrollback first). Ctrl-C to stop.
    Attach {
        terminal: String,
    },
    /// Create a terminal, run one command, print its output for a few seconds, close it.
    Run {
        #[arg(long)]
        thread: Option<String>,
        #[arg(long)]
        cwd: Option<String>,
        #[arg(long, default_value_t = 3)]
        seconds: u64,
        command: Vec<String>,
    },
    Close {
        terminal: String,
    },
}

#[derive(Subcommand)]
enum ApprovalsCmd {
    /// Submit a provider-native JSON answer to a question or form.
    Answer {
        id: String,
        response: String,
    },
    List,
    Allow {
        id: String,
        #[arg(long)]
        always: bool,
    },
    Deny {
        id: String,
        #[arg(long)]
        reason: Option<String>,
    },
}

#[derive(Subcommand)]
enum TasksCmd {
    /// List task history as well as active work.
    List {
        #[arg(long)]
        all: bool,
    },
    /// Stop one task without interrupting its parent thread.
    Stop { task: String },
    /// Move one foreground task to the background.
    Background { task: String },
}

#[derive(Subcommand)]
enum CursorCmd {
    /// Install the exact SDK version supported by this Kybern build (requires Node.js and npm).
    Install,
    /// Sign in in your browser. This is separate from Cursor CLI sign-in.
    Login,
    /// Show SDK sign-in status without exposing credentials.
    Status,
    /// Forget the SDK browser login. Running chats retain their current credential until closed.
    Logout,
}

async fn preview_cmd(client: &Client, cmd: PreviewCmd, json: bool) -> Result<()> {
    match cmd {
        PreviewCmd::Servers { thread } => {
            let thread_id = thread.map(|id| id.parse()).transpose()?;
            let r = client.call::<PreviewServersList>(PreviewServersListParams { thread_id }).await?;
            if json {
                println!("{}", serde_json::to_string_pretty(&r)?);
            } else if r.servers.is_empty() {
                println!("No local web servers found.");
            } else {
                for server in r.servers {
                    println!(
                        "{:<28} {}{:<10} {:<16} {}",
                        server.url,
                        if server.in_project { "* " } else { "  " },
                        server.framework.unwrap_or_default(),
                        server.process_name.unwrap_or_default(),
                        server.title.or(server.cwd).unwrap_or_default()
                    );
                }
            }
        }
        PreviewCmd::Open { thread, target, allow_folder } => {
            let r =
                client.call::<PreviewOpen>(PreviewOpenParams { thread_id: thread.parse()?, target, allow_folder, proxy: false }).await?;
            if json {
                println!("{}", serde_json::to_string_pretty(&r)?);
            } else if let Some(request) = &r.needs_permission {
                println!("Needs permission to preview files in {}. Run again with --allow-folder.", request.folder);
            } else if let Some(path) = &r.path {
                println!("{path}");
            } else {
                match &r.target {
                    PreviewTargetInfo::Server { url, .. } => println!("{url}"),
                    PreviewTargetInfo::External { url } => println!("{url} (opens in your browser)"),
                    PreviewTargetInfo::File { path, .. } => println!("{path}"),
                }
            }
        }
        PreviewCmd::Server { url, proxy } => {
            let result = client.call::<PreviewServerOpen>(PreviewServerOpenParams { url, proxy }).await?;
            if json {
                println!("{}", serde_json::to_string_pretty(&result)?);
            } else if let Some(path) = result.path {
                println!("{path}");
            } else if let PreviewTargetInfo::Server { url, .. } = result.target {
                println!("{url}");
            }
        }
        PreviewCmd::Folders { remove } => {
            let mut settings = client.call::<SettingsGet>(Empty {}).await?;
            if let Some(folder) = remove {
                let wanted = std::fs::canonicalize(&folder).map(|p| p.to_string_lossy().into_owned()).unwrap_or(folder.clone());
                let before = settings.preview_allowed_folders.len();
                settings.preview_allowed_folders.retain(|stored| *stored != folder && *stored != wanted);
                if settings.preview_allowed_folders.len() == before {
                    anyhow::bail!("{folder} is not an allowed preview folder. Run `kybern preview folders` to see the list.");
                }
                settings = client.call::<SettingsUpdate>(SettingsUpdateParams { settings }).await?;
            }
            if json {
                println!("{}", serde_json::to_string_pretty(&settings.preview_allowed_folders)?);
            } else if settings.preview_allowed_folders.is_empty() {
                println!("No folders are allowed for preview.");
            } else {
                for folder in &settings.preview_allowed_folders {
                    println!("{folder}");
                }
            }
        }
        PreviewCmd::Probe { address } => {
            let r = client.call::<PreviewProbe>(PreviewProbeParams { url: address }).await?;
            if json {
                println!("{}", serde_json::to_string_pretty(&r)?);
            } else {
                match (r.reachable, r.error) {
                    (true, _) => println!("reachable (status {})", r.status.map_or_else(|| "?".into(), |s| s.to_string())),
                    (false, Some(error)) => println!("not reachable: {}", serde_json::to_value(error)?.as_str().unwrap_or("error")),
                    (false, None) => println!("not reachable"),
                }
                if let Some(title) = &r.title {
                    println!("title: {title}");
                }
                if let Some(location) = &r.location {
                    println!("redirects to: {location}");
                }
                if let Some(block) = &r.blocked_by {
                    println!("blocks embedding: {}: {}", block.header, block.value);
                }
            }
        }
    }
    Ok(())
}

async fn cursor_setup(cmd: &CursorCmd) -> Result<()> {
    let context = kybern_drivers::ProbeContext::default();
    let action = match cmd {
        CursorCmd::Install => {
            eprintln!("Installing Cursor SDK {}…", kybern_drivers::cursor::SDK_VERSION);
            let directory = kybern_drivers::cursor::install(&context).await?;
            println!("Cursor SDK installed at {}. Run `kybern cursor login` to sign in.", directory.display());
            return Ok(());
        }
        CursorCmd::Login => "login",
        CursorCmd::Status => "status",
        CursorCmd::Logout => "logout",
    };
    let mut command = kybern_drivers::cursor::setup_command(&context, action)?;
    command
        .stdin(std::process::Stdio::inherit())
        .stdout(std::process::Stdio::inherit())
        .stderr(std::process::Stdio::inherit())
        .kill_on_drop(true);
    let mut child = command.spawn()?;
    let status = child.wait().await?;
    if !status.success() {
        anyhow::bail!("Cursor SDK {action} failed. Resolve the error above and try again.");
    }
    Ok(())
}

fn parse_mode(s: &str) -> Result<PermissionMode, String> {
    serde_json::from_value(serde_json::Value::String(s.to_string()))
        .map_err(|_| format!("unknown mode {s}; use supervised|accept-edits|auto|full-access"))
}

/// Run the CLI with the process arguments. `kybern` is a thin wrapper around this.
#[tokio::main]
pub async fn run() -> Result<()> {
    let cli = Cli::parse();
    // SDK setup is local and must work before a daemon has started.
    if let Cmd::Cursor { cmd } = &cli.cmd {
        return cursor_setup(cmd).await;
    }
    // The helper talks to its own session's endpoint, not to a client connection.
    if let Cmd::Askpass { prompt } = &cli.cmd {
        return requests::askpass(prompt).await;
    }
    // Speaks MCP on stdio to `agy` and to its own session's gateway.
    if matches!(cli.cmd, Cmd::McpBridge) {
        return mcp_bridge::run().await;
    }
    // Local too: the GPUIX client calls this to keep device tokens in the platform store.
    if let Cmd::Credentials { cmd } = &cli.cmd {
        let code = credentials::run(cmd);
        std::process::exit(code);
    }
    let ep = Endpoint::resolve(cli.url.clone(), cli.token.clone(), cli.data_dir.clone())?;
    let explicit = cli.url.is_some() || cli.data_dir.is_some() || std::env::var_os("KYBERN_URL").is_some();
    let client = Client::connect(&ep).await.map_err(|error| {
        if explicit {
            error.context(format!("the daemon at {} did not answer; if it stopped for `kybern machines rejoin`, start it again", ep.url))
        } else {
            error
        }
    })?;
    let json = cli.json;

    match cli.cmd {
        Cmd::Cursor { .. } => unreachable!("local Cursor setup handled before connecting"),
        Cmd::Askpass { .. } => unreachable!("askpass handled before connecting"),
        Cmd::McpBridge => unreachable!("mcp-bridge handled before connecting"),
        Cmd::Credentials { .. } => unreachable!("credentials handled before connecting"),
        Cmd::Requests { cmd } => requests::requests(&client, cmd).await?,
        Cmd::Coordinator { cmd } => coordinator::coordinator(&client, cmd).await?,
        Cmd::History { cmd } => coordinator::history(&client, cmd).await?,
        Cmd::Info => {
            let info = client.call::<DaemonInfoMethod>(Empty {}).await?;
            if json { println!("{}", serde_json::to_string_pretty(&info)?) } else { render::info(&info) }
        }
        Cmd::Activity => {
            let activity = client.call::<DaemonActivityMethod>(Empty {}).await?;
            if json { println!("{}", serde_json::to_string_pretty(&activity)?) } else { render::activity(&activity) }
        }
        Cmd::StopDaemon => {
            client.call::<DaemonShutdown>(Empty {}).await?;
            if json {
                println!("{}", serde_json::to_string(&Empty {})?);
            } else {
                println!("kybernd stopping");
            }
        }
        Cmd::Providers { project, refresh } => {
            let project_id = match project {
                Some(project) => Some(resolve_project(&client, &project, false).await?),
                None => None,
            };
            let r = client.call::<ProvidersList>(ProvidersListParams { project_id, force_refresh: refresh }).await?;
            if json { println!("{}", serde_json::to_string_pretty(&r)?) } else { render::providers(&r.providers) }
        }
        Cmd::Accounts { cmd } => match cmd {
            AccountsCmd::List { provider } => {
                match client.call::<AccountsList>(AccountsListParams { kind: Some(provider), refresh: false }).await {
                    Ok(list) if json => println!("{}", serde_json::to_string_pretty(&list)?),
                    Ok(list) => accounts::render_list(&list.accounts),
                    // An older daemon has no identity probe: show the stored settings.
                    Err(_) => {
                        let settings = client.call::<SettingsGet>(Empty {}).await?;
                        let provider_settings = settings.providers.get(&provider).cloned().unwrap_or_default();
                        println!("{}", serde_json::to_string_pretty(&provider_settings)?);
                    }
                }
            }
            AccountsCmd::Login { provider, account, mode, directory, upstream } => {
                accounts::login(&client, provider, account, mode, directory, upstream).await?;
            }
            AccountsCmd::Rename { provider, account, name } => {
                let summary = client
                    .call::<AccountsUpdate>(AccountsUpdateParams { kind: provider, instance: account, name: Some(name), color: None })
                    .await?;
                println!("{}", serde_json::to_string_pretty(&summary)?);
            }
            AccountsCmd::Color { provider, account, color } => {
                let summary = client
                    .call::<AccountsUpdate>(AccountsUpdateParams { kind: provider, instance: account, name: None, color: Some(color) })
                    .await?;
                println!("{}", serde_json::to_string_pretty(&summary)?);
            }
            AccountsCmd::SignOut { provider, account } => {
                let summary = client.call::<AccountsSignOut>(ProviderInstance { kind: provider, instance: account }).await?;
                println!("{}", serde_json::to_string_pretty(&summary)?);
            }
            AccountsCmd::Remove { provider, account } => {
                client.call::<AccountsRemove>(ProviderInstance { kind: provider, instance: account }).await?;
                println!("{}", serde_json::to_string(&Empty {})?);
            }
            AccountsCmd::Create { provider, name, directory } => {
                let account = client.call::<AccountsCreate>(AccountsCreateParams { kind: provider, name, directory }).await?;
                println!("{}", serde_json::to_string_pretty(&account)?);
            }
            AccountsCmd::SignIn { provider, account } => {
                let terminal = client.call::<AccountsSignIn>(ProviderInstance { kind: provider, instance: account }).await?;
                println!("{}", serde_json::to_string_pretty(&terminal)?);
            }
            AccountsCmd::Usage { provider, account } => {
                let usage = client.call::<AccountsUsage>(ProviderInstance { kind: provider, instance: account }).await?;
                println!("{}", serde_json::to_string_pretty(&usage)?);
            }
            AccountsCmd::Catalog { provider, account, project, refresh } => {
                let project_id = match project {
                    Some(project) => Some(resolve_project(&client, &project, false).await?),
                    None => None,
                };
                let status = client
                    .call::<AccountsCatalog>(AccountsCatalogParams {
                        provider: ProviderInstance { kind: provider, instance: account },
                        project_id,
                        force_refresh: refresh,
                    })
                    .await?;
                println!("{}", serde_json::to_string_pretty(&status)?);
            }
            AccountsCmd::Default { provider, account, project } => {
                let mut settings = client.call::<SettingsGet>(Empty {}).await?;
                let path = match project {
                    Some(project) => {
                        let id = resolve_project(&client, &project, false).await?;
                        Some(
                            client
                                .call::<ProjectsList>(ProjectsListParams::default())
                                .await?
                                .projects
                                .into_iter()
                                .find(|p| p.id == id)
                                .ok_or_else(|| anyhow!("project not found"))?
                                .path,
                        )
                    }
                    None => None,
                };
                let provider_settings = settings.providers.entry(provider).or_default();
                if let Some(path) = path {
                    if account == "inherit" {
                        provider_settings.project_accounts.remove(&path);
                    } else {
                        provider_settings.project_accounts.insert(path, account);
                    }
                } else {
                    provider_settings.default_account = Some(account);
                }
                println!("{}", serde_json::to_string_pretty(&client.call::<SettingsUpdate>(SettingsUpdateParams { settings }).await?)?);
            }
        },
        Cmd::Target { thread, provider, account, model, effort, inherit } => {
            let state = client.call::<ThreadsTargetGet>(ThreadsInterruptParams { thread_id: thread }).await?;
            if provider.is_some() || account.is_some() || model.is_some() || effort.is_some() || inherit {
                let mut target = state.target;
                if let Some(provider) = provider {
                    target.provider.kind = provider;
                    target.provider.instance = "default".into();
                    target.model = None;
                    target.effort = None;
                }
                if let Some(account) = account {
                    target.provider.instance = account;
                }
                if let Some(model) = model {
                    target.model = (!model.is_empty()).then_some(model);
                }
                if let Some(effort) = effort {
                    target.effort = (!effort.is_empty()).then_some(effort);
                }
                println!(
                    "{}",
                    serde_json::to_string_pretty(
                        &client
                            .call::<ThreadsTargetSet>(ThreadTargetParams { thread_id: thread, target, inherit_account: inherit })
                            .await?
                    )?
                );
            } else {
                println!("{}", serde_json::to_string_pretty(&state)?);
            }
        }
        Cmd::SwitchContinue { thread, provider, account, message_id } => {
            println!(
                "{}",
                serde_json::to_string_pretty(
                    &client
                        .call::<ThreadsSwitchContinue>(ThreadsSwitchContinueParams {
                            thread_id: thread,
                            provider: ProviderInstance { kind: provider, instance: account },
                            message_id: message_id.unwrap_or_else(uuid::Uuid::now_v7)
                        })
                        .await?
                )?
            );
        }
        Cmd::ApplyPermissions { thread } => {
            println!(
                "{}",
                serde_json::to_string_pretty(&client.call::<ThreadsPermissionsApply>(ThreadsInterruptParams { thread_id: thread }).await?)?
            );
        }
        Cmd::Sessions { cmd: Some(cmd), .. } => sessions_command(&client, cmd, json).await?,
        Cmd::Sessions { cmd: None, provider, project, cursor, query } => {
            let provider = provider.ok_or_else(|| anyhow!("Choose a harness with --provider, or run `kybern sessions scan`."))?;
            let project_id = match project {
                Some(project) => Some(resolve_project(&client, &project, false).await?),
                None => None,
            };
            let result = client.call::<SessionsList>(SessionsListParams { provider, project_id, cursor, query }).await?;
            if json {
                println!("{}", serde_json::to_string_pretty(&result)?);
            } else {
                for session in result.sessions {
                    println!("{}  {}  {}", session.id, session.title, session.cwd);
                }
                if let Some(cursor) = result.next_cursor {
                    println!("More sessions: --cursor {cursor}");
                }
            }
        }
        Cmd::Resume { provider, project, session_id } => {
            let project_id = match project {
                Some(project) => Some(resolve_project(&client, &project, false).await?),
                None => None,
            };
            let thread = client.call::<SessionsResume>(SessionsResumeParams { provider, session_id, project_id }).await?;
            if json {
                println!("{}", serde_json::to_string_pretty(&thread)?);
            } else {
                println!("{}", thread.id);
            }
        }
        Cmd::HarnessUpdates { run } => {
            if let Some(kind) = run {
                let result = client.call::<HarnessUpdatesRun>(HarnessUpdateParams { kind }).await?;
                println!("{}", serde_json::to_string_pretty(&result)?);
            } else {
                let result = client.call::<HarnessUpdatesList>(Empty {}).await?;
                println!("{}", serde_json::to_string_pretty(&result)?);
            }
        }
        Cmd::DaemonUpdate { check, run } => {
            let result = if run {
                client.call::<DaemonUpdateRun>(Empty {}).await?
            } else if check {
                client.call::<DaemonUpdateCheck>(Empty {}).await?
            } else {
                client.call::<DaemonUpdateStatusMethod>(Empty {}).await?
            };
            println!("{}", serde_json::to_string_pretty(&result)?);
        }
        Cmd::Projects { cmd } => match cmd {
            ProjectsCmd::List { cached, space } => {
                let mut r = client.call::<ProjectsList>(ProjectsListParams { refresh_git: cached.then_some(false) }).await?;
                // An older daemon has no spaces: the column is simply absent.
                let spaces = spaces::list(&client).await.unwrap_or_default();
                if let Some(key) = space {
                    let space_id = spaces::resolve_space(&spaces, &key)?;
                    r.projects.retain(|project| project.space_id == Some(space_id));
                }
                if json { println!("{}", serde_json::to_string_pretty(&r)?) } else { render::projects(&r.projects, &spaces) }
            }
            ProjectsCmd::Browse { path } => {
                let result = client.call::<ProjectsBrowse>(ProjectsBrowseParams { path }).await?;
                println!("{}", serde_json::to_string_pretty(&result)?);
            }
            ProjectsCmd::Add { path, name, space, group, members } => {
                let space_id = match space {
                    Some(key) => Some(spaces::resolve_space(&spaces::list(&client).await?, &key)?),
                    None => None,
                };
                let p = client
                    .call::<ProjectsAdd>(ProjectsAddParams {
                        path: host_path(&client, &path.to_string_lossy()).await?,
                        name,
                        space_id,
                        kind: group.then_some(ProjectKind::Group),
                        members: group.then_some(members),
                    })
                    .await?;
                if json {
                    println!("{}", serde_json::to_string_pretty(&p)?)
                } else {
                    println!("{}  {}  {}{}", p.id, p.name, p.path, render::repo_count(&p))
                }
            }
            ProjectsCmd::Scan { path, include, depth } => {
                let result = client
                    .call::<ProjectsScanRepos>(ProjectsScanReposParams {
                        path: host_path(&client, &path.to_string_lossy()).await?,
                        include,
                        depth,
                    })
                    .await?;
                if json { println!("{}", serde_json::to_string_pretty(&result)?) } else { render::scan(&result) }
            }
            ProjectsCmd::Members { project, set, copy } => {
                let project_id = resolve_project(&client, &project, false).await?;
                if set.is_some() || copy.is_some() {
                    client
                        .call::<ProjectsUpdate>(ProjectsUpdateParams {
                            project_id,
                            name: None,
                            worktrees_default: None,
                            task_prefix: None,
                            brief_in_all_threads: None,
                            coordinator: None,
                            space_id: None,
                            members: set.map(|paths| paths.into_iter().map(|p| p.trim().to_string()).filter(|p| !p.is_empty()).collect()),
                            worktree_copy: copy
                                .map(|names| names.into_iter().map(|n| n.trim().to_string()).filter(|n| !n.is_empty()).collect()),
                            instructions_offer_dismissed: None,
                        })
                        .await?;
                }
                let status = client.call::<ProjectsMembersStatus>(ProjectsMembersStatusParams { project_id }).await?;
                if json {
                    println!("{}", serde_json::to_string_pretty(&status)?)
                } else {
                    let project = client.call::<ProjectsList>(ProjectsListParams { refresh_git: Some(false) }).await?.projects;
                    render::members(&status, project.iter().find(|p| p.id == project_id))
                }
            }
            ProjectsCmd::Provision { project, machine, path, env_files, no_env, plan } => {
                let project_id = resolve_project(&client, &project, false).await?;
                if plan {
                    let result = client.call::<ProjectsProvisionPlan>(ProjectsProvisionPlanParams { project_id, machine, path }).await?;
                    println!("{}", serde_json::to_string_pretty(&result)?);
                } else {
                    let env_files = if no_env { Some(Vec::new()) } else { (!env_files.is_empty()).then_some(env_files) };
                    let result = client
                        .call::<ProjectsProvision>(ProjectsProvisionParams {
                            project_id,
                            machine,
                            choice: ProvisionChoice { path, env_files },
                            operation_id: None,
                        })
                        .await?;
                    println!("{}", serde_json::to_string_pretty(&result)?);
                }
            }
            ProjectsCmd::ProvisionStatus { operation_id } => {
                let result = client.call::<ProjectsProvisionStatusMethod>(ProjectsProvisionStatusParams { operation_id }).await?;
                println!("{}", serde_json::to_string_pretty(&result)?);
            }
            ProjectsCmd::Map { project } => {
                let project_id = resolve_project(&client, &project, false).await?;
                let result = client.call::<ProjectsMap>(ProjectsMapParams { project_id }).await?;
                if json { println!("{}", serde_json::to_string_pretty(&result)?) } else { render::project_map(&result.map) }
            }
            ProjectsCmd::Instructions { project, dismiss } => {
                let project_id = resolve_project(&client, &project, false).await?;
                if dismiss {
                    client
                        .call::<ProjectsUpdate>(ProjectsUpdateParams {
                            project_id,
                            name: None,
                            worktrees_default: None,
                            task_prefix: None,
                            brief_in_all_threads: None,
                            coordinator: None,
                            space_id: None,
                            members: None,
                            worktree_copy: None,
                            instructions_offer_dismissed: Some(true),
                        })
                        .await?;
                    println!("offer hidden");
                } else {
                    let r = client.call::<ProjectsInstructionsCreate>(ProjectsInstructionsCreateParams { project_id }).await?;
                    if json {
                        println!("{}", serde_json::to_string_pretty(&r)?)
                    } else if r.created.is_empty() {
                        println!("AGENTS.md and CLAUDE.md already exist")
                    } else {
                        println!("created {}", r.created.join(", "))
                    }
                }
            }
            ProjectsCmd::Move { project, space } => {
                let project_id = resolve_project(&client, &project, false).await?;
                let all = spaces::list(&client).await?;
                let space_id = spaces::resolve_space(&all, &space)?;
                let space_name = all.iter().find(|s| s.id == space_id).map_or_else(|| space_id.to_string(), |s| s.name.clone());
                let p = client
                    .call::<ProjectsUpdate>(ProjectsUpdateParams {
                        project_id,
                        name: None,
                        worktrees_default: None,
                        task_prefix: None,
                        brief_in_all_threads: None,
                        coordinator: None,
                        space_id: Some(space_id),
                        members: None,
                        worktree_copy: None,
                        instructions_offer_dismissed: None,
                    })
                    .await?;
                if json { println!("{}", serde_json::to_string_pretty(&p)?) } else { println!("{}  moved to {space_name}", p.name) }
            }
            ProjectsCmd::Remove { id } => {
                client.call::<ProjectsRemove>(ProjectsRemoveParams { project_id: id.parse()? }).await?;
                println!("removed");
            }
        },
        Cmd::Spaces { cmd } => spaces::run(&client, cmd, json).await?,
        Cmd::Machines { cmd } => machines::run(&client, cmd, json).await?,
        Cmd::Fork(args) => fork::run(&client, args, json).await?,
        Cmd::Continue(args) => continue_on::run(&client, args, json).await?,
        Cmd::ContinueAbandon(args) => continue_on::abandon(&client, args, json).await?,
        Cmd::Outbox { cmd } => outbox::run(&client, cmd, json).await?,
        Cmd::Threads { project, archived } => {
            let project_id = match project {
                Some(p) => Some(resolve_project(&client, &p, false).await?),
                None => None,
            };
            let r = client.call::<ThreadsList>(ThreadsListParams { project_id, include_archived: archived, ..Default::default() }).await?;
            if json { println!("{}", serde_json::to_string_pretty(&r)?) } else { render::threads(&r.threads) }
        }
        Cmd::New { project, provider, model, effort, mode, worktree, branch, detach, prompt } => {
            let project_id = resolve_project(&client, &project, true).await?;
            let prompt = join_prompt(prompt)?;
            let sub = if detach {
                None
            } else {
                Some(
                    client
                        .call::<EventsSubscribe>(EventsSubscribeParams { thread_id: None, after_seq: None, include_tool_output: None })
                        .await?,
                )
            };
            let thread = client
                .call::<ThreadsCreate>(ThreadsCreateParams {
                    pin_account: false,
                    project_id: Some(project_id),
                    provider: ProviderInstance::default_for(provider.parse().map_err(|e: String| anyhow!(e))?),
                    model,
                    effort,
                    permission_mode: mode,
                    use_worktree: if worktree { Some(true) } else { None },
                    base_branch: branch,
                    title: None,
                    message: Some(UserMessage::text(prompt)),
                    thread_id: None,
                    message_id: None,
                    members: None,
                    base_branches: None,
                })
                .await?;
            eprintln!("thread {}", thread.id);
            if let Some(sub) = sub {
                render::follow_turn(&client, sub.subscription_id, thread.id, json).await?;
            }
        }
        Cmd::Send { thread, detach, prompt } => {
            let thread_id: ThreadId = thread.parse().context("thread id must be a UUID")?;
            let prompt = join_prompt(prompt)?;
            let sub = if detach {
                None
            } else {
                Some(
                    client
                        .call::<EventsSubscribe>(EventsSubscribeParams {
                            thread_id: Some(thread_id),
                            after_seq: None,
                            include_tool_output: None,
                        })
                        .await?,
                )
            };
            let r = client
                .call::<ThreadsSend>(ThreadsSendParams {
                    thread_id,
                    message: UserMessage::text(prompt),
                    message_id: Some(uuid::Uuid::now_v7()),
                })
                .await?;
            eprintln!("turn {}", r.turn_id);
            if let Some(sub) = sub {
                render::follow_turn(&client, sub.subscription_id, thread_id, json).await?;
            }
        }
        Cmd::SubagentSend { thread, message_id, prompt } => {
            let result = client
                .call::<SubagentsSend>(QueuedMessage {
                    thread_id: thread.parse()?,
                    id: message_id.unwrap_or_else(uuid::Uuid::now_v7),
                    message: UserMessage::text(join_prompt(prompt)?),
                })
                .await?;
            println!("{}", serde_json::to_string_pretty(&result)?);
        }
        Cmd::SubagentMessages { thread } => {
            let result = client.call::<SubagentsMessages>(ThreadsInterruptParams { thread_id: thread.parse()? }).await?;
            println!("{}", serde_json::to_string_pretty(&result)?);
        }
        Cmd::SubagentSendToParent { thread, message_id } => {
            let result =
                client.call::<SubagentsSendToParent>(SubagentMessageActionParams { thread_id: thread.parse()?, message_id }).await?;
            println!("{}", serde_json::to_string_pretty(&result)?);
        }
        Cmd::Steer { thread, prompt } => {
            let result = client
                .call::<ThreadsSteer>(QueuedMessage {
                    thread_id: thread.parse()?,
                    id: uuid::Uuid::now_v7(),
                    message: UserMessage::text(join_prompt(prompt)?),
                })
                .await?;
            println!("{}", serde_json::to_string_pretty(&result)?);
        }
        Cmd::Notes { thread, file } => {
            let thread_id = thread.parse()?;
            let mut notes = client.call::<ThreadNotesGet>(ThreadsInterruptParams { thread_id }).await?;
            if let Some(file) = file {
                notes = client
                    .call::<ThreadNotesSet>(ThreadNotesSetParams {
                        thread_id,
                        text: std::fs::read_to_string(file)?,
                        expected_revision: notes.revision,
                    })
                    .await?;
            }
            if json {
                println!("{}", serde_json::to_string_pretty(&notes)?);
            } else {
                println!("{}", notes.text);
            }
        }
        Cmd::Note { cmd } => note_command(&client, cmd, json).await?,
        Cmd::Task { cmd } => task_command(&client, cmd, json).await?,
        Cmd::Queue { cmd } => match cmd {
            QueueCmd::List { thread } => {
                let result = client.call::<QueueList>(QueueListParams { thread_id: thread.map(|id| id.parse()).transpose()? }).await?;
                println!("{}", serde_json::to_string_pretty(&result)?);
            }
            QueueCmd::Add { thread, prompt } => {
                let id = uuid::Uuid::now_v7();
                client
                    .call::<QueueAdd>(QueuedMessage { id, thread_id: thread.parse()?, message: UserMessage::text(prompt.join(" ")) })
                    .await?;
                println!("{id}");
            }
            QueueCmd::Remove { thread, id } => {
                client.call::<QueueRemove>(QueueRemoveParams { thread_id: thread.parse()?, id: id.parse()? }).await?;
            }
            QueueCmd::Edit { thread, id, prompt } => {
                let thread_id = thread.parse()?;
                let id: MessageId = id.parse()?;
                let mut item = client
                    .call::<QueueList>(QueueListParams { thread_id: Some(thread_id) })
                    .await?
                    .messages
                    .into_iter()
                    .find(|item| item.id == id)
                    .ok_or_else(|| anyhow!("Queued message not found."))?;
                let text = join_prompt(prompt)?;
                item.message.parts.retain(|part| !matches!(part, ContentPart::Text { .. }));
                item.message.parts.insert(0, ContentPart::Text { text });
                client.call::<QueueUpdate>(item).await?;
            }
        },
        Cmd::Messages { cmd } => match cmd {
            MessagesCmd::List { thread, states } => {
                let result = client
                    .call::<ThreadMessagesList>(ThreadMessagesListParams {
                        thread_id: thread.parse()?,
                        states: (!states.is_empty()).then_some(states),
                    })
                    .await?;
                if json {
                    println!("{}", serde_json::to_string_pretty(&result)?);
                } else {
                    render::thread_messages(&result.messages);
                }
            }
            MessagesCmd::Deliver { message } => {
                let record = client.call::<ThreadMessagesDeliver>(ThreadMessageIdParams { message_id: message.parse()? }).await?;
                if json {
                    println!("{}", serde_json::to_string_pretty(&record)?);
                } else {
                    render::thread_messages(&[record]);
                }
            }
            MessagesCmd::Dismiss { message } => {
                let record = client.call::<ThreadMessagesDismiss>(ThreadMessageIdParams { message_id: message.parse()? }).await?;
                if json {
                    println!("{}", serde_json::to_string_pretty(&record)?);
                } else {
                    render::thread_messages(&[record]);
                }
            }
        },
        Cmd::Delegations { cmd } => match cmd {
            DelegationsCmd::RemoveWorktree { thread, force } => {
                let thread =
                    client.call::<DelegationsWorktreeRemove>(DelegationsWorktreeRemoveParams { thread_id: thread.parse()?, force }).await?;
                if json {
                    println!("{}", serde_json::to_string_pretty(&thread)?);
                } else {
                    render::threads(&[thread]);
                }
            }
        },
        Cmd::Show { thread, limit, before_seq, through_seq } => {
            let r = client
                .call::<ThreadsGet>(ThreadsGetParams {
                    thread_id: thread.parse()?,
                    transcript_limit: limit,
                    before_seq,
                    through_seq,
                    ..Default::default()
                })
                .await?;
            if json { print_json(&r)? } else { render::transcript(&r) }
        }
        Cmd::RecoverOmpAnswer { thread, turn } => {
            let result = client
                .call::<ThreadsRecoverOmpAnswer>(ThreadsRecoverOmpAnswerParams { thread_id: thread.parse()?, turn_id: turn.parse()? })
                .await?;
            print_json(&result)?;
        }
        Cmd::ToolOutput { thread, tool_call_id, start_seq, through_seq } => {
            let result = client
                .call::<ThreadsToolOutput>(ThreadsToolOutputParams {
                    thread_id: thread.parse()?,
                    tool_call_id,
                    start_seq,
                    through_seq,
                    include_tool_stream: None,
                })
                .await?;
            print_json(&result)?;
        }
        Cmd::Watch { thread, after } => {
            let thread_id = thread.map(|t| t.parse::<ThreadId>()).transpose()?;
            let sub =
                client.call::<EventsSubscribe>(EventsSubscribeParams { thread_id, after_seq: after, include_tool_output: None }).await?;
            render::watch(&client, sub.subscription_id, json).await?;
        }
        Cmd::Release { thread } => {
            client.call::<ThreadsRelease>(ThreadsInterruptParams { thread_id: thread.parse()? }).await?;
        }
        Cmd::Answer { thread, request_id, answers } => {
            client.call::<ThreadsAnswer>(ThreadsAnswerParams { thread_id: thread.parse()?, request_id, answers }).await?;
        }
        Cmd::Compact { thread } => {
            let result = client.call::<ThreadsCompact>(ThreadsInterruptParams { thread_id: thread.parse()? }).await?;
            println!("{}", serde_json::to_string_pretty(&result)?);
        }
        Cmd::Interrupt { thread } => {
            client.call::<ThreadsInterrupt>(ThreadsInterruptParams { thread_id: thread.parse()? }).await?;
            println!("interrupt sent");
        }
        Cmd::Tasks { thread, cmd } => {
            let thread_id = thread.parse()?;
            match cmd.unwrap_or(TasksCmd::List { all: false }) {
                TasksCmd::List { all } => {
                    let result = client.call::<TasksList>(TasksListParams { thread_id, include_completed: all }).await?;
                    if json {
                        println!("{}", serde_json::to_string_pretty(&result)?);
                    } else if result.tasks.is_empty() {
                        println!("no provider tasks");
                    } else {
                        for task in result.tasks {
                            println!(
                                "{:<38} {:<9} {:<11} {}",
                                task.id,
                                format!("{:?}", task.kind).to_lowercase(),
                                format!("{:?}", task.status).to_lowercase(),
                                task.title
                            );
                        }
                    }
                }
                TasksCmd::Stop { task } => {
                    let result = client.call::<TaskStop>(TaskControlParams { thread_id, task_id: task }).await?;
                    if json {
                        println!("{}", serde_json::to_string_pretty(&result)?);
                    } else {
                        println!("stop requested for {}", result.title);
                    }
                }
                TasksCmd::Background { task } => {
                    let result = client.call::<TaskBackground>(TaskControlParams { thread_id, task_id: task }).await?;
                    if json {
                        println!("{}", serde_json::to_string_pretty(&result)?);
                    } else {
                        println!("moved {} to the background", result.title);
                    }
                }
            }
        }
        Cmd::Approvals { cmd } => match cmd.unwrap_or(ApprovalsCmd::List) {
            ApprovalsCmd::List => {
                let r = client.call::<ApprovalsList>(ApprovalsListParams { thread_id: None }).await?;
                if json { println!("{}", serde_json::to_string_pretty(&r)?) } else { render::approvals(&r.approvals) }
            }
            ApprovalsCmd::Answer { id, response } => {
                client
                    .call::<ApprovalsRespond>(ApprovalsRespondParams {
                        approval_id: id.parse()?,
                        decision: ApprovalDecision::Submit { response: serde_json::from_str(&response)? },
                    })
                    .await?;
            }
            ApprovalsCmd::Allow { id, always } => {
                let decision = if always { ApprovalDecision::AllowAlways } else { ApprovalDecision::AllowOnce };
                client.call::<ApprovalsRespond>(ApprovalsRespondParams { approval_id: id.parse()?, decision }).await?;
                println!("allowed");
            }
            ApprovalsCmd::Deny { id, reason } => {
                client
                    .call::<ApprovalsRespond>(ApprovalsRespondParams {
                        approval_id: id.parse()?,
                        decision: ApprovalDecision::Deny { reason },
                    })
                    .await?;
                println!("denied");
            }
        },
        Cmd::Worktree { thread, remove, force, delete_branch } => {
            let thread_id = thread.parse()?;
            let result = if remove {
                client.call::<WorktreeRemove>(WorktreeRemoveParams { thread_id, force, delete_branch }).await?
            } else {
                client.call::<WorktreeInspect>(WorktreeInspectParams { thread_id }).await?
            };
            println!("{}", serde_json::to_string_pretty(&result)?);
        }
        Cmd::Archive { thread } => {
            client.call::<ThreadsArchive>(ThreadsArchiveParams { thread_id: thread.parse()? }).await?;
            println!("archived");
        }
        Cmd::Checkpoints { thread } => {
            let r = client.call::<ThreadsCheckpoints>(ThreadsCheckpointsParams { thread_id: thread.parse()? }).await?;
            if json {
                println!("{}", serde_json::to_string_pretty(&r)?)
            } else {
                for c in r.checkpoints {
                    println!(
                        "{}  {} → {}  {}",
                        c.turn_id,
                        &c.before[..10],
                        c.after.as_deref().map(|a| &a[..10]).unwrap_or("(running)"),
                        c.created_at.to_rfc3339()
                    );
                }
            }
        }
        Cmd::Diff { thread, turn, stat } => {
            let d = client
                .call::<ThreadsDiff>(ThreadsDiffParams {
                    thread_id: thread.parse()?,
                    turn_id: turn.map(|t| t.parse()).transpose()?,
                    include_patch: !stat,
                    path: None,
                })
                .await?;
            if json {
                print_json(&d)?
            } else {
                for f in &d.files {
                    println!("{:<10} +{:<5} -{:<5} {}", format!("{:?}", f.status).to_lowercase(), f.additions, f.deletions, f.path);
                }
                if !stat && !d.patch.is_empty() {
                    println!("\n{}", d.patch);
                    if d.patch_truncated {
                        eprintln!("\n(diff output truncated at 1 MiB; use git diff for the full patch)");
                    }
                }
            }
        }
        Cmd::Revert { thread, turn } => {
            let r = client.call::<ThreadsRevert>(ThreadsRevertParams { thread_id: thread.parse()?, turn_id: turn.parse()? }).await?;
            println!("working tree restored to {}{}", &r.commit[..10], if r.conversation_rewound { " (conversation rewound)" } else { "" });
        }
        Cmd::Settings { cmd } => match cmd.unwrap_or(SettingsCmd::Show) {
            SettingsCmd::Show => println!("{}", serde_json::to_string_pretty(&client.call::<SettingsGet>(Empty {}).await?)?),
            SettingsCmd::Set { file } => {
                let text = if file == "-" { std::io::read_to_string(std::io::stdin())? } else { std::fs::read_to_string(&file)? };
                let settings: Settings = serde_json::from_str(&text)?;
                let r = client.call::<SettingsUpdate>(SettingsUpdateParams { settings }).await?;
                println!("{}", serde_json::to_string_pretty(&r)?);
            }
        },
        Cmd::SecretStore { thread, key, file, add_to_gitignore } => {
            let mut value = String::new();
            std::io::Read::read_to_string(&mut std::io::stdin(), &mut value)?;
            let value = value.trim_end_matches(['\n', '\r']).to_owned();
            let result = client
                .call::<SecretsStore>(SecretsStoreParams {
                    workspace: SecretsWorkspace { thread_id: Some(thread.parse()?), project_id: None },
                    env_key: key,
                    value,
                    file: Some(file),
                    add_to_gitignore,
                })
                .await?;
            println!(
                "Stored {} in {}{}{}.",
                result.env_key,
                result.file,
                if result.replaced { " (replaced)" } else { "" },
                if result.gitignore_updated { " and added it to .gitignore" } else { "" }
            );
        }
        Cmd::Computer { cmd } => {
            let status = match cmd.unwrap_or(ComputerCmd::Doctor) {
                ComputerCmd::Status => {
                    println!("{}", serde_json::to_string_pretty(&client.call::<ComputerStatusGet>(Empty {}).await?)?);
                    return Ok(());
                }
                ComputerCmd::Doctor => client.call::<ComputerStatusGet>(Empty {}).await?,
                ComputerCmd::Frame { thread, out } => {
                    use base64::Engine;
                    let result = client.call::<ComputerFrameGet>(ComputerFrameParams { thread_id: thread.parse()?, after: None }).await?;
                    let Some(frame) = result.frame else {
                        println!(
                            "No frame yet. Frames are captured while Kybern shows the live view, or when the agent takes a screenshot."
                        );
                        return Ok(());
                    };
                    std::fs::write(&out, base64::engine::general_purpose::STANDARD.decode(frame.data)?)?;
                    println!(
                        "{} · {}{} → {out}",
                        frame.app,
                        frame.action.as_deref().unwrap_or("frame"),
                        frame.title.map(|title| format!(" · {title}")).unwrap_or_default()
                    );
                    return Ok(());
                }
                ComputerCmd::Notes { app, set, clear } => {
                    let notes = client.call::<ComputerNotesList>(Empty {}).await?.notes;
                    let Some(app) = app else {
                        if notes.is_empty() {
                            println!("No app notes yet. Agents save them when an app needs a non-obvious approach.");
                        }
                        for note in &notes {
                            let lines = note.text.lines().count();
                            println!("{} ({}) · {lines} line{}", note.app, note.bundle_id, if lines == 1 { "" } else { "s" });
                        }
                        return Ok(());
                    };
                    let found = notes.iter().find(|note| note.bundle_id == app || note.app.eq_ignore_ascii_case(&app));
                    if set.is_none() && !clear {
                        match found {
                            Some(note) => println!("# {} ({})\n\n{}", note.app, note.bundle_id, note.text),
                            None => println!("No notes on {app}."),
                        }
                        return Ok(());
                    }
                    // A new note needs the bundle id; an existing one can be named.
                    let bundle_id = found.map(|note| note.bundle_id.clone()).unwrap_or(app.clone());
                    let text = match set.as_deref() {
                        Some("-") => {
                            let mut text = String::new();
                            std::io::Read::read_to_string(&mut std::io::stdin(), &mut text)?;
                            text
                        }
                        Some(path) => std::fs::read_to_string(path)?,
                        None => String::new(),
                    };
                    client
                        .call::<ComputerNoteSet>(ComputerNoteSetParams { bundle_id: bundle_id.clone(), text: text.clone(), app: None })
                        .await?;
                    println!("{} the notes on {bundle_id}.", if text.trim().is_empty() { "Cleared" } else { "Replaced" });
                    return Ok(());
                }
                ComputerCmd::Install => {
                    let name = computer_driver_name(&client.call::<ComputerStatusGet>(Empty {}).await?);
                    eprintln!("Installing {name}; this can take a few minutes…");
                    client.call::<ComputerSetup>(ComputerSetupParams { action: ComputerSetupAction::Install }).await?
                }
                ComputerCmd::Grant => {
                    let before = client.call::<ComputerStatusGet>(Empty {}).await?;
                    eprintln!("{}", computer_grant_hint(&before));
                    client.call::<ComputerSetup>(ComputerSetupParams { action: ComputerSetupAction::GrantPermissions }).await?
                }
            };
            for check in &status.checks {
                println!("{} {}", if check.ok { "✓" } else { "✗" }, check.message);
                if let Some(fix) = check.fix.as_deref().filter(|_| !check.ok) {
                    println!("  → {fix}");
                }
            }
            if let Some(session) = status.session.as_deref() {
                println!("Session: {session}");
            }
            println!(
                "{}",
                if status.ready {
                    format!("Computer use ({}) is ready for new conversations.", computer_driver_name(&status))
                } else {
                    format!("Computer use ({}) is not ready yet.", computer_driver_name(&status))
                }
            );
        }
        Cmd::Usage { by, days, limits } => {
            if limits {
                let r = client.call::<UsageLimits>(UsageLimitsParams { refresh: true, ..Default::default() }).await?;
                println!("{}", serde_json::to_string_pretty(&r)?);
            } else {
                let group_by = serde_json::from_value(serde_json::Value::String(by))?;
                let since = days.map(|d| chrono::Utc::now() - chrono::Duration::days(d));
                let r = client.call::<UsageSummary>(UsageSummaryParams { since, group_by }).await?;
                if json { println!("{}", serde_json::to_string_pretty(&r)?) } else { render::usage(&r) }
            }
        }
        Cmd::Pair { label, address, tailscale } => {
            // Validate before minting a code, so a typo doesn't waste an invitation.
            let address = address.map(|value| kybern_client::address::normalize(&value)).transpose()?;
            let info = client.call::<DaemonInfoMethod>(Empty {}).await?;
            if tailscale {
                let exposure = client.call::<ExposureSet>(ExposureSetParams { tailscale: true }).await?;
                if !json {
                    eprintln!("Listening on {}", exposure.listeners.join(", "));
                }
            }
            let mut pairing = client.call::<PairingCreate>(PairingCreateParams { label }).await?;
            if let Some(address) = address {
                pairing.endpoints = vec![address];
            }
            let report = kybern_client::pairing::PairingReport::new(&pairing, &info.environment_id)?;
            if json {
                println!("{}", serde_json::to_string_pretty(&report)?);
            } else {
                print!("{}", report.render());
            }
        }
        Cmd::Tokens { cmd } => match cmd.unwrap_or(TokensCmd::List) {
            TokensCmd::List => {
                let r = client.call::<TokensList>(Empty {}).await?;
                for t in r.tokens {
                    println!("{}  {:<20} {}{}", t.id, t.label, t.created_at.format("%Y-%m-%d"), if t.revoked { "  (revoked)" } else { "" });
                }
            }
            TokensCmd::Revoke { id } => {
                client.call::<TokensRevoke>(TokensRevokeParams { token_id: id.parse()? }).await?;
                println!("revoked");
            }
        },
        Cmd::Ports { cmd } => utilities::run_ports(&client, cmd, json).await?,
        Cmd::Files { cmd: Some(cmd), .. } => utilities::run_files(&client, cmd, json).await?,
        Cmd::Files { cmd: None, project, query } => {
            let project = project.ok_or_else(|| anyhow!("Name a project: kybern files <project> [query], or kybern files send --help"))?;
            let r = client.call::<FilesSearch>(FilesSearchParams { project_id: project.parse()?, query, limit: 30 }).await?;
            for f in &r.files {
                println!("{f}");
            }
            eprintln!("{} of {} files", r.files.len(), r.total);
        }
        Cmd::Integrations { cmd } => match cmd {
            IntegrationsCmd::List { project, provider } => {
                let project_id = resolve_project(&client, &project, false).await?;
                let result = client.call::<IntegrationsList>(IntegrationsListParams { project_id, provider }).await?;
                println!("{}", serde_json::to_string_pretty(&result)?);
            }
            IntegrationsCmd::Change { project, id, action, provider, kind, scope } => {
                let project_id = resolve_project(&client, &project, false).await?;
                let result = client
                    .call::<IntegrationChange>(IntegrationChangeParams {
                        project_id,
                        provider,
                        id,
                        scope,
                        action: serde_json::from_value(serde_json::Value::String(action))?,
                        kind: serde_json::from_value(serde_json::Value::String(kind))?,
                    })
                    .await?;
                println!("{}", serde_json::to_string_pretty(&result)?);
            }
            IntegrationsCmd::Login { thread, name } => {
                let result = client.call::<IntegrationLogin>(IntegrationLoginParams { thread_id: thread.parse()?, name }).await?;
                println!("{}", serde_json::to_string_pretty(&result)?);
            }
        },
        Cmd::Preview { cmd } => preview_cmd(&client, cmd, json).await?,
        Cmd::Visuals { cmd } => match cmd {
            VisualsCmd::Publish { thread, path, title, height } => {
                let html = tokio::fs::read_to_string(path).await?;
                print_json(
                    &client
                        .call::<HtmlPublish>(HtmlPublishParams {
                            thread_id: thread.parse()?,
                            html,
                            title,
                            height,
                            await_answer: None,
                            answer_schema: None,
                            timeout_seconds: None,
                        })
                        .await?,
                )?;
            }
            VisualsCmd::Preview { thread, path, width, appearance, screenshot } => {
                let html = tokio::fs::read_to_string(path).await?;
                let mut result = client
                    .call::<HtmlPreview>(HtmlPreviewParams {
                        thread_id: thread.parse()?,
                        html,
                        width: Some(width),
                        appearance: Some(appearance),
                    })
                    .await?;
                use base64::Engine;
                tokio::fs::write(&screenshot, base64::engine::general_purpose::STANDARD.decode(&result.screenshot)?).await?;
                result.screenshot.clear();
                print_json(&result)?;
            }
            VisualsCmd::Read { thread, visual } => {
                let result =
                    client.call::<HtmlRead>(HtmlReadParams { thread_id: thread.parse()?, visual_id: visual, max_bytes: None }).await?;
                if json {
                    print_json(&result)?;
                } else {
                    print!("{}", result.html);
                }
            }
            VisualsCmd::Frame { thread, visual } => {
                print_json(
                    &client.call::<HtmlFrame>(HtmlReadParams { thread_id: thread.parse()?, visual_id: visual, max_bytes: None }).await?,
                )?;
            }
            VisualsCmd::Revoke { thread, ticket } => {
                client.call::<HtmlRevoke>(HtmlRevokeParams { thread_id: thread.parse()?, ticket }).await?;
            }
            VisualsCmd::Snapshot { thread, visual, width, appearance } => {
                let thread_id: ThreadId = thread.parse()?;
                let theme = serde_json::json!({ "appearance": appearance, "variables": { "--background": "transparent" } });
                let result =
                    client.call::<HtmlSnapshot>(HtmlSnapshotParams { thread_id, visual_id: visual, width, theme, lock: None }).await?;
                println!("/threads/{thread_id}/visuals/{visual}/snapshot/{}.png", result.key);
                print_json(&result)?;
            }
        },
        Cmd::Artifacts { cmd } => match cmd {
            ArtifactsCmd::List { thread, before_seq, limit } => {
                let result = client.call::<ArtifactsList>(ArtifactsListParams { thread_id: thread.parse()?, before_seq, limit }).await?;
                println!("{}", serde_json::to_string_pretty(&result)?);
            }
            ArtifactsCmd::Read { thread, path } => {
                let result = client.call::<ArtifactRead>(ArtifactReadParams { thread_id: thread.parse()?, path }).await?;
                if json {
                    print_json(&result)?;
                } else {
                    print!("{}", result.content);
                }
            }
            ArtifactsCmd::Preview { thread, path } => {
                let result = client.call::<ArtifactPreview>(ArtifactReadParams { thread_id: thread.parse()?, path }).await?;
                println!("{}", serde_json::to_string_pretty(&result)?);
            }
        },
        Cmd::Plugins { cmd } => plugin_sync::run(&client, cmd, json).await?,
        Cmd::Skills { cmd: Some(cmd), .. } => skill_sync::run(&client, cmd, json).await?,
        Cmd::Skills { cmd: None, project, provider } => {
            let project = project.ok_or_else(|| anyhow!("Name a project: kybern skills <project>, or kybern skills machines"))?;
            let project_id = resolve_project(&client, &project, false).await?;
            let r = client.call::<SkillsList>(SkillsListParams { project_id, provider, instance: None }).await?;
            if json {
                println!("{}", serde_json::to_string_pretty(&r)?);
            } else {
                for skill in r.skills {
                    println!(
                        "${:<28} {:<8} {}",
                        skill.name,
                        format!("{:?}", skill.scope).to_lowercase(),
                        skill.description.unwrap_or_default()
                    );
                }
            }
        }
        Cmd::Ls { project, path } => {
            let r = client.call::<FilesList>(FilesListParams { project_id: project.parse()?, path }).await?;
            for e in &r.entries {
                match e.kind {
                    FileEntryKind::Directory => println!("{}/", e.name),
                    FileEntryKind::File => println!("{}  {}", e.name, e.size.unwrap_or(0)),
                }
            }
        }
        Cmd::ThreadCat { thread, path } => {
            let r = client
                .call::<ThreadFileRead>(ThreadFileReadParams { thread_id: thread.parse()?, path, max_bytes: 512 * 1024, if_version: None })
                .await?;
            if r.binary {
                eprintln!("binary file, {} bytes", r.size);
            } else {
                print!("{}", r.content);
                if r.truncated {
                    eprintln!("\n[file truncated]");
                }
            }
        }
        Cmd::ThreadResolve { thread, paths } => {
            let r =
                client.call::<ThreadFilesResolve>(ThreadFilesResolveParams { thread_id: thread.parse()?, paths: paths.clone() }).await?;
            for (path, resolved) in paths.iter().zip(r.paths) {
                println!("{path}\t{}", resolved.as_deref().unwrap_or("(no file)"));
            }
        }
        Cmd::Write { project, path, from, expected_version } => {
            let content = read_input(&from)?;
            let r = client.call::<FilesWrite>(FilesWriteParams { project_id: project.parse()?, path, content, expected_version }).await?;
            println!("{}  {} bytes", r.version, r.size);
        }
        Cmd::ThreadWrite { thread, path, from, expected_version } => {
            let content = read_input(&from)?;
            let r = client
                .call::<ThreadFileWrite>(ThreadFileWriteParams { thread_id: thread.parse()?, path, content, expected_version })
                .await?;
            println!("{}  {} bytes", r.version, r.size);
        }
        Cmd::Cat { project, path } => {
            let r = client
                .call::<FilesRead>(FilesReadParams { project_id: project.parse()?, path, max_bytes: 512 * 1024, if_version: None })
                .await?;
            if r.binary {
                eprintln!("binary file, {} bytes", r.size);
            } else {
                print!("{}", r.content);
                if r.truncated {
                    eprintln!("\n[truncated at 512 KiB of {} bytes]", r.size);
                }
            }
        }
        Cmd::Git { thread } => {
            let r = client.call::<GitStatusMethod>(GitStatusParams { thread_id: thread.parse()? }).await?;
            println!("{}", serde_json::to_string_pretty(&r)?);
        }
        Cmd::Branches { project } => {
            let project_id = resolve_project(&client, &project, false).await?;
            let r = client.call::<GitBranches>(GitBranchesParams { project_id, repo: None, fetch: false, include_remote: false }).await?;
            for b in r.branches {
                let upstream = b.upstream.map(|u| format!("  -> {u}")).unwrap_or_default();
                println!("{} {}{upstream}", if b.is_current { "*" } else { " " }, b.name);
            }
        }
        Cmd::Commit { thread, message } => {
            let r = client.call::<GitCommit>(GitCommitParams { thread_id: thread.parse()?, message }).await?;
            println!("{}  {}", &r.commit[..10], r.message.lines().next().unwrap_or(""));
        }
        Cmd::Actions { cmd } => match cmd {
            ActionsCmd::Runs { project, workflow, branch, status, page, repo } => {
                let project_id = resolve_project(&client, &project, false).await?;
                let r = client
                    .call::<ActionsRuns>(ActionsRunsParams {
                        project_id,
                        repo,
                        workflow,
                        branch,
                        status,
                        event: None,
                        page,
                        per_page: 30,
                        pages: 1,
                        pulls: false,
                    })
                    .await?;
                for run in r.runs {
                    let state = if run.conclusion.is_empty() { run.status } else { run.conclusion };
                    println!("{}  {state:<11}  {}  #{}  {}  ({})", run.id, run.workflow_name, run.number, run.title, run.head_branch);
                }
                if r.has_more {
                    eprintln!("more: --page {}", page + 1);
                }
            }
            ActionsCmd::View { project, run, attempt, repo } => {
                let project_id = resolve_project(&client, &project, false).await?;
                let r = client.call::<ActionsRun>(ActionsRunParams { project_id, repo, run_id: run, attempt }).await?;
                println!("{}", serde_json::to_string_pretty(&r)?);
            }
            ActionsCmd::Log { project, job, offset, limit, excerpt, repo } => {
                let project_id = resolve_project(&client, &project, false).await?;
                let r = client.call::<ActionsLog>(ActionsLogParams { project_id, repo, job_id: job, offset, limit, excerpt }).await?;
                if !r.available {
                    eprintln!("The job is still running; its log appears when it finishes.");
                }
                for line in &r.lines {
                    println!("{line}");
                }
                let end = r.offset + r.lines.len() as u32;
                if end < r.total_lines {
                    eprintln!("lines {}-{end} of {}: --offset {end} for more", r.offset, r.total_lines);
                }
            }
            ActionsCmd::Workflows { project, repo } => {
                let project_id = resolve_project(&client, &project, false).await?;
                let r = client.call::<ActionsWorkflows>(ActionsWorkflowsParams { project_id, repo }).await?;
                for w in r.workflows {
                    println!("{}  {}  {}  {}", w.id, w.name, w.path, w.state);
                }
            }
            ActionsCmd::Rerun { project, run, failed, job, repo } => {
                let project_id = resolve_project(&client, &project, false).await?;
                let action = match (job, failed) {
                    (Some(_), _) => ActionsActionKind::RerunJob,
                    (None, true) => ActionsActionKind::RerunFailed,
                    (None, false) => ActionsActionKind::Rerun,
                };
                client.call::<ActionsAction>(ActionsActionParams { project_id, repo, action, run_id: run, job_id: job }).await?;
            }
            ActionsCmd::Cancel { project, run, repo } => {
                let project_id = resolve_project(&client, &project, false).await?;
                client
                    .call::<ActionsAction>(ActionsActionParams {
                        project_id,
                        repo,
                        action: ActionsActionKind::Cancel,
                        run_id: run,
                        job_id: None,
                    })
                    .await?;
            }
            ActionsCmd::Dispatch { project, workflow, git_ref, fields, repo } => {
                let project_id = resolve_project(&client, &project, false).await?;
                let mut inputs = std::collections::BTreeMap::new();
                for field in fields {
                    let (name, value) = field.split_once('=').ok_or_else(|| anyhow::anyhow!("Write each input as name=value: {field}"))?;
                    inputs.insert(name.to_owned(), value.to_owned());
                }
                client.call::<ActionsDispatch>(ActionsDispatchParams { project_id, repo, workflow, git_ref, inputs }).await?;
            }
        },
        Cmd::Github { cmd } => match cmd {
            GithubCmd::Starred => {
                let result = client.call::<GithubStarred>(GithubStarredParams {}).await?;
                println!("{}", serde_json::to_string_pretty(&result)?);
            }
            GithubCmd::Star => {
                client.call::<GithubStar>(GithubStarParams {}).await?;
                println!("Starred Tresnanda/kybern");
            }
        },
        Cmd::Pr { cmd } => match cmd {
            PrCmd::View { project, number, repo } => {
                let project_id = resolve_project(&client, &project, false).await?;
                let result = client.call::<PrDetail>(PrDetailParams { project_id, number, repo }).await?;
                println!("{}", serde_json::to_string_pretty(&result)?);
            }
            PrCmd::Page { project, number, kind, page, repo, cursor } => {
                let project_id = resolve_project(&client, &project, false).await?;
                let kind = serde_json::from_value(serde_json::Value::String(kind))?;
                let result = client.call::<PrPage>(PrPageParams { project_id, number, kind, page, repo, cursor }).await?;
                println!("{}", serde_json::to_string_pretty(&result)?);
            }
            PrCmd::Merge { project, number, method, admin, auto, cancel_auto, repo } => {
                let action = match (cancel_auto, auto) {
                    (true, _) => PrActionKind::DisableAutoMerge,
                    (_, true) => PrActionKind::EnableAutoMerge,
                    _ => PrActionKind::Merge,
                };
                pr_simple_action(&client, &project, number, repo, action, method, admin).await?;
            }
            PrCmd::UpdateBranch { project, number, repo } => {
                pr_simple_action(&client, &project, number, repo, PrActionKind::UpdateBranch, None, false).await?;
            }
            PrCmd::Ready { project, number, repo } => {
                pr_simple_action(&client, &project, number, repo, PrActionKind::MarkReady, None, false).await?;
            }
            PrCmd::Action { file } => {
                let source = if file == "-" {
                    use std::io::Read;
                    let mut source = String::new();
                    std::io::stdin().read_to_string(&mut source)?;
                    source
                } else {
                    std::fs::read_to_string(file)?
                };
                client.call::<PrAction>(serde_json::from_str(&source)?).await?;
            }
            PrCmd::Create { thread, title, body, base, draft } => {
                let r = client
                    .call::<PrCreate>(PrCreateParams { thread_id: thread.parse()?, title, body, base, draft, commit_first: true })
                    .await?;
                println!("#{}  {}\n{}", r.number, r.title, r.url);
            }
            PrCmd::List { project, state } => {
                let project_id = resolve_project(&client, &project, false).await?;
                let r = client.call::<PrList>(PrListParams { project_id, state, limit: 30 }).await?;
                for pr in r.pull_requests {
                    let repo = pr.repo.as_deref().map(|repo| format!("{repo}  ")).unwrap_or_default();
                    println!("#{:<5} {:<8} {repo}{}  {}", pr.number, pr.state.to_lowercase(), pr.title, pr.url);
                }
                for repo in r.repos.iter().filter(|repo| repo.error.is_some()) {
                    eprintln!("{}: {}", repo.name, repo.error.as_deref().unwrap_or_default());
                }
            }
        },
        Cmd::Terminal { cmd } => render::terminal(&client, cmd, json).await?,
        Cmd::Call { method, params } => {
            let params = match params {
                Some(p) => serde_json::from_str(&p)?,
                None => serde_json::Value::Null,
            };
            let v = client.call_raw(&method, params).await?;
            print_json(&v)?;
        }
    }
    Ok(())
}

pub(crate) use TerminalCmd as TerminalCommand;

async fn host_path(client: &Client, path: &str) -> Result<String> {
    let explicit = std::env::var_os("KYBERN_URL").is_some() || std::env::args().any(|arg| arg == "--url" || arg.starts_with("--url="));
    let path = if !explicit && !PathBuf::from(path).is_absolute() && !path.starts_with('~') {
        std::env::current_dir()?.join(path).to_string_lossy().into_owned()
    } else {
        path.into()
    };
    Ok(client.call::<ProjectsBrowse>(ProjectsBrowseParams { path: Some(path) }).await?.path)
}

fn join_prompt(parts: Vec<String>) -> Result<String> {
    let s = parts.join(" ");
    if s.trim().is_empty() {
        return Err(anyhow!("prompt is empty"));
    }
    Ok(s)
}

/// Serialize large CLI results directly to stdout instead of retaining a
/// second, fully formatted JSON string beside the decoded response.
fn print_json(value: &impl serde::Serialize) -> Result<()> {
    use std::io::Write;

    let stdout = std::io::stdout();
    let mut output = std::io::BufWriter::new(stdout.lock());
    serde_json::to_writer_pretty(&mut output, value)?;
    output.write_all(b"\n")?;
    output.flush()?;
    Ok(())
}

/// The text of a file, or of stdin for `-`.
fn read_input(path: &str) -> Result<String> {
    if path == "-" {
        let mut text = String::new();
        std::io::Read::read_to_string(&mut std::io::stdin(), &mut text)?;
        return Ok(text);
    }
    Ok(std::fs::read_to_string(path)?)
}

/// A full note id, or any unambiguous prefix of one.
async fn resolve_note(client: &Client, key: &str) -> Result<NoteId> {
    if let Ok(id) = key.parse::<NoteId>() {
        return Ok(id);
    }
    let notes = client.call::<NotesList>(NotesListParams::default()).await?.notes;
    let mut matches = notes.iter().filter(|note| !key.is_empty() && note.id.to_string().starts_with(key));
    match (matches.next(), matches.next()) {
        (Some(note), None) => Ok(note.id),
        (Some(_), Some(_)) => Err(anyhow!("note id {key} matches several notes; use more of the id")),
        _ => Err(anyhow!("note {key} not found; run `kybern note list` for ids")),
    }
}

fn day(at: chrono::DateTime<chrono::Utc>) -> String {
    at.format("%Y-%m-%d").to_string()
}

/// Next pushed notification of `method`, decoded; other notifications are dropped.
async fn next_notification<T: serde::de::DeserializeOwned>(
    client: &Client,
    methods: &[&str],
    wait: std::time::Duration,
) -> Option<(String, T)> {
    let mut notifications = client.notifications.lock().await;
    loop {
        let note = tokio::time::timeout(wait, notifications.recv()).await.ok()?.ok()?;
        if methods.contains(&note.method.as_str())
            && let Ok(value) = serde_json::from_value(note.params)
        {
            return Some((note.method, value));
        }
    }
}

async fn sessions_command(client: &Client, cmd: SessionsCmd, json: bool) -> Result<()> {
    use std::time::Duration;
    match cmd {
        SessionsCmd::Scan { provider, suggested } => {
            let started = client
                .call::<SessionsScanProjects>(SessionsScanProjectsParams { providers: (!provider.is_empty()).then_some(provider) })
                .await?;
            let mut printed: std::collections::HashMap<(ProviderKind, String), ScanProviderState> = Default::default();
            let mut scan = started;
            loop {
                for status in &scan.providers {
                    let key = (status.provider, status.instance.clone());
                    if printed.insert(key, status.state) != Some(status.state) && !json && status.state != ScanProviderState::Pending {
                        let label = if status.instance == "default" {
                            status.provider.display_name().to_string()
                        } else {
                            format!("{} ({})", status.provider.display_name(), status.instance)
                        };
                        match &status.error {
                            Some(error) => eprintln!("{label}: {:?} - {error}", status.state),
                            None => eprintln!("{label}: {:?}, {} sessions", status.state, status.sessions),
                        }
                    }
                }
                if scan.state != SessionsJobState::Running {
                    break;
                }
                // A pushed update, or a plain read every few seconds in case one was missed.
                match next_notification::<SessionsScanChanged>(client, &[SESSIONS_SCAN_CHANGED_NOTIFICATION], Duration::from_secs(3)).await
                {
                    Some((_, changed)) if changed.scan.scan_id == scan.scan_id => scan = changed.scan,
                    Some(_) => {}
                    None => scan = client.call::<SessionsScanGet>(SessionsScanRef { scan_id: scan.scan_id.clone() }).await?,
                }
            }
            if json {
                println!("{}", serde_json::to_string_pretty(&scan)?);
                return Ok(());
            }
            println!("scan {}: {} sessions in {} repositories", scan.scan_id, scan.sessions_found, scan.projects.len());
            for project in scan.projects.iter().filter(|project| !suggested || project.suggested) {
                let providers = project
                    .providers
                    .iter()
                    .map(|count| format!("{} {}", count.provider.as_str(), count.sessions))
                    .collect::<Vec<_>>()
                    .join(", ");
                println!(
                    "{} {:>4} sessions ({} imported)  {}  {}  [{}]{}",
                    if project.suggested { "*" } else { " " },
                    project.session_count,
                    project.imported_count,
                    day(project.last_activity),
                    project.root,
                    providers,
                    if project.folder_missing { "  folder missing" } else { "" },
                );
            }
        }
        SessionsCmd::Import { roots, provider, limit, all, project, scan_id, detach } => {
            let project_id = match project {
                Some(_) if roots.len() != 1 => return Err(anyhow!("--project works with exactly one folder.")),
                Some(project) => Some(resolve_project(client, &project, false).await?),
                None => None,
            };
            let providers = (!provider.is_empty()).then_some(provider);
            let roots = roots
                .into_iter()
                .map(|root| SessionsImportRoot {
                    root: std::fs::canonicalize(&root).map(|path| path.to_string_lossy().into_owned()).unwrap_or(root),
                    project_id,
                    providers: providers.clone(),
                    limit: (!all).then_some(limit),
                })
                .collect();
            let mut job = client.call::<SessionsImportAll>(SessionsImportAllParams { roots, scan_id }).await?;
            if detach {
                println!("{}", job.job_id);
                return Ok(());
            }
            if !json {
                eprintln!("import job {} (stop it with `kybern sessions cancel {}`)", job.job_id, job.job_id);
            }
            let id = job.job_id.clone();
            while job.state == SessionsJobState::Running {
                match next_notification::<serde_json::Value>(
                    client,
                    &[SESSIONS_IMPORT_PROGRESS_NOTIFICATION, SESSIONS_IMPORT_FINISHED_NOTIFICATION],
                    Duration::from_secs(3),
                )
                .await
                {
                    Some((method, params)) if method == SESSIONS_IMPORT_PROGRESS_NOTIFICATION => {
                        if let Ok(progress) = serde_json::from_value::<SessionsImportProgress>(params)
                            && progress.job_id == id
                            && !json
                        {
                            eprintln!(
                                "{}: {}/{}  imported {}, already {}, skipped {}",
                                progress.root,
                                progress.done,
                                progress.total,
                                progress.imported,
                                progress.already_imported,
                                progress.skipped_count
                            );
                        }
                    }
                    Some(_) => job = client.call::<SessionsImportGet>(SessionsImportRef { job_id: id.clone() }).await?,
                    None => job = client.call::<SessionsImportGet>(SessionsImportRef { job_id: id.clone() }).await?,
                }
            }
            if json {
                println!("{}", serde_json::to_string_pretty(&job)?);
            } else {
                print_import(&job);
            }
        }
        SessionsCmd::Job { job_id } => {
            let job = client.call::<SessionsImportGet>(SessionsImportRef { job_id }).await?;
            if json {
                println!("{}", serde_json::to_string_pretty(&job)?);
            } else {
                print_import(&job);
            }
        }
        SessionsCmd::Cancel { job_id } => {
            let job = client.call::<SessionsImportCancel>(SessionsImportRef { job_id }).await?;
            if json {
                println!("{}", serde_json::to_string_pretty(&job)?);
            } else {
                println!("stopping; {} imported so far", job.imported);
            }
        }
    }
    Ok(())
}

fn print_import(job: &SessionsImportJob) {
    println!("{:?}: imported {}, already imported {}, skipped {}", job.state, job.imported, job.already_imported, job.skipped_count);
    for project in &job.projects {
        println!("  {}  {}/{} done, {} imported", project.root, project.done, project.total, project.imported);
        if let Some(error) = &project.error {
            println!("    {error}");
        }
        for skip in &project.skipped {
            println!("    skipped {} {} ({:?}): {}", skip.provider.as_str(), skip.session_id, skip.reason, skip.detail);
        }
    }
}

async fn note_command(client: &Client, cmd: NoteCmd, json: bool) -> Result<()> {
    match cmd {
        NoteCmd::List { deleted, machine } => {
            let mut result = client.call::<NotesList>(NotesListParams { machine }).await?;
            result.notes.retain(|note| note.deleted_at.is_some() == deleted);
            if json {
                println!("{}", serde_json::to_string_pretty(&result)?);
            } else if result.notes.is_empty() {
                println!(
                    "{}",
                    if deleted { "No deleted notes." } else { "No notes yet. Create one with `kybern note new --title Ideas`." }
                );
            } else if let Some(federation) = &result.federation {
                for section in &federation.sections {
                    let notes: Vec<_> = result.notes.iter().filter(|note| section.ids.contains(&note.id)).cloned().collect();
                    println!("{}{}", section.machine_name, if section.is_self { " (this machine)" } else { "" });
                    if notes.is_empty() {
                        println!("  No notes.");
                    } else {
                        render::notes(&notes);
                    }
                }
                if let Some(footer) = &federation.footer {
                    println!("{footer}");
                }
            } else {
                render::notes(&result.notes);
            }
        }
        NoteCmd::Tidy { project, not_now, start } => {
            let project_id = resolve_project(client, &project, false).await?;
            let params = NotesTidyParams { project_id };
            if start {
                let started = client.call::<NotesTidyStart>(params).await?;
                if json {
                    println!("{}", serde_json::to_string_pretty(&started)?);
                } else {
                    println!("Asked the Coordinator ({}) to check {} notes.", started.thread_id, started.notes);
                }
                return Ok(());
            }
            let offer = if not_now { client.call::<NotesTidySnooze>(params).await? } else { client.call::<NotesTidyOffer>(params).await? };
            if json {
                println!("{}", serde_json::to_string_pretty(&offer)?);
            } else if offer.stale.is_empty() {
                println!("No notes look outdated.");
            } else {
                let state = match (offer.offer, offer.snoozed_until) {
                    (true, _) => "offered".to_owned(),
                    (false, Some(until)) => format!("snoozed until {}", until.format("%Y-%m-%d")),
                    (false, None) => format!("below the {NOTES_TIDY_THRESHOLD}-note threshold"),
                };
                println!("{} notes may be outdated ({state}).", offer.stale.len());
                for id in &offer.stale {
                    println!("{id}");
                }
            }
        }
        NoteCmd::Show { id } => {
            let note = client.call::<NotesGet>(NotesGetParams { id: Some(resolve_note(client, &id).await?), thread_id: None }).await?.note;
            let note = note.ok_or_else(|| anyhow!("note {id} not found; run `kybern note list` for ids"))?;
            if json {
                println!("{}", serde_json::to_string_pretty(&note)?);
            } else {
                if !note.summary.title.is_empty() {
                    println!("# {}\n", note.summary.title);
                }
                println!("{}", note.body);
            }
        }
        NoteCmd::New { project, title, file, category } => {
            let project_id = match &project {
                Some(project) => Some(resolve_project(client, project, false).await?),
                None => None,
            };
            let note = client
                .call::<NotesCreate>(NotesCreateParams {
                    scope: if project_id.is_some() { NoteScope::Project } else { NoteScope::Global },
                    project_id,
                    title,
                    body: file.as_deref().map(read_input).transpose()?,
                    project_role: None,
                    category,
                })
                .await?;
            if json {
                println!("{}", serde_json::to_string_pretty(&note)?);
            } else {
                println!("{}", note.summary.id);
            }
        }
        NoteCmd::Edit { id, file, title, category } => {
            if file.is_none() && title.is_none() && category.is_none() {
                return Err(anyhow!("nothing to change; pass --file <path|-> for the body, --title for the title or --category"));
            }
            let id = resolve_note(client, &id).await?;
            let current = client
                .call::<NotesGet>(NotesGetParams { id: Some(id), thread_id: None })
                .await?
                .note
                .ok_or_else(|| anyhow!("note {id} not found; run `kybern note list` for ids"))?;
            let note = client
                .call::<NotesUpdate>(NotesUpdateParams {
                    id: Some(id),
                    thread_id: None,
                    expected_revision: current.summary.revision,
                    title,
                    body: file.as_deref().map(read_input).transpose()?,
                    category,
                    clear_category: false,
                })
                .await?;
            if json {
                println!("{}", serde_json::to_string_pretty(&note)?);
            } else {
                println!("Saved revision {}.", note.summary.revision);
            }
        }
        NoteCmd::Delete { id } => {
            let note = client.call::<NotesDelete>(NotesIdParams { id: resolve_note(client, &id).await? }).await?;
            if json {
                println!("{}", serde_json::to_string_pretty(&note)?);
            } else {
                println!("Deleted. Restore it within 30 days with `kybern note restore {}`.", note.id);
            }
        }
        NoteCmd::Restore { id } => {
            let note = client.call::<NotesRestore>(NotesIdParams { id: resolve_note(client, &id).await? }).await?;
            if json {
                println!("{}", serde_json::to_string_pretty(&note)?);
            } else {
                println!("Restored{}.", if note.scope == NoteScope::Global && note.origin.is_some() { " as a global note" } else { "" });
            }
        }
        NoteCmd::Search { query, machine } => {
            let query = query.join(" ");
            if query.trim().is_empty() {
                return Err(anyhow!("type something to search for, like `kybern note search release plan`"));
            }
            let result = client.call::<NotesSearch>(NotesSearchParams { query, limit: None, machine }).await?;
            if json {
                println!("{}", serde_json::to_string_pretty(&result)?);
            } else if result.results.is_empty() {
                println!("No matching notes.");
            }
            for hit in result.results.iter().filter(|_| !json) {
                let machine = result
                    .federation
                    .as_ref()
                    .and_then(|federation| federation.sections.iter().find(|section| section.ids.contains(&hit.id)))
                    .map_or(String::new(), |section| format!("  [{}]", section.machine_name));
                println!("{}{machine}  {}", hit.id, hit.snippet);
            }
            if let Some(footer) = result.federation.as_ref().and_then(|federation| federation.footer.as_ref()).filter(|_| !json) {
                println!("{footer}");
            }
        }
    }
    Ok(())
}

fn parse_note_category(value: &str) -> Result<NoteCategory, String> {
    NoteCategory::parse(&value.to_ascii_lowercase())
        .ok_or_else(|| format!("unknown category {value}; use preference, decision, convention or gotcha"))
}

fn parse_task_status(value: &str) -> Result<TaskStatus, String> {
    match value.to_ascii_lowercase().replace('_', "-").as_str() {
        "inbox" => Ok(TaskStatus::Inbox),
        "todo" | "to-do" => Ok(TaskStatus::Todo),
        "running" => Ok(TaskStatus::Running),
        "needs-review" | "review" => Ok(TaskStatus::NeedsReview),
        "done" => Ok(TaskStatus::Done),
        "canceled" | "cancelled" => Ok(TaskStatus::Canceled),
        other => Err(format!("unknown status {other}; use inbox, todo, running, needs-review, done or canceled")),
    }
}

/// A task by key (`ADE-14`) or id.
async fn resolve_task(client: &Client, key: &str) -> Result<TaskItem> {
    let params = match key.parse::<TaskItemId>() {
        Ok(id) => TaskItemsGetParams { id: Some(id), key: None },
        Err(_) => TaskItemsGetParams { id: None, key: Some(key.to_owned()) },
    };
    client.call::<TaskItemsGet>(params).await?.task.ok_or_else(|| anyhow!("task {key} not found; run `kybern task list --all` for keys"))
}

async fn task_command(client: &Client, cmd: TaskCmd, json: bool) -> Result<()> {
    match cmd {
        TaskCmd::List { status, project, all, machine } => {
            let project_id = match &project {
                Some(project) => Some(resolve_project(client, project, false).await?),
                None => None,
            };
            let TaskItemsListResult { mut tasks, federation } = client.call::<TaskItemsList>(TaskItemsListParams { machine }).await?;
            tasks.retain(|task| {
                project_id.is_none_or(|id| task.project_id == Some(id))
                    && match status {
                        Some(status) => task.status == status,
                        None => all || !matches!(task.status, TaskStatus::Done | TaskStatus::Canceled),
                    }
            });
            if json {
                println!("{}", serde_json::to_string_pretty(&TaskItemsListResult { tasks, federation })?);
            } else if tasks.is_empty() {
                println!("No tasks. Create one with `kybern task new \"Fix the login redirect\"`.");
            } else if let Some(federation) = &federation {
                for section in &federation.sections {
                    let tasks: Vec<_> = tasks.iter().filter(|task| section.ids.contains(&task.id)).cloned().collect();
                    println!("{}{}", section.machine_name, if section.is_self { " (this machine)" } else { "" });
                    if tasks.is_empty() {
                        println!("  No tasks.");
                    } else {
                        render::tasks(&tasks);
                    }
                }
                if let Some(footer) = &federation.footer {
                    println!("{footer}");
                }
            } else {
                render::tasks(&tasks);
            }
        }
        TaskCmd::Show { task } => {
            let task = resolve_task(client, &task).await?;
            if json {
                println!("{}", serde_json::to_string_pretty(&task)?);
            } else {
                render::task(&task);
            }
        }
        TaskCmd::New { title, project, file, status, priority } => {
            let project_id = match &project {
                Some(project) => Some(resolve_project(client, project, false).await?),
                None => None,
            };
            let task = client
                .call::<TaskItemsCreate>(TaskItemsCreateParams {
                    scope: if project_id.is_some() { TaskScope::Project } else { TaskScope::Global },
                    project_id,
                    title,
                    body: file.as_deref().map(read_input).transpose()?,
                    status,
                    priority,
                    note_ids: None,
                    source: None,
                })
                .await?;
            if json {
                println!("{}", serde_json::to_string_pretty(&task)?);
            } else {
                println!("{}  {}", task.key, task.title);
            }
        }
        TaskCmd::Edit { task, title, file, status, priority } => {
            if title.is_none() && file.is_none() && status.is_none() && priority.is_none() {
                return Err(anyhow!("nothing to change; pass --title, --file, --status or --priority"));
            }
            let current = resolve_task(client, &task).await?;
            let task = client
                .call::<TaskItemsUpdate>(TaskItemsUpdateParams {
                    id: current.id,
                    expected_revision: Some(current.revision),
                    title,
                    body: file.as_deref().map(read_input).transpose()?,
                    status,
                    priority,
                    scope: None,
                    project_id: None,
                    note_ids: None,
                    pending_followup: None,
                    before_id: None,
                })
                .await?;
            if json {
                println!("{}", serde_json::to_string_pretty(&task)?);
            } else {
                println!("{}  {}  {}", task.key, render::task_status(task.status), task.title);
            }
        }
        TaskCmd::Done { task } => {
            let current = resolve_task(client, &task).await?;
            let task = client
                .call::<TaskItemsUpdate>(TaskItemsUpdateParams {
                    id: current.id,
                    expected_revision: None,
                    title: None,
                    body: None,
                    status: Some(TaskStatus::Done),
                    priority: None,
                    scope: None,
                    project_id: None,
                    note_ids: None,
                    pending_followup: None,
                    before_id: None,
                })
                .await?;
            if json {
                println!("{}", serde_json::to_string_pretty(&task)?);
            } else {
                println!("{}  done", task.key);
            }
        }
        TaskCmd::Followup { task, text } => {
            let current = resolve_task(client, &task).await?;
            let sent = client.call::<TaskItemsFollowup>(TaskItemsFollowupParams::text(current.id, join_prompt(text)?)).await?;
            if json {
                println!("{}", serde_json::to_string_pretty(&sent)?);
            } else {
                match sent.sent_to {
                    Some(thread) => println!("{} follow-up sent to thread {thread}.", sent.task.key),
                    None => println!("{} follow-up saved for the next run.", sent.task.key),
                }
            }
        }
        TaskCmd::Send { task, provider, model, effort, mode, worktree, branch, project, prompt } => {
            let task = resolve_task(client, &task).await?;
            let project_id = match &project {
                Some(project) => Some(resolve_project(client, project, false).await?),
                None => None,
            };
            let provider = match provider {
                Some(provider) => provider.parse::<ProviderKind>().map_err(|e: String| anyhow!(e))?,
                None => client.call::<SettingsGet>(Empty {}).await?.default_provider,
            };
            let prompt = if prompt.is_empty() {
                let body = task.body.trim();
                if body.is_empty() { task.title.clone() } else { format!("{}\n\n{body}", task.title) }
            } else {
                join_prompt(prompt)?
            };
            let sent = client
                .call::<TaskItemsSend>(TaskItemsSendParams {
                    id: task.id,
                    provider: ProviderInstance::default_for(provider),
                    model,
                    effort,
                    permission_mode: mode,
                    use_worktree: if worktree { Some(true) } else { None },
                    base_branch: branch,
                    project_id,
                    prompt: Some(prompt),
                    message: None,
                    note_ids: Some(task.note_ids.clone()),
                    thread_id: None,
                    message_id: None,
                })
                .await?;
            if json {
                println!("{}", serde_json::to_string_pretty(&sent)?);
            } else {
                println!("{} started on thread {}. Follow it with `kybern task show {}`.", sent.task.key, sent.thread_id, sent.task.key);
            }
        }
        TaskCmd::SendBatch { tasks, combined, prompt, provider, model, effort, mode, worktree, branch, project } => {
            let mut ids = Vec::new();
            let mut keys = std::collections::HashMap::new();
            for task in &tasks {
                let task = resolve_task(client, task).await?;
                keys.insert(task.id, task.key.clone());
                ids.push(task.id);
            }
            let project_id = match &project {
                Some(project) => Some(resolve_project(client, project, false).await?),
                None => None,
            };
            let provider = match provider {
                Some(provider) => provider.parse::<ProviderKind>().map_err(|e: String| anyhow!(e))?,
                None => client.call::<SettingsGet>(Empty {}).await?.default_provider,
            };
            let sent = client
                .call::<TaskItemsSendBatch>(TaskItemsSendBatchParams {
                    ids,
                    mode: if combined { TaskBatchMode::Combined } else { TaskBatchMode::Separate },
                    provider: ProviderInstance::default_for(provider),
                    model,
                    effort,
                    permission_mode: mode,
                    use_worktree: if worktree { Some(true) } else { None },
                    base_branch: branch,
                    project_id,
                    prompt: Some(prompt),
                    message: None,
                    note_ids: None,
                })
                .await?;
            if json {
                println!("{}", serde_json::to_string_pretty(&sent)?);
            } else {
                for started in &sent.started {
                    println!("{} started on thread {}.", started.task.key, started.thread_id);
                }
                for skipped in &sent.skipped {
                    let name = keys.get(&skipped.id).cloned().unwrap_or_else(|| skipped.id.to_string());
                    println!("{name} skipped: {}.", skipped.reason.trim_end_matches('.'));
                }
            }
        }
    }
    Ok(())
}

async fn resolve_project(client: &Client, key: &str, add_if_missing: bool) -> Result<ProjectId> {
    if let Ok(id) = key.parse::<ProjectId>() {
        return Ok(id);
    }
    let list = client.call::<ProjectsList>(ProjectsListParams::default()).await?;
    if let Some(p) = list.projects.iter().find(|p| p.path == key || p.name == key) {
        return Ok(p.id);
    }
    let path = host_path(client, key).await?;
    if let Some(project) = list.projects.iter().find(|project| project.path == path) {
        return Ok(project.id);
    }
    if add_if_missing {
        let p = client.call::<ProjectsAdd>(ProjectsAddParams { path, name: None, space_id: None, kind: None, members: None }).await?;
        eprintln!("added project {} ({})", p.name, p.id);
        return Ok(p.id);
    }
    Err(anyhow!("project {key} not found"))
}
