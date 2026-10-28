//! Shared test doubles. The fake LocalAPI mirrors the real responses checked
//! on the reference host: it requires the `local-tailscaled.sock` host,
//! answers `status` (with `CertDomains`), `whois` (404 for an address no
//! peer owns), and `cert/<domain>?type=pair` (PEM key, then certificates).
#![allow(dead_code)]

use std::{
    collections::HashMap,
    net::{IpAddr, SocketAddr},
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};

use axum::{
    extract::{Path as UrlPath, Query, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::get,
    Json, Router,
};
use serde_json::json;
use tokio::{net::UnixListener, task::JoinHandle};

/// A tailnet node as WhoIs reports it.
#[derive(Clone, Debug)]
pub struct FakePeer {
    pub stable_id: String,
    pub name: String,
    pub login: String,
    pub tags: Vec<String>,
}

impl FakePeer {
    pub fn new(stable_id: &str, name: &str, login: &str) -> Self {
        Self {
            stable_id: stable_id.into(),
            name: name.into(),
            login: login.into(),
            tags: Vec::new(),
        }
    }

    /// A tagged node: WhoIs names the shared `tagged-devices` user.
    pub fn tagged(stable_id: &str, name: &str, tags: &[&str]) -> Self {
        Self {
            stable_id: stable_id.into(),
            name: name.into(),
            login: "tagged-devices".into(),
            tags: tags.iter().map(|tag| (*tag).into()).collect(),
        }
    }
}

/// What `cert/<domain>?type=pair` answers.
#[derive(Clone, Debug)]
pub enum CertAnswer {
    Pair(String),
    Status(u16),
}

#[derive(Clone)]
pub struct FakeState {
    pub backend_state: String,
    pub stable_id: String,
    pub addresses: Vec<IpAddr>,
    pub cert_domains: Vec<String>,
    /// Keyed by peer IP address.
    pub peers: HashMap<String, FakePeer>,
    pub cert: CertAnswer,
    /// Domains requested from the cert endpoint, in order.
    pub cert_requests: Vec<String>,
}

#[derive(Clone)]
pub struct Fake(pub Arc<Mutex<FakeState>>);

impl Fake {
    pub fn new(addresses: &[&str]) -> Self {
        Self(Arc::new(Mutex::new(FakeState {
            backend_state: "Running".into(),
            stable_id: "nSELF".into(),
            addresses: addresses.iter().map(|a| a.parse().unwrap()).collect(),
            cert_domains: Vec::new(),
            peers: HashMap::new(),
            cert: CertAnswer::Status(403),
            cert_requests: Vec::new(),
        })))
    }

    pub fn set(&self, f: impl FnOnce(&mut FakeState)) {
        f(&mut self.0.lock().unwrap());
    }

    pub fn get<T>(&self, f: impl FnOnce(&FakeState) -> T) -> T {
        f(&self.0.lock().unwrap())
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
        "CertDomains": if state.cert_domains.is_empty() { None } else { Some(state.cert_domains) },
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
        Some(peer) => Json(json!({
            "Node": {
                "ID": 7,
                "StableID": peer.stable_id,
                "Name": peer.name,
                "Tags": if peer.tags.is_empty() { None } else { Some(peer.tags) },
            },
            "UserProfile": { "ID": 9, "LoginName": peer.login },
            "CapMap": {},
        }))
        .into_response(),
        None => StatusCode::NOT_FOUND.into_response(),
    }
}

async fn cert(
    State(fake): State<Fake>,
    headers: HeaderMap,
    UrlPath(domain): UrlPath<String>,
    Query(query): Query<HashMap<String, String>>,
) -> Response {
    if !host_ok(&headers) || query.get("type").map(String::as_str) != Some("pair") {
        return StatusCode::BAD_REQUEST.into_response();
    }
    let mut state = fake.0.lock().unwrap();
    state.cert_requests.push(domain.clone());
    if !state.cert_domains.contains(&domain) {
        return StatusCode::BAD_REQUEST.into_response();
    }
    match &state.cert {
        CertAnswer::Pair(pem) => pem.clone().into_response(),
        CertAnswer::Status(status) => StatusCode::from_u16(*status).unwrap().into_response(),
    }
}

pub fn start_fake(socket: &Path, fake: Fake) -> JoinHandle<()> {
    let listener = UnixListener::bind(socket).unwrap();
    let router = Router::new()
        .route("/localapi/v0/status", get(status))
        .route("/localapi/v0/whois", get(whois))
        .route("/localapi/v0/cert/{domain}", get(cert))
        .with_state(fake);
    tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap();
    })
}

pub fn socket_in(dir: &tempfile::TempDir) -> PathBuf {
    dir.path().join("tailscaled.sock")
}

/// A throwaway certificate authority for portal certificates.
pub struct TestCa {
    params: rcgen::CertificateParams,
    key: rcgen::KeyPair,
    /// The CA certificate, for clients to trust.
    pub pem: String,
}

impl TestCa {
    pub fn new() -> Self {
        let key = rcgen::KeyPair::generate().unwrap();
        let mut params = rcgen::CertificateParams::new(Vec::<String>::new()).unwrap();
        params.is_ca = rcgen::IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
        params
            .distinguished_name
            .push(rcgen::DnType::CommonName, "distr-hnsw test CA");
        params.key_usages = vec![
            rcgen::KeyUsagePurpose::KeyCertSign,
            rcgen::KeyUsagePurpose::DigitalSignature,
        ];
        let pem = params.self_signed(&key).unwrap().pem();
        Self { params, key, pem }
    }

    /// LocalAPI's pair format for `domain`, valid between the two Unix
    /// times; also returns the leaf's DER.
    pub fn pair(&self, domain: &str, not_before: i64, not_after: i64) -> (String, Vec<u8>) {
        let key = rcgen::KeyPair::generate().unwrap();
        let mut params = rcgen::CertificateParams::new(vec![domain.to_owned()]).unwrap();
        params.not_before = time::OffsetDateTime::from_unix_timestamp(not_before).unwrap();
        params.not_after = time::OffsetDateTime::from_unix_timestamp(not_after).unwrap();
        let issuer = rcgen::Issuer::from_params(&self.params, &self.key);
        let leaf = params.signed_by(&key, &issuer).unwrap();
        (
            format!("{}{}", key.serialize_pem(), leaf.pem()),
            leaf.der().to_vec(),
        )
    }
}

pub fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64
}
