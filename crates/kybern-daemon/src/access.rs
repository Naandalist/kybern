//! Pairing codes and token management. A code is six digits, lives ten
//! minutes, and is exchanged once at `POST /pair` for a client-scoped token.

use std::collections::HashMap;
use std::sync::Mutex;

use anyhow::{Result, anyhow};
use chrono::{DateTime, Duration, Utc};
use kybern_protocol::Scope;
use kybern_store::Store;
use rand::RngExt;
use uuid::Uuid;

/// What a code is for. A device code pairs a client; a machine code lets a member add the machine
/// that shows it to the network (design §2.4c). The two never redeem each other.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Purpose {
    Device,
    Machine,
}

struct Pending {
    label: String,
    expires_at: DateTime<Utc>,
    purpose: Purpose,
}

/// The scopes a paired client gets: the ordinary client scopes plus `network:admin`, so a phone can
/// add and remove machines too (decision 2026-10-28). `Scope::CLIENT` itself stays the read side.
pub fn paired_scopes() -> Vec<Scope> {
    let mut scopes = Scope::CLIENT.to_vec();
    scopes.push(Scope::NetworkAdmin);
    scopes
}

/// Tokens paired before the network existed lack `network:read` and `network:admin`; give them
/// both once at startup. Idempotent. A token that is not a paired client (the bootstrap token has
/// every scope already) is untouched in effect.
pub fn upgrade_paired_tokens(store: &Store) {
    for scope in [Scope::NetworkRead, Scope::NetworkAdmin] {
        match store.tokens_add_scope(scope, |_| true) {
            Ok(0) => {}
            Ok(count) => {
                tracing::info!(target: "kybern::network", count, scope = scope.as_str(), "added a scope to tokens paired before the machine network")
            }
            Err(error) => tracing::warn!(target: "kybern::network", %error, "could not upgrade paired tokens"),
        }
    }
}

const MAX_PAIRING_CODES: usize = 128;
const MAX_PAIRING_LABEL_BYTES: usize = 256;

#[derive(Default)]
pub struct Pairing {
    codes: Mutex<HashMap<String, Pending>>,
    attempts: Mutex<Vec<DateTime<Utc>>>,
}

#[derive(Debug, thiserror::Error)]
#[error("Too many pairing attempts. Wait a minute and try again")]
pub struct PairingRateLimited;

#[derive(Debug, thiserror::Error)]
#[error("That device name is reserved. Choose another name and pair again")]
pub struct ReservedDeviceName;

impl Pairing {
    pub fn create(&self, label: Option<String>) -> Result<(String, DateTime<Utc>)> {
        self.create_for(Purpose::Device, label)
    }

    /// A code this machine shows so a member can add it to a network (`network.code.create`).
    pub fn create_machine_code(&self) -> Result<(String, DateTime<Utc>)> {
        self.create_for(Purpose::Machine, Some("machine join".into()))
    }

    fn create_for(&self, purpose: Purpose, label: Option<String>) -> Result<(String, DateTime<Utc>)> {
        if label.as_ref().is_some_and(|label| label.len() > MAX_PAIRING_LABEL_BYTES) {
            return Err(anyhow!("Device label is too long"));
        }
        if label.as_deref().is_some_and(crate::auth::is_reserved_label) {
            return Err(ReservedDeviceName.into());
        }
        let mut codes = self.codes.lock().unwrap();
        codes.retain(|_, p| p.expires_at > Utc::now());
        if codes.len() >= MAX_PAIRING_CODES {
            return Err(anyhow!("Too many active pairing invitations. Wait for one to expire and retry"));
        }
        let code = loop {
            let n: u32 = rand::rng().random_range(0..1_000_000);
            let code = format!("{n:06}");
            if !codes.contains_key(&code) {
                break code;
            }
        };
        let expires_at = Utc::now() + Duration::minutes(10);
        codes.insert(code.clone(), Pending { label: label.unwrap_or_else(|| "paired device".into()), expires_at, purpose });
        Ok((code, expires_at))
    }

    /// Count an attempt against the global limit: five a minute, whoever guesses. This also works
    /// behind private proxies, without trusting spoofable forwarding headers.
    fn spend_attempt(&self) -> Result<()> {
        let mut attempts = self.attempts.lock().unwrap();
        let now = Utc::now();
        attempts.retain(|at| *at > now - Duration::minutes(1));
        if attempts.len() >= 5 {
            return Err(PairingRateLimited.into());
        }
        attempts.push(now);
        Ok(())
    }

    fn take(&self, code: &str, purpose: Purpose) -> Result<Pending> {
        let mut codes = self.codes.lock().unwrap();
        codes.retain(|_, p| p.expires_at > Utc::now());
        match codes.get(code.trim()) {
            Some(pending) if pending.purpose == purpose => Ok(codes.remove(code.trim()).expect("just found")),
            _ => Err(anyhow!("pairing code is invalid or expired")),
        }
    }

    /// Exchange a code for a new token. Returns the raw token and its scopes.
    pub fn redeem(&self, store: &Store, code: &str, device_name: Option<String>) -> Result<(String, Vec<Scope>)> {
        self.spend_attempt()?;
        // Refused before the code is spent, so the device can retry under another name.
        if device_name.as_deref().is_some_and(crate::auth::is_reserved_label) {
            return Err(ReservedDeviceName.into());
        }
        let pending = self.take(code, Purpose::Device)?;
        let token = crate::auth::generate();
        let label = device_name.unwrap_or(pending.label);
        let scopes = paired_scopes();
        store.token_insert(Uuid::now_v7(), &crate::auth::hash(&token), &label, &scopes)?;
        Ok((token, scopes))
    }

    /// Spend a machine code shown by `network.code.create`. One use.
    pub fn redeem_machine_code(&self, code: &str) -> Result<()> {
        self.spend_attempt()?;
        self.take(code, Purpose::Machine).map(|_| ())
    }
}

struct Ticket {
    principal: crate::auth::Principal,
    expires_at: DateTime<Utc>,
}

/// Browser WebSockets cannot set authorization headers. Exchange the device
/// credential over HTTP for a single-use ticket, never a long-lived URL token.
#[derive(Default)]
pub struct Tickets(Mutex<HashMap<String, Ticket>>);

impl Tickets {
    pub fn create(&self, principal: crate::auth::Principal) -> Result<String> {
        let mut tickets = self.0.lock().unwrap();
        tickets.retain(|_, ticket| ticket.expires_at > Utc::now());
        if tickets.len() >= 1024 {
            return Err(anyhow!("Too many pending connections; retry shortly"));
        }
        let raw = crate::auth::generate();
        tickets.insert(crate::auth::hash(&raw), Ticket { principal, expires_at: Utc::now() + Duration::seconds(30) });
        Ok(raw)
    }

    pub fn redeem(&self, store: &Store, raw: &str) -> Result<Option<crate::auth::Principal>> {
        let Some(ticket) = self.0.lock().unwrap().remove(&crate::auth::hash(raw)) else { return Ok(None) };
        if ticket.expires_at <= Utc::now() || !store.token_is_active(ticket.principal.token_id)? {
            return Ok(None);
        }
        Ok(Some(ticket.principal))
    }
}

/// Every address the daemon listens on: the primary listener plus any opened
/// at runtime through `access.exposure.set`.
pub async fn listeners(state: &crate::state::AppState) -> Vec<std::net::SocketAddr> {
    let primary = *state.listen_addr.read().unwrap();
    let mut listeners: Vec<_> = primary.into_iter().collect();
    listeners.extend(state.exposure.extra_addrs().await);
    listeners
}

/// Explicit advertised URLs take precedence; otherwise discover the current
/// interfaces and Tailscale proxy for this daemon's actual listeners.
pub async fn endpoints(state: &crate::state::AppState) -> Vec<String> {
    let configured = state.advertised_urls.read().unwrap().clone();
    if !configured.is_empty() {
        return configured;
    }
    crate::discovery::endpoints(&listeners(state).await).await
}

pub async fn exposure(state: &crate::state::AppState) -> kybern_protocol::methods::Exposure {
    let primary = *state.listen_addr.read().unwrap();
    match primary {
        Some(primary) => state.exposure.status(primary).await,
        None => kybern_protocol::methods::Exposure { tailscale_ip: None, tailscale: false, listeners: vec![] },
    }
}

/// Open or close the Tailscale listener and remember the choice in settings.
pub async fn set_exposure(state: &crate::state::AppState, tailscale: bool) -> Result<kybern_protocol::methods::Exposure> {
    let primary = (*state.listen_addr.read().unwrap()).ok_or_else(|| anyhow!("The daemon is still starting"))?;
    state.exposure.set_tailscale(primary, tailscale).await?;
    let mut settings = state.settings.get();
    if settings.access.tailscale != tailscale {
        settings.access.tailscale = tailscale;
        state.settings.set(settings)?;
    }
    Ok(state.exposure.status(primary).await)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pairing_limits_guesses_and_expires_codes() {
        let store = Store::open_in_memory().unwrap();
        let pairing = Pairing::default();
        let (code, _) = pairing.create(None).unwrap();
        pairing.codes.lock().unwrap().get_mut(&code).unwrap().expires_at = Utc::now() - Duration::seconds(1);
        assert!(pairing.redeem(&store, &code, None).is_err());
        for _ in 0..4 {
            assert!(pairing.redeem(&store, "bad", None).is_err());
        }
        assert!(pairing.redeem(&store, "bad", None).unwrap_err().is::<PairingRateLimited>());
    }

    #[test]
    fn pairing_bounds_pending_codes_and_labels() {
        let pairing = Pairing::default();
        assert!(pairing.create(Some("x".repeat(MAX_PAIRING_LABEL_BYTES + 1))).is_err());
        for _ in 0..MAX_PAIRING_CODES {
            pairing.create(None).unwrap();
        }
        assert!(pairing.create(None).is_err());
        assert_eq!(pairing.codes.lock().unwrap().len(), MAX_PAIRING_CODES);
    }

    #[test]
    fn tickets_expire_and_cannot_revive_revoked_devices() {
        let store = Store::open_in_memory().unwrap();
        let pairing = Pairing::default();
        let (code, _) = pairing.create(None).unwrap();
        let (token, _) = pairing.redeem(&store, &code, None).unwrap();
        let principal = crate::auth::authenticate(&store, &token).unwrap().unwrap();
        let tickets = Tickets::default();
        let raw = tickets.create(principal.clone()).unwrap();
        tickets.0.lock().unwrap().get_mut(&crate::auth::hash(&raw)).unwrap().expires_at = Utc::now() - Duration::seconds(1);
        assert!(tickets.redeem(&store, &raw).unwrap().is_none());
        let raw = tickets.create(principal.clone()).unwrap();
        store.token_revoke(principal.token_id).unwrap();
        assert!(tickets.redeem(&store, &raw).unwrap().is_none());
    }
}
