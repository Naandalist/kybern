//! The Fast mode of a forwarded preview (ADE-116): `https://localhost:<port>` with HTTP/2 for Kybern's web views.
//!
//! Over plain HTTP/1.1 a browser opens at most six connections to an origin and sends one request at a time on each, so a
//! dev server that serves hundreds of modules (Vite) costs a round trip to the other machine for every six of them. Here
//! the web view speaks HTTP/2 to this listener (TLS with the per-install certificate of [`super::tls`]), and every
//! request travels over **one** HTTP/2 connection to the target ([`super::direct::MODE_HTTP2`]), which sends them on to
//! the dev server over pooled HTTP/1.1. A cold load then costs about one round trip per level of the import graph.
//!
//! WebSocket upgrades (hot reload) arrive over HTTP/1.1, which the web views use for them on the same origin; each gets
//! its own raw direct socket. Server-sent events are ordinary streaming responses. Each request is handled as on the plain
//! listener ([`super::http::exchange`]): counted, cached and compressed the same way.

use std::convert::Infallible;
use std::sync::Arc;
use std::time::Duration;

use hyper::Request;
use hyper::body::Incoming;
use hyper::client::conn::http2::SendRequest;
use hyper_util::rt::{TokioExecutor, TokioIo, TokioTimer};
use kybern_protocol::*;
use tokio::net::TcpListener;
use tokio_util::sync::CancellationToken;

use super::direct::{self, MODE_HTTP2, ProxyBody};
use super::{Counted, Forward};
use crate::network::Network;

const TARGET: &str = "kybern::network::ports";

/// The one HTTP/2 connection of a forward to its target, opened on first use and again after it ends.
#[derive(Default)]
pub struct Tunnel {
    /// The connection and the route it runs on: a new route (a new session) needs a new connection.
    sender: tokio::sync::Mutex<Option<(Arc<direct::Route>, SendRequest<ProxyBody>)>>,
}

impl Tunnel {
    pub(super) async fn sender(
        &self,
        network: &Network,
        route: &Arc<direct::Route>,
        cancel: &CancellationToken,
    ) -> Option<SendRequest<ProxyBody>> {
        let mut held = self.sender.lock().await;
        if let Some((on, sender)) = held.as_ref()
            && Arc::ptr_eq(on, route)
            && !sender.is_closed()
        {
            return Some(sender.clone());
        }
        let socket = route.open(network, MODE_HTTP2).await?;
        let mut builder = hyper::client::conn::http2::Builder::new(TokioExecutor::new());
        builder
            .timer(TokioTimer::new())
            .adaptive_window(true)
            .max_frame_size(64 * 1024)
            .max_send_buf_size(1024 * 1024)
            .keep_alive_interval(Some(Duration::from_secs(20)))
            .keep_alive_timeout(Duration::from_secs(20))
            .keep_alive_while_idle(true);
        let (sender, connection) = match builder.handshake(TokioIo::new(socket)).await {
            Ok(handshake) => handshake,
            Err(error) => {
                tracing::debug!(target: TARGET, ?error, "could not open a forward's HTTP/2 connection");
                return None;
            }
        };
        let cancel = cancel.clone();
        tokio::spawn(async move {
            tokio::select! {
                result = connection => if let Err(error) = result { tracing::debug!(target: TARGET, ?error, "a forward's HTTP/2 connection ended"); },
                _ = cancel.cancelled() => {}
            }
        });
        *held = Some((route.clone(), sender.clone()));
        Some(sender)
    }

    /// Open the connection ahead of the first request.
    pub async fn warm(&self, network: &Network, route: &Arc<direct::Route>, cancel: &CancellationToken) {
        let _ = self.sender(network, route, cancel).await;
    }
}

/// Start the HTTPS listener of `forward` and return its port.
pub(super) async fn start(network: &Arc<Network>, forward: &Arc<Forward>) -> Result<u16, RpcError> {
    let Some(route) = super::current_route(network, forward).await.filter(|route| route.ready_sockets()) else {
        return Err(RpcError::new(
            codes::NOT_AVAILABLE,
            "That machine cannot serve this preview over HTTP/2. Update Kybern there to use Fast mode.",
        ));
    };
    let cert =
        network.ports.certificate().map_err(|error| RpcError::internal(format!("Could not create the preview certificate: {error:#}")))?;
    let listener = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0)).await.map_err(RpcError::internal)?;
    let port = listener.local_addr().map_err(RpcError::internal)?.port();
    let acceptor = tokio_rustls::TlsAcceptor::from(cert.config.clone());
    let (network, forward) = (network.clone(), forward.clone());
    tokio::spawn({
        let (network, forward, route) = (network.clone(), forward.clone(), route.clone());
        async move { forward.tunnel.warm(&network, &route, &forward.cancel).await }
    });
    tokio::spawn(async move {
        loop {
            let accepted = tokio::select! {
                _ = forward.cancel.cancelled() => break,
                accepted = listener.accept() => accepted,
            };
            let Ok((socket, _)) = accepted else { continue };
            let _ = socket.set_nodelay(true);
            let (network, forward, acceptor) = (network.clone(), forward.clone(), acceptor.clone());
            tokio::spawn(async move {
                let _count = Counted::new(&forward);
                let Ok(Ok(tls)) = tokio::time::timeout(Duration::from_secs(10), acceptor.accept(socket)).await else { return };
                let alpn = tls.get_ref().1.alpn_protocol().map(|p| String::from_utf8_lossy(p).into_owned()).unwrap_or_default();
                tracing::debug!(target: TARGET, port = forward.local_port, alpn = %alpn, "a page connection to a Fast listener");
                let service = {
                    let (network, forward) = (network.clone(), forward.clone());
                    hyper::service::service_fn(move |request: Request<Incoming>| {
                        let (network, forward) = (network.clone(), forward.clone());
                        async move { Ok::<_, Infallible>(super::http::exchange(&network, &forward, request, "https").await) }
                    })
                };
                let mut builder = hyper_util::server::conn::auto::Builder::new(TokioExecutor::new());
                builder.http2().timer(TokioTimer::new()).max_concurrent_streams(1024).adaptive_window(true);
                let connection = builder.serve_connection_with_upgrades(TokioIo::new(tls), service);
                tokio::select! {
                    _ = connection => {},
                    _ = forward.cancel.cancelled() => {},
                }
            });
        }
    });
    Ok(port)
}
