//! `kybern machines`: the machine network from the command line (design §10.3).
//!
//! `list`/`status` show the members and what is found on the tailnet; `add` joins a found machine,
//! `code` shows the code a machine that is not on the tailnet gives to a member, `join` redeems it;
//! `remove`, `pause`, `resume`, `rename` and `merge` change the network; `routes` and `doctor` say
//! how machines are reached and what is wrong; `reachable` keeps this machine reachable when Kybern
//! is closed.

use std::io::{IsTerminal, Write};

use anyhow::{Result, anyhow};
use clap::Subcommand;
use kybern_client::Client;
use kybern_protocol::methods::*;
use kybern_protocol::*;

#[derive(Subcommand)]
pub enum MachinesCmd {
    /// List the machines in your network, and the machines found on your tailnet.
    List,
    /// The same, plus this machine's Tailscale state.
    Status,
    /// Add a machine found on your tailnet. Run `kybern machines list` to see them.
    Add {
        /// A found machine's name or id.
        found: String,
    },
    /// Show the six-digit code another machine needs to add this one.
    Code,
    /// Add the machine that shows `code` at `address`.
    Join {
        /// The six digits shown by `kybern machines code` on the other machine.
        code: String,
        /// Its address: a Tailscale name or address, or an https URL.
        address: String,
        /// The machine is an environment this client already uses.
        #[arg(long)]
        migrate: bool,
    },
    /// Remove a machine from the network, on every machine.
    Remove {
        /// A machine's name or id.
        machine: String,
        /// The machine might be lost or stolen.
        #[arg(long)]
        stolen: bool,
        /// Also remove this machine (repeatable): one that the removed machine added.
        #[arg(long = "also")]
        also: Vec<String>,
        /// Remove without asking.
        #[arg(long)]
        yes: bool,
    },
    /// Stop opening sessions to a machine. Operations keep syncing.
    Pause { machine: String },
    /// Resume a paused machine.
    Resume { machine: String },
    /// Rename a machine.
    Rename { machine: String, name: String },
    /// Merge this machine's network with the network of a found machine of yours.
    Merge { found: String },
    /// Show the routes a client can reach a machine on.
    Routes { machine: String },
    /// Check Tailscale, membership and each machine, and say what to do about what is wrong.
    Doctor {
        /// Only this machine's checks.
        machine: Option<String>,
    },
    /// Turn on Connect to your other machines.
    Enable {
        /// Enable only if this machine has never chosen on or off (trusted setup).
        #[arg(long)]
        if_unconfigured: bool,
    },
    /// Turn off Connect to your other machines. Membership is kept.
    Disable,
    /// This machine was removed: make a new identity so another machine can add it again.
    Rejoin,
    /// Update every machine in the network to the newest version.
    Update,
    /// Keep all machines on the same version: a machine updates itself when another runs a newer one.
    FollowUpdates {
        /// `on` or `off`.
        #[arg(value_parser = ["on", "off"])]
        state: String,
    },
    /// Keep this machine reachable when Kybern is closed: start the daemon at login while it is in a network.
    Reachable {
        /// `on` or `off`. Without it, show the setting and the login item.
        #[arg(value_parser = ["on", "off"])]
        state: Option<String>,
    },
    /// This machine's CPU, memory, graphics, harnesses and repositories.
    Info,
    /// The login broker: the machine that holds the real sign-ins the others use.
    Broker {
        #[command(subcommand)]
        cmd: BrokerCmd,
    },
    /// Use a stored key on other machines: it is sealed to each one and never travels in the clear.
    PushKey {
        /// The key's name in the secret store, such as provider.cursor.CURSOR_API_KEY.
        name: String,
        /// Only these machines (repeatable). Without it, every other machine.
        #[arg(long = "machine")]
        machines: Vec<String>,
    },
}

#[derive(Subcommand)]
pub enum BrokerCmd {
    /// Show who the broker is and where each agent's sign-in comes from on this machine.
    Status,
    /// Make a machine the login broker (`none` clears it). It must already be signed in.
    Set {
        /// A machine's name or id, or `none`.
        machine: String,
        /// Leave the old broker signed in after the others moved to the new one.
        #[arg(long)]
        keep_old_signed_in: bool,
    },
    /// Make an account on this machine sign in through the broker.
    Use {
        /// The agent: claude-code, codex or omp.
        agent: String,
        /// The account on this machine (a named one, not the CLI account).
        account: String,
        /// The broker's account to borrow; the broker's CLI account when left out.
        #[arg(long)]
        from: Option<String>,
    },
    /// Make an account sign in on this machine again, instead of through the broker.
    Release { agent: String, account: String },
}

fn parse_agent(value: &str) -> Result<ProviderKind> {
    serde_json::from_value(serde_json::Value::String(value.to_lowercase()))
        .map_err(|_| anyhow!("Agents the login broker lends are claude-code, codex and omp."))
}

fn render_broker(status: &LoginBrokerStatus) {
    match &status.broker {
        Some(broker) => println!(
            "Login broker: {}{}",
            broker.name.as_deref().unwrap_or(&broker.machine_id),
            if status.is_broker {
                " (this machine)"
            } else if broker.reachable {
                ""
            } else {
                " (offline)"
            }
        ),
        None => println!("Login broker: none"),
    }
    for provider in &status.providers {
        let state = match provider.state {
            BrokerLoginState::Lending => "lending",
            BrokerLoginState::Borrowing => "signs in through the broker",
            BrokerLoginState::BrokerOffline => "broker offline",
            BrokerLoginState::BrokerNotSignedIn => "not signed in on the broker",
            BrokerLoginState::BrokerUnsupported => "broker needs a newer Kybern",
            BrokerLoginState::NotUsed => "own sign-in",
        };
        println!("  {:<12} {state}", format!("{:?}", provider.provider).to_lowercase());
        if let Some(message) = &provider.message {
            println!("               {message}");
        }
        for account in &provider.lendable {
            println!("               can lend: {} ({})", account.name, account.account);
        }
    }
    for loan in &status.loans {
        println!("  lent {:?} to {}", loan.provider, short(&loan.machine_id));
    }
}

/// The part of an id that tells machines apart: ids are UUIDv7, whose first characters are a clock.
fn short(id: &str) -> &str {
    id.get(id.len().saturating_sub(8)..).unwrap_or(id)
}

fn status_word(card: &MachineCard) -> &'static str {
    match card.status {
        MachineStatus::Online => "online",
        MachineStatus::Relaying => "relaying",
        MachineStatus::Offline => "offline",
        MachineStatus::Paused => "paused",
    }
}

/// Find a member by id, id prefix or name (ignoring case). An unknown or ambiguous key is an error.
pub fn resolve_machine<'a>(members: &'a [MachineCard], key: &str) -> Result<&'a MachineCard> {
    let lowered = key.to_lowercase();
    let exact: Vec<&MachineCard> = members.iter().filter(|card| card.machine_id == key).collect();
    if let [only] = exact.as_slice() {
        return Ok(only);
    }
    let named: Vec<&MachineCard> = members.iter().filter(|card| card.name.to_lowercase() == lowered).collect();
    if let [only] = named.as_slice() {
        return Ok(only);
    }
    let prefixed: Vec<&MachineCard> =
        members.iter().filter(|card| key.len() >= 4 && (card.machine_id.starts_with(key) || card.machine_id.ends_with(key))).collect();
    match prefixed.as_slice() {
        [only] => Ok(only),
        [] if named.len() > 1 => Err(anyhow!("More than one machine is named \"{key}\". Use its id from kybern machines list.")),
        _ => Err(anyhow!("No machine named \"{key}\". Run kybern machines list to see them.")),
    }
}

/// Find a found machine by node id, machine id or name.
pub fn resolve_found<'a>(found: &'a [FoundMachine], key: &str) -> Result<&'a FoundMachine> {
    let lowered = key.to_lowercase();
    let by = |keep: &dyn Fn(&FoundMachine) -> bool| found.iter().filter(|machine| keep(machine)).collect::<Vec<_>>();
    for matches in [
        by(&|machine| machine.found_id == key || machine.machine_id.as_deref() == Some(key)),
        by(&|machine| machine.name.to_lowercase() == lowered),
    ] {
        if let [only] = matches.as_slice() {
            return Ok(only);
        }
        if matches.len() > 1 {
            return Err(anyhow!("More than one found machine matches \"{key}\". Use its id from kybern machines list."));
        }
    }
    Err(anyhow!("No machine named \"{key}\" was found on your tailnet. Run kybern machines list."))
}

/// What a login item is called on this platform, for the user.
fn login_item_name(kind: LoginServiceKind) -> &'static str {
    match kind {
        LoginServiceKind::TaskScheduler => "a Task Scheduler task",
        LoginServiceKind::LaunchAgent => "a LaunchAgent",
        LoginServiceKind::SystemdUser => "a systemd user service",
        LoginServiceKind::XdgAutostart => "an autostart entry",
    }
}

/// One line on "Keep this machine reachable when Kybern is closed".
pub fn reachable_line(status: &KeepReachableStatus) -> String {
    let Some(kind) = status.kind else {
        return "Reachable when Kybern is closed: not supported on this system.".into();
    };
    if let Some(external) = &status.external {
        return format!("Reachable when Kybern is closed: yes. {external}");
    }
    let state = match (status.enabled, status.wanted, status.registered) {
        (false, _, _) => {
            "off. Other machines lose this one when Kybern is closed. Turn it on with kybern machines reachable on.".to_string()
        }
        (true, false, _) => "on. It applies once this machine is in a network with Connect to your other machines on.".to_string(),
        (true, true, true) => format!(
            "on. Kybern starts at login through {}{}.",
            login_item_name(kind),
            if status.running { ", which started this daemon" } else { "" }
        ),
        (true, true, false) => format!("on, but {} is not registered yet.", login_item_name(kind)),
    };
    let mut line = format!("Reachable when Kybern is closed: {state}");
    for problem in status.error.iter().chain(&status.warnings) {
        line.push_str(&format!("\n  {problem}"));
    }
    line
}

async fn status(client: &Client) -> Result<NetworkStatus> {
    client.call::<NetworkStatusMethod>(Empty {}).await
}

fn confirmed(prompt: &str) -> Result<bool> {
    eprint!("{prompt} [y/N] ");
    std::io::stderr().flush()?;
    let mut answer = String::new();
    std::io::stdin().read_line(&mut answer)?;
    Ok(matches!(answer.trim().to_lowercase().as_str(), "y" | "yes"))
}

pub fn render_status(status: &NetworkStatus, with_tailscale: bool) {
    if !status.enabled {
        println!("Connect to your other machines is off on this machine. Turn it on with kybern machines enable.");
    }
    if status.removed {
        println!("This machine was removed from its network. Run kybern machines rejoin to get a new identity.");
    }
    if with_tailscale {
        let ts = &status.tailscale;
        let line = if !ts.installed {
            format!("Tailscale: not available{}", ts.detail.as_deref().map(|detail| format!(" ({detail})")).unwrap_or_default())
        } else if !ts.running {
            format!("Tailscale: {}", ts.detail.as_deref().unwrap_or("not running"))
        } else {
            format!(
                "Tailscale: {} on {}{}",
                ts.self_name.as_deref().unwrap_or("this machine"),
                ts.tailnet.as_deref().unwrap_or("your tailnet"),
                ts.listening_port.map(|port| format!(", listening on port {port}")).unwrap_or_default()
            )
        };
        println!("{line}");
        if let Some(reachable) = &status.keep_reachable {
            println!("{}", reachable_line(reachable));
        }
    }
    match &status.network_id {
        Some(id) => println!("Network {}  {} machine{}", short(id), status.members.len(), if status.members.len() == 1 { "" } else { "s" }),
        None => println!("This machine is not in a network yet."),
    }
    for card in &status.members {
        println!(
            "{} {:<20} {:<9} {:<8} {:<8} {}{}",
            if card.is_self { "*" } else { " " },
            card.name,
            status_word(card),
            card.os,
            card.version.as_deref().unwrap_or("-"),
            short(&card.machine_id),
            card.added_via.map(|via| format!("  (added by {via:?})").to_lowercase()).unwrap_or_default()
        );
    }
    for candidate in &status.setup_candidates {
        println!(
            "  {} ({}) — {}. Open Settings → Machines to connect with saved access or install Kybern.",
            candidate.name,
            candidate.address,
            if candidate.online { "Kybern has not advertised a network" } else { "offline on Tailscale" }
        );
    }
    if !status.found.is_empty() {
        println!("\nFound on your tailnet:");
        for machine in &status.found {
            let how = match machine.action {
                FoundAction::Add => "kybern machines add",
                FoundAction::Code => "needs a code: run kybern machines code there, then kybern machines join",
                FoundAction::Merge => "in another of your networks: kybern machines merge",
            };
            println!("  {:<20} {:<8} {}  {}", machine.name, machine.os.as_deref().unwrap_or("-"), how, machine.found_id);
        }
    }
}

pub fn render_doctor(report: &DoctorReport) {
    for check in &report.checks {
        let mark = match check.status {
            DoctorStatus::Pass => "ok  ",
            DoctorStatus::Warn => "warn",
            DoctorStatus::Fail => "FAIL",
        };
        println!("{mark}  {}", check.title);
        if let Some(next) = &check.next_step {
            println!("      {next}");
        }
    }
    let failed = report.checks.iter().filter(|check| check.status == DoctorStatus::Fail).count();
    let warned = report.checks.iter().filter(|check| check.status == DoctorStatus::Warn).count();
    println!("\n{} checks, {failed} failed, {warned} to look at", report.checks.len());
}

async fn set_paused(client: &Client, machine: &str, paused: bool, json: bool) -> Result<()> {
    let members = status(client).await?.members;
    let id = resolve_machine(&members, machine)?.machine_id.clone();
    let card = client.call::<NetworkPause>(NetworkPauseParams { machine_id: id, paused }).await?;
    if json {
        println!("{}", serde_json::to_string_pretty(&card)?)
    } else {
        println!("{} is {}.", card.name, if paused { "paused" } else { "resumed" })
    }
    Ok(())
}

pub async fn run(client: &Client, cmd: MachinesCmd, json: bool) -> Result<()> {
    match cmd {
        MachinesCmd::List | MachinesCmd::Status => {
            let with_tailscale = matches!(cmd, MachinesCmd::Status);
            let status = status(client).await?;
            if json { println!("{}", serde_json::to_string_pretty(&status)?) } else { render_status(&status, with_tailscale) }
        }
        MachinesCmd::Add { found } => {
            let status = status(client).await?;
            let machine = resolve_found(&status.found, &found)?;
            let result = client.call::<NetworkAdd>(NetworkFoundParams { found_id: machine.found_id.clone() }).await?;
            if json {
                println!("{}", serde_json::to_string_pretty(&result)?);
            } else if let Some(card) = result.machine {
                println!("Added {} to your network.", card.name);
            } else {
                println!(
                    "{} can't be added without its code. On that machine run kybern machines code, then here:\n  kybern machines join <code> <address>",
                    machine.name
                );
            }
        }
        MachinesCmd::Code => {
            let code = client.call::<NetworkCodeCreate>(Empty {}).await?;
            if json {
                println!("{}", serde_json::to_string_pretty(&code)?);
            } else {
                println!("Code: {}  (valid for 10 minutes)", code.code);
                println!("On a machine in your network, run:");
                let address = code.endpoints.first().map(String::as_str).unwrap_or("<this machine's address>");
                println!("  kybern machines join {} {address}", code.code);
                if code.endpoints.len() > 1 {
                    println!("Other addresses: {}", code.endpoints[1..].join(", "));
                }
                println!("Or open: {}", code.qr);
            }
        }
        MachinesCmd::Join { code, address, migrate } => {
            let card = client.call::<NetworkJoin>(NetworkJoinParams { code, address, migrate }).await?;
            if json { println!("{}", serde_json::to_string_pretty(&card)?) } else { println!("Added {} to your network.", card.name) }
        }
        MachinesCmd::Remove { machine, stolen, also, yes } => {
            let members = status(client).await?.members;
            let card = resolve_machine(&members, &machine)?;
            let also_ids: Vec<MachineId> =
                also.iter().map(|key| resolve_machine(&members, key).map(|card| card.machine_id.clone())).collect::<Result<_>>()?;
            if !yes {
                if !std::io::stdin().is_terminal() {
                    return Err(anyhow!("Pass --yes to remove a machine without a prompt."));
                }
                let mut question = format!("Remove {} from your network? It loses access on every machine.", card.name);
                if stolen {
                    question.push_str(" It is marked lost or stolen.");
                }
                if !confirmed(&question)? {
                    return Ok(());
                }
            }
            let name = card.name.clone();
            let after = client
                .call::<NetworkRemove>(NetworkRemoveParams { machine_id: card.machine_id.clone(), stolen, also_remove: also_ids })
                .await?;
            if json {
                println!("{}", serde_json::to_string_pretty(&after)?);
            } else {
                println!("Removed {name}.");
                if stolen {
                    println!(
                        "It could still reach the tailnet. Remove or lock its node in the Tailscale admin console: https://login.tailscale.com/admin/machines"
                    );
                    println!("Rotate any API keys it had, at their providers.");
                }
            }
        }
        MachinesCmd::Pause { machine } => set_paused(client, &machine, true, json).await?,
        MachinesCmd::Resume { machine } => set_paused(client, &machine, false, json).await?,
        MachinesCmd::Rename { machine, name } => {
            let members = status(client).await?.members;
            let id = resolve_machine(&members, &machine)?.machine_id.clone();
            let card = client.call::<NetworkRename>(NetworkRenameParams { machine_id: id, name }).await?;
            if json { println!("{}", serde_json::to_string_pretty(&card)?) } else { println!("Renamed to {}.", card.name) }
        }
        MachinesCmd::Merge { found } => {
            let status = status(client).await?;
            let machine = resolve_found(&status.found, &found)?;
            let after = client.call::<NetworkMerge>(NetworkFoundParams { found_id: machine.found_id.clone() }).await?;
            if json {
                println!("{}", serde_json::to_string_pretty(&after)?)
            } else {
                println!("Merged. Your network has {} machines.", after.members.len())
            }
        }
        MachinesCmd::Routes { machine } => {
            let members = status(client).await?.members;
            let id = resolve_machine(&members, &machine)?.machine_id.clone();
            let routes = client.call::<NetworkRoutes>(NetworkRoutesParams { machine_id: id }).await?;
            if json {
                println!("{}", serde_json::to_string_pretty(&routes)?);
            } else if routes.routes.is_empty() {
                println!("No routes: that machine is not reachable from this one right now.");
            } else {
                for route in routes.routes {
                    let via_name = route.via.as_deref().map(|via| {
                        members.iter().find(|card| card.machine_id == via).map_or_else(|| short(via).to_string(), |card| card.name.clone())
                    });
                    println!("{:<20} {}", route_label(route.kind, via_name.as_deref()), route.url);
                }
            }
        }
        MachinesCmd::Doctor { machine } => {
            let machine_id = match machine {
                Some(key) => Some(resolve_machine(&status(client).await?.members, &key)?.machine_id.clone()),
                None => None,
            };
            let report = client.call::<NetworkDoctor>(NetworkDoctorParams { machine_id }).await?;
            let failed = report.checks.iter().any(|check| check.status == DoctorStatus::Fail);
            if json {
                println!("{}", serde_json::to_string_pretty(&report)?)
            } else {
                render_doctor(&report)
            }
            if failed {
                std::process::exit(1);
            }
        }
        MachinesCmd::Enable { .. } | MachinesCmd::Disable => {
            let enabled = matches!(cmd, MachinesCmd::Enable { .. });
            let only_if_unconfigured = matches!(cmd, MachinesCmd::Enable { if_unconfigured: true });
            // Old daemons ignore unknown parameters. Never send a guarded request until
            // the daemon advertises that it understands the configured/off distinction.
            if only_if_unconfigured && client.call::<NetworkStatusMethod>(Empty {}).await?.configured.is_none() {
                anyhow::bail!(
                    "Update Kybern on this machine before using --if-unconfigured; this daemon cannot preserve an earlier off choice."
                );
            }
            let after = client.call::<NetworkSetEnabled>(NetworkSetEnabledParams { enabled, only_if_unconfigured }).await?;
            if json {
                println!("{}", serde_json::to_string_pretty(&after)?)
            } else {
                println!("Connect to your other machines is {}.", if after.enabled { "on" } else { "off" })
            }
        }
        MachinesCmd::Update => {
            let result = client.call::<NetworkUpdateAll>(Empty {}).await?;
            if json {
                println!("{}", serde_json::to_string_pretty(&result)?);
            } else {
                let members = status(client).await?.members;
                let name = |id: &str| {
                    members.iter().find(|card| card.machine_id == id).map_or_else(|| short(id).to_string(), |card| card.name.clone())
                };
                for id in &result.started {
                    println!("{:<20} updating", name(id));
                }
                for skipped in &result.skipped {
                    println!("{:<20} skipped: {}", name(&skipped.machine_id), skipped.reason);
                }
            }
        }
        MachinesCmd::FollowUpdates { state } => {
            let after = client.call::<NetworkFollowUpdatesSet>(NetworkFollowUpdatesParams { enabled: state == "on" }).await?;
            if json {
                println!("{}", serde_json::to_string_pretty(&after)?)
            } else {
                println!("Keeping all machines on the same version is {}.", if after.follow_updates { "on" } else { "off" })
            }
        }
        MachinesCmd::Reachable { state } => {
            let after = match state {
                Some(state) => client.call::<NetworkKeepReachableSet>(NetworkKeepReachableParams { enabled: state == "on" }).await?,
                None => status(client).await?,
            };
            if json {
                println!("{}", serde_json::to_string_pretty(&after.keep_reachable)?)
            } else {
                match &after.keep_reachable {
                    Some(reachable) => println!("{}", reachable_line(reachable)),
                    None => println!("This daemon cannot keep the machine reachable when Kybern is closed. Update Kybern on this machine."),
                }
            }
        }
        MachinesCmd::Broker { cmd } => match cmd {
            BrokerCmd::Status => {
                let status = client.call::<LoginBrokerStatusMethod>(Empty {}).await?;
                if json { println!("{}", serde_json::to_string_pretty(&status)?) } else { render_broker(&status) }
            }
            BrokerCmd::Set { machine, keep_old_signed_in } => {
                let machine_id = if machine.eq_ignore_ascii_case("none") {
                    None
                } else {
                    Some(resolve_machine(&status(client).await?.members, &machine)?.machine_id.clone())
                };
                let result = client.call::<LoginBrokerSet>(LoginBrokerSetParams { machine_id, sign_out_old: !keep_old_signed_in }).await?;
                if json {
                    println!("{}", serde_json::to_string_pretty(&result)?)
                } else {
                    render_broker(&result.status);
                    println!("{} machine(s) use the new broker; {} offline.", result.rebound.len(), result.offline.len());
                    if let Some(note) = &result.note {
                        println!("{note}");
                    }
                }
            }
            BrokerCmd::Use { agent, account, from } => {
                let provider = parse_agent(&agent)?;
                let status = client
                    .call::<LoginBrokerBind>(LoginBrokerBindParams {
                        provider,
                        instance: account,
                        broker_account: Some(from.unwrap_or_else(|| "default".into())),
                    })
                    .await?;
                if json { println!("{}", serde_json::to_string_pretty(&status)?) } else { render_broker(&status) }
            }
            BrokerCmd::Release { agent, account } => {
                let status = client
                    .call::<LoginBrokerBind>(LoginBrokerBindParams {
                        provider: parse_agent(&agent)?,
                        instance: account,
                        broker_account: None,
                    })
                    .await?;
                if json { println!("{}", serde_json::to_string_pretty(&status)?) } else { render_broker(&status) }
            }
        },
        MachinesCmd::PushKey { name, machines } => {
            let result = client.call::<SecretsPush>(SecretsPushParams { name, machines }).await?;
            if json {
                println!("{}", serde_json::to_string_pretty(&result)?);
            } else {
                for machine in &result.machines {
                    let state = match machine.state {
                        SecretsPushState::Delivered => "stored",
                        SecretsPushState::Queued => "waiting for the machine to come back",
                        SecretsPushState::Refused => "refused",
                    };
                    println!("{:<20} {state}{}", machine.name, machine.detail.as_deref().map(|d| format!(": {d}")).unwrap_or_default());
                }
            }
        }
        MachinesCmd::Info => {
            let info = client.call::<MachinesInfo>(Empty {}).await?;
            if json {
                println!("{}", serde_json::to_string_pretty(&info)?);
            } else {
                if let Some(cpu) = &info.cpu {
                    println!("CPU        {} ({} cores)", cpu.model, cpu.logical_cores);
                }
                if let Some(mem) = &info.mem {
                    println!("Memory     {:.1} GiB", mem.total_bytes as f64 / (1024.0 * 1024.0 * 1024.0));
                }
                for gpu in &info.gpu {
                    println!("Graphics   {gpu}");
                }
                for harness in &info.harnesses {
                    let version = harness.version.as_deref().unwrap_or("");
                    println!(
                        "{:<10} {}",
                        format!("{:?}", harness.kind).to_lowercase(),
                        if harness.available { version } else { "not installed" }
                    );
                }
                println!("{} repositories", info.projects.len());
            }
        }
        MachinesCmd::Rejoin => {
            let after = client.call::<NetworkRejoin>(Empty {}).await?;
            if json {
                println!("{}", serde_json::to_string_pretty(&after)?);
            } else {
                println!(
                    "This machine has a new identity and its daemon is stopping so the identity takes effect. A desktop app or a service restarts it; a daemon started by hand needs to be started again (kybernd). Then add this machine from another one, or run kybern machines code here."
                );
            }
        }
    }
    Ok(())
}

/// What a route is, in words.
fn route_label(kind: RouteKind, via: Option<&str>) -> String {
    match kind {
        RouteKind::DirectTailnet => "Tailscale".into(),
        RouteKind::DirectAdvertised => "Advertised URL".into(),
        RouteKind::Ssh => "SSH tunnel".into(),
        RouteKind::Relay => via.map_or_else(|| "Relay".into(), |via| format!("Relay via {via}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_reachable_line_says_what_the_login_item_does_and_what_to_do() {
        let on = KeepReachableStatus {
            enabled: true,
            wanted: true,
            kind: Some(LoginServiceKind::TaskScheduler),
            registered: true,
            running: true,
            external: None,
            error: None,
            warnings: vec![],
        };
        assert_eq!(
            reachable_line(&on),
            "Reachable when Kybern is closed: on. Kybern starts at login through a Task Scheduler task, which started this daemon."
        );
        let off = KeepReachableStatus { enabled: false, registered: false, running: false, ..on.clone() };
        assert!(reachable_line(&off).contains("kybern machines reachable on"));
        let failed = KeepReachableStatus { registered: false, error: Some("Access is denied.".into()), ..on.clone() };
        assert!(reachable_line(&failed).ends_with("\n  Access is denied."));
        assert!(reachable_line(&KeepReachableStatus { kind: None, ..on }).contains("not supported"));
    }

    #[test]
    fn routes_are_named_in_words() {
        assert_eq!(route_label(RouteKind::DirectTailnet, None), "Tailscale");
        assert_eq!(route_label(RouteKind::DirectAdvertised, None), "Advertised URL");
        assert_eq!(route_label(RouteKind::Relay, Some("instabox")), "Relay via instabox");
    }

    fn card(id: &str, name: &str) -> MachineCard {
        MachineCard {
            machine_id: id.into(),
            name: name.into(),
            os: "linux".into(),
            arch: None,
            ts_name: None,
            signing_key: None,
            status: MachineStatus::Online,
            is_self: false,
            version: None,
            network_protocol: 1,
            features: vec![],
            last_seen_at: None,
            relay_via: None,
            harnesses: vec![],
            allow: Default::default(),
            added_by: None,
            added_via: None,
            added_at: None,
        }
    }

    #[test]
    fn a_machine_is_found_by_id_prefix_or_name() {
        let members = vec![card("0192aaaa-1", "Mac"), card("0192bbbb-2", "satyamas"), card("0192cccc-3", "mac")];
        assert_eq!(resolve_machine(&members, "0192bbbb-2").unwrap().name, "satyamas");
        assert_eq!(resolve_machine(&members, "SATYAMAS").unwrap().machine_id, "0192bbbb-2");
        assert_eq!(resolve_machine(&members, "0192cccc").unwrap().machine_id, "0192cccc-3");
        assert_eq!(resolve_machine(&members, "bbb-2").unwrap().machine_id, "0192bbbb-2", "the tail of an id works too");
        assert_eq!(short("019a8c3e-1234-7abc-8def-0123456789ab"), "456789ab");
        assert!(resolve_machine(&members, "mac").unwrap_err().to_string().contains("More than one"));
        assert!(resolve_machine(&members, "nope").unwrap_err().to_string().contains("No machine named"));
        assert!(resolve_machine(&members, "019").is_err(), "a prefix needs four characters");
    }

    #[test]
    fn a_found_machine_is_resolved_by_node_id_or_name() {
        let found = |node: &str, name: &str| FoundMachine {
            found_id: node.into(),
            name: name.into(),
            machine_id: Some(format!("m-{node}")),
            os: None,
            ts_name: None,
            version: None,
            action: FoundAction::Add,
            other_network_id: None,
        };
        let list = vec![found("nA", "pc"), found("nB", "vps")];
        assert_eq!(resolve_found(&list, "nB").unwrap().name, "vps");
        assert_eq!(resolve_found(&list, "PC").unwrap().found_id, "nA");
        assert_eq!(resolve_found(&list, "m-nB").unwrap().name, "vps");
        assert!(resolve_found(&list, "x").is_err());
    }
}
