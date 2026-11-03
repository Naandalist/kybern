//! Members that are not on the tailnet (design §2.4c, §3.5).
//!
//! A machine added by code from outside the tailnet has no tailnet address, so no member can dial it.
//! It dials out instead. Every member that can be reached from the internet says where in the
//! `machine.<id>.url` register (its explicit `--advertise-url`, an `https` address); a machine that
//! cannot see another member on its tailnet opens its session to that URL. The session is the same
//! one a tailnet pair has (signed request, grant, one socket used both ways); the member that was
//! dialed sees an accepted link and relays clients to the machine through it (§3.4).
//!
//! Only members that publish a URL can be dialed. Other members still learn everything the dialed
//! one hears (operations are re-gossiped to every link), and a client reaches the machine through
//! the member holding its link, but a pair where neither side can be dialed has no daemon session.
//! Reaching such a machine from a third member by relaying its session is not implemented.

use std::net::SocketAddr;
use std::time::Duration;

use kybern_protocol::Scope;
use serde_json::json;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::http::HeaderValue;
use tokio_tungstenite::tungstenite::http::header::AUTHORIZATION;

use super::ops::Projection;
use super::peers::{run_dialed, tcp_connect};
use super::session::{self, Target, url_allowed};
use super::{Network, TARGET};
use crate::state::AppState;

/// The register that says where a machine can be reached from outside the tailnet.
pub fn url_key(machine_id: &str) -> String {
    format!("machine.{machine_id}.url")
}

/// The URL member `machine_id` published, if it did.
pub fn url_of(projection: &Projection, machine_id: &str) -> Option<String> {
    projection.register(&url_key(machine_id)).and_then(|register| register.value.as_str()).filter(|url| !url.is_empty()).map(str::to_string)
}

/// The URL this machine can be reached at from the internet: the first explicit advertised address
/// that a session may use.
pub fn own_url(state: &AppState) -> Option<String> {
    let fake = state.network.tailscale.is_fake();
    state.advertised_urls.read().unwrap_or_else(|poisoned| poisoned.into_inner()).iter().find(|url| url_allowed(url, fake)).cloned()
}

/// Write this machine's URL into its register when it changed. A machine that no longer has one
/// clears it, so nobody keeps dialing an address that is gone.
pub async fn publish_url(state: &AppState) {
    let Some(membership) = state.network.membership() else { return };
    if !membership.self_is_member() {
        return;
    }
    let me = membership.machine_id();
    let published = url_of(&membership.projection(), &me);
    let wanted = own_url(state);
    if published == wanted {
        return;
    }
    match membership.set(&url_key(&me), json!(wanted.clone().unwrap_or_default())) {
        Ok(op) => {
            tracing::info!(target: TARGET, url = ?wanted, "published where this machine can be reached from outside the tailnet");
            state.network.publish_ops(vec![op]);
        }
        Err(error) => tracing::warn!(target: TARGET, %error, "could not publish this machine's URL"),
    }
}

/// Open the socket of a session to a published URL. The TCP connection comes from the same source
/// address the handshake used, and TLS (for `wss`) wraps it.
async fn connect_socket(
    network: &Network,
    target: &Target,
    token: &str,
) -> Result<tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>, String> {
    let ws_url = target.ws_url(network);
    let parsed = reqwest::Url::parse(&ws_url).map_err(|error| format!("{ws_url} is not an address: {error}"))?;
    let host = parsed.host_str().ok_or("the address has no host")?.to_string();
    let port = parsed.port_or_known_default().ok_or("the address has no port")?;
    let addr: SocketAddr = tokio::time::timeout(Duration::from_secs(10), tokio::net::lookup_host((host.as_str(), port)))
        .await
        .map_err(|_| format!("could not find {host} in time"))?
        .map_err(|error| format!("could not find {host}: {error}"))?
        .next()
        .ok_or_else(|| format!("{host} has no address"))?;
    let stream = tcp_connect(network, addr).await.map_err(|error| format!("could not connect the session socket: {error}"))?;
    let mut request = ws_url.as_str().into_client_request().map_err(|error| error.to_string())?;
    let header = HeaderValue::from_str(&format!("Bearer {token}")).map_err(|error| error.to_string())?;
    request.headers_mut().insert(AUTHORIZATION, header);
    let wait = super::peers::upgrade_timeout(&network.timing);
    let (socket, _) = tokio::time::timeout(wait, tokio_tungstenite::client_async_tls(request, stream))
        .await
        .map_err(|_| format!("the session socket did not answer within {} s", wait.as_secs_f32()))?
        .map_err(|error| format!("the session socket was refused: {error}"))?;
    Ok(socket)
}

/// Dial a member at the URL it published: verify it, open a session, connect, and run the link.
pub async fn dial(state: &AppState, member: &str, url: &str) -> Result<(), String> {
    let network = &state.network;
    let membership = network.membership().ok_or("the network is unavailable")?;
    let projection = membership.projection();
    let entry = projection.entry(member).filter(|entry| entry.removed.is_none()).ok_or("not a member")?;
    let target = Target::at_url(member.to_string(), entry.descriptor.signing_key.clone(), url.to_string());
    let wanted: Vec<Scope> = Scope::MACHINE.to_vec();
    let established = session::open(network, membership, &target, &wanted, None).await.map_err(|error| error.to_string())?;
    let socket = connect_socket(network, &target, &established.token).await?;
    run_dialed(state.clone(), target, established, socket);
    Ok(())
}
