//! `previews.servers.list`: find local web servers the user could preview.
//!
//! One scan lists TCP listeners (macOS `lsof`, Linux `/proc`, else a list of
//! common dev ports), each is probed for an HTML answer, and the result is
//! cached for 2 s so any number of polling clients cost one scan. Probe
//! answers are cached per `(pid, port)` for 15 s.

use std::collections::{HashMap, HashSet};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use std::path::Path;
use std::sync::{Arc, Mutex as StdMutex, OnceLock};
use std::time::{Duration, Instant};

use chrono::{DateTime, Utc};
use futures::StreamExt as _;
use kybern_protocol::methods::PreviewServer;

use super::probe;

pub const SCAN_CACHE_TTL: Duration = Duration::from_secs(2);
pub const PROBE_CACHE_TTL: Duration = Duration::from_secs(15);
pub const PROBE_CONCURRENCY: usize = 16;
pub const PROBE_TIMEOUT: Duration = Duration::from_secs(1);
#[cfg(not(target_os = "linux"))]
const LSOF_TIMEOUT: Duration = Duration::from_secs(3);

pub const COMMON_PORTS: [u16; 15] = [3000, 3001, 4173, 4200, 4321, 5000, 5173, 5174, 6006, 8000, 8080, 8081, 8787, 8888, 9000];

/// One listening TCP socket.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Listener {
    pub addr: IpAddr,
    pub port: u16,
    pub pid: Option<u32>,
    pub process_name: Option<String>,
    pub cwd: Option<String>,
}

// ---- parsers (platform independent so they test everywhere) ----

#[cfg_attr(target_os = "linux", allow(dead_code))]
fn parse_socket_name(name: &str) -> Option<(IpAddr, u16)> {
    let name = name.split_whitespace().next()?;
    let (host, port) = name.rsplit_once(':')?;
    let port: u16 = port.parse().ok()?;
    let host = host.trim_start_matches('[').trim_end_matches(']');
    let addr = if host == "*" { IpAddr::V4(Ipv4Addr::UNSPECIFIED) } else { host.parse().ok()? };
    Some((addr, port))
}

/// Parse `lsof -F pcn` output into listeners (one per `n` line).
#[cfg_attr(target_os = "linux", allow(dead_code))]
pub fn parse_lsof_listeners(text: &str) -> Vec<Listener> {
    let mut out = Vec::new();
    let mut pid = None;
    let mut name: Option<String> = None;
    for line in text.lines() {
        let Some((tag, rest)) = line.split_at_checked(1) else { continue };
        match tag {
            "p" => {
                pid = rest.parse().ok();
                name = None;
            }
            "c" => name = Some(rest.to_string()),
            "n" => {
                if let Some((addr, port)) = parse_socket_name(rest) {
                    out.push(Listener { addr, port, pid, process_name: name.clone(), cwd: None });
                }
            }
            _ => {}
        }
    }
    out
}

/// Parse `lsof -d cwd -Fn` output into `pid -> cwd`.
#[cfg_attr(target_os = "linux", allow(dead_code))]
pub fn parse_lsof_cwds(text: &str) -> HashMap<u32, String> {
    let mut out = HashMap::new();
    let mut pid = None;
    for line in text.lines() {
        if let Some(rest) = line.strip_prefix('p') {
            pid = rest.parse().ok();
        } else if let (Some(rest), Some(pid)) = (line.strip_prefix('n'), pid) {
            out.entry(pid).or_insert_with(|| rest.to_string());
        }
    }
    out
}

/// A LISTEN row of `/proc/net/tcp{,6}`: bind address, port and socket inode.
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub struct ProcSocket {
    pub addr: IpAddr,
    pub port: u16,
    pub inode: u64,
}

#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn hex_ip(hex: &str) -> Option<IpAddr> {
    // Each 32-bit word is printed in host (little-endian) byte order.
    let word = |chunk: &str| u32::from_str_radix(chunk, 16).ok().map(u32::to_le_bytes);
    match hex.len() {
        8 => Some(IpAddr::V4(Ipv4Addr::from(word(hex)?))),
        32 => {
            let mut bytes = [0u8; 16];
            for i in 0..4 {
                bytes[i * 4..i * 4 + 4].copy_from_slice(&word(&hex[i * 8..i * 8 + 8])?);
            }
            Some(IpAddr::V6(Ipv6Addr::from(bytes)))
        }
        _ => None,
    }
}

/// Parse `/proc/net/tcp` or `/proc/net/tcp6`, keeping state `0A` (LISTEN).
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub fn parse_proc_net_tcp(text: &str) -> Vec<ProcSocket> {
    text.lines()
        .skip(1)
        .filter_map(|line| {
            let fields: Vec<&str> = line.split_whitespace().collect();
            if fields.get(3) != Some(&"0A") {
                return None;
            }
            let (ip, port) = fields.get(1)?.split_once(':')?;
            Some(ProcSocket { addr: hex_ip(ip)?, port: u16::from_str_radix(port, 16).ok()?, inode: fields.get(9)?.parse().ok()? })
        })
        .collect()
}

/// Keep listeners reachable from this machine: `127.0.0.0/8`, `::1`, `0.0.0.0`, `::`.
pub fn is_local_bind(addr: IpAddr) -> bool {
    addr.is_loopback() || addr.is_unspecified()
}

// ---- platform listing ----

#[cfg(not(target_os = "linux"))]
async fn host_uid() -> Option<String> {
    let text = crate::discovery::output_within("id", &["-u"], Duration::from_secs(2)).await?;
    let uid = text.trim();
    (!uid.is_empty() && uid.bytes().all(|b| b.is_ascii_digit())).then(|| uid.to_string())
}

#[cfg(not(target_os = "linux"))]
async fn list_lsof() -> Option<Vec<Listener>> {
    let uid = host_uid().await?;
    let (_, text) =
        crate::discovery::run_within("lsof", &["-a", "-u", &uid, "-iTCP", "-sTCP:LISTEN", "-nP", "-F", "pcn"], LSOF_TIMEOUT).await?;
    let mut listeners = parse_lsof_listeners(&text);
    let mut pids: Vec<u32> = listeners.iter().filter_map(|l| l.pid).collect();
    pids.sort_unstable();
    pids.dedup();
    if !pids.is_empty() {
        let list = pids.iter().map(u32::to_string).collect::<Vec<_>>().join(",");
        if let Some((_, text)) = crate::discovery::run_within("lsof", &["-a", "-p", &list, "-d", "cwd", "-Fn"], LSOF_TIMEOUT).await {
            let cwds = parse_lsof_cwds(&text);
            for listener in &mut listeners {
                listener.cwd = listener.pid.and_then(|pid| cwds.get(&pid).cloned());
            }
        }
    }
    Some(listeners)
}

#[cfg(target_os = "linux")]
async fn list_proc() -> Option<Vec<Listener>> {
    tokio::task::spawn_blocking(|| {
        let mut sockets = parse_proc_net_tcp(&std::fs::read_to_string("/proc/net/tcp").ok()?);
        if let Ok(v6) = std::fs::read_to_string("/proc/net/tcp6") {
            sockets.extend(parse_proc_net_tcp(&v6));
        }
        // Map socket inodes to processes this user can inspect.
        let wanted: HashSet<u64> = sockets.iter().map(|s| s.inode).collect();
        let mut owners: HashMap<u64, u32> = HashMap::new();
        for entry in std::fs::read_dir("/proc").ok()?.flatten() {
            let Some(pid) = entry.file_name().to_str().and_then(|n| n.parse::<u32>().ok()) else { continue };
            let Ok(fds) = std::fs::read_dir(entry.path().join("fd")) else { continue };
            for fd in fds.flatten() {
                let Ok(target) = std::fs::read_link(fd.path()) else { continue };
                let target = target.to_string_lossy();
                if let Some(inode) = target.strip_prefix("socket:[").and_then(|t| t.strip_suffix(']')).and_then(|t| t.parse().ok())
                    && wanted.contains(&inode)
                {
                    owners.entry(inode).or_insert(pid);
                }
            }
        }
        let mut info: HashMap<u32, (Option<String>, Option<String>)> = HashMap::new();
        Some(
            sockets
                .into_iter()
                // Sockets of other users (no owner we can see) are not ours to offer.
                .filter_map(|socket| {
                    let pid = *owners.get(&socket.inode)?;
                    let (name, cwd) = info.entry(pid).or_insert_with(|| {
                        (
                            std::fs::read_to_string(format!("/proc/{pid}/comm")).ok().map(|s| s.trim().to_string()),
                            std::fs::read_link(format!("/proc/{pid}/cwd")).ok().map(|p| p.display().to_string()),
                        )
                    });
                    Some(Listener { addr: socket.addr, port: socket.port, pid: Some(pid), process_name: name.clone(), cwd: cwd.clone() })
                })
                .collect(),
        )
    })
    .await
    .ok()
    .flatten()
}

/// Without a listener table (Windows, or no `lsof`), the usual dev ports are probed. They are listed as bound to `::`
/// so a probe tries 127.0.0.1 and then [::1]: Node resolves `localhost` to ::1 on Windows, and a Vite server there
/// listens on [::1] alone.
fn common_port_listeners() -> Vec<Listener> {
    COMMON_PORTS
        .iter()
        .map(|&port| Listener { addr: IpAddr::V6(Ipv6Addr::UNSPECIFIED), port, pid: None, process_name: None, cwd: None })
        .collect()
}

async fn list_listeners() -> (Vec<Listener>, &'static str) {
    #[cfg(target_os = "linux")]
    if let Some(listeners) = list_proc().await {
        return (listeners, "proc");
    }
    #[cfg(not(target_os = "linux"))]
    if let Some(listeners) = list_lsof().await {
        return (listeners, "lsof");
    }
    (common_port_listeners(), "common_ports")
}

// ---- probing and caching ----

/// What a probe of a candidate learned; `None` means "not a web page".
#[derive(Debug, Clone)]
struct Answer {
    title: Option<String>,
    favicon: Option<String>,
    framework: Option<String>,
}

#[derive(Debug, Clone, Copy, Default)]
pub struct ScanOptions {
    /// Ports owned by the daemon itself.
    pub own_port: Option<u16>,
    pub own_pid: Option<u32>,
}

pub struct Snapshot {
    pub servers: Vec<PreviewServer>,
    pub scanned_at: DateTime<Utc>,
    pub method: &'static str,
}

type ProbeKey = (Option<u32>, u16);

pub struct Scanner {
    snapshot: tokio::sync::Mutex<Option<(Instant, Arc<Snapshot>)>>,
    answers: StdMutex<HashMap<ProbeKey, (Instant, Option<Answer>)>>,
}

impl Default for Scanner {
    fn default() -> Self {
        Self::new()
    }
}

/// The process-wide scanner every client shares.
pub fn shared() -> &'static Scanner {
    static SHARED: OnceLock<Scanner> = OnceLock::new();
    SHARED.get_or_init(Scanner::new)
}

impl Scanner {
    pub fn new() -> Self {
        Self { snapshot: tokio::sync::Mutex::new(None), answers: StdMutex::default() }
    }

    /// The latest snapshot, scanning only if the cached one is older than 2 s.
    /// Concurrent callers wait on the same scan.
    pub async fn scan(&self, options: ScanOptions) -> Arc<Snapshot> {
        let mut cached = self.snapshot.lock().await;
        if let Some((at, snapshot)) = cached.as_ref()
            && at.elapsed() < SCAN_CACHE_TTL
        {
            return snapshot.clone();
        }
        let (listeners, method) = list_listeners().await;
        let snapshot = Arc::new(self.build(listeners, method, options).await);
        *cached = Some((Instant::now(), snapshot.clone()));
        snapshot
    }

    async fn build(&self, listeners: Vec<Listener>, method: &'static str, options: ScanOptions) -> Snapshot {
        let candidates = candidates(listeners, options);
        let seen: HashSet<ProbeKey> = candidates.iter().map(|c| (c.pid, c.port)).collect();
        let servers = futures::stream::iter(candidates)
            .map(|candidate| async move {
                let answer = self.answer(&candidate).await?;
                Some(server(&candidate, answer))
            })
            .buffer_unordered(PROBE_CONCURRENCY)
            .filter_map(|server| async move { server })
            .collect::<Vec<_>>()
            .await;
        self.answers.lock().unwrap_or_else(|p| p.into_inner()).retain(|key, (at, _)| seen.contains(key) && at.elapsed() < PROBE_CACHE_TTL);
        let mut servers = servers;
        servers.sort_by_key(|s| s.port);
        Snapshot { servers, scanned_at: Utc::now(), method }
    }

    async fn answer(&self, candidate: &Candidate) -> Option<Answer> {
        let key = (candidate.pid, candidate.port);
        if let Some((at, answer)) = self.answers.lock().unwrap_or_else(|p| p.into_inner()).get(&key)
            && at.elapsed() < PROBE_CACHE_TTL
        {
            return answer.clone();
        }
        let answer = probe_candidate(candidate).await;
        self.answers.lock().unwrap_or_else(|p| p.into_inner()).insert(key, (Instant::now(), answer.clone()));
        answer
    }
}

#[derive(Debug, Clone)]
struct Candidate {
    port: u16,
    /// Loopback hosts to try, in order.
    hosts: Vec<IpAddr>,
    /// Prefer `[::1]` in the URL (listener is IPv6-loopback only).
    v6_only: bool,
    pid: Option<u32>,
    process_name: Option<String>,
    cwd: Option<String>,
}

fn candidates(listeners: Vec<Listener>, options: ScanOptions) -> Vec<Candidate> {
    let mut by_port: Vec<Candidate> = Vec::new();
    for listener in listeners {
        if !is_local_bind(listener.addr)
            || Some(listener.port) == options.own_port
            || (listener.pid.is_some() && listener.pid == options.own_pid)
            || listener.process_name.as_deref().is_some_and(|n| {
                let n = n.to_ascii_lowercase();
                n.contains("cuadriver") || n.contains("cua-driver")
            })
        {
            continue;
        }
        let v6 = listener.addr.is_ipv6() && !listener.addr.is_unspecified();
        let hosts: Vec<IpAddr> = match listener.addr {
            addr if addr.is_unspecified() && addr.is_ipv6() => vec![IpAddr::V4(Ipv4Addr::LOCALHOST), IpAddr::V6(Ipv6Addr::LOCALHOST)],
            addr if addr.is_unspecified() => vec![IpAddr::V4(Ipv4Addr::LOCALHOST)],
            addr => vec![addr],
        };
        if let Some(existing) = by_port.iter_mut().find(|c| c.port == listener.port) {
            for host in hosts {
                if !existing.hosts.contains(&host) {
                    existing.hosts.push(host);
                }
            }
            existing.v6_only &= v6;
            existing.pid = existing.pid.or(listener.pid);
            existing.process_name = existing.process_name.take().or(listener.process_name);
            existing.cwd = existing.cwd.take().or(listener.cwd);
        } else {
            by_port.push(Candidate {
                port: listener.port,
                hosts,
                v6_only: v6,
                pid: listener.pid,
                process_name: listener.process_name,
                cwd: listener.cwd,
            });
        }
    }
    by_port
}

fn host_literal(ip: IpAddr) -> String {
    match ip {
        IpAddr::V4(ip) => ip.to_string(),
        IpAddr::V6(ip) => format!("[{ip}]"),
    }
}

async fn probe_candidate(candidate: &Candidate) -> Option<Answer> {
    for host in &candidate.hosts {
        let url = format!("http://{}:{}/", host_literal(*host), candidate.port);
        let Ok(outcome) = probe::probe_detailed(&url, PROBE_TIMEOUT).await else { continue };
        if outcome.result.error.is_some() && outcome.result.status.is_none() {
            continue;
        }
        if !(outcome.is_html() || outcome.is_redirect()) {
            return None;
        }
        let favicon = match outcome.resolved {
            Some(addr) => probe::favicon_data_url(&url, outcome.favicon_href.as_deref(), addr, PROBE_TIMEOUT).await,
            None => None,
        };
        let framework = framework_hint(outcome.vite, outcome.powered_by.as_deref(), outcome.server.as_deref());
        return Some(Answer { title: outcome.result.title, favicon, framework });
    }
    None
}

/// A short framework name from Vite's client marker or the `X-Powered-By` / `Server` headers.
pub fn framework_hint(vite: bool, powered_by: Option<&str>, server: Option<&str>) -> Option<String> {
    if vite {
        return Some("Vite".into());
    }
    let clean = |value: &str| {
        let value = value.trim();
        (!value.is_empty()).then(|| value.chars().take(40).collect::<String>())
    };
    powered_by.and_then(clean).or_else(|| server.and_then(clean))
}

fn server(candidate: &Candidate, answer: Answer) -> PreviewServer {
    let (host, url) = if candidate.v6_only {
        ("::1".to_string(), format!("http://[::1]:{}", candidate.port))
    } else {
        ("localhost".to_string(), format!("http://localhost:{}", candidate.port))
    };
    PreviewServer {
        url,
        port: candidate.port,
        host,
        pid: candidate.pid,
        process_name: candidate.process_name.clone(),
        cwd: candidate.cwd.clone(),
        title: answer.title,
        favicon: answer.favicon,
        framework: answer.framework,
        in_project: false,
    }
}

/// Mark `in_project` for roots (canonical or raw) and sort in-project servers first.
pub fn mark_in_project(servers: &[PreviewServer], roots: &[&Path]) -> Vec<PreviewServer> {
    let roots: Vec<_> = roots.iter().flat_map(|root| [Some(root.to_path_buf()), std::fs::canonicalize(root).ok()]).flatten().collect();
    let mut out: Vec<PreviewServer> = servers
        .iter()
        .cloned()
        .map(|mut s| {
            s.in_project = s.cwd.as_deref().is_some_and(|cwd| roots.iter().any(|root| Path::new(cwd).starts_with(root)));
            s
        })
        .collect();
    out.sort_by_key(|s| (!s.in_project, s.port));
    out
}

/// `previews.servers.list`: the shared snapshot, with `in_project` for the thread's roots.
pub async fn servers_list(
    state: &crate::state::AppState,
    thread_id: Option<kybern_protocol::ThreadId>,
) -> anyhow::Result<kybern_protocol::methods::PreviewServersListResult> {
    let mut roots: Vec<std::path::PathBuf> = Vec::new();
    if let Some(id) = thread_id {
        let thread = state.store.thread_get(id)?.ok_or_else(|| anyhow::anyhow!("thread not found"))?;
        roots.push(thread.cwd.clone().into());
        if let Some(project) = state.store.project_get(thread.project_id)? {
            roots.push(project.path.into());
        }
    }
    let port = state.port.load(std::sync::atomic::Ordering::Relaxed);
    let snapshot = shared().scan(ScanOptions { own_port: (port != 0).then_some(port), own_pid: Some(std::process::id()) }).await;
    let root_refs: Vec<&Path> = roots.iter().map(|r| r.as_path()).collect();
    Ok(kybern_protocol::methods::PreviewServersListResult {
        servers: mark_in_project(&snapshot.servers, &root_refs),
        scanned_at: snapshot.scanned_at,
        method: snapshot.method.to_string(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    const LSOF: &str = "p101\ncnode\nn*:5173\nn[::1]:5173\np202\ncpostgres\nn127.0.0.1:5432\nn[::]:5432\np303\ncPython\nn192.168.1.5:8000\np404\ncmulti\nn*:3000 (LISTEN)\n";

    #[test]
    fn lsof_listeners_parse() {
        let listeners = parse_lsof_listeners(LSOF);
        assert_eq!(listeners.len(), 6);
        assert_eq!(
            listeners[0],
            Listener { addr: "0.0.0.0".parse().unwrap(), port: 5173, pid: Some(101), process_name: Some("node".into()), cwd: None }
        );
        assert_eq!(listeners[1].addr, "::1".parse::<IpAddr>().unwrap());
        assert_eq!(listeners[2].addr, "127.0.0.1".parse::<IpAddr>().unwrap());
        assert_eq!(listeners[3].addr, "::".parse::<IpAddr>().unwrap());
        assert_eq!(listeners[3].pid, Some(202));
        assert_eq!((listeners[5].port, listeners[5].pid), (3000, Some(404)));
    }

    #[test]
    fn lsof_cwds_parse() {
        let cwds = parse_lsof_cwds("p101\nfcwd\nn/Users/me/app\np202\nfcwd\nn/\n");
        assert_eq!(cwds.get(&101).map(String::as_str), Some("/Users/me/app"));
        assert_eq!(cwds.get(&202).map(String::as_str), Some("/"));
    }

    #[test]
    fn proc_net_tcp_parses_listeners_only() {
        let v4 = "  sl  local_address rem_address   st tx_queue rx_queue tr tm->when retrnsmt   uid  timeout inode\n   0: 0100007F:1435 00000000:0000 0A 00000000:00000000 00:00000000 00000000  1000        0 12345 1 0000000000000000 100 0 0 10 0\n   1: 00000000:0050 00000000:0000 0A 00000000:00000000 00:00000000 00000000     0        0 222 1 0 100 0 0 10 0\n   2: 0100007F:8000 0100007F:1435 01 00000000:00000000 00:00000000 00000000  1000        0 999 1 0 100 0 0 10 0\n";
        let sockets = parse_proc_net_tcp(v4);
        assert_eq!(sockets.len(), 2);
        assert_eq!(sockets[0], ProcSocket { addr: "127.0.0.1".parse().unwrap(), port: 5173, inode: 12345 });
        assert_eq!(sockets[1].addr, "0.0.0.0".parse::<IpAddr>().unwrap());
        assert_eq!(sockets[1].port, 80);
    }

    #[test]
    fn proc_net_tcp6_parses_addresses() {
        let v6 = "  sl  local_address                         remote_address                        st tx_queue rx_queue tr tm->when retrnsmt   uid  timeout inode\n   0: 00000000000000000000000001000000:0BB8 00000000000000000000000000000000:0000 0A 00000000:00000000 00:00000000 00000000  1000        0 777 1 0 100 0 0 10 0\n   1: 00000000000000000000000000000000:1F90 00000000000000000000000000000000:0000 0A 00000000:00000000 00:00000000 00000000  1000        0 778 1 0 100 0 0 10 0\n   2: 0000000000000000FFFF00000100007F:0050 00000000000000000000000000000000:0000 0A 00000000:00000000 00:00000000 00000000  1000        0 779 1 0 100 0 0 10 0\n";
        let sockets = parse_proc_net_tcp(v6);
        assert_eq!(sockets.len(), 3);
        assert_eq!(sockets[0], ProcSocket { addr: "::1".parse().unwrap(), port: 3000, inode: 777 });
        assert_eq!(sockets[1].addr, "::".parse::<IpAddr>().unwrap());
        assert_eq!(sockets[1].port, 8080);
        assert_eq!(sockets[2].addr, "::ffff:127.0.0.1".parse::<IpAddr>().unwrap());
    }

    fn listener(addr: &str, port: u16, pid: u32, name: &str) -> Listener {
        Listener { addr: addr.parse().unwrap(), port, pid: Some(pid), process_name: Some(name.into()), cwd: None }
    }

    #[test]
    fn common_ports_are_probed_on_ipv4_and_ipv6_loopback() {
        let found = candidates(common_port_listeners(), ScanOptions::default());
        let vite = found.iter().find(|c| c.port == 5173).unwrap();
        assert_eq!(vite.hosts, vec![IpAddr::V4(Ipv4Addr::LOCALHOST), IpAddr::V6(Ipv6Addr::LOCALHOST)]);
        assert!(!vite.v6_only);
    }

    #[test]
    fn candidates_filter_and_merge() {
        let listeners = vec![
            listener("0.0.0.0", 5173, 1, "node"),
            listener("::1", 5173, 1, "node"),
            listener("192.168.1.5", 8000, 2, "python"),
            listener("127.0.0.1", 4199, 3, "kybernd"),
            listener("127.0.0.1", 5000, 4, "kybernd"),
            listener("::1", 6000, 5, "node"),
            listener("127.0.0.1", 7000, 6, "CuaDriver"),
        ];
        let found = candidates(listeners, ScanOptions { own_port: Some(4199), own_pid: Some(4) });
        let ports: Vec<u16> = found.iter().map(|c| c.port).collect();
        assert_eq!(ports, [5173, 6000]);
        assert!(!found[0].v6_only);
        assert!(found[1].v6_only);
    }

    #[test]
    fn framework_hints() {
        assert_eq!(framework_hint(true, Some("Express"), None).as_deref(), Some("Vite"));
        assert_eq!(framework_hint(false, Some("Next.js"), Some("x")).as_deref(), Some("Next.js"));
        assert_eq!(framework_hint(false, None, Some(" nginx/1.2 ")).as_deref(), Some("nginx/1.2"));
        assert_eq!(framework_hint(false, None, None), None);
    }

    #[test]
    fn in_project_sorts_first() {
        let make = |port, cwd: &str| PreviewServer {
            url: format!("http://localhost:{port}"),
            port,
            host: "localhost".into(),
            pid: None,
            process_name: None,
            cwd: Some(cwd.into()),
            title: None,
            favicon: None,
            framework: None,
            in_project: false,
        };
        let servers = vec![make(3000, "/other/app"), make(5173, "/work/proj/web")];
        let marked = mark_in_project(&servers, &[Path::new("/work/proj")]);
        assert_eq!(marked[0].port, 5173);
        assert!(marked[0].in_project && !marked[1].in_project);
    }

    async fn serve(response: &'static str) -> u16 {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            loop {
                let Ok((mut socket, _)) = listener.accept().await else { return };
                tokio::spawn(async move {
                    let mut buf = [0u8; 4096];
                    let n = socket.read(&mut buf).await.unwrap_or(0);
                    let request = String::from_utf8_lossy(&buf[..n]).into_owned();
                    let reply = if request.starts_with("GET /icon.svg") {
                        "HTTP/1.1 200 OK\r\nContent-Type: image/svg+xml\r\nConnection: close\r\n\r\n<svg/>".to_string()
                    } else {
                        response.to_string()
                    };
                    let _ = socket.write_all(reply.as_bytes()).await;
                    let _ = socket.shutdown().await;
                });
            }
        });
        port
    }

    #[tokio::test]
    async fn build_keeps_html_servers_with_title_favicon_framework() {
        let html = serve("HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nX-Powered-By: Express\r\nConnection: close\r\n\r\n<title>My app</title><link rel=icon href=/icon.svg>").await;
        let json = serve("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nConnection: close\r\n\r\n{}").await;
        let redirect = serve("HTTP/1.1 302 Found\r\nLocation: /app\r\nConnection: close\r\n\r\n").await;
        let ssh = serve("SSH-2.0-OpenSSH\r\n").await;
        let listeners =
            [html, json, redirect, ssh].iter().enumerate().map(|(i, &port)| listener("127.0.0.1", port, 10 + i as u32, "t")).collect();
        let scanner = Scanner::new();
        let snapshot = scanner.build(listeners, "lsof", ScanOptions::default()).await;
        let ports: Vec<u16> = snapshot.servers.iter().map(|s| s.port).collect();
        let mut expected = vec![html, redirect];
        expected.sort_unstable();
        assert_eq!(ports, expected);
        let page = snapshot.servers.iter().find(|s| s.port == html).unwrap();
        assert_eq!(page.title.as_deref(), Some("My app"));
        assert_eq!(page.framework.as_deref(), Some("Express"));
        assert!(page.favicon.as_deref().is_some_and(|f| f.starts_with("data:image/svg+xml;base64,")));
        assert_eq!(page.url, format!("http://localhost:{html}"));
    }

    #[tokio::test]
    async fn probe_answers_are_cached_per_pid_and_port() {
        let port = serve("HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nConnection: close\r\n\r\n<title>a</title>").await;
        let scanner = Scanner::new();
        let make = || vec![listener("127.0.0.1", port, 77, "node")];
        assert_eq!(scanner.build(make(), "lsof", ScanOptions::default()).await.servers.len(), 1);
        // A second scan answers from the cache even though nothing listens any more.
        let key = (Some(77), port);
        scanner.answers.lock().unwrap().get_mut(&key).unwrap().1.as_mut().unwrap().title = Some("cached".into());
        let again = scanner.build(make(), "lsof", ScanOptions::default()).await;
        assert_eq!(again.servers[0].title.as_deref(), Some("cached"));
    }
}
