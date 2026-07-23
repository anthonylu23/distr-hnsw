use std::{net::SocketAddr, path::PathBuf, sync::Arc};

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
    durability::{DurableStore, InventoryPage, StoreError, MAX_INVENTORY_PAGE},
    object::ObjectHash,
    CHUNK_SIZE,
};

#[derive(Clone)]
struct AgentState {
    store: Arc<DurableStore>,
    identity: AgentIdentity,
}

#[derive(Clone, Debug, Serialize)]
pub struct AgentIdentity {
    pub id: String,
    pub failure_domain: String,
}

pub async fn serve_agent(
    listener: TcpListener,
    volume: PathBuf,
    identity: AgentIdentity,
) -> anyhow::Result<()> {
    let store = DurableStore::open(volume)?;
    let router = router(store, identity);
    axum::serve(listener, router).await?;
    Ok(())
}

pub async fn bind_and_serve_agent(
    bind: SocketAddr,
    volume: PathBuf,
    identity: AgentIdentity,
) -> anyhow::Result<()> {
    if !bind.ip().is_loopback() {
        anyhow::bail!("M1 agent must bind to a loopback address; Tailscale authorization is M2")
    }
    let listener = TcpListener::bind(bind).await?;
    serve_agent(listener, volume, identity).await
}

pub fn router(store: DurableStore, identity: AgentIdentity) -> Router {
    let state = AgentState {
        store: Arc::new(store),
        identity,
    };
    Router::new()
        .route("/v1/health", get(health))
        .route("/v1/inventory/{kind}", get(inventory))
        .route("/v1/objects/{kind}/{hash}", get(get_object).put(put_object))
        .layer(DefaultBodyLimit::max(CHUNK_SIZE + 1024 * 1024))
        .with_state(state)
}

async fn health(State(state): State<AgentState>) -> Json<AgentIdentity> {
    Json(state.identity)
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
    let kind = kind.parse()?;
    let hash = ObjectHash::parse(hash)?;
    tokio::task::spawn_blocking(move || state.store.put(kind, &hash, &body))
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
}

impl IntoResponse for AgentError {
    fn into_response(self) -> Response {
        let status = match &self {
            Self::Object(_) => StatusCode::BAD_REQUEST,
            Self::Store(StoreError::NotFound(_)) => StatusCode::NOT_FOUND,
            Self::Store(StoreError::HashMismatch { .. }) => StatusCode::CONFLICT,
            Self::Store(StoreError::InvalidInventoryLimit(_)) => StatusCode::BAD_REQUEST,
            Self::Store(StoreError::MalformedInventoryEntry(_)) => {
                StatusCode::INTERNAL_SERVER_ERROR
            }
            Self::Store(StoreError::InvalidPath(_))
            | Self::Store(StoreError::Io(_))
            | Self::Join(_) => StatusCode::INTERNAL_SERVER_ERROR,
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
