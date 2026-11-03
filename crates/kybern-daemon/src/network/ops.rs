//! Membership operations (design §2.3): the signed envelope, hashing, verification, the validation
//! rules that decide which operations a machine keeps, and the projection from the kept set to
//! members and registers.
//!
//! Everything here is pure. [`OpSet`] holds the operations a machine has accepted and
//! [`Projection`] is derived from it; `membership.rs` adds the store, the signing key and the
//! operations a machine creates.
//!
//! What is dropped and what is only ignored matters for convergence. Two builds that disagree about
//! which operations to keep would keep different sets and never agree on a digest, so [`OpSet::apply`]
//! rejects only what every build rejects: a body that is not a body, an `id` that is not the hash of
//! the body, a bad signature, a signer that has never been validly added, an operation of a network
//! that is not ours, and a few structural faults. Whether a kept operation *does* anything
//! (a `set` for a key nobody reads, an `allow` register signed by the wrong machine, an unknown
//! kind) is decided by the projection, from the set alone.

use std::collections::{BTreeMap, BTreeSet, HashSet};

use anyhow::{Result, bail};
use base64::Engine;
use base64::engine::general_purpose::STANDARD as B64;
use chrono::{DateTime, Utc};
use kybern_protocol::{
    JoinVia, MachineCard, MachineDescriptor, MachineId, MachineStatus, OP_SIGNATURE_DOMAIN, OP_VERSION, OpBody, OpEnvelope, OpKind,
    RemoveReason,
};
use serde_json::Value;
use sha2::{Digest, Sha256};

use super::keys::{self, MachineKeys};

// ---- envelopes ----

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

/// An operation's id: the SHA-256 of its exact body text, lowercase hex.
pub fn op_id(body: &str) -> String {
    hex(&Sha256::digest(body.as_bytes()))
}

/// The bytes an operation's signature covers: the domain separator followed by the body text.
pub fn signing_input(body: &str) -> Vec<u8> {
    let mut input = Vec::with_capacity(OP_SIGNATURE_DOMAIN.len() + body.len());
    input.extend_from_slice(OP_SIGNATURE_DOMAIN.as_bytes());
    input.extend_from_slice(body.as_bytes());
    input
}

/// The JSON text of a new operation's body. This text is what gets hashed and signed, and it is
/// never re-serialized afterwards.
pub fn encode_body(network_id: &str, at: DateTime<Utc>, lamport: u64, op: &OpKind) -> Result<String> {
    if matches!(op, OpKind::Unknown) {
        bail!("an operation of an unknown kind cannot be created");
    }
    Ok(serde_json::to_string(&OpBody { v: OP_VERSION, network_id: network_id.to_string(), at, lamport, op: op.clone() })?)
}

/// Hash and sign a body as `signer`.
pub fn seal(keys: &MachineKeys, signer: &str, body: String) -> OpEnvelope {
    let signature = B64.encode(keys.sign(&signing_input(&body)));
    OpEnvelope { id: op_id(&body), body, signer: signer.to_string(), signature }
}

/// The digest two machines compare when a session opens: the SHA-256 of the sorted operation ids,
/// each followed by a newline.
pub fn digest_of(ids: &[String]) -> String {
    let mut sorted: Vec<&String> = ids.iter().collect();
    sorted.sort();
    let mut hasher = Sha256::new();
    for id in sorted {
        hasher.update(id.as_bytes());
        hasher.update(b"\n");
    }
    hex(&hasher.finalize())
}

// ---- parsing and validation ----

/// Why an operation was refused.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum OpError {
    #[error("not a valid operation: {0}")]
    Malformed(String),
    #[error("the id is not the hash of the body")]
    IdMismatch,
    #[error("the signature does not verify")]
    BadSignature,
    #[error("the signer has never been added to the network")]
    NotMember,
    #[error("the operation belongs to a network this machine is not in")]
    ForeignNetwork,
    #[error("this machine is not in a network")]
    NoNetwork,
    #[error("the network already has a genesis")]
    DuplicateGenesis,
    #[error("the other network belongs to a different owner")]
    WrongOwner,
    #[error("the machine id is already bound to a different signing key")]
    KeyConflict,
}

impl OpError {
    /// A fault that an honest peer never produces. The sender's session is closed (§2.3).
    pub fn is_tamper(&self) -> bool {
        matches!(self, Self::Malformed(_) | Self::IdMismatch | Self::BadSignature | Self::NotMember)
    }
}

/// An operation that was refused, and why.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rejection {
    pub id: String,
    pub reason: OpError,
}

/// An envelope whose id matches its body and whose body parsed. The signature is not checked yet:
/// that needs the signer's key, which only the set knows.
#[derive(Debug, Clone)]
pub struct ParsedOp {
    pub envelope: OpEnvelope,
    pub network_id: String,
    /// The kind string as written in the body, also for kinds this build does not know.
    pub kind: String,
    pub lamport: u64,
    pub at: DateTime<Utc>,
    pub op: OpKind,
}

impl ParsedOp {
    pub fn id(&self) -> &str {
        &self.envelope.id
    }

    pub fn signer(&self) -> &str {
        &self.envelope.signer
    }

    fn order(&self) -> (u64, &str) {
        (self.lamport, &self.envelope.id)
    }
}

const KNOWN_KINDS: [&str; 5] = ["genesis", "add", "remove", "set", "merge"];

/// Check an envelope's id and read its body. A body of a newer version, or of a kind this build
/// does not know, parses as [`OpKind::Unknown`]: it is kept, verified and gossiped.
pub fn parse(envelope: &OpEnvelope) -> Result<ParsedOp, OpError> {
    if op_id(&envelope.body) != envelope.id {
        return Err(OpError::IdMismatch);
    }
    let malformed = |what: &str| OpError::Malformed(what.to_string());
    let value: Value = serde_json::from_str(&envelope.body).map_err(|error| OpError::Malformed(error.to_string()))?;
    let fields = value.as_object().ok_or_else(|| malformed("the body is not an object"))?;
    let version = fields.get("v").and_then(Value::as_u64).ok_or_else(|| malformed("v"))?;
    let network_id =
        fields.get("network_id").and_then(Value::as_str).filter(|id| !id.is_empty()).ok_or_else(|| malformed("network_id"))?.to_string();
    let kind = fields.get("kind").and_then(Value::as_str).ok_or_else(|| malformed("kind"))?.to_string();
    let lamport = fields.get("lamport").and_then(Value::as_u64).ok_or_else(|| malformed("lamport"))?;
    let at: DateTime<Utc> = fields.get("at").and_then(Value::as_str).and_then(|text| text.parse().ok()).ok_or_else(|| malformed("at"))?;
    let op = if version > u64::from(OP_VERSION) || !KNOWN_KINDS.contains(&kind.as_str()) {
        OpKind::Unknown
    } else {
        serde_json::from_value::<OpKind>(value).map_err(|error| OpError::Malformed(error.to_string()))?
    };
    Ok(ParsedOp { envelope: envelope.clone(), network_id, kind, lamport, at, op })
}

fn verify_with(signing_key_b64: &str, op: &ParsedOp) -> Result<(), OpError> {
    let signature = B64.decode(&op.envelope.signature).map_err(|_| OpError::BadSignature)?;
    keys::verify_signature(signing_key_b64, &signing_input(&op.envelope.body), &signature).map_err(|_| OpError::BadSignature)
}

/// A descriptor an operation can use: it names a machine and carries a usable signing key.
fn check_descriptor(machine: &MachineDescriptor) -> Result<(), OpError> {
    if machine.machine_id.is_empty() {
        return Err(OpError::Malformed("a machine descriptor has no machine id".into()));
    }
    let usable = B64.decode(&machine.signing_key).map(|raw| raw.len() == 32).unwrap_or(false);
    if !usable {
        return Err(OpError::Malformed("a machine descriptor has no usable signing key".into()));
    }
    Ok(())
}

/// A genesis the way a merge carries it: a genesis that signed itself, naming the network it says.
fn check_embedded_genesis(op: &ParsedOp, expected_network: &str) -> Result<(), OpError> {
    let OpKind::Genesis { machine, .. } = &op.op else {
        return Err(OpError::Malformed("the other network's genesis is not a genesis".into()));
    };
    if op.network_id != expected_network {
        return Err(OpError::Malformed("the other genesis is for a different network".into()));
    }
    if op.signer() != machine.machine_id {
        return Err(OpError::Malformed("a genesis must be signed by the machine it founds".into()));
    }
    check_descriptor(machine)?;
    verify_with(&machine.signing_key, op)
}

// ---- the set ----

/// What [`OpSet::apply`] did with a batch.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Applied {
    /// The ids kept now, in the order they were accepted. A merge brings the genesis it carries.
    pub accepted: Vec<String>,
    /// Operations already in the set.
    pub duplicates: usize,
    pub rejected: Vec<Rejection>,
}

enum Step {
    Accept,
    Duplicate,
    Reject(OpError),
    /// Not decidable yet; something later in the batch may settle it.
    Defer(OpError),
}

/// Which genesis a joining set adopts as its network: one that no merge in the batch names as the
/// "other" side if there is one (a one-sided merge absorbs the named network into the other), else
/// the oldest. A symmetric merge works whichever is chosen.
fn choose_founding_network(pending: &[(ParsedOp, OpError)]) -> Option<String> {
    let named: HashSet<&str> = pending
        .iter()
        .filter_map(|(op, _)| match &op.op {
            OpKind::Merge { other_network_id, .. } => Some(other_network_id.as_str()),
            _ => None,
        })
        .collect();
    let geneses: Vec<&ParsedOp> = pending.iter().map(|(op, _)| op).filter(|op| matches!(op.op, OpKind::Genesis { .. })).collect();
    geneses.iter().filter(|op| !named.contains(op.network_id.as_str())).chain(geneses.iter()).next().map(|op| op.network_id.clone())
}

/// The operations a machine has accepted, with the facts validation needs.
#[derive(Debug, Clone, Default)]
pub struct OpSet {
    ops: BTreeMap<String, ParsedOp>,
    /// Every machine that has ever had a valid Add (or founded a network), with its signing key.
    /// Removed machines stay: their earlier operations are still data (§2.3).
    signers: BTreeMap<MachineId, String>,
    /// The network this set was founded or adopted as: the first genesis it accepted.
    primary: Option<String>,
    /// `primary` plus every network a merge brought in.
    networks: BTreeSet<String>,
    /// The owner the primary network's genesis names; a merge must come from the same owner.
    owner_login: Option<String>,
}

impl OpSet {
    pub fn is_empty(&self) -> bool {
        self.ops.is_empty()
    }

    /// The network id this set goes by: the smallest of the networks it holds, so that two
    /// machines that merged the same networks agree on it whichever one they started from.
    pub fn network_id(&self) -> Option<&str> {
        self.networks.iter().next().map(String::as_str)
    }

    /// Whether `network_id` is this set's network or one a merge brought in.
    pub fn has_network(&self, network_id: &str) -> bool {
        self.networks.contains(network_id)
    }

    pub fn contains(&self, id: &str) -> bool {
        self.ops.contains_key(id)
    }

    pub fn get(&self, id: &str) -> Option<&ParsedOp> {
        self.ops.get(id)
    }

    /// The highest `lamport` held, or 0. A new operation uses this plus one.
    pub fn max_lamport(&self) -> u64 {
        self.ops.values().map(|op| op.lamport).max().unwrap_or(0)
    }

    /// The signing key of a machine that has ever had a valid Add.
    pub fn signer_key(&self, machine_id: &str) -> Option<&str> {
        self.signers.get(machine_id).map(String::as_str)
    }

    /// Every operation, oldest first: by `lamport`, then id.
    pub fn sorted(&self) -> Vec<&ParsedOp> {
        let mut all: Vec<&ParsedOp> = self.ops.values().collect();
        all.sort_by(|a, b| a.order().cmp(&b.order()));
        all
    }

    pub fn envelopes(&self) -> Vec<OpEnvelope> {
        self.sorted().into_iter().map(|op| op.envelope.clone()).collect()
    }

    /// The ids, sorted.
    pub fn ids(&self) -> Vec<String> {
        self.ops.keys().cloned().collect()
    }

    pub fn digest(&self) -> String {
        digest_of(&self.ids())
    }

    /// The operations whose ids are not in `known`, oldest first.
    pub fn missing_from(&self, known: &[String]) -> Vec<OpEnvelope> {
        let known: HashSet<&str> = known.iter().map(String::as_str).collect();
        self.sorted().into_iter().filter(|op| !known.contains(op.id())).map(|op| op.envelope.clone()).collect()
    }

    /// Validate `batch` against the set and keep what passes.
    ///
    /// Operations are tried oldest first, and again after each pass, so a batch may hold an Add and
    /// the operations its machine signed in any order. With `bootstrap` a set with no network adopts
    /// the oldest genesis in the batch (joining); without it a lone set keeps nothing.
    pub fn apply(&mut self, batch: Vec<ParsedOp>, bootstrap: bool) -> Applied {
        let mut applied = Applied::default();
        let mut seen: HashSet<String> = HashSet::new();
        let mut pending: Vec<(ParsedOp, OpError)> = Vec::new();
        for op in batch {
            if self.ops.contains_key(op.id()) || !seen.insert(op.id().to_string()) {
                applied.duplicates += 1;
            } else {
                pending.push((op, OpError::NotMember));
            }
        }
        pending.sort_by(|(a, _), (b, _)| a.order().cmp(&b.order()));
        let adopt = if bootstrap && self.primary.is_none() { choose_founding_network(&pending) } else { None };
        loop {
            let mut progress = false;
            let mut deferred = Vec::new();
            for (op, _) in std::mem::take(&mut pending) {
                match self.step(&op, bootstrap, adopt.as_deref()) {
                    Step::Accept => {
                        self.keep(op, &mut applied);
                        progress = true;
                    }
                    Step::Duplicate => applied.duplicates += 1,
                    Step::Reject(reason) => applied.rejected.push(Rejection { id: op.id().to_string(), reason }),
                    Step::Defer(reason) => deferred.push((op, reason)),
                }
            }
            pending = deferred;
            if !progress || pending.is_empty() {
                break;
            }
        }
        for (op, reason) in pending {
            applied.rejected.push(Rejection { id: op.id().to_string(), reason });
        }
        applied
    }

    fn step(&self, op: &ParsedOp, bootstrap: bool, adopt: Option<&str>) -> Step {
        if self.ops.contains_key(op.id()) {
            return Step::Duplicate;
        }
        match &op.op {
            OpKind::Genesis { machine, .. } => {
                if op.signer() != machine.machine_id {
                    return Step::Reject(OpError::Malformed("a genesis must be signed by the machine it founds".into()));
                }
                if let Err(error) = check_descriptor(machine) {
                    return Step::Reject(error);
                }
                if let Err(error) = verify_with(&machine.signing_key, op) {
                    return Step::Reject(error);
                }
                match &self.primary {
                    None if bootstrap && adopt == Some(op.network_id.as_str()) => Step::Accept,
                    None if bootstrap => Step::Defer(OpError::ForeignNetwork),
                    None => Step::Reject(OpError::NoNetwork),
                    Some(_) if self.networks.contains(&op.network_id) => Step::Reject(OpError::DuplicateGenesis),
                    // Another network's genesis waits for a merge that names it.
                    Some(_) => Step::Defer(OpError::ForeignNetwork),
                }
            }
            kind => {
                if self.primary.is_none() {
                    return if bootstrap { Step::Defer(OpError::NoNetwork) } else { Step::Reject(OpError::NoNetwork) };
                }
                if !self.networks.contains(&op.network_id) {
                    return Step::Defer(OpError::ForeignNetwork);
                }
                let Some(key) = self.signers.get(op.signer()) else {
                    return Step::Defer(OpError::NotMember);
                };
                if let Err(error) = verify_with(key, op) {
                    return Step::Reject(error);
                }
                match kind {
                    OpKind::Add { machine, .. } => {
                        if let Err(error) = check_descriptor(machine) {
                            return Step::Reject(error);
                        }
                        if self.signers.get(&machine.machine_id).is_some_and(|existing| *existing != machine.signing_key) {
                            return Step::Reject(OpError::KeyConflict);
                        }
                    }
                    OpKind::Merge { other_network_id, other_genesis } => {
                        let embedded = match parse(other_genesis) {
                            Ok(embedded) => embedded,
                            Err(error) => return Step::Reject(error),
                        };
                        if let Err(error) = check_embedded_genesis(&embedded, other_network_id) {
                            return Step::Reject(error);
                        }
                        if *other_network_id == op.network_id {
                            return Step::Reject(OpError::Malformed("a network cannot merge with itself".into()));
                        }
                        let OpKind::Genesis { owner_login, .. } = &embedded.op else { unreachable!("checked above") };
                        let ours = self.owner_login.as_deref().unwrap_or_default();
                        if !owner_login.eq_ignore_ascii_case(ours) {
                            return Step::Reject(OpError::WrongOwner);
                        }
                    }
                    _ => {}
                }
                Step::Accept
            }
        }
    }

    /// Record an accepted operation and what it makes valid.
    fn keep(&mut self, op: ParsedOp, applied: &mut Applied) {
        match &op.op {
            OpKind::Genesis { owner_login, machine, .. } => {
                if self.primary.is_none() {
                    self.primary = Some(op.network_id.clone());
                    self.owner_login = Some(owner_login.clone());
                }
                self.networks.insert(op.network_id.clone());
                self.signers.insert(machine.machine_id.clone(), machine.signing_key.clone());
            }
            OpKind::Add { machine, .. } => {
                self.signers.entry(machine.machine_id.clone()).or_insert_with(|| machine.signing_key.clone());
            }
            OpKind::Merge { other_network_id, other_genesis } => {
                self.networks.insert(other_network_id.clone());
                // The genesis the merge carries is part of the set from now on, so that a peer that
                // syncs by ids receives it. It was checked when the merge was accepted.
                if let Ok(embedded) = parse(other_genesis)
                    && !self.ops.contains_key(embedded.id())
                    && let OpKind::Genesis { machine, .. } = &embedded.op
                {
                    self.signers.entry(machine.machine_id.clone()).or_insert_with(|| machine.signing_key.clone());
                    applied.accepted.push(embedded.id().to_string());
                    self.ops.insert(embedded.id().to_string(), embedded);
                }
            }
            _ => {}
        }
        applied.accepted.push(op.id().to_string());
        self.ops.insert(op.id().to_string(), op);
    }

    pub fn project(&self) -> Projection {
        Projection::build(self)
    }

    /// What a removed machine needs to verify that it was removed (ADE-120), oldest first: every Remove that names
    /// `machine_id`, and the chain that makes each signer valid under [`OpSet::apply`], i.e. the Genesis or
    /// earliest Add of the signer, then that operation's own signer, and so on. A Merge that brings in a network
    /// one of those operations belongs to comes too, with its own chain, since the removed machine may have
    /// been offline across it. Nothing else of the set is in it: no registers, no other Remove, no Add of a
    /// machine that signed none of these. Empty when no Remove names the machine.
    pub fn removal_proof(&self, machine_id: &str) -> Vec<OpEnvelope> {
        let sorted = self.sorted();
        // The operation that first made each machine a signer, the way the projection counts it.
        let mut admitted: BTreeMap<&str, &ParsedOp> = BTreeMap::new();
        for op in &sorted {
            if let OpKind::Genesis { machine, .. } | OpKind::Add { machine, .. } = &op.op {
                admitted.entry(machine.machine_id.as_str()).or_insert(op);
            }
        }
        let mut work: Vec<&ParsedOp> = sorted
            .iter()
            .copied()
            .filter(|op| matches!(&op.op, OpKind::Remove { machine_id: removed, .. } if removed == machine_id))
            .collect();
        let mut chosen: BTreeMap<&str, &ParsedOp> = BTreeMap::new();
        let mut networks: BTreeSet<&str> = BTreeSet::new();
        loop {
            while let Some(op) = work.pop() {
                if chosen.insert(op.id(), op).is_some() {
                    continue;
                }
                networks.insert(op.network_id.as_str());
                if let OpKind::Merge { other_network_id, .. } = &op.op {
                    networks.insert(other_network_id.as_str());
                }
                if let Some(admitting) = admitted.get(op.signer()) {
                    work.push(admitting);
                }
            }
            let merges: Vec<&ParsedOp> = sorted
                .iter()
                .copied()
                .filter(|op| !chosen.contains_key(op.id()))
                .filter(|op| {
                    matches!(&op.op, OpKind::Merge { other_network_id, .. }
                        if networks.contains(other_network_id.as_str()) || networks.contains(op.network_id.as_str()))
                })
                .collect();
            if merges.is_empty() {
                break;
            }
            work.extend(merges);
        }
        let mut proof: Vec<&ParsedOp> = chosen.into_values().collect();
        proof.sort_by(|a, b| a.order().cmp(&b.order()));
        proof.into_iter().map(|op| op.envelope.clone()).collect()
    }
}

// ---- the projection ----

/// When and by whom a machine was removed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Removal {
    pub by: MachineId,
    pub reason: RemoveReason,
    pub at: DateTime<Utc>,
    pub op_id: String,
}

/// A machine the set has ever validly added, current or removed.
#[derive(Debug, Clone, PartialEq)]
pub struct MachineEntry {
    /// As its Add (or Genesis) wrote it, with the current name filled in.
    pub descriptor: MachineDescriptor,
    pub paused: bool,
    /// The signer of its Add. `None` for the machine that founded the network.
    pub added_by: Option<MachineId>,
    pub added_via: Option<JoinVia>,
    pub added_at: DateTime<Utc>,
    pub add_op: String,
    pub removed: Option<Removal>,
}

/// The winner of a last-writer register.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Register {
    pub value: Value,
    pub lamport: u64,
    pub signer: MachineId,
    pub op_id: String,
}

/// The members and registers a set implies. It is rebuilt from the set after every change.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Projection {
    pub network_id: Option<String>,
    pub owner_login: Option<String>,
    pub tailnet: Option<String>,
    /// Networks a merge brought in, besides `network_id`.
    pub merged_networks: Vec<String>,
    pub machines: BTreeMap<MachineId, MachineEntry>,
    pub registers: BTreeMap<String, Register>,
    /// Tailscale node ids of removed machines. Auto-join never applies to them (§2.6).
    pub tombstoned_nodes: BTreeSet<String>,
    /// Client devices revoked anywhere (`device.<id>.revoked`). Like a removal, a revocation is
    /// permanent: any operation that ever said so counts, whatever was written after it (§3.4).
    pub revoked_devices: BTreeSet<String>,
}

impl Projection {
    pub fn build(set: &OpSet) -> Self {
        let mut projection = Projection { network_id: set.network_id().map(str::to_string), ..Default::default() };
        let mut removals: Vec<(MachineId, Option<String>, Removal)> = Vec::new();
        let mut sets: Vec<&ParsedOp> = Vec::new();

        // Oldest first, so the earliest Add of a machine is the one that counts, on every machine.
        for op in set.sorted() {
            match &op.op {
                OpKind::Genesis { owner_login, tailnet, machine } => {
                    if Some(op.network_id.as_str()) == set.network_id() {
                        projection.tailnet = Some(tailnet.clone());
                        projection.owner_login = Some(owner_login.clone());
                    }
                    projection.machines.entry(machine.machine_id.clone()).or_insert_with(|| MachineEntry {
                        descriptor: machine.clone(),
                        paused: false,
                        added_by: None,
                        added_via: None,
                        added_at: op.at,
                        add_op: op.id().to_string(),
                        removed: None,
                    });
                }
                OpKind::Add { machine, via } => {
                    projection.machines.entry(machine.machine_id.clone()).or_insert_with(|| MachineEntry {
                        descriptor: machine.clone(),
                        paused: false,
                        added_by: Some(op.signer().to_string()),
                        added_via: Some(*via),
                        added_at: op.at,
                        add_op: op.id().to_string(),
                        removed: None,
                    });
                }
                OpKind::Remove { machine_id, ts_node_id, reason } => {
                    removals.push((
                        machine_id.clone(),
                        ts_node_id.clone(),
                        Removal { by: op.signer().to_string(), reason: *reason, at: op.at, op_id: op.id().to_string() },
                    ));
                }
                OpKind::Merge { other_network_id, .. } => projection.merged_networks.push(other_network_id.clone()),
                OpKind::Set { .. } => sets.push(op),
                OpKind::Unknown => {}
            }
        }

        // Removal wins and is permanent: it applies whatever order the operations arrived in, and it
        // tombstones the signing key as well as the machine id.
        let mut removal_by_id: BTreeMap<MachineId, Removal> = BTreeMap::new();
        for (machine_id, ts_node_id, removal) in removals {
            if let Some(node) = ts_node_id {
                projection.tombstoned_nodes.insert(node);
            }
            removal_by_id.entry(machine_id).or_insert(removal);
        }
        let mut removal_by_key: BTreeMap<String, Removal> = BTreeMap::new();
        for (id, entry) in projection.machines.iter_mut() {
            if let Some(removal) = removal_by_id.get(id) {
                if let Some(node) = &entry.descriptor.ts_node_id {
                    projection.tombstoned_nodes.insert(node.clone());
                }
                removal_by_key.entry(entry.descriptor.signing_key.clone()).or_insert_with(|| removal.clone());
                entry.removed = Some(removal.clone());
            }
        }
        for entry in projection.machines.values_mut() {
            if entry.removed.is_none()
                && let Some(removal) = removal_by_key.get(&entry.descriptor.signing_key)
            {
                entry.removed = Some(removal.clone());
            }
        }

        for op in sets {
            let OpKind::Set { key, value } = &op.op else { continue };
            if !register_is_valid(key, op.signer()) {
                continue;
            }
            if let Some(device) = revoked_device_key(key)
                && value.as_bool() == Some(true)
            {
                projection.revoked_devices.insert(device.to_string());
            }
            let candidate =
                Register { value: value.clone(), lamport: op.lamport, signer: op.signer().to_string(), op_id: op.id().to_string() };
            let wins = projection
                .registers
                .get(key)
                .is_none_or(|held| (candidate.lamport, &candidate.signer, &candidate.op_id) > (held.lamport, &held.signer, &held.op_id));
            if wins {
                projection.registers.insert(key.clone(), candidate);
            }
        }

        let renames: Vec<(MachineId, String)> = projection
            .machines
            .keys()
            .filter_map(|id| {
                projection
                    .registers
                    .get(&format!("machine.{id}.name"))
                    .and_then(|r| r.value.as_str())
                    .map(|name| (id.clone(), name.to_string()))
            })
            .filter(|(_, name)| !name.trim().is_empty())
            .collect();
        for (id, name) in renames {
            if let Some(entry) = projection.machines.get_mut(&id) {
                entry.descriptor.name = name;
            }
        }
        let paused: Vec<MachineId> = projection
            .machines
            .keys()
            .filter(|id| projection.registers.get(&format!("machine.{id}.paused")).and_then(|r| r.value.as_bool()) == Some(true))
            .cloned()
            .collect();
        for id in paused {
            if let Some(entry) = projection.machines.get_mut(&id) {
                entry.paused = true;
            }
        }
        projection
    }

    pub fn entry(&self, machine_id: &str) -> Option<&MachineEntry> {
        self.machines.get(machine_id)
    }

    /// A machine with a valid Add and no Remove.
    pub fn is_member(&self, machine_id: &str) -> bool {
        self.machines.get(machine_id).is_some_and(|entry| entry.removed.is_none())
    }

    pub fn is_removed(&self, machine_id: &str) -> bool {
        self.machines.get(machine_id).is_some_and(|entry| entry.removed.is_some())
    }

    /// Current members, by machine id.
    pub fn members(&self) -> impl Iterator<Item = (&MachineId, &MachineEntry)> {
        self.machines.iter().filter(|(_, entry)| entry.removed.is_none())
    }

    /// Whether this key belongs to a removed machine. A removed key never comes back (§2.3).
    pub fn is_key_tombstoned(&self, signing_key_b64: &str) -> bool {
        self.machines.values().any(|entry| entry.removed.is_some() && entry.descriptor.signing_key == signing_key_b64)
    }

    pub fn is_node_tombstoned(&self, ts_node_id: &str) -> bool {
        self.tombstoned_nodes.contains(ts_node_id)
    }

    /// Whether the machine that added `machine_id` has itself been removed: the UI marks such Adds
    /// "added by <machine>" so the user can clean up (§2.3).
    pub fn added_by_removed(&self, machine_id: &str) -> bool {
        self.entry(machine_id).and_then(|entry| entry.added_by.as_deref()).is_some_and(|adder| self.is_removed(adder))
    }

    /// Whether a client device was revoked on any member.
    pub fn is_device_revoked(&self, device_id: &str) -> bool {
        self.revoked_devices.contains(device_id)
    }

    pub fn register(&self, key: &str) -> Option<&Register> {
        self.registers.get(key)
    }

    /// The machine named by the `broker` register, if any.
    pub fn broker(&self) -> Option<&str> {
        self.register("broker").and_then(|register| register.value.as_str()).filter(|id| !id.is_empty())
    }

    pub fn follow_updates(&self) -> bool {
        self.register("follow_updates").and_then(|register| register.value.as_bool()).unwrap_or(false)
    }

    /// What `machine_id` lets its peers use: the `allow.<machine_id>.<capability>` registers.
    pub fn allowed(&self, machine_id: &str) -> BTreeMap<String, bool> {
        let prefix = format!("allow.{machine_id}.");
        self.registers
            .iter()
            .filter_map(|(key, register)| Some((key.strip_prefix(&prefix)?.to_string(), register.value.as_bool()?)))
            .collect()
    }

    /// A card for a current member with what the set knows. Reachability, version and harnesses
    /// come from presence, which the peer manager adds.
    pub fn card(&self, machine_id: &str, self_id: &str) -> Option<MachineCard> {
        let entry = self.entry(machine_id).filter(|entry| entry.removed.is_none())?;
        let is_self = machine_id == self_id;
        let status = if entry.paused {
            MachineStatus::Paused
        } else if is_self {
            MachineStatus::Online
        } else {
            MachineStatus::Offline
        };
        Some(MachineCard {
            machine_id: machine_id.to_string(),
            name: entry.descriptor.name.clone(),
            os: entry.descriptor.os.clone(),
            arch: None,
            ts_name: entry.descriptor.ts_name.clone(),
            signing_key: Some(entry.descriptor.signing_key.clone()),
            status,
            is_self,
            version: None,
            network_protocol: 0,
            features: Vec::new(),
            last_seen_at: None,
            relay_via: None,
            harnesses: Vec::new(),
            allow: self.allowed(machine_id),
            added_by: entry.added_by.clone(),
            added_via: entry.added_via,
            added_at: Some(entry.added_at),
        })
    }
}

/// The register that revokes a client device on every member.
pub fn revoked_device_register(device_id: &str) -> String {
    format!("device.{device_id}.revoked")
}

/// The device a `device.<id>.revoked` key names. Device ids may hold dots, so the id is everything
/// between the prefix and the suffix.
fn revoked_device_key(key: &str) -> Option<&str> {
    key.strip_prefix("device.")?.strip_suffix(".revoked").filter(|device| !device.is_empty())
}

/// Whether a register's key and signer are a pair the design allows. An invalid one is kept in the
/// set, since every build must keep the same operations, and has no effect.
pub fn register_is_valid(key: &str, signer: &str) -> bool {
    let mut parts = key.splitn(3, '.');
    match (parts.next(), parts.next(), parts.next()) {
        // `allow.<machine>.<capability>` is that machine's own policy, signed by its own key (§4.1).
        (Some("allow"), Some(owner), Some(capability)) => owner == signer && !capability.is_empty(),
        (Some("allow"), _, _) => false,
        // `machine.<id>.url` is where that machine says it can be reached from outside the tailnet (§3.5);
        // only the machine itself can say so. Names and the paused flag may be set by any member.
        (Some("machine"), Some(owner), Some("url")) => owner == signer,
        _ => true,
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn descriptor(id: &str, keys: &MachineKeys) -> MachineDescriptor {
        MachineDescriptor {
            machine_id: id.into(),
            name: id.into(),
            os: "linux".into(),
            signing_key: keys.signing_public_b64(),
            sealing_key: keys.sealing_public_b64(),
            ts_node_id: Some(format!("node-{id}")),
            ts_name: None,
            port: None,
        }
    }

    fn genesis(keys: &MachineKeys, id: &str, network: &str) -> OpEnvelope {
        let kind =
            OpKind::Genesis { owner_login: "owner@example.com".into(), tailnet: "tail.ts.net".into(), machine: descriptor(id, keys) };
        seal(keys, id, encode_body(network, Utc::now(), 1, &kind).unwrap())
    }

    #[test]
    fn id_is_the_hash_of_the_body_and_the_signature_covers_the_domain() {
        let keys = MachineKeys::generate();
        let envelope = genesis(&keys, "a", "net");
        assert_eq!(envelope.id, op_id(&envelope.body));
        assert_eq!(envelope.id.len(), 64);
        let signature = B64.decode(&envelope.signature).unwrap();
        keys::verify_signature(&keys.signing_public_b64(), &signing_input(&envelope.body), &signature).unwrap();
        // Without the domain separator the same signature does not verify.
        assert!(keys::verify_signature(&keys.signing_public_b64(), envelope.body.as_bytes(), &signature).is_err());
        assert!(signing_input("x").starts_with(b"kybern-network-op-v1\0"));
    }

    #[test]
    fn the_signed_text_is_kept_exactly_and_parses_back() {
        let keys = MachineKeys::generate();
        let envelope = genesis(&keys, "a", "net");
        let parsed = parse(&envelope).unwrap();
        assert_eq!(parsed.network_id, "net");
        assert_eq!(parsed.kind, "genesis");
        assert_eq!(parsed.lamport, 1);
        assert!(matches!(parsed.op, OpKind::Genesis { .. }));
        // Odd spacing and key order survive: the id and signature cover the text as written.
        let value: Value = serde_json::from_str(&envelope.body).unwrap();
        let respaced = serde_json::to_string_pretty(&value).unwrap();
        assert_ne!(respaced, envelope.body);
        let reformatted = OpEnvelope { body: respaced, ..envelope.clone() };
        assert_eq!(parse(&reformatted).unwrap_err(), OpError::IdMismatch);
    }

    #[test]
    fn unknown_kinds_and_newer_versions_parse_as_unknown_but_known_kinds_must_be_well_formed() {
        let keys = MachineKeys::generate();
        let forge = |body: Value| seal(&keys, "a", body.to_string());
        let at = Utc::now().to_rfc3339();
        let unknown = parse(&forge(json!({"v":1,"network_id":"net","kind":"frobnicate","at":at,"lamport":3,"x":[1,2]}))).unwrap();
        assert_eq!((unknown.kind.as_str(), &unknown.op), ("frobnicate", &OpKind::Unknown));
        let future = parse(&forge(json!({"v":2,"network_id":"net","kind":"add","at":at,"lamport":3,"shape":"new"}))).unwrap();
        assert_eq!(future.op, OpKind::Unknown, "a newer body version is kept, not interpreted");
        let broken = parse(&forge(json!({"v":1,"network_id":"net","kind":"add","at":at,"lamport":3})));
        assert!(matches!(broken, Err(OpError::Malformed(_))));
        for body in [json!([1]), json!({"v":1}), json!({"v":1,"network_id":"","kind":"set","at":at,"lamport":1})] {
            assert!(matches!(parse(&forge(body)), Err(OpError::Malformed(_))));
        }
        assert!(encode_body("net", Utc::now(), 1, &OpKind::Unknown).is_err());
    }

    #[test]
    fn the_digest_is_over_the_sorted_ids() {
        let a = vec!["bb".to_string(), "aa".to_string()];
        let b = vec!["aa".to_string(), "bb".to_string()];
        assert_eq!(digest_of(&a), digest_of(&b));
        assert_ne!(digest_of(&a), digest_of(&["aa".to_string()]));
        assert_eq!(digest_of(&[]), hex(&Sha256::digest(b"")));
    }

    #[test]
    fn register_keys_are_checked_for_their_owner() {
        assert!(register_is_valid("allow.m1.files", "m1"));
        assert!(!register_is_valid("allow.m1.files", "m2"));
        assert!(!register_is_valid("allow.m1", "m1"));
        assert!(!register_is_valid("allow", "m1"));
        assert!(register_is_valid("machine.m1.name", "m2"));
        assert!(register_is_valid("broker", "m2"));
        assert!(register_is_valid("something.new", "m2"));
        assert!(register_is_valid("machine.m1.url", "m1"));
        assert!(!register_is_valid("machine.m1.url", "m2"), "only a machine says where it can be reached");
        assert!(register_is_valid("machine.m1.paused", "m2"));
    }

    #[test]
    fn a_lone_set_keeps_nothing_until_it_joins() {
        let keys = MachineKeys::generate();
        let envelope = genesis(&keys, "a", "net");
        let parsed = parse(&envelope).unwrap();
        let mut set = OpSet::default();
        let refused = set.apply(vec![parsed.clone()], false);
        assert_eq!(refused.rejected, vec![Rejection { id: envelope.id.clone(), reason: OpError::NoNetwork }]);
        assert!(set.is_empty());
        let joined = set.apply(vec![parsed], true);
        assert_eq!(joined.accepted, vec![envelope.id]);
        assert_eq!(set.network_id(), Some("net"));
    }

    #[test]
    fn a_revoked_device_stays_revoked_whatever_is_written_after() {
        let keys = MachineKeys::generate();
        let mut set = OpSet::default();
        set.apply(vec![parse(&genesis(&keys, "a", "net")).unwrap()], true);
        let write = |lamport: u64, key: &str, value: Value| {
            let kind = OpKind::Set { key: key.into(), value };
            parse(&seal(&keys, "a", encode_body("net", Utc::now(), lamport, &kind).unwrap())).unwrap()
        };
        let register = revoked_device_register("phone:1.a");
        set.apply(
            vec![write(2, &register, json!(true)), write(3, &register, json!(false)), write(4, "device..revoked", json!(true))],
            false,
        );
        let projection = set.project();
        assert!(projection.is_device_revoked("phone:1.a"), "a later write does not bring a revoked device back");
        assert_eq!(projection.revoked_devices.len(), 1, "an empty device id names nothing");
        assert!(!projection.is_device_revoked("phone"));
    }

    // ADE-120: a machine that was offline while a later member removed it can verify the proof alone.
    #[test]
    fn a_removal_proof_holds_the_removal_and_its_signer_chain_and_nothing_else() {
        let keys: BTreeMap<&str, MachineKeys> = ["a", "b", "c", "d", "e"].into_iter().map(|id| (id, MachineKeys::generate())).collect();
        let sign =
            |signer: &str, lamport: u64, kind: OpKind| seal(&keys[signer], signer, encode_body("net", Utc::now(), lamport, &kind).unwrap());
        let add =
            |by: &str, lamport: u64, id: &str| sign(by, lamport, OpKind::Add { machine: descriptor(id, &keys[id]), via: JoinVia::Auto });
        let genesis = genesis(&keys["a"], "a", "net");
        let (add_c, add_b) = (add("a", 2, "c"), add("a", 3, "b"));
        // What b held when it went offline.
        let offline_view = [genesis.clone(), add_c.clone(), add_b.clone()];
        // While b was away: c added d and e, wrote a register, and d removed b and e.
        let (add_d, add_e) = (add("c", 4, "d"), add("c", 5, "e"));
        let register = sign("c", 6, OpKind::Set { key: "machine.c.name".into(), value: json!("Studio") });
        let remove = |lamport: u64, id: &str| {
            sign("d", lamport, OpKind::Remove { machine_id: id.into(), ts_node_id: None, reason: RemoveReason::User })
        };
        let (remove_b, remove_e) = (remove(7, "b"), remove(8, "e"));
        let mut member = OpSet::default();
        let everything = [&offline_view[..], &[add_d.clone(), add_e, register, remove_b.clone(), remove_e][..]].concat();
        member.apply(everything.iter().map(|op| parse(op).unwrap()).collect(), true);

        let proof = member.removal_proof("b");
        let ids: Vec<&str> = proof.iter().map(|op| op.id.as_str()).collect();
        assert_eq!(ids, vec![genesis.id.as_str(), add_c.id.as_str(), add_d.id.as_str(), remove_b.id.as_str()], "oldest first");
        assert!(member.removal_proof("c").is_empty(), "no Remove names c");

        // The removed machine verifies it from what it held: the chain makes d a signer it knows.
        let mut b = OpSet::default();
        b.apply(offline_view.iter().map(|op| parse(op).unwrap()).collect(), true);
        let mut alone = b.clone();
        let refused = alone.apply(vec![parse(&remove_b).unwrap()], false);
        assert!(refused.accepted.is_empty() && !alone.project().is_removed("b"), "without the chain, d is unknown");
        let applied = b.apply(proof.iter().map(|op| parse(op).unwrap()).collect(), false);
        assert!(applied.rejected.is_empty(), "{:?}", applied.rejected);
        assert!(b.project().is_removed("b"));
        assert!(!b.project().machines.contains_key("e"), "nothing about e reaches b");
    }
}
