//! Listeners opened at runtime besides the loopback port. Today that is the
//! machine's Tailscale address, so a phone on the same tailnet can pair and
//! connect directly, without an SSH tunnel or a Serve proxy. The choice is
//! persisted in settings and re-applied on start.

use std::net::{IpAddr, SocketAddr};

use anyhow::{Context, Result};
use axum::Router;
use kybern_protocol::methods::Exposure as ExposureInfo;
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;

use crate::network::Network;

struct Listener {
    addr: SocketAddr,
    stop: CancellationToken,
}

impl Drop for Listener {
    fn drop(&mut self) {
        self.stop.cancel();
    }
}

#[derive(Default)]
struct Inner {
    router: Option<Router>,
    tailscale: Option<Listener>,
    /// The machine network's listener (design §3.1): bound on the Tailscale address at the
    /// network port, independent of the primary listener's port.
    network: Option<NetworkListener>,
}

struct NetworkListener {
    listener: Listener,
    /// The port settings asked for; the listener may hold a later one in the range.
    requested: u16,
}

/// How many ports after the requested one the network listener tries (§3.1): 4173 to 4179.
const NETWORK_PORT_SPAN: u16 = 6;

#[derive(Default)]
pub struct Exposure {
    inner: Mutex<Inner>,
}

/// Whether `bound` already accepts connections on `ip`.
fn covers(bound: SocketAddr, ip: IpAddr) -> bool {
    bound.ip() == ip || (bound.ip().is_unspecified() && bound.is_ipv4() == ip.is_ipv4())
}

impl Exposure {
    /// Hand over the app router once the daemon serves, so extra listeners
    /// run the same routes as the primary one.
    pub async fn attach(&self, router: Router) {
        self.inner.lock().await.router = Some(router);
    }

    /// Addresses of the extra listeners currently open.
    pub async fn extra_addrs(&self) -> Vec<SocketAddr> {
        let inner = self.inner.lock().await;
        let mut addrs: Vec<SocketAddr> = inner.tailscale.iter().map(|listener| listener.addr).collect();
        if let Some(network) = &inner.network
            && !addrs.contains(&network.listener.addr)
        {
            addrs.push(network.listener.addr);
        }
        addrs
    }

    pub async fn status(&self, primary: SocketAddr) -> ExposureInfo {
        let ip = crate::discovery::tailscale_ip().await;
        let inner = self.inner.lock().await;
        let mut listeners = vec![primary.to_string()];
        listeners.extend(inner.tailscale.iter().map(|listener| listener.addr.to_string()));
        if let Some(network) = &inner.network
            && !listeners.contains(&network.listener.addr.to_string())
        {
            listeners.push(network.listener.addr.to_string());
        }
        let tailscale = inner.tailscale.is_some() || inner.network.is_some() || ip.is_some_and(|ip| covers(primary, ip));
        ExposureInfo { tailscale_ip: ip.map(|ip| ip.to_string()), tailscale, listeners }
    }

    /// Open or close the Tailscale listener on the primary listener's port.
    pub async fn set_tailscale(&self, primary: SocketAddr, enabled: bool) -> Result<()> {
        let mut inner = self.inner.lock().await;
        if !enabled {
            if let Some(listener) = inner.tailscale.take() {
                tracing::info!(addr = %listener.addr, "stopped listening on Tailscale");
            }
            return Ok(());
        }
        let ip = crate::discovery::tailscale_ip()
            .await
            .context("Tailscale is not running on this machine, or it runs in userspace mode with no address the daemon can bind")?;
        let addr = SocketAddr::new(ip, primary.port());
        // The network listener may already serve this very address.
        if covers(primary, ip)
            || inner.tailscale.as_ref().is_some_and(|listener| listener.addr.ip() == ip)
            || inner.network.as_ref().is_some_and(|network| network.listener.addr == addr)
        {
            return Ok(());
        }
        let router = inner.router.clone().context("The daemon is still starting; try again in a moment")?;
        let listener = tokio::net::TcpListener::bind(addr).await.with_context(|| format!("Listen on {addr}"))?;
        inner.tailscale = Some(serve_on(listener, addr, router, "Tailscale"));
        Ok(())
    }

    /// Open the machine network's listener on the Tailscale address (design §3.1).
    ///
    /// It binds the network port, not the primary listener's port, which may be the desktop's random
    /// port. Because it binds the Tailscale address only, it cannot shadow or collide with a daemon
    /// on `127.0.0.1:4173`. If the port is taken the next six are tried; if the occupant is another
    /// Kybern daemon on this computer, that is an error rather than a fallback: two daemons share
    /// one Tailscale node, so only one of them can be a machine on the network.
    pub async fn set_network(&self, primary: SocketAddr, network: &Network, want_port: u16) -> Result<SocketAddr> {
        let mut inner = self.inner.lock().await;
        let ip = network
            .tailscale
            .listen_ip()
            .await
            .context("Tailscale is not running on this machine, or it runs in userspace mode with no address the daemon can bind")?;
        if let Some(held) = &inner.network {
            if held.listener.addr.ip() == ip && held.requested == want_port {
                return Ok(held.listener.addr);
            }
            inner.network = None;
        }
        // The primary listener already accepts on the Tailscale address (a wildcard bind): it is the network listener.
        if covers(primary, ip) {
            return Ok(SocketAddr::new(ip, primary.port()));
        }
        let router = inner.router.clone().context("The daemon is still starting; try again in a moment")?;
        let last = want_port.saturating_add(NETWORK_PORT_SPAN);
        for port in want_port..=last {
            let addr = SocketAddr::new(ip, port);
            if inner.tailscale.as_ref().is_some_and(|listener| listener.addr == addr) {
                // The "Tailscale access" listener already serves this address and port (an upgraded
                // daemon holds `<tailscale ip>:4173` this way). It becomes the network listener, so
                // turning "Tailscale access" off later does not close the network.
                let listener = inner.tailscale.take().expect("checked above");
                inner.network = Some(NetworkListener { listener, requested: want_port });
                return Ok(addr);
            }
            match tokio::net::TcpListener::bind(addr).await {
                Ok(listener) => {
                    let held = serve_on(listener, addr, router, "machine network");
                    inner.network = Some(NetworkListener { listener: held, requested: want_port });
                    return Ok(addr);
                }
                Err(error) if error.kind() == std::io::ErrorKind::AddrInUse => {
                    if let Some(other) = occupant(network, addr).await {
                        anyhow::bail!(
                            "Another Kybern daemon on this computer (machine {other}) already serves the machine network on {addr}. A computer can run one daemon on the network, because both would be the same Tailscale node. Turn the network off in one of them."
                        );
                    }
                }
                Err(error) => return Err(error).with_context(|| format!("Listen on {addr}")),
            }
        }
        anyhow::bail!("Ports {want_port} to {last} are all in use on {ip}; free one or choose another network port in settings")
    }

    /// Close the network listener (the network was turned off or Tailscale went away).
    pub async fn clear_network(&self) {
        if let Some(held) = self.inner.lock().await.network.take() {
            tracing::info!(addr = %held.listener.addr, "stopped the machine network listener");
        }
    }
}

fn serve_on(listener: tokio::net::TcpListener, addr: SocketAddr, router: Router, what: &'static str) -> Listener {
    let stop = CancellationToken::new();
    let cancelled = stop.clone();
    tokio::spawn(async move {
        let shutdown = async move { cancelled.cancelled().await };
        use axum::serve::ListenerExt as _;
        // TCP_NODELAY: the machine network's session frames and relayed pages are small, latency-bound writes.
        let listener = listener.tap_io(|socket| {
            let _ = socket.set_nodelay(true);
        });
        if let Err(error) =
            axum::serve(listener, router.into_make_service_with_connect_info::<SocketAddr>()).with_graceful_shutdown(shutdown).await
        {
            tracing::warn!(%addr, %error, "{what} listener ended");
        }
    });
    tracing::info!(%addr, "listening: {what}");
    Listener { addr, stop }
}

/// The id of the Kybern machine that answers the well-known endpoint at `addr`, when it is not this
/// daemon. `None` when nothing there speaks Kybern or the answer is this very daemon.
async fn occupant(network: &Network, addr: SocketAddr) -> Option<String> {
    let base = crate::network::well_known::base_url(addr.ip(), addr.port());
    let answer =
        crate::network::well_known::probe(&network.http, &base, None, std::time::Duration::from_secs(2), network.timing.clock_window)
            .await
            .ok()?;
    (Some(answer.machine_id.clone()) != network.machine_id()).then_some(answer.machine_id)
}

/// Bring the network listener in line with `settings.network` (design §3.1, §12 Q1). Called on every
/// pass of the peer manager, so it also opens the listener once Tailscale comes up and moves it when
/// the address changes. A failure is kept in [`Network::listen_info`] and logged once.
pub async fn apply_network(state: &crate::state::AppState, enabled: bool) {
    let network = &state.network;
    let Some(primary) = *state.listen_addr.read().unwrap_or_else(|poisoned| poisoned.into_inner()) else { return };
    if network.membership().is_none() {
        return;
    }
    if !enabled {
        state.exposure.clear_network().await;
        network.set_listen(Default::default());
        // "Tailscale access" is its own setting: turning the network off leaves it as the user chose.
        if state.settings.get().access.tailscale {
            let _ = state.exposure.set_tailscale(primary, true).await;
        }
        return;
    }
    let want = state.settings.get().network.port;
    let info = match state.exposure.set_network(primary, network, want).await {
        Ok(addr) => crate::network::ListenInfo { addr: Some(addr), error: None },
        Err(error) => {
            let message = format!("{error:#}");
            if network.listen_info().error.as_deref() != Some(message.as_str()) {
                tracing::warn!(target: crate::network::TARGET, error = %message, "the machine network cannot listen");
            }
            crate::network::ListenInfo { addr: None, error: Some(message) }
        }
    };
    network.set_listen(info);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wildcard_and_exact_listeners_cover_the_tailscale_address() {
        let ip: IpAddr = "100.64.1.2".parse().unwrap();
        assert!(covers("0.0.0.0:4173".parse().unwrap(), ip));
        assert!(covers("100.64.1.2:4173".parse().unwrap(), ip));
        assert!(!covers("127.0.0.1:4173".parse().unwrap(), ip));
        assert!(!covers("[::]:4173".parse().unwrap(), ip));
    }

    #[tokio::test]
    async fn disabling_without_a_listener_is_a_no_op_and_enabling_needs_a_router() {
        let exposure = Exposure::default();
        let primary: SocketAddr = "127.0.0.1:4173".parse().unwrap();
        exposure.set_tailscale(primary, false).await.unwrap();
        assert!(exposure.extra_addrs().await.is_empty());
        let status = exposure.status(primary).await;
        assert_eq!(status.listeners, ["127.0.0.1:4173"]);
        assert!(!status.tailscale || status.tailscale_ip.is_none());
    }
}
