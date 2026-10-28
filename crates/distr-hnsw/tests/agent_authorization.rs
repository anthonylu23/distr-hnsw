//! Request-level authorization matrix for agents (M2 work packages 2 and
//! 3; docs/m2-implementation-plan.md): every route refuses a missing,
//! malformed, altered, expired, wrong-agent, wrong-operation, wrong-object,
//! or stale-epoch grant; a newer epoch or unknown key asks the portal for
//! its key set; join tokens are single-use; key rotation and revocation
//! reach agents on their next request.

use std::{
    path::PathBuf,
    time::{SystemTime, UNIX_EPOCH},
};

use axum::body::Bytes;
use distr_hnsw::{
    access::{self, GrantMaterial},
    agent::{serve_agent, AgentIdentity, Refusal, KEY_SET_OUTDATED_HEADER},
    agent_client::{join_agent, AgentClient, JoinError},
    crypto::MasterKey,
    grant::{Grant, KeySet, KeyStatus, Operation, PortalSigner, Scope, TrustedKey, GRANT_SCHEME},
    metadata::Database,
    object::{ObjectHash, ObjectKind},
    portal::AgentTarget,
    tailscale::LocalApi,
};
use reqwest::{Method, StatusCode};
use tokio::net::TcpListener;
use uuid::Uuid;

fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64
}

fn no_localapi() -> LocalApi {
    LocalApi::new("/nonexistent/tailscaled.sock")
}

struct Harness {
    _dir: tempfile::TempDir,
    database_path: PathBuf,
    key: MasterKey,
    agent: AgentTarget,
}

impl Harness {
    fn database(&self) -> Database {
        Database::open(&self.database_path).unwrap()
    }

    fn material(&self) -> GrantMaterial {
        GrantMaterial::load_or_create(&mut self.database(), &self.key).unwrap()
    }

    fn client(&self) -> AgentClient {
        AgentClient::open(&mut self.database(), &self.key).unwrap()
    }
}

/// A loopback agent `a1` started with a fresh token, optionally joined.
async fn agent(join: bool) -> (Harness, String) {
    let dir = tempfile::tempdir().unwrap();
    let database_path = dir.path().join("portal.sqlite");
    let key = MasterKey::from_bytes([3; 32]);
    let mut database = Database::open(&database_path).unwrap();
    database.bind_master_key_id(&key.key_id_hex()).unwrap();
    let token = access::create_join_token(&mut database, "a1", now()).unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let agent = AgentTarget {
        id: "a1".into(),
        failure_domain: "d1".into(),
        base_url: format!("http://{}", listener.local_addr().unwrap()),
    };
    let volume = dir.path().join("volume");
    let agent_token = token.clone();
    tokio::spawn(async move {
        serve_agent(
            listener,
            volume,
            AgentIdentity {
                id: "a1".into(),
                failure_domain: "d1".into(),
            },
            &agent_token,
        )
        .await
        .unwrap();
    });
    if join {
        join_agent(&mut database, &key, &agent, &token, &no_localapi())
            .await
            .unwrap();
    }
    (
        Harness {
            _dir: dir,
            database_path,
            key,
            agent,
        },
        token,
    )
}

struct Route {
    method: Method,
    path: String,
    operation: Operation,
    scope: Scope,
    body: Option<&'static [u8]>,
    proof: Option<Uuid>,
}

fn routes(hash: &ObjectHash) -> Vec<Route> {
    let object = Scope::Object(ObjectKind::Chunk, hash.clone());
    let path = format!("/v1/objects/chunk/{hash}");
    vec![
        Route {
            method: Method::GET,
            path: "/v1/health".into(),
            operation: Operation::Health,
            scope: Scope::Agent,
            body: None,
            proof: None,
        },
        Route {
            method: Method::GET,
            path: "/v1/inventory/chunk".into(),
            operation: Operation::Inventory,
            scope: Scope::Kind(ObjectKind::Chunk),
            body: None,
            proof: None,
        },
        Route {
            method: Method::PUT,
            path: path.clone(),
            operation: Operation::Put,
            scope: object.clone(),
            body: Some(b"hello"),
            proof: None,
        },
        Route {
            method: Method::GET,
            path: path.clone(),
            operation: Operation::Get,
            scope: object.clone(),
            body: None,
            proof: None,
        },
        Route {
            method: Method::DELETE,
            path,
            operation: Operation::Delete,
            scope: object,
            body: None,
            proof: Some(Uuid::new_v4()),
        },
    ]
}

async fn call(
    agent: &AgentTarget,
    route: &Route,
    authorization: Option<String>,
) -> (StatusCode, Option<String>, Option<Refusal>) {
    let mut request = reqwest::Client::new().request(
        route.method.clone(),
        format!("{}{}", agent.base_url, route.path),
    );
    if let Some(value) = authorization {
        request = request.header("authorization", value);
    }
    if let Some(body) = route.body {
        request = request.body(body);
    }
    let response = request.send().await.unwrap();
    let status = response.status();
    let outdated = response
        .headers()
        .get(KEY_SET_OUTDATED_HEADER)
        .map(|value| value.to_str().unwrap().to_owned());
    let refusal = response.json::<Refusal>().await.ok();
    (status, outdated, refusal)
}

fn bearer(token: String) -> Option<String> {
    Some(format!("{GRANT_SCHEME} {token}"))
}

fn grant(signer: &PortalSigner, epoch: u64, agent: &str, route: &Route) -> Grant {
    let issued = now();
    Grant {
        key_id: signer.key_id(),
        epoch,
        agent_id: agent.into(),
        operation: route.operation,
        scope: route.scope.clone(),
        gc_proof: route.proof,
        issued_at: issued,
        expires_at: issued + 60,
        nonce: [9; 16],
    }
}

#[tokio::test]
async fn every_agent_route_refuses_every_bad_grant_and_accepts_a_valid_one() {
    let (harness, _) = agent(true).await;
    let material = harness.material();
    let signer = material.active();
    let epoch = material.epoch();
    let hash = ObjectHash::digest(b"hello");
    let other = ObjectHash::digest(b"other");
    let stranger = PortalSigner::generate();

    for route in routes(&hash) {
        let label = format!("{} {}", route.method, route.path);
        let refused =
            |status: StatusCode,
             code: &str,
             outcome: (StatusCode, Option<String>, Option<Refusal>)| {
                assert_eq!(outcome.0, status, "{label}: expected {status}");
                assert_eq!(
                    outcome.2.map(|r| r.code).as_deref(),
                    Some(code),
                    "{label}: refusal code"
                );
            };

        refused(
            StatusCode::UNAUTHORIZED,
            "grant_missing",
            call(&harness.agent, &route, None).await,
        );
        refused(
            StatusCode::UNAUTHORIZED,
            "grant_missing",
            call(&harness.agent, &route, Some("Bearer abc".into())).await,
        );
        refused(
            StatusCode::FORBIDDEN,
            "grant_rejected",
            call(&harness.agent, &route, bearer("not-a-grant".into())).await,
        );

        let mut wrong_agent = grant(signer, epoch, "a2", &route);
        wrong_agent.agent_id = "a2".into();
        refused(
            StatusCode::FORBIDDEN,
            "grant_rejected",
            call(
                &harness.agent,
                &route,
                bearer(signer.sign_grant(&wrong_agent)),
            )
            .await,
        );

        let mut wrong_operation = grant(signer, epoch, "a1", &route);
        wrong_operation.operation = if route.operation == Operation::Get {
            Operation::Put
        } else {
            Operation::Get
        };
        refused(
            StatusCode::FORBIDDEN,
            "grant_rejected",
            call(
                &harness.agent,
                &route,
                bearer(signer.sign_grant(&wrong_operation)),
            )
            .await,
        );

        let mut wrong_object = grant(signer, epoch, "a1", &route);
        wrong_object.scope = match &route.scope {
            Scope::Agent => Scope::Kind(ObjectKind::Chunk),
            Scope::Kind(_) => Scope::Kind(ObjectKind::Manifest),
            Scope::Object(kind, _) => Scope::Object(*kind, other.clone()),
        };
        refused(
            StatusCode::FORBIDDEN,
            "grant_rejected",
            call(
                &harness.agent,
                &route,
                bearer(signer.sign_grant(&wrong_object)),
            )
            .await,
        );

        let mut expired = grant(signer, epoch, "a1", &route);
        expired.issued_at -= 1_000;
        expired.expires_at -= 1_000;
        refused(
            StatusCode::FORBIDDEN,
            "grant_rejected",
            call(&harness.agent, &route, bearer(signer.sign_grant(&expired))).await,
        );

        let valid = signer.sign_grant(&grant(signer, epoch, "a1", &route));
        let mut altered = valid.clone().into_bytes();
        let last = altered.len() - 1;
        altered[last] = if altered[last] == b'A' { b'B' } else { b'A' };
        refused(
            StatusCode::FORBIDDEN,
            "grant_rejected",
            call(
                &harness.agent,
                &route,
                bearer(String::from_utf8(altered).unwrap()),
            )
            .await,
        );

        let stale = grant(signer, epoch - 1, "a1", &route);
        refused(
            StatusCode::FORBIDDEN,
            "grant_rejected",
            call(&harness.agent, &route, bearer(signer.sign_grant(&stale))).await,
        );

        // A newer epoch or an unknown key asks the portal for its key set.
        let newer = grant(signer, epoch + 1, "a1", &route);
        let outcome = call(&harness.agent, &route, bearer(signer.sign_grant(&newer))).await;
        assert_eq!(outcome.1.as_deref(), Some("1"), "{label}: outdated header");
        refused(StatusCode::CONFLICT, "key_set_outdated", outcome);
        let forged = grant(&stranger, epoch, "a1", &route);
        let outcome = call(&harness.agent, &route, bearer(stranger.sign_grant(&forged))).await;
        assert!(
            outcome.1.is_some(),
            "{label}: unknown key asks for the key set"
        );
        refused(StatusCode::CONFLICT, "key_set_outdated", outcome);

        if route.operation == Operation::Delete {
            let mut unproven = grant(signer, epoch, "a1", &route);
            unproven.gc_proof = None;
            refused(
                StatusCode::FORBIDDEN,
                "grant_rejected",
                call(&harness.agent, &route, bearer(signer.sign_grant(&unproven))).await,
            );
        }

        let (status, _, _) = call(&harness.agent, &route, bearer(valid)).await;
        assert!(status.is_success(), "{label}: valid grant got {status}");
    }
}

#[tokio::test]
async fn an_unjoined_agent_answers_only_join_and_tokens_are_single_use() {
    let (harness, token) = agent(false).await;
    let hash = ObjectHash::digest(b"hello");
    let material = harness.material();
    let health = &routes(&hash)[0];
    let valid =
        material
            .active()
            .sign_grant(&grant(material.active(), material.epoch(), "a1", health));
    let (status, _, refusal) = call(&harness.agent, health, bearer(valid)).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(refusal.unwrap().code, "not_joined");

    let mut database = harness.database();
    // A token for another agent id, and a token the agent was not started
    // with, are both refused.
    let other = access::create_join_token(&mut database, "a2", now()).unwrap();
    assert!(matches!(
        join_agent(&mut database, &harness.key, &harness.agent, &other, &no_localapi()).await,
        Err(JoinError::TokenForAnotherAgent(id)) if id == "a2"
    ));
    let unarmed = access::create_join_token(&mut database, "a1", now()).unwrap();
    assert!(matches!(
        join_agent(&mut database, &harness.key, &harness.agent, &unarmed, &no_localapi()).await,
        Err(JoinError::Refused { status, .. }) if status == StatusCode::FORBIDDEN
    ));

    let outcome = join_agent(
        &mut database,
        &harness.key,
        &harness.agent,
        &token,
        &no_localapi(),
    )
    .await
    .unwrap();
    assert!(!outcome.rejoined && !outcome.portal_node_pinned && outcome.agent_node.is_none());
    assert!(matches!(
        join_agent(
            &mut database,
            &harness.key,
            &harness.agent,
            &token,
            &no_localapi()
        )
        .await,
        Err(JoinError::TokenUsed)
    ));
    assert!(harness
        .client()
        .health(&harness.agent)
        .await
        .unwrap()
        .status()
        .is_success());

    // Another portal's key set cannot take over a joined agent.
    let intruder = PortalSigner::generate();
    let foreign = KeySet {
        version: 99,
        epoch: 99,
        keys: vec![TrustedKey {
            key_id: intruder.key_id(),
            public: intruder.verifying_key(),
            status: KeyStatus::Active,
        }],
    }
    .sign(&[&intruder]);
    let response = reqwest::Client::new()
        .put(format!("{}/v1/trust", harness.agent.base_url))
        .body(foreign.to_bytes())
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
    assert_eq!(
        response.json::<Refusal>().await.unwrap().code,
        "key_set_untrusted"
    );
}

#[tokio::test]
async fn rotation_and_revocation_reach_agents_on_their_next_request() {
    let (harness, _) = agent(true).await;
    let hash = ObjectHash::digest(b"hello");
    let original = harness.material();
    let old_signer_grant = |route: &Route| {
        original
            .active()
            .sign_grant(&grant(original.active(), original.epoch(), "a1", route))
    };

    // Rotate: the agent learns the new key from the key set the client
    // pushes when it answers 409, signed by both keys.
    let mut database = harness.database();
    let new_key = access::rotate_signing_key(&mut database, &harness.key).unwrap();
    let client = harness.client();
    assert_eq!(client.material().active().key_id(), new_key);
    assert!(client
        .put(
            &harness.agent,
            ObjectKind::Chunk,
            &hash,
            Bytes::from_static(b"hello")
        )
        .await
        .unwrap()
        .status()
        .is_success());
    // The retiring key still verifies until it is revoked.
    let get = &routes(&hash)[3];
    assert!(call(&harness.agent, get, bearer(old_signer_grant(get)))
        .await
        .0
        .is_success());

    // Revoke the old key: the epoch rises, the next request pushes the new
    // set, and grants from the revoked key are stale.
    access::end_signing_key(&mut database, &original.active().key_id().to_string(), true).unwrap();
    let client = harness.client();
    assert_eq!(client.material().epoch(), original.epoch() + 1);
    assert!(client
        .get(&harness.agent, ObjectKind::Chunk, &hash)
        .await
        .unwrap()
        .status()
        .is_success());
    let (status, _, refusal) = call(&harness.agent, get, bearer(old_signer_grant(get))).await;
    assert!(
        status == StatusCode::FORBIDDEN || status == StatusCode::CONFLICT,
        "revoked key's grant got {status}"
    );
    assert_ne!(refusal.map(|r| r.code), None);
    assert!(!call(&harness.agent, get, bearer(old_signer_grant(get)))
        .await
        .0
        .is_success());

    // An older key set is refused.
    let response = reqwest::Client::new()
        .put(format!("{}/v1/trust", harness.agent.base_url))
        .body(original.signed_key_set().to_bytes())
        .send()
        .await
        .unwrap();
    assert_eq!(
        response.status(),
        StatusCode::FORBIDDEN,
        "signed only by the revoked key"
    );
}

#[tokio::test]
async fn an_agent_joined_to_one_portal_refuses_another_portals_grants() {
    let (harness, _) = agent(true).await;
    let dir = tempfile::tempdir().unwrap();
    let other_key = MasterKey::from_bytes([5; 32]);
    let mut other = Database::open(&dir.path().join("other.sqlite")).unwrap();
    other.bind_master_key_id(&other_key.key_id_hex()).unwrap();
    let response = AgentClient::open(&mut other, &other_key)
        .unwrap()
        .health(&harness.agent)
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
    assert_eq!(
        response.json::<Refusal>().await.unwrap().code,
        "key_set_untrusted"
    );
}
