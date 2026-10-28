//! The portal daemon, `portal serve` (DESIGN §3, §9, §12;
//! docs/m2-implementation-plan.md, decisions 7 and 8).
//!
//! It listens only on this node's tailnet address and speaks only HTTPS,
//! with the node's certificate from `tailscaled` ([`crate::tls`]). Each
//! accepted connection is resolved with LocalAPI WhoIs on its socket
//! address before the TLS handshake; a peer WhoIs cannot name is dropped.
//! Connections have a header-read timeout and a bounded lifetime, after
//! which a client reconnects and is resolved again, and their number is
//! capped per address and in total. Identity headers such as
//! `Tailscale-User-Login` are removed, logged, and audited, and never grant
//! anything. A request then authenticates with a session cookie or an API
//! key, and each route names the principals it admits:
//!
//! | Route | Session | API key |
//! |---|---|---|
//! | `GET /api/whoami` | yes | yes (also unauthenticated) |
//! | `POST /api/session` | sign in by WhoIs identity | no |
//! | `DELETE /api/session` | yes | no |
//! | `GET, POST /api/keys`, `DELETE /api/keys/{id}` | yes | no |
//! | `GET /api/audit` | yes | no |
//! | `POST /api/files`, `DELETE /api/files/{id}` | yes | `files:write` |
//! | `GET /api/files/{id}` | yes | `files:read` |
//!
//! A session is bound to the tailnet node and user that created it, and a
//! state-changing request on a session needs its CSRF token and, when the
//! browser sends one, a same-origin `Origin`. Sign-in from the portal's own
//! node is refused unless the operator allows it, because any process there
//! would pass WhoIs as the node's owner. An API key is bound to one node
//! unless it was explicitly created for any node.
//!
//! Every state change is audited before it runs and again when it ends; if
//! the first record cannot be written, the change does not happen. Records
//! of refusals and ignored headers are rate-limited per peer, so a peer
//! cannot grow the log without bound. File operations run one at a time on
//! a worker thread that owns the [`Portal`] and writes their audit records,
//! so a client that disconnects mid-request cannot skip them.

use std::{
    collections::HashMap,
    io,
    net::{IpAddr, SocketAddr},
    path::{Path as FsPath, PathBuf},
    pin::Pin,
    sync::{Arc, Mutex},
    task::{Context, Poll},
    time::{Duration, Instant},
};

use axum::{
    body::Body,
    extract::{
        rejection::JsonRejection, ConnectInfo, FromRequestParts, Path, Query, Request, State,
    },
    http::{header, request::Parts, HeaderMap, HeaderValue, Method, StatusCode},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::{delete, get, post},
    Json, Router,
};
use http_body_util::BodyExt;
use hyper_util::rt::{TokioIo, TokioTimer};
use serde::{Deserialize, Serialize};
use serde_json::json;
use tokio::{
    io::AsyncWriteExt,
    net::{TcpListener, TcpStream},
    sync::{mpsc, oneshot, OwnedSemaphorePermit, Semaphore},
};
use tokio_rustls::TlsAcceptor;
use tower::ServiceExt;
use uuid::Uuid;

use crate::{
    agent::TailnetAddressChanged,
    auth::{
        ApiKeySecret, CollectionScope, Right, Scopes, SessionToken, API_KEY_PREFIX, CSRF_HEADER,
        FILES_COLLECTION, SESSION_COOKIE, SESSION_IDLE_SECONDS, SESSION_LIFETIME_SECONDS,
    },
    crypto::MasterKey,
    durability::filesystem_space,
    metadata::{ApiKeyRow, AuditRecord, Database, MetadataError, SessionRow, UserRow},
    portal::{AgentTarget, Portal, PortalError},
    tailscale::{self, BindError, BindMode, BindSpec, LocalApi, PeerIdentity},
    tls::{self, now_seconds, CertificateStore, RenewalPolicy},
};

/// A TLS handshake must finish this soon after WhoIs.
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);
/// Session and key `last_used` stamps are written at most this often.
const TOUCH_INTERVAL_SECONDS: i64 = 60;
/// Ended sessions are deleted this long after they end.
const SESSION_RETENTION_SECONDS: i64 = 7 * 86_400;
const MAX_AUDIT_PAGE: usize = 1000;
const MAX_IDEMPOTENCY_KEY: usize = 128;
/// Refusal and observation records per peer: a burst, then one per six
/// seconds; the rest are counted and the count is noted on the next record.
const AUDIT_BURST: f64 = 30.0;
const AUDIT_PER_SECOND: f64 = 1.0 / 6.0;
/// Sign-ins per node: a burst, then one per thirty seconds.
const SIGN_IN_BURST: f64 = 10.0;
const SIGN_IN_PER_SECOND: f64 = 1.0 / 30.0;

/// Headers a proxy or a forger could use to claim an identity. The portal
/// never reads them; it removes them and records that they were sent.
/// Any `Tailscale-*` header is included.
const IDENTITY_HEADERS: &[&str] = &[
    "forwarded",
    "remote-user",
    "x-auth-request-email",
    "x-auth-request-user",
    "x-forwarded-email",
    "x-forwarded-for",
    "x-forwarded-host",
    "x-forwarded-proto",
    "x-forwarded-user",
    "x-real-ip",
    "x-remote-user",
    "x-webauth-user",
];

/// Connection and request limits.
#[derive(Clone, Copy, Debug)]
pub struct ServeLimits {
    /// A request's headers must arrive within this time.
    pub header_read_timeout: Duration,
    /// After this, a connection finishes its current request and closes,
    /// so the client reconnects and WhoIs resolves it afresh.
    pub connection_lifetime: Duration,
    pub max_connections: usize,
    pub max_connections_per_address: usize,
    pub max_concurrent_uploads: usize,
    pub max_concurrent_downloads: usize,
    pub max_upload_bytes: u64,
    /// Free space the spool's filesystem keeps after an upload is spooled.
    pub spool_floor_bytes: u64,
}

impl Default for ServeLimits {
    fn default() -> Self {
        Self {
            header_read_timeout: Duration::from_secs(10),
            connection_lifetime: Duration::from_secs(600),
            max_connections: 512,
            max_connections_per_address: 32,
            max_concurrent_uploads: 4,
            max_concurrent_downloads: 4,
            max_upload_bytes: 4 << 30,
            spool_floor_bytes: 1 << 30,
        }
    }
}

/// How to run `portal serve`.
pub struct PortalServeConfig {
    pub database: PathBuf,
    pub master_key: MasterKey,
    pub agents: Vec<AgentTarget>,
    pub bind: BindSpec,
    pub api: LocalApi,
    /// Certificate domain; must be one LocalAPI lists. Default: the first.
    pub tls_domain: Option<String>,
    pub tailnet_wait: Duration,
    pub address_poll: Duration,
    pub renewal: RenewalPolicy,
    pub limits: ServeLimits,
    /// Allow browser sign-in from the portal's own node.
    pub allow_portal_node_sign_in: bool,
}

/// The tailnet peer behind a connection, resolved once per connection.
#[derive(Clone, Debug)]
pub struct Peer {
    pub addr: SocketAddr,
    pub identity: Arc<PeerIdentity>,
}

/// Bind, obtain a certificate, and serve until the tailnet address changes
/// (an error, [`TailnetAddressChanged`]) or serving fails.
pub async fn serve_portal(config: PortalServeConfig) -> anyhow::Result<()> {
    let (addr, mode) =
        match tailscale::resolve_bind(config.bind, false, &config.api, config.tailnet_wait).await {
            Err(BindError::LoopbackNotAllowed(addr)) => anyhow::bail!(
                "refusing to bind loopback {addr}: the portal serves only on a tailnet address"
            ),
            resolved => resolved?,
        };
    anyhow::ensure!(
        mode == BindMode::Tailnet,
        "the portal serves only on a tailnet address"
    );
    let node = config.api.status().await?;
    let domain = match &config.tls_domain {
        Some(domain) if node.cert_domains.contains(domain) => domain.clone(),
        Some(domain) => anyhow::bail!(
            "{domain} is not a certificate domain of this node (LocalAPI lists {:?})",
            node.cert_domains
        ),
        None => node.cert_domains.first().cloned().ok_or_else(|| {
            anyhow::anyhow!(
                "this tailnet has HTTPS certificates turned off; the portal serves only HTTPS"
            )
        })?,
    };

    let mut database = Database::open(&config.database)?;
    database.bind_master_key_id(&config.master_key.key_id_hex())?;
    let spool = spool_directory(&config.database)?;
    let worker = PortalWorker::start(config.database.clone(), config.master_key, config.agents)?;

    let certificates = Arc::new(CertificateStore::default());
    certificates
        .install(tls::obtain(&config.api, &domain, config.renewal, config.tailnet_wait).await?);
    let listener = TcpListener::bind(addr).await?;
    let local = listener.local_addr()?;
    let origin = if local.port() == 443 {
        format!("https://{domain}")
    } else {
        format!("https://{domain}:{}", local.port())
    };
    let limits = config.limits;
    let state = Arc::new(ServerState {
        database: Mutex::new(database),
        worker,
        origin: origin.clone(),
        portal_node: node.stable_id,
        allow_portal_node_sign_in: config.allow_portal_node_sign_in,
        spool,
        limits,
        uploads: Arc::new(Semaphore::new(limits.max_concurrent_uploads)),
        downloads: Arc::new(Semaphore::new(limits.max_concurrent_downloads)),
        audit_limiter: RateLimiter::new(AUDIT_BURST, AUDIT_PER_SECOND),
        sign_in_limiter: RateLimiter::new(SIGN_IN_BURST, SIGN_IN_PER_SECOND),
    });
    eprintln!("portal serving {origin} on {local}");

    let acceptor = TlsAcceptor::from(tls::server_config(certificates.clone()));
    let router = router(state.clone());
    tokio::select! {
        () = accept_connections(listener, state, config.api.clone(), acceptor, router) => {
            unreachable!("the accept loop never returns")
        }
        () = tls::maintain(certificates, config.api.clone(), domain, config.renewal) => {
            unreachable!("certificate upkeep never returns")
        }
        lost = tailscale::address_lost(config.api, local.ip(), config.address_poll) => {
            Err(TailnetAddressChanged(lost).into())
        }
    }
}

/// Open connections, in total and per remote address.
#[derive(Default)]
struct ConnectionSlots {
    counts: Mutex<(usize, HashMap<IpAddr, usize>)>,
}

/// One open connection's place; released when dropped.
struct ConnectionSlot {
    slots: Arc<ConnectionSlots>,
    ip: IpAddr,
}

impl ConnectionSlots {
    fn claim(self: &Arc<Self>, ip: IpAddr, limits: &ServeLimits) -> Option<ConnectionSlot> {
        let mut counts = self.counts.lock().unwrap_or_else(|p| p.into_inner());
        let (total, per_address) = &mut *counts;
        let mine = per_address.get(&ip).copied().unwrap_or(0);
        if *total >= limits.max_connections || mine >= limits.max_connections_per_address {
            return None;
        }
        *total += 1;
        per_address.insert(ip, mine + 1);
        Some(ConnectionSlot {
            slots: self.clone(),
            ip,
        })
    }
}

impl Drop for ConnectionSlot {
    fn drop(&mut self) {
        let mut counts = self.slots.counts.lock().unwrap_or_else(|p| p.into_inner());
        let (total, per_address) = &mut *counts;
        *total -= 1;
        if let Some(mine) = per_address.get_mut(&self.ip) {
            *mine -= 1;
            if *mine == 0 {
                per_address.remove(&self.ip);
            }
        }
    }
}

/// Accept TCP connections and serve each on its own task.
async fn accept_connections(
    listener: TcpListener,
    state: Arc<ServerState>,
    api: LocalApi,
    acceptor: TlsAcceptor,
    router: Router,
) {
    let slots = Arc::new(ConnectionSlots::default());
    loop {
        let (stream, addr) = match listener.accept().await {
            Ok(accepted) => accepted,
            Err(error) => {
                eprintln!("warning: accept failed: {error}");
                tokio::time::sleep(Duration::from_millis(100)).await;
                continue;
            }
        };
        let Some(slot) = slots.claim(addr.ip(), &state.limits) else {
            state.record_limited(
                &addr.ip().to_string(),
                AuditRecord {
                    at: now_seconds(),
                    event: "connection".into(),
                    outcome: "denied".into(),
                    reason: Some("too_many_connections".into()),
                    peer: Some(addr.to_string()),
                    ..AuditRecord::default()
                },
            );
            continue;
        };
        tokio::spawn(serve_connection(
            state.clone(),
            api.clone(),
            acceptor.clone(),
            router.clone(),
            stream,
            addr,
            slot,
        ));
    }
}

/// Resolve the peer with WhoIs, complete the TLS handshake, and serve
/// HTTP/1.1 until the client closes or the connection's lifetime ends.
async fn serve_connection(
    state: Arc<ServerState>,
    api: LocalApi,
    acceptor: TlsAcceptor,
    router: Router,
    stream: TcpStream,
    addr: SocketAddr,
    _slot: ConnectionSlot,
) {
    let identity = match api.whois(addr).await {
        Ok(identity) => identity,
        Err(error) => {
            state.record_limited(
                &addr.ip().to_string(),
                AuditRecord {
                    at: now_seconds(),
                    event: "connection".into(),
                    outcome: "denied".into(),
                    reason: Some("peer_unknown".into()),
                    peer: Some(addr.to_string()),
                    detail: Some(error.to_string()),
                    ..AuditRecord::default()
                },
            );
            return;
        }
    };
    let Ok(Ok(stream)) = tokio::time::timeout(HANDSHAKE_TIMEOUT, acceptor.accept(stream)).await
    else {
        return;
    };
    let peer = Peer {
        addr,
        identity: Arc::new(identity),
    };
    let service =
        hyper::service::service_fn(move |mut request: hyper::Request<hyper::body::Incoming>| {
            request.extensions_mut().insert(ConnectInfo(peer.clone()));
            router.clone().oneshot(request.map(Body::new))
        });
    let mut builder = hyper::server::conn::http1::Builder::new();
    builder
        .timer(TokioTimer::new())
        .header_read_timeout(state.limits.header_read_timeout);
    let connection = builder.serve_connection(TokioIo::new(stream), service);
    let mut connection = std::pin::pin!(connection);
    tokio::select! {
        _ = connection.as_mut() => {}
        () = tokio::time::sleep(state.limits.connection_lifetime) => {
            connection.as_mut().graceful_shutdown();
            let _ = connection.await;
        }
    }
}

fn spool_directory(database: &FsPath) -> io::Result<PathBuf> {
    let mut name = database
        .file_name()
        .map(|name| name.to_os_string())
        .unwrap_or_default();
    name.push(".spool");
    let spool = database.with_file_name(name);
    // Leftovers are partial uploads and downloads from an earlier process.
    match std::fs::remove_dir_all(&spool) {
        Err(error) if error.kind() != io::ErrorKind::NotFound => return Err(error),
        _ => {}
    }
    std::fs::create_dir_all(&spool)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&spool, std::fs::Permissions::from_mode(0o700))?;
    }
    Ok(spool)
}

/// A spool entry removed when dropped.
struct SpoolEntry(PathBuf);

impl SpoolEntry {
    fn create(spool: &FsPath) -> io::Result<Self> {
        let path = spool.join(Uuid::new_v4().to_string());
        std::fs::create_dir(&path)?;
        Ok(Self(path))
    }
}

impl Drop for SpoolEntry {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Keeps an upload's spooled body and its concurrency permit until the
/// worker is done with them, even if the client has gone.
struct UploadHold {
    _entry: SpoolEntry,
    _permit: OwnedSemaphorePermit,
}

enum Job {
    Upload {
        source: PathBuf,
        idempotency_key: String,
        audit: AuditRecord,
        _hold: UploadHold,
        reply: oneshot::Sender<Result<Uuid, ApiError>>,
    },
    Download {
        file_id: Uuid,
        destination: PathBuf,
        reply: oneshot::Sender<Result<(), ApiError>>,
    },
    Delete {
        file_id: Uuid,
        idempotency_key: String,
        audit: AuditRecord,
        reply: oneshot::Sender<Result<(), ApiError>>,
    },
}

/// Write the `allowed` record of a state change; without it the change
/// does not run.
fn audit_begin(database: &mut Database, record: &AuditRecord) -> Result<(), ApiError> {
    let mut allowed = record.clone();
    allowed.at = now_seconds();
    allowed.outcome = "allowed".into();
    database
        .append_audit(&clipped(allowed))
        .map(|_| ())
        .map_err(ApiError::audit_unavailable)
}

/// Write how a state change ended.
fn audit_end(
    database: &mut Database,
    mut record: AuditRecord,
    result: Result<Option<String>, &ApiError>,
) {
    record.at = now_seconds();
    match result {
        Ok(target) => {
            record.outcome = "completed".into();
            record.target = target.or(record.target);
        }
        Err(error) => {
            record.outcome = "failed".into();
            record.reason = Some(error.code.into());
        }
    }
    if let Err(error) = database.append_audit(&clipped(record.clone())) {
        eprintln!("error: audit write failed ({error}): {record:?}");
    }
}

/// Runs file operations one at a time on a thread that owns the portal
/// (its database handle is not shareable across threads).
struct PortalWorker {
    jobs: mpsc::Sender<Job>,
}

impl PortalWorker {
    fn start(
        database: PathBuf,
        master_key: MasterKey,
        agents: Vec<AgentTarget>,
    ) -> anyhow::Result<Self> {
        let (jobs, mut queue) = mpsc::channel::<Job>(16);
        let (ready, opened) = std::sync::mpsc::channel();
        std::thread::Builder::new()
            .name("portal-worker".into())
            .spawn(move || {
                let runtime = match tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                {
                    Ok(runtime) => runtime,
                    Err(error) => {
                        let _ = ready.send(Err(anyhow::Error::from(error)));
                        return;
                    }
                };
                runtime.block_on(async move {
                    let mut portal = match Portal::open(&database, master_key, agents) {
                        Ok(portal) => {
                            let _ = ready.send(Ok(()));
                            portal
                        }
                        Err(error) => {
                            let _ = ready.send(Err(error.into()));
                            return;
                        }
                    };
                    while let Some(job) = queue.recv().await {
                        run_job(&mut portal, job).await;
                    }
                });
            })?;
        opened
            .recv()
            .map_err(|_| anyhow::anyhow!("the portal worker exited during startup"))??;
        Ok(Self { jobs })
    }

    async fn run<T>(
        &self,
        job: impl FnOnce(oneshot::Sender<Result<T, ApiError>>) -> Job,
    ) -> Result<T, ApiError> {
        let (reply, result) = oneshot::channel();
        self.jobs
            .send(job(reply))
            .await
            .map_err(|_| ApiError::internal("the portal worker stopped"))?;
        result
            .await
            .map_err(|_| ApiError::internal("the portal worker stopped"))?
    }
}

async fn run_job(portal: &mut Portal, job: Job) {
    match job {
        Job::Upload {
            source,
            idempotency_key,
            audit,
            _hold,
            reply,
        } => {
            let result = match audit_begin(&mut portal.database, &audit) {
                Ok(()) => match portal.reload_grant_material() {
                    // Pick up a signing-key rotation made by the CLI.
                    Ok(()) => portal
                        .upload(&source, &idempotency_key)
                        .await
                        .map_err(ApiError::from),
                    Err(error) => Err(error.into()),
                },
                Err(error) => {
                    let _ = reply.send(Err(error));
                    return;
                }
            };
            audit_end(
                &mut portal.database,
                audit,
                result.as_ref().map(|file_id| Some(file_id.to_string())),
            );
            let _ = reply.send(result);
        }
        Job::Download {
            file_id,
            destination,
            reply,
        } => {
            let result = match portal.reload_grant_material() {
                Ok(()) => portal
                    .download(file_id, &destination)
                    .await
                    .map_err(ApiError::from),
                Err(error) => Err(error.into()),
            };
            let _ = reply.send(result);
        }
        Job::Delete {
            file_id,
            idempotency_key,
            audit,
            reply,
        } => {
            let result = match audit_begin(&mut portal.database, &audit) {
                Ok(()) => match portal.reload_grant_material() {
                    Ok(()) => portal
                        .delete(file_id, &idempotency_key)
                        .await
                        .map(|_| ())
                        .map_err(ApiError::from),
                    Err(error) => Err(error.into()),
                },
                Err(error) => {
                    let _ = reply.send(Err(error));
                    return;
                }
            };
            audit_end(&mut portal.database, audit, result.as_ref().map(|()| None));
            let _ = reply.send(result);
        }
    }
}

/// Per-key token buckets.
struct RateLimiter {
    burst: f64,
    per_second: f64,
    buckets: Mutex<HashMap<String, Bucket>>,
}

struct Bucket {
    tokens: f64,
    updated: Instant,
    suppressed: u64,
}

impl RateLimiter {
    fn new(burst: f64, per_second: f64) -> Self {
        Self {
            burst,
            per_second,
            buckets: Mutex::new(HashMap::new()),
        }
    }

    /// Take a token for `key`. On success, returns how many earlier
    /// attempts were refused since the last success.
    fn admit(&self, key: &str) -> Option<u64> {
        let now = Instant::now();
        let mut buckets = self.buckets.lock().unwrap_or_else(|p| p.into_inner());
        if buckets.len() > 4096 {
            // Forget keys whose bucket has refilled and owes no count.
            buckets.retain(|_, bucket| {
                bucket.suppressed > 0
                    || bucket.tokens
                        + now.duration_since(bucket.updated).as_secs_f64() * self.per_second
                        < self.burst
            });
        }
        let bucket = buckets.entry(key.to_owned()).or_insert(Bucket {
            tokens: self.burst,
            updated: now,
            suppressed: 0,
        });
        bucket.tokens = (bucket.tokens
            + now.duration_since(bucket.updated).as_secs_f64() * self.per_second)
            .min(self.burst);
        bucket.updated = now;
        if bucket.tokens >= 1.0 {
            bucket.tokens -= 1.0;
            Some(std::mem::take(&mut bucket.suppressed))
        } else {
            bucket.suppressed += 1;
            None
        }
    }
}

/// Run blocking database work without stalling the async workers.
fn blocking<T>(work: impl FnOnce() -> T) -> T {
    match tokio::runtime::Handle::try_current() {
        Ok(handle) if handle.runtime_flavor() == tokio::runtime::RuntimeFlavor::MultiThread => {
            tokio::task::block_in_place(work)
        }
        _ => work(),
    }
}

/// Cap the free-text fields a peer can influence.
fn clipped(mut record: AuditRecord) -> AuditRecord {
    fn clip(field: &mut Option<String>, max: usize) {
        if let Some(text) = field {
            if text.len() > max {
                let mut end = max;
                while !text.is_char_boundary(end) {
                    end -= 1;
                }
                text.truncate(end);
                text.push('…');
            }
        }
    }
    clip(&mut record.path, 256);
    clip(&mut record.detail, 512);
    clip(&mut record.target, 128);
    clip(&mut record.method, 16);
    record
}

struct ServerState {
    database: Mutex<Database>,
    worker: PortalWorker,
    /// `https://<domain>[:port]`, the only origin browsers may send.
    origin: String,
    /// This node's stable id.
    portal_node: String,
    allow_portal_node_sign_in: bool,
    spool: PathBuf,
    limits: ServeLimits,
    uploads: Arc<Semaphore>,
    downloads: Arc<Semaphore>,
    audit_limiter: RateLimiter,
    sign_in_limiter: RateLimiter,
}

impl ServerState {
    fn with_database<T>(&self, work: impl FnOnce(&mut Database) -> T) -> T {
        blocking(|| work(&mut self.database.lock().unwrap_or_else(|p| p.into_inner())))
    }

    /// Append a record that must exist before a change proceeds.
    fn record_required(&self, record: AuditRecord) -> Result<(), ApiError> {
        self.with_database(|database| database.append_audit(&clipped(record)))
            .map(|_| ())
            .map_err(ApiError::audit_unavailable)
    }

    /// Append a refusal or observation about the peer `key`, rate-limited
    /// per peer. Returns whether it was recorded.
    fn record_limited(&self, key: &str, mut record: AuditRecord) -> bool {
        let Some(suppressed) = self.audit_limiter.admit(key) else {
            return false;
        };
        if suppressed > 0 {
            let note = format!("{suppressed} earlier records from this peer were not written");
            record.detail = Some(match record.detail.take() {
                Some(detail) => format!("{detail}; {note}"),
                None => note,
            });
        }
        eprintln!(
            "audit: {} {} {} from {}",
            record.event,
            record.outcome,
            record.reason.as_deref().unwrap_or(""),
            record.peer.as_deref().unwrap_or("?")
        );
        if let Err(error) = self.with_database(|database| database.append_audit(&clipped(record))) {
            eprintln!("error: audit write failed: {error}");
        }
        true
    }
}

fn router(state: Arc<ServerState>) -> Router {
    Router::new()
        .route("/api/whoami", get(whoami))
        .route("/api/session", post(login).delete(logout))
        .route("/api/keys", get(list_keys).post(create_key))
        .route("/api/keys/{key_id}", delete(revoke_key))
        .route("/api/audit", get(read_audit))
        .route("/api/files", post(upload_file))
        .route(
            "/api/files/{file_id}",
            get(download_file).delete(delete_file),
        )
        .layer(middleware::from_fn_with_state(
            state.clone(),
            strip_identity_headers,
        ))
        .layer(middleware::from_fn(security_headers))
        .with_state(state)
}

/// A refusal or failure: `{"code", "error"}` with a status.
#[derive(Debug)]
pub struct ApiError {
    status: StatusCode,
    code: &'static str,
    message: String,
    clear_session: bool,
}

impl ApiError {
    fn new(status: StatusCode, code: &'static str, message: impl Into<String>) -> Self {
        Self {
            status,
            code,
            message: message.into(),
            clear_session: false,
        }
    }

    /// An internal failure: the detail goes to the log, not the caller.
    fn internal(detail: impl std::fmt::Display) -> Self {
        eprintln!("error: {detail}");
        Self::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            "internal",
            "internal error; see the portal log",
        )
    }

    fn audit_unavailable(detail: impl std::fmt::Display) -> Self {
        eprintln!("error: audit write failed, refusing the change: {detail}");
        Self::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "audit_unavailable",
            "the audit log cannot be written, so no change was made",
        )
    }

    fn bad_request(code: &'static str, message: impl Into<String>) -> Self {
        Self::new(StatusCode::BAD_REQUEST, code, message)
    }

    fn clearing_session(mut self) -> Self {
        self.clear_session = true;
        self
    }
}

impl From<MetadataError> for ApiError {
    fn from(error: MetadataError) -> Self {
        Self::internal(error)
    }
}

impl From<io::Error> for ApiError {
    fn from(error: io::Error) -> Self {
        Self::internal(error)
    }
}

impl From<PortalError> for ApiError {
    fn from(error: PortalError) -> Self {
        let (status, code, message) = match &error {
            PortalError::FileNotVisible(_) | PortalError::AlreadyDeleted(_) => {
                (StatusCode::NOT_FOUND, "file_not_found", "no such file")
            }
            PortalError::IdempotencyConflict => (
                StatusCode::CONFLICT,
                "idempotency_conflict",
                "the idempotency key was used for a different request",
            ),
            PortalError::InsufficientCapacity { .. } => (
                StatusCode::INSUFFICIENT_STORAGE,
                "insufficient_capacity",
                "the storage agents lack capacity for this file",
            ),
            PortalError::AgentRejected { .. }
            | PortalError::AgentIdentityMismatch(_)
            | PortalError::AgentNotVerified(_)
            | PortalError::ReplicaFloorNotMet { .. }
            | PortalError::NoValidReplica { .. }
            | PortalError::Http(_) => (
                StatusCode::BAD_GATEWAY,
                "storage_unavailable",
                "the storage agents could not complete the request",
            ),
            _ => return Self::internal(error),
        };
        eprintln!("warning: {code}: {error}");
        Self::new(status, code, message)
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let mut response = (
            self.status,
            Json(json!({ "code": self.code, "error": self.message })),
        )
            .into_response();
        if self.clear_session {
            response
                .headers_mut()
                .insert(header::SET_COOKIE, clear_session_cookie());
        }
        response
    }
}

fn session_cookie(token: &SessionToken) -> HeaderValue {
    HeaderValue::from_str(&format!(
        "{SESSION_COOKIE}={}; Path=/; Secure; HttpOnly; SameSite=Strict; Max-Age={SESSION_LIFETIME_SECONDS}",
        token.encode()
    ))
    .expect("base64url is a valid header value")
}

fn clear_session_cookie() -> HeaderValue {
    HeaderValue::from_static(
        "__Host-distr-session=; Path=/; Secure; HttpOnly; SameSite=Strict; Max-Age=0",
    )
}

/// The session token from the `Cookie` header(s), if any. More than one
/// session cookie is ambiguous and refused.
fn session_cookie_value(headers: &HeaderMap) -> Result<Option<&str>, ApiError> {
    let mut found = None;
    for value in headers.get_all(header::COOKIE) {
        let Ok(value) = value.to_str() else { continue };
        for pair in value.split(';') {
            if let Some((name, token)) = pair.trim().split_once('=') {
                if name == SESSION_COOKIE {
                    if found.is_some() {
                        return Err(ApiError::bad_request(
                            "ambiguous_credentials",
                            "more than one session cookie",
                        ));
                    }
                    found = Some(token);
                }
            }
        }
    }
    Ok(found)
}

/// Audit fields for a request from `peer`.
fn audit_base(peer: &Peer, method: &Method, path: &str) -> AuditRecord {
    AuditRecord {
        at: now_seconds(),
        peer: Some(peer.addr.to_string()),
        node_stable_id: Some(peer.identity.node_stable_id.clone()),
        node_name: Some(peer.identity.node_name.clone()),
        user_login: Some(peer.identity.user_login.clone()),
        method: Some(method.to_string()),
        path: Some(path.to_owned()),
        ..AuditRecord::default()
    }
}

fn peer_of(parts: &Parts) -> Result<Peer, ApiError> {
    parts
        .extensions
        .get::<ConnectInfo<Peer>>()
        .map(|info| info.0.clone())
        .ok_or_else(|| ApiError::internal("request has no peer identity"))
}

/// Remove identity headers before any handler sees the request.
async fn strip_identity_headers(
    State(state): State<Arc<ServerState>>,
    ConnectInfo(peer): ConnectInfo<Peer>,
    mut request: Request,
    next: Next,
) -> Response {
    let mut found: Vec<String> = request
        .headers()
        .keys()
        .map(|name| name.as_str())
        .filter(|name| name.starts_with("tailscale-") || IDENTITY_HEADERS.contains(name))
        .map(str::to_owned)
        .collect();
    if !found.is_empty() {
        found.sort();
        found.dedup();
        for name in &found {
            request.headers_mut().remove(name.as_str());
        }
        state.record_limited(
            &peer.identity.node_stable_id,
            AuditRecord {
                event: "identity_headers".into(),
                outcome: "observed".into(),
                reason: Some("ignored".into()),
                detail: Some(found.join(",")),
                ..audit_base(&peer, request.method(), request.uri().path())
            },
        );
    }
    next.run(request).await
}

async fn security_headers(request: Request, next: Next) -> Response {
    let mut response = next.run(request).await;
    let headers = response.headers_mut();
    for (name, value) in [
        ("cache-control", "no-store"),
        ("x-content-type-options", "nosniff"),
        ("referrer-policy", "no-referrer"),
        ("x-frame-options", "DENY"),
        (
            "content-security-policy",
            "default-src 'none'; frame-ancestors 'none'",
        ),
    ] {
        headers.insert(name, HeaderValue::from_static(value));
    }
    response
}

enum Principal {
    /// A tailnet peer with no credential.
    Anonymous,
    Session {
        user: UserRow,
        session: SessionRow,
        token: SessionToken,
    },
    ApiKey {
        user: UserRow,
        key: ApiKeyRow,
        scopes: Scopes,
    },
}

/// The authenticated caller of one request.
struct Caller {
    peer: Peer,
    principal: Principal,
    method: Method,
    path: String,
}

impl Caller {
    fn record(&self, event: &str, outcome: &str) -> AuditRecord {
        let mut record = audit_base(&self.peer, &self.method, &self.path);
        record.event = event.into();
        record.outcome = outcome.into();
        match &self.principal {
            Principal::Anonymous => {}
            Principal::Session { user, session, .. } => {
                record.user_id = Some(user.user_id.to_string());
                record.session_id = Some(session.session_id.to_string());
            }
            Principal::ApiKey { user, key, .. } => {
                record.user_id = Some(user.user_id.to_string());
                record.api_key_id = Some(key.key_id.clone());
            }
        }
        record
    }

    /// Record a refusal of this request and return it.
    fn deny(&self, state: &ServerState, error: ApiError) -> ApiError {
        let mut record = self.record("authorize", "denied");
        record.reason = Some(error.code.into());
        state.record_limited(&self.peer.identity.node_stable_id, record);
        error
    }

    /// The user behind a browser session; API keys are refused.
    fn session_user(&self, state: &ServerState) -> Result<&UserRow, ApiError> {
        match &self.principal {
            Principal::Session { user, .. } => Ok(user),
            Principal::ApiKey { .. } => Err(self.deny(
                state,
                ApiError::new(
                    StatusCode::FORBIDDEN,
                    "session_required",
                    "this route needs a browser session, not an API key",
                ),
            )),
            Principal::Anonymous => Err(self.deny(state, unauthenticated())),
        }
    }

    /// A session, or an API key with `right` on `collection`.
    fn require_scope(
        &self,
        state: &ServerState,
        collection: &str,
        right: Right,
    ) -> Result<(), ApiError> {
        match &self.principal {
            Principal::Session { .. } => Ok(()),
            Principal::ApiKey { scopes, .. } if scopes.allows(collection, right) => Ok(()),
            Principal::ApiKey { .. } => Err(self.deny(
                state,
                ApiError::new(
                    StatusCode::FORBIDDEN,
                    "scope_denied",
                    format!(
                        "this API key lacks {}",
                        CollectionScope {
                            collection: collection.into(),
                            right
                        }
                    ),
                ),
            )),
            Principal::Anonymous => Err(self.deny(state, unauthenticated())),
        }
    }

    /// Namespace for idempotency keys, so callers cannot collide.
    fn idempotency_namespace(&self) -> String {
        match &self.principal {
            Principal::Session { user, .. } => format!("api:user:{}", user.user_id),
            Principal::ApiKey { key, .. } => format!("api:key:{}", key.key_id),
            Principal::Anonymous => "api:anonymous".into(),
        }
    }
}

fn unauthenticated() -> ApiError {
    ApiError::new(
        StatusCode::UNAUTHORIZED,
        "unauthenticated",
        "sign in with POST /api/session or present an API key",
    )
}

/// Refuse a request a browser marks as cross-site: an `Origin` other than
/// the portal's, or `Sec-Fetch-Site` other than `same-origin` or `none`.
fn check_same_origin(state: &ServerState, headers: &HeaderMap) -> Result<(), ApiError> {
    let cross_origin = || {
        ApiError::new(
            StatusCode::FORBIDDEN,
            "cross_origin",
            "cross-origin state-changing request",
        )
    };
    if let Some(origin) = headers.get(header::ORIGIN) {
        if origin.as_bytes() != state.origin.as_bytes() {
            return Err(cross_origin());
        }
    }
    if let Some(site) = headers.get("sec-fetch-site") {
        if site != "same-origin" && site != "none" {
            return Err(cross_origin());
        }
    }
    Ok(())
}

impl FromRequestParts<Arc<ServerState>> for Caller {
    type Rejection = ApiError;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &Arc<ServerState>,
    ) -> Result<Self, Self::Rejection> {
        let mut caller = Caller {
            peer: peer_of(parts)?,
            principal: Principal::Anonymous,
            method: parts.method.clone(),
            path: parts.uri.path().to_owned(),
        };
        let refused = |caller: &Caller, error: ApiError| {
            let mut record = caller.record("authenticate", "denied");
            record.reason = Some(error.code.into());
            state.record_limited(&caller.peer.identity.node_stable_id, record);
            error
        };
        let authorization = parts
            .headers
            .get(header::AUTHORIZATION)
            .map(|value| value.to_str().unwrap_or(""));
        let cookie = match session_cookie_value(&parts.headers) {
            Ok(cookie) => cookie,
            Err(error) => return Err(refused(&caller, error)),
        };
        caller.principal = match (authorization, cookie) {
            (Some(_), Some(_)) => {
                return Err(refused(
                    &caller,
                    ApiError::bad_request(
                        "ambiguous_credentials",
                        "send a session cookie or an API key, not both",
                    ),
                ));
            }
            (Some(authorization), None) => {
                let Some(text) = authorization.strip_prefix("Bearer ") else {
                    return Err(refused(
                        &caller,
                        ApiError::new(
                            StatusCode::UNAUTHORIZED,
                            "unsupported_authorization",
                            format!("use Authorization: Bearer {API_KEY_PREFIX}…"),
                        ),
                    ));
                };
                authenticate_api_key(state, &caller.peer, text.trim())
                    .map_err(|error| refused(&caller, error))?
            }
            (None, Some(token)) => {
                let principal = authenticate_session(state, &caller.peer, token)
                    .map_err(|error| refused(&caller, error.clearing_session()))?;
                if !caller.method.is_safe() {
                    let csrf = parts
                        .headers
                        .get(CSRF_HEADER)
                        .and_then(|value| value.to_str().ok())
                        .unwrap_or("");
                    let Principal::Session { token, .. } = &principal else {
                        unreachable!("authenticate_session returns a session")
                    };
                    let csrf_valid = token.csrf_matches(csrf);
                    caller.principal = principal;
                    check_same_origin(state, &parts.headers)
                        .map_err(|error| refused(&caller, error))?;
                    if !csrf_valid {
                        let error = ApiError::new(
                            StatusCode::FORBIDDEN,
                            "csrf_rejected",
                            format!("state-changing requests need the session's {CSRF_HEADER}"),
                        );
                        return Err(refused(&caller, error));
                    }
                    return Ok(caller);
                }
                principal
            }
            (None, None) => Principal::Anonymous,
        };
        Ok(caller)
    }
}

fn authenticate_api_key(
    state: &ServerState,
    peer: &Peer,
    text: &str,
) -> Result<Principal, ApiError> {
    let invalid = |code| ApiError::new(StatusCode::UNAUTHORIZED, code, "API key refused");
    let secret = ApiKeySecret::parse(text).ok_or_else(|| invalid("api_key_malformed"))?;
    state.with_database(|database| {
        let key = database
            .api_key(secret.key_id())?
            .ok_or_else(|| invalid("api_key_unknown"))?;
        if !secret.matches(&key.verifier) {
            return Err(invalid("api_key_mismatch"));
        }
        let now = now_seconds();
        if key.revoked_at.is_some() {
            return Err(invalid("api_key_revoked"));
        }
        if key.expires_at.is_some_and(|expires| now >= expires) {
            return Err(invalid("api_key_expired"));
        }
        // The tailnet node is checked as well as the key, unless the key
        // was created for any node.
        if key
            .node_stable_id
            .as_ref()
            .is_some_and(|node| *node != peer.identity.node_stable_id)
        {
            return Err(ApiError::new(
                StatusCode::FORBIDDEN,
                "api_key_wrong_node",
                "this API key is bound to another tailnet node",
            ));
        }
        let user = database
            .user(key.user_id)?
            .filter(|user| user.disabled_at.is_none())
            .ok_or_else(|| invalid("user_disabled"))?;
        let scopes = Scopes::parse(&key.scopes).map_err(ApiError::internal)?;
        if key
            .last_used_at
            .is_none_or(|used| now - used >= TOUCH_INTERVAL_SECONDS)
        {
            database.touch_api_key(&key.key_id, now)?;
        }
        Ok(Principal::ApiKey { user, key, scopes })
    })
}

fn authenticate_session(
    state: &ServerState,
    peer: &Peer,
    text: &str,
) -> Result<Principal, ApiError> {
    let invalid = |code| ApiError::new(StatusCode::UNAUTHORIZED, code, "session refused");
    let token = SessionToken::parse(text).ok_or_else(|| invalid("session_malformed"))?;
    state.with_database(|database| {
        let session = database
            .session_by_verifier(&token.verifier())?
            .ok_or_else(|| invalid("session_unknown"))?;
        let now = now_seconds();
        if session.revoked_at.is_some() {
            return Err(invalid("session_revoked"));
        }
        if now >= session.expires_at {
            database.revoke_session(session.session_id, now, "expired")?;
            return Err(invalid("session_expired"));
        }
        if now - session.last_seen_at >= SESSION_IDLE_SECONDS {
            database.revoke_session(session.session_id, now, "idle")?;
            return Err(invalid("session_idle"));
        }
        // A session cookie is valid only from the node and user that created
        // it; anywhere else it is treated as stolen and revoked.
        if session.node_stable_id != peer.identity.node_stable_id {
            database.revoke_session(session.session_id, now, "wrong_node")?;
            return Err(invalid("session_wrong_node"));
        }
        let user = database
            .user_by_login(&peer.identity.user_login)?
            .filter(|user| user.user_id == session.user_id && !peer.identity.is_tagged());
        let Some(user) = user else {
            database.revoke_session(session.session_id, now, "wrong_user")?;
            return Err(invalid("session_wrong_user"));
        };
        if user.disabled_at.is_some() {
            return Err(invalid("user_disabled"));
        }
        if now - session.last_seen_at >= TOUCH_INTERVAL_SECONDS {
            database.touch_session(session.session_id, now)?;
        }
        Ok(Principal::Session {
            user,
            session,
            token,
        })
    })
}

#[derive(Serialize)]
struct PeerView<'a> {
    node: &'a str,
    node_name: &'a str,
    user_login: &'a str,
    tags: &'a [String],
}

async fn whoami(caller: Caller) -> Json<serde_json::Value> {
    let peer = PeerView {
        node: &caller.peer.identity.node_stable_id,
        node_name: &caller.peer.identity.node_name,
        user_login: &caller.peer.identity.user_login,
        tags: &caller.peer.identity.tags,
    };
    let principal = match &caller.principal {
        Principal::Anonymous => json!({ "kind": "anonymous" }),
        Principal::Session {
            user,
            session,
            token,
        } => json!({
            "kind": "session",
            "user_id": user.user_id,
            "display_name": user.display_name,
            "expires_at": session.expires_at,
            "csrf_token": token.csrf_token(),
        }),
        Principal::ApiKey { user, key, scopes } => json!({
            "kind": "api_key",
            "user_id": user.user_id,
            "key_id": key.key_id,
            "scopes": scopes.as_strings(),
        }),
    };
    Json(json!({ "peer": peer, "principal": principal }))
}

/// Sign in as the user the connection's WhoIs login belongs to. A session
/// cookie presented here is revoked and replaced (rotation).
async fn login(
    State(state): State<Arc<ServerState>>,
    ConnectInfo(peer): ConnectInfo<Peer>,
    method: Method,
    headers: HeaderMap,
) -> Result<Response, ApiError> {
    let mut record = audit_base(&peer, &method, "/api/session");
    record.event = "login".into();
    let node = peer.identity.node_stable_id.as_str();
    let refuse = |mut record: AuditRecord, error: ApiError| {
        record.outcome = "denied".into();
        record.reason = Some(error.code.into());
        state.record_limited(node, record);
        error
    };
    if let Err(error) = check_same_origin(&state, &headers) {
        return Err(refuse(record, error));
    }
    if headers.contains_key(header::AUTHORIZATION) {
        let error = ApiError::new(
            StatusCode::FORBIDDEN,
            "session_login_with_api_key",
            "API keys cannot open browser sessions",
        );
        return Err(refuse(record, error));
    }
    if peer.identity.is_tagged() {
        let error = ApiError::new(
            StatusCode::FORBIDDEN,
            "tagged_node",
            "a tagged node has no user identity; use an API key",
        );
        return Err(refuse(record, error));
    }
    if node == state.portal_node && !state.allow_portal_node_sign_in {
        let error = ApiError::new(
            StatusCode::FORBIDDEN,
            "portal_node",
            "sign in from another device: any process on the portal's node would pass WhoIs \
             as its owner (see --allow-portal-node-sign-in)",
        );
        return Err(refuse(record, error));
    }
    if state.sign_in_limiter.admit(node).is_none() {
        let error = ApiError::new(
            StatusCode::TOO_MANY_REQUESTS,
            "too_many_sign_ins",
            "too many sign-ins from this node; wait and retry",
        );
        return Err(refuse(record, error));
    }
    let user = state.with_database(|database| database.user_by_login(&peer.identity.user_login))?;
    let Some(user) = user else {
        let error = ApiError::new(
            StatusCode::FORBIDDEN,
            "unknown_user",
            format!(
                "tailnet user {} is not a portal user",
                peer.identity.user_login
            ),
        );
        return Err(refuse(record, error));
    };
    if user.disabled_at.is_some() {
        let error = ApiError::new(StatusCode::FORBIDDEN, "user_disabled", "user is disabled");
        return Err(refuse(record, error));
    }
    let presented = session_cookie_value(&headers)
        .ok()
        .flatten()
        .and_then(SessionToken::parse);
    let now = now_seconds();
    let token = SessionToken::generate();
    let session = SessionRow {
        session_id: Uuid::new_v4(),
        user_id: user.user_id,
        node_stable_id: peer.identity.node_stable_id.clone(),
        created_at: now,
        last_seen_at: now,
        expires_at: now + SESSION_LIFETIME_SECONDS,
        revoked_at: None,
    };
    record.outcome = "allowed".into();
    record.user_id = Some(user.user_id.to_string());
    record.session_id = Some(session.session_id.to_string());
    state.record_required(record)?;
    state.with_database(|database| -> Result<(), MetadataError> {
        if let Some(old) = presented {
            if let Some(old) = database.session_by_verifier(&old.verifier())? {
                if old.user_id == user.user_id {
                    database.revoke_session(old.session_id, now, "rotated")?;
                }
            }
        }
        database.prune_sessions(now - SESSION_RETENTION_SECONDS)?;
        database.insert_session(&session, &token.verifier())
    })?;
    let mut response = (
        StatusCode::CREATED,
        Json(json!({
            "user_id": user.user_id,
            "display_name": user.display_name,
            "csrf_token": token.csrf_token(),
            "expires_at": session.expires_at,
            "idle_timeout_seconds": SESSION_IDLE_SECONDS,
        })),
    )
        .into_response();
    response
        .headers_mut()
        .insert(header::SET_COOKIE, session_cookie(&token));
    Ok(response)
}

async fn logout(
    State(state): State<Arc<ServerState>>,
    caller: Caller,
) -> Result<Response, ApiError> {
    caller.session_user(&state)?;
    let Principal::Session { session, .. } = &caller.principal else {
        unreachable!("session_user admits only sessions")
    };
    state.record_required(caller.record("logout", "allowed"))?;
    state.with_database(|database| {
        database.revoke_session(session.session_id, now_seconds(), "logout")
    })?;
    let mut response = StatusCode::NO_CONTENT.into_response();
    response
        .headers_mut()
        .insert(header::SET_COOKIE, clear_session_cookie());
    Ok(response)
}

fn key_view(key: &ApiKeyRow) -> serde_json::Value {
    json!({
        "key_id": key.key_id,
        "name": key.name,
        "scopes": key.scopes.split_whitespace().collect::<Vec<_>>(),
        "node": key.node_stable_id,
        "any_node": key.node_stable_id.is_none(),
        "created_at": key.created_at,
        "expires_at": key.expires_at,
        "revoked_at": key.revoked_at,
        "last_used_at": key.last_used_at,
    })
}

async fn list_keys(
    State(state): State<Arc<ServerState>>,
    caller: Caller,
) -> Result<Json<serde_json::Value>, ApiError> {
    let user_id = caller.session_user(&state)?.user_id;
    let keys = state.with_database(|database| database.api_keys(Some(user_id)))?;
    Ok(Json(
        json!({ "keys": keys.iter().map(key_view).collect::<Vec<_>>() }),
    ))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CreateKey {
    name: String,
    scopes: Vec<String>,
    /// Accept the key only from this tailnet node: a stable node id, or
    /// `self` for the node making this request.
    node: Option<String>,
    /// Accept the key from any tailnet node instead.
    #[serde(default)]
    any_node: bool,
    expires_in_days: Option<u32>,
}

/// Where an API key may be used from.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum KeyNode {
    /// Only this tailnet node (stable node id).
    Node(String),
    /// Any tailnet node: only the key is checked.
    Any,
}

/// Validate a new API key and mint its secret; the caller audits and then
/// stores the row. Shared by the route and the CLI.
pub fn new_api_key(
    user_id: Uuid,
    name: &str,
    scopes: &[String],
    node: KeyNode,
    expires_in_days: Option<u32>,
    now: i64,
) -> Result<(ApiKeyRow, ApiKeySecret), String> {
    if name.is_empty() || name.len() > 64 || name.chars().any(char::is_control) {
        return Err("name must be 1 to 64 printable characters".into());
    }
    let scopes = Scopes::new(
        scopes
            .iter()
            .map(|scope| scope.parse())
            .collect::<Result<Vec<_>, _>>()?,
    )?;
    let node = match node {
        KeyNode::Node(node)
            if !node.is_empty()
                && node.len() <= 64
                && node.bytes().all(|byte| byte.is_ascii_alphanumeric()) =>
        {
            Some(node)
        }
        KeyNode::Node(_) => return Err("node must be a tailnet stable node id".into()),
        KeyNode::Any => None,
    };
    if expires_in_days == Some(0) {
        return Err("expires_in_days must be at least 1".into());
    }
    let secret = ApiKeySecret::generate();
    let row = ApiKeyRow {
        key_id: secret.key_id().to_owned(),
        user_id,
        name: name.to_owned(),
        verifier: secret.verifier(),
        scopes: scopes.to_string(),
        node_stable_id: node,
        created_at: now,
        expires_at: expires_in_days.map(|days| now + i64::from(days) * 86_400),
        revoked_at: None,
        last_used_at: None,
    };
    Ok((row, secret))
}

async fn create_key(
    State(state): State<Arc<ServerState>>,
    caller: Caller,
    request: Result<Json<CreateKey>, JsonRejection>,
) -> Result<Response, ApiError> {
    let user_id = caller.session_user(&state)?.user_id;
    let bad = |message: String| caller.deny(&state, ApiError::bad_request("bad_request", message));
    let Json(request) = request.map_err(|rejection| bad(rejection.body_text()))?;
    let node = match (request.node, request.any_node) {
        (Some(node), false) if node == "self" => {
            KeyNode::Node(caller.peer.identity.node_stable_id.clone())
        }
        (Some(node), false) => KeyNode::Node(node),
        (None, true) => KeyNode::Any,
        _ => {
            return Err(bad(
                "name the node the key will be used from (`node`, a stable node id or \
                 \"self\") or set `any_node`, not both"
                    .into(),
            ))
        }
    };
    let (row, secret) = new_api_key(
        user_id,
        &request.name,
        &request.scopes,
        node,
        request.expires_in_days,
        now_seconds(),
    )
    .map_err(bad)?;
    let mut record = caller.record("api_key_create", "allowed");
    record.target = Some(row.key_id.clone());
    record.detail = Some(format!(
        "{}; node {}",
        row.scopes,
        row.node_stable_id.as_deref().unwrap_or("any")
    ));
    state.record_required(record)?;
    state.with_database(|database| database.insert_api_key(&row))?;
    let mut view = key_view(&row);
    view["key"] = json!(secret.encode().as_str());
    Ok((StatusCode::CREATED, Json(view)).into_response())
}

async fn revoke_key(
    State(state): State<Arc<ServerState>>,
    caller: Caller,
    Path(key_id): Path<String>,
) -> Result<StatusCode, ApiError> {
    let user_id = caller.session_user(&state)?.user_id;
    let live = state
        .with_database(|database| database.api_key(&key_id))?
        .is_some_and(|key| key.user_id == user_id && key.revoked_at.is_none());
    if !live {
        return Err(caller.deny(
            &state,
            ApiError::new(
                StatusCode::NOT_FOUND,
                "api_key_not_found",
                "you have no live API key with that id",
            ),
        ));
    }
    let mut record = caller.record("api_key_revoke", "allowed");
    record.target = Some(key_id.clone());
    state.record_required(record)?;
    state
        .with_database(|database| database.revoke_api_key(&key_id, Some(user_id), now_seconds()))?;
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Deserialize)]
struct AuditQuery {
    #[serde(default)]
    after: i64,
    limit: Option<usize>,
}

async fn read_audit(
    State(state): State<Arc<ServerState>>,
    caller: Caller,
    Query(query): Query<AuditQuery>,
) -> Result<Json<serde_json::Value>, ApiError> {
    caller.session_user(&state)?;
    let limit = query.limit.unwrap_or(100).min(MAX_AUDIT_PAGE);
    let records = state.with_database(|database| database.audit_records(query.after, limit))?;
    Ok(Json(json!({
        "records": records.iter().map(audit_view).collect::<Vec<_>>()
    })))
}

pub fn audit_view(record: &AuditRecord) -> serde_json::Value {
    json!({
        "seq": record.seq,
        "at": record.at,
        "event": record.event,
        "outcome": record.outcome,
        "reason": record.reason,
        "peer": record.peer,
        "node": record.node_stable_id,
        "node_name": record.node_name,
        "user_login": record.user_login,
        "user_id": record.user_id,
        "session_id": record.session_id,
        "api_key_id": record.api_key_id,
        "method": record.method,
        "path": record.path,
        "target": record.target,
        "detail": record.detail,
    })
}

fn idempotency_key(headers: &HeaderMap) -> Result<Option<String>, ApiError> {
    let Some(value) = headers.get("idempotency-key") else {
        return Ok(None);
    };
    let value = value
        .to_str()
        .ok()
        .filter(|value| {
            !value.is_empty()
                && value.len() <= MAX_IDEMPOTENCY_KEY
                && value.bytes().all(|byte| byte.is_ascii_graphic())
        })
        .ok_or_else(|| {
            ApiError::bad_request(
                "bad_idempotency_key",
                "Idempotency-Key must be 1 to 128 visible ASCII characters",
            )
        })?;
    Ok(Some(value.to_owned()))
}

#[derive(Deserialize)]
struct UploadQuery {
    name: String,
}

fn valid_file_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 255
        && name != "."
        && name != ".."
        && !name.contains(['/', '\\', '\0'])
        && !name.chars().any(char::is_control)
}

fn busy(what: &str) -> ApiError {
    ApiError::new(
        StatusCode::SERVICE_UNAVAILABLE,
        "busy",
        format!("too many {what} in progress; retry shortly"),
    )
}

async fn upload_file(
    State(state): State<Arc<ServerState>>,
    caller: Caller,
    query: Result<Query<UploadQuery>, axum::extract::rejection::QueryRejection>,
    headers: HeaderMap,
    body: Body,
) -> Result<Response, ApiError> {
    caller.require_scope(&state, FILES_COLLECTION, Right::Write)?;
    let deny = |error| caller.deny(&state, error);
    let name = match query {
        Ok(Query(query)) if valid_file_name(&query.name) => query.name,
        _ => {
            return Err(deny(ApiError::bad_request(
                "bad_file_name",
                "name must be a plain file name (?name=…)",
            )))
        }
    };
    let key = idempotency_key(&headers).map_err(deny)?.ok_or_else(|| {
        deny(ApiError::bad_request(
            "idempotency_key_required",
            "uploads need an Idempotency-Key",
        ))
    })?;
    let limit = state.limits.max_upload_bytes;
    let declared = headers
        .get(header::CONTENT_LENGTH)
        .and_then(|value| value.to_str().ok()?.parse::<u64>().ok());
    if declared.is_some_and(|declared| declared > limit) {
        return Err(deny(too_large(limit)));
    }
    let permit = state
        .uploads
        .clone()
        .try_acquire_owned()
        .map_err(|_| deny(busy("uploads")))?;
    let (free, _) = filesystem_space(&state.spool)?;
    let needed = declared.unwrap_or(limit) + state.limits.spool_floor_bytes;
    if free < needed {
        return Err(deny(ApiError::new(
            StatusCode::INSUFFICIENT_STORAGE,
            "spool_full",
            "the portal's spool lacks space for this upload",
        )));
    }
    let entry = SpoolEntry::create(&state.spool)?;
    let source = entry.0.join(&name);
    let received = spool_body(&source, body, limit).await.map_err(deny)?;
    let idempotency_key = format!("{}:{key}", caller.idempotency_namespace());
    let mut audit = caller.record("file_upload", "allowed");
    audit.detail = Some(format!("{received} bytes, name {name:?}"));
    state
        .worker
        .run(|reply| Job::Upload {
            source,
            idempotency_key,
            audit,
            _hold: UploadHold {
                _entry: entry,
                _permit: permit,
            },
            reply,
        })
        .await
        .map(|file_id| (StatusCode::CREATED, Json(json!({ "file_id": file_id }))).into_response())
}

fn too_large(limit: u64) -> ApiError {
    ApiError::new(
        StatusCode::PAYLOAD_TOO_LARGE,
        "upload_too_large",
        format!("uploads are limited to {limit} bytes"),
    )
}

/// Stream a request body to `path`, refusing more than `limit` bytes.
async fn spool_body(path: &FsPath, body: Body, limit: u64) -> Result<u64, ApiError> {
    let mut file = tokio::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .await?;
    let mut body = body;
    let mut total = 0_u64;
    while let Some(frame) = body.frame().await {
        let frame = frame.map_err(|_| {
            ApiError::bad_request("body_read_failed", "the request body ended early")
        })?;
        if let Ok(data) = frame.into_data() {
            total += data.len() as u64;
            if total > limit {
                return Err(too_large(limit));
            }
            file.write_all(&data).await?;
        }
    }
    file.sync_all().await?;
    Ok(total)
}

/// A response body that holds a download slot until it has been sent.
struct HeldStream<S> {
    inner: S,
    _permit: OwnedSemaphorePermit,
}

impl<S: futures_core::Stream + Unpin> futures_core::Stream for HeldStream<S> {
    type Item = S::Item;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<S::Item>> {
        Pin::new(&mut self.inner).poll_next(cx)
    }
}

async fn download_file(
    State(state): State<Arc<ServerState>>,
    caller: Caller,
    Path(file_id): Path<Uuid>,
) -> Result<Response, ApiError> {
    caller.require_scope(&state, FILES_COLLECTION, Right::Read)?;
    let permit = state
        .downloads
        .clone()
        .try_acquire_owned()
        .map_err(|_| caller.deny(&state, busy("downloads")))?;
    let entry = SpoolEntry::create(&state.spool)?;
    let destination = entry.0.join("file");
    let target = destination.clone();
    state
        .worker
        .run(|reply| Job::Download {
            file_id,
            destination: target,
            reply,
        })
        .await?;
    // The open handle keeps the bytes readable after the spool entry goes.
    let file = tokio::fs::File::open(&destination).await?;
    let length = file.metadata().await?.len();
    drop(entry);
    let mut response = Body::from_stream(HeldStream {
        inner: tokio_util::io::ReaderStream::new(file),
        _permit: permit,
    })
    .into_response();
    let headers = response.headers_mut();
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/octet-stream"),
    );
    headers.insert(header::CONTENT_LENGTH, HeaderValue::from(length));
    Ok(response)
}

async fn delete_file(
    State(state): State<Arc<ServerState>>,
    caller: Caller,
    Path(file_id): Path<Uuid>,
    headers: HeaderMap,
) -> Result<Json<serde_json::Value>, ApiError> {
    caller.require_scope(&state, FILES_COLLECTION, Right::Write)?;
    let key = idempotency_key(&headers)
        .map_err(|error| caller.deny(&state, error))?
        .unwrap_or_else(|| format!("delete:{file_id}"));
    let idempotency_key = format!("{}:{key}", caller.idempotency_namespace());
    let mut audit = caller.record("file_delete", "allowed");
    audit.target = Some(file_id.to_string());
    state
        .worker
        .run(|reply| Job::Delete {
            file_id,
            idempotency_key,
            audit,
            reply,
        })
        .await?;
    Ok(Json(json!({ "file_id": file_id, "deleted": true })))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_rate_limiter_allows_a_burst_then_counts_what_it_drops() {
        let limiter = RateLimiter::new(3.0, 0.0);
        assert_eq!(limiter.admit("a"), Some(0));
        assert_eq!(limiter.admit("a"), Some(0));
        assert_eq!(limiter.admit("a"), Some(0));
        assert_eq!(limiter.admit("a"), None);
        assert_eq!(limiter.admit("a"), None);
        assert_eq!(limiter.admit("b"), Some(0), "keys are independent");
        let refilling = RateLimiter::new(1.0, 1000.0);
        assert_eq!(refilling.admit("a"), Some(0));
        let _ = refilling.admit("a");
        std::thread::sleep(Duration::from_millis(5));
        assert!(refilling.admit("a").is_some());
    }

    #[test]
    fn clipped_records_cap_peer_controlled_fields() {
        let record = clipped(AuditRecord {
            path: Some("é".repeat(1000)),
            detail: Some("x".repeat(10_000)),
            ..AuditRecord::default()
        });
        assert!(record.path.unwrap().len() <= 256 + '…'.len_utf8());
        assert!(record.detail.unwrap().len() <= 512 + '…'.len_utf8());
    }

    #[test]
    fn connection_slots_cap_each_address_and_the_total() {
        let slots = Arc::new(ConnectionSlots::default());
        let limits = ServeLimits {
            max_connections: 3,
            max_connections_per_address: 2,
            ..ServeLimits::default()
        };
        let a: IpAddr = "100.64.0.1".parse().unwrap();
        let b: IpAddr = "100.64.0.2".parse().unwrap();
        let first = slots.claim(a, &limits).unwrap();
        let _second = slots.claim(a, &limits).unwrap();
        assert!(slots.claim(a, &limits).is_none(), "per-address cap");
        let _third = slots.claim(b, &limits).unwrap();
        assert!(slots.claim(b, &limits).is_none(), "total cap");
        drop(first);
        assert!(slots.claim(a, &limits).is_some(), "released on drop");
    }
}
