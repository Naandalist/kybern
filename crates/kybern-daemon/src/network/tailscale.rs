//! The Tailscale seam of the machine network (design §3.1).
//!
//! Everything the network needs from Tailscale goes through the [`Tailscale`] trait: who is on the
//! tailnet (`status`), who a connection came from (`whois`), and the address the network listener
//! binds. [`Cli`] runs the `tailscale` command line; [`Fake`] reads a fixture file so several
//! daemons on one box can play a tailnet (§11.3, `KYBERN_TAILSCALE_FAKE=<fixture.json>`).
//!
//! The fixture maps each fake tailnet address to the `127.0.0.x` address its daemon really listens
//! on, and gives every node the identity `whois` reports: the same user, a tagged node, a node
//! shared in from another tailnet, or another user. The plain-HTTP rule accepts `127.0.0.0/8` only
//! under the fake ([`plain_http_allowed`]).

use std::collections::HashMap;
use std::net::IpAddr;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// How long a `whois` answer is reused (§3.1).
const WHOIS_TTL: Duration = Duration::from_secs(60);
/// How long a `tailscale status` answer is shared by concurrent callers.
const STATUS_TTL: Duration = Duration::from_secs(2);
/// Each CLI call is killed after this long.
const CLI_TIMEOUT: Duration = Duration::from_secs(5);

/// A node as `tailscale status --json` lists it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TsNode {
    /// The stable node id (`nXXXXXXCNTRL`), which membership tombstones.
    pub node_id: String,
    pub host_name: String,
    /// The MagicDNS name without the trailing dot.
    pub dns_name: String,
    pub ips: Vec<IpAddr>,
    pub online: bool,
    pub tags: Vec<String>,
    pub user_id: Option<i64>,
    /// Shared in from another tailnet.
    pub shared_in: bool,
}

impl TsNode {
    pub fn tagged(&self) -> bool {
        !self.tags.is_empty()
    }

    /// The short name: the host name, or the first label of the MagicDNS name.
    pub fn name(&self) -> String {
        if !self.host_name.is_empty() {
            return self.host_name.clone();
        }
        self.dns_name.split('.').next().unwrap_or_default().to_string()
    }

    /// The address peers use: the first IPv4 address, else the first address.
    pub fn ip(&self) -> Option<IpAddr> {
        self.ips.iter().find(|ip| ip.is_ipv4()).or_else(|| self.ips.first()).copied()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct TsUser {
    pub login_name: String,
    pub display_name: String,
}

/// What `tailscale status --json` says, with peers.
#[derive(Debug, Clone, Default)]
pub struct TsStatus {
    pub running: bool,
    pub backend_state: String,
    pub self_node: Option<TsNode>,
    pub peers: Vec<TsNode>,
    /// The tailnet's name (`CurrentTailnet.Name`).
    pub tailnet: Option<String>,
    pub users: HashMap<i64, TsUser>,
    pub lock_enabled: Option<bool>,
    /// Why Tailscale is not usable, when it is not.
    pub detail: Option<String>,
}

impl TsStatus {
    /// The login of a user id, when status lists it.
    pub fn login_of(&self, user_id: Option<i64>) -> Option<&str> {
        self.users.get(&user_id?).map(|user| user.login_name.as_str())
    }

    /// The peer with this node id.
    pub fn peer(&self, node_id: &str) -> Option<&TsNode> {
        self.peers.iter().find(|peer| peer.node_id == node_id)
    }

    /// The number of distinct untagged users among the nodes that are not shared in, this node
    /// included. §2.4(b) offers the codeless Add only when this is one.
    pub fn human_users(&self) -> usize {
        let mut users: Vec<i64> = self
            .self_node
            .iter()
            .chain(self.peers.iter())
            .filter(|node| !node.shared_in && !node.tagged())
            .filter_map(|node| node.user_id)
            .collect();
        users.sort_unstable();
        users.dedup();
        users.len()
    }
}

/// What `tailscale whois --json <addr>` says about the node behind an address.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Whois {
    pub node_id: String,
    pub name: String,
    pub ips: Vec<IpAddr>,
    pub tags: Vec<String>,
    /// The owning user's login; `None` for a tagged node.
    pub login_name: Option<String>,
    pub shared_in: bool,
}

impl Whois {
    pub fn tagged(&self) -> bool {
        !self.tags.is_empty()
    }

    /// Untagged, not shared in, and owned by `login`: the identity auto-join and the daemon-to-daemon
    /// handshake require (§2.4, §3.3).
    pub fn is_owner(&self, login: &str) -> bool {
        !self.tagged() && !self.shared_in && self.login_name.as_deref().is_some_and(|name| name.eq_ignore_ascii_case(login))
    }
}

#[async_trait]
pub trait Tailscale: Send + Sync {
    /// `tailscale status --json` with peers.
    async fn status(&self) -> Result<TsStatus>;

    /// `tailscale whois --json <addr>`, cached for 60 s per address.
    async fn whois(&self, addr: IpAddr) -> Result<Whois>;

    /// The address the network listener binds, `None` when Tailscale is not running or runs in
    /// userspace mode with no address the daemon can bind.
    async fn listen_ip(&self) -> Option<IpAddr>;

    /// The address to connect to for a peer's tailnet address. The fake maps it to a loopback
    /// address; the CLI returns it unchanged.
    fn dial_target(&self, ts_ip: IpAddr) -> IpAddr {
        ts_ip
    }

    /// The local address outgoing connections are bound to, so the peer's `whois` can name this
    /// node. Only the fake sets it.
    fn dial_source(&self) -> Option<IpAddr> {
        None
    }

    /// This is the fixture-driven fake.
    fn is_fake(&self) -> bool {
        false
    }
}

/// Whether a daemon may speak plain `http`/`ws` to `ip` (§3.3): only inside the tailnet's address
/// ranges `100.64.0.0/10` and `fd7a:115c:a1e0::/48`, and `127.0.0.0/8` under the fake.
pub fn plain_http_allowed(ip: IpAddr, fake: bool) -> bool {
    match ip {
        IpAddr::V4(ip) => {
            let o = ip.octets();
            (o[0] == 100 && (o[1] & 0xc0) == 0x40) || (fake && o[0] == 127)
        }
        IpAddr::V6(ip) => {
            let s = ip.segments();
            (s[0] == 0xfd7a && s[1] == 0x115c && s[2] == 0xa1e0) || (fake && ip.is_loopback())
        }
    }
}

// ---- the command line ----

/// Candidate locations of the CLI, after `tailscale` on `PATH` (§3.1, §10.1).
fn candidates() -> Vec<PathBuf> {
    let mut list = vec![PathBuf::from(if cfg!(windows) { "tailscale.exe" } else { "tailscale" })];
    if cfg!(target_os = "macos") {
        list.extend(
            ["/Applications/Tailscale.app/Contents/MacOS/Tailscale", "/opt/homebrew/bin/tailscale", "/usr/local/bin/tailscale"]
                .map(PathBuf::from),
        );
    } else if cfg!(windows) {
        list.extend([r"C:\Program Files\Tailscale\tailscale.exe", r"C:\Program Files (x86)\Tailscale\tailscale.exe"].map(PathBuf::from));
    } else {
        list.extend(["/usr/bin/tailscale", "/usr/sbin/tailscale", "/usr/local/bin/tailscale"].map(PathBuf::from));
    }
    list
}

/// The real CLI. Every call is a child process with no console window.
#[derive(Default)]
pub struct Cli {
    binary: Mutex<Option<String>>,
    status: Mutex<Option<(Instant, TsStatus)>>,
    whois: Mutex<HashMap<IpAddr, (Instant, Whois)>>,
    /// The `whois` call running for an address, which concurrent callers wait on instead of starting their own: a
    /// browser opening several connections at once costs one CLI call, not one each.
    whois_running: Mutex<HashMap<IpAddr, Running>>,
}

/// A `whois` call in flight: its answer, or the error's text, once it is known.
type Running = Arc<tokio::sync::OnceCell<std::result::Result<Whois, String>>>;

impl Cli {
    async fn run(&self, args: &[&str]) -> Option<(bool, String)> {
        let known = self.binary.lock().unwrap_or_else(|p| p.into_inner()).clone();
        if let Some(binary) = known {
            return crate::discovery::run_within(&binary, args, CLI_TIMEOUT).await;
        }
        for candidate in candidates() {
            let Some(path) = candidate.to_str() else { continue };
            // A bare name is resolved through PATH; an absolute candidate must exist.
            if candidate.is_absolute() && !candidate.exists() {
                continue;
            }
            if let Some(result) = crate::discovery::run_within(path, args, CLI_TIMEOUT).await {
                *self.binary.lock().unwrap_or_else(|p| p.into_inner()) = Some(path.to_string());
                return Some(result);
            }
        }
        None
    }
}

fn parse_node(value: &Value) -> Option<TsNode> {
    let ips: Vec<IpAddr> = value["TailscaleIPs"].as_array()?.iter().filter_map(|ip| ip.as_str()?.parse().ok()).collect();
    Some(TsNode {
        node_id: value["ID"].as_str().unwrap_or_default().to_string(),
        host_name: value["HostName"].as_str().unwrap_or_default().to_string(),
        dns_name: value["DNSName"].as_str().unwrap_or_default().trim_end_matches('.').to_string(),
        ips,
        online: value["Online"].as_bool().unwrap_or(false),
        tags: value["Tags"].as_array().into_iter().flatten().filter_map(|tag| tag.as_str().map(str::to_string)).collect(),
        user_id: value["UserID"].as_i64(),
        shared_in: value["ShareeNode"].as_bool().unwrap_or(false),
    })
}

/// Parse the output of `tailscale status --json`.
pub fn parse_status(value: &Value) -> TsStatus {
    let backend_state = value["BackendState"].as_str().unwrap_or_default().to_string();
    let self_node = parse_node(&value["Self"]);
    let running = backend_state == "Running" && self_node.as_ref().is_some_and(|node| node.online);
    let peers = value["Peer"].as_object().into_iter().flat_map(|peers| peers.values()).filter_map(parse_node).collect();
    let users = value["User"]
        .as_object()
        .into_iter()
        .flatten()
        .filter_map(|(id, user)| {
            Some((
                id.parse().ok()?,
                TsUser {
                    login_name: user["LoginName"].as_str().unwrap_or_default().to_string(),
                    display_name: user["DisplayName"].as_str().unwrap_or_default().to_string(),
                },
            ))
        })
        .collect();
    let tailnet = value["CurrentTailnet"]["Name"].as_str().map(str::to_string);
    let detail = (!running).then(|| match backend_state.as_str() {
        "" => "Tailscale did not answer".to_string(),
        "NeedsLogin" => "Tailscale needs you to sign in".to_string(),
        "Stopped" => "Tailscale is turned off".to_string(),
        other => format!("Tailscale is {other}"),
    });
    TsStatus { running, backend_state, self_node, peers, tailnet, users, lock_enabled: None, detail }
}

/// Parse the output of `tailscale whois --json`.
pub fn parse_whois(value: &Value) -> Option<Whois> {
    let node = value.get("Node")?;
    let tags: Vec<String> = node["Tags"].as_array().into_iter().flatten().filter_map(|tag| tag.as_str().map(str::to_string)).collect();
    let login = value["UserProfile"]["LoginName"].as_str().filter(|login| !login.is_empty() && *login != "tagged-devices");
    Some(Whois {
        node_id: node["StableID"].as_str().or_else(|| node["ID"].as_str()).unwrap_or_default().to_string(),
        name: node["ComputedName"].as_str().map(str::to_string).unwrap_or_else(|| {
            node["Name"].as_str().unwrap_or_default().trim_end_matches('.').split('.').next().unwrap_or_default().to_string()
        }),
        ips: node["Addresses"].as_array().into_iter().flatten().filter_map(|ip| ip.as_str()?.split('/').next()?.parse().ok()).collect(),
        login_name: if tags.is_empty() { login.map(str::to_string) } else { None },
        tags,
        shared_in: node["Sharer"].as_i64().is_some_and(|sharer| sharer != 0),
    })
}

#[async_trait]
impl Tailscale for Cli {
    async fn status(&self) -> Result<TsStatus> {
        if let Some((at, status)) = &*self.status.lock().unwrap_or_else(|p| p.into_inner())
            && at.elapsed() < STATUS_TTL
        {
            return Ok(status.clone());
        }
        let status = match self.run(&["status", "--json"]).await {
            None => TsStatus { detail: Some("Tailscale is not installed on this machine".into()), ..Default::default() },
            // `tailscale status` exits 1 when the backend is stopped but still prints the JSON.
            Some((_, text)) => match serde_json::from_str::<Value>(&text) {
                Ok(value) => parse_status(&value),
                Err(_) => TsStatus { detail: Some("Tailscale did not answer".into()), ..Default::default() },
            },
        };
        *self.status.lock().unwrap_or_else(|p| p.into_inner()) = Some((Instant::now(), status.clone()));
        Ok(status)
    }

    async fn whois(&self, addr: IpAddr) -> Result<Whois> {
        if let Some((at, whois)) = self.whois.lock().unwrap_or_else(|p| p.into_inner()).get(&addr)
            && at.elapsed() < WHOIS_TTL
        {
            return Ok(whois.clone());
        }
        let running = self.whois_running.lock().unwrap_or_else(|p| p.into_inner()).entry(addr).or_default().clone();
        let answer = running.get_or_init(|| async { self.whois_now(addr).await.map_err(|error| format!("{error:#}")) }).await.clone();
        {
            let mut table = self.whois_running.lock().unwrap_or_else(|p| p.into_inner());
            if table.get(&addr).is_some_and(|current| Arc::ptr_eq(current, &running)) {
                table.remove(&addr);
            }
        }
        answer.map_err(|message| anyhow!(message))
    }

    async fn listen_ip(&self) -> Option<IpAddr> {
        crate::discovery::tailscale_ip().await
    }
}

impl Cli {
    async fn whois_now(&self, addr: IpAddr) -> Result<Whois> {
        let address = addr.to_string();
        let (ok, text) = self.run(&["whois", "--json", &address]).await.context("Tailscale is not installed on this machine")?;
        if !ok {
            return Err(anyhow!("{addr} is not a node on this tailnet"));
        }
        let value: Value = serde_json::from_str(&text).context("Tailscale returned unreadable whois output")?;
        let whois = parse_whois(&value).ok_or_else(|| anyhow!("{addr} is not a node on this tailnet"))?;
        let mut cache = self.whois.lock().unwrap_or_else(|p| p.into_inner());
        cache.retain(|_, (at, _)| at.elapsed() < WHOIS_TTL);
        cache.insert(addr, (Instant::now(), whois.clone()));
        Ok(whois)
    }
}

// ---- the fake ----

/// One node of the fixture.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FakeNode {
    pub name: String,
    pub node_id: String,
    /// The fake tailnet address, in `100.64.0.0/10`.
    pub ip: IpAddr,
    /// The loopback address its daemon listens on.
    pub listen: IpAddr,
    /// The owning user's login; ignored for a tagged node.
    pub login: String,
    #[serde(default)]
    pub user_id: i64,
    #[serde(default)]
    pub tags: Vec<String>,
    #[serde(default)]
    pub shared_in: bool,
    #[serde(default = "online_default")]
    pub online: bool,
}

fn online_default() -> bool {
    true
}

impl FakeNode {
    fn ts_node(&self) -> TsNode {
        TsNode {
            node_id: self.node_id.clone(),
            host_name: self.name.clone(),
            dns_name: format!("{}.fake.ts.net", self.name),
            ips: vec![self.ip],
            online: self.online,
            tags: self.tags.clone(),
            user_id: (self.tags.is_empty()).then_some(self.user_id),
            shared_in: self.shared_in,
        }
    }
}

/// The fixture file: the tailnet, its nodes and which one is the daemon reading it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FakeFixture {
    #[serde(rename = "self")]
    pub self_name: String,
    #[serde(default = "tailnet_default")]
    pub tailnet: String,
    pub nodes: Vec<FakeNode>,
}

fn tailnet_default() -> String {
    "fake.ts.net".into()
}

impl FakeFixture {
    pub fn save(&self, path: &Path) -> Result<()> {
        std::fs::write(path, serde_json::to_vec_pretty(self)?).with_context(|| format!("write {}", path.display()))
    }

    fn node_for(&self, addr: IpAddr) -> Option<&FakeNode> {
        self.nodes.iter().find(|node| node.ip == addr || node.listen == addr)
    }
}

/// A tailnet played by a fixture file, re-read on every call so a test can take a node offline or
/// remove it while daemons run.
pub struct Fake {
    path: PathBuf,
}

impl Fake {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    fn load(&self) -> Result<FakeFixture> {
        let text = std::fs::read_to_string(&self.path).with_context(|| format!("read tailscale fixture {}", self.path.display()))?;
        serde_json::from_str(&text).with_context(|| format!("parse tailscale fixture {}", self.path.display()))
    }
}

#[async_trait]
impl Tailscale for Fake {
    async fn status(&self) -> Result<TsStatus> {
        let fixture = self.load()?;
        let me = fixture
            .nodes
            .iter()
            .find(|node| node.name == fixture.self_name)
            .with_context(|| format!("the fixture has no node {}", fixture.self_name))?;
        let users = fixture
            .nodes
            .iter()
            .filter(|node| node.tags.is_empty())
            .map(|node| (node.user_id, TsUser { login_name: node.login.clone(), display_name: node.login.clone() }))
            .collect();
        Ok(TsStatus {
            // A node that lost its coordination server is "Running" but not online, as the real thing says.
            running: me.online,
            backend_state: "Running".into(),
            self_node: Some(me.ts_node()),
            peers: fixture.nodes.iter().filter(|node| node.name != me.name).map(FakeNode::ts_node).collect(),
            tailnet: Some(fixture.tailnet.clone()),
            users,
            lock_enabled: None,
            detail: None,
        })
    }

    async fn whois(&self, addr: IpAddr) -> Result<Whois> {
        let fixture = self.load()?;
        let node = fixture.node_for(addr).ok_or_else(|| anyhow!("{addr} is not a node on this tailnet"))?;
        Ok(Whois {
            node_id: node.node_id.clone(),
            name: node.name.clone(),
            ips: vec![node.ip],
            tags: node.tags.clone(),
            login_name: node.tags.is_empty().then(|| node.login.clone()),
            shared_in: node.shared_in,
        })
    }

    async fn listen_ip(&self) -> Option<IpAddr> {
        let fixture = self.load().ok()?;
        fixture.nodes.iter().find(|node| node.name == fixture.self_name).map(|node| node.listen)
    }

    fn dial_target(&self, ts_ip: IpAddr) -> IpAddr {
        self.load().ok().and_then(|fixture| fixture.node_for(ts_ip).map(|node| node.listen)).unwrap_or(ts_ip)
    }

    fn dial_source(&self) -> Option<IpAddr> {
        let fixture = self.load().ok()?;
        fixture.nodes.iter().find(|node| node.name == fixture.self_name).map(|node| node.listen)
    }

    fn is_fake(&self) -> bool {
        true
    }
}

/// The fake when `KYBERN_TAILSCALE_FAKE` names a fixture, else the real CLI.
pub fn from_env() -> std::sync::Arc<dyn Tailscale> {
    match std::env::var_os("KYBERN_TAILSCALE_FAKE").filter(|path| !path.is_empty()) {
        Some(path) => {
            tracing::warn!(target: "kybern::network::discovery", fixture = ?path, "using a fake Tailscale fixture; this is a test setup");
            std::sync::Arc::new(Fake::new(path))
        }
        None => std::sync::Arc::new(Cli::default()),
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn status_json() -> Value {
        json!({
            "BackendState": "Running",
            "CurrentTailnet": {"Name": "owner@example.com"},
            "Self": {"ID": "nSELF", "HostName": "mac", "DNSName": "mac.tail1.ts.net.", "TailscaleIPs": ["100.64.0.1", "fd7a:115c:a1e0::1"], "Online": true, "UserID": 7},
            "Peer": {
                "k1": {"ID": "nA", "HostName": "pc", "DNSName": "pc.tail1.ts.net.", "TailscaleIPs": ["100.64.0.2"], "Online": true, "UserID": 7},
                "k2": {"ID": "nB", "HostName": "vps", "DNSName": "vps.tail1.ts.net.", "TailscaleIPs": ["100.64.0.3"], "Online": false, "Tags": ["tag:server"], "UserID": 1},
                "k3": {"ID": "nC", "HostName": "friend", "TailscaleIPs": ["100.64.0.4"], "Online": true, "UserID": 9, "ShareeNode": true}
            },
            "User": {"7": {"LoginName": "owner@example.com"}, "9": {"LoginName": "other@example.com"}, "1": {"LoginName": "tagged-devices"}}
        })
    }

    #[test]
    fn status_parses_self_peers_tags_and_sharing() {
        let status = parse_status(&status_json());
        assert!(status.running);
        assert_eq!(status.tailnet.as_deref(), Some("owner@example.com"));
        let me = status.self_node.as_ref().unwrap();
        assert_eq!(me.name(), "mac");
        assert_eq!(me.ip(), Some("100.64.0.1".parse().unwrap()));
        assert_eq!(status.peers.len(), 3);
        let vps = status.peer("nB").unwrap();
        assert!(vps.tagged() && !vps.online);
        assert!(status.peer("nC").unwrap().shared_in);
        assert_eq!(status.login_of(status.peer("nA").unwrap().user_id), Some("owner@example.com"));
        // Only the owner counts: the tagged node and the shared-in node do not.
        assert_eq!(status.human_users(), 1);
    }

    #[test]
    fn a_stopped_backend_is_not_running_and_says_why() {
        let status =
            parse_status(&json!({"BackendState": "Stopped", "Self": {"ID": "n", "TailscaleIPs": ["100.64.0.1"], "Online": false}}));
        assert!(!status.running);
        assert_eq!(status.detail.as_deref(), Some("Tailscale is turned off"));
    }

    #[test]
    fn whois_distinguishes_owner_tagged_and_shared_in_nodes() {
        let owner = parse_whois(&json!({"Node": {"StableID": "nA", "Name": "pc.tail1.ts.net.", "Addresses": ["100.64.0.2/32"]}, "UserProfile": {"LoginName": "Owner@Example.com"}})).unwrap();
        assert_eq!(owner.node_id, "nA");
        assert_eq!(owner.name, "pc");
        assert!(owner.is_owner("owner@example.com"));
        let tagged = parse_whois(
            &json!({"Node": {"StableID": "nB", "Name": "vps.", "Tags": ["tag:server"]}, "UserProfile": {"LoginName": "tagged-devices"}}),
        )
        .unwrap();
        assert!(tagged.tagged() && tagged.login_name.is_none() && !tagged.is_owner("owner@example.com"));
        let shared = parse_whois(
            &json!({"Node": {"StableID": "nC", "Name": "f.", "Sharer": 12}, "UserProfile": {"LoginName": "owner@example.com"}}),
        )
        .unwrap();
        assert!(shared.shared_in && !shared.is_owner("owner@example.com"));
        assert!(parse_whois(&json!({})).is_none());
    }

    #[test]
    fn plain_http_is_for_the_tailnet_ranges_and_loopback_only_under_the_fake() {
        let ip = |text: &str| -> IpAddr { text.parse().unwrap() };
        assert!(plain_http_allowed(ip("100.64.0.1"), false));
        assert!(plain_http_allowed(ip("100.127.255.254"), false));
        assert!(!plain_http_allowed(ip("100.128.0.1"), false));
        assert!(!plain_http_allowed(ip("100.63.255.1"), false));
        assert!(plain_http_allowed(ip("fd7a:115c:a1e0::5"), false));
        assert!(!plain_http_allowed(ip("fd7a:115c:a1e1::5"), false));
        assert!(!plain_http_allowed(ip("192.168.1.2"), false));
        assert!(!plain_http_allowed(ip("127.0.0.2"), false));
        assert!(plain_http_allowed(ip("127.0.0.2"), true));
        assert!(plain_http_allowed(ip("127.255.1.1"), true));
        assert!(!plain_http_allowed(ip("8.8.8.8"), true));
    }

    #[tokio::test]
    async fn the_fake_plays_a_tailnet_from_a_fixture_that_can_change_while_it_runs() {
        let dir = super::super::testutil::TempDir::new("fake-ts");
        let path = dir.path().join("fixture.json");
        let node = |name: &str, n: u8, tags: &[&str]| FakeNode {
            name: name.into(),
            node_id: format!("n{name}"),
            ip: format!("100.64.0.{n}").parse().unwrap(),
            listen: format!("127.0.0.{}", n + 1).parse().unwrap(),
            login: "me@example.com".into(),
            user_id: 1,
            tags: tags.iter().map(|tag| tag.to_string()).collect(),
            shared_in: false,
            online: true,
        };
        let mut fixture = FakeFixture {
            self_name: "a".into(),
            tailnet: "t.ts.net".into(),
            nodes: vec![node("a", 1, &[]), node("b", 2, &[]), node("v", 3, &["tag:server"])],
        };
        fixture.save(&path).unwrap();
        let fake = Fake::new(&path);
        let status = fake.status().await.unwrap();
        assert_eq!(status.self_node.unwrap().name(), "a");
        assert_eq!(status.peers.len(), 2);
        assert_eq!(fake.listen_ip().await, Some("127.0.0.2".parse().unwrap()));
        assert_eq!(fake.dial_target("100.64.0.2".parse().unwrap()), "127.0.0.3".parse::<IpAddr>().unwrap());
        // The peer sees a connection from the dialer's loopback address and names the dialer.
        let whois = fake.whois(fake.dial_source().unwrap()).await.unwrap();
        assert_eq!(whois.node_id, "na");
        assert!(whois.is_owner("me@example.com"));
        assert!(!fake.whois("100.64.0.3".parse().unwrap()).await.unwrap().is_owner("me@example.com"));
        assert!(fake.whois("100.64.0.99".parse().unwrap()).await.is_err());
        fixture.nodes[1].online = false;
        fixture.save(&path).unwrap();
        assert!(!fake.status().await.unwrap().peers.iter().find(|peer| peer.name() == "b").unwrap().online);
    }
}
