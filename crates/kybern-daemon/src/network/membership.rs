//! A machine's membership: its keys, the operation set in the store, the projection to members,
//! the operations it creates (Genesis, Add, Remove, Set, Merge), the sync functions the peer
//! manager calls over the wire, and Rejoin.
//!
//! Nothing here touches a socket. N3 carries [`Membership::sync_request`], [`Membership::sync_respond`]
//! and [`Membership::apply_ops`] over `peer.ops.sync` and `peer.ops.push`; N4 drives the joining
//! flows with [`Membership::build_add`], [`Membership::join`] and friends.

use std::collections::BTreeSet;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, MutexGuard};

use base64::Engine;
use base64::engine::general_purpose::STANDARD as B64;
use chrono::Utc;
use kybern_protocol::{JoinVia, MachineDescriptor, MachineId, OpEnvelope, OpKind, PeerOpsSyncParams, PeerOpsSyncResult, RemoveReason};
use kybern_store::{NetworkOpRecord, Store};

use super::keys::{self, MachineKeys};
use super::ops::{self, Applied, OpError, OpSet, ParsedOp, Projection, Rejection};

/// What a machine says about itself in the operations it writes. The ids and keys come from the
/// machine's own identity.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SelfInfo {
    pub name: String,
    /// `macos`, `linux` or `windows`.
    pub os: String,
    pub ts_node_id: Option<String>,
    pub ts_name: Option<String>,
    /// The network port when it is not the default (§3.1).
    pub port: Option<u16>,
}

#[derive(Debug, thiserror::Error)]
pub enum MembershipError {
    #[error("this machine is not in a network")]
    NoNetwork,
    #[error("this machine is already in a network")]
    AlreadyInNetwork,
    #[error("this machine is not a current member of its network")]
    NotMember,
    #[error("this machine's key does not match its membership; leave the network and rejoin")]
    KeyMismatch,
    #[error("that machine was removed from the network and cannot be added again; it needs a new key")]
    Tombstoned,
    #[error("that machine is already a member")]
    AlreadyMember,
    #[error("that machine is not in the network")]
    UnknownMachine,
    #[error("that machine was already removed")]
    AlreadyRemoved,
    #[error("this machine was not removed from its network")]
    NotRemoved,
    #[error("the operation set does not make this machine a member")]
    NotInSet,
    #[error("{0}")]
    Invalid(String),
    #[error("the operation was refused: {0}")]
    Rejected(OpError),
    #[error(transparent)]
    Store(#[from] anyhow::Error),
}

type Result<T, E = MembershipError> = std::result::Result<T, E>;

/// What applying a batch of operations did.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct ApplyOutcome {
    /// The operations that were new. Re-gossip these once (§2.3).
    pub accepted: Vec<OpEnvelope>,
    pub duplicates: usize,
    pub rejected: Vec<Rejection>,
    /// Machines that became members through this batch.
    pub added: Vec<MachineId>,
    /// Machines that stopped being members through this batch. Close their sessions and revoke
    /// their grants (§2.5).
    pub removed: Vec<MachineId>,
    /// This machine is among `removed`.
    pub self_removed: bool,
}

impl ApplyOutcome {
    /// The batch changed the set.
    pub fn changed(&self) -> bool {
        !self.accepted.is_empty()
    }

    /// The batch held a fault an honest peer never produces: close the sender's session (§2.3).
    pub fn tampered(&self) -> bool {
        self.rejected.iter().any(|rejection| rejection.reason.is_tamper())
    }
}

/// The new identity a machine has after Rejoin or after leaving a network it was removed from.
#[derive(Debug, Clone)]
pub struct IdentityReset {
    pub old_machine_id: MachineId,
    pub new_machine_id: MachineId,
    pub keys: Arc<MachineKeys>,
}

struct Inner {
    machine_id: MachineId,
    keys: Arc<MachineKeys>,
    set: OpSet,
    projection: Arc<Projection>,
}

/// One machine's view of its network. Cheap to share behind an `Arc`; every method takes `&self`.
pub struct Membership {
    store: Store,
    network_dir: PathBuf,
    inner: Mutex<Inner>,
}

/// `meta` keys that hold what is public about this machine's identity (§2.1).
pub const META_SIGNING_KEY: &str = "network.signing_key";
pub const META_SEALING_KEY: &str = "network.sealing_key";
pub const META_FINGERPRINT: &str = "network.fingerprint";
/// A JSON array of the machine ids this daemon had before a Rejoin, oldest first.
pub const META_PREVIOUS_MACHINE_IDS: &str = "previous_machine_ids";

fn write_identity_meta(store: &Store, keys: &MachineKeys) -> anyhow::Result<()> {
    store.meta_set_many(&[
        (META_SIGNING_KEY, &keys.signing_public_b64()),
        (META_SEALING_KEY, &keys.sealing_public_b64()),
        (META_FINGERPRINT, &keys.fingerprint()),
    ])
}

/// The machine ids this daemon had before it rejoined, oldest first (§2.6).
pub fn previous_machine_ids(store: &Store) -> anyhow::Result<Vec<MachineId>> {
    Ok(store.meta_get(META_PREVIOUS_MACHINE_IDS)?.and_then(|json| serde_json::from_str(&json).ok()).unwrap_or_default())
}

fn record_of(op: &ParsedOp) -> NetworkOpRecord {
    NetworkOpRecord {
        id: op.id().to_string(),
        body: op.envelope.body.clone().into_bytes(),
        signer: op.signer().to_string(),
        signature: B64.decode(&op.envelope.signature).unwrap_or_default(),
        kind: op.kind.clone(),
        lamport: op.lamport,
        received_at: Utc::now(),
    }
}

fn envelope_of(record: &NetworkOpRecord) -> Option<OpEnvelope> {
    Some(OpEnvelope {
        id: record.id.clone(),
        body: String::from_utf8(record.body.clone()).ok()?,
        signer: record.signer.clone(),
        signature: B64.encode(&record.signature),
    })
}

fn parse_batch(envelopes: &[OpEnvelope], rejected: &mut Vec<Rejection>) -> Vec<ParsedOp> {
    envelopes
        .iter()
        .filter_map(|envelope| match ops::parse(envelope) {
            Ok(parsed) => Some(parsed),
            Err(reason) => {
                rejected.push(Rejection { id: envelope.id.clone(), reason });
                None
            }
        })
        .collect()
}

fn member_ids(projection: &Projection) -> BTreeSet<MachineId> {
    projection.members().map(|(id, _)| id.clone()).collect()
}

impl Membership {
    /// Load the operation set from the store. Operations that no longer verify are left out of
    /// memory (and logged), never deleted.
    pub fn open(store: Store, network_dir: PathBuf, machine_id: MachineId, keys: Arc<MachineKeys>) -> anyhow::Result<Self> {
        let mut set = OpSet::default();
        let mut rejected = Vec::new();
        let envelopes: Vec<OpEnvelope> = store.network_ops_list()?.iter().filter_map(envelope_of).collect();
        let parsed = parse_batch(&envelopes, &mut rejected);
        rejected.extend(set.apply(parsed, true).rejected);
        for rejection in &rejected {
            tracing::warn!(target: "kybern::network", id = %rejection.id, reason = %rejection.reason, "a stored membership operation no longer verifies and is ignored");
        }
        let projection = Arc::new(set.project());
        let membership = Self { store, network_dir, inner: Mutex::new(Inner { machine_id, keys, set, projection }) };
        membership.sync_machine_cache(&membership.lock().projection)?;
        Ok(membership)
    }

    fn lock(&self) -> MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    // ---- what this machine is ----

    pub fn machine_id(&self) -> MachineId {
        self.lock().machine_id.clone()
    }

    pub fn keys(&self) -> Arc<MachineKeys> {
        self.lock().keys.clone()
    }

    /// The current members, registers and tombstones. The snapshot does not change; ask again after
    /// an operation is applied.
    pub fn projection(&self) -> Arc<Projection> {
        self.lock().projection.clone()
    }

    pub fn network_id(&self) -> Option<String> {
        self.lock().set.network_id().map(str::to_string)
    }

    pub fn has_network(&self) -> bool {
        self.lock().set.network_id().is_some()
    }

    pub fn is_member(&self, machine_id: &str) -> bool {
        self.lock().projection.is_member(machine_id)
    }

    /// This machine is a current member of its network.
    pub fn self_is_member(&self) -> bool {
        let inner = self.lock();
        inner.projection.is_member(&inner.machine_id)
    }

    /// This machine was removed from its network and may rejoin (§2.6).
    pub fn self_removed(&self) -> bool {
        let inner = self.lock();
        inner.projection.is_removed(&inner.machine_id)
    }

    /// The set names this machine, but with a different signing key than the one on disk: the key
    /// file was lost or replaced. The machine cannot sign until it leaves and rejoins.
    pub fn key_mismatch(&self) -> bool {
        let inner = self.lock();
        key_mismatch(&inner)
    }

    /// This machine's descriptor, with its keys and id.
    pub fn self_descriptor(&self, info: &SelfInfo) -> MachineDescriptor {
        let inner = self.lock();
        descriptor_of(&inner, info)
    }

    /// The genesis of this machine's network, for a merge offer.
    pub fn genesis_envelope(&self) -> Option<OpEnvelope> {
        let inner = self.lock();
        let network = inner.set.network_id()?;
        inner
            .set
            .sorted()
            .into_iter()
            .find(|op| op.network_id == network && matches!(op.op, OpKind::Genesis { .. }))
            .map(|op| op.envelope.clone())
    }

    // ---- the set, for sync ----

    pub fn digest(&self) -> String {
        self.lock().set.digest()
    }

    /// The ids of every operation held, sorted.
    pub fn op_ids(&self) -> Vec<String> {
        self.lock().set.ids()
    }

    /// Every operation, oldest first: the full set an Add offer carries.
    pub fn all_ops(&self) -> Vec<OpEnvelope> {
        self.lock().set.envelopes()
    }

    /// The operations whose ids are not in `known`, oldest first. This is what a peer that sent its
    /// id list is missing.
    pub fn ops_since(&self, known: &[String]) -> Vec<OpEnvelope> {
        self.lock().set.missing_from(known)
    }

    /// The opening of a sync: this machine's digest and ids, to send as `peer.ops.sync` params.
    pub fn sync_request(&self) -> PeerOpsSyncParams {
        let inner = self.lock();
        PeerOpsSyncParams { digest: inner.set.digest(), ids: inner.set.ids() }
    }

    /// Answer a peer's sync request: this machine's digest and ids, and the operations the peer
    /// lacks. A peer holding an old copy gets every Remove it is missing here. The peer then calls
    /// [`Membership::ops_since`] with the returned ids to send back what this machine lacks.
    pub fn sync_respond(&self, theirs: &PeerOpsSyncParams) -> PeerOpsSyncResult {
        let inner = self.lock();
        let digest = inner.set.digest();
        let ids = inner.set.ids();
        let ops = if digest == theirs.digest { Vec::new() } else { inner.set.missing_from(&theirs.ids) };
        PeerOpsSyncResult { digest, ids, ops }
    }

    /// Whether a peer's digest differs from this machine's.
    pub fn differs_from(&self, peer_digest: &str) -> bool {
        self.lock().set.digest() != peer_digest
    }

    /// Take the operations of a `peer.ops.sync` answer or a `peer.ops.push`.
    pub fn apply_ops(&self, envelopes: Vec<OpEnvelope>) -> Result<ApplyOutcome> {
        let mut inner = self.lock();
        self.apply_locked(&mut inner, &envelopes, false)
    }

    /// The operations that prove `machine_id` was removed, for that machine alone ([`OpSet::removal_proof`]).
    /// Empty unless the set says it was removed.
    pub fn removal_proof(&self, machine_id: &str) -> Vec<OpEnvelope> {
        let inner = self.lock();
        if !inner.projection.is_removed(machine_id) {
            return Vec::new();
        }
        inner.set.removal_proof(machine_id)
    }

    /// Take a member's word that this machine was removed (ADE-120): the operations are verified exactly as a
    /// sync verifies them, and kept only when they make this machine removed. Anything else, a forgery from a
    /// machine that is not a member among them, changes nothing and fails with
    /// [`MembershipError::NotRemoved`].
    pub fn apply_removal_proof(&self, envelopes: Vec<OpEnvelope>) -> Result<ApplyOutcome> {
        let mut inner = self.lock();
        if inner.set.network_id().is_none() {
            return Err(MembershipError::NoNetwork);
        }
        let mut ignored = Vec::new();
        let mut candidate = inner.set.clone();
        candidate.apply(parse_batch(&envelopes, &mut ignored), false);
        if !candidate.project().is_removed(&inner.machine_id) {
            return Err(MembershipError::NotRemoved);
        }
        self.apply_locked(&mut inner, &envelopes, false)
    }

    /// Adopt the operation set of a network this machine has been added to (§2.4 step 3). It must
    /// be a lone machine, and the set must make it a member with the key it holds; otherwise
    /// nothing is kept.
    pub fn join(&self, envelopes: Vec<OpEnvelope>) -> Result<ApplyOutcome> {
        let mut inner = self.lock();
        if inner.set.network_id().is_some() {
            return Err(MembershipError::AlreadyInNetwork);
        }
        let mut rejected = Vec::new();
        let parsed = parse_batch(&envelopes, &mut rejected);
        let mut candidate = OpSet::default();
        candidate.apply(parsed, true);
        let projection = candidate.project();
        let me = &inner.machine_id;
        let listed = projection.is_member(me) && candidate.signer_key(me) == Some(inner.keys.signing_public_b64().as_str());
        if !listed {
            return Err(MembershipError::NotInSet);
        }
        self.apply_locked(&mut inner, &envelopes, true)
    }

    fn apply_locked(&self, inner: &mut Inner, envelopes: &[OpEnvelope], bootstrap: bool) -> Result<ApplyOutcome> {
        let mut parse_rejections = Vec::new();
        let parsed = parse_batch(envelopes, &mut parse_rejections);
        let before = member_ids(&inner.projection);
        let mut set = inner.set.clone();
        let Applied { accepted, duplicates, rejected } = set.apply(parsed, bootstrap);

        let mut kept = Vec::with_capacity(accepted.len());
        for id in &accepted {
            if let Some(op) = set.get(id) {
                self.store.network_op_insert(&record_of(op))?;
                kept.push(op.envelope.clone());
            }
        }
        let mut all_rejected = parse_rejections;
        all_rejected.extend(rejected);
        for rejection in &all_rejected {
            tracing::warn!(target: "kybern::network", id = %rejection.id, reason = %rejection.reason, "dropped a membership operation");
        }

        let projection = Arc::new(set.project());
        let after = member_ids(&projection);
        let outcome = ApplyOutcome {
            added: after.difference(&before).cloned().collect(),
            removed: before.difference(&after).cloned().collect(),
            self_removed: projection.is_removed(&inner.machine_id) && !inner.projection.is_removed(&inner.machine_id),
            accepted: kept,
            duplicates,
            rejected: all_rejected,
        };
        if outcome.changed() {
            self.sync_machine_cache(&projection)?;
            inner.set = set;
            inner.projection = projection;
        }
        Ok(outcome)
    }

    /// The projection to the `network_machines` table: one row per current member, with the
    /// descriptor as the set describes it. Presence and last-seen on a row are kept.
    fn sync_machine_cache(&self, projection: &Projection) -> anyhow::Result<()> {
        let mut keep: BTreeSet<&str> = BTreeSet::new();
        for (id, entry) in projection.members() {
            self.store.network_machine_upsert(&entry.descriptor)?;
            keep.insert(id.as_str());
        }
        for cached in self.store.network_machines_list()? {
            if !keep.contains(cached.machine_id.as_str()) {
                self.store.network_machine_delete(&cached.machine_id)?;
            }
        }
        Ok(())
    }

    // ---- creating operations ----
    //
    // `build_*` signs an operation and returns it without keeping it: an offer carries an Add that
    // the other machine may still refuse (§2.4). The plain names build, keep and return it, for the
    // caller to gossip.

    fn require_member(&self, inner: &Inner) -> Result<()> {
        if inner.set.network_id().is_none() {
            return Err(MembershipError::NoNetwork);
        }
        if key_mismatch(inner) {
            return Err(MembershipError::KeyMismatch);
        }
        if !inner.projection.is_member(&inner.machine_id) {
            return Err(MembershipError::NotMember);
        }
        Ok(())
    }

    /// Sign `kind` as this machine, with the next lamport value, and check that this machine's own
    /// set would keep it.
    fn build(&self, inner: &Inner, network_id: &str, kind: OpKind, bootstrap: bool) -> Result<OpEnvelope> {
        let lamport = inner.set.max_lamport() + 1;
        let body = ops::encode_body(network_id, Utc::now(), lamport, &kind).map_err(|error| MembershipError::Invalid(error.to_string()))?;
        let envelope = ops::seal(&inner.keys, &inner.machine_id, body);
        let parsed = ops::parse(&envelope).map_err(MembershipError::Rejected)?;
        let applied = inner.set.clone().apply(vec![parsed], bootstrap);
        match applied.rejected.into_iter().next() {
            Some(rejection) => Err(MembershipError::Rejected(rejection.reason)),
            None => Ok(envelope),
        }
    }

    fn keep_own(&self, inner: &mut Inner, envelope: &OpEnvelope, bootstrap: bool) -> Result<OpEnvelope> {
        let outcome = self.apply_locked(inner, std::slice::from_ref(envelope), bootstrap)?;
        match outcome.rejected.into_iter().next() {
            Some(rejection) => Err(MembershipError::Rejected(rejection.reason)),
            None => Ok(envelope.clone()),
        }
    }

    /// Found a network with this machine as its first member. The owner is the Tailscale identity
    /// of the founding node (§2.2). A lone machine only; the one with the smaller machine id of two
    /// same-owner machines that meet founds it (§2.4), which the caller decides.
    pub fn build_genesis(&self, owner_login: &str, tailnet: &str, info: &SelfInfo) -> Result<OpEnvelope> {
        let inner = self.lock();
        self.genesis_locked(&inner, owner_login, tailnet, info)
    }

    fn genesis_locked(&self, inner: &Inner, owner_login: &str, tailnet: &str, info: &SelfInfo) -> Result<OpEnvelope> {
        if inner.set.network_id().is_some() {
            return Err(MembershipError::AlreadyInNetwork);
        }
        if owner_login.trim().is_empty() {
            return Err(MembershipError::Invalid("a network needs an owner login".into()));
        }
        let network_id = uuid::Uuid::now_v7().to_string();
        let kind =
            OpKind::Genesis { owner_login: owner_login.to_string(), tailnet: tailnet.to_string(), machine: descriptor_of(inner, info) };
        self.build(inner, &network_id, kind, true)
    }

    /// [`Membership::build_genesis`], kept.
    pub fn genesis(&self, owner_login: &str, tailnet: &str, info: &SelfInfo) -> Result<OpEnvelope> {
        let mut inner = self.lock();
        let envelope = self.genesis_locked(&inner, owner_login, tailnet, info)?;
        self.keep_own(&mut inner, &envelope, true)
    }

    fn add_locked(&self, inner: &Inner, machine: MachineDescriptor, via: JoinVia) -> Result<OpEnvelope> {
        self.require_member(inner)?;
        let projection = &inner.projection;
        if projection.is_removed(&machine.machine_id) || projection.is_key_tombstoned(&machine.signing_key) {
            return Err(MembershipError::Tombstoned);
        }
        if let Some(existing) = projection.entry(&machine.machine_id) {
            if existing.descriptor.signing_key != machine.signing_key {
                return Err(MembershipError::Rejected(OpError::KeyConflict));
            }
            return Err(MembershipError::AlreadyMember);
        }
        let network_id = inner.set.network_id().map(str::to_string).ok_or(MembershipError::NoNetwork)?;
        self.build(inner, &network_id, OpKind::Add { machine, via }, false)
    }

    /// Sign an Add for `machine`, without keeping it.
    pub fn build_add(&self, machine: MachineDescriptor, via: JoinVia) -> Result<OpEnvelope> {
        let inner = self.lock();
        self.add_locked(&inner, machine, via)
    }

    /// [`Membership::build_add`], kept.
    pub fn add(&self, machine: MachineDescriptor, via: JoinVia) -> Result<OpEnvelope> {
        let mut inner = self.lock();
        let envelope = self.add_locked(&inner, machine, via)?;
        self.keep_own(&mut inner, &envelope, false)
    }

    fn remove_locked(&self, inner: &Inner, machine_id: &str, reason: RemoveReason) -> Result<OpEnvelope> {
        self.require_member(inner)?;
        let entry = inner.projection.entry(machine_id).ok_or(MembershipError::UnknownMachine)?;
        if entry.removed.is_some() {
            return Err(MembershipError::AlreadyRemoved);
        }
        let kind = OpKind::Remove { machine_id: machine_id.to_string(), ts_node_id: entry.descriptor.ts_node_id.clone(), reason };
        let network_id = inner.set.network_id().map(str::to_string).ok_or(MembershipError::NoNetwork)?;
        self.build(inner, &network_id, kind, false)
    }

    /// Sign a Remove for `machine_id` (§2.5), without keeping it. A machine may remove itself.
    pub fn build_remove(&self, machine_id: &str, reason: RemoveReason) -> Result<OpEnvelope> {
        let inner = self.lock();
        self.remove_locked(&inner, machine_id, reason)
    }

    /// [`Membership::build_remove`], kept. Gossip the result right away.
    pub fn remove(&self, machine_id: &str, reason: RemoveReason) -> Result<OpEnvelope> {
        let mut inner = self.lock();
        let envelope = self.remove_locked(&inner, machine_id, reason)?;
        self.keep_own(&mut inner, &envelope, false)
    }

    fn set_locked(&self, inner: &Inner, key: &str, value: serde_json::Value) -> Result<OpEnvelope> {
        self.require_member(inner)?;
        if key.trim().is_empty() {
            return Err(MembershipError::Invalid("a register needs a key".into()));
        }
        if !ops::register_is_valid(key, &inner.machine_id) {
            return Err(MembershipError::Invalid("an allow register can only be set by the machine it describes".into()));
        }
        let network_id = inner.set.network_id().map(str::to_string).ok_or(MembershipError::NoNetwork)?;
        self.build(inner, &network_id, OpKind::Set { key: key.to_string(), value }, false)
    }

    /// Sign a write to a register (`machine.<id>.name`, `machine.<id>.paused`, `broker`,
    /// `follow_updates`, `allow.<own id>.<capability>`), without keeping it.
    pub fn build_set(&self, key: &str, value: serde_json::Value) -> Result<OpEnvelope> {
        let inner = self.lock();
        self.set_locked(&inner, key, value)
    }

    /// [`Membership::build_set`], kept.
    pub fn set(&self, key: &str, value: serde_json::Value) -> Result<OpEnvelope> {
        let mut inner = self.lock();
        let envelope = self.set_locked(&inner, key, value)?;
        self.keep_own(&mut inner, &envelope, false)
    }

    fn merge_locked(&self, inner: &Inner, other_genesis: OpEnvelope) -> Result<OpEnvelope> {
        self.require_member(inner)?;
        let other = ops::parse(&other_genesis).map_err(MembershipError::Rejected)?;
        if !matches!(other.op, OpKind::Genesis { .. }) {
            return Err(MembershipError::Invalid("a merge carries the other network's genesis".into()));
        }
        let network_id = inner.set.network_id().map(str::to_string).ok_or(MembershipError::NoNetwork)?;
        if inner.set.has_network(&other.network_id) || inner.set.get(other.id()).is_some() {
            return Err(MembershipError::Invalid("that network is already this one".into()));
        }
        let kind = OpKind::Merge { other_network_id: other.network_id.clone(), other_genesis };
        self.build(inner, &network_id, kind, false)
    }

    /// Sign a merge with another network of the same owner (§2.4), without keeping it. It names the
    /// other network's genesis; the other network's own member signs the mirror image.
    pub fn build_merge(&self, other_genesis: OpEnvelope) -> Result<OpEnvelope> {
        let inner = self.lock();
        self.merge_locked(&inner, other_genesis)
    }

    /// [`Membership::build_merge`], kept.
    pub fn merge(&self, other_genesis: OpEnvelope) -> Result<OpEnvelope> {
        let mut inner = self.lock();
        let envelope = self.merge_locked(&inner, other_genesis)?;
        self.keep_own(&mut inner, &envelope, false)
    }

    // ---- Rejoin ----

    /// Rejoin (§2.6): a machine that was removed makes a new key and a new machine id, forgets the
    /// network it was removed from, and becomes a lone machine that another member can add. Its
    /// projects and threads keep their ids; the old machine id is kept in `previous_machine_ids` so
    /// clients can re-attach cached data.
    ///
    /// The daemon's cached `environment_id` is read when it starts, so the caller must restart or
    /// refresh it: `meta.environment_id` already holds the new id.
    pub fn rejoin(&self) -> Result<IdentityReset> {
        let mut inner = self.lock();
        if !inner.projection.is_removed(&inner.machine_id) {
            return Err(MembershipError::NotRemoved);
        }
        self.reset_locked(&mut inner)
    }

    /// Leave the network and start over, whether or not this machine was removed: what a
    /// cooperating machine does when it learns it was removed (§2.5), and the way out of a key
    /// mismatch. Same effects as [`Membership::rejoin`].
    pub fn leave_and_reset(&self) -> Result<IdentityReset> {
        let mut inner = self.lock();
        self.reset_locked(&mut inner)
    }

    fn reset_locked(&self, inner: &mut Inner) -> Result<IdentityReset> {
        // The new key is written first, atomically over the old one, so a crash never leaves a
        // machine with no key. A crash before the store changes leaves a key mismatch, which
        // `leave_and_reset` clears.
        let keys = Arc::new(keys::create(&self.network_dir)?);
        let old_machine_id = inner.machine_id.clone();
        let new_machine_id = uuid::Uuid::now_v7().to_string();

        let mut previous = previous_machine_ids(&self.store)?;
        previous.push(old_machine_id.clone());
        let previous = serde_json::to_string(&previous).map_err(anyhow::Error::from)?;
        self.store.meta_set_many(&[
            ("environment_id", &new_machine_id),
            (META_PREVIOUS_MACHINE_IDS, &previous),
            (META_SIGNING_KEY, &keys.signing_public_b64()),
            (META_SEALING_KEY, &keys.sealing_public_b64()),
            (META_FINGERPRINT, &keys.fingerprint()),
        ])?;
        self.store.network_clear()?;
        self.store.space_replica_clear()?;

        inner.machine_id = new_machine_id.clone();
        inner.keys = keys.clone();
        inner.set = OpSet::default();
        inner.projection = Arc::new(inner.set.project());
        Ok(IdentityReset { old_machine_id, new_machine_id, keys })
    }
}

fn descriptor_of(inner: &Inner, info: &SelfInfo) -> MachineDescriptor {
    MachineDescriptor {
        machine_id: inner.machine_id.clone(),
        name: info.name.clone(),
        os: info.os.clone(),
        signing_key: inner.keys.signing_public_b64(),
        sealing_key: inner.keys.sealing_public_b64(),
        ts_node_id: info.ts_node_id.clone(),
        ts_name: info.ts_name.clone(),
        port: info.port,
    }
}

fn key_mismatch(inner: &Inner) -> bool {
    inner.set.signer_key(&inner.machine_id).is_some_and(|recorded| recorded != inner.keys.signing_public_b64())
}

/// Write this machine's public keys to `meta` and open its membership: load or make the keys under
/// `<data dir>/network/`, record their public halves and fingerprint (§2.1), and load the set.
///
/// Call it once at startup with the daemon's `environment_id` as the machine id. It makes the keys
/// the first time it runs. A key file that is damaged is an error, not a new identity.
pub fn start(data_dir: &std::path::Path, store: &Store, machine_id: &str) -> anyhow::Result<Membership> {
    let network_dir = data_dir.join(keys::NETWORK_DIR);
    let keys = Arc::new(keys::load_or_create(&network_dir)?);
    write_identity_meta(store, &keys)?;
    let membership = Membership::open(store.clone(), network_dir, machine_id.to_string(), keys)?;
    if membership.key_mismatch() {
        tracing::warn!(
            target: "kybern::network",
            "machine.key does not match this machine's recorded signing key; the network is read-only until the machine leaves and rejoins"
        );
    }
    Ok(membership)
}

#[cfg(test)]
mod tests {
    use serde_json::{Value, json};

    use super::super::testutil::TempDir;
    use super::*;

    const OWNER: &str = "owner@example.com";

    /// One machine: its own data directory, store and keys.
    struct Node {
        dir: TempDir,
        store: Store,
        membership: Membership,
        name: String,
    }

    impl Node {
        fn new(name: &str) -> Self {
            let dir = TempDir::new(name);
            let store = Store::open_in_memory().unwrap();
            let machine_id = uuid::Uuid::now_v7().to_string();
            store.meta_set("environment_id", &machine_id).unwrap();
            let network_dir = dir.path().join(keys::NETWORK_DIR);
            let keys = Arc::new(keys::create(&network_dir).unwrap());
            let membership = Membership::open(store.clone(), network_dir, machine_id, keys).unwrap();
            Self { dir, store, membership, name: name.into() }
        }

        fn founder(name: &str, owner: &str) -> Self {
            let node = Self::new(name);
            node.membership.genesis(owner, "tail.ts.net", &node.info()).unwrap();
            node
        }

        fn info(&self) -> SelfInfo {
            SelfInfo {
                name: self.name.clone(),
                os: "linux".into(),
                ts_node_id: Some(format!("node-{}", self.name)),
                ts_name: None,
                port: None,
            }
        }

        fn id(&self) -> String {
            self.membership.machine_id()
        }

        fn descriptor(&self) -> MachineDescriptor {
            self.membership.self_descriptor(&self.info())
        }

        /// Add `joiner` through `self`, and hand the joiner the set.
        fn enroll(&self, joiner: &Node, via: JoinVia) {
            self.membership.add(joiner.descriptor(), via).unwrap();
            joiner.membership.join(self.membership.all_ops()).unwrap();
        }

        fn member_names(&self) -> Vec<String> {
            let mut names: Vec<String> = self.membership.projection().members().map(|(_, entry)| entry.descriptor.name.clone()).collect();
            names.sort();
            names
        }
    }

    /// A full two-way sync, the way `peer.ops.sync` runs it.
    fn sync(a: &Node, b: &Node) {
        let request = a.membership.sync_request();
        let answer = b.membership.sync_respond(&request);
        a.membership.apply_ops(answer.ops).unwrap();
        b.membership.apply_ops(a.membership.ops_since(&answer.ids)).unwrap();
    }

    fn converged(a: &Node, b: &Node) {
        assert_eq!(a.membership.digest(), b.membership.digest(), "the sets are the same");
        assert_eq!(*a.membership.projection(), *b.membership.projection(), "and so is what they imply");
    }

    /// Sign an arbitrary body, as a hand-made (possibly hostile) peer would.
    fn forge(node: &Node, body: Value) -> OpEnvelope {
        ops::seal(&node.membership.keys(), &node.id(), body.to_string())
    }

    fn body(node: &Node, kind: &str, lamport: u64, fields: Value) -> Value {
        let mut value = json!({"v": 1, "network_id": node.membership.network_id().unwrap(), "kind": kind, "at": Utc::now().to_rfc3339(), "lamport": lamport});
        value.as_object_mut().unwrap().extend(fields.as_object().unwrap().clone());
        value
    }

    fn flip_signature(envelope: &OpEnvelope) -> OpEnvelope {
        let mut signature = B64.decode(&envelope.signature).unwrap();
        signature[10] ^= 1;
        OpEnvelope { signature: B64.encode(signature), ..envelope.clone() }
    }

    #[test]
    fn genesis_founds_a_network_with_one_member_and_fills_the_machine_table() {
        let a = Node::new("mac");
        assert!(!a.membership.has_network());
        assert!(!a.membership.self_is_member());
        let genesis = a.membership.genesis(OWNER, "tail.ts.net", &a.info()).unwrap();

        let projection = a.membership.projection();
        assert_eq!(a.membership.network_id(), projection.network_id);
        assert_eq!(projection.owner_login.as_deref(), Some(OWNER));
        assert_eq!(projection.tailnet.as_deref(), Some("tail.ts.net"));
        assert!(a.membership.self_is_member() && !a.membership.self_removed());
        let card = projection.card(&a.id(), &a.id()).unwrap();
        assert_eq!((card.name.as_str(), card.is_self, card.added_by), ("mac", true, None));
        assert_eq!(a.membership.op_ids(), vec![genesis.id.clone()]);
        assert_eq!(a.membership.genesis_envelope().unwrap(), genesis);
        // The projection is in the machines table, and the op is in the ops table, byte for byte.
        assert_eq!(a.store.network_machines_list().unwrap().len(), 1);
        let stored = a.store.network_op_get(&genesis.id).unwrap().unwrap();
        assert_eq!((stored.body, stored.kind.as_str(), stored.lamport), (genesis.body.clone().into_bytes(), "genesis", 1));
        // A second genesis is refused, and so is a network without an owner.
        assert!(matches!(a.membership.genesis(OWNER, "t", &a.info()), Err(MembershipError::AlreadyInNetwork)));
        let lone = Node::new("lone");
        assert!(matches!(lone.membership.genesis(" ", "t", &lone.info()), Err(MembershipError::Invalid(_))));
        assert!(matches!(lone.membership.add(a.descriptor(), JoinVia::Auto), Err(MembershipError::NoNetwork)));
    }

    #[test]
    fn an_added_machine_joins_with_the_set_and_both_agree() {
        let a = Node::founder("mac", OWNER);
        let b = Node::new("pc");
        a.enroll(&b, JoinVia::Auto);
        assert_eq!(a.member_names(), vec!["mac", "pc"]);
        converged(&a, &b);
        assert!(b.membership.self_is_member());
        let card = b.membership.projection().card(&b.id(), &b.id()).unwrap();
        assert_eq!((card.added_by, card.added_via), (Some(a.id()), Some(JoinVia::Auto)));
        assert_eq!(b.store.network_machines_list().unwrap().len(), 2);
        // Joining is for lone machines only, and only with a set that lists the joiner.
        assert!(matches!(b.membership.join(a.membership.all_ops()), Err(MembershipError::AlreadyInNetwork)));
        let stranger = Node::new("stranger");
        assert!(matches!(stranger.membership.join(a.membership.all_ops()), Err(MembershipError::NotInSet)));
        assert!(!stranger.membership.has_network(), "nothing was kept");
        assert!(stranger.store.network_ops_list().unwrap().is_empty());
        // A key the set does not know for this machine id is not this machine.
        let impostor = Node::new("impostor");
        let twin =
            Membership::open(impostor.store.clone(), impostor.dir.path().join("twin"), b.id(), Arc::new(MachineKeys::generate())).unwrap();
        assert!(matches!(twin.join(a.membership.all_ops()), Err(MembershipError::NotInSet)));
    }

    #[test]
    fn a_bad_signature_is_refused_and_closes_the_session() {
        let a = Node::founder("mac", OWNER);
        let b = Node::new("pc");
        a.enroll(&b, JoinVia::Auto);
        let c = Node::new("vps");
        let add_c = a.membership.build_add(c.descriptor(), JoinVia::Found).unwrap();
        let before = b.membership.digest();

        let flipped = flip_signature(&add_c);
        let outcome = b.membership.apply_ops(vec![flipped.clone()]).unwrap();
        assert_eq!(outcome.rejected, vec![Rejection { id: flipped.id.clone(), reason: OpError::BadSignature }]);
        assert!(outcome.tampered() && !outcome.changed());

        // Signed by the wrong machine's key while naming a real member as the signer.
        let wrong_key = ops::seal(&c.membership.keys(), &a.id(), add_c.body.clone());
        assert_eq!(b.membership.apply_ops(vec![wrong_key]).unwrap().rejected[0].reason, OpError::BadSignature);
        // A body changed after signing: the id no longer matches.
        let edited = OpEnvelope { body: add_c.body.replace("vps", "evil"), ..add_c.clone() };
        assert_eq!(b.membership.apply_ops(vec![edited.clone()]).unwrap().rejected[0].reason, OpError::IdMismatch);
        // The id was recomputed but the signature was not.
        let rehashed = OpEnvelope { id: ops::op_id(&edited.body), ..edited };
        assert_eq!(b.membership.apply_ops(vec![rehashed]).unwrap().rejected[0].reason, OpError::BadSignature);
        // Not base64 at all.
        let garbled = OpEnvelope { signature: "***".into(), ..add_c.clone() };
        assert_eq!(b.membership.apply_ops(vec![garbled]).unwrap().rejected[0].reason, OpError::BadSignature);

        assert_eq!(b.membership.digest(), before, "none of them was kept");
        assert!(!b.membership.is_member(&c.id()));
        // The untouched op is fine.
        assert!(b.membership.apply_ops(vec![add_c]).unwrap().changed());
        assert!(b.membership.is_member(&c.id()));
    }

    #[test]
    fn a_signer_that_was_never_added_is_refused() {
        let a = Node::founder("mac", OWNER);
        let b = Node::new("pc");
        a.enroll(&b, JoinVia::Auto);
        let outsider = Node::new("outsider");
        let network = a.membership.network_id().unwrap();
        // Right network id, valid signature, but the signer has no Add.
        let body =
            json!({"v":1,"network_id":network,"kind":"set","at":Utc::now().to_rfc3339(),"lamport":9,"key":"broker","value":outsider.id()});
        let op = forge(&outsider, body);
        let outcome = b.membership.apply_ops(vec![op.clone()]).unwrap();
        assert_eq!(outcome.rejected, vec![Rejection { id: op.id, reason: OpError::NotMember }]);
        assert!(outcome.tampered());
        assert_eq!(b.membership.projection().broker(), None);
        // It cannot add itself, either.
        let self_add = forge(&outsider, body_for_add(&a, &outsider, 9));
        assert_eq!(b.membership.apply_ops(vec![self_add]).unwrap().rejected[0].reason, OpError::NotMember);
        // A lone machine keeps nothing a peer sends it.
        let lone = Node::new("lone");
        let from_a = a.membership.all_ops();
        let refused = lone.membership.apply_ops(from_a).unwrap();
        assert!(!refused.changed() && refused.rejected.iter().all(|r| r.reason == OpError::NoNetwork));
    }

    fn body_for_add(admin: &Node, joiner: &Node, lamport: u64) -> Value {
        body(admin, "add", lamport, json!({"machine": joiner.descriptor(), "via": "auto"}))
    }

    #[test]
    fn operations_may_arrive_before_the_add_that_makes_their_signer_valid() {
        let a = Node::founder("mac", OWNER);
        let b = Node::new("pc");
        a.enroll(&b, JoinVia::Auto);
        let fresh = Node::new("fresh");
        let add_fresh = a.membership.add(fresh.descriptor(), JoinVia::Found).unwrap();
        fresh.membership.join(a.membership.all_ops()).unwrap();
        let by_fresh = fresh.membership.set("broker", json!(fresh.id())).unwrap();

        // `b` has seen neither the Add nor what `fresh` signed. Delivered newest first.
        let outcome = b.membership.apply_ops(vec![by_fresh.clone(), add_fresh.clone()]).unwrap();
        assert_eq!(outcome.accepted.len(), 2, "{:?}", outcome.rejected);
        assert_eq!(b.membership.projection().broker(), Some(fresh.id().as_str()));
        sync(&a, &b);
        converged(&a, &b);
        // Repeats are counted, not kept twice.
        let again = b.membership.apply_ops(vec![by_fresh, add_fresh]).unwrap();
        assert_eq!((again.duplicates, again.changed()), (2, false));
    }

    #[test]
    fn removal_wins_and_is_permanent() {
        let a = Node::founder("mac", OWNER);
        let b = Node::new("pc");
        let c = Node::new("vps");
        a.enroll(&b, JoinVia::Auto);
        a.enroll(&c, JoinVia::Found);
        sync(&a, &b);
        sync(&a, &c);

        let removal = a.membership.remove(&b.id(), RemoveReason::User).unwrap();
        assert!(!a.membership.is_member(&b.id()) && a.membership.projection().is_removed(&b.id()));
        // It reaches everyone, and tombstones the Tailscale node too.
        let outcome = c.membership.apply_ops(vec![removal]).unwrap();
        assert_eq!((outcome.removed, outcome.self_removed), (vec![b.id()], false));
        assert!(c.membership.projection().is_node_tombstoned("node-pc"));
        assert_eq!(c.member_names(), vec!["mac", "vps"]);
        sync(&a, &b);
        assert!(b.membership.self_removed(), "the removed machine learns it was removed");
        assert!(matches!(b.membership.set("broker", json!("x")), Err(MembershipError::NotMember)));

        // Nobody can add that machine back, by id or by key.
        assert!(matches!(a.membership.add(b.descriptor(), JoinVia::Found), Err(MembershipError::Tombstoned)));
        let relabelled = MachineDescriptor { machine_id: "someone-else".into(), ..b.descriptor() };
        assert!(matches!(a.membership.add(relabelled.clone(), JoinVia::Found), Err(MembershipError::Tombstoned)));
        // A hand-made Add is kept as data, as a Remove is never undone by anything that arrives later.
        let sneaky = forge(&a, body_for_add_descriptor(&a, &b.descriptor(), 20));
        assert!(c.membership.apply_ops(vec![sneaky]).unwrap().changed());
        let sneaky_key = forge(&a, body_for_add_descriptor(&a, &relabelled, 21));
        c.membership.apply_ops(vec![sneaky_key]).unwrap();
        assert!(!c.membership.is_member(&b.id()), "removal wins");
        assert!(!c.membership.is_member("someone-else"), "the key is tombstoned, not just the id");
        assert_eq!(c.member_names(), vec!["mac", "vps"]);

        // Order does not matter: a node that gets the Remove before the Add it removes ends the same.
        let mut all = a.membership.all_ops();
        all.reverse();
        let replay = Node::new("replay");
        replay.membership.apply_in_any_order(all);
        assert!(!replay.membership.is_member(&b.id()));
        assert_eq!(replay.member_names(), vec!["mac", "vps"]);

        // Remove is for known, not yet removed machines.
        assert!(matches!(a.membership.remove(&b.id(), RemoveReason::User), Err(MembershipError::AlreadyRemoved)));
        assert!(matches!(a.membership.remove("nobody", RemoveReason::User), Err(MembershipError::UnknownMachine)));
        // The machine table holds members only.
        let cached: Vec<String> = a.store.network_machines_list().unwrap().into_iter().map(|m| m.machine_id).collect();
        assert!(!cached.contains(&b.id()));
    }

    fn body_for_add_descriptor(admin: &Node, machine: &MachineDescriptor, lamport: u64) -> Value {
        body(admin, "add", lamport, json!({"machine": machine, "via": "auto"}))
    }

    #[test]
    fn a_stale_peer_cannot_roll_back_a_removal_and_gets_it_on_the_next_sync() {
        let a = Node::founder("mac", OWNER);
        let b = Node::new("pc");
        let c = Node::new("vps");
        a.enroll(&b, JoinVia::Auto);
        a.enroll(&c, JoinVia::Auto);
        sync(&a, &c);
        // The set as `c` holds it, from before the removal.
        let old_ops = c.membership.all_ops();
        a.membership.remove(&b.id(), RemoveReason::Stolen).unwrap();

        // The stale peer offers its old copy. Nothing changes: every op in it is already held.
        let held = a.membership.digest();
        let offer = a.membership.apply_ops(old_ops.clone()).unwrap();
        assert_eq!((offer.changed(), offer.duplicates, offer.rejected.len()), (false, old_ops.len(), 0));
        assert_eq!(a.membership.digest(), held);
        assert!(a.membership.projection().is_removed(&b.id()));

        // And its digest and ids bring the missing Remove back to it.
        let stale_request = c.membership.sync_request();
        let answer = a.membership.sync_respond(&stale_request);
        assert_ne!(answer.digest, stale_request.digest);
        assert_eq!(answer.ops.len(), 1);
        assert!(answer.ops[0].body.contains("\"remove\""));
        let applied = c.membership.apply_ops(answer.ops).unwrap();
        assert_eq!(applied.removed, vec![b.id()]);
        converged(&a, &c);
        // Same digest means nothing to send.
        let up_to_date = a.membership.sync_respond(&c.membership.sync_request());
        assert!(up_to_date.ops.is_empty() && !a.membership.differs_from(&up_to_date.digest));
    }

    #[test]
    fn an_unknown_kind_is_kept_verified_and_gossiped_but_does_nothing() {
        let a = Node::founder("mac", OWNER);
        let b = Node::new("pc");
        let c = Node::new("vps");
        a.enroll(&b, JoinVia::Auto);
        a.enroll(&c, JoinVia::Auto);
        sync(&a, &b);
        sync(&a, &c);
        let projection_before = (*b.membership.projection()).clone();

        let future = forge(&a, body(&a, "frobnicate", 50, json!({"payload": {"x": [1, 2, 3]}, "machine": 7})));
        let outcome = b.membership.apply_ops(vec![future.clone()]).unwrap();
        assert_eq!(outcome.accepted, vec![future.clone()]);
        assert_eq!(*b.membership.projection(), projection_before, "an unknown kind changes nothing");
        assert_eq!(b.store.network_op_get(&future.id).unwrap().unwrap().kind, "frobnicate");
        // Gossiped on: `c` gets it from `b`, byte for byte.
        sync(&b, &c);
        assert!(c.membership.all_ops().contains(&future));
        converged(&b, &c);
        // A forged one is still verified: same shape, wrong signature.
        let forged = flip_signature(&forge(&a, body(&a, "frobnicate", 51, json!({}))));
        assert_eq!(c.membership.apply_ops(vec![forged]).unwrap().rejected[0].reason, OpError::BadSignature);
        // A known kind with broken fields is not an unknown kind.
        let broken = forge(&a, body(&a, "add", 52, json!({"machine": {"machine_id": "x"}, "via": "auto"})));
        assert!(matches!(c.membership.apply_ops(vec![broken]).unwrap().rejected[0].reason, OpError::Malformed(_)));
        // The lamport of an unknown op counts: the next op created is later than it.
        let next = b.membership.set("broker", json!(b.id())).unwrap();
        assert_eq!(ops::parse(&next).unwrap().lamport, 51);
    }

    #[test]
    fn concurrent_registers_converge_on_the_highest_lamport_then_signer() {
        let a = Node::founder("mac", OWNER);
        let b = Node::new("pc");
        a.enroll(&b, JoinVia::Auto);
        sync(&a, &b);

        // Both write `broker` without seeing the other: the same lamport.
        let by_a = a.membership.set("broker", json!("mac-id")).unwrap();
        let by_b = b.membership.set("broker", json!("pc-id")).unwrap();
        assert_eq!(ops::parse(&by_a).unwrap().lamport, ops::parse(&by_b).unwrap().lamport);
        assert_ne!(a.membership.projection().broker(), b.membership.projection().broker());
        sync(&a, &b);
        converged(&a, &b);
        let winner_signer = a.id().max(b.id());
        let register = a.membership.projection().register("broker").unwrap().clone();
        assert_eq!(register.signer, winner_signer, "ties go to the higher signer");

        // A later write wins over both, wherever it is made.
        b.membership.set("broker", json!("vps-id")).unwrap();
        sync(&a, &b);
        converged(&a, &b);
        assert_eq!(a.membership.projection().broker(), Some("vps-id"));
        // And the order the operations arrive in makes no difference.
        let mut forward = a.membership.all_ops();
        let replica_registers = |ops: Vec<OpEnvelope>| {
            let node = Node::new("replica");
            node.membership.apply_in_any_order(ops);
            node.membership.projection().registers.clone()
        };
        let one = replica_registers(forward.clone());
        forward.reverse();
        assert_eq!(one, replica_registers(forward));
    }

    impl Membership {
        /// Adopt a whole set at once, in the order given.
        fn apply_in_any_order(&self, ops: Vec<OpEnvelope>) {
            let mut inner = self.lock();
            self.apply_locked(&mut inner, &ops, true).unwrap();
        }
    }

    #[test]
    fn name_and_pause_registers_show_in_the_projection_and_the_machine_table() {
        let a = Node::founder("mac", OWNER);
        let b = Node::new("pc");
        a.enroll(&b, JoinVia::Auto);
        a.membership.set(&format!("machine.{}.name", b.id()), json!("satyamas")).unwrap();
        a.membership.set(&format!("machine.{}.paused", b.id()), json!(true)).unwrap();
        a.membership.set("follow_updates", json!(true)).unwrap();
        let projection = a.membership.projection();
        let card = projection.card(&b.id(), &a.id()).unwrap();
        assert_eq!((card.name.as_str(), card.status), ("satyamas", kybern_protocol::MachineStatus::Paused));
        assert!(projection.follow_updates());
        let cached = a.store.network_machine_get(&b.id()).unwrap().unwrap();
        assert_eq!(cached.descriptor.name, "satyamas");
        a.membership.set(&format!("machine.{}.paused", b.id()), json!(false)).unwrap();
        assert_ne!(a.membership.projection().card(&b.id(), &a.id()).unwrap().status, kybern_protocol::MachineStatus::Paused);
    }

    #[test]
    fn allow_registers_count_only_when_the_described_machine_signed_them() {
        let a = Node::founder("mac", OWNER);
        let b = Node::new("pc");
        a.enroll(&b, JoinVia::Auto);
        sync(&a, &b);
        // A machine sets its own policy.
        b.membership.set(&format!("allow.{}.files", b.id()), json!(false)).unwrap();
        assert!(matches!(a.membership.set(&format!("allow.{}.files", b.id()), json!(true)), Err(MembershipError::Invalid(_))));
        // A forged one is kept (every build keeps the same ops) and ignored.
        let forged = forge(&a, body(&a, "set", 40, json!({"key": format!("allow.{}.terminals", b.id()), "value": true})));
        assert!(a.membership.apply_ops(vec![forged]).unwrap().changed());
        sync(&a, &b);
        let allowed = a.membership.projection().allowed(&b.id());
        assert_eq!(allowed.get("files"), Some(&false));
        assert!(!allowed.contains_key("terminals"));
        assert_eq!(a.membership.projection().card(&b.id(), &a.id()).unwrap().allow, allowed);
    }

    #[test]
    fn an_add_by_a_removed_machine_stays_visible_as_added_by() {
        let a = Node::founder("mac", OWNER);
        let b = Node::new("pc");
        let c = Node::new("vps");
        a.enroll(&b, JoinVia::Auto);
        sync(&a, &b);
        // `b` adds `c` while it is still a member.
        let add_c = b.membership.add(c.descriptor(), JoinVia::Found).unwrap();
        c.membership.join(b.membership.all_ops()).unwrap();
        // Then `a` removes `b`, before `a` has heard of the Add.
        a.membership.remove(&b.id(), RemoveReason::Stolen).unwrap();
        // The Add reaches `a` afterwards. It is accepted as data: `b` was validly added.
        let outcome = a.membership.apply_ops(vec![add_c]).unwrap();
        assert!(outcome.changed() && outcome.rejected.is_empty(), "{:?}", outcome.rejected);
        let projection = a.membership.projection();
        assert!(projection.is_member(&c.id()), "the machine it added is a member");
        let card = projection.card(&c.id(), &a.id()).unwrap();
        assert_eq!((card.added_by.clone(), card.added_via), (Some(b.id()), Some(JoinVia::Found)));
        assert!(projection.is_removed(&b.id()) && projection.added_by_removed(&c.id()), "so the UI can say who added it");
        assert!(!projection.added_by_removed(&b.id()));
        // The user then cleans up with "Remove these too".
        a.membership.remove(&c.id(), RemoveReason::Stolen).unwrap();
        assert_eq!(a.member_names(), vec!["mac"]);
        // Ops the removed machine signs after its removal are also data, not tamper.
        let late = b.membership.set("broker", json!(b.id())).unwrap();
        let late_outcome = a.membership.apply_ops(vec![late]).unwrap();
        assert!(late_outcome.rejected.is_empty() && !late_outcome.tampered());
    }

    #[test]
    fn rejoin_gives_a_removed_machine_a_new_identity_and_a_clean_slate() {
        let a = Node::founder("mac", OWNER);
        let b = Node::new("pc");
        a.enroll(&b, JoinVia::Auto);
        sync(&a, &b);
        assert!(matches!(b.membership.rejoin(), Err(MembershipError::NotRemoved)));
        let old_id = b.id();
        let old_key = b.membership.keys().signing_public();
        a.membership.remove(&b.id(), RemoveReason::User).unwrap();
        sync(&a, &b);
        assert!(b.membership.self_removed());

        let reset = b.membership.rejoin().unwrap();
        assert_eq!(reset.old_machine_id, old_id);
        assert_ne!(reset.new_machine_id, old_id);
        assert_eq!(b.id(), reset.new_machine_id);
        assert_ne!(reset.keys.signing_public(), old_key);
        assert!(!b.membership.has_network() && !b.membership.self_removed(), "a lone machine again");
        assert!(b.store.network_ops_list().unwrap().is_empty() && b.store.network_machines_list().unwrap().is_empty());
        // The store and the key file say the same thing.
        assert_eq!(b.store.meta_get("environment_id").unwrap().unwrap(), reset.new_machine_id);
        assert_eq!(previous_machine_ids(&b.store).unwrap(), vec![old_id.clone()]);
        assert_eq!(b.store.meta_get(META_FINGERPRINT).unwrap().unwrap(), reset.keys.fingerprint());
        let on_disk = keys::load(&b.dir.path().join(keys::NETWORK_DIR)).unwrap().unwrap();
        assert_eq!(on_disk.signing_public(), reset.keys.signing_public());

        // Another member adds it again, as a new machine. The old tombstones do not apply.
        let renamed = &b;
        assert!(matches!(
            a.membership.add(MachineDescriptor { machine_id: old_id.clone(), ..renamed.descriptor() }, JoinVia::Found),
            Err(MembershipError::Tombstoned)
        ));
        a.membership.add(renamed.descriptor(), JoinVia::Found).unwrap();
        renamed.membership.join(a.membership.all_ops()).unwrap();
        assert_eq!(a.member_names(), vec!["mac", "pc"]);
        assert!(a.membership.projection().is_removed(&old_id));
        // Rejoining twice in a row accumulates the history.
        a.membership.remove(&renamed.id(), RemoveReason::Rejoin).unwrap();
        sync(&a, renamed);
        renamed.membership.rejoin().unwrap();
        assert_eq!(previous_machine_ids(&renamed.store).unwrap().len(), 2);
    }

    #[test]
    fn a_cooperating_machine_can_leave_and_start_over() {
        let a = Node::founder("mac", OWNER);
        let b = Node::new("pc");
        a.enroll(&b, JoinVia::Auto);
        let reset = b.membership.leave_and_reset().unwrap();
        assert_ne!(reset.old_machine_id, reset.new_machine_id);
        assert!(!b.membership.has_network());
        // The network it left still lists the old identity until someone removes it.
        assert!(a.membership.is_member(&reset.old_machine_id));
    }

    #[test]
    fn two_networks_of_the_same_owner_merge_and_converge() {
        let a = Node::founder("mac", OWNER);
        let b = Node::founder("pc", OWNER);
        let c = Node::new("vps");
        b.enroll(&c, JoinVia::Auto);

        // One click on either side writes a merge from a member of each network.
        let from_a = a.membership.merge(b.membership.genesis_envelope().unwrap()).unwrap();
        let from_b = b.membership.merge(a.membership.genesis_envelope().unwrap()).unwrap();
        // They exchange what they hold.
        a.membership.apply_ops(b.membership.all_ops()).unwrap();
        b.membership.apply_ops(a.membership.all_ops()).unwrap();
        c.membership.apply_ops(b.membership.all_ops()).unwrap();
        c.membership.apply_ops(a.membership.all_ops()).unwrap();
        for node in [&a, &b, &c] {
            assert_eq!(node.member_names(), vec!["mac", "pc", "vps"], "{}", node.name);
        }
        converged(&a, &b);
        converged(&a, &c);
        assert_eq!(a.membership.projection().merged_networks.len(), 2);
        assert!(a.membership.all_ops().contains(&from_a) && a.membership.all_ops().contains(&from_b));
        // A member of the absorbed network now signs for the merged one.
        let by_c = c.membership.set("broker", json!(c.id())).unwrap();
        assert!(a.membership.apply_ops(vec![by_c]).unwrap().changed());
        assert_eq!(a.membership.projection().broker(), Some(c.id().as_str()));
        // A merge is also stored once and is idempotent.
        assert!(matches!(a.membership.merge(b.membership.genesis_envelope().unwrap()), Err(MembershipError::Invalid(_))));
    }

    #[test]
    fn a_merge_with_another_owners_network_is_refused() {
        let a = Node::founder("mac", OWNER);
        let stranger = Node::founder("theirs", "someone-else@example.com");
        let error = a.membership.merge(stranger.membership.genesis_envelope().unwrap()).unwrap_err();
        assert!(matches!(error, MembershipError::Rejected(OpError::WrongOwner)), "{error}");
        // Nor can a peer slip one in.
        let network = a.membership.network_id().unwrap();
        let other_genesis = stranger.membership.genesis_envelope().unwrap();
        let other_network = stranger.membership.network_id().unwrap();
        let hand_made = forge(
            &a,
            json!({"v":1,"network_id":network,"kind":"merge","at":Utc::now().to_rfc3339(),"lamport":9,"other_network_id":other_network,"other_genesis":other_genesis}),
        );
        let outcome = a.membership.apply_ops(vec![hand_made]).unwrap();
        assert_eq!(outcome.rejected[0].reason, OpError::WrongOwner);
        // Their operations are not ours to keep either.
        let theirs = a.membership.apply_ops(stranger.membership.all_ops()).unwrap();
        assert!(!theirs.changed());
        assert_eq!(theirs.rejected[0].reason, OpError::ForeignNetwork);
        assert!(!theirs.tampered(), "a foreign network is not a forgery");
    }

    #[test]
    fn operations_of_another_network_and_second_genesis_are_refused() {
        let a = Node::founder("mac", OWNER);
        let b = Node::new("pc");
        a.enroll(&b, JoinVia::Auto);
        let wrong_network = ops::seal(
            &a.membership.keys(),
            &a.id(),
            json!({"v":1,"network_id":"not-ours","kind":"set","at":Utc::now().to_rfc3339(),"lamport":9,"key":"broker","value":"x"})
                .to_string(),
        );
        assert_eq!(b.membership.apply_ops(vec![wrong_network]).unwrap().rejected[0].reason, OpError::ForeignNetwork);
        // Another genesis for our own network id is a forgery of the founding.
        let network = a.membership.network_id().unwrap();
        let again = forge(
            &b,
            json!({"v":1,"network_id":network,"kind":"genesis","at":Utc::now().to_rfc3339(),"lamport":1,"owner_login":"mallory","tailnet":"x","machine":b.descriptor()}),
        );
        assert_eq!(a.membership.apply_ops(vec![again]).unwrap().rejected[0].reason, OpError::DuplicateGenesis);
        // A genesis must be signed by the machine it founds.
        let c = Node::new("vps");
        let borrowed = ops::seal(
            &c.membership.keys(),
            &c.id(),
            json!({"v":1,"network_id":"net","kind":"genesis","at":Utc::now().to_rfc3339(),"lamport":1,"owner_login":OWNER,"tailnet":"x","machine":b.descriptor()}).to_string(),
        );
        let lone = Node::new("lone");
        assert!(matches!(lone.membership.join(vec![borrowed]), Err(MembershipError::NotInSet)));
    }

    #[test]
    fn the_same_machine_id_cannot_be_given_a_second_key() {
        let a = Node::founder("mac", OWNER);
        let b = Node::new("pc");
        a.enroll(&b, JoinVia::Auto);
        let impostor_key = MachineKeys::generate();
        let swapped = MachineDescriptor { signing_key: impostor_key.signing_public_b64(), ..b.descriptor() };
        assert!(matches!(a.membership.add(swapped.clone(), JoinVia::Found), Err(MembershipError::Rejected(OpError::KeyConflict))));
        let forged = forge(&a, body_for_add_descriptor(&a, &swapped, 30));
        assert_eq!(a.membership.apply_ops(vec![forged]).unwrap().rejected[0].reason, OpError::KeyConflict);
        assert!(matches!(a.membership.add(b.descriptor(), JoinVia::Found), Err(MembershipError::AlreadyMember)));
    }

    #[test]
    fn the_projection_does_not_depend_on_the_order_operations_arrive_in() {
        let a = Node::founder("mac", OWNER);
        let b = Node::new("pc");
        let c = Node::new("vps");
        a.enroll(&b, JoinVia::Auto);
        a.enroll(&c, JoinVia::Auto);
        sync(&a, &b);
        sync(&a, &c);
        b.membership.set("broker", json!("b")).unwrap();
        c.membership.set("broker", json!("c")).unwrap();
        a.membership.set(&format!("machine.{}.name", c.id()), json!("vps2")).unwrap();
        sync(&a, &b);
        sync(&a, &c);
        sync(&b, &c);
        a.membership.remove(&c.id(), RemoveReason::User).unwrap();
        sync(&a, &b);
        converged(&a, &b);

        let ops = a.membership.all_ops();
        let baseline = (*a.membership.projection()).clone();
        for rotation in 0..ops.len() {
            let mut shuffled = ops.clone();
            shuffled.rotate_left(rotation);
            if rotation % 2 == 1 {
                shuffled.reverse();
            }
            let replica = Node::new("replica");
            replica.membership.apply_in_any_order(shuffled);
            assert_eq!(*replica.membership.projection(), baseline, "rotation {rotation}");
        }
    }

    #[test]
    fn the_set_survives_a_restart_and_leaves_nothing_out() {
        let a = Node::founder("mac", OWNER);
        let b = Node::new("pc");
        a.enroll(&b, JoinVia::Auto);
        a.membership.set("broker", json!(b.id())).unwrap();
        a.membership.remove(&b.id(), RemoveReason::User).unwrap();
        let reopened = Membership::open(a.store.clone(), a.dir.path().join(keys::NETWORK_DIR), a.id(), a.membership.keys()).unwrap();
        assert_eq!(reopened.digest(), a.membership.digest());
        assert_eq!(*reopened.projection(), *a.membership.projection());
        assert_eq!(reopened.all_ops(), a.membership.all_ops());
    }

    #[test]
    fn a_key_that_does_not_match_the_membership_blocks_signing() {
        let a = Node::founder("mac", OWNER);
        // The key file was lost and replaced: same machine id, new key.
        let replaced = Membership::open(a.store.clone(), a.dir.path().join("replaced"), a.id(), Arc::new(MachineKeys::generate())).unwrap();
        assert!(replaced.key_mismatch());
        assert!(!a.membership.key_mismatch());
        assert!(matches!(replaced.set("broker", json!("x")), Err(MembershipError::KeyMismatch)));
        assert!(matches!(replaced.remove(&a.id(), RemoveReason::User), Err(MembershipError::KeyMismatch)));
        // Leaving and rejoining is the way out.
        replaced.leave_and_reset().unwrap();
        assert!(!replaced.key_mismatch() && !replaced.has_network());
    }

    #[test]
    fn start_makes_keys_once_records_them_in_meta_and_keeps_the_identity() {
        let dir = TempDir::new("start");
        let store = Store::open_in_memory().unwrap();
        let first = start(dir.path(), &store, "machine-1").unwrap();
        let keys = first.keys();
        assert_eq!(store.meta_get(META_SIGNING_KEY).unwrap().unwrap(), keys.signing_public_b64());
        assert_eq!(store.meta_get(META_SEALING_KEY).unwrap().unwrap(), keys.sealing_public_b64());
        assert_eq!(store.meta_get(META_FINGERPRINT).unwrap().unwrap(), keys.fingerprint());
        assert!(dir.path().join(keys::NETWORK_DIR).join(keys::KEY_FILE).exists());
        let second = start(dir.path(), &store, "machine-1").unwrap();
        assert_eq!(second.keys().signing_public(), keys.signing_public());
        assert_eq!(second.machine_id(), "machine-1");
        // A damaged key file stops the network; it does not replace the identity.
        std::fs::write(dir.path().join(keys::NETWORK_DIR).join(keys::KEY_FILE), b"garbage").unwrap();
        assert!(start(dir.path(), &store, "machine-1").is_err());
    }

    #[test]
    fn builders_need_a_current_member() {
        let a = Node::founder("mac", OWNER);
        let b = Node::new("pc");
        a.enroll(&b, JoinVia::Auto);
        let c = Node::new("vps");
        // A machine that was removed cannot sign anything new for the network.
        a.membership.remove(&b.id(), RemoveReason::User).unwrap();
        sync(&a, &b);
        assert!(matches!(b.membership.add(c.descriptor(), JoinVia::Auto), Err(MembershipError::NotMember)));
        assert!(matches!(b.membership.merge(a.membership.genesis_envelope().unwrap()), Err(MembershipError::NotMember)));
        // `build_add` does not keep what it signs; `add` does.
        let before = a.membership.digest();
        let offer = a.membership.build_add(c.descriptor(), JoinVia::Found).unwrap();
        assert_eq!(a.membership.digest(), before);
        assert!(!a.membership.is_member(&c.id()));
        assert!(a.membership.apply_ops(vec![offer]).unwrap().changed());
        assert!(a.membership.is_member(&c.id()));
    }
}
