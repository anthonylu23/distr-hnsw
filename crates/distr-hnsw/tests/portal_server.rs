//! The portal daemon against a fake LocalAPI (M2 work packages 4 to 6;
//! docs/m2-implementation-plan.md): identity comes only from WhoIs on the
//! connection and forged identity headers grant nothing; sessions are bound
//! to their node and user, rotate, expire, and need CSRF tokens; API keys
//! are stored as verifiers and enforce collection scopes; certificates
//! renew and hot-reload, and an expired certificate refuses TLS instead of
//! falling back to plaintext; the portal binds only its tailnet address.
//!
//! Loopback aliases stand in for tailnet nodes: the portal binds 127.0.0.1
//! (the fake reports it as this node's tailnet address) and clients connect
//! from 127.0.0.2 and up, which WhoIs names. Linux routes all of 127/8 to
//! loopback; where aliases are unavailable these tests skip.

mod common;

use std::{
    io::Read,
    net::{IpAddr, SocketAddr},
    path::PathBuf,
    process::{Command, Stdio},
    time::Duration,
};

use common::{now, socket_in, start_fake, CertAnswer, Fake, FakePeer, TestCa};
use distr_hnsw::{
    access,
    agent::{serve_agent, AgentIdentity},
    agent_client::join_agent,
    auth::{ApiKeySecret, SessionToken},
    crypto::MasterKey,
    metadata::{Database, SessionRow},
    portal::AgentTarget,
    portal_server::{serve_portal, PortalServeConfig, ServeLimits},
    tailscale::LocalApi,
    tls::RenewalPolicy,
};
use reqwest::StatusCode;
use serde_json::{json, Value};
use tokio::{net::TcpListener, task::JoinHandle};
use uuid::Uuid;

const DOMAIN: &str = "portal.example.ts.net";
/// owner@example.com on node nLAPTOP.
const LAPTOP: &str = "127.0.0.2";
/// mallory@example.com on node nINTRUDER, a tailnet peer with no account.
const INTRUDER: &str = "127.0.0.3";
/// A tagged server node (no user).
const SERVER: &str = "127.0.0.4";
/// owner@example.com on a second node, nPHONE.
const PHONE: &str = "127.0.0.5";
/// An address WhoIs cannot name.
const STRANGER: &str = "127.0.0.6";
/// owner@example.com on the portal's own node, nSELF.
const PORTAL_NODE: &str = "127.0.0.7";

fn loopback_aliases() -> bool {
    let available = std::net::TcpListener::bind("127.0.0.2:0").is_ok();
    if !available {
        eprintln!("skipped: loopback aliases (127.0.0.2) are unavailable on this host");
    }
    available
}

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

fn fast_renewal() -> RenewalPolicy {
    RenewalPolicy {
        retry_min: Duration::from_millis(100),
        retry_max: Duration::from_millis(400),
        refresh: Duration::from_secs(3600),
    }
}

/// Two joined loopback agents in separate failure domains.
async fn start_agents(
    dir: &std::path::Path,
    database: &mut Database,
    key: &MasterKey,
) -> Vec<AgentTarget> {
    let mut targets = Vec::new();
    for (id, domain) in [("a1", "d1"), ("a2", "d2")] {
        let token = access::create_join_token(database, id, now()).unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let target = AgentTarget {
            id: id.into(),
            failure_domain: domain.into(),
            base_url: format!("http://{}", listener.local_addr().unwrap()),
        };
        let volume = dir.join(id);
        let identity = AgentIdentity {
            id: id.into(),
            failure_domain: domain.into(),
        };
        let agent_token = token.clone();
        tokio::spawn(async move {
            serve_agent(listener, volume, identity, &agent_token)
                .await
                .unwrap();
        });
        join_agent(
            database,
            key,
            &target,
            &token,
            &LocalApi::new("/nonexistent/tailscaled.sock"),
        )
        .await
        .unwrap();
        targets.push(target);
    }
    targets
}

struct Harness {
    dir: tempfile::TempDir,
    fake: Fake,
    ca: TestCa,
    database_path: PathBuf,
    port: u16,
    /// When the portal was started; its first certificate's validity is
    /// relative to this.
    started_at: i64,
    initial_certificate: Vec<u8>,
    server: JoinHandle<anyhow::Result<()>>,
    _localapi: JoinHandle<()>,
}

impl Harness {
    /// A portal with a certificate valid for a day.
    async fn start() -> Self {
        Self::start_with((-60, 86_400), fast_renewal()).await
    }

    fn test_limits() -> ServeLimits {
        ServeLimits {
            max_upload_bytes: 8 << 20,
            spool_floor_bytes: 0,
            ..ServeLimits::default()
        }
    }

    async fn start_with(validity: (i64, i64), renewal: RenewalPolicy) -> Self {
        Self::start_with_limits(validity, renewal, Self::test_limits()).await
    }

    /// `validity` is the first certificate's window in seconds relative to
    /// the moment the portal starts.
    async fn start_with_limits(
        validity: (i64, i64),
        renewal: RenewalPolicy,
        limits: ServeLimits,
    ) -> Self {
        let ca = TestCa::new();
        let dir = tempfile::tempdir().unwrap();
        let socket = socket_in(&dir);
        let fake = Fake::new(&["127.0.0.1"]);
        fake.set(|s| {
            s.cert_domains = vec![DOMAIN.into()];
            for (ip, peer) in [
                (
                    LAPTOP,
                    FakePeer::new("nLAPTOP", "laptop.example.ts.net.", "owner@example.com"),
                ),
                (
                    INTRUDER,
                    FakePeer::new(
                        "nINTRUDER",
                        "intruder.example.ts.net.",
                        "mallory@example.com",
                    ),
                ),
                (
                    SERVER,
                    FakePeer::tagged("nSERVER", "server.example.ts.net.", &["tag:server"]),
                ),
                (
                    PHONE,
                    FakePeer::new("nPHONE", "phone.example.ts.net.", "owner@example.com"),
                ),
                (
                    PORTAL_NODE,
                    FakePeer::new("nSELF", "portal.example.ts.net.", "owner@example.com"),
                ),
            ] {
                s.peers.insert(ip.into(), peer);
            }
        });
        let localapi = start_fake(&socket, fake.clone());

        let key = MasterKey::from_bytes([5; 32]);
        let database_path = dir.path().join("portal.sqlite");
        let mut database = Database::open(&database_path).unwrap();
        database.bind_master_key_id(&key.key_id_hex()).unwrap();
        let agents = start_agents(dir.path(), &mut database, &key).await;
        database
            .add_user("Owner", "owner@example.com", now())
            .unwrap();
        drop(database);

        let port = free_port();
        let started_at = now();
        let (pem, initial_certificate) =
            ca.pair(DOMAIN, started_at + validity.0, started_at + validity.1);
        fake.set(|s| s.cert = CertAnswer::Pair(pem));
        let server = tokio::spawn(serve_portal(PortalServeConfig {
            database: database_path.clone(),
            master_key: key,
            agents,
            bind: format!("tailnet:{port}").parse().unwrap(),
            api: LocalApi::new(&socket),
            tls_domain: None,
            tailnet_wait: Duration::from_secs(5),
            address_poll: Duration::from_millis(100),
            renewal,
            limits,
            allow_portal_node_sign_in: false,
        }));
        let deadline = std::time::Instant::now() + Duration::from_secs(20);
        while tokio::net::TcpStream::connect(("127.0.0.1", port))
            .await
            .is_err()
        {
            assert!(!server.is_finished(), "portal exited: {:?}", server.await);
            assert!(std::time::Instant::now() < deadline, "portal did not start");
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        Self {
            dir,
            fake,
            ca,
            database_path,
            port,
            started_at,
            initial_certificate,
            server,
            _localapi: localapi,
        }
    }

    fn client(&self, from: &str) -> reqwest::Client {
        reqwest::Client::builder()
            .resolve(DOMAIN, SocketAddr::from(([127, 0, 0, 1], self.port)))
            .local_address(from.parse::<IpAddr>().unwrap())
            .add_root_certificate(reqwest::Certificate::from_pem(self.ca.pem.as_bytes()).unwrap())
            .tls_built_in_root_certs(false)
            .tls_info(true)
            .timeout(Duration::from_secs(20))
            .build()
            .unwrap()
    }

    fn url(&self, path: &str) -> String {
        format!("https://{DOMAIN}:{}{path}", self.port)
    }

    fn database(&self) -> Database {
        Database::open(&self.database_path).unwrap()
    }
}

impl Drop for Harness {
    fn drop(&mut self) {
        self.server.abort();
    }
}

async fn code(response: reqwest::Response) -> String {
    let body: Value = response.json().await.unwrap();
    body["code"].as_str().unwrap_or_default().to_owned()
}

/// A signed-in browser: the session cookie and its CSRF token.
struct Browser {
    client: reqwest::Client,
    cookie: String,
    csrf: String,
}

async fn sign_in(harness: &Harness, from: &str) -> Browser {
    sign_in_with(harness, harness.client(from), None).await
}

async fn sign_in_with(harness: &Harness, client: reqwest::Client, old: Option<&str>) -> Browser {
    let mut request = client.post(harness.url("/api/session"));
    if let Some(old) = old {
        request = request.header("cookie", old);
    }
    let response = request.send().await.unwrap();
    assert_eq!(response.status(), StatusCode::CREATED);
    let set_cookie = response.headers()["set-cookie"]
        .to_str()
        .unwrap()
        .to_owned();
    let body: Value = response.json().await.unwrap();
    Browser {
        client,
        cookie: set_cookie.split(';').next().unwrap().to_owned(),
        csrf: body["csrf_token"].as_str().unwrap().to_owned(),
    }
}

impl Browser {
    fn get(&self, harness: &Harness, path: &str) -> reqwest::RequestBuilder {
        self.client
            .get(harness.url(path))
            .header("cookie", &self.cookie)
    }

    /// A state-changing request the way the portal's own page sends it.
    fn send(
        &self,
        harness: &Harness,
        method: reqwest::Method,
        path: &str,
    ) -> reqwest::RequestBuilder {
        self.client
            .request(method, harness.url(path))
            .header("cookie", &self.cookie)
            .header("x-csrf-token", &self.csrf)
            .header("origin", format!("https://{DOMAIN}:{}", harness.port))
            .header("sec-fetch-site", "same-origin")
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn identity_comes_only_from_whois_and_forged_headers_grant_nothing() {
    if !loopback_aliases() {
        return;
    }
    let harness = Harness::start().await;
    let forged = [
        ("Tailscale-User-Login", "owner@example.com"),
        ("Tailscale-User-Name", "Owner"),
        ("X-Forwarded-For", LAPTOP),
        ("X-Forwarded-User", "owner@example.com"),
        ("Forwarded", "for=127.0.0.2"),
    ];
    let intruder = harness.client(INTRUDER);
    let with_forged = |mut request: reqwest::RequestBuilder| {
        for (name, value) in forged {
            request = request.header(name, value);
        }
        request
    };

    let response = with_forged(intruder.get(harness.url("/api/whoami")))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body: Value = response.json().await.unwrap();
    assert_eq!(body["peer"]["user_login"], "mallory@example.com");
    assert_eq!(body["peer"]["node"], "nINTRUDER");
    assert_eq!(body["principal"]["kind"], "anonymous");

    let response = with_forged(intruder.post(harness.url("/api/session")))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
    assert_eq!(code(response).await, "unknown_user");
    let response =
        with_forged(intruder.get(harness.url(&format!("/api/files/{}", Uuid::new_v4()))))
            .send()
            .await
            .unwrap();
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);

    // A tagged node is a service, not a user: no session.
    let response = harness
        .client(SERVER)
        .post(harness.url("/api/session"))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
    assert_eq!(code(response).await, "tagged_node");

    // An address WhoIs cannot name never reaches TLS, let alone HTTP.
    assert!(harness
        .client(STRANGER)
        .get(harness.url("/api/whoami"))
        .send()
        .await
        .is_err());

    // The owner's laptop signs in; the cookie carries every attribute.
    let response = harness
        .client(LAPTOP)
        .post(harness.url("/api/session"))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::CREATED);
    let cookie = response.headers()["set-cookie"]
        .to_str()
        .unwrap()
        .to_owned();
    assert!(cookie.starts_with("__Host-distr-session="), "{cookie}");
    assert!(
        cookie.ends_with("; Path=/; Secure; HttpOnly; SameSite=Strict; Max-Age=43200"),
        "{cookie}"
    );

    let records = harness.database().audit_records(0, 1000).unwrap();
    let has = |event: &str, outcome: &str, reason: Option<&str>, node: Option<&str>| {
        records.iter().any(|record| {
            record.event == event
                && record.outcome == outcome
                && (reason.is_none() || record.reason.as_deref() == reason)
                && (node.is_none() || record.node_stable_id.as_deref() == node)
        })
    };
    assert!(has(
        "identity_headers",
        "observed",
        Some("ignored"),
        Some("nINTRUDER")
    ));
    let ignored = records
        .iter()
        .find(|record| record.event == "identity_headers")
        .unwrap();
    assert_eq!(
        ignored.detail.as_deref(),
        Some("forwarded,tailscale-user-login,tailscale-user-name,x-forwarded-for,x-forwarded-user")
    );
    assert!(has(
        "login",
        "denied",
        Some("unknown_user"),
        Some("nINTRUDER")
    ));
    assert!(has("login", "denied", Some("tagged_node"), Some("nSERVER")));
    assert!(has("connection", "denied", Some("peer_unknown"), None));
    assert!(has("login", "allowed", None, Some("nLAPTOP")));
    assert!(has(
        "authorize",
        "denied",
        Some("unauthenticated"),
        Some("nINTRUDER")
    ));
}

#[tokio::test(flavor = "multi_thread")]
async fn sessions_need_csrf_stay_on_their_node_rotate_and_expire() {
    if !loopback_aliases() {
        return;
    }
    let harness = Harness::start().await;
    let browser = sign_in(&harness, LAPTOP).await;
    let create = json!({ "name": "app", "scopes": ["files:read"], "node": "self" });

    let whoami: Value = browser
        .get(&harness, "/api/whoami")
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(whoami["principal"]["kind"], "session");
    assert_eq!(whoami["principal"]["csrf_token"], browser.csrf.as_str());
    assert_eq!(
        browser
            .get(&harness, "/api/keys")
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::OK
    );

    // State-changing requests without the CSRF token, with another
    // session's token, or marked cross-site by the browser are refused.
    let other = sign_in(&harness, LAPTOP).await;
    let attempts = [
        (None, None, None),
        (Some(other.csrf.as_str()), None, None),
        (
            Some(browser.csrf.as_str()),
            Some("https://evil.example"),
            None,
        ),
        (Some(browser.csrf.as_str()), None, Some("cross-site")),
        (Some(browser.csrf.as_str()), None, Some("same-site")),
    ];
    for (csrf, origin, site) in attempts {
        let mut request = browser
            .client
            .post(harness.url("/api/keys"))
            .header("cookie", &browser.cookie)
            .json(&create);
        if let Some(csrf) = csrf {
            request = request.header("x-csrf-token", csrf);
        }
        if let Some(origin) = origin {
            request = request.header("origin", origin);
        }
        if let Some(site) = site {
            request = request.header("sec-fetch-site", site);
        }
        let response = request.send().await.unwrap();
        assert_eq!(
            response.status(),
            StatusCode::FORBIDDEN,
            "{csrf:?} {origin:?} {site:?}"
        );
        let code = code(response).await;
        assert!(code == "csrf_rejected" || code == "cross_origin", "{code}");
    }
    assert_eq!(
        browser
            .send(&harness, reqwest::Method::POST, "/api/keys")
            .json(&create)
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::CREATED
    );
    // A cross-origin login is refused too.
    let response = harness
        .client(LAPTOP)
        .post(harness.url("/api/session"))
        .header("origin", "https://evil.example")
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::FORBIDDEN);

    // The cookie used from another node (even the same user's) is treated
    // as stolen: refused there and revoked everywhere.
    let stolen = harness
        .client(PHONE)
        .get(harness.url("/api/keys"))
        .header("cookie", &browser.cookie)
        .send()
        .await
        .unwrap();
    assert_eq!(stolen.status(), StatusCode::UNAUTHORIZED);
    assert!(stolen.headers()["set-cookie"]
        .to_str()
        .unwrap()
        .contains("Max-Age=0"));
    assert_eq!(code(stolen).await, "session_wrong_node");
    let after = browser.get(&harness, "/api/keys").send().await.unwrap();
    assert_eq!(code(after).await, "session_revoked");
    let response = harness
        .client(INTRUDER)
        .get(harness.url("/api/keys"))
        .header("cookie", &other.cookie)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);

    // Signing in again with a live cookie rotates it.
    let first = sign_in(&harness, LAPTOP).await;
    let second = sign_in_with(&harness, harness.client(LAPTOP), Some(&first.cookie)).await;
    assert_ne!(first.cookie, second.cookie);
    assert_eq!(
        code(first.get(&harness, "/api/keys").send().await.unwrap()).await,
        "session_revoked"
    );
    assert_eq!(
        second
            .get(&harness, "/api/keys")
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::OK
    );

    // Logout ends the session.
    let response = second
        .send(&harness, reqwest::Method::DELETE, "/api/session")
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::NO_CONTENT);
    assert_eq!(
        code(second.get(&harness, "/api/keys").send().await.unwrap()).await,
        "session_revoked"
    );

    // Expired and idle sessions are refused.
    let owner = harness
        .database()
        .user_by_login("owner@example.com")
        .unwrap()
        .unwrap();
    for (code_expected, created, last_seen, expires) in [
        ("session_expired", now() - 50_000, now() - 10, now() - 1),
        ("session_idle", now() - 7200, now() - 3601, now() + 3600),
    ] {
        let token = SessionToken::generate();
        harness
            .database()
            .insert_session(
                &SessionRow {
                    session_id: Uuid::new_v4(),
                    user_id: owner.user_id,
                    node_stable_id: "nLAPTOP".into(),
                    created_at: created,
                    last_seen_at: last_seen,
                    expires_at: expires,
                    revoked_at: None,
                },
                &token.verifier(),
            )
            .unwrap();
        let response = harness
            .client(LAPTOP)
            .get(harness.url("/api/keys"))
            .header("cookie", format!("__Host-distr-session={}", token.encode()))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        assert_eq!(code(response).await, code_expected);
    }

    // Disabling the user is a privilege change: every session ends.
    let live = sign_in(&harness, LAPTOP).await;
    harness
        .database()
        .disable_user(owner.user_id, now())
        .unwrap();
    assert_eq!(
        code(live.get(&harness, "/api/keys").send().await.unwrap()).await,
        "session_revoked"
    );
    let response = harness
        .client(LAPTOP)
        .post(harness.url("/api/session"))
        .send()
        .await
        .unwrap();
    assert_eq!(code(response).await, "user_disabled");
}

async fn create_key(harness: &Harness, browser: &Browser, request: Value) -> String {
    let response = browser
        .send(harness, reqwest::Method::POST, "/api/keys")
        .json(&request)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::CREATED);
    let body: Value = response.json().await.unwrap();
    body["key"].as_str().unwrap().to_owned()
}

async fn upload(
    harness: &Harness,
    from: &str,
    key: &str,
    idempotency: &str,
    body: Vec<u8>,
) -> reqwest::Response {
    harness
        .client(from)
        .post(harness.url("/api/files?name=report.bin"))
        .bearer_auth(key)
        .header("idempotency-key", idempotency)
        .body(body)
        .send()
        .await
        .unwrap()
}

#[tokio::test(flavor = "multi_thread")]
async fn api_keys_are_verifiers_and_enforce_collection_scopes() {
    if !loopback_aliases() {
        return;
    }
    let harness = Harness::start().await;
    let browser = sign_in(&harness, LAPTOP).await;
    let writer = create_key(
        &harness,
        &browser,
        json!({"name": "ingest", "scopes": ["files:write"], "node": "nSERVER"}),
    )
    .await;
    let reader = create_key(
        &harness,
        &browser,
        json!({"name": "viewer", "scopes": ["files:read"], "any_node": true}),
    )
    .await;
    let other = create_key(
        &harness,
        &browser,
        json!({"name": "notes", "scopes": ["notes:admin"], "any_node": true}),
    )
    .await;
    let pinned = create_key(
        &harness,
        &browser,
        json!({"name": "laptop", "scopes": ["files:write"], "node": "self"}),
    )
    .await;

    // Only verifiers are stored: no key's secret appears in the database
    // files, and no stored value equals it.
    let mut stored = Vec::new();
    for entry in std::fs::read_dir(harness.dir.path()).unwrap() {
        let path = entry.unwrap().path();
        if path
            .file_name()
            .unwrap()
            .to_string_lossy()
            .starts_with("portal.sqlite")
            && path.is_file()
        {
            std::fs::File::open(&path)
                .unwrap()
                .read_to_end(&mut stored)
                .unwrap();
        }
    }
    for key in [&writer, &reader, &other, &pinned] {
        // dhk_<id>_<secret>; the base64url secret may itself contain '_'.
        let secret = key.splitn(3, '_').nth(2).unwrap();
        assert_eq!(secret.len(), 43);
        assert!(
            !stored
                .windows(secret.len())
                .any(|window| window == secret.as_bytes()),
            "API key secret found in the database"
        );
        let parsed = ApiKeySecret::parse(key).unwrap();
        let row = harness
            .database()
            .api_key(parsed.key_id())
            .unwrap()
            .unwrap();
        assert!(parsed.matches(&row.verifier));
    }

    // Upload, read, and delete under the matching scopes, from a tagged
    // service node (API keys need no user identity).
    let content = b"scoped file contents".repeat(1000);
    let response = upload(&harness, SERVER, &writer, "upload-1", content.clone()).await;
    assert_eq!(response.status(), StatusCode::CREATED);
    let file_id: Value = response.json().await.unwrap();
    let file_id = file_id["file_id"].as_str().unwrap().to_owned();
    let download = |key: String| {
        let client = harness.client(SERVER);
        let url = harness.url(&format!("/api/files/{file_id}"));
        async move { client.get(url).bearer_auth(key).send().await.unwrap() }
    };
    let response = download(reader.clone()).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.bytes().await.unwrap().as_ref(), content.as_slice());
    // The same idempotency key under another principal is a new upload.
    let response = upload(&harness, LAPTOP, &pinned, "upload-1", content.clone()).await;
    assert_eq!(response.status(), StatusCode::CREATED);
    let again: Value = response.json().await.unwrap();
    assert_ne!(again["file_id"].as_str().unwrap(), file_id);

    // Scope refusals.
    let response = upload(&harness, SERVER, &reader, "upload-2", b"x".to_vec()).await;
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
    assert_eq!(code(response).await, "scope_denied");
    for key in [&reader, &other] {
        let response = harness
            .client(SERVER)
            .delete(harness.url(&format!("/api/files/{file_id}")))
            .bearer_auth(key)
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
    }
    assert_eq!(
        download(other.clone()).await.status(),
        StatusCode::FORBIDDEN
    );
    // A node-bound key works only from its node ("self" bound it to the
    // laptop that created it); keys must say which node, or any node.
    for (from, key) in [(SERVER, &pinned), (LAPTOP, &writer), (INTRUDER, &writer)] {
        let response = upload(&harness, from, key, "upload-3", b"x".to_vec()).await;
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
        assert_eq!(code(response).await, "api_key_wrong_node");
    }
    for request in [
        json!({"name": "unbound", "scopes": ["files:read"]}),
        json!({"name": "both", "scopes": ["files:read"], "node": "self", "any_node": true}),
        json!({"name": "bad", "scopes": ["files:own"], "any_node": true}),
        json!({"name": "extra", "scopes": ["files:read"], "any_node": true, "admin": true}),
    ] {
        let response = browser
            .send(&harness, reqwest::Method::POST, "/api/keys")
            .json(&request)
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST, "{request}");
    }
    // Keys cannot manage keys, read the audit log, or open sessions.
    for path in ["/api/keys", "/api/audit"] {
        let response = harness
            .client(SERVER)
            .get(harness.url(path))
            .bearer_auth(&writer)
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
        assert_eq!(code(response).await, "session_required");
    }
    let response = harness
        .client(LAPTOP)
        .post(harness.url("/api/session"))
        .bearer_auth(&writer)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::FORBIDDEN);

    // Malformed, unknown, altered, and doubled credentials.
    // Change a middle character: the last one carries padding bits, so
    // changing it can make the key malformed rather than wrong.
    let mut altered = writer.clone().into_bytes();
    let middle = altered.len() - 10;
    altered[middle] = if altered[middle] == b'A' { b'B' } else { b'A' };
    let altered = String::from_utf8(altered).unwrap();
    let unknown = ApiKeySecret::generate().encode().to_string();
    for (key, expected) in [
        ("dhk_nonsense".to_owned(), "api_key_malformed"),
        (unknown, "api_key_unknown"),
        (altered, "api_key_mismatch"),
    ] {
        let response = download(key).await;
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        assert_eq!(code(response).await, expected);
    }
    let response = harness
        .client(LAPTOP)
        .get(harness.url("/api/whoami"))
        .bearer_auth(&reader)
        .header("cookie", &browser.cookie)
        .send()
        .await
        .unwrap();
    assert_eq!(code(response).await, "ambiguous_credentials");
    let response = harness
        .client(LAPTOP)
        .get(harness.url("/api/whoami"))
        .basic_auth("owner", Some("secret"))
        .send()
        .await
        .unwrap();
    assert_eq!(code(response).await, "unsupported_authorization");

    // Oversized uploads and missing idempotency keys are refused.
    // A declared length over the limit is refused before the body is read,
    // so the client may see the 413 or a closed connection.
    let oversized = harness
        .client(SERVER)
        .post(harness.url("/api/files?name=report.bin"))
        .bearer_auth(&writer)
        .header("idempotency-key", "big")
        .body(vec![0; (8 << 20) + 1])
        .send()
        .await;
    if let Ok(response) = oversized {
        assert_eq!(response.status(), StatusCode::PAYLOAD_TOO_LARGE);
    }
    assert!(harness
        .database()
        .audit_records(0, 10_000)
        .unwrap()
        .iter()
        .any(|record| record.reason.as_deref() == Some("upload_too_large")));
    let response = harness
        .client(SERVER)
        .post(harness.url("/api/files?name=report.bin"))
        .bearer_auth(&writer)
        .body("x")
        .send()
        .await
        .unwrap();
    assert_eq!(code(response).await, "idempotency_key_required");

    // Delete with write scope, then the file is gone.
    let response = harness
        .client(SERVER)
        .delete(harness.url(&format!("/api/files/{file_id}")))
        .bearer_auth(&writer)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        download(reader.clone()).await.status(),
        StatusCode::NOT_FOUND
    );

    // Revocation and expiry.
    let reader_id = ApiKeySecret::parse(&reader).unwrap().key_id().to_owned();
    let response = browser
        .send(
            &harness,
            reqwest::Method::DELETE,
            &format!("/api/keys/{reader_id}"),
        )
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::NO_CONTENT);
    assert_eq!(
        code(download(reader.clone()).await).await,
        "api_key_revoked"
    );

    // Every state change and refusal is in the audit log.
    let records = harness.database().audit_records(0, 1000).unwrap();
    for (event, outcome) in [
        ("api_key_create", "allowed"),
        ("file_upload", "allowed"),
        ("file_upload", "completed"),
        ("file_delete", "allowed"),
        ("file_delete", "completed"),
        ("api_key_revoke", "allowed"),
        ("authorize", "denied"),
        ("authenticate", "denied"),
    ] {
        assert!(
            records
                .iter()
                .any(|record| record.event == event && record.outcome == outcome),
            "{event} {outcome}"
        );
    }
    let audit: Value = browser
        .get(&harness, "/api/audit?limit=5")
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(audit["records"].as_array().unwrap().len(), 5);
}

/// The DER of the certificate the portal presents to a new connection.
async fn served_certificate(harness: &Harness) -> Result<Vec<u8>, reqwest::Error> {
    let response = harness
        .client(LAPTOP)
        .get(harness.url("/api/whoami"))
        .send()
        .await?;
    Ok(response
        .extensions()
        .get::<reqwest::tls::TlsInfo>()
        .and_then(|info| info.peer_certificate())
        .expect("TLS info")
        .to_vec())
}

/// What a plaintext HTTP request from the owner's laptop gets back: the
/// peer passes WhoIs, so only TLS stands between it and the API.
async fn plaintext_answer(port: u16) -> String {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let socket = tokio::net::TcpSocket::new_v4().unwrap();
    socket.bind(format!("{LAPTOP}:0").parse().unwrap()).unwrap();
    let mut stream = socket
        .connect(SocketAddr::from(([127, 0, 0, 1], port)))
        .await
        .unwrap();
    let _ = stream
        .write_all(b"GET /api/whoami HTTP/1.1\r\nHost: portal\r\n\r\n")
        .await;
    let mut answer = Vec::new();
    let _ = tokio::time::timeout(Duration::from_secs(5), stream.read_to_end(&mut answer)).await;
    String::from_utf8_lossy(&answer).into_owned()
}

async fn wait_until(what: &str, seconds: u64, mut check: impl AsyncFnMut() -> bool) {
    let deadline = std::time::Instant::now() + Duration::from_secs(seconds);
    while !check().await {
        assert!(
            std::time::Instant::now() < deadline,
            "timed out waiting for {what}"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn certificates_renew_hot_reload_and_fail_closed() {
    if !loopback_aliases() {
        return;
    }
    // A: six seconds of validity, so renewal is due after four.
    let harness = Harness::start_with((-1, 5), fast_renewal()).await;
    let t0 = harness.started_at;
    assert_eq!(
        served_certificate(&harness).await.unwrap(),
        harness.initial_certificate
    );
    assert_eq!(harness.fake.get(|s| s.cert_requests.len()), 1);
    let answer = plaintext_answer(harness.port).await;
    assert!(
        !answer.contains("HTTP/1."),
        "plaintext answered: {answer:?}"
    );

    // tailscaled renews; the portal fetches only once renewal is due and
    // serves the new certificate without a restart.
    let (pem_b, der_b) = harness.ca.pair(DOMAIN, now() - 1, t0 + 7);
    harness.fake.set(|s| s.cert = CertAnswer::Pair(pem_b));
    tokio::time::sleep(Duration::from_millis(1500)).await;
    if now() < t0 + 3 {
        assert_eq!(
            harness.fake.get(|s| s.cert_requests.len()),
            1,
            "no fetch before a third of the lifetime remains"
        );
    }
    wait_until("the renewed certificate", 10, async || {
        served_certificate(&harness).await.ok().as_deref() == Some(der_b.as_slice())
    })
    .await;

    // Renewal now fails. Until B expires it is still served; after that
    // every handshake fails and plaintext gets no HTTP answer.
    harness.fake.set(|s| s.cert = CertAnswer::Status(403));
    wait_until("certificate expiry", 15, async || {
        served_certificate(&harness).await.is_err()
    })
    .await;
    let answer = plaintext_answer(harness.port).await;
    assert!(
        !answer.contains("HTTP/1."),
        "plaintext answered: {answer:?}"
    );

    // tailscaled recovers: the portal serves again on its own.
    let (pem_c, der_c) = harness.ca.pair(DOMAIN, now() - 1, now() + 3600);
    harness.fake.set(|s| s.cert = CertAnswer::Pair(pem_c));
    wait_until("recovery", 10, async || {
        served_certificate(&harness).await.ok().as_deref() == Some(der_c.as_slice())
    })
    .await;
    assert!(!harness.server.is_finished());
}

fn portal_serve(dir: &std::path::Path, socket: &std::path::Path, bind: &str) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_distr-hnsw"));
    command
        .args(["portal", "serve", "--database"])
        .arg(dir.join("portal.sqlite"))
        .arg("--master-key")
        .arg(dir.join("master.key"))
        .args([
            "--agent",
            "a1,d1,http://127.0.0.1:9",
            "--agent",
            "a2,d2,http://127.0.0.1:9",
        ])
        .args([
            "--bind",
            bind,
            "--tailnet-wait-seconds",
            "1",
            "--tailnet-poll-ms",
            "100",
        ])
        .env("DISTR_HNSW_TAILSCALE_SOCKET", socket)
        .stdout(Stdio::null())
        .stderr(Stdio::piped());
    command
}

#[tokio::test(flavor = "multi_thread")]
async fn the_portal_binds_only_its_tailnet_address_and_only_with_https() {
    let dir = tempfile::tempdir().unwrap();
    let socket = socket_in(&dir);
    let init = Command::new(env!("CARGO_BIN_EXE_distr-hnsw"))
        .args(["portal", "init", "--no-recovery-bundle", "--database"])
        .arg(dir.path().join("portal.sqlite"))
        .arg("--master-key")
        .arg(dir.path().join("master.key"))
        .output()
        .unwrap();
    assert!(init.status.success(), "{init:?}");
    let refusal = |bind: &str| {
        let output = portal_serve(dir.path(), &socket, bind).output().unwrap();
        assert!(!output.status.success(), "{bind} must not serve");
        String::from_utf8_lossy(&output.stderr).into_owned()
    };
    assert!(refusal("0.0.0.0:8443").contains("wildcard"));
    assert!(refusal("[::]:8443").contains("wildcard"));
    assert!(refusal("127.0.0.1:8443").contains("serves only on a tailnet address"));
    assert!(refusal("tailnet:8443").contains("not ready"));

    let fake = Fake::new(&["127.0.0.1"]);
    let _localapi = start_fake(&socket, fake.clone());
    assert!(refusal("192.168.1.20:8443").contains("not one of this node's tailnet addresses"));
    // HTTPS certificates off in the tailnet: no plaintext fallback.
    assert!(refusal("tailnet:8443").contains("HTTPS certificates turned off"));

    // Serving, then the tailnet address changes: exit 75 for the supervisor.
    let ca = TestCa::new();
    let (pem, _) = ca.pair(DOMAIN, now() - 60, now() + 3600);
    fake.set(|s| {
        s.cert_domains = vec![DOMAIN.into()];
        s.cert = CertAnswer::Pair(pem);
    });
    let port = free_port();
    let mut child = portal_serve(dir.path(), &socket, &format!("tailnet:{port}"))
        .spawn()
        .unwrap();
    let deadline = std::time::Instant::now() + Duration::from_secs(20);
    while tokio::net::TcpStream::connect(("127.0.0.1", port))
        .await
        .is_err()
    {
        if let Some(status) = child.try_wait().unwrap() {
            let mut stderr = String::new();
            child
                .stderr
                .take()
                .unwrap()
                .read_to_string(&mut stderr)
                .unwrap();
            panic!("portal exited with {status}: {stderr}");
        }
        assert!(std::time::Instant::now() < deadline, "portal did not start");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    fake.set(|s| s.addresses = vec!["127.0.0.9".parse().unwrap()]);
    let status = tokio::task::spawn_blocking(move || child.wait().unwrap())
        .await
        .unwrap();
    assert_eq!(status.code(), Some(75));
}

#[test]
fn the_identity_commands_manage_users_and_keys_and_audit_each_change() {
    let dir = tempfile::tempdir().unwrap();
    let database = dir.path().join("portal.sqlite");
    let run = |args: &[&str]| {
        let output = Command::new(env!("CARGO_BIN_EXE_distr-hnsw"))
            .arg("portal")
            .args(args)
            .arg("--database")
            .arg(&database)
            .output()
            .unwrap();
        (
            output.status.success(),
            String::from_utf8_lossy(&output.stdout).into_owned(),
            String::from_utf8_lossy(&output.stderr).into_owned(),
        )
    };
    let (ok, out, _) = run(&["user", "add", "--tailscale-login", "alice@github"]);
    assert!(ok);
    let user: Value = serde_json::from_str(&out).unwrap();
    assert_eq!(user["logins"], json!(["alice@github"]));
    let (ok, _, err) = run(&["user", "add", "--tailscale-login", "alice@github"]);
    assert!(!ok && err.contains("already belongs"), "{err}");

    let (ok, _, err) = run(&[
        "api-key",
        "create",
        "--tailscale-login",
        "bob@github",
        "--name",
        "x",
        "--scope",
        "files:read",
        "--any-node",
    ]);
    assert!(!ok && err.contains("no enabled user"), "{err}");
    let (ok, _, err) = run(&[
        "api-key",
        "create",
        "--tailscale-login",
        "alice@github",
        "--name",
        "x",
        "--scope",
        "files:own",
        "--any-node",
    ]);
    assert!(!ok && err.contains("read, write, or admin"), "{err}");
    // A key must name its node or explicitly allow any node.
    let (ok, _, err) = run(&[
        "api-key",
        "create",
        "--tailscale-login",
        "alice@github",
        "--name",
        "x",
        "--scope",
        "files:read",
    ]);
    assert!(!ok && err.contains("--node"), "{err}");
    let (ok, key, _) = run(&[
        "api-key",
        "create",
        "--tailscale-login",
        "alice@github",
        "--name",
        "ingest",
        "--scope",
        "files:write",
        "--scope",
        "notes:read",
        "--node",
        "nLAPTOP",
    ]);
    assert!(ok);
    let key = key.trim().to_owned();
    let parsed = ApiKeySecret::parse(&key).expect("the printed key parses");
    let (_, list, _) = run(&["api-key", "list"]);
    assert!(!list.contains(&key), "list must not print secrets");
    let listed: Value = serde_json::from_str(list.trim()).unwrap();
    assert_eq!(listed["scopes"], json!(["files:write", "notes:read"]));
    assert_eq!(listed["node"], "nLAPTOP");
    assert!(run(&["api-key", "revoke", "--key-id", parsed.key_id()]).0);
    assert!(!run(&["api-key", "revoke", "--key-id", parsed.key_id()]).0);

    let user_id = user["user_id"].as_str().unwrap();
    let (ok, out, _) = run(&["user", "disable", "--user-id", user_id]);
    assert!(ok);
    assert_eq!(
        serde_json::from_str::<Value>(&out).unwrap()["api_keys_revoked"],
        0
    );

    let (_, audit, _) = run(&["audit"]);
    let events: Vec<String> = audit
        .lines()
        .map(|line| serde_json::from_str::<Value>(line).unwrap())
        .map(|record| {
            format!(
                "{}:{}",
                record["event"].as_str().unwrap(),
                record["peer"].as_str().unwrap()
            )
        })
        .collect();
    assert_eq!(
        events,
        [
            "user_add:cli",
            "api_key_create:cli",
            "api_key_revoke:cli",
            "user_disable:cli"
        ]
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn every_route_refuses_anonymous_callers_and_sign_in_is_limited() {
    if !loopback_aliases() {
        return;
    }
    let harness = Harness::start().await;
    let anonymous = harness.client(INTRUDER);
    let file = Uuid::new_v4();
    for (method, path) in [
        (reqwest::Method::GET, "/api/keys".to_owned()),
        (reqwest::Method::POST, "/api/keys".to_owned()),
        (
            reqwest::Method::DELETE,
            "/api/keys/0011223344556677".to_owned(),
        ),
        (reqwest::Method::GET, "/api/audit".to_owned()),
        (reqwest::Method::DELETE, "/api/session".to_owned()),
        (reqwest::Method::POST, "/api/files?name=x".to_owned()),
        (reqwest::Method::GET, format!("/api/files/{file}")),
        (reqwest::Method::DELETE, format!("/api/files/{file}")),
    ] {
        let response = anonymous
            .request(method.clone(), harness.url(&path))
            .header("idempotency-key", "k")
            .send()
            .await
            .unwrap();
        assert_eq!(
            response.status(),
            StatusCode::UNAUTHORIZED,
            "{method} {path}"
        );
        assert_eq!(code(response).await, "unauthenticated", "{method} {path}");
    }

    // The owner, but on the portal's own node: any process there would pass
    // WhoIs as the owner, so sign-in is refused by default.
    let response = harness
        .client(PORTAL_NODE)
        .post(harness.url("/api/session"))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
    assert_eq!(code(response).await, "portal_node");

    // Sign-ins are limited per node: a burst of ten, then refused.
    for _ in 0..10 {
        sign_in(&harness, PHONE).await;
    }
    let response = harness
        .client(PHONE)
        .post(harness.url("/api/session"))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
    sign_in(&harness, LAPTOP).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn sessions_change_files_only_with_csrf_and_only_as_their_user() {
    use distr_hnsw::portal_server::{new_api_key, KeyNode};
    if !loopback_aliases() {
        return;
    }
    let harness = Harness::start().await;
    let browser = sign_in(&harness, LAPTOP).await;
    let content = b"session upload".repeat(100);
    let upload = |csrf: bool| {
        let mut request = browser
            .client
            .post(harness.url("/api/files?name=notes.txt"))
            .header("cookie", &browser.cookie)
            .header("idempotency-key", "session-upload")
            .body(content.clone());
        if csrf {
            request = request.header("x-csrf-token", &browser.csrf);
        }
        request.send()
    };
    let response = upload(false).await.unwrap();
    assert_eq!(code(response).await, "csrf_rejected");
    let response = upload(true).await.unwrap();
    assert_eq!(response.status(), StatusCode::CREATED);
    let file_id: Value = response.json().await.unwrap();
    let file_id = file_id["file_id"].as_str().unwrap().to_owned();
    let path = format!("/api/files/{file_id}");
    let response = browser.get(&harness, &path).send().await.unwrap();
    assert_eq!(response.bytes().await.unwrap().as_ref(), content.as_slice());
    let response = browser
        .client
        .delete(harness.url(&path))
        .header("cookie", &browser.cookie)
        .send()
        .await
        .unwrap();
    assert_eq!(code(response).await, "csrf_rejected");
    let response = browser
        .send(&harness, reqwest::Method::DELETE, &path)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        browser.get(&harness, &path).send().await.unwrap().status(),
        StatusCode::NOT_FOUND
    );

    // An expired key, and another user's key, are refused.
    let database_owner = harness
        .database()
        .user_by_login("owner@example.com")
        .unwrap()
        .unwrap();
    let (expired, secret) = new_api_key(
        database_owner.user_id,
        "old",
        &["files:read".into()],
        KeyNode::Any,
        Some(1),
        now() - 2 * 86_400,
    )
    .unwrap();
    harness.database().insert_api_key(&expired).unwrap();
    let response = harness
        .client(SERVER)
        .get(harness.url(&path))
        .bearer_auth(secret.encode().as_str())
        .send()
        .await
        .unwrap();
    assert_eq!(code(response).await, "api_key_expired");
    let carol = harness
        .database()
        .add_user("Carol", "carol@example.com", now())
        .unwrap();
    let (carols, _) = new_api_key(
        carol.user_id,
        "carol",
        &["files:read".into()],
        KeyNode::Any,
        None,
        now(),
    )
    .unwrap();
    harness.database().insert_api_key(&carols).unwrap();
    let response = browser
        .send(
            &harness,
            reqwest::Method::DELETE,
            &format!("/api/keys/{}", carols.key_id),
        )
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    assert!(harness
        .database()
        .api_key(&carols.key_id)
        .unwrap()
        .unwrap()
        .revoked_at
        .is_none());

    // The node's tailnet user changes: its old session is not the new
    // user's, and is revoked.
    harness.fake.set(|s| {
        s.peers.insert(
            LAPTOP.into(),
            FakePeer::new("nLAPTOP", "laptop.example.ts.net.", "carol@example.com"),
        );
    });
    let response = harness
        .client(LAPTOP)
        .get(harness.url("/api/keys"))
        .header("cookie", &browser.cookie)
        .send()
        .await
        .unwrap();
    assert_eq!(code(response).await, "session_wrong_user");

    let records = harness.database().audit_records(0, 1000).unwrap();
    for (event, outcome) in [
        ("file_upload", "allowed"),
        ("file_upload", "completed"),
        ("file_delete", "allowed"),
        ("file_delete", "completed"),
    ] {
        assert!(
            records.iter().any(|record| record.event == event
                && record.outcome == outcome
                && record.session_id.is_some()),
            "{event} {outcome}"
        );
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_client_that_hangs_up_cannot_skip_the_audit_of_its_change() {
    if !loopback_aliases() {
        return;
    }
    let harness = Harness::start().await;
    let browser = sign_in(&harness, LAPTOP).await;
    let key = create_key(
        &harness,
        &browser,
        json!({"name": "writer", "scopes": ["files:write"], "any_node": true}),
    )
    .await;
    let mut files = Vec::new();
    for (index, wait_ms) in [1_u64, 3, 10, 30, 100].into_iter().enumerate() {
        let response = upload(
            &harness,
            SERVER,
            &key,
            &format!("u{index}"),
            vec![index as u8; 5000],
        )
        .await;
        let body: Value = response.json().await.unwrap();
        let file_id = body["file_id"].as_str().unwrap().to_owned();
        // Hang up after `wait_ms`, possibly while the delete is in flight.
        let request = harness
            .client(SERVER)
            .delete(harness.url(&format!("/api/files/{file_id}")))
            .bearer_auth(&key)
            .send();
        let _ = tokio::time::timeout(Duration::from_millis(wait_ms), request).await;
        files.push(file_id);
    }
    tokio::time::sleep(Duration::from_secs(2)).await;
    let records = harness.database().audit_records(0, 10_000).unwrap();
    for file_id in files {
        let response = harness
            .client(SERVER)
            .get(harness.url(&format!("/api/files/{file_id}")))
            .bearer_auth(&key)
            .send()
            .await
            .unwrap();
        // files:write includes read; 404 means the delete happened.
        let deleted = response.status() == StatusCode::NOT_FOUND;
        let audited = |outcome: &str| {
            records.iter().any(|record| {
                record.event == "file_delete"
                    && record.outcome == outcome
                    && record.target.as_deref() == Some(file_id.as_str())
            })
        };
        assert_eq!(deleted, audited("allowed"), "{file_id}");
        assert_eq!(deleted, audited("completed"), "{file_id}");
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn refusal_records_are_rate_limited_per_peer() {
    if !loopback_aliases() {
        return;
    }
    let harness = Harness::start().await;
    let intruder = harness.client(INTRUDER);
    for _ in 0..60 {
        let response = intruder
            .get(harness.url(&format!("/api/files/{}", Uuid::new_v4())))
            .header("x-forwarded-for", LAPTOP)
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    }
    let from = |records: &[distr_hnsw::metadata::AuditRecord], node: &str| {
        records
            .iter()
            .filter(|record| record.node_stable_id.as_deref() == Some(node))
            .count()
    };
    let records = harness.database().audit_records(0, 10_000).unwrap();
    // 120 refusals and observations; a burst of 30 is written.
    let written = from(&records, "nINTRUDER");
    assert!((30..=32).contains(&written), "{written} records");
    // Another peer is unaffected.
    let response = harness
        .client(SERVER)
        .get(harness.url("/api/whoami"))
        .header("x-forwarded-for", LAPTOP)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let records = harness.database().audit_records(0, 10_000).unwrap();
    assert_eq!(from(&records, "nSERVER"), 1);
    // Once the bucket refills, the next record says how many were dropped.
    tokio::time::sleep(Duration::from_millis(6500)).await;
    let _ = intruder.get(harness.url("/api/keys")).send().await.unwrap();
    let records = harness.database().audit_records(0, 10_000).unwrap();
    let last = records
        .iter()
        .rev()
        .find(|record| record.node_stable_id.as_deref() == Some("nINTRUDER"))
        .unwrap();
    assert!(
        last.detail.as_deref().is_some_and(
            |detail| detail.contains("earlier records from this peer were not written")
        ),
        "{last:?}"
    );
}

async fn tls_from(
    harness: &Harness,
    from: &str,
) -> tokio_rustls::client::TlsStream<tokio::net::TcpStream> {
    use rustls::pki_types::{pem::PemObject, CertificateDer, ServerName};
    let mut roots = rustls::RootCertStore::empty();
    roots
        .add(CertificateDer::from_pem_slice(harness.ca.pem.as_bytes()).unwrap())
        .unwrap();
    let config = rustls::ClientConfig::builder_with_provider(std::sync::Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .unwrap()
    .with_root_certificates(roots)
    .with_no_client_auth();
    let socket = tokio::net::TcpSocket::new_v4().unwrap();
    socket.bind(format!("{from}:0").parse().unwrap()).unwrap();
    let stream = socket
        .connect(SocketAddr::from(([127, 0, 0, 1], harness.port)))
        .await
        .unwrap();
    tokio_rustls::TlsConnector::from(std::sync::Arc::new(config))
        .connect(ServerName::try_from(DOMAIN).unwrap(), stream)
        .await
        .unwrap()
}

#[tokio::test(flavor = "multi_thread")]
async fn slow_long_and_excess_connections_are_cut_off() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    if !loopback_aliases() {
        return;
    }
    let limits = ServeLimits {
        header_read_timeout: Duration::from_secs(1),
        connection_lifetime: Duration::from_secs(2),
        max_connections_per_address: 2,
        ..Harness::test_limits()
    };
    let harness = Harness::start_with_limits((-60, 86_400), fast_renewal(), limits).await;
    let read_all = |mut stream: tokio_rustls::client::TlsStream<tokio::net::TcpStream>| async move {
        let mut answer = Vec::new();
        let _ = tokio::time::timeout(Duration::from_secs(5), stream.read_to_end(&mut answer)).await;
        String::from_utf8_lossy(&answer).into_owned()
    };

    // Headers that never finish: the connection is closed unanswered.
    let mut slow = tls_from(&harness, LAPTOP).await;
    slow.write_all(b"GET /api/whoami HTTP/1.1\r\nHost: portal\r\n")
        .await
        .unwrap();
    let started = std::time::Instant::now();
    let answer = read_all(slow).await;
    assert!(!answer.contains("200 OK"), "{answer}");
    assert!(
        started.elapsed() < Duration::from_secs(4),
        "closed by the header timeout"
    );

    // A keep-alive connection ends after its lifetime, so the client must
    // reconnect and be resolved again.
    let mut kept = tls_from(&harness, LAPTOP).await;
    kept.write_all(b"GET /api/whoami HTTP/1.1\r\nHost: portal\r\n\r\n")
        .await
        .unwrap();
    let mut first = [0_u8; 15];
    kept.read_exact(&mut first).await.unwrap();
    assert_eq!(&first, b"HTTP/1.1 200 OK");
    let answer = read_all(kept).await;
    assert!(
        started.elapsed() < Duration::from_secs(9),
        "closed after its lifetime: {answer}"
    );

    // A third concurrent connection from one address is dropped at once.
    let hold = |from: &'static str| async move {
        let socket = tokio::net::TcpSocket::new_v4().unwrap();
        socket.bind(format!("{from}:0").parse().unwrap()).unwrap();
        socket
            .connect(SocketAddr::from(([127, 0, 0, 1], harness.port)))
            .await
            .unwrap()
    };
    let _first = hold(INTRUDER).await;
    let _second = hold(INTRUDER).await;
    tokio::time::sleep(Duration::from_millis(200)).await;
    let mut third = hold(INTRUDER).await;
    let mut byte = [0_u8; 1];
    let read = tokio::time::timeout(Duration::from_secs(2), third.read(&mut byte))
        .await
        .expect("the third connection is closed, not left waiting");
    assert!(matches!(read, Ok(0) | Err(_)));
    // Other addresses still connect.
    assert_eq!(
        harness
            .client(SERVER)
            .get(harness.url("/api/whoami"))
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::OK
    );
}
