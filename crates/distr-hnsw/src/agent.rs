use std::{
    net::SocketAddr,
    path::PathBuf,
    sync::{
        atomic::{AtomicI64, Ordering},
        Arc,
    },
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use axum::{
    body::Bytes,
    extract::{ConnectInfo, DefaultBodyLimit, Path, Query, State},
    http::{header::AUTHORIZATION, HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::{get, post, put},
    Json, Router,
};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use serde::{Deserialize, Serialize};
use tokio::net::TcpListener;

use crate::{
    durability::{DurableStore, InventoryPage, StoreError, StoreFailpoint, MAX_INVENTORY_PAGE},
    grant::{token_from_header, Expectation, GrantError, Operation, Scope, SignedKeySet},
    object::{ObjectHash, ObjectKind},
    tailscale::{self, BindMode, BindSpec, LocalApi},
    trust::{AgentTrust, TrustError, TrustStore},
    CHUNK_SIZE,
};

const GIB: u64 = 1024 * 1024 * 1024;
const MIB: u64 = 1024 * 1024;
const ENOSPC_WINDOW_SECONDS: i64 = 15 * 60;

/// Operator-configured capacity policy for one volume (DESIGN §11.1,
/// decision 3). Defaults: reserve = max(10% of quota or filesystem, 1 GiB),
/// hard floor = max(1%, 256 MiB).
#[derive(Clone, Copy, Debug, Default)]
pub struct CapacityConfig {
    pub quota_bytes: Option<u64>,
    pub reserve_bytes: Option<u64>,
    pub hard_floor_bytes: Option<u64>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CapacityState {
    Ok,
    Warning,
    AdmissionPaused,
    EnospcObserved,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CapacityLimit {
    Quota,
    Filesystem,
    Reserve,
    Enospc,
}

/// Capacity as reported by `GET /v1/health`.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct CapacityReport {
    pub quota_bytes: Option<u64>,
    pub used_bytes: u64,
    pub fs_free_bytes: u64,
    pub reserve_bytes: u64,
    pub hard_floor_bytes: u64,
    /// Bytes a regular chunk may still consume.
    pub effective_free_bytes: u64,
    /// Bytes a control object (manifest, marker) or repair may consume.
    pub control_free_bytes: u64,
    pub state: CapacityState,
    pub last_refusal_at: Option<i64>,
}

#[derive(Clone)]
struct AgentState {
    store: Arc<DurableStore>,
    trust: Arc<TrustStore>,
    /// LocalAPI for WhoIs; `Some` only on a tailnet-bound agent.
    api: Option<LocalApi>,
    health: AgentHealth,
    bind_mode: BindMode,
    capacity: CapacityConfig,
    last_refusal_at: Arc<AtomicI64>,
    last_enospc_at: Arc<AtomicI64>,
}

impl AgentState {
    fn capacity(&self) -> Result<CapacityReport, StoreError> {
        let used = self.store.used_bytes();
        let (fs_free, fs_total) = self.store.filesystem_free_bytes()?;
        let basis = self.capacity.quota_bytes.unwrap_or(fs_total).max(1);
        let reserve = self
            .capacity
            .reserve_bytes
            .unwrap_or_else(|| (basis / 10).max(GIB));
        let hard_floor = self
            .capacity
            .hard_floor_bytes
            .unwrap_or_else(|| (basis / 100).max(256 * MIB));
        let quota_headroom = self
            .capacity
            .quota_bytes
            .map_or(u64::MAX, |quota| quota.saturating_sub(used));
        let raw_free = quota_headroom.min(fs_free);
        let effective = raw_free.saturating_sub(reserve);
        let control = raw_free.saturating_sub(hard_floor);
        let now = now_seconds();
        let last_enospc = self.last_enospc_at.load(Ordering::Relaxed);
        let state = if last_enospc > 0 && now - last_enospc < ENOSPC_WINDOW_SECONDS {
            CapacityState::EnospcObserved
        } else if effective < CHUNK_SIZE as u64 {
            CapacityState::AdmissionPaused
        } else if raw_free < basis / 5 {
            CapacityState::Warning
        } else {
            CapacityState::Ok
        };
        let last_refusal = self.last_refusal_at.load(Ordering::Relaxed);
        Ok(CapacityReport {
            quota_bytes: self.capacity.quota_bytes,
            used_bytes: used,
            fs_free_bytes: fs_free,
            reserve_bytes: reserve,
            hard_floor_bytes: hard_floor,
            effective_free_bytes: effective,
            control_free_bytes: control,
            state,
            last_refusal_at: (last_refusal > 0).then_some(last_refusal),
        })
    }

    fn admit(&self, kind: ObjectKind, size: u64) -> Result<(), AgentError> {
        let report = self.capacity()?;
        let allowed = match kind {
            ObjectKind::Chunk => report.effective_free_bytes,
            ObjectKind::Manifest | ObjectKind::DeletionMarker => report.control_free_bytes,
        };
        if size <= allowed {
            return Ok(());
        }
        self.last_refusal_at.store(now_seconds(), Ordering::Relaxed);
        let limiting =
            if self.capacity.quota_bytes.is_some_and(|quota| {
                quota.saturating_sub(report.used_bytes) <= report.fs_free_bytes
            }) {
                if size
                    <= report
                        .quota_bytes
                        .unwrap_or(0)
                        .saturating_sub(report.used_bytes)
                {
                    CapacityLimit::Reserve
                } else {
                    CapacityLimit::Quota
                }
            } else if size <= report.fs_free_bytes {
                CapacityLimit::Reserve
            } else {
                CapacityLimit::Filesystem
            };
        Err(AgentError::InsufficientCapacity {
            required_bytes: size,
            available_bytes: allowed,
            limiting,
            volume_id: self.health.id.clone(),
        })
    }
}

fn now_seconds() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs() as i64)
        .unwrap_or(0)
}

/// Operator-configured agent identity. The incarnation is not configured; it
/// is loaded from the volume so a wiped volume cannot impersonate its past.
#[derive(Clone, Debug, Serialize)]
pub struct AgentIdentity {
    pub id: String,
    pub failure_domain: String,
}

/// Identity reported by `GET /v1/health`, including the immutable incarnation
/// of the volume this agent serves.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct AgentHealth {
    pub id: String,
    pub failure_domain: String,
    pub incarnation_id: String,
}

/// Full health response: identity, capacity, and how the agent is bound.
#[derive(Clone, Debug, Serialize)]
pub struct AgentHealthResponse {
    pub id: String,
    pub failure_domain: String,
    pub incarnation_id: String,
    pub capacity: CapacityReport,
    /// `tailnet`, or `insecure_loopback` when the operator chose loopback.
    pub bind_mode: &'static str,
}

/// Serve an agent on a listener the caller bound to loopback (tests and
/// single-host development). `join_token` arms the one-time join; an agent
/// refuses every request except `POST /v1/join` until it is joined.
pub async fn serve_agent(
    listener: TcpListener,
    volume: PathBuf,
    identity: AgentIdentity,
    join_token: &str,
) -> anyhow::Result<()> {
    serve_agent_with_capacity(
        listener,
        volume,
        identity,
        CapacityConfig::default(),
        join_token,
    )
    .await
}

/// [`serve_agent`] with a capacity policy.
pub async fn serve_agent_with_capacity(
    listener: TcpListener,
    volume: PathBuf,
    identity: AgentIdentity,
    capacity: CapacityConfig,
    join_token: &str,
) -> anyhow::Result<()> {
    let store = DurableStore::open(volume)?;
    let trust = TrustStore::open(store.root())?;
    trust.arm_join_token(join_token)?;
    let router = prepared_router(
        store,
        trust,
        identity,
        capacity,
        BindMode::InsecureLoopback,
        None,
    )?;
    serve(listener, router).await?;
    Ok(())
}

async fn serve(listener: TcpListener, router: Router) -> std::io::Result<()> {
    axum::serve(
        listener,
        router.into_make_service_with_connect_info::<SocketAddr>(),
    )
    .await
}

fn prepared_router(
    store: DurableStore,
    trust: TrustStore,
    identity: AgentIdentity,
    capacity: CapacityConfig,
    bind_mode: BindMode,
    api: Option<LocalApi>,
) -> anyhow::Result<Router> {
    let swept = store.sweep_stale_temporaries(std::time::Duration::from_secs(3600))?;
    if swept > 0 {
        eprintln!("removed {swept} stale temporary file(s) left by interrupted writes");
    }
    let incarnation_id = store.load_or_create_incarnation()?;
    let state = AgentState {
        store: Arc::new(store),
        trust: Arc::new(trust),
        api,
        health: AgentHealth {
            id: identity.id,
            failure_domain: identity.failure_domain,
            incarnation_id,
        },
        bind_mode,
        capacity,
        last_refusal_at: Arc::new(AtomicI64::new(0)),
        last_enospc_at: Arc::new(AtomicI64::new(0)),
    };
    Ok(Router::new()
        .route("/v1/join", post(join))
        .route("/v1/trust", put(update_trust))
        .route("/v1/health", get(health))
        .route("/v1/inventory/{kind}", get(inventory))
        .route(
            "/v1/objects/{kind}/{hash}",
            get(get_object).put(put_object).delete(delete_object),
        )
        .layer(DefaultBodyLimit::max(crate::MAX_OBJECT_BYTES))
        .with_state(state))
}

/// How an agent listens (docs/m2-implementation-plan.md, decision 2).
#[derive(Clone, Debug)]
pub struct AgentBind {
    pub spec: BindSpec,
    pub insecure_loopback: bool,
    pub api: LocalApi,
    /// How long to wait for `tailscaled` at startup.
    pub tailnet_wait: Duration,
    /// How often to check that this node still owns the bound address.
    pub address_poll: Duration,
}

/// A join token given to an agent at startup.
#[derive(Clone, Debug)]
pub struct JoinToken {
    pub token: String,
    /// Forget the portal this volume trusts before arming the token
    /// (`--reset-trust`).
    pub reset_trust: bool,
}

/// The agent's tailnet address changed or vanished while it was serving.
#[derive(Debug, thiserror::Error)]
#[error("tailnet address {0} is no longer this node's; restart to rebind")]
pub struct TailnetAddressChanged(pub std::net::IpAddr);

pub async fn bind_and_serve_agent(
    bind: AgentBind,
    volume: PathBuf,
    identity: AgentIdentity,
    capacity: CapacityConfig,
    failpoint: Option<StoreFailpoint>,
    join_token: Option<JoinToken>,
) -> anyhow::Result<()> {
    let mut store = DurableStore::open(volume)?;
    let trust = TrustStore::open(store.root())?;
    match join_token {
        Some(JoinToken {
            token,
            reset_trust: true,
        }) => {
            eprintln!(
                "warning: --reset-trust: forgetting the current portal; waiting for a new join"
            );
            trust.reset_and_arm(&token)?;
        }
        Some(JoinToken { token, .. }) => trust.arm_join_token(&token)?,
        None if !trust.is_joined() => anyhow::bail!(
            "agent is not joined; start it with a join token from `portal agent token` \
             (DISTR_HNSW_JOIN_TOKEN or --join-token-file)"
        ),
        None => {}
    }
    let (listener, mode) = tailscale::bind_listener(
        bind.spec,
        bind.insecure_loopback,
        &bind.api,
        bind.tailnet_wait,
    )
    .await?;
    let local = listener.local_addr()?;
    if mode == BindMode::InsecureLoopback {
        eprintln!("warning: agent bound to loopback {local} with --insecure-loopback");
    }
    if let Some(failpoint) = failpoint {
        store = store.with_failpoint(failpoint);
    }
    let api = (mode == BindMode::Tailnet).then(|| bind.api.clone());
    let router = prepared_router(store, trust, identity, capacity, mode, api)?;
    if mode == BindMode::InsecureLoopback {
        serve(listener, router).await?;
        return Ok(());
    }
    tokio::select! {
        served = serve(listener, router) => served.map_err(anyhow::Error::from),
        lost = tailscale::address_lost(bind.api, local.ip(), bind.address_poll) => {
            Err(TailnetAddressChanged(lost).into())
        }
    }
}

/// Header on a 409 telling the portal to push its current key set; the
/// value is this agent's key-set version.
pub const KEY_SET_OUTDATED_HEADER: &str = "x-distr-key-set-outdated";

impl AgentState {
    /// On a tailnet agent, the caller must be the pinned portal node
    /// (WhoIs on the socket address; identity headers are never read).
    async fn check_peer(&self, trust: &AgentTrust, peer: SocketAddr) -> Result<(), AgentError> {
        let Some(api) = &self.api else {
            return Ok(());
        };
        let Some(portal) = &trust.portal_node else {
            return Err(AgentError::PeerRejected(
                "joined without a portal node".into(),
            ));
        };
        let identity = api
            .whois(peer)
            .await
            .map_err(|error| AgentError::PeerRejected(error.to_string()))?;
        if &identity.node_stable_id != portal {
            return Err(AgentError::PeerRejected(format!(
                "{} is not the portal node",
                identity.node_name
            )));
        }
        Ok(())
    }

    /// Check the caller and the request's grant (decision 3).
    async fn authorize(
        &self,
        peer: SocketAddr,
        headers: &HeaderMap,
        operation: Operation,
        scope: Scope,
    ) -> Result<(), AgentError> {
        let trust = self.trust.current().ok_or(AgentError::NotJoined)?;
        self.check_peer(&trust, peer).await?;
        let key_set = &trust.key_set.key_set;
        let rejected = |error| AgentError::Grant {
            error,
            key_set_version: key_set.version,
        };
        let token = headers
            .get(AUTHORIZATION)
            .and_then(|value| value.to_str().ok())
            .and_then(token_from_header)
            .ok_or_else(|| rejected(GrantError::Missing))?;
        let expect = Expectation {
            agent_id: &self.health.id,
            operation,
            scope: &scope,
        };
        key_set
            .verify(token, &expect, now_seconds())
            .map(|_| ())
            .map_err(rejected)
    }
}

/// Body of `POST /v1/join`.
#[derive(Deserialize, Serialize)]
pub struct JoinRequest {
    pub token: String,
    /// A self-signed key set ([`SignedKeySet::to_bytes`]), base64url.
    pub key_set: String,
}

/// Response to a successful join.
#[derive(Debug, Deserialize, Serialize)]
pub struct JoinResponse {
    pub agent_id: String,
    pub incarnation_id: String,
    pub key_set_version: u64,
    /// Whether the agent pinned the caller's tailnet node as the portal.
    pub portal_node_pinned: bool,
}

async fn join(
    State(state): State<AgentState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    Json(request): Json<JoinRequest>,
) -> Result<Json<JoinResponse>, AgentError> {
    // A joined agent refuses before looking at the request.
    if state.trust.is_joined() {
        return Err(TrustError::AlreadyJoined.into());
    }
    let bytes = URL_SAFE_NO_PAD
        .decode(request.key_set.trim())
        .map_err(|_| AgentError::BadRequest("key set is not base64url"))?;
    let key_set = SignedKeySet::from_bytes(&bytes)
        .map_err(|_| AgentError::BadRequest("malformed key set"))?;
    let portal_node = match &state.api {
        Some(api) => Some(
            api.whois(peer)
                .await
                .map_err(|error| AgentError::PeerRejected(error.to_string()))?
                .node_stable_id,
        ),
        None => None,
    };
    let version = key_set.key_set.version;
    let pinned = portal_node.is_some();
    let trust = state.trust.clone();
    let token = request.token;
    tokio::task::spawn_blocking(move || trust.join(&token, key_set, portal_node))
        .await
        .map_err(AgentError::Join)??;
    Ok(Json(JoinResponse {
        agent_id: state.health.id.clone(),
        incarnation_id: state.health.incarnation_id.clone(),
        key_set_version: version,
        portal_node_pinned: pinned,
    }))
}

async fn update_trust(
    State(state): State<AgentState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    body: Bytes,
) -> Result<StatusCode, AgentError> {
    let trust = state.trust.current().ok_or(AgentError::NotJoined)?;
    state.check_peer(&trust, peer).await?;
    let key_set =
        SignedKeySet::from_bytes(&body).map_err(|_| AgentError::BadRequest("malformed key set"))?;
    let store = state.trust.clone();
    tokio::task::spawn_blocking(move || store.update(key_set))
        .await
        .map_err(AgentError::Join)??;
    Ok(StatusCode::NO_CONTENT)
}

async fn health(
    State(state): State<AgentState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
) -> Result<Json<AgentHealthResponse>, AgentError> {
    state
        .authorize(peer, &headers, Operation::Health, Scope::Agent)
        .await?;
    let capacity = state.capacity()?;
    Ok(Json(AgentHealthResponse {
        id: state.health.id.clone(),
        failure_domain: state.health.failure_domain.clone(),
        incarnation_id: state.health.incarnation_id.clone(),
        capacity,
        bind_mode: match state.bind_mode {
            BindMode::Tailnet => "tailnet",
            BindMode::InsecureLoopback => "insecure_loopback",
        },
    }))
}

#[derive(Deserialize)]
struct InventoryQuery {
    after: Option<String>,
    limit: Option<usize>,
}

async fn inventory(
    State(state): State<AgentState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Path(kind): Path<String>,
    Query(query): Query<InventoryQuery>,
) -> Result<Json<InventoryPage>, AgentError> {
    let kind: ObjectKind = kind.parse()?;
    state
        .authorize(peer, &headers, Operation::Inventory, Scope::Kind(kind))
        .await?;
    let after = query.after.map(ObjectHash::parse).transpose()?;
    let limit = query.limit.unwrap_or(MAX_INVENTORY_PAGE);
    let page =
        tokio::task::spawn_blocking(move || state.store.inventory(kind, after.as_ref(), limit))
            .await
            .map_err(AgentError::Join)??;
    Ok(Json(page))
}

async fn put_object(
    State(state): State<AgentState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Path((kind, hash)): Path<(String, String)>,
    body: Bytes,
) -> Result<StatusCode, AgentError> {
    let kind: ObjectKind = kind.parse()?;
    let hash = ObjectHash::parse(hash)?;
    state
        .authorize(
            peer,
            &headers,
            Operation::Put,
            Scope::Object(kind, hash.clone()),
        )
        .await?;
    state.admit(kind, body.len() as u64)?;
    let store = state.store.clone();
    let result = tokio::task::spawn_blocking(move || store.put(kind, &hash, &body))
        .await
        .map_err(AgentError::Join)?;
    match result {
        Ok(()) => Ok(StatusCode::NO_CONTENT),
        Err(StoreError::Io(error)) if error.raw_os_error() == Some(libc::ENOSPC) => {
            state.last_enospc_at.store(now_seconds(), Ordering::Relaxed);
            state
                .last_refusal_at
                .store(now_seconds(), Ordering::Relaxed);
            Err(AgentError::InsufficientCapacity {
                required_bytes: 0,
                available_bytes: 0,
                limiting: CapacityLimit::Enospc,
                volume_id: state.health.id.clone(),
            })
        }
        Err(error) => Err(error.into()),
    }
}

async fn delete_object(
    State(state): State<AgentState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Path((kind, hash)): Path<(String, String)>,
) -> Result<StatusCode, AgentError> {
    let kind: ObjectKind = kind.parse()?;
    let hash = ObjectHash::parse(hash)?;
    state
        .authorize(
            peer,
            &headers,
            Operation::Delete,
            Scope::Object(kind, hash.clone()),
        )
        .await?;
    tokio::task::spawn_blocking(move || state.store.delete(kind, &hash))
        .await
        .map_err(AgentError::Join)??;
    Ok(StatusCode::NO_CONTENT)
}

async fn get_object(
    State(state): State<AgentState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Path((kind, hash)): Path<(String, String)>,
) -> Result<Bytes, AgentError> {
    let kind: ObjectKind = kind.parse()?;
    let hash = ObjectHash::parse(hash)?;
    state
        .authorize(
            peer,
            &headers,
            Operation::Get,
            Scope::Object(kind, hash.clone()),
        )
        .await?;
    let bytes = tokio::task::spawn_blocking(move || state.store.get(kind, &hash))
        .await
        .map_err(AgentError::Join)??;
    Ok(bytes.into())
}

#[derive(Debug, thiserror::Error)]
enum AgentError {
    #[error(transparent)]
    Object(#[from] crate::object::ObjectError),
    #[error(transparent)]
    Store(#[from] StoreError),
    #[error("blocking storage task failed: {0}")]
    Join(tokio::task::JoinError),
    #[error("insufficient capacity on volume {volume_id}: {required_bytes} bytes requested, {available_bytes} admissible ({limiting:?})")]
    InsufficientCapacity {
        required_bytes: u64,
        available_bytes: u64,
        limiting: CapacityLimit,
        volume_id: String,
    },
    #[error("agent is not joined")]
    NotJoined,
    #[error("caller rejected: {0}")]
    PeerRejected(String),
    #[error("{error}")]
    Grant {
        error: GrantError,
        key_set_version: u64,
    },
    #[error(transparent)]
    Trust(#[from] TrustError),
    #[error("bad request: {0}")]
    BadRequest(&'static str),
}

/// Body of an HTTP 507 refusal.
#[derive(Serialize, Deserialize)]
pub struct CapacityRefusal {
    pub code: String,
    pub error: String,
    pub limiting: CapacityLimit,
    pub volume_id: String,
    pub required_bytes: u64,
    pub available_bytes: u64,
}

/// Body of an authorization or join refusal.
#[derive(Debug, Deserialize, Serialize)]
pub struct Refusal {
    pub code: String,
    pub error: String,
}

impl IntoResponse for AgentError {
    fn into_response(self) -> Response {
        if let Self::InsufficientCapacity {
            required_bytes,
            available_bytes,
            limiting,
            volume_id,
        } = &self
        {
            return (
                StatusCode::INSUFFICIENT_STORAGE,
                Json(CapacityRefusal {
                    code: "insufficient_capacity".to_owned(),
                    error: self.to_string(),
                    limiting: *limiting,
                    volume_id: volume_id.clone(),
                    required_bytes: *required_bytes,
                    available_bytes: *available_bytes,
                }),
            )
                .into_response();
        }
        let refusal = |status: StatusCode, code: &str, error: String| {
            (
                status,
                Json(Refusal {
                    code: code.to_owned(),
                    error,
                }),
            )
                .into_response()
        };
        match &self {
            Self::NotJoined => {
                return refusal(StatusCode::FORBIDDEN, "not_joined", self.to_string())
            }
            Self::PeerRejected(_) => {
                return refusal(StatusCode::FORBIDDEN, "peer_rejected", self.to_string())
            }
            Self::Grant {
                error: GrantError::Missing,
                ..
            } => return refusal(StatusCode::UNAUTHORIZED, "grant_missing", self.to_string()),
            Self::Grant {
                error: GrantError::UnknownKey(_) | GrantError::NewerEpoch { .. },
                key_set_version,
            } => {
                let mut response =
                    refusal(StatusCode::CONFLICT, "key_set_outdated", self.to_string());
                response.headers_mut().insert(
                    KEY_SET_OUTDATED_HEADER,
                    key_set_version.to_string().parse().expect("ascii digits"),
                );
                return response;
            }
            Self::Grant { .. } => {
                return refusal(StatusCode::FORBIDDEN, "grant_rejected", self.to_string())
            }
            Self::Trust(error) => {
                let (status, code) = match error {
                    TrustError::AlreadyJoined => (StatusCode::CONFLICT, "already_joined"),
                    TrustError::NotJoined => (StatusCode::FORBIDDEN, "not_joined"),
                    TrustError::NoJoinToken | TrustError::BadToken => {
                        (StatusCode::FORBIDDEN, "join_refused")
                    }
                    TrustError::Untrusted => (StatusCode::FORBIDDEN, "key_set_untrusted"),
                    TrustError::NotNewer { .. } => (StatusCode::CONFLICT, "key_set_not_newer"),
                    TrustError::EpochRegression { .. } => {
                        (StatusCode::CONFLICT, "key_set_epoch_regression")
                    }
                    TrustError::Malformed(..) | TrustError::Io(_) => {
                        (StatusCode::INTERNAL_SERVER_ERROR, "trust_store_error")
                    }
                };
                return refusal(status, code, self.to_string());
            }
            Self::BadRequest(_) => {
                return refusal(StatusCode::BAD_REQUEST, "bad_request", self.to_string())
            }
            _ => {}
        }
        let status = match &self {
            Self::Object(_) => StatusCode::BAD_REQUEST,
            Self::Store(StoreError::NotFound(_)) => StatusCode::NOT_FOUND,
            Self::Store(StoreError::HashMismatch { .. }) => StatusCode::CONFLICT,
            Self::Store(StoreError::InvalidInventoryLimit(_)) => StatusCode::BAD_REQUEST,
            Self::Store(StoreError::MalformedInventoryEntry(_))
            | Self::Store(StoreError::MalformedIncarnation(_)) => StatusCode::INTERNAL_SERVER_ERROR,
            Self::Store(StoreError::InvalidPath(_))
            | Self::Store(StoreError::Io(_))
            | Self::Join(_) => StatusCode::INTERNAL_SERVER_ERROR,
            _ => StatusCode::INTERNAL_SERVER_ERROR,
        };
        (
            status,
            Json(ErrorBody {
                error: self.to_string(),
            }),
        )
            .into_response()
    }
}

#[derive(Serialize)]
struct ErrorBody {
    error: String,
}
