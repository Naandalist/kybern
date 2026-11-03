//! Daemon-to-daemon sessions (design §3.3): the signed handshake, the short-lived grant it yields,
//! and the table that checks a grant at use time.
//!
//! ```text
//! A                                          B
//! GET /.well-known/kybern/machine?nonce=nA →
//!                              ← descriptor + proof(nA)    A checks B's key from the membership set
//! POST /network/session {from, to, nA, at, scopes, sig_A} →
//!                              B: whois(source) is A's node? A a member, not removed?
//!                                 sig_A verifies, `at` fresh, nA never seen
//!                              ← {grant, scopes, nB, sig_B over nA‖nB}
//! WS /ws  Authorization: Bearer <grant>
//! ```
//!
//! A grant is a random `kyb_m_…` token. Only its hash is kept, in memory, so grants never survive a
//! restart. It is bound to the holder's machine id and source address, lasts an hour, and is
//! renewed after half of that by a new signed request that names it.
//!
//! A machine that was removed while it was offline asks for a session as if it were still a member. Once its
//! signature verifies against the key the set recorded for it, and if its request lists
//! [`features::REMOVAL_PROOF_V1`], the refusal carries the signed operations that removed it
//! ([`RemovalNotice`]); the caller verifies them like a sync and leaves the network (ADE-120).

use std::collections::{HashMap, VecDeque};
use std::net::IpAddr;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use base64::Engine;
use base64::engine::general_purpose::{STANDARD as B64, URL_SAFE_NO_PAD as B64URL};
use chrono::{DateTime, Utc};
use kybern_protocol::{OpEnvelope, Scope, features};
use rand::RngExt;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use super::keys;
use super::tailscale::plain_http_allowed;
use super::well_known::{self, Expected, ProbeError, base_url, now_rfc3339};
use super::{Membership, Network};
use crate::state::AppState;
use crate::ws::Peer;

pub const SESSION_PATH: &str = "/network/session";
const REQUEST_DOMAIN: &str = "kybern-session-v1\0";
const RESPONSE_DOMAIN: &str = "kybern-session-resp-v1\0";
pub const GRANT_PREFIX: &str = "kyb_m_";

const MAX_GRANTS: usize = 256;
const MAX_GRANTS_PER_MACHINE: usize = 8;
const MAX_REPLAY_ENTRIES: usize = 4096;

/// The signed request that opens or renews a session.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionRequest {
    pub from: String,
    pub to: String,
    /// The nonce `from` sent in its well-known check of `to`. Single use.
    pub nonce_a: String,
    /// RFC 3339; within a minute of the target's clock.
    pub at: String,
    /// Machine scopes wanted, `machine:peer` and the like.
    pub scopes: Vec<String>,
    /// The id of a grant to extend instead of issuing a new one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub renews: Option<String>,
    /// `ed25519(from, request_message)`, standard base64.
    pub sig: String,
    /// What the caller understands in an answer, such as [`features::REMOVAL_PROOF_V1`]. Not signed: it only asks
    /// for more in a refusal, and an older target ignores it.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub features: Vec<String>,
}

/// The body of the 403 a removed machine gets for a session request (ADE-120): the operations that removed it
/// and the chain that makes their signers valid ([`super::ops::OpSet::removal_proof`]), nothing else of the set.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RemovalNotice {
    pub message: String,
    pub removed: Vec<OpEnvelope>,
}

/// The target's answer.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionResponse {
    /// The grant token; absent when a grant was renewed (the holder keeps its token).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub grant: Option<String>,
    pub grant_id: String,
    pub scopes: Vec<String>,
    pub expires_at: DateTime<Utc>,
    pub nonce_b: String,
    /// `ed25519(to, response_message)`, standard base64.
    pub sig_b: String,
}

fn scope_list(scopes: &[String]) -> String {
    let mut sorted: Vec<&str> = scopes.iter().map(String::as_str).collect();
    sorted.sort_unstable();
    sorted.join(",")
}

pub(crate) fn request_message(request: &SessionRequest) -> Vec<u8> {
    let parts = [
        request.from.as_str(),
        request.to.as_str(),
        request.nonce_a.as_str(),
        request.at.as_str(),
        &scope_list(&request.scopes),
        request.renews.as_deref().unwrap_or(""),
    ];
    format!("{REQUEST_DOMAIN}{}", parts.join("\0")).into_bytes()
}

fn response_message(nonce_a: &str, nonce_b: &str, from: &str, to: &str, grant_id: &str) -> Vec<u8> {
    format!("{RESPONSE_DOMAIN}{}", [nonce_a, nonce_b, from, to, grant_id].join("\0")).into_bytes()
}

// ---- grants ----

/// What a grant allows its holder.
#[derive(Debug, Clone)]
pub struct Grant {
    pub id: Uuid,
    pub machine_id: String,
    /// The tailnet address the session request came from; the socket must come from it too.
    pub origin_ip: IpAddr,
    pub scopes: Vec<Scope>,
    pub issued_at: DateTime<Utc>,
    pub expires_at: DateTime<Utc>,
    expires: Instant,
}

impl Grant {
    fn live(&self) -> bool {
        Instant::now() < self.expires
    }
}

/// The grants this machine has issued. Held in memory only.
#[derive(Default)]
pub struct Grants {
    table: Mutex<HashMap<String, Grant>>,
}

impl Grants {
    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<String, Grant>> {
        self.table.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Make a grant for `machine_id`; the raw token is returned once and never kept.
    pub fn issue(&self, machine_id: &str, origin_ip: IpAddr, scopes: Vec<Scope>, ttl: Duration) -> (String, Grant) {
        let mut raw = [0u8; 32];
        rand::rng().fill(&mut raw);
        let token = format!("{GRANT_PREFIX}{}", B64URL.encode(raw));
        let now = Utc::now();
        let grant = Grant {
            id: Uuid::now_v7(),
            machine_id: machine_id.to_string(),
            origin_ip,
            scopes,
            issued_at: now,
            expires_at: now + chrono::Duration::from_std(ttl).unwrap_or(chrono::Duration::hours(1)),
            expires: Instant::now() + ttl,
        };
        let mut table = self.lock();
        table.retain(|_, grant| grant.live());
        // A machine that reconnects many times keeps only its newest grants, and the table is bounded.
        let mut held: Vec<(String, DateTime<Utc>)> =
            table.iter().filter(|(_, held)| held.machine_id == machine_id).map(|(hash, held)| (hash.clone(), held.issued_at)).collect();
        held.sort_by_key(|(_, issued)| *issued);
        while held.len() >= MAX_GRANTS_PER_MACHINE {
            table.remove(&held.remove(0).0);
        }
        if table.len() >= MAX_GRANTS
            && let Some(oldest) = table.iter().min_by_key(|(_, grant)| grant.issued_at).map(|(hash, _)| hash.clone())
        {
            table.remove(&oldest);
        }
        table.insert(crate::auth::hash(&token), grant.clone());
        (token, grant)
    }

    /// The live grant a bearer token names.
    pub fn lookup(&self, token: &str) -> Option<Grant> {
        self.lock().get(&crate::auth::hash(token)).filter(|grant| grant.live()).cloned()
    }

    /// The live grant with this id.
    pub fn get(&self, id: Uuid) -> Option<Grant> {
        self.lock().values().find(|grant| grant.id == id && grant.live()).cloned()
    }

    pub fn is_active(&self, id: Uuid) -> bool {
        self.get(id).is_some()
    }

    /// Extend a grant by `ttl` from now, only for the machine and address it was issued to.
    pub fn renew(&self, id: Uuid, machine_id: &str, origin_ip: IpAddr, ttl: Duration) -> Option<Grant> {
        let mut table = self.lock();
        let grant = table.values_mut().find(|grant| grant.id == id && grant.live())?;
        if grant.machine_id != machine_id || grant.origin_ip != origin_ip {
            return None;
        }
        grant.expires = Instant::now() + ttl;
        grant.expires_at = Utc::now() + chrono::Duration::from_std(ttl).unwrap_or(chrono::Duration::hours(1));
        Some(grant.clone())
    }

    /// Revoke every grant held by `machine_id` (removal, §2.5). Returns the ids, which the
    /// connection layer turns into closed sockets.
    pub fn revoke_machine(&self, machine_id: &str) -> Vec<Uuid> {
        let mut table = self.lock();
        let ids = table.values().filter(|grant| grant.machine_id == machine_id).map(|grant| grant.id).collect();
        table.retain(|_, grant| grant.machine_id != machine_id);
        ids
    }

    pub fn revoke(&self, id: Uuid) {
        self.lock().retain(|_, grant| grant.id != id);
    }

    /// Live grants, for the doctor's "a grant is held, with time left".
    pub fn held(&self) -> Vec<Grant> {
        self.lock().values().filter(|grant| grant.live()).cloned().collect()
    }
}

/// Nonces a session request has used. A captured request cannot be replayed within its window.
#[derive(Default)]
pub struct ReplayCache {
    seen: Mutex<VecDeque<(Instant, String)>>,
}

impl ReplayCache {
    /// Record `nonce`; `false` when it was already used within `window`.
    pub fn insert(&self, nonce: &str, window: Duration) -> bool {
        let mut seen = self.seen.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        let now = Instant::now();
        // Keep a nonce twice as long as the window in which its request would be accepted.
        let keep = window * 2;
        while seen.front().is_some_and(|(at, _)| now.duration_since(*at) > keep) {
            seen.pop_front();
        }
        if seen.iter().any(|(_, used)| used == nonce) {
            return false;
        }
        if seen.len() >= MAX_REPLAY_ENTRIES {
            seen.pop_front();
        }
        seen.push_back((now, nonce.to_string()));
        true
    }
}

// ---- the target: POST /network/session ----

fn refuse(status: StatusCode, message: impl Into<String>) -> Response {
    (status, message.into()).into_response()
}

/// Handle a session request. Every refusal is logged under `kybern::network::session` without the
/// token, signature or nonce.
pub async fn serve(State(state): State<AppState>, Peer(peer): Peer, Json(request): Json<SessionRequest>) -> Response {
    let network = &state.network;
    let Some(membership) = network.membership() else {
        return refuse(StatusCode::SERVICE_UNAVAILABLE, "the machine network is unavailable on this machine");
    };
    if !network.is_enabled() {
        return refuse(StatusCode::NOT_FOUND, "the machine network is turned off on this machine");
    }
    let Some(peer) = peer else { return refuse(StatusCode::BAD_REQUEST, "the source address is unknown") };
    if let Some(notice) = removal_notice(network, membership, &request) {
        tracing::info!(target: "kybern::network::session", machine = %request.from, source = %peer.ip(), "told a removed machine that it was removed");
        return (StatusCode::FORBIDDEN, Json(notice)).into_response();
    }
    match handle(&state, membership, peer.ip(), request).await {
        Ok(response) => Json(response).into_response(),
        Err((status, reason)) => {
            tracing::warn!(target: "kybern::network::session", source = %peer.ip(), %reason, "refused a session request");
            refuse(status, reason)
        }
    }
}

/// The answer to a removed machine that asks for a session (ADE-120), when it may have one: it asked for it, it was
/// removed (not merely unknown), its request is fresh and signed by the key the set recorded for it, the nonce is
/// new, and it was not told within [`super::Timing::removal_notice_every`]. Otherwise `None`, and the request is
/// refused plainly like any other non-member's. A stranger learns nothing: without the removed machine's key no
/// signature verifies.
fn removal_notice(network: &Network, membership: &Membership, request: &SessionRequest) -> Option<RemovalNotice> {
    if !request.features.iter().any(|feature| feature == features::REMOVAL_PROOF_V1) || request.to != membership.machine_id() {
        return None;
    }
    let projection = membership.projection();
    let entry = projection.entry(&request.from).filter(|entry| entry.removed.is_some())?;
    let window = network.timing.clock_window;
    let at = DateTime::parse_from_rfc3339(&request.at).ok()?;
    if (Utc::now() - at.with_timezone(&Utc)).abs() > chrono::Duration::from_std(window).unwrap_or(chrono::Duration::seconds(60)) {
        return None;
    }
    let signature = B64.decode(&request.sig).ok()?;
    keys::verify_signature(&entry.descriptor.signing_key, &request_message(request), &signature).ok()?;
    if !network.replay.insert(&request.nonce_a, window) || !network.removal_notice_due(&request.from) {
        return None;
    }
    let removed = membership.removal_proof(&request.from);
    (!removed.is_empty()).then(|| RemovalNotice { message: "this machine was removed from the network".into(), removed })
}

async fn handle(
    state: &AppState,
    membership: &Membership,
    source: IpAddr,
    request: SessionRequest,
) -> Result<SessionResponse, (StatusCode, String)> {
    let network = &state.network;
    let me = membership.machine_id();
    if request.to != me {
        return Err((StatusCode::BAD_REQUEST, "this request is for another machine".into()));
    }
    let projection = membership.projection();
    let Some(entry) = projection.entry(&request.from).filter(|entry| entry.removed.is_none()) else {
        return Err((StatusCode::FORBIDDEN, "this machine is not a member of the network".into()));
    };
    let window = network.timing.clock_window;
    let at = DateTime::parse_from_rfc3339(&request.at).map_err(|_| (StatusCode::BAD_REQUEST, "`at` is not a time".to_string()))?;
    if (Utc::now() - at.with_timezone(&Utc)).abs() > chrono::Duration::from_std(window).unwrap_or(chrono::Duration::seconds(60)) {
        return Err((StatusCode::UNAUTHORIZED, "the request is too old or its clock is off".into()));
    }
    let signature = B64.decode(&request.sig).map_err(|_| (StatusCode::UNAUTHORIZED, "the signature is not valid".to_string()))?;
    keys::verify_signature(&entry.descriptor.signing_key, &request_message(&request), &signature)
        .map_err(|_| (StatusCode::UNAUTHORIZED, "the signature does not verify".to_string()))?;
    if !network.replay.insert(&request.nonce_a, window) {
        return Err((StatusCode::UNAUTHORIZED, "this request was already used".into()));
    }
    // Bound to the address: the tailnet says the source is the node the membership set recorded. A
    // member that joined from outside the tailnet (a code, §2.4c) has no node to check; it is known by
    // its signed request alone, and its grant is bound to the address it came from like any other.
    if let Some(recorded) = entry.descriptor.ts_node_id.as_deref() {
        let whois = network
            .tailscale
            .whois(source)
            .await
            .map_err(|error| (StatusCode::FORBIDDEN, format!("the source is not on the tailnet: {error}")))?;
        if recorded != whois.node_id || projection.is_node_tombstoned(&whois.node_id) {
            return Err((StatusCode::FORBIDDEN, "the source address is not this machine's tailnet node".into()));
        }
    }

    let ttl = network.timing.grant_ttl;
    if let Some(renews) = request.renews.as_deref() {
        let id = Uuid::parse_str(renews).map_err(|_| (StatusCode::BAD_REQUEST, "`renews` is not a grant id".to_string()))?;
        let grant = network
            .grants
            .renew(id, &request.from, source, ttl)
            .ok_or_else(|| (StatusCode::UNAUTHORIZED, "that grant is gone; open a new session".to_string()))?;
        return Ok(respond(membership, &request, None, &grant));
    }

    let wanted: Vec<Scope> =
        request.scopes.iter().filter_map(|name| Scope::MACHINE.into_iter().find(|scope| scope.as_str() == name)).collect();
    let policy = network.policy_scopes(&request.from);
    let mut scopes: Vec<Scope> = wanted.into_iter().filter(|scope| policy.contains(scope)).collect();
    if !scopes.contains(&Scope::MachinePeer) {
        scopes.insert(0, Scope::MachinePeer);
    }
    scopes.dedup();
    let (token, grant) = network.grants.issue(&request.from, source, scopes, ttl);
    tracing::info!(target: "kybern::network::session", machine = %request.from, "issued a session grant");
    network.notify_changed();
    Ok(respond(membership, &request, Some(token), &grant))
}

fn respond(membership: &Membership, request: &SessionRequest, token: Option<String>, grant: &Grant) -> SessionResponse {
    let nonce_b = well_known::new_nonce();
    let grant_id = grant.id.to_string();
    let signature = membership.keys().sign(&response_message(&request.nonce_a, &nonce_b, &request.from, &request.to, &grant_id));
    SessionResponse {
        grant: token,
        grant_id,
        scopes: grant.scopes.iter().map(|scope| scope.as_str().to_string()).collect(),
        expires_at: grant.expires_at,
        nonce_b,
        sig_b: B64.encode(signature),
    }
}

// ---- the caller ----

/// The machine a session is opened to.
#[derive(Debug, Clone)]
pub struct Target {
    pub machine_id: String,
    /// The signing key the membership set records for it.
    pub signing_key: String,
    /// Its tailnet address.
    pub ts_ip: IpAddr,
    pub port: u16,
    /// The URL it published in its `machine.<id>.url` register, for a machine reached from outside
    /// the tailnet (§3.5). When set, `ts_ip` and `port` are not used.
    pub url: Option<String>,
}

impl Target {
    /// A target reached at a published URL rather than a tailnet address.
    pub fn at_url(machine_id: String, signing_key: String, url: String) -> Self {
        Self { machine_id, signing_key, ts_ip: IpAddr::V4(std::net::Ipv4Addr::UNSPECIFIED), port: 0, url: Some(url) }
    }

    /// The address actually connected to (the fake maps it to loopback).
    pub fn base(&self, network: &Network) -> String {
        match &self.url {
            Some(url) => url.trim_end_matches('/').to_string(),
            None => base_url(network.tailscale.dial_target(self.ts_ip), self.port),
        }
    }

    pub fn ws_url(&self, network: &Network) -> String {
        format!("{}/ws", self.base(network).replacen("https://", "wss://", 1).replacen("http://", "ws://", 1))
    }
}

/// Whether a published URL may carry a session: only `https`, since a session crosses the internet,
/// and plain loopback under the fake tailnet the tests play.
pub fn url_allowed(url: &str, fake: bool) -> bool {
    let Ok(parsed) = reqwest::Url::parse(url) else { return false };
    match parsed.scheme() {
        "https" => true,
        "http" => fake && parsed.host_str().is_some_and(|host| host == "localhost" || host.starts_with("127.")),
        _ => false,
    }
}

/// A session that is open.
#[derive(Debug, Clone)]
pub struct Established {
    pub token: String,
    pub grant_id: String,
    pub scopes: Vec<Scope>,
    pub expires_at: DateTime<Utc>,
}

#[derive(Debug, thiserror::Error)]
pub enum SessionError {
    /// The identity check failed; nothing but the well-known GET was sent.
    #[error("{0}")]
    Identity(#[from] ProbeError),
    #[error("{0} is outside the tailnet, so no session is opened over plain HTTP")]
    NotTailnet(IpAddr),
    #[error("{0} is not an https address, so no session is opened to it")]
    NotHttps(String),
    #[error("the machine refused the session ({status}): {message}")]
    Refused { status: u16, message: String },
    #[error("the machine answered with a session response that does not verify")]
    BadResponse,
    /// The target said this machine was removed, with operations that prove it; they were applied.
    #[error("this machine was removed from the network")]
    Removed,
    #[error("{0}")]
    Transport(String),
}

fn probe_timeout(ip: IpAddr) -> Duration {
    if ip.is_loopback() { Duration::from_secs(3) } else { Duration::from_secs(10) }
}

/// A member refused a session because this machine was removed, and sent the operations that say so (ADE-120).
/// They are verified exactly as a sync verifies them and kept only if they make this machine removed; then the
/// removal runs as if it had been pushed. Anything else is a plain refusal.
pub(super) fn take_removal_notice(network: &Network, from: &str, notice: RemovalNotice) -> SessionError {
    let refused =
        |message: &str| SessionError::Refused { status: StatusCode::FORBIDDEN.as_u16(), message: message.chars().take(200).collect() };
    let Some(membership) = network.membership() else { return refused(&notice.message) };
    match membership.apply_removal_proof(notice.removed) {
        Ok(outcome) => {
            tracing::warn!(target: "kybern::network::session", machine = %from, "a member says this machine was removed, and its operations prove it");
            network.after_apply(Some(from), &outcome);
            SessionError::Removed
        }
        Err(error) => {
            tracing::warn!(target: "kybern::network::session", machine = %from, %error, "ignored a removal notice that does not prove this machine was removed");
            refused("the machine refused the session")
        }
    }
}

/// Verify the target, then open (or renew) a session with it. The well-known check runs first and
/// uses the key from the membership set; if it fails, nothing else is sent (§3.2).
pub async fn open(
    network: &Network,
    membership: &Membership,
    target: &Target,
    scopes: &[Scope],
    renews: Option<&str>,
) -> Result<Established, SessionError> {
    let (base, timeout) = match &target.url {
        Some(url) => {
            if !url_allowed(url, network.tailscale.is_fake()) {
                return Err(SessionError::NotHttps(url.clone()));
            }
            let timeout = if url.contains("127.") || url.contains("localhost") { Duration::from_secs(3) } else { Duration::from_secs(10) };
            (target.base(network), timeout)
        }
        None => {
            if !plain_http_allowed(target.ts_ip, network.tailscale.is_fake()) {
                return Err(SessionError::NotTailnet(target.ts_ip));
            }
            (target.base(network), probe_timeout(network.tailscale.dial_target(target.ts_ip)))
        }
    };
    let expected = Expected { machine_id: &target.machine_id, signing_key: &target.signing_key };
    let nonce_a = well_known::new_nonce();
    // The nonce of the check is the nonce of the request: the proof binds the answer to this attempt.
    let url = format!("{base}{}?nonce={nonce_a}", well_known::PATH);
    let response = network
        .http
        .get(&url)
        .timeout(timeout)
        .send()
        .await
        .map_err(|error| ProbeError::Unreachable(if error.is_timeout() { "timed out".into() } else { "could not connect".into() }))?;
    if !response.status().is_success() {
        return Err(ProbeError::Unreachable(format!("answered {}", response.status())).into());
    }
    let answer: well_known::WellKnown = response.json().await.map_err(|_| ProbeError::Malformed)?;
    well_known::verify(&answer, &nonce_a, well_known::authority(&base), Some(expected), network.timing.clock_window)?;

    let me = membership.machine_id();
    let mut request = SessionRequest {
        from: me.clone(),
        to: target.machine_id.clone(),
        nonce_a: nonce_a.clone(),
        at: now_rfc3339(),
        scopes: scopes.iter().map(|scope| scope.as_str().to_string()).collect(),
        renews: renews.map(str::to_string),
        sig: String::new(),
        features: vec![features::REMOVAL_PROOF_V1.to_string()],
    };
    request.sig = B64.encode(membership.keys().sign(&request_message(&request)));
    let response = network
        .http
        .post(format!("{base}{SESSION_PATH}"))
        .timeout(timeout)
        .json(&request)
        .send()
        .await
        .map_err(|error| SessionError::Transport(error.without_url().to_string()))?;
    if !response.status().is_success() {
        let status = response.status().as_u16();
        let message = response.text().await.unwrap_or_default();
        if status == StatusCode::FORBIDDEN.as_u16()
            && let Ok(notice) = serde_json::from_str::<RemovalNotice>(&message)
        {
            return Err(take_removal_notice(network, &target.machine_id, notice));
        }
        return Err(SessionError::Refused { status, message: message.chars().take(200).collect() });
    }
    let granted: SessionResponse = response.json().await.map_err(|_| SessionError::BadResponse)?;
    let signature = B64.decode(&granted.sig_b).map_err(|_| SessionError::BadResponse)?;
    keys::verify_signature(
        &target.signing_key,
        &response_message(&nonce_a, &granted.nonce_b, &me, &target.machine_id, &granted.grant_id),
        &signature,
    )
    .map_err(|_| SessionError::BadResponse)?;
    let token = match (granted.grant, renews) {
        (Some(token), _) if token.starts_with(GRANT_PREFIX) => token,
        (None, Some(_)) => String::new(),
        _ => return Err(SessionError::BadResponse),
    };
    let scopes = granted.scopes.iter().filter_map(|name| Scope::MACHINE.into_iter().find(|scope| scope.as_str() == name)).collect();
    Ok(Established { token, grant_id: granted.grant_id, scopes, expires_at: granted.expires_at })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_request_message_binds_every_field_in_order() {
        let request = SessionRequest {
            from: "a".into(),
            to: "b".into(),
            nonce_a: "n".into(),
            at: "t".into(),
            scopes: vec!["machine:relay".into(), "machine:peer".into()],
            renews: None,
            sig: String::new(),
            features: vec![],
        };
        assert_eq!(request_message(&request), b"kybern-session-v1\0a\0b\0n\0t\0machine:peer,machine:relay\0");
        // Scope order does not matter; every other field does.
        let reordered = SessionRequest { scopes: vec!["machine:peer".into(), "machine:relay".into()], ..request.clone() };
        assert_eq!(request_message(&request), request_message(&reordered));
        for changed in [
            SessionRequest { from: "x".into(), ..request.clone() },
            SessionRequest { to: "x".into(), ..request.clone() },
            SessionRequest { nonce_a: "x".into(), ..request.clone() },
            SessionRequest { at: "x".into(), ..request.clone() },
            SessionRequest { scopes: vec![], ..request.clone() },
            SessionRequest { renews: Some("g".into()), ..request.clone() },
        ] {
            assert_ne!(request_message(&request), request_message(&changed));
        }
    }

    #[test]
    fn grants_are_bound_expire_renew_and_revoke_with_their_machine() {
        let grants = Grants::default();
        let ip: IpAddr = "100.64.0.2".parse().unwrap();
        let (token, grant) = grants.issue("m1", ip, vec![Scope::MachinePeer], Duration::from_secs(3600));
        assert!(token.starts_with(GRANT_PREFIX));
        assert_eq!(grants.lookup(&token).unwrap().id, grant.id);
        assert!(grants.lookup("kyb_m_nope").is_none());
        assert!(grants.is_active(grant.id));
        // Renewal only works for the machine and address the grant was issued to.
        assert!(grants.renew(grant.id, "m2", ip, Duration::from_secs(3600)).is_none());
        assert!(grants.renew(grant.id, "m1", "100.64.0.9".parse().unwrap(), Duration::from_secs(3600)).is_none());
        assert!(grants.renew(grant.id, "m1", ip, Duration::from_secs(3600)).is_some());
        let (_, other) = grants.issue("m2", ip, vec![Scope::MachinePeer], Duration::from_secs(3600));
        assert_eq!(grants.revoke_machine("m1"), vec![grant.id]);
        assert!(grants.lookup(&token).is_none() && !grants.is_active(grant.id));
        assert!(grants.is_active(other.id));
        let (token, short) = grants.issue("m3", ip, vec![], Duration::from_millis(1));
        std::thread::sleep(Duration::from_millis(10));
        assert!(grants.lookup(&token).is_none() && !grants.is_active(short.id));
    }

    #[test]
    fn a_machine_keeps_only_its_newest_grants() {
        let grants = Grants::default();
        let ip: IpAddr = "100.64.0.2".parse().unwrap();
        let first = grants.issue("m1", ip, vec![], Duration::from_secs(3600));
        for _ in 0..MAX_GRANTS_PER_MACHINE {
            grants.issue("m1", ip, vec![], Duration::from_secs(3600));
        }
        assert!(grants.lookup(&first.0).is_none());
        assert_eq!(grants.held().len(), MAX_GRANTS_PER_MACHINE);
    }

    #[test]
    fn a_nonce_is_used_once() {
        let cache = ReplayCache::default();
        assert!(cache.insert("n1", Duration::from_secs(60)));
        assert!(!cache.insert("n1", Duration::from_secs(60)));
        assert!(cache.insert("n2", Duration::from_secs(60)));
    }
}
