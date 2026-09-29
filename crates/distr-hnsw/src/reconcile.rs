//! Continuous inventory comparison and scrub for the M1 blob plane.
//!
//! A scrub job verifies every object the current file projection requires:
//! it lists each reachable agent's namespaces with strict paginated
//! inventories, reads the bytes of every listed required copy, checks the
//! content address, and records the outcome on the placement row. It then
//! derives per-object durability health and, in repair mode, restores copies
//! using the copy-first protocol from `docs/m1-lifecycle-contract.md`.
//!
//! Scrub never deletes anything. Absence is only concluded from a complete
//! scan of an active incarnation; a failed page or an unreachable agent
//! leaves the placement unverified rather than missing.

use std::collections::{BTreeMap, BTreeSet};

use serde::Serialize;
use thiserror::Error;
use uuid::Uuid;

use crate::{
    durability::{InventoryPage, MAX_INVENTORY_PAGE},
    metadata::{
        AgentIncarnation, Database, IncarnationObservation, JobMode, JobStatus, MetadataError,
        ObjectHealth, ObjectHealthState, ObjectHealthSummary, PlacementState, PlacementTarget,
        ReconcileJob, RequiredObject, ScanCompletion, ScanStatus,
    },
    object::{ObjectHash, ObjectKind},
    portal::AgentTarget,
};

const MINIMUM_REPLICAS: usize = 2;
const DESIRED_REPLICAS: usize = 2;

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct AgentScrubSummary {
    pub agent_id: String,
    pub failure_domain: String,
    pub reachable: bool,
    pub incarnation_id: Option<String>,
    pub incarnation: Option<IncarnationObservation>,
    pub listed_objects: usize,
    pub unreferenced_objects: usize,
    pub valid_copies: usize,
    pub corrupt_copies: usize,
    pub missing_copies: usize,
    pub unverified_copies: usize,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct ScanSummary {
    pub agent_id: String,
    pub incarnation_id: String,
    pub kind: ObjectKind,
    pub status: ScanStatus,
    pub object_count: u64,
    pub inventory_digest: Option<String>,
    pub detail: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct ScrubRepair {
    pub kind: ObjectKind,
    pub hash: String,
    pub source_agent: String,
    pub target_agent: String,
    pub status: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct PlacementFinding {
    pub agent_id: String,
    pub state: PlacementState,
    pub verified_in_this_job: bool,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct ObjectFinding {
    pub kind: ObjectKind,
    pub hash: String,
    pub file_id: Uuid,
    pub generation: u64,
    pub state: ObjectHealthState,
    pub verified_copies: usize,
    pub verified_domains: usize,
    pub placements: Vec<PlacementFinding>,
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize)]
pub struct ScrubTotals {
    pub agents: usize,
    pub reachable_agents: usize,
    pub required_objects: usize,
    pub valid_copies: usize,
    pub corrupt_copies: usize,
    pub missing_copies: usize,
    pub unverified_copies: usize,
    pub repairs_planned: usize,
    pub repairs_applied: usize,
    pub repairs_deferred: usize,
    pub files_affected: usize,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct ScrubReportV1 {
    pub report_type: String,
    pub version: u16,
    pub mode: JobMode,
    pub job_id: Uuid,
    /// True when every configured agent was reachable and every namespace
    /// scan completed. Only a complete observation may drive repair.
    pub complete_observation: bool,
    pub agents: Vec<AgentScrubSummary>,
    pub scans: Vec<ScanSummary>,
    pub health: ObjectHealthSummary,
    pub findings: Vec<ObjectFinding>,
    pub repairs: Vec<ScrubRepair>,
    pub issues: Vec<String>,
    pub totals: ScrubTotals,
}

impl ScrubReportV1 {
    /// 0 when every required object is durable under a complete observation;
    /// 2 when anything is degraded, at risk, lost, or unobserved.
    pub fn exit_code(&self) -> u8 {
        let non_durable = self.health.degraded + self.health.at_risk + self.health.lost;
        if self.complete_observation && non_durable == 0 && self.issues.is_empty() {
            0
        } else {
            2
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct HealthReportV1 {
    pub report_type: String,
    pub version: u16,
    pub latest_job: Option<ReconcileJob>,
    pub incarnations: Vec<AgentIncarnation>,
    pub latest_complete_scans: Vec<ScanSummary>,
    pub health: ObjectHealthSummary,
    pub unhealthy_objects: Vec<ObjectHealth>,
}

impl HealthReportV1 {
    pub fn exit_code(&self) -> u8 {
        let complete = self
            .latest_job
            .as_ref()
            .is_some_and(|job| job.status == JobStatus::Complete);
        let non_durable = self.health.degraded + self.health.at_risk + self.health.lost;
        if complete && non_durable == 0 {
            0
        } else {
            2
        }
    }
}

/// Externally observable health derived from persisted state only; it does
/// not contact agents.
pub fn health_report(database: &Database) -> Result<HealthReportV1, ScrubError> {
    let latest_complete_scans = database
        .latest_complete_scans()?
        .into_iter()
        .map(|scan| ScanSummary {
            agent_id: scan.agent_id,
            incarnation_id: scan.incarnation_id,
            kind: scan.kind,
            status: ScanStatus::Complete,
            object_count: 0,
            inventory_digest: Some(scan.inventory_digest),
            detail: None,
        })
        .collect();
    Ok(HealthReportV1 {
        report_type: "HealthReportV1".to_owned(),
        version: 1,
        latest_job: database.latest_reconcile_job()?,
        incarnations: database.agent_incarnations()?,
        latest_complete_scans,
        health: database.object_health_summary()?,
        unhealthy_objects: database.unhealthy_objects()?,
    })
}

pub async fn scrub(
    database: &mut Database,
    agents: &[AgentTarget],
    client: &reqwest::Client,
    mode: JobMode,
) -> Result<ScrubReportV1, ScrubError> {
    let job_id = database.create_reconcile_job(mode)?;
    let result = run_job(database, agents, client, mode, job_id).await;
    match result {
        Ok(report) => {
            let json = serde_json::to_string(&report)?;
            database.finish_reconcile_job(job_id, JobStatus::Complete, Some(&json))?;
            Ok(report)
        }
        Err(error) => {
            let detail = error.to_string();
            database.finish_reconcile_job(job_id, JobStatus::Failed, Some(&detail))?;
            Err(error)
        }
    }
}

struct ReachableAgent {
    target: AgentTarget,
    incarnation_id: String,
}

impl ReachableAgent {
    fn placement(&self) -> PlacementTarget {
        PlacementTarget {
            agent_id: self.target.id.clone(),
            failure_domain: self.target.failure_domain.clone(),
            incarnation_id: self.incarnation_id.clone(),
        }
    }
}

async fn run_job(
    database: &mut Database,
    agents: &[AgentTarget],
    client: &reqwest::Client,
    mode: JobMode,
    job_id: Uuid,
) -> Result<ScrubReportV1, ScrubError> {
    let mut issues = Vec::new();
    let mut summaries: Vec<AgentScrubSummary> = Vec::new();
    let mut reachable: Vec<ReachableAgent> = Vec::new();

    for agent in agents {
        match fetch_health(client, agent).await {
            Ok(identity) => {
                let outcome = database.observe_agent_incarnation(
                    &agent.id,
                    &agent.failure_domain,
                    &identity.incarnation_id,
                )?;
                if let IncarnationObservation::Superseded { previous } = &outcome {
                    issues.push(format!(
                        "agent {} returned as incarnation {} replacing {previous}; its earlier copies no longer count",
                        agent.id, identity.incarnation_id
                    ));
                }
                summaries.push(AgentScrubSummary {
                    agent_id: agent.id.clone(),
                    failure_domain: agent.failure_domain.clone(),
                    reachable: true,
                    incarnation_id: Some(identity.incarnation_id.clone()),
                    incarnation: Some(outcome),
                    listed_objects: 0,
                    unreferenced_objects: 0,
                    valid_copies: 0,
                    corrupt_copies: 0,
                    missing_copies: 0,
                    unverified_copies: 0,
                });
                reachable.push(ReachableAgent {
                    target: agent.clone(),
                    incarnation_id: identity.incarnation_id,
                });
            }
            Err(ScrubError::Unreachable { agent: id, detail }) => {
                issues.push(format!("agent {id} unreachable: {detail}"));
                summaries.push(AgentScrubSummary {
                    agent_id: agent.id.clone(),
                    failure_domain: agent.failure_domain.clone(),
                    reachable: false,
                    incarnation_id: None,
                    incarnation: None,
                    listed_objects: 0,
                    unreferenced_objects: 0,
                    valid_copies: 0,
                    corrupt_copies: 0,
                    missing_copies: 0,
                    unverified_copies: 0,
                });
            }
            Err(error) => return Err(error),
        }
    }

    // Strict inventories. A namespace counts as observed only when every page
    // arrived in order and the final page had no continuation cursor.
    let mut listings: BTreeMap<(ObjectKind, String), BTreeMap<String, u64>> = BTreeMap::new();
    let mut complete_scans: BTreeSet<(String, ObjectKind)> = BTreeSet::new();
    let mut scans = Vec::new();
    for agent in &reachable {
        for kind in ObjectKind::ALL {
            let scan_id =
                database.start_scan(job_id, &agent.target.id, &agent.incarnation_id, kind)?;
            match list_namespace(client, &agent.target, kind).await {
                Ok(listing) => {
                    database.complete_scan(&ScanCompletion {
                        scan_id,
                        final_cursor: listing.final_cursor.clone(),
                        object_count: listing.objects.len() as u64,
                        inventory_digest: listing.digest.clone(),
                    })?;
                    complete_scans.insert((agent.target.id.clone(), kind));
                    scans.push(ScanSummary {
                        agent_id: agent.target.id.clone(),
                        incarnation_id: agent.incarnation_id.clone(),
                        kind,
                        status: ScanStatus::Complete,
                        object_count: listing.objects.len() as u64,
                        inventory_digest: Some(listing.digest),
                        detail: None,
                    });
                    let summary = summary_mut(&mut summaries, &agent.target.id);
                    summary.listed_objects += listing.objects.len();
                    for (hash, size) in listing.objects {
                        listings
                            .entry((kind, hash))
                            .or_default()
                            .insert(agent.target.id.clone(), size);
                    }
                }
                Err(error) => {
                    let detail = error.to_string();
                    database.fail_scan(scan_id, &detail)?;
                    issues.push(format!(
                        "inventory of {kind} on agent {} failed and proves nothing: {detail}",
                        agent.target.id
                    ));
                    scans.push(ScanSummary {
                        agent_id: agent.target.id.clone(),
                        incarnation_id: agent.incarnation_id.clone(),
                        kind,
                        status: ScanStatus::Failed,
                        object_count: 0,
                        inventory_digest: None,
                        detail: Some(detail),
                    });
                }
            }
        }
    }
    let complete_observation = reachable.len() == agents.len()
        && complete_scans.len() == reachable.len() * ObjectKind::ALL.len();

    let required = database.required_objects()?;
    let required_keys: BTreeSet<(ObjectKind, String)> = required
        .iter()
        .map(|object| (object.kind, object.hash.to_string()))
        .collect();
    for ((kind, hash), holders) in &listings {
        if !required_keys.contains(&(*kind, hash.clone())) {
            for agent_id in holders.keys() {
                summary_mut(&mut summaries, agent_id).unreferenced_objects += 1;
            }
        }
    }

    let mut totals = ScrubTotals {
        agents: agents.len(),
        reachable_agents: reachable.len(),
        required_objects: required.len(),
        ..ScrubTotals::default()
    };
    let mut health = Vec::with_capacity(required.len());
    let mut findings = Vec::new();
    let mut repairs = Vec::new();
    let mut files_affected: BTreeSet<Uuid> = BTreeSet::new();
    let mut deferred_notice = false;

    for object in &required {
        let mut verification = verify_object(
            database,
            client,
            agents,
            &reachable,
            &complete_scans,
            &listings,
            object,
            job_id,
            &mut summaries,
        )
        .await?;
        totals.valid_copies += verification.valid.len();
        totals.corrupt_copies += verification.corrupt;
        totals.missing_copies += verification.missing;
        totals.unverified_copies += verification.unverified;

        let mut state = classify(database, agents, object, &verification)?;
        if state != ObjectHealthState::Durable && mode == JobMode::Repair {
            if !complete_observation {
                totals.repairs_deferred += 1;
                if !deferred_notice {
                    issues.push(
                        "repairs deferred: observation incomplete, refusing to change placements"
                            .to_owned(),
                    );
                    deferred_notice = true;
                }
            } else if let Some(bytes) = verification.source_bytes.clone() {
                let applied = repair_object(
                    database,
                    client,
                    &reachable,
                    object,
                    &bytes,
                    &mut verification,
                    job_id,
                    &mut repairs,
                    &mut issues,
                )
                .await?;
                totals.repairs_planned += applied.0;
                totals.repairs_applied += applied.1;
                state = classify(database, agents, object, &verification)?;
            }
        }

        health.push(ObjectHealth {
            kind: object.kind,
            hash: object.hash.clone(),
            file_id: object.file_id,
            generation: object.generation,
            verified_copies: verification.valid.len(),
            verified_domains: verification.domains().len(),
            state,
            job_id,
            updated_at: 0,
        });
        if state != ObjectHealthState::Durable {
            files_affected.insert(object.file_id);
            let placements = database
                .placement_states(object.kind, &object.hash)?
                .into_iter()
                .map(|(agent_id, placement_state, _)| PlacementFinding {
                    verified_in_this_job: verification.valid.contains(&agent_id)
                        || verification.touched.contains(&agent_id),
                    agent_id,
                    state: placement_state,
                })
                .collect();
            findings.push(ObjectFinding {
                kind: object.kind,
                hash: object.hash.to_string(),
                file_id: object.file_id,
                generation: object.generation,
                state,
                verified_copies: verification.valid.len(),
                verified_domains: verification.domains().len(),
                placements,
            });
        }
    }
    totals.files_affected = files_affected.len();
    database.replace_object_health(job_id, &health)?;
    let health_summary = database.object_health_summary()?;

    Ok(ScrubReportV1 {
        report_type: "ScrubReportV1".to_owned(),
        version: 1,
        mode,
        job_id,
        complete_observation,
        agents: summaries,
        scans,
        health: health_summary,
        findings,
        repairs,
        issues,
        totals,
    })
}

struct Verification {
    /// Agents whose copy was read back and hash-verified in this job.
    valid: BTreeSet<String>,
    /// Failure domain of each valid agent.
    valid_domains: BTreeMap<String, String>,
    /// Agents whose placement state was changed in this job.
    touched: BTreeSet<String>,
    corrupt: usize,
    missing: usize,
    unverified: usize,
    source_bytes: Option<Vec<u8>>,
}

impl Verification {
    fn domains(&self) -> BTreeSet<&str> {
        self.valid_domains.values().map(String::as_str).collect()
    }
}

#[allow(clippy::too_many_arguments)]
async fn verify_object(
    database: &mut Database,
    client: &reqwest::Client,
    agents: &[AgentTarget],
    reachable: &[ReachableAgent],
    complete_scans: &BTreeSet<(String, ObjectKind)>,
    listings: &BTreeMap<(ObjectKind, String), BTreeMap<String, u64>>,
    object: &RequiredObject,
    job_id: Uuid,
    summaries: &mut [AgentScrubSummary],
) -> Result<Verification, ScrubError> {
    let mut verification = Verification {
        valid: BTreeSet::new(),
        valid_domains: BTreeMap::new(),
        touched: BTreeSet::new(),
        corrupt: 0,
        missing: 0,
        unverified: 0,
        source_bytes: None,
    };
    let existing: BTreeMap<String, PlacementState> = database
        .placement_states(object.kind, &object.hash)?
        .into_iter()
        .map(|(agent, state, _)| (agent, state))
        .collect();
    let holders = listings.get(&(object.kind, object.hash.to_string()));

    for agent in reachable {
        let agent_id = agent.target.id.as_str();
        let listed_size = holders.and_then(|holders| holders.get(agent_id)).copied();
        if let Some(size) = listed_size {
            let valid = match fetch_bytes(client, &agent.target, object.kind, &object.hash).await {
                Some(bytes)
                    if ObjectHash::digest(&bytes) == object.hash
                        && bytes.len() as u64 == size
                        && object
                            .expected_len
                            .is_none_or(|expected| expected == bytes.len() as u64) =>
                {
                    if verification.source_bytes.is_none() {
                        verification.source_bytes = Some(bytes);
                    }
                    true
                }
                _ => false,
            };
            let state = if valid {
                PlacementState::Confirmed
            } else {
                PlacementState::Corrupt
            };
            database.set_placement_verification(
                object.kind,
                &object.hash,
                &agent.placement(),
                state,
                job_id,
            )?;
            verification.touched.insert(agent_id.to_owned());
            let summary = summary_mut(summaries, agent_id);
            if valid {
                summary.valid_copies += 1;
                verification.valid.insert(agent_id.to_owned());
                verification
                    .valid_domains
                    .insert(agent_id.to_owned(), agent.target.failure_domain.clone());
            } else {
                summary.corrupt_copies += 1;
                verification.corrupt += 1;
            }
        } else if complete_scans.contains(&(agent_id.to_owned(), object.kind)) {
            // A complete scan of this incarnation proves absence.
            if existing.contains_key(agent_id) {
                database.set_placement_verification(
                    object.kind,
                    &object.hash,
                    &agent.placement(),
                    PlacementState::Missing,
                    job_id,
                )?;
                verification.touched.insert(agent_id.to_owned());
                summary_mut(summaries, agent_id).missing_copies += 1;
                verification.missing += 1;
            }
        } else if existing.contains_key(agent_id) {
            summary_mut(summaries, agent_id).unverified_copies += 1;
            verification.unverified += 1;
        }
    }
    // Placements on configured agents that could not be contacted stay in
    // their recorded state and are reported as unverified.
    for agent in agents {
        let reachable_now = reachable.iter().any(|other| other.target.id == agent.id);
        if !reachable_now && existing.contains_key(agent.id.as_str()) {
            summary_mut(summaries, &agent.id).unverified_copies += 1;
            verification.unverified += 1;
        }
    }
    Ok(verification)
}

fn classify(
    database: &Database,
    agents: &[AgentTarget],
    object: &RequiredObject,
    verification: &Verification,
) -> Result<ObjectHealthState, ScrubError> {
    let copies = verification.valid.len();
    let domains = verification.domains().len();
    if copies == 0 {
        return Ok(ObjectHealthState::Lost);
    }
    if domains < MINIMUM_REPLICAS {
        return Ok(ObjectHealthState::AtRisk);
    }
    let configured: BTreeSet<&str> = agents.iter().map(|agent| agent.id.as_str()).collect();
    let damaged = database
        .placement_states(object.kind, &object.hash)?
        .into_iter()
        .any(|(agent, state, _)| {
            configured.contains(agent.as_str())
                && matches!(state, PlacementState::Missing | PlacementState::Corrupt)
        });
    if damaged || copies < DESIRED_REPLICAS {
        return Ok(ObjectHealthState::Degraded);
    }
    Ok(ObjectHealthState::Durable)
}

/// Copy-first repair: PUT to a destination, read it back, and only then
/// record the confirmed placement. Returns `(planned, applied)`.
#[allow(clippy::too_many_arguments)]
async fn repair_object(
    database: &mut Database,
    client: &reqwest::Client,
    reachable: &[ReachableAgent],
    object: &RequiredObject,
    bytes: &[u8],
    verification: &mut Verification,
    job_id: Uuid,
    repairs: &mut Vec<ScrubRepair>,
    issues: &mut Vec<String>,
) -> Result<(usize, usize), ScrubError> {
    let source = verification
        .valid
        .iter()
        .next()
        .cloned()
        .expect("repair requires a verified source copy");
    let mut planned = 0;
    let mut applied = 0;
    for agent in reachable {
        let domains = verification.domains().len();
        if domains >= MINIMUM_REPLICAS && verification.valid.len() >= DESIRED_REPLICAS {
            break;
        }
        let agent_id = agent.target.id.as_str();
        if verification.valid.contains(agent_id) {
            continue;
        }
        let domain_has_copy = verification
            .valid_domains
            .values()
            .any(|domain| domain == &agent.target.failure_domain);
        // Restore a damaged copy in place, or extend to a new failure domain
        // when the floor is not met.
        if domain_has_copy && domains >= MINIMUM_REPLICAS {
            continue;
        }
        planned += 1;
        let mut repair = ScrubRepair {
            kind: object.kind,
            hash: object.hash.to_string(),
            source_agent: source.clone(),
            target_agent: agent_id.to_owned(),
            status: "planned".to_owned(),
        };
        let url = format!(
            "{}/v1/objects/{}/{}",
            agent.target.base_url,
            object.kind.as_str(),
            object.hash
        );
        let put = client.put(url).body(bytes.to_vec()).send().await;
        let accepted = match put {
            Ok(response) if response.status().is_success() => true,
            Ok(response) => {
                issues.push(format!(
                    "repair target {agent_id} rejected {} {} with HTTP {}",
                    object.kind,
                    object.hash,
                    response.status()
                ));
                false
            }
            Err(error) => {
                issues.push(format!(
                    "repair target {agent_id} failed for {} {}: {error}",
                    object.kind, object.hash
                ));
                false
            }
        };
        if accepted {
            let read_back = fetch_bytes(client, &agent.target, object.kind, &object.hash)
                .await
                .is_some_and(|got| ObjectHash::digest(&got) == object.hash);
            if read_back {
                database.set_placement_verification(
                    object.kind,
                    &object.hash,
                    &agent.placement(),
                    PlacementState::Confirmed,
                    job_id,
                )?;
                verification.valid.insert(agent_id.to_owned());
                verification
                    .valid_domains
                    .insert(agent_id.to_owned(), agent.target.failure_domain.clone());
                verification.touched.insert(agent_id.to_owned());
                repair.status = "applied".to_owned();
                applied += 1;
            } else {
                issues.push(format!(
                    "repair target {agent_id} did not read back {} {} intact",
                    object.kind, object.hash
                ));
            }
        }
        repairs.push(repair);
    }
    Ok((planned, applied))
}

struct Listing {
    objects: Vec<(String, u64)>,
    final_cursor: Option<ObjectHash>,
    digest: String,
}

async fn list_namespace(
    client: &reqwest::Client,
    agent: &AgentTarget,
    kind: ObjectKind,
) -> Result<Listing, ScrubError> {
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"distr-hnsw:scan-inventory:v1");
    hasher.update(kind.as_str().as_bytes());
    let mut objects = Vec::new();
    let mut after: Option<ObjectHash> = None;
    loop {
        let mut request = client
            .get(format!("{}/v1/inventory/{}", agent.base_url, kind.as_str()))
            .query(&[("limit", MAX_INVENTORY_PAGE.to_string())]);
        if let Some(cursor) = &after {
            request = request.query(&[("after", cursor.as_str())]);
        }
        let response = request.send().await?;
        if !response.status().is_success() {
            return Err(ScrubError::InventoryRejected {
                agent: agent.id.clone(),
                status: response.status().as_u16(),
                body: response.text().await.unwrap_or_default(),
            });
        }
        let page: InventoryPage = response.json().await?;
        let mut previous = after.as_ref().map(ObjectHash::as_str);
        for object in &page.objects {
            if previous.is_some_and(|value| object.hash.as_str() <= value) {
                return Err(ScrubError::MalformedInventory(agent.id.clone()));
            }
            previous = Some(object.hash.as_str());
            hasher.update(object.hash.as_str().as_bytes());
            hasher.update(&object.size.to_le_bytes());
            objects.push((object.hash.to_string(), object.size));
        }
        match page.next_after {
            Some(cursor) if !page.objects.is_empty() => after = Some(cursor),
            Some(_) => return Err(ScrubError::MalformedInventory(agent.id.clone())),
            None => break,
        }
    }
    Ok(Listing {
        final_cursor: objects
            .last()
            .map(|(hash, _)| ObjectHash::parse(hash.clone()))
            .transpose()?,
        objects,
        digest: hasher.finalize().to_hex().to_string(),
    })
}

#[derive(serde::Deserialize)]
struct RemoteHealth {
    id: String,
    failure_domain: String,
    incarnation_id: String,
}

async fn fetch_health(
    client: &reqwest::Client,
    agent: &AgentTarget,
) -> Result<RemoteHealth, ScrubError> {
    let response = match client
        .get(format!("{}/v1/health", agent.base_url))
        .send()
        .await
    {
        Ok(response) => response,
        Err(error) => {
            return Err(ScrubError::Unreachable {
                agent: agent.id.clone(),
                detail: error.to_string(),
            })
        }
    };
    if !response.status().is_success() {
        return Err(ScrubError::Unreachable {
            agent: agent.id.clone(),
            detail: format!("health returned HTTP {}", response.status()),
        });
    }
    let identity: RemoteHealth =
        response
            .json()
            .await
            .map_err(|error| ScrubError::Unreachable {
                agent: agent.id.clone(),
                detail: format!("malformed health response: {error}"),
            })?;
    if identity.id != agent.id || identity.failure_domain != agent.failure_domain {
        return Err(ScrubError::AgentIdentity(agent.id.clone()));
    }
    Ok(identity)
}

async fn fetch_bytes(
    client: &reqwest::Client,
    agent: &AgentTarget,
    kind: ObjectKind,
    hash: &ObjectHash,
) -> Option<Vec<u8>> {
    let url = format!("{}/v1/objects/{}/{}", agent.base_url, kind.as_str(), hash);
    match client.get(url).send().await {
        Ok(response) if response.status().is_success() => {
            response.bytes().await.ok().map(|bytes| bytes.to_vec())
        }
        _ => None,
    }
}

fn summary_mut<'a>(
    summaries: &'a mut [AgentScrubSummary],
    agent_id: &str,
) -> &'a mut AgentScrubSummary {
    summaries
        .iter_mut()
        .find(|summary| summary.agent_id == agent_id)
        .expect("every configured agent has a summary")
}

#[derive(Debug, Error)]
pub enum ScrubError {
    #[error("agent identity does not match configuration: {0}")]
    AgentIdentity(String),
    #[error("agent {agent} unreachable: {detail}")]
    Unreachable { agent: String, detail: String },
    #[error("agent {agent} rejected inventory with HTTP {status}: {body}")]
    InventoryRejected {
        agent: String,
        status: u16,
        body: String,
    },
    #[error("agent returned malformed or non-monotonic inventory: {0}")]
    MalformedInventory(String),
    #[error(transparent)]
    Http(#[from] reqwest::Error),
    #[error(transparent)]
    Metadata(#[from] MetadataError),
    #[error(transparent)]
    Object(#[from] crate::object::ObjectError),
    #[error(transparent)]
    Json(#[from] serde_json::Error),
}
