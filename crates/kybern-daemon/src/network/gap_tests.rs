//! The gaps M1 left (ADE-90): repository identity on projects, `machines.info`, updating every
//! machine, following updates, and a member off the tailnet that dials out. Every scenario runs real
//! daemons over real sockets on the fake tailnet of the multi-daemon harness.

use std::sync::atomic::Ordering;
use std::time::Duration;

use chrono::Utc;
use kybern_client::{Client, Endpoint};
use kybern_protocol::methods::*;
use kybern_protocol::*;

use super::dial_out;
use super::harness::*;
use super::updates;

fn cluster(specs: &[NodeSpec]) -> Option<Cluster> {
    Cluster::new(specs).map(|cluster| cluster.auto_join(true))
}

async fn admin(node: &Node) -> Client {
    Client::connect(&Endpoint { url: node.ws_url(), token: node.device_token(&crate::access::paired_scopes()) }).await.unwrap()
}

fn converged(nodes: &[&Node], expected: usize) -> bool {
    let digest = nodes[0].membership().digest();
    nodes.iter().all(|node| {
        node.membership().projection().members().count() == expected
            && node.membership().digest() == digest
            && node.membership().has_network()
    })
}

/// `observer` has heard `machine` say who it is, so a presence a test plants is not overwritten by it.
async fn heard_from(observer: &Node, machine: &Node) {
    eventually(&format!("{} heard {}", observer.name, machine.name), WAIT, || {
        observer.state.network.peers.status(&machine.id()).presence.is_some()
    })
    .await;
}

async fn all_linked(nodes: &[&Node]) {
    for (index, first) in nodes.iter().enumerate() {
        for second in &nodes[index + 1..] {
            eventually(&format!("{} and {} linked", first.name, second.name), WAIT, || linked(first, second)).await;
        }
    }
}

/// What `owner` last heard `machine` say about itself.
fn presence(version: &str, features: &[&str]) -> PeerPresence {
    PeerPresence {
        version: version.into(),
        network_protocol: NETWORK_PROTOCOL,
        features: features.iter().map(|feature| feature.to_string()).collect(),
        harnesses: vec![],
        load: None,
        battery: None,
        at: Utc::now(),
    }
}

fn reason<'a>(result: &'a NetworkUpdateAllResult, machine: &str) -> Option<&'a str> {
    result.skipped.iter().find(|skipped| skipped.machine_id == machine).map(|skipped| skipped.reason.as_str())
}

fn git(dir: &std::path::Path, args: &[&str]) {
    let out = std::process::Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(["-c", "user.name=t", "-c", "user.email=t@example.com", "-c", "commit.gpgsign=false"])
        .args(args)
        .output()
        .unwrap();
    assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_new_project_gets_its_repository_identity_and_machines_info_reports_it() {
    let Some(cluster) = cluster(&[NodeSpec::owner("a")]) else { return };
    let a = cluster.start("a", false).await;
    let work = super::testutil::TempDir::new("gap-project");
    let repo = work.path().join("thing");
    std::fs::create_dir_all(&repo).unwrap();
    git(&repo, &["init", "-q", "-b", "main"]);
    std::fs::write(repo.join("f.txt"), "one").unwrap();
    git(&repo, &["add", "f.txt"]);
    git(&repo, &["commit", "-q", "-m", "one"]);
    git(&repo, &["remote", "add", "origin", "git@github.com:Acme/Thing.git"]);

    let client = admin(&a).await;
    let added = client
        .call::<ProjectsAdd>(ProjectsAddParams {
            path: repo.to_string_lossy().into_owned(),
            name: None,
            space_id: None,
            kind: None,
            members: None,
        })
        .await
        .unwrap();
    assert!(added.is_git);
    // The identity is read in the background, so the project is listed at once and gets it a moment later.
    eventually("the project has its repository identity", WAIT, || {
        a.state.store.project_get(added.id).unwrap().unwrap().repo.is_some_and(|repo| repo.root_commit.is_some())
    })
    .await;
    let listed = client.call::<ProjectsList>(ProjectsListParams { refresh_git: Some(false) }).await.unwrap();
    let identity = listed.projects.iter().find(|project| project.id == added.id).and_then(|project| project.repo.clone()).unwrap();
    assert_eq!(identity.remote.as_deref(), Some("github.com/acme/thing"));

    let info = client.call::<MachinesInfo>(Empty {}).await.unwrap();
    assert_eq!(info.projects, vec![identity], "the machine reports every repository once");
    assert!(info.cpu.as_ref().is_some_and(|cpu| cpu.logical_cores >= 1 && !cpu.model.is_empty()));
    assert!(info.mem.is_some_and(|mem| mem.total_bytes > 0));
    assert_eq!(info.harnesses.len(), ProviderKind::ALL.len(), "every harness is listed, installed or not");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn update_all_starts_the_machines_that_can_update_and_says_why_the_others_did_not() {
    let Some(cluster) = cluster(&[NodeSpec::owner("a"), NodeSpec::owner("b"), NodeSpec::owner("c"), NodeSpec::owner("d")]) else { return };
    let a = cluster.start("a", true).await;
    let b = cluster.start("b", true).await;
    let c = cluster.start("c", true).await;
    let d = cluster.start("d", true).await;
    eventually("one network", WAIT, || converged(&[&a, &b, &c, &d], 4)).await;
    all_linked(&[&a, &b, &c, &d]).await;

    // A test build is a development build, which never replaces itself; `a` and `c` may be updated here.
    a.state.daemon_updates.allow_development.store(true, Ordering::SeqCst);
    c.state.daemon_updates.allow_development.store(true, Ordering::SeqCst);
    // `d` is the desktop app's daemon with the app open: the app takes the update (ADE-125).
    d.state.desktop_managed.store(true, Ordering::SeqCst);
    *d.state.app_updates.test_install.lock().unwrap() =
        Some(crate::app_update::Detected::Install(crate::app_update::DesktopInstall::MacApp {
            bundle: "/nowhere/Kybern GPUIX.app".into(),
        }));
    let window = uuid::Uuid::now_v7();
    d.state.app_updates.attach(crate::app_update::Attached {
        connection: window,
        version: "0.0.0".into(),
        can_install: true,
        reason: None,
    });
    // As far as `a` knows, `b` already runs the newest version.
    heard_from(&a, &b).await;
    a.state.network.peers.set_presence(&b.id(), presence("99.0.0", &[features::UPDATE_V1]));

    let result = admin(&a).await.call::<NetworkUpdateAll>(Empty {}).await.unwrap();
    let mut started = result.started.clone();
    started.sort();
    let mut expected = vec![a.id(), c.id(), d.id()];
    expected.sort();
    assert_eq!(started, expected, "this machine, the member that can update and the desktop app's machine start: {result:?}");
    assert_eq!(reason(&result, &b.id()), Some("Already up to date"));
    assert_eq!(result.started.len() + result.skipped.len(), 4, "every member is listed once");

    // The asked machines queued their updates; the desktop app's machine asks its open app once nothing runs.
    eventually("c queued its update", WAIT, || c.state.daemon_updates.get().status == DaemonUpdateStatus::Waiting).await;
    assert_eq!(a.state.daemon_updates.get().status, DaemonUpdateStatus::Waiting);
    assert!(d.state.app_updates.pending().is_some(), "d's app update is queued");
    assert_eq!(crate::app_update::tick(&d.state).await, crate::app_update::Step::Handed(window), "d asks its open app");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn update_all_skips_offline_machines_and_ones_that_cannot_be_updated_from_here() {
    let Some(cluster) = cluster(&[NodeSpec::owner("a"), NodeSpec::owner("b"), NodeSpec::owner("c")]) else { return };
    let a = cluster.start("a", true).await;
    let b = cluster.start("b", true).await;
    let mut c = cluster.start("c", true).await;
    eventually("one network", WAIT, || converged(&[&a, &b, &c], 3)).await;
    all_linked(&[&a, &b, &c]).await;

    // `b` runs a Kybern from before updates could be asked for.
    heard_from(&a, &b).await;
    a.state.network.peers.set_presence(&b.id(), presence("0.1.0", &[features::NETWORK_V1]));
    c.stop();
    eventually("a loses c", WAIT, || a.state.network.peers.link(&c.id()).is_none()).await;

    let result = admin(&a).await.call::<NetworkUpdateAll>(Empty {}).await.unwrap();
    assert!(reason(&result, &b.id()).is_some_and(|reason| reason.contains("0.1.0") && reason.contains("its own Kybern")), "{result:?}");
    assert_eq!(reason(&result, &c.id()), Some("Offline"));
    assert!(result.started.is_empty(), "a development build starts nothing: {result:?}");
    assert!(reason(&result, &a.id()).is_some_and(|reason| reason.contains("Development build")));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_member_refuses_peer_update_start_when_its_owner_does_not_let_others_change_its_settings() {
    let Some(cluster) = cluster(&[NodeSpec::owner("a"), NodeSpec::owner("b")]) else { return };
    let a = cluster.start("a", true).await;
    let b = cluster.start("b", true).await;
    eventually("one network", WAIT, || converged(&[&a, &b], 2)).await;
    all_linked(&[&a, &b]).await;
    b.state.daemon_updates.allow_development.store(true, Ordering::SeqCst);
    heard_from(&a, &b).await;
    a.state.network.peers.set_presence(&b.id(), presence("0.1.0", &[features::UPDATE_V1]));

    // `b` turns the capability off for everyone.
    admin(&b)
        .await
        .call::<NetworkAllowSet>(NetworkAllowSetParams { machine_id: b.id(), capability: "settings".into(), allowed: false })
        .await
        .unwrap();
    eventually("a sees the policy", WAIT, || a.membership().projection().allowed(&b.id()).get("settings") == Some(&false)).await;
    // The session was granted with the scope before the policy changed; the target narrows it at use time.
    let result = admin(&a).await.call::<NetworkUpdateAll>(Empty {}).await.unwrap();
    assert_eq!(reason(&result, &b.id()), Some("It does not let other machines change its settings"), "{result:?}");
    assert_ne!(b.state.daemon_updates.get().status, DaemonUpdateStatus::Waiting);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn following_updates_is_a_network_setting_and_a_member_updates_itself_once_per_newer_version() {
    let Some(cluster) = cluster(&[NodeSpec::owner("a"), NodeSpec::owner("b")]) else { return };
    let a = cluster.start("a", true).await;
    let b = cluster.start("b", true).await;
    eventually("one network", WAIT, || converged(&[&a, &b], 2)).await;
    all_linked(&[&a, &b]).await;
    b.state.daemon_updates.allow_development.store(true, Ordering::SeqCst);

    let at_a = admin(&a).await;
    assert!(!at_a.call::<NetworkStatusMethod>(Empty {}).await.unwrap().follow_updates);
    // Off: a newer version elsewhere changes nothing.
    heard_from(&b, &a).await;
    b.state.network.peers.set_presence(&a.id(), presence("99.0.0", &[features::UPDATE_V1]));
    assert_eq!(updates::follow_tick(&b.state).await, None);

    let status = at_a.call::<NetworkFollowUpdatesSet>(NetworkFollowUpdatesParams { enabled: true }).await.unwrap();
    assert!(status.follow_updates);
    eventually("b sees the setting", WAIT, || b.membership().projection().follow_updates()).await;
    assert!(admin(&b).await.call::<NetworkStatusMethod>(Empty {}).await.unwrap().follow_updates, "every member reports it");

    assert_eq!(updates::follow_tick(&b.state).await.as_deref(), Some("99.0.0"), "b follows the newer version");
    assert_eq!(b.state.daemon_updates.get().status, DaemonUpdateStatus::Waiting);
    assert_eq!(updates::follow_tick(&b.state).await, None, "a version is tried once");
    // A member on the newest version has nothing to follow.
    assert_eq!(updates::follow_tick(&a.state).await, None);

    let off = at_a.call::<NetworkFollowUpdatesSet>(NetworkFollowUpdatesParams { enabled: false }).await.unwrap();
    assert!(!off.follow_updates);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_member_off_the_tailnet_dials_out_to_a_member_that_publishes_a_url() {
    let Some(mut cluster) = cluster(&[NodeSpec::owner("a"), NodeSpec::owner("b")]) else { return };
    // `b` is not on the tailnet at all: it has no Tailscale and no tailnet address.
    cluster.set_online("b", false);
    let a = cluster.start("a", true).await;
    let b = cluster.start_off_tailnet("b").await;
    // `a` can be reached from the internet at its advertised address.
    let url = a.http_url();
    *a.state.advertised_urls.write().unwrap() = vec![url.clone()];
    cluster.form(&a, &[], JoinVia::Auto).await;
    eventually("a publishes its URL", WAIT, || dial_out::url_of(&a.membership().projection(), &a.id()).as_deref() == Some(url.as_str()))
        .await;
    // `b` joins by code, as a machine outside the tailnet does; the set it is handed holds a's URL.
    cluster.enroll(&a, &b, JoinVia::Code).await;
    assert_eq!(b.membership().projection().entry(&b.id()).unwrap().descriptor.ts_node_id, None);

    eventually("b dials out and a accepts", WAIT, || linked(&a, &b)).await;
    let from_b = b.state.network.peers.link(&a.id()).unwrap();
    let at_a = a.state.network.peers.link(&b.id()).unwrap();
    assert_eq!(from_b.direction, super::peers::Direction::Dialed);
    assert_eq!(at_a.direction, super::peers::Direction::Accepted);

    // The one session works both ways: an operation a writes reaches b.
    admin(&a).await.call::<NetworkRename>(NetworkRenameParams { machine_id: b.id(), name: "offgrid".into() }).await.unwrap();
    eventually("b hears of its new name", WAIT, || b.membership().projection().entry(&b.id()).unwrap().descriptor.name == "offgrid").await;
    // And a calls b over the same link.
    assert!(at_a.call::<PeerPresenceMethod>(presence("0.19.0", &[])).await.is_ok());

    // A machine that stops being reachable takes its URL back, so nobody keeps dialing it.
    a.state.advertised_urls.write().unwrap().clear();
    a.state.network.wake();
    eventually("a clears its URL", Duration::from_secs(10), || dial_out::url_of(&a.membership().projection(), &a.id()).is_none()).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_member_tailscale_lists_offline_is_still_dialed_when_it_answers() {
    let Some(mut cluster) = cluster(&[NodeSpec::owner("a"), NodeSpec::owner("b")]) else { return };
    // Each side's Tailscale lists the other offline (as one that lost its coordination server is), but
    // both daemons are up, so whichever has the smaller id dials and the probe finds the other.
    cluster.list_offline_for("a", "b", true);
    cluster.list_offline_for("b", "a", true);
    let a = cluster.start("a", true).await;
    let b = cluster.start("b", true).await;
    cluster.form(&a, &[&b], JoinVia::Auto).await;
    eventually("they link although each is listed offline", WAIT, || linked(&a, &b)).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_machine_whose_own_tailscale_lost_its_coordination_server_still_dials() {
    let Some(mut cluster) = cluster(&[NodeSpec::owner("a"), NodeSpec::owner("b")]) else { return };
    let a = cluster.start("a", true).await;
    let b = cluster.start("b", true).await;
    cluster.form(&a, &[&b], JoinVia::Auto).await;
    eventually("they link", WAIT, || linked(&a, &b)).await;
    // Both lose their coordination server: "Running" but offline, so neither status says running and
    // each lists the other offline. The sessions drop; the machines still answer on their ports.
    cluster.set_online("a", false);
    cluster.set_online("b", false);
    let before = a.state.network.peers.link(&b.id()).unwrap();
    a.state.network.drop_session(&b.id());
    b.state.network.drop_session(&a.id());
    eventually("a new session opens without a running Tailscale", WAIT, || {
        linked(&a, &b) && a.state.network.peers.link(&b.id()).is_some_and(|link| !std::sync::Arc::ptr_eq(&link, &before))
    })
    .await;
}

#[test]
fn only_a_machine_can_say_where_it_is_reached_and_only_an_https_address_carries_a_session() {
    // Another member cannot point traffic for `a` somewhere else.
    assert!(super::ops::register_is_valid(&dial_out::url_key("a"), "a"));
    assert!(!super::ops::register_is_valid(&dial_out::url_key("a"), "b"));
    // A session crosses the internet: https only, and plain loopback under the fake tailnet.
    assert!(super::session::url_allowed("https://box.example.com", false));
    assert!(!super::session::url_allowed("http://box.example.com", false));
    assert!(!super::session::url_allowed("http://127.0.0.1:4173", false));
    assert!(super::session::url_allowed("http://127.0.0.1:4173", true));
    assert!(!super::session::url_allowed("http://box.example.com", true));
    assert!(!super::session::url_allowed("ftp://box.example.com", true));
    assert!(!super::session::url_allowed("not a url", true));
}
