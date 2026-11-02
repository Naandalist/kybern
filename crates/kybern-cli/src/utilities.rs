//! `kybern files send` and `kybern ports`: the machine utilities (design §8.1, §8.3).

use std::time::Duration;

use anyhow::{Result, anyhow};
use clap::Subcommand;
use kybern_client::Client;
use kybern_protocol::methods::*;
use kybern_protocol::*;

#[derive(Subcommand)]
pub enum FilesCmd {
    /// Send a file or folder of a project to another machine. It waits in the outbox if that machine is offline.
    Send {
        /// A project id from `kybern projects`.
        project: String,
        /// The file or folder, relative to the project root.
        path: String,
        /// The machine to send to: its name or id.
        #[arg(long)]
        to: String,
        /// Put it in this project of the other machine (a name, id or path) instead of its Downloads/Kybern folder.
        #[arg(long)]
        to_project: Option<String>,
        /// A folder inside that project.
        #[arg(long, requires = "to_project")]
        subdir: Option<String>,
        /// Wait until it has landed (or failed) and print where.
        #[arg(long)]
        wait: bool,
    },
}

#[derive(Subcommand)]
pub enum PortsCmd {
    /// Forward a port of another machine to a port on this one (loopback only).
    Forward {
        /// The machine: its name or id.
        machine: String,
        /// The port on that machine.
        remote_port: u16,
        /// The port to listen on here. Defaults to the remote port or the next free port.
        #[arg(long)]
        local_port: Option<u16>,
        /// Also serve it as https://localhost over HTTP/2 (Fast mode), with Kybern's own certificate.
        #[arg(long)]
        https: bool,
    },
    /// List the forwarded ports.
    List,
    /// Stop a forward.
    Stop {
        /// A forward id from `kybern ports list`.
        id: uuid::Uuid,
    },
}

fn describe_bytes(bytes: u64) -> String {
    match bytes {
        b if b >= 1 << 30 => format!("{:.1} GiB", b as f64 / (1u64 << 30) as f64),
        b if b >= 1 << 20 => format!("{:.1} MiB", b as f64 / (1u64 << 20) as f64),
        b if b >= 1 << 10 => format!("{:.1} KiB", b as f64 / (1u64 << 10) as f64),
        b => format!("{b} B"),
    }
}

pub async fn run_files(client: &Client, cmd: FilesCmd, json: bool) -> Result<()> {
    match cmd {
        FilesCmd::Send { project, path, to, to_project, subdir, wait } => {
            let dest = to_project.map(|project| PushDest::Project { project, subdir });
            let sent = client.call::<FilesSend>(FilesSendParams { project_id: project.parse()?, path, machine: to, dest }).await?;
            let mut item = sent.item.clone();
            if wait {
                loop {
                    let list = client.call::<OutboxList>(OutboxListParams { target: Some(item.target.clone()), state: None }).await?;
                    if let Some(current) = list.items.into_iter().find(|current| current.id == item.id) {
                        item = current;
                    }
                    if !matches!(item.state, OutboxState::Queued | OutboxState::Sending) {
                        break;
                    }
                    tokio::time::sleep(Duration::from_millis(500)).await;
                }
            }
            if json {
                println!("{}", serde_json::to_string_pretty(&serde_json::json!({ "send": sent, "item": item }))?);
            } else {
                println!(
                    "{} file(s), {}{}",
                    sent.files,
                    describe_bytes(sent.bytes),
                    if sent.skipped > 0 { format!(", {} skipped", sent.skipped) } else { String::new() }
                );
                println!("{}  {}", item.id, item.state.as_str());
                match item.state {
                    OutboxState::Delivered => {
                        if let Some(path) = item.result.as_ref().and_then(|result| result["dest_path"].as_str()) {
                            println!("landed at {path}");
                        }
                    }
                    OutboxState::Failed | OutboxState::Expired | OutboxState::Cancelled => {
                        return Err(anyhow!(item.last_error.unwrap_or_else(|| format!("the send {}", item.state.as_str()))));
                    }
                    _ => {
                        if let Some(error) = &item.last_error {
                            println!("waiting: {error}");
                        }
                    }
                }
            }
        }
    }
    Ok(())
}

fn render_forwards(forwards: &[PortForward]) {
    if forwards.is_empty() {
        println!("no forwarded ports");
    }
    for forward in forwards {
        let https = forward.https_port.map(|port| format!("  https://localhost:{port} (HTTP/2)")).unwrap_or_default();
        let rtt = forward.rtt_ms.map(|ms| format!("  {ms} ms")).unwrap_or_default();
        println!(
            "{}  127.0.0.1:{} -> {}:{}  {} connection(s){rtt}{https}",
            forward.id, forward.local_port, forward.machine_name, forward.remote_port, forward.connections
        );
    }
}

pub async fn run_ports(client: &Client, cmd: PortsCmd, json: bool) -> Result<()> {
    match cmd {
        PortsCmd::Forward { machine, remote_port, local_port, https } => {
            let forward = client.call::<PortsForward>(PortsForwardParams { machine, remote_port, local_port, https }).await?;
            if json { println!("{}", serde_json::to_string_pretty(&forward)?) } else { render_forwards(&[forward]) }
        }
        PortsCmd::List => {
            let list = client.call::<PortsList>(Empty {}).await?;
            if json { println!("{}", serde_json::to_string_pretty(&list)?) } else { render_forwards(&list.forwards) }
        }
        PortsCmd::Stop { id } => {
            let before = client.call::<PortsList>(Empty {}).await?;
            let Some(stopped) = before.forwards.iter().find(|forward| forward.id == id) else {
                if json {
                    println!("{}", serde_json::to_string_pretty(&before)?)
                } else {
                    println!("No forward with that id. `kybern ports list` shows them.");
                }
                return Ok(());
            };
            let list = client.call::<PortsStop>(PortsStopParams { id }).await?;
            if json {
                println!("{}", serde_json::to_string_pretty(&list)?)
            } else {
                println!("Stopped the forward from 127.0.0.1:{} to {}:{}.", stopped.local_port, stopped.machine_name, stopped.remote_port);
                if !list.forwards.is_empty() {
                    println!("Still forwarding:");
                    render_forwards(&list.forwards);
                }
            }
        }
    }
    Ok(())
}
