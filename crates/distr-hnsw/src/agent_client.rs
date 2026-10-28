//! The portal's client for agents (docs/m2-implementation-plan.md,
//! decisions 3 and 5).
//!
//! Every request carries a fresh capability grant signed with the active
//! key. An agent whose key set is older than the grant answers 409 with
//! [`KEY_SET_OUTDATED_HEADER`]; the client then pushes the current signed
//! key set and retries once. An agent that trusts another portal refuses
//! the push, and the caller sees that refusal (403 `key_set_untrusted`). Joining sends a single-use token with the
//! first key set and records the agent's tailnet node.

use std::{
    net::SocketAddr,
    sync::Arc,
    time::{SystemTime, UNIX_EPOCH},
};

use axum::body::Bytes;
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use reqwest::{header::AUTHORIZATION, RequestBuilder, Response, StatusCode};
use thiserror::Error;
use uuid::Uuid;

use crate::{
    access::{AccessError, GrantMaterial},
    agent::{JoinRequest, JoinResponse, Refusal, KEY_SET_OUTDATED_HEADER},
    crypto::MasterKey,
    grant::{Operation, Scope, GRANT_SCHEME},
    metadata::{AgentJoin, Database, MetadataError},
    object::{ObjectHash, ObjectKind},
    portal::AgentTarget,
    tailscale::{LocalApi, LocalApiError},
    trust::join_token_hash,
};

fn now_seconds() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs() as i64)
        .unwrap_or(0)
}

/// Sends granted requests to agents.
#[derive(Clone)]
pub struct AgentClient {
    http: reqwest::Client,
    material: Arc<GrantMaterial>,
}

impl AgentClient {
    pub fn new(material: GrantMaterial) -> Self {
        Self {
            http: reqwest::Client::new(),
            material: Arc::new(material),
        }
    }

    /// Load the portal's signing keys (creating the first one if needed).
    pub fn open(database: &mut Database, master: &MasterKey) -> Result<Self, AccessError> {
        Ok(Self::new(GrantMaterial::load_or_create(database, master)?))
    }

    pub fn material(&self) -> &GrantMaterial {
        &self.material
    }

    pub async fn health(&self, agent: &AgentTarget) -> reqwest::Result<Response> {
        let url = format!("{}/v1/health", agent.base_url);
        self.send(agent, Operation::Health, Scope::Agent, None, |http| {
            http.get(&url)
        })
        .await
    }

    pub async fn inventory(
        &self,
        agent: &AgentTarget,
        kind: ObjectKind,
        after: Option<&ObjectHash>,
        limit: usize,
    ) -> reqwest::Result<Response> {
        let url = format!("{}/v1/inventory/{}", agent.base_url, kind.as_str());
        let mut query = vec![("limit", limit.to_string())];
        if let Some(after) = after {
            query.push(("after", after.to_string()));
        }
        self.send(
            agent,
            Operation::Inventory,
            Scope::Kind(kind),
            None,
            |http| http.get(&url).query(&query),
        )
        .await
    }

    pub async fn get(
        &self,
        agent: &AgentTarget,
        kind: ObjectKind,
        hash: &ObjectHash,
    ) -> reqwest::Result<Response> {
        let url = object_url(agent, kind, hash);
        self.send(
            agent,
            Operation::Get,
            Scope::Object(kind, hash.clone()),
            None,
            |http| http.get(&url),
        )
        .await
    }

    pub async fn put(
        &self,
        agent: &AgentTarget,
        kind: ObjectKind,
        hash: &ObjectHash,
        body: Bytes,
    ) -> reqwest::Result<Response> {
        let url = object_url(agent, kind, hash);
        self.send(
            agent,
            Operation::Put,
            Scope::Object(kind, hash.clone()),
            None,
            |http| http.put(&url).body(body.clone()),
        )
        .await
    }

    /// Delete an object; only the application of a GC proof may do this.
    pub async fn delete(
        &self,
        agent: &AgentTarget,
        kind: ObjectKind,
        hash: &ObjectHash,
        gc_proof: Uuid,
    ) -> reqwest::Result<Response> {
        let url = object_url(agent, kind, hash);
        self.send(
            agent,
            Operation::Delete,
            Scope::Object(kind, hash.clone()),
            Some(gc_proof),
            |http| http.delete(&url),
        )
        .await
    }

    /// Send the current signed key set (`PUT /v1/trust`).
    pub async fn push_key_set(&self, agent: &AgentTarget) -> reqwest::Result<Response> {
        self.http
            .put(format!("{}/v1/trust", agent.base_url))
            .body(self.material.signed_key_set().to_bytes())
            .send()
            .await
    }

    async fn send(
        &self,
        agent: &AgentTarget,
        operation: Operation,
        scope: Scope,
        gc_proof: Option<Uuid>,
        build: impl Fn(&reqwest::Client) -> RequestBuilder,
    ) -> reqwest::Result<Response> {
        let mut pushed = false;
        loop {
            let token = self
                .material
                .active()
                .issue(
                    self.material.epoch(),
                    &agent.id,
                    operation,
                    scope.clone(),
                    gc_proof,
                    now_seconds(),
                )
                .expect("agent ids are validated when targets are parsed");
            let response = build(&self.http)
                .header(AUTHORIZATION, format!("{GRANT_SCHEME} {token}"))
                .send()
                .await?;
            if !pushed
                && response.status() == StatusCode::CONFLICT
                && response.headers().contains_key(KEY_SET_OUTDATED_HEADER)
            {
                pushed = true;
                let push = self.push_key_set(agent).await?;
                // 409 here means the agent already holds this version.
                if push.status().is_success() || push.status() == StatusCode::CONFLICT {
                    continue;
                }
                // The agent refuses this portal's key set (it trusts another
                // portal): report that instead of the outdated-key answer.
                return Ok(push);
            }
            return Ok(response);
        }
    }
}

fn object_url(agent: &AgentTarget, kind: ObjectKind, hash: &ObjectHash) -> String {
    format!("{}/v1/objects/{}/{}", agent.base_url, kind.as_str(), hash)
}

#[derive(Debug, Error)]
pub enum JoinError {
    #[error(transparent)]
    Access(#[from] AccessError),
    #[error(transparent)]
    Metadata(#[from] MetadataError),
    #[error("agent request failed: {0}")]
    Http(#[from] reqwest::Error),
    #[error("join token is unknown to this portal")]
    UnknownToken,
    #[error("join token was issued for agent {0}")]
    TokenForAnotherAgent(String),
    #[error("join token was already used")]
    TokenUsed,
    #[error("join token expired")]
    TokenExpired,
    #[error("cannot resolve agent address {0}")]
    Resolve(String),
    #[error("cannot identify the agent's tailnet node: {0}")]
    WhoIs(#[from] LocalApiError),
    #[error("agent is already joined and does not accept this portal's grants")]
    JoinedElsewhere,
    #[error("agent refused the join ({status}): {message}")]
    Refused { status: StatusCode, message: String },
}

/// What a join recorded.
#[derive(Debug)]
pub struct JoinOutcome {
    pub agent_id: String,
    /// The agent's tailnet node; `None` for a loopback agent.
    pub agent_node: Option<String>,
    /// The agent pinned this portal's node (false for loopback).
    pub portal_node_pinned: bool,
    /// The agent id was joined before; the grant epoch was raised.
    pub rejoined: bool,
}

/// The tailnet node behind an agent URL, or `None` when the URL names a
/// loopback address (an agent started with `--insecure-loopback`).
async fn agent_node(agent: &AgentTarget, api: &LocalApi) -> Result<Option<String>, JoinError> {
    let url = reqwest::Url::parse(&agent.base_url)
        .map_err(|_| JoinError::Resolve(agent.base_url.clone()))?;
    let host = url
        .host_str()
        .ok_or_else(|| JoinError::Resolve(agent.base_url.clone()))?
        .trim_start_matches('[')
        .trim_end_matches(']')
        .to_owned();
    let port = url
        .port_or_known_default()
        .ok_or_else(|| JoinError::Resolve(agent.base_url.clone()))?;
    let addresses: Vec<SocketAddr> = tokio::net::lookup_host((host.as_str(), port))
        .await
        .map_err(|_| JoinError::Resolve(agent.base_url.clone()))?
        .collect();
    let Some(address) = addresses.first() else {
        return Err(JoinError::Resolve(agent.base_url.clone()));
    };
    if addresses.iter().all(|address| address.ip().is_loopback()) {
        return Ok(None);
    }
    Ok(Some(api.whois(*address).await?.node_stable_id))
}

/// Join an agent that was started with `token`: send it the current key
/// set, record its node, and burn the token.
pub async fn join_agent(
    database: &mut Database,
    master: &MasterKey,
    agent: &AgentTarget,
    token: &str,
    api: &LocalApi,
) -> Result<JoinOutcome, JoinError> {
    let token_hash = join_token_hash(token);
    let row = database
        .join_token(&token_hash)?
        .ok_or(JoinError::UnknownToken)?;
    if row.agent_id != agent.id {
        return Err(JoinError::TokenForAnotherAgent(row.agent_id));
    }
    if row.used_at.is_some() {
        return Err(JoinError::TokenUsed);
    }
    if now_seconds() > row.expires_at {
        return Err(JoinError::TokenExpired);
    }
    let node = agent_node(agent, api).await?;
    let client = AgentClient::open(database, master)?;
    let request = JoinRequest {
        token: token.trim().to_owned(),
        key_set: URL_SAFE_NO_PAD.encode(client.material().signed_key_set().to_bytes()),
    };
    let response = client
        .http
        .post(format!("{}/v1/join", agent.base_url))
        .json(&request)
        .send()
        .await?;
    let status = response.status();
    let portal_node_pinned = if status.is_success() {
        response.json::<JoinResponse>().await?.portal_node_pinned
    } else {
        let refusal = response.json::<Refusal>().await.ok();
        if status == StatusCode::CONFLICT
            && refusal.as_ref().is_some_and(|r| r.code == "already_joined")
        {
            // The agent joined but the reply was lost: it must already
            // accept this portal's grants.
            if !client.health(agent).await?.status().is_success() {
                return Err(JoinError::JoinedElsewhere);
            }
            node.is_some()
        } else {
            return Err(JoinError::Refused {
                status,
                message: refusal.map_or_else(String::new, |r| r.error),
            });
        }
    };
    let rejoined = database.record_agent_join(
        &AgentJoin {
            agent_id: agent.id.clone(),
            node_stable_id: node.clone(),
            base_url: agent.base_url.clone(),
        },
        &token_hash,
    )?;
    Ok(JoinOutcome {
        agent_id: agent.id.clone(),
        agent_node: node,
        portal_node_pinned,
        rejoined,
    })
}
