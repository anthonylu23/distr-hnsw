use std::{
    path::PathBuf,
    sync::{
        atomic::{AtomicI64, Ordering},
        Arc,
    },
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use axum::{
    body::Bytes,
    extract::{DefaultBodyLimit, Path, Query, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::get,
    Json, Router,
};
use serde::{Deserialize, Serialize};
use tokio::net::TcpListener;

use crate::{
    durability::{DurableStore, InventoryPage, StoreError, StoreFailpoint, MAX_INVENTORY_PAGE},
    object::{ObjectHash, ObjectKind},
    tailscale::{self, BindMode, BindSpec, LocalApi},
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

pub async fn serve_agent(
    listener: TcpListener,
    volume: PathBuf,
    identity: AgentIdentity,
) -> anyhow::Result<()> {
    serve_agent_with_capacity(listener, volume, identity, CapacityConfig::default()).await
}

/// Serve an agent on a listener the caller bound to loopback (tests).
pub async fn serve_agent_with_capacity(
    listener: TcpListener,
    volume: PathBuf,
    identity: AgentIdentity,
    capacity: CapacityConfig,
) -> anyhow::Result<()> {
    let router = prepared_router(
        DurableStore::open(volume)?,
        identity,
        capacity,
        BindMode::InsecureLoopback,
    )?;
    axum::serve(listener, router).await?;
    Ok(())
}

fn prepared_router(
    store: DurableStore,
    identity: AgentIdentity,
    capacity: CapacityConfig,
    bind_mode: BindMode,
) -> anyhow::Result<Router> {
    let swept = store.sweep_stale_temporaries(std::time::Duration::from_secs(3600))?;
    if swept > 0 {
        eprintln!("removed {swept} stale temporary file(s) left by interrupted writes");
    }
    Ok(router_with_mode(store, identity, capacity, bind_mode)?)
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
) -> anyhow::Result<()> {
    let (listener, mode) = tailscale::bind_listener(
        bind.spec,
        bind.insecure_loopback,
        &bind.api,
        bind.tailnet_wait,
    )
    .await?;
    let local = listener.local_addr()?;
    if mode == BindMode::Tailnet {
        // Agent routes are unauthenticated until capability grants and join
        // land (M2 work packages 2 and 3); on a tailnet address any node
        // could call DELETE.
        anyhow::bail!(
            "refusing to serve on tailnet address {local}: agents accept tailnet \
             traffic only with capability grants (M2 work package 2); use \
             --insecure-loopback until then"
        );
    }
    eprintln!("warning: agent bound to loopback {local} with --insecure-loopback");
    let mut store = DurableStore::open(volume)?;
    if let Some(failpoint) = failpoint {
        store = store.with_failpoint(failpoint);
    }
    let router = prepared_router(store, identity, capacity, mode)?;
    let server = axum::serve(listener, router);
    if mode == BindMode::InsecureLoopback {
        server.await?;
        return Ok(());
    }
    tokio::select! {
        served = server => served.map_err(anyhow::Error::from),
        lost = tailscale::address_lost(bind.api, local.ip(), bind.address_poll) => {
            Err(TailnetAddressChanged(lost).into())
        }
    }
}

pub fn router(
    store: DurableStore,
    identity: AgentIdentity,
    capacity: CapacityConfig,
) -> Result<Router, StoreError> {
    router_with_mode(store, identity, capacity, BindMode::InsecureLoopback)
}

fn router_with_mode(
    store: DurableStore,
    identity: AgentIdentity,
    capacity: CapacityConfig,
    bind_mode: BindMode,
) -> Result<Router, StoreError> {
    let incarnation_id = store.load_or_create_incarnation()?;
    let state = AgentState {
        store: Arc::new(store),
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
        .route("/v1/health", get(health))
        .route("/v1/inventory/{kind}", get(inventory))
        .route(
            "/v1/objects/{kind}/{hash}",
            get(get_object).put(put_object).delete(delete_object),
        )
        .layer(DefaultBodyLimit::max(crate::MAX_OBJECT_BYTES))
        .with_state(state))
}

async fn health(State(state): State<AgentState>) -> Result<Json<AgentHealthResponse>, AgentError> {
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
    Path(kind): Path<String>,
    Query(query): Query<InventoryQuery>,
) -> Result<Json<InventoryPage>, AgentError> {
    let kind = kind.parse()?;
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
    Path((kind, hash)): Path<(String, String)>,
    body: Bytes,
) -> Result<StatusCode, AgentError> {
    let kind: ObjectKind = kind.parse()?;
    let hash = ObjectHash::parse(hash)?;
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
    Path((kind, hash)): Path<(String, String)>,
) -> Result<StatusCode, AgentError> {
    let kind = kind.parse()?;
    let hash = ObjectHash::parse(hash)?;
    tokio::task::spawn_blocking(move || state.store.delete(kind, &hash))
        .await
        .map_err(AgentError::Join)??;
    Ok(StatusCode::NO_CONTENT)
}

async fn get_object(
    State(state): State<AgentState>,
    Path((kind, hash)): Path<(String, String)>,
) -> Result<Bytes, AgentError> {
    let kind = kind.parse()?;
    let hash = ObjectHash::parse(hash)?;
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
            Self::InsufficientCapacity { .. } => StatusCode::INSUFFICIENT_STORAGE,
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
