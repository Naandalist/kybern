//! Bearer tokens. The raw token lives only in the client's hands and the
//! bootstrap file; the store keeps SHA-256 hashes.

use anyhow::Result;
use base64::Engine;
use kybern_protocol::Scope;
use kybern_store::Store;
use rand::RngExt;
use sha2::{Digest, Sha256};
use uuid::Uuid;

pub fn hash(token: &str) -> String {
    let mut h = Sha256::new();
    h.update(token.as_bytes());
    hex(&h.finalize())
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

pub fn generate() -> String {
    let mut bytes = [0u8; 32];
    rand::rng().fill(&mut bytes);
    format!("kyb_{}", base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes))
}

/// The label the bootstrap token is stored under. A label is only a name: a paired device chooses
/// its own, so nothing may be authorized by it. Pairing refuses it (see [`is_reserved_label`]).
pub const BOOTSTRAP_LABEL: &str = "bootstrap";

/// Whether a device may not pair under this name: the bootstrap token's, in any case.
pub fn is_reserved_label(label: &str) -> bool {
    label.trim().eq_ignore_ascii_case(BOOTSTRAP_LABEL)
}

/// The bootstrap token: its raw value (only for the local desktop app) and its id in the store.
pub struct Bootstrap {
    pub token: String,
    pub id: Uuid,
}

/// Ensure a bootstrap token exists on disk and in the store.
pub fn ensure_bootstrap(store: &Store, path: &std::path::Path) -> Result<Bootstrap> {
    if let Ok(existing) = std::fs::read_to_string(path) {
        let existing = existing.trim().to_string();
        if !existing.is_empty()
            && let Some(rec) = store.token_lookup(&hash(&existing))?
            && !rec.revoked
        {
            return Ok(Bootstrap { token: existing, id: rec.id });
        }
    }
    let token = generate();
    let id = Uuid::now_v7();
    store.token_insert(id, &hash(&token), BOOTSTRAP_LABEL, &Scope::ALL)?;
    write_private(path, &token)?;
    tracing::info!(path = %path.display(), "wrote new bootstrap token");
    Ok(Bootstrap { token, id })
}

fn write_private(path: &std::path::Path, contents: &str) -> Result<()> {
    std::fs::write(path, contents)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    }
    Ok(())
}

/// Who is on the other end of a connection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PrincipalKind {
    /// A paired client (or the bootstrap token): a row in the token store.
    Device,
    /// Another member of the machine network, holding a session grant (design §3.3). The grant is
    /// bound to the machine and to the tailnet address it came from.
    Machine { machine_id: String, origin_ip: std::net::IpAddr },
}

#[derive(Debug, Clone)]
pub struct Principal {
    /// The token's id; for a machine, the grant's id.
    pub token_id: Uuid,
    pub label: String,
    pub scopes: Vec<Scope>,
    pub kind: PrincipalKind,
}

impl Principal {
    pub fn has(&self, scope: Scope) -> bool {
        self.scopes.contains(&scope)
    }
}

/// Whether the principal holds this daemon's bootstrap token, the one only the local desktop app
/// reads from the data directory. Decided by token id, never by label.
pub fn is_bootstrap(state: &crate::state::AppState, principal: &Principal) -> bool {
    principal.kind == PrincipalKind::Device && principal.token_id == state.bootstrap_token_id
}

/// Whether the principal may still use the connection: a device token that is not revoked, or a
/// machine whose grant is live and which is still a member (design §3.3). Checked on every request,
/// so removal and revocation take effect at use time.
pub fn principal_is_active(state: &crate::state::AppState, principal: &Principal) -> bool {
    match &principal.kind {
        PrincipalKind::Device => state.store.token_is_active(principal.token_id).unwrap_or(false),
        PrincipalKind::Machine { machine_id, .. } => {
            state.network.grants.is_active(principal.token_id)
                && state.network.is_enabled()
                && state.network.membership().is_some_and(|membership| membership.is_member(machine_id))
        }
    }
}

/// What the principal may do right now. A device keeps the scopes it was issued; a machine keeps
/// the grant's scopes narrowed by this machine's current policy for it, so pausing a machine or
/// turning a capability off cuts it at once (a paused machine keeps `machine:peer`).
pub fn effective_scopes(state: &crate::state::AppState, principal: &Principal) -> Vec<Scope> {
    match &principal.kind {
        PrincipalKind::Device => principal.scopes.clone(),
        PrincipalKind::Machine { machine_id, .. } => {
            let policy = state.network.policy_scopes(machine_id);
            principal.scopes.iter().copied().filter(|scope| policy.contains(scope)).collect()
        }
    }
}

pub fn authenticate(store: &Store, raw: &str) -> Result<Option<Principal>> {
    let Some(rec) = store.token_lookup(&hash(raw))? else { return Ok(None) };
    if rec.revoked {
        return Ok(None);
    }
    let _ = store.token_touch(rec.id);
    Ok(Some(Principal { token_id: rec.id, label: rec.label, scopes: rec.scopes, kind: PrincipalKind::Device }))
}
