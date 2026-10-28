//! The LocalAPI client and bind discipline against a fake LocalAPI on a
//! temporary Unix socket (docs/m2-implementation-plan.md, work package 1).
//! The fake mirrors the real responses checked on the reference host: it
//! requires the `local-tailscaled.sock` host, answers `status` and `whois`,
//! and returns 404 for an address no peer owns.

use std::{
    collections::HashMap,
    net::{IpAddr, SocketAddr},
    path::{Path, PathBuf},
    process::Command,
    sync::{Arc, Mutex},
    time::Duration,
};

use axum::{
    extract::{Query, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::get,
    Json, Router,
};
use distr_hnsw::tailscale::{
    address_lost, resolve_bind, BindError, BindMode, BindSpec, LocalApi, LocalApiError,
};
use serde_json::json;
use tokio::{net::UnixListener, task::JoinHandle};

#[derive(Clone)]
struct FakeState {
    backend_state: String,
    stable_id: String,
    addresses: Vec<IpAddr>,
    peers: HashMap<String, (String, String, String)>,
}

#[derive(Clone)]
struct Fake(Arc<Mutex<FakeState>>);

impl Fake {
    fn new(addresses: &[&str]) -> Self {
        Self(Arc::new(Mutex::new(FakeState {
            backend_state: "Running".into(),
            stable_id: "nSELF".into(),
            addresses: addresses.iter().map(|a| a.parse().unwrap()).collect(),
            peers: HashMap::new(),
        })))
    }

    fn set(&self, f: impl FnOnce(&mut FakeState)) {
        f(&mut self.0.lock().unwrap());
    }
}

fn host_ok(headers: &HeaderMap) -> bool {
    headers
        .get("host")
        .is_some_and(|host| host == "local-tailscaled.sock")
}

async fn status(State(fake): State<Fake>, headers: HeaderMap) -> Response {
    if !host_ok(&headers) {
        return StatusCode::BAD_REQUEST.into_response();
    }
    let state = fake.0.lock().unwrap().clone();
    Json(json!({
        "BackendState": state.backend_state,
        "Self": { "ID": state.stable_id, "TailscaleIPs": state.addresses, "Online": true },
    }))
    .into_response()
}

async fn whois(
    State(fake): State<Fake>,
    headers: HeaderMap,
    Query(query): Query<HashMap<String, String>>,
) -> Response {
    if !host_ok(&headers) {
        return StatusCode::BAD_REQUEST.into_response();
    }
    let Some(addr) = query.get("addr") else {
        return StatusCode::BAD_REQUEST.into_response();
    };
    let ip = addr.parse::<SocketAddr>().map(|a| a.ip().to_string());
    let state = fake.0.lock().unwrap().clone();
    match ip.ok().and_then(|ip| state.peers.get(&ip).cloned()) {
        Some((stable_id, name, login)) => Json(json!({
            "Node": { "ID": 7, "StableID": stable_id, "Name": name, "Tags": null },
            "UserProfile": { "ID": 9, "LoginName": login },
            "CapMap": {},
        }))
        .into_response(),
        None => StatusCode::NOT_FOUND.into_response(),
    }
}

fn start_fake(socket: &Path, fake: Fake) -> JoinHandle<()> {
    let listener = UnixListener::bind(socket).unwrap();
    let router = Router::new()
        .route("/localapi/v0/status", get(status))
        .route("/localapi/v0/whois", get(whois))
        .with_state(fake);
    tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap();
    })
}

fn socket_in(dir: &tempfile::TempDir) -> PathBuf {
    dir.path().join("tailscaled.sock")
}

#[tokio::test]
async fn status_and_whois_parse_and_unknown_peers_are_refused() {
    let dir = tempfile::tempdir().unwrap();
    let socket = socket_in(&dir);
    let fake = Fake::new(&["100.101.102.103", "fd7a:115c:a1e0::1"]);
    fake.set(|s| {
        s.peers.insert(
            "100.64.0.9".into(),
            (
                "nPEER".into(),
                "laptop.example.ts.net.".into(),
                "owner@example.com".into(),
            ),
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
fn the_agent_binary_refuses_wildcard_unflagged_loopback_and_unguarded_tailnet_binds() {
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
fn the_agent_binary_does_not_serve_tailnet_traffic_before_grants_exist() {
    let dir = tempfile::tempdir().unwrap();
    let socket = socket_in(&dir);
    let runtime = tokio::runtime::Runtime::new().unwrap();
    // The fake reports a loopback address as this node's tailnet address so
    // the bind itself can succeed on any machine.
    let _context = runtime.enter();
    let _server = start_fake(&socket, Fake::new(&["127.0.0.1"]));
    let output = Command::new(env!("CARGO_BIN_EXE_distr-hnsw"))
        .args([
            "agent",
            "--id",
            "a",
            "--failure-domain",
            "d",
            "--bind",
            "tailnet:0",
        ])
        .arg("--volume")
        .arg(dir.path().join("volume"))
        .env("DISTR_HNSW_TAILSCALE_SOCKET", &socket)
        .output()
        .unwrap();
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("capability grants"), "{stderr}");
}
