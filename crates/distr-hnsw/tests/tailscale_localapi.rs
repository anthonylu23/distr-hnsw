//! The LocalAPI client and bind discipline against a fake LocalAPI on a
//! temporary Unix socket (docs/m2-implementation-plan.md, work package 1).
//! The fake (`tests/common`) mirrors the real responses checked on the
//! reference host.

mod common;

use std::{net::IpAddr, process::Command, time::Duration};

use axum::http::StatusCode;

use common::{socket_in, start_fake, Fake, FakePeer, FakeState};
use distr_hnsw::tailscale::{
    address_lost, resolve_bind, BindError, BindMode, BindSpec, LocalApi, LocalApiError,
};

#[tokio::test]
async fn status_and_whois_parse_and_unknown_peers_are_refused() {
    let dir = tempfile::tempdir().unwrap();
    let socket = socket_in(&dir);
    let fake = Fake::new(&["100.101.102.103", "fd7a:115c:a1e0::1"]);
    fake.set(|s| {
        s.peers.insert(
            "100.64.0.9".into(),
            FakePeer::new("nPEER", "laptop.example.ts.net.", "owner@example.com"),
        );
    });
    let _server = start_fake(&socket, fake);
    let api = LocalApi::new(&socket);

    let node = api.status().await.unwrap();
    assert!(node.running);
    assert_eq!(node.stable_id, "nSELF");
    assert_eq!(node.addresses.len(), 2);

    let peer = api
        .whois("100.64.0.9:41000".parse().unwrap())
        .await
        .unwrap();
    assert_eq!(peer.node_stable_id, "nPEER");
    assert_eq!(peer.user_login, "owner@example.com");
    assert!(peer.tags.is_empty());

    let unknown = "100.64.0.10:41000".parse().unwrap();
    assert!(matches!(
        api.whois(unknown).await,
        Err(LocalApiError::UnknownPeer(addr)) if addr == unknown
    ));
}

#[tokio::test]
async fn wildcards_and_unflagged_loopback_are_refused_without_contacting_tailscaled() {
    // No LocalAPI exists at this path; these decisions must not need one.
    let api = LocalApi::new("/nonexistent/tailscaled.sock");
    let wait = Duration::from_millis(50);
    for spec in ["0.0.0.0:7101", "[::]:7101"] {
        let spec: BindSpec = spec.parse().unwrap();
        assert!(matches!(
            resolve_bind(spec, true, &api, wait).await,
            Err(BindError::Wildcard(_))
        ));
    }
    let loopback: BindSpec = "127.0.0.1:7101".parse().unwrap();
    assert!(matches!(
        resolve_bind(loopback, false, &api, wait).await,
        Err(BindError::LoopbackNotAllowed(_))
    ));
    let (addr, mode) = resolve_bind(loopback, true, &api, wait).await.unwrap();
    assert_eq!(addr, "127.0.0.1:7101".parse().unwrap());
    assert_eq!(mode, BindMode::InsecureLoopback);
}

#[tokio::test]
async fn tailnet_binds_resolve_through_localapi_and_foreign_addresses_are_refused() {
    let dir = tempfile::tempdir().unwrap();
    let socket = socket_in(&dir);
    let _server = start_fake(
        &socket,
        Fake::new(&["fd7a:115c:a1e0::1", "100.101.102.103"]),
    );
    let api = LocalApi::new(&socket);
    let wait = Duration::from_secs(2);

    let (addr, mode) = resolve_bind("tailnet:7101".parse().unwrap(), false, &api, wait)
        .await
        .unwrap();
    assert_eq!(
        addr,
        "100.101.102.103:7101".parse().unwrap(),
        "first IPv4 address"
    );
    assert_eq!(mode, BindMode::Tailnet);

    let explicit = "[fd7a:115c:a1e0::1]:7101".parse().unwrap();
    assert_eq!(
        resolve_bind(explicit, false, &api, wait).await.unwrap(),
        (
            "[fd7a:115c:a1e0::1]:7101".parse().unwrap(),
            BindMode::Tailnet
        )
    );
    for foreign in ["192.168.1.20:7101", "100.101.102.104:7101"] {
        assert!(matches!(
            resolve_bind(foreign.parse().unwrap(), true, &api, wait).await,
            Err(BindError::NotTailnet(_))
        ));
    }
}

#[tokio::test]
async fn binding_waits_for_tailscaled_and_gives_up_after_the_deadline() {
    let dir = tempfile::tempdir().unwrap();
    let socket = socket_in(&dir);
    let api = LocalApi::new(&socket);
    let spec: BindSpec = "tailnet:7101".parse().unwrap();

    // No socket at all: not ready within the wait.
    assert!(matches!(
        resolve_bind(spec, false, &api, Duration::from_millis(300)).await,
        Err(BindError::NotReady(..))
    ));

    // tailscaled starts, logged out, then comes up during the wait.
    let fake = Fake::new(&["100.101.102.103"]);
    fake.set(|s| s.backend_state = "Starting".into());
    let _server = start_fake(&socket, fake.clone());
    let waiting = tokio::spawn({
        let api = api.clone();
        async move { resolve_bind(spec, false, &api, Duration::from_secs(10)).await }
    });
    tokio::time::sleep(Duration::from_millis(400)).await;
    assert!(
        !waiting.is_finished(),
        "must not bind while tailscaled is starting"
    );
    fake.set(|s| s.backend_state = "Running".into());
    let (addr, _) = waiting.await.unwrap().unwrap();
    assert_eq!(addr, "100.101.102.103:7101".parse().unwrap());
}

#[tokio::test]
async fn the_watcher_fires_on_an_address_change_but_not_while_tailscaled_restarts() {
    let dir = tempfile::tempdir().unwrap();
    let socket = socket_in(&dir);
    let fake = Fake::new(&["100.101.102.103"]);
    let server = start_fake(&socket, fake.clone());
    let api = LocalApi::new(&socket);
    let bound: IpAddr = "100.101.102.103".parse().unwrap();
    let poll = Duration::from_millis(20);
    let watcher = tokio::spawn(address_lost(api, bound, poll));

    // tailscaled goes away (restart) and comes back stopped: neither is a
    // confirmed change of address.
    server.abort();
    let _ = server.await;
    std::fs::remove_file(&socket).unwrap();
    tokio::time::sleep(Duration::from_millis(150)).await;
    fake.set(|s| {
        s.backend_state = "Stopped".into();
        s.addresses.clear();
    });
    let _server = start_fake(&socket, fake.clone());
    tokio::time::sleep(Duration::from_millis(150)).await;
    assert!(!watcher.is_finished());

    // Running again with a different address: the watcher fires.
    fake.set(|s| {
        s.backend_state = "Running".into();
        s.addresses = vec!["100.101.102.104".parse().unwrap()];
    });
    let lost = tokio::time::timeout(Duration::from_secs(2), watcher)
        .await
        .expect("watcher fired")
        .unwrap();
    assert_eq!(lost, bound);
}

#[test]
fn the_agent_binary_refuses_wildcard_and_unflagged_loopback_binds() {
    let dir = tempfile::tempdir().unwrap();
    let run = |bind: &str, extra: &[&str]| {
        let output = Command::new(env!("CARGO_BIN_EXE_distr-hnsw"))
            .args([
                "agent",
                "--id",
                "a",
                "--failure-domain",
                "d",
                "--bind",
                bind,
            ])
            .args(extra)
            .arg("--volume")
            .arg(dir.path().join("volume"))
            .args(["--tailnet-wait-seconds", "1"])
            .env(
                "DISTR_HNSW_TAILSCALE_SOCKET",
                dir.path().join("absent.sock"),
            )
            .env("DISTR_HNSW_JOIN_TOKEN", "unused")
            .output()
            .unwrap();
        assert!(!output.status.success(), "{bind} must not serve");
        String::from_utf8_lossy(&output.stderr).into_owned()
    };
    assert!(run("0.0.0.0:7101", &["--insecure-loopback"]).contains("wildcard"));
    assert!(run("127.0.0.1:7101", &[]).contains("--insecure-loopback"));
    assert!(run("tailnet:7101", &[]).contains("not ready"));
}

#[test]
fn an_unjoined_agent_without_a_join_token_refuses_to_start() {
    let dir = tempfile::tempdir().unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_distr-hnsw"))
        .args(["agent", "--id", "a", "--failure-domain", "d"])
        .args(["--bind", "127.0.0.1:0", "--insecure-loopback", "--volume"])
        .arg(dir.path().join("volume"))
        .env_remove("DISTR_HNSW_JOIN_TOKEN")
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("not joined"));
}

struct Child(std::process::Child);

impl Drop for Child {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// A tailnet-bound agent serves only the node it pinned at join, resolved
/// with WhoIs on the socket address; identity headers change nothing.
#[tokio::test(flavor = "multi_thread")]
async fn a_tailnet_agent_serves_only_the_pinned_portal_node() {
    use distr_hnsw::{
        access,
        agent_client::{join_agent, AgentClient},
        crypto::MasterKey,
        metadata::Database,
        portal::AgentTarget,
    };

    let dir = tempfile::tempdir().unwrap();
    let socket = socket_in(&dir);
    // Loopback stands in for this node's tailnet address, and WhoIs on it
    // names whichever node the test chooses.
    let fake = Fake::new(&["127.0.0.1"]);
    let as_node = |stable_id: &str| {
        let stable_id = stable_id.to_owned();
        move |s: &mut FakeState| {
            s.peers.insert(
                "127.0.0.1".into(),
                FakePeer::new(&stable_id, "node.example.ts.net.", "owner@example.com"),
            );
        }
    };
    fake.set(as_node("nPORTAL"));
    let _server = start_fake(&socket, fake.clone());

    let key = MasterKey::from_bytes([4; 32]);
    let database_path = dir.path().join("portal.sqlite");
    let mut database = Database::open(&database_path).unwrap();
    database.bind_master_key_id(&key.key_id_hex()).unwrap();
    let token = access::create_join_token(
        &mut database,
        "a1",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64,
    )
    .unwrap();
    let port = std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let _agent = Child(
        Command::new(env!("CARGO_BIN_EXE_distr-hnsw"))
            .args(["agent", "--id", "a1", "--failure-domain", "d1"])
            .args(["--bind", &format!("tailnet:{port}"), "--volume"])
            .arg(dir.path().join("volume"))
            .env("DISTR_HNSW_TAILSCALE_SOCKET", &socket)
            .env("DISTR_HNSW_JOIN_TOKEN", &token)
            .spawn()
            .unwrap(),
    );
    let deadline = std::time::Instant::now() + Duration::from_secs(20);
    while tokio::net::TcpStream::connect(("127.0.0.1", port))
        .await
        .is_err()
    {
        assert!(std::time::Instant::now() < deadline, "agent did not start");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let agent = AgentTarget {
        id: "a1".into(),
        failure_domain: "d1".into(),
        base_url: format!("http://127.0.0.1:{port}"),
    };
    let outcome = join_agent(&mut database, &key, &agent, &token, &LocalApi::new(&socket))
        .await
        .unwrap();
    assert!(
        outcome.portal_node_pinned,
        "the agent pinned the joining node"
    );

    let client = AgentClient::open(&mut database, &key).unwrap();
    let health = client.health(&agent).await.unwrap();
    assert!(health.status().is_success());
    let body: serde_json::Value = health.json().await.unwrap();
    assert_eq!(body["bind_mode"], "tailnet");

    // Another node with a valid grant and forged identity headers.
    fake.set(as_node("nINTRUDER"));
    let response = client.health(&agent).await.unwrap();
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
    let forged = reqwest::Client::new()
        .get(format!("{}/v1/health", agent.base_url))
        .header("Tailscale-User-Login", "owner@example.com")
        .header("Tailscale-User-Name", "Owner")
        .header("X-Forwarded-For", "100.64.0.1")
        .send()
        .await
        .unwrap();
    assert!(!forged.status().is_success());

    fake.set(as_node("nPORTAL"));
    assert!(client.health(&agent).await.unwrap().status().is_success());
}
