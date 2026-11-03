//! ADE-120: a machine that was offline or unreachable when it was removed learns it from a member it reaches later.
//! The scenarios with the id order both ways and the login item are in `login_service/tests.rs`; these check what
//! the answer holds, who gets it, how often, and that a forgery changes nothing. Real daemons and sockets on the
//! fake tailnet of the multi-daemon harness.

use std::time::Duration;

use base64::Engine;
use base64::engine::general_purpose::STANDARD as B64;
use chrono::Utc;
use kybern_protocol::*;

use super::harness::*;
use super::keys::MachineKeys;
use super::session::{self, RemovalNotice, SessionError, SessionRequest};
use super::{MembershipError, ops, well_known};

async fn listen(node: &Node) {
    crate::exposure::apply_network(&node.state, true).await;
    assert!(node.state.network.listening_port().is_some(), "{} did not listen: {:?}", node.name, node.state.network.listen_info());
}

/// A session request from `from` to `to`, signed with `keys` (the sender's own unless a test forges one).
fn request(from: &Node, to: &Node, keys: &MachineKeys, asks_for_proof: bool) -> SessionRequest {
    let mut request = SessionRequest {
        from: from.id(),
        to: to.id(),
        nonce_a: well_known::new_nonce(),
        at: well_known::now_rfc3339(),
        scopes: vec![Scope::MachinePeer.as_str().to_string()],
        renews: None,
        sig: String::new(),
        features: if asks_for_proof { vec![features::REMOVAL_PROOF_V1.to_string()] } else { vec![] },
    };
    request.sig = B64.encode(keys.sign(&session::request_message(&request)));
    request
}

/// POST the request to `to`'s session endpoint: the status, and the removal notice when the body is one.
async fn post(from: &Node, to: &Node, request: &SessionRequest) -> (u16, Option<RemovalNotice>, String) {
    let response = from
        .state
        .network
        .http
        .post(format!("{}{}", to.network_base(DEFAULT_PORT), session::SESSION_PATH))
        .json(request)
        .send()
        .await
        .unwrap();
    let status = response.status().as_u16();
    let body = response.text().await.unwrap();
    (status, serde_json::from_str(&body).ok(), body)
}

/// `a` founds the network and adds `b` and `c`; no worker runs, so nothing but the test reaches anyone.
async fn trio(cluster: &Cluster) -> (Node, Node, Node) {
    let a = cluster.start("a", false).await;
    let b = cluster.start("b", false).await;
    let c = cluster.start("c", false).await;
    cluster.form(&a, &[&b, &c], JoinVia::Auto).await;
    listen(&a).await;
    (a, b, c)
}

// (d) The handshake of a removed machine is answered with its own removal and the chain that verifies it, nothing
// else; an older caller, a stranger and a forger get the plain refusal they always got.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_removed_machine_is_answered_with_its_own_removal_only() {
    let specs = [NodeSpec::owner("a"), NodeSpec::owner("b"), NodeSpec::owner("c"), NodeSpec::owner("lone")];
    let Some(cluster) = Cluster::new(&specs) else { return };
    let (a, b, c) = trio(&cluster).await;
    a.membership().set(&format!("machine.{}.name", c.id()), serde_json::json!("Studio")).unwrap();
    a.membership().remove(&b.id(), RemoveReason::User).unwrap();
    a.membership().remove(&c.id(), RemoveReason::Stolen).unwrap();

    let (status, notice, body) = post(&b, &a, &request(&b, &a, &b.membership().keys(), true)).await;
    assert_eq!(status, 403, "{body}");
    let notice = notice.unwrap_or_else(|| panic!("not a removal notice: {body}"));
    let parsed: Vec<ops::ParsedOp> = notice.removed.iter().map(|op| ops::parse(op).unwrap()).collect();
    let removals: Vec<&str> = parsed
        .iter()
        .filter_map(|op| match &op.op {
            OpKind::Remove { machine_id, .. } => Some(machine_id.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(removals, vec![b.id().as_str()], "only the operation that removed b");
    // The rest is the chain that makes its signer valid: a's genesis. No register, no Add, nothing about c.
    assert_eq!(parsed.len(), 2, "{:?}", parsed.iter().map(|op| &op.kind).collect::<Vec<_>>());
    assert!(matches!(&parsed[0].op, OpKind::Genesis { machine, .. } if machine.machine_id == a.id()));
    assert!(!body.contains(&c.id()), "nothing about other members");
    // It is exactly what b needs.
    b.membership().apply_removal_proof(notice.removed).unwrap();
    assert!(b.membership().self_removed());

    // An older build does not ask, and gets the plain refusal, as before.
    let (status, notice, body) = post(&c, &a, &request(&c, &a, &c.membership().keys(), false)).await;
    assert_eq!((status, notice.is_none()), (403, true), "{body}");
    // A stranger that claims to be c, without c's key, learns nothing.
    let (status, notice, body) = post(&c, &a, &request(&c, &a, &MachineKeys::generate(), true)).await;
    assert_eq!((status, notice.is_none()), (403, true), "{body}");
    // Nor does a machine that was never a member.
    let lone = cluster.start("lone", false).await;
    let (status, notice, body) = post(&lone, &a, &request(&lone, &a, &lone.membership().keys(), true)).await;
    assert_eq!((status, notice.is_none()), (403, true), "{body}");
    // A replayed request is not answered twice.
    let fresh = request(&c, &a, &c.membership().keys(), true);
    assert!(post(&c, &a, &fresh).await.1.is_some());
    assert!(post(&c, &a, &fresh).await.1.is_none(), "a replayed request is not answered");
}

// (e) A removed machine that keeps retrying is told once per window and refused plainly in between.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_removed_machine_that_keeps_retrying_is_told_once_per_window() {
    let Some(cluster) = Cluster::new(&[NodeSpec::owner("a"), NodeSpec::owner("b")])
        .map(|cluster| cluster.timing(|timing| timing.removal_notice_every = Duration::from_millis(1500)))
    else {
        return;
    };
    let a = cluster.start("a", false).await;
    let b = cluster.start("b", false).await;
    cluster.form(&a, &[&b], JoinVia::Auto).await;
    listen(&a).await;
    a.membership().remove(&b.id(), RemoveReason::User).unwrap();

    let keys = b.membership().keys();
    assert!(post(&b, &a, &request(&b, &a, &keys, true)).await.1.is_some(), "told the first time");
    for _ in 0..5 {
        let (status, notice, _) = post(&b, &a, &request(&b, &a, &keys, true)).await;
        assert_eq!((status, notice.is_none()), (403, true), "refused plainly within the window");
    }
    tokio::time::sleep(Duration::from_millis(1600)).await;
    assert!(post(&b, &a, &request(&b, &a, &keys, true)).await.1.is_some(), "told again once the window passed");
}

// (c) A removal claim that does not verify against the set never makes a machine leave its network.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_forged_removal_claim_is_ignored() {
    let Some(cluster) = Cluster::new(&[NodeSpec::owner("a"), NodeSpec::owner("b"), NodeSpec::owner("x")]) else { return };
    let a = cluster.start("a", true).await;
    let b = cluster.start("b", true).await;
    let x = cluster.start("x", false).await;
    cluster.form(&a, &[&b], JoinVia::Auto).await;
    eventually("linked", WAIT, || linked(&a, &b)).await;
    let network_id = b.membership().network_id().unwrap();
    let remove_b = OpKind::Remove { machine_id: b.id(), ts_node_id: None, reason: RemoveReason::Stolen };
    let body = |lamport: u64| ops::encode_body(&network_id, Utc::now(), lamport, &remove_b).unwrap();
    let outsider = x.membership().keys();
    // Signed by a machine that was never added; signed by it in a member's name; and a member's real operation
    // with its body changed.
    let by_stranger = ops::seal(&outsider, &x.id(), body(50));
    let in_a_name = ops::seal(&outsider, &a.id(), body(51));
    let genuine = ops::seal(&a.membership().keys(), &a.id(), body(52));
    let altered = OpEnvelope { body: body(53), ..genuine.clone() };
    // x founds a network of its own and removes "b" there.
    x.membership().genesis(OWNER, TAILNET, &x.state.network.self_info().await).unwrap();
    let foreign =
        ops::seal(&outsider, &x.id(), ops::encode_body(&x.membership().network_id().unwrap(), Utc::now(), 54, &remove_b).unwrap());

    for forged in [vec![by_stranger], vec![in_a_name], vec![altered], vec![foreign]] {
        let refused = b.membership().apply_removal_proof(forged.clone());
        assert!(matches!(refused, Err(MembershipError::NotRemoved)), "{refused:?}");
        // The same claim in a session refusal from the machine b dialed changes nothing either.
        let notice = RemovalNotice { message: "this machine was removed from the network".into(), removed: forged };
        let answer = session::take_removal_notice(&b.state.network, &a.id(), notice);
        assert!(matches!(answer, SessionError::Refused { status: 403, .. }), "{answer:?}");
        assert!(b.membership().self_is_member() && !b.membership().self_removed());
    }
    assert!(linked(&a, &b), "b kept its network and its session");
    assert!(b.membership().projection().is_member(&b.id()));
}
