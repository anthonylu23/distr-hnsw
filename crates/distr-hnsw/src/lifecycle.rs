//! Placement movement, node retirement, and proof-producing garbage
//! collection for the M1 blob plane, following `docs/m1-lifecycle-contract.md`.
//!
//! - `drain` moves an agent's copies elsewhere copy-first and only then marks
//!   its placements orphaned.
//! - `retire` proves the durability floor holds for every historical object
//!   without an agent, then retires its incarnation so it can never rejoin.
//! - `gc` plans physical deletion by recording a proof per object whose
//!   inputs are content-addressed (content generation, active incarnation
//!   set, inventory digests); `--apply` re-derives every fact and refuses a
//!   stale proof. Agent DELETE is reachable only through an applied proof.

use std::collections::{BTreeMap, BTreeSet};

use serde::Serialize;
use thiserror::Error;
use uuid::Uuid;

use crate::{
    metadata::{
        Database, HistoryObject, IncarnationStatus, JobStatus, MetadataError, NewGcProof,
        PlacementState, PlacementTarget,
    },
    object::{ObjectHash, ObjectKind},
    portal::AgentTarget,
};

const MINIMUM_REPLICAS: usize = 2;
const DESIRED_REPLICAS: usize = 2;

struct LiveAgent {
    target: AgentTarget,
    incarnation_id: String,
}

impl LiveAgent {
    fn placement(&self) -> PlacementTarget {
        PlacementTarget {
            agent_id: self.target.id.clone(),
            failure_domain: self.target.failure_domain.clone(),
            incarnation_id: self.incarnation_id.clone(),
        }
    }
}

#[derive(serde::Deserialize)]
struct RemoteHealth {
    id: String,
    failure_domain: String,
    incarnation_id: String,
}

/// Verify identity and record incarnations for every reachable agent.
/// Identity mismatches and refused incarnations fail closed.
async fn verify_agents(
    database: &mut Database,
    agents: &[AgentTarget],
    client: &reqwest::Client,
    issues: &mut Vec<String>,
) -> Result<Vec<LiveAgent>, LifecycleError> {
    let mut live = Vec::new();
    for agent in agents {
        let response = match client
            .get(format!("{}/v1/health", agent.base_url))
            .send()
            .await
        {
            Ok(response) if response.status().is_success() => response,
            Ok(response) => {
                issues.push(format!(
                    "agent {} unreachable: health returned HTTP {}",
                    agent.id,
                    response.status()
                ));
                continue;
            }
            Err(error) => {
                issues.push(format!("agent {} unreachable: {error}", agent.id));
                continue;
            }
        };
        let identity: RemoteHealth = response
            .json()
            .await
            .map_err(|_| LifecycleError::AgentIdentity(agent.id.clone()))?;
        if identity.id != agent.id || identity.failure_domain != agent.failure_domain {
            return Err(LifecycleError::AgentIdentity(agent.id.clone()));
        }
        database.observe_agent_incarnation(
            &agent.id,
            &agent.failure_domain,
            &identity.incarnation_id,
        )?;
        live.push(LiveAgent {
            target: agent.clone(),
            incarnation_id: identity.incarnation_id,
        });
    }
    Ok(live)
}

async fn fetch_valid(
    client: &reqwest::Client,
    agent: &AgentTarget,
    kind: ObjectKind,
    hash: &ObjectHash,
) -> Option<Vec<u8>> {
    let url = format!("{}/v1/objects/{}/{}", agent.base_url, kind.as_str(), hash);
    match client.get(url).send().await {
        Ok(response) if response.status().is_success() => response
            .bytes()
            .await
            .ok()
            .filter(|bytes| ObjectHash::digest(bytes) == *hash)
            .map(|bytes| bytes.to_vec()),
        _ => None,
    }
}

/// Copy-first placement: PUT, read back, confirm. Returns true on success.
async fn place_copy(
    database: &mut Database,
    client: &reqwest::Client,
    agent: &LiveAgent,
    kind: ObjectKind,
    hash: &ObjectHash,
    bytes: &[u8],
    job_id: Uuid,
) -> bool {
    let url = format!(
        "{}/v1/objects/{}/{}",
        agent.target.base_url,
        kind.as_str(),
        hash
    );
    let accepted = matches!(
        client.put(url).body(bytes.to_vec()).send().await,
        Ok(response) if response.status().is_success()
    );
    if !accepted {
        return false;
    }
    if fetch_valid(client, &agent.target, kind, hash)
        .await
        .is_none()
    {
        return false;
    }
    database
        .set_placement_verification(
            kind,
            hash,
            &agent.placement(),
            PlacementState::Confirmed,
            job_id,
        )
        .is_ok()
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct MoveRecord {
    pub kind: ObjectKind,
    pub hash: String,
    pub source_agent: String,
    pub target_agent: String,
    pub status: String,
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize)]
pub struct DrainTotals {
    pub objects_considered: usize,
    pub already_redundant: usize,
    pub moves_planned: usize,
    pub moves_applied: usize,
    pub orphaned: usize,
    pub blocked: usize,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct DrainReportV1 {
    pub report_type: String,
    pub version: u16,
    pub job_id: Uuid,
    pub agent_id: String,
    pub dry_run: bool,
    pub totals: DrainTotals,
    pub moves: Vec<MoveRecord>,
    pub issues: Vec<String>,
}

impl DrainReportV1 {
    pub fn exit_code(&self) -> u8 {
        if self.totals.blocked == 0 && self.issues.is_empty() {
            0
        } else {
            2
        }
    }
}

/// Move every historical copy held by `agent_id` so the floor and desired
/// placement hold without it, then orphan its placements.
pub async fn drain(
    database: &mut Database,
    agents: &[AgentTarget],
    client: &reqwest::Client,
    agent_id: &str,
    dry_run: bool,
) -> Result<DrainReportV1, LifecycleError> {
    if !agents.iter().any(|agent| agent.id == agent_id) {
        return Err(LifecycleError::UnknownAgent(agent_id.to_owned()));
    }
    let job_id = database.create_reconcile_job(crate::metadata::JobMode::Repair)?;
    let result = run_drain(database, agents, client, agent_id, dry_run, job_id).await;
    match &result {
        Ok(report) => {
            let json = serde_json::to_string(report)?;
            database.finish_reconcile_job(job_id, JobStatus::Complete, Some(&json))?;
        }
        Err(error) => {
            database.finish_reconcile_job(job_id, JobStatus::Failed, Some(&error.to_string()))?;
        }
    }
    result
}

async fn run_drain(
    database: &mut Database,
    agents: &[AgentTarget],
    client: &reqwest::Client,
    agent_id: &str,
    dry_run: bool,
    job_id: Uuid,
) -> Result<DrainReportV1, LifecycleError> {
    let mut issues = Vec::new();
    let live = verify_agents(database, agents, client, &mut issues).await?;
    let history: BTreeMap<(ObjectKind, String), HistoryObject> = database
        .history_objects()?
        .into_iter()
        .map(|object| ((object.kind, object.hash.to_string()), object))
        .collect();
    let held: Vec<_> = database
        .placements_for_agent(agent_id)?
        .into_iter()
        .filter(|(_, _, state)| *state != PlacementState::Orphaned)
        .collect();
    let mut totals = DrainTotals::default();
    let mut moves = Vec::new();

    for (kind, hash, _) in held {
        if !history.contains_key(&(kind, hash.to_string())) {
            // Unreferenced garbage is the GC planner's business.
            continue;
        }
        totals.objects_considered += 1;
        let mut valid: BTreeMap<String, String> = BTreeMap::new();
        let mut bytes = None;
        let mut source_agent = None;
        for agent in &live {
            if let Some(got) = fetch_valid(client, &agent.target, kind, &hash).await {
                valid.insert(agent.target.id.clone(), agent.target.failure_domain.clone());
                // Prefer a source other than the agent being drained.
                if bytes.is_none() || agent.target.id != agent_id {
                    bytes = Some(got);
                    source_agent = Some(agent.target.id.clone());
                }
            }
        }
        let satisfied = |valid: &BTreeMap<String, String>| {
            let others: Vec<_> = valid.iter().filter(|(id, _)| *id != agent_id).collect();
            let domains: BTreeSet<_> = others.iter().map(|(_, domain)| *domain).collect();
            domains.len() >= MINIMUM_REPLICAS && others.len() >= DESIRED_REPLICAS
        };
        if satisfied(&valid) {
            totals.already_redundant += 1;
        } else {
            let Some(bytes) = bytes.clone() else {
                totals.blocked += 1;
                issues.push(format!(
                    "no valid copy of {kind} {hash} on any reachable agent"
                ));
                continue;
            };
            let source = source_agent.clone().unwrap_or_default();
            for agent in &live {
                if satisfied(&valid) {
                    break;
                }
                if agent.target.id == agent_id || valid.contains_key(&agent.target.id) {
                    continue;
                }
                totals.moves_planned += 1;
                let mut record = MoveRecord {
                    kind,
                    hash: hash.to_string(),
                    source_agent: source.clone(),
                    target_agent: agent.target.id.clone(),
                    status: "planned".to_owned(),
                };
                if dry_run {
                    valid.insert(agent.target.id.clone(), agent.target.failure_domain.clone());
                } else if place_copy(database, client, agent, kind, &hash, &bytes, job_id).await {
                    valid.insert(agent.target.id.clone(), agent.target.failure_domain.clone());
                    record.status = "applied".to_owned();
                    totals.moves_applied += 1;
                } else {
                    record.status = "failed".to_owned();
                    issues.push(format!(
                        "agent {} did not accept {kind} {hash} intact",
                        agent.target.id
                    ));
                }
                moves.push(record);
            }
            if !satisfied(&valid) {
                totals.blocked += 1;
                issues.push(format!(
                    "{kind} {hash} cannot reach RF2 without agent {agent_id}; add a target in another failure domain"
                ));
                continue;
            }
        }
        // Only now, with replacements confirmed, does the source stop counting.
        if !dry_run {
            database.set_placement_state(kind, &hash, agent_id, PlacementState::Orphaned)?;
        }
        totals.orphaned += 1;
    }

    Ok(DrainReportV1 {
        report_type: "DrainReportV1".to_owned(),
        version: 1,
        job_id,
        agent_id: agent_id.to_owned(),
        dry_run,
        totals,
        moves,
        issues,
    })
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct RetireReportV1 {
    pub report_type: String,
    pub version: u16,
    pub agent_id: String,
    pub retired_incarnation: Option<String>,
    pub objects_checked: usize,
    pub objects_at_risk_without_agent: usize,
    pub issues: Vec<String>,
}

impl RetireReportV1 {
    pub fn exit_code(&self) -> u8 {
        if self.retired_incarnation.is_some() {
            0
        } else {
            2
        }
    }
}

/// Retire an agent's active incarnation. The agent itself need not be
/// reachable; every other configured agent must be, and every historical
/// object must already meet RF2 on them.
pub async fn retire(
    database: &mut Database,
    agents: &[AgentTarget],
    client: &reqwest::Client,
    agent_id: &str,
) -> Result<RetireReportV1, LifecycleError> {
    let mut issues = Vec::new();
    let others: Vec<AgentTarget> = agents
        .iter()
        .filter(|agent| agent.id != agent_id)
        .cloned()
        .collect();
    let live = verify_agents(database, &others, client, &mut issues).await?;
    if live.len() != others.len() {
        return Ok(RetireReportV1 {
            report_type: "RetireReportV1".to_owned(),
            version: 1,
            agent_id: agent_id.to_owned(),
            retired_incarnation: None,
            objects_checked: 0,
            objects_at_risk_without_agent: 0,
            issues,
        });
    }
    if database.active_incarnation(agent_id)?.is_none() {
        return Err(LifecycleError::Metadata(
            MetadataError::NoActiveIncarnation(agent_id.to_owned()),
        ));
    }
    let history = database.history_objects()?;
    let mut at_risk = 0;
    for object in &history {
        let mut domains = BTreeSet::new();
        let mut copies = 0;
        for agent in &live {
            if fetch_valid(client, &agent.target, object.kind, &object.hash)
                .await
                .is_some()
            {
                copies += 1;
                domains.insert(agent.target.failure_domain.clone());
            }
        }
        if domains.len() < MINIMUM_REPLICAS || copies < DESIRED_REPLICAS {
            at_risk += 1;
            issues.push(format!(
                "{} {} would fall below RF2 without agent {agent_id}",
                object.kind, object.hash
            ));
        }
    }
    let retired = if at_risk == 0 {
        Some(database.retire_incarnation(agent_id)?)
    } else {
        None
    };
    Ok(RetireReportV1 {
        report_type: "RetireReportV1".to_owned(),
        version: 1,
        agent_id: agent_id.to_owned(),
        retired_incarnation: retired,
        objects_checked: history.len(),
        objects_at_risk_without_agent: at_risk,
        issues,
    })
}

#[derive(Clone, Copy, Debug)]
pub struct GcOptions {
    pub retention_seconds: i64,
    pub staging_grace_seconds: i64,
    pub apply: bool,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct GcCandidate {
    pub kind: ObjectKind,
    pub hash: String,
    pub reason: String,
    pub status: String,
    pub proof_id: Option<Uuid>,
    pub blockers: Vec<String>,
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize)]
pub struct GcTotals {
    pub candidates: usize,
    pub proven: usize,
    pub blocked: usize,
    pub applied: usize,
    pub stale: usize,
    pub deferred: usize,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct GcReportV1 {
    pub report_type: String,
    pub version: u16,
    pub apply: bool,
    pub content_generation: i64,
    pub incarnation_digest: String,
    pub observation_digest: String,
    pub retention_seconds: i64,
    pub staging_grace_seconds: i64,
    pub candidates: Vec<GcCandidate>,
    pub totals: GcTotals,
    pub issues: Vec<String>,
}

impl GcReportV1 {
    pub fn exit_code(&self) -> u8 {
        if self.issues.is_empty() && self.totals.stale == 0 && self.totals.deferred == 0 {
            0
        } else {
            2
        }
    }
}

struct ProofContext {
    now: i64,
    content_generation: i64,
    incarnation_digest: String,
    observation_digest: String,
    /// (incarnation, kind) -> latest complete inventory digest.
    observations: BTreeMap<(String, ObjectKind), String>,
    active: Vec<(String, String)>,
}

fn proof_context(database: &Database, now: i64) -> Result<ProofContext, LifecycleError> {
    let active: Vec<(String, String)> = database
        .agent_incarnations()?
        .into_iter()
        .filter(|incarnation| incarnation.status == IncarnationStatus::Active)
        .map(|incarnation| (incarnation.agent_id, incarnation.incarnation_id))
        .collect();
    let mut incarnation_hasher = blake3::Hasher::new();
    incarnation_hasher.update(b"distr-hnsw:gc-incarnations:v1");
    for (agent, incarnation) in &active {
        incarnation_hasher.update(agent.as_bytes());
        incarnation_hasher.update(b"/");
        incarnation_hasher.update(incarnation.as_bytes());
        incarnation_hasher.update(b"\n");
    }
    let mut observations = BTreeMap::new();
    let mut observation_hasher = blake3::Hasher::new();
    observation_hasher.update(b"distr-hnsw:gc-observations:v1");
    for scan in database.latest_complete_scans()? {
        observation_hasher.update(scan.incarnation_id.as_bytes());
        observation_hasher.update(scan.kind.as_str().as_bytes());
        observation_hasher.update(scan.inventory_digest.as_bytes());
        observation_hasher.update(b"\n");
        observations.insert((scan.incarnation_id, scan.kind), scan.inventory_digest);
    }
    Ok(ProofContext {
        now,
        content_generation: database.content_generation()?,
        incarnation_digest: incarnation_hasher.finalize().to_hex().to_string(),
        observation_digest: observation_hasher.finalize().to_hex().to_string(),
        observations,
        active,
    })
}

/// Evaluate every proof fact for one candidate; blockers are the missing facts.
fn evaluate(
    database: &Database,
    context: &ProofContext,
    kind: ObjectKind,
    hash: &ObjectHash,
    threshold: i64,
    retention: i64,
) -> Result<Vec<String>, LifecycleError> {
    let mut blockers = Vec::new();
    if database.required_by_live_file(kind, hash)? {
        blockers.push("required by the highest live generation".to_owned());
    }
    if kind == ObjectKind::DeletionMarker {
        blockers.push("deletion markers are retained in M1".to_owned());
    }
    if context.now - threshold < retention {
        blockers.push(format!(
            "retention horizon not elapsed ({}s of {retention}s)",
            context.now - threshold
        ));
    }
    if context.active.is_empty() {
        blockers.push("no active agent incarnation has been observed".to_owned());
    }
    for (agent, incarnation) in &context.active {
        if !context
            .observations
            .contains_key(&(incarnation.clone(), kind))
        {
            blockers.push(format!(
                "agent {agent} (incarnation {incarnation}) has no complete {kind} observation"
            ));
        }
    }
    let scans_after_threshold = database
        .latest_complete_scans()?
        .into_iter()
        .filter(|scan| scan.kind == kind)
        .all(|scan| scan.completed_at >= threshold);
    if !scans_after_threshold {
        blockers.push(format!(
            "a complete {kind} observation predates the deletion or expiry at {threshold}"
        ));
    }
    for (agent, _, state) in database.all_placements(kind, hash)? {
        if state == PlacementState::Pending {
            blockers.push(format!(
                "placement on {agent} is pending (movement in flight)"
            ));
        }
    }
    Ok(blockers)
}

/// Plan, and optionally apply, garbage collection.
pub async fn gc(
    database: &mut Database,
    agents: &[AgentTarget],
    client: &reqwest::Client,
    options: GcOptions,
) -> Result<GcReportV1, LifecycleError> {
    let now = i64::try_from(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_err(|_| LifecycleError::Clock)?
            .as_secs(),
    )
    .map_err(|_| LifecycleError::Clock)?;
    let mut issues = Vec::new();
    let live = verify_agents(database, agents, client, &mut issues).await?;
    let context = proof_context(database, now)?;

    // Candidates: superseded generations of deleted files, and expired
    // staging uploads. Anything the live projection needs is never listed.
    let mut candidates: BTreeMap<(ObjectKind, String), (ObjectHash, String, i64, i64)> =
        BTreeMap::new();
    for (file_id, generation, deleted_at) in database.deleted_files()? {
        for (kind, hash) in database.superseded_objects(file_id, generation)? {
            candidates.entry((kind, hash.to_string())).or_insert((
                hash,
                format!("generation below {generation} of deleted file {file_id}"),
                deleted_at,
                options.retention_seconds,
            ));
        }
    }
    for upload in database.expired_uploads(now - options.staging_grace_seconds)? {
        for (kind, hash) in upload.objects {
            candidates.entry((kind, hash.to_string())).or_insert((
                hash,
                format!("expired staging upload {}", upload.upload_id),
                upload.created_at,
                options.staging_grace_seconds,
            ));
        }
    }

    let mut report_candidates = Vec::new();
    let mut totals = GcTotals::default();
    let planned = database.planned_gc_proofs()?;
    for ((kind, hash_text), (hash, reason, threshold, retention)) in candidates {
        if database.required_by_live_file(kind, &hash)? || database.gc_applied(kind, &hash)? {
            continue;
        }
        totals.candidates += 1;
        let blockers = evaluate(database, &context, kind, &hash, threshold, retention)?;
        if !blockers.is_empty() {
            // A proof recorded earlier no longer holds; it must never execute.
            for existing in planned
                .iter()
                .filter(|proof| proof.kind == kind && proof.hash == hash)
            {
                database.finish_gc_proof(
                    existing.proof_id,
                    "stale",
                    Some(&format!("blocked before apply: {}", blockers.join("; "))),
                )?;
                totals.stale += 1;
            }
            totals.blocked += 1;
            report_candidates.push(GcCandidate {
                kind,
                hash: hash_text,
                reason,
                status: "blocked".to_owned(),
                proof_id: None,
                blockers,
            });
            continue;
        }
        // Reuse a matching planned proof; supersede any stale one.
        let mut proof_id = None;
        for existing in planned
            .iter()
            .filter(|proof| proof.kind == kind && proof.hash == hash)
        {
            if existing.content_generation == context.content_generation
                && existing.incarnation_digest == context.incarnation_digest
                && existing.observation_digest == context.observation_digest
            {
                proof_id = Some(existing.proof_id);
            } else {
                database.finish_gc_proof(
                    existing.proof_id,
                    "stale",
                    Some("inputs changed before apply"),
                )?;
                totals.stale += 1;
            }
        }
        let proof_id = match proof_id {
            Some(id) => id,
            None => database.record_gc_proof(&NewGcProof {
                kind,
                hash: hash.clone(),
                reason: reason.clone(),
                content_generation: context.content_generation,
                incarnation_digest: context.incarnation_digest.clone(),
                observation_digest: context.observation_digest.clone(),
                retention_seconds: retention,
            })?,
        };
        totals.proven += 1;
        let mut status = "planned".to_owned();
        if options.apply {
            // Re-derive at apply time; deletion only follows a fresh proof.
            let fresh = proof_context(database, now)?;
            let recheck = evaluate(database, &fresh, kind, &hash, threshold, retention)?;
            if fresh.content_generation != context.content_generation
                || fresh.incarnation_digest != context.incarnation_digest
                || fresh.observation_digest != context.observation_digest
                || !recheck.is_empty()
            {
                database.finish_gc_proof(proof_id, "stale", Some("inputs changed before apply"))?;
                totals.stale += 1;
                status = "stale".to_owned();
            } else if live.len() != agents.len() {
                totals.deferred += 1;
                status = "deferred".to_owned();
            } else {
                let mut all_deleted = true;
                for agent in &live {
                    let url = format!(
                        "{}/v1/objects/{}/{}",
                        agent.target.base_url,
                        kind.as_str(),
                        hash
                    );
                    match client.delete(url).send().await {
                        Ok(response) if response.status().is_success() => {
                            database.delete_placement(kind, &hash, &agent.target.id)?;
                        }
                        Ok(response) => {
                            all_deleted = false;
                            issues.push(format!(
                                "agent {} refused DELETE of {kind} {hash}: HTTP {}",
                                agent.target.id,
                                response.status()
                            ));
                        }
                        Err(error) => {
                            all_deleted = false;
                            issues.push(format!(
                                "agent {} DELETE of {kind} {hash} failed: {error}",
                                agent.target.id
                            ));
                        }
                    }
                }
                if all_deleted {
                    // Rows for superseded or retired incarnations refer to
                    // volumes that retirement or wiping already owns.
                    database.delete_placements(kind, &hash)?;
                    database.finish_gc_proof(proof_id, "applied", None)?;
                    totals.applied += 1;
                    status = "applied".to_owned();
                } else {
                    totals.deferred += 1;
                    status = "partial".to_owned();
                }
            }
        }
        report_candidates.push(GcCandidate {
            kind,
            hash: hash_text,
            reason,
            status,
            proof_id: Some(proof_id),
            blockers: Vec::new(),
        });
    }
    if options.apply && live.len() != agents.len() && totals.deferred > 0 {
        issues.push("deletion deferred: not every configured agent was reachable".to_owned());
    }

    Ok(GcReportV1 {
        report_type: "GcReportV1".to_owned(),
        version: 1,
        apply: options.apply,
        content_generation: context.content_generation,
        incarnation_digest: context.incarnation_digest,
        observation_digest: context.observation_digest,
        retention_seconds: options.retention_seconds,
        staging_grace_seconds: options.staging_grace_seconds,
        candidates: report_candidates,
        totals,
        issues,
    })
}

#[derive(Debug, Error)]
pub enum LifecycleError {
    #[error("agent identity does not match configuration: {0}")]
    AgentIdentity(String),
    #[error("agent {0} is not in the configured agent set")]
    UnknownAgent(String),
    #[error("system clock is before the Unix epoch")]
    Clock,
    #[error(transparent)]
    Metadata(#[from] MetadataError),
    #[error(transparent)]
    Http(#[from] reqwest::Error),
    #[error(transparent)]
    Json(#[from] serde_json::Error),
}
