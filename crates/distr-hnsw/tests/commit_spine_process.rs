#![cfg(unix)]

use std::{
    fs,
    net::{SocketAddr, TcpListener, TcpStream},
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    thread,
    time::{Duration, Instant},
};

use distr_hnsw::{metadata::Database, portal::Failpoint, CHUNK_SIZE};
use uuid::Uuid;

struct AgentProcess {
    child: Child,
}

impl Drop for AgentProcess {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn free_address() -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.local_addr().unwrap()
}

fn start_agent(binary: &Path, id: &str, domain: &str, volume: &Path) -> (AgentProcess, String) {
    let address = free_address();
    let child = Command::new(binary)
        .args([
            "agent",
            "--id",
            id,
            "--failure-domain",
            domain,
            "--bind",
            &address.to_string(),
            "--volume",
        ])
        .arg(volume)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();

    let deadline = Instant::now() + Duration::from_secs(5);
    while TcpStream::connect(address).is_err() {
        assert!(Instant::now() < deadline, "agent {id} did not start");
        thread::sleep(Duration::from_millis(20));
    }
    (
        AgentProcess { child },
        format!("{id},{domain},http://{address}"),
    )
}

fn portal_command(
    binary: &Path,
    operation: &str,
    database: &Path,
    master_key: &Path,
    agents: &[String],
) -> Command {
    let mut command = Command::new(binary);
    command
        .args(["portal", operation, "--database"])
        .arg(database)
        .arg("--master-key")
        .arg(master_key);
    for agent in agents {
        command.arg("--agent").arg(agent);
    }
    command
}

#[test]
fn abrupt_portal_exit_at_every_boundary_recovers() {
    let binary = PathBuf::from(env!("CARGO_BIN_EXE_distr-hnsw"));
    let workspace = tempfile::tempdir().unwrap();
    let (_agent_a, target_a) = start_agent(
        &binary,
        "agent-a",
        "host-a",
        &workspace.path().join("agent-a"),
    );
    let (_agent_b, target_b) = start_agent(
        &binary,
        "agent-b",
        "host-b",
        &workspace.path().join("agent-b"),
    );
    let agents = vec![target_a, target_b];
    let source = workspace.path().join("source.bin");
    let expected: Vec<_> = (0..CHUNK_SIZE + 19)
        .map(|index| ((index * 13 + 5) % 253) as u8)
        .collect();
    fs::write(&source, &expected).unwrap();

    let failpoints = [
        Failpoint::AfterPlan,
        Failpoint::AfterFirstChunkReplica,
        Failpoint::AfterChunksDurable,
        Failpoint::AfterFirstManifestReplica,
        Failpoint::AfterManifestDurable,
        Failpoint::BeforeCommit,
        Failpoint::AfterCommit,
    ];

    for failpoint in failpoints {
        let case = workspace.path().join(failpoint.as_str());
        fs::create_dir_all(&case).unwrap();
        let database = case.join("portal.sqlite");
        let master_key = case.join("master.key");
        let init = Command::new(&binary)
            .args(["portal", "init", "--no-recovery-bundle", "--database"])
            .arg(&database)
            .arg("--master-key")
            .arg(&master_key)
            .output()
            .unwrap();
        assert!(
            init.status.success(),
            "{}",
            String::from_utf8_lossy(&init.stderr)
        );

        let idempotency_key = format!("process-{}", failpoint.as_str());
        let crashed = portal_command(&binary, "put", &database, &master_key, &agents)
            .arg("--idempotency-key")
            .arg(&idempotency_key)
            .arg(&source)
            .env("DISTR_HNSW_FAILPOINT", failpoint.as_str())
            .output()
            .unwrap();
        assert_eq!(
            crashed.status.code(),
            Some(86),
            "failpoint {} did not exit abruptly: {}",
            failpoint.as_str(),
            String::from_utf8_lossy(&crashed.stderr)
        );

        let metadata = Database::open(&database).unwrap();
        let upload = metadata
            .upload_by_idempotency(&idempotency_key)
            .unwrap()
            .unwrap();
        let visible_before_retry = metadata.file_by_id(upload.file_id).unwrap().is_some();
        assert_eq!(
            visible_before_retry,
            failpoint == Failpoint::AfterCommit,
            "pre-commit visibility leaked at {}",
            failpoint.as_str()
        );
        let expected_file_id = upload.file_id;
        drop(metadata);

        let resumed = portal_command(&binary, "put", &database, &master_key, &agents)
            .arg("--idempotency-key")
            .arg(&idempotency_key)
            .arg(&source)
            .output()
            .unwrap();
        assert!(
            resumed.status.success(),
            "resume after {} failed: {}",
            failpoint.as_str(),
            String::from_utf8_lossy(&resumed.stderr)
        );
        let file_id = String::from_utf8(resumed.stdout).unwrap();
        let file_id = Uuid::parse_str(file_id.trim()).unwrap();
        assert_eq!(file_id, expected_file_id);

        let destination = case.join("download.bin");
        let downloaded = portal_command(&binary, "get", &database, &master_key, &agents)
            .arg(file_id.to_string())
            .arg(&destination)
            .output()
            .unwrap();
        assert!(
            downloaded.status.success(),
            "download after {} failed: {}",
            failpoint.as_str(),
            String::from_utf8_lossy(&downloaded.stderr)
        );
        assert_eq!(fs::read(destination).unwrap(), expected);
    }
}

#[test]
fn abrupt_delete_exit_at_every_boundary_recovers_with_the_same_key() {
    let binary = PathBuf::from(env!("CARGO_BIN_EXE_distr-hnsw"));
    let workspace = tempfile::tempdir().unwrap();
    let (_agent_a, target_a) = start_agent(
        &binary,
        "agent-a",
        "host-a",
        &workspace.path().join("delete-agent-a"),
    );
    let (_agent_b, target_b) = start_agent(
        &binary,
        "agent-b",
        "host-b",
        &workspace.path().join("delete-agent-b"),
    );
    let agents = vec![target_a, target_b];
    let source = workspace.path().join("empty.bin");
    fs::write(&source, []).unwrap();

    for failpoint in [
        Failpoint::AfterDeletePlan,
        Failpoint::AfterFirstMarkerReplica,
        Failpoint::AfterMarkerDurable,
        Failpoint::BeforeDeleteCommit,
        Failpoint::AfterDeleteCommit,
    ] {
        let case = workspace.path().join(failpoint.as_str());
        fs::create_dir_all(&case).unwrap();
        let database = case.join("portal.sqlite");
        let master_key = case.join("master.key");
        assert!(Command::new(&binary)
            .args(["portal", "init", "--no-recovery-bundle", "--database"])
            .arg(&database)
            .arg("--master-key")
            .arg(&master_key)
            .status()
            .unwrap()
            .success());
        let uploaded = portal_command(&binary, "put", &database, &master_key, &agents)
            .arg("--idempotency-key")
            .arg(format!("upload-{}", failpoint.as_str()))
            .arg(&source)
            .output()
            .unwrap();
        assert!(uploaded.status.success());
        let file_id = Uuid::parse_str(String::from_utf8(uploaded.stdout).unwrap().trim()).unwrap();
        let delete_key = format!("delete-{}", failpoint.as_str());

        let crashed = portal_command(&binary, "delete", &database, &master_key, &agents)
            .arg("--idempotency-key")
            .arg(&delete_key)
            .arg(file_id.to_string())
            .env("DISTR_HNSW_FAILPOINT", failpoint.as_str())
            .output()
            .unwrap();
        assert_eq!(crashed.status.code(), Some(86));

        let before = case.join("before-retry.bin");
        let get_before = portal_command(&binary, "get", &database, &master_key, &agents)
            .arg(file_id.to_string())
            .arg(&before)
            .output()
            .unwrap();
        assert_eq!(
            get_before.status.success(),
            failpoint != Failpoint::AfterDeleteCommit,
            "unexpected visibility at {}: {}",
            failpoint.as_str(),
            String::from_utf8_lossy(&get_before.stderr)
        );

        let resumed = portal_command(&binary, "delete", &database, &master_key, &agents)
            .arg("--idempotency-key")
            .arg(&delete_key)
            .arg(file_id.to_string())
            .output()
            .unwrap();
        assert!(resumed.status.success());
        let repeated = portal_command(&binary, "delete", &database, &master_key, &agents)
            .arg("--idempotency-key")
            .arg(&delete_key)
            .arg(file_id.to_string())
            .output()
            .unwrap();
        assert!(repeated.status.success());
        assert_eq!(resumed.stdout, repeated.stdout);

        let new_key = portal_command(&binary, "delete", &database, &master_key, &agents)
            .arg("--idempotency-key")
            .arg("different-key")
            .arg(file_id.to_string())
            .output()
            .unwrap();
        assert!(!new_key.status.success());
        assert!(String::from_utf8_lossy(&new_key.stderr).contains("already deleted"));
    }
}

#[test]
fn scrub_and_health_commands_report_a_durable_cluster() {
    let binary = env!("CARGO_BIN_EXE_distr-hnsw");
    let binary = Path::new(binary);
    let workspace = tempfile::tempdir().unwrap();
    let volume_a = workspace.path().join("agent-a");
    let volume_b = workspace.path().join("agent-b");
    fs::create_dir_all(&volume_a).unwrap();
    fs::create_dir_all(&volume_b).unwrap();
    let (_agent_a, target_a) = start_agent(binary, "agent-a", "host-a", &volume_a);
    let (_agent_b, target_b) = start_agent(binary, "agent-b", "host-b", &volume_b);
    let agents = vec![target_a, target_b];
    let database = workspace.path().join("portal.sqlite");
    let master_key = workspace.path().join("master.key");
    let status = Command::new(binary)
        .args(["portal", "init", "--no-recovery-bundle", "--database"])
        .arg(&database)
        .arg("--master-key")
        .arg(&master_key)
        .status()
        .unwrap();
    assert!(status.success());
    let source = workspace.path().join("source.bin");
    fs::write(&source, vec![7_u8; CHUNK_SIZE / 2]).unwrap();
    let put = portal_command(binary, "put", &database, &master_key, &agents)
        .args(["--idempotency-key", "cli-scrub"])
        .arg(&source)
        .output()
        .unwrap();
    assert!(
        put.status.success(),
        "{}",
        String::from_utf8_lossy(&put.stderr)
    );

    let scrub = Command::new(binary)
        .args(["portal", "scrub", "--database"])
        .arg(&database)
        .args(agents.iter().flat_map(|agent| ["--agent", agent.as_str()]))
        .output()
        .unwrap();
    assert!(
        scrub.status.success(),
        "{}",
        String::from_utf8_lossy(&scrub.stderr)
    );
    let report: serde_json::Value = serde_json::from_slice(&scrub.stdout).unwrap();
    assert_eq!(report["report_type"], "ScrubReportV1");
    assert_eq!(report["complete_observation"], true);
    assert_eq!(report["health"]["durable"], 2);
    assert_eq!(report["health"]["at_risk"], 0);

    let health = Command::new(binary)
        .args(["portal", "health", "--database"])
        .arg(&database)
        .output()
        .unwrap();
    assert!(
        health.status.success(),
        "{}",
        String::from_utf8_lossy(&health.stderr)
    );
    let report: serde_json::Value = serde_json::from_slice(&health.stdout).unwrap();
    assert_eq!(report["report_type"], "HealthReportV1");
    assert_eq!(report["latest_job"]["status"], "complete");
    assert_eq!(report["incarnations"].as_array().unwrap().len(), 2);

    // Corrupt one copy: verify-only scrub exits 2 and repair restores it.
    let database_handle = Database::open(&database).unwrap();
    let chunk = database_handle
        .required_objects()
        .unwrap()
        .into_iter()
        .find(|object| object.kind == distr_hnsw::object::ObjectKind::Chunk)
        .unwrap();
    drop(database_handle);
    let path = distr_hnsw::durability::DurableStore::open(&volume_a)
        .unwrap()
        .object_path(chunk.kind, &chunk.hash);
    fs::write(&path, b"flipped").unwrap();
    let scrub = Command::new(binary)
        .args(["portal", "scrub", "--database"])
        .arg(&database)
        .args(agents.iter().flat_map(|agent| ["--agent", agent.as_str()]))
        .output()
        .unwrap();
    assert_eq!(scrub.status.code(), Some(2));
    let repair = Command::new(binary)
        .args(["portal", "scrub", "--repair", "--database"])
        .arg(&database)
        .args(agents.iter().flat_map(|agent| ["--agent", agent.as_str()]))
        .output()
        .unwrap();
    assert!(
        repair.status.success(),
        "{}",
        String::from_utf8_lossy(&repair.stderr)
    );
    let report: serde_json::Value = serde_json::from_slice(&repair.stdout).unwrap();
    assert_eq!(report["totals"]["repairs_applied"], 1);
    assert_eq!(report["health"]["durable"], 2);
}

#[test]
fn recovery_bundle_ceremony_restores_the_key_and_refuses_the_wrong_one() {
    use std::io::Write as _;

    let binary = Path::new(env!("CARGO_BIN_EXE_distr-hnsw"));
    let workspace = tempfile::tempdir().unwrap();
    let volume_a = workspace.path().join("agent-a");
    let volume_b = workspace.path().join("agent-b");
    fs::create_dir_all(&volume_a).unwrap();
    fs::create_dir_all(&volume_b).unwrap();
    let (_agent_a, target_a) = start_agent(binary, "agent-a", "host-a", &volume_a);
    let (_agent_b, target_b) = start_agent(binary, "agent-b", "host-b", &volume_b);
    let agents = vec![target_a, target_b];
    let database = workspace.path().join("portal.sqlite");
    let master_key = workspace.path().join("master.key");
    let floor = [
        "--kdf-memory-kib",
        "19456",
        "--kdf-time",
        "2",
        "--kdf-parallelism",
        "1",
    ];
    let passphrase = "correct horse battery staple\n";

    // Init with an operator-supplied passphrase emits an armored bundle.
    let mut init = Command::new(binary)
        .args(["portal", "init", "--passphrase-stdin", "--database"])
        .arg(&database)
        .arg("--master-key")
        .arg(&master_key)
        .args(floor)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    init.stdin
        .take()
        .unwrap()
        .write_all(passphrase.as_bytes())
        .unwrap();
    let init = init.wait_with_output().unwrap();
    assert!(init.status.success());
    let stdout = String::from_utf8(init.stdout).unwrap();
    let begin = stdout
        .find("-----BEGIN DISTR-HNSW RECOVERY BUNDLE-----")
        .unwrap();
    let end_marker = "-----END DISTR-HNSW RECOVERY BUNDLE-----";
    let end = stdout.find(end_marker).unwrap() + end_marker.len();
    let bundle_path = workspace.path().join("bundle.txt");
    fs::write(&bundle_path, &stdout[begin..end]).unwrap();
    assert!(
        !stdout.contains("recovery passphrase:"),
        "supplied passphrase is never echoed"
    );

    let source = workspace.path().join("source.bin");
    fs::write(&source, vec![3_u8; 4096]).unwrap();
    let put = portal_command(binary, "put", &database, &master_key, &agents)
        .args(["--idempotency-key", "bundle-drill"])
        .arg(&source)
        .output()
        .unwrap();
    assert!(
        put.status.success(),
        "{}",
        String::from_utf8_lossy(&put.stderr)
    );
    let file_id = String::from_utf8(put.stdout).unwrap().trim().to_owned();

    // A different key is refused before any object is read.
    let other_key = workspace.path().join("other.key");
    distr_hnsw::crypto::MasterKey::create(&other_key).unwrap();
    let denied = portal_command(binary, "get", &database, &other_key, &agents)
        .arg(&file_id)
        .arg(workspace.path().join("denied.bin"))
        .output()
        .unwrap();
    assert!(!denied.status.success());
    assert!(String::from_utf8_lossy(&denied.stderr).contains("does not match the key"));

    // Lose the key file; the wrong passphrase fails, the right one verifies.
    fs::remove_file(&master_key).unwrap();
    let restore = |verify: bool, secret: &str, target: &Path| {
        let mut command = Command::new(binary);
        command
            .args(["portal", "key", "restore", "--bundle"])
            .arg(&bundle_path)
            .arg("--master-key")
            .arg(target)
            .arg("--database")
            .arg(&database);
        if verify {
            command.arg("--verify");
        }
        let mut child = command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(secret.as_bytes())
            .unwrap();
        child.wait_with_output().unwrap()
    };
    let wrong = restore(true, "incorrect horse battery staple\n", &master_key);
    assert!(!wrong.status.success());
    assert!(String::from_utf8_lossy(&wrong.stderr).contains("wrong passphrase"));
    assert!(!master_key.exists());
    let verified = restore(true, passphrase, &master_key);
    assert!(
        verified.status.success(),
        "{}",
        String::from_utf8_lossy(&verified.stderr)
    );
    assert!(!master_key.exists(), "verify never writes");
    let restored = restore(false, passphrase, &master_key);
    assert!(
        restored.status.success(),
        "{}",
        String::from_utf8_lossy(&restored.stderr)
    );
    assert!(master_key.exists());
    let again = restore(false, passphrase, &master_key);
    assert!(
        !again.status.success(),
        "restore never overwrites an existing key"
    );

    let destination = workspace.path().join("restored.bin");
    let get = portal_command(binary, "get", &database, &master_key, &agents)
        .arg(&file_id)
        .arg(&destination)
        .output()
        .unwrap();
    assert!(
        get.status.success(),
        "{}",
        String::from_utf8_lossy(&get.stderr)
    );
    assert_eq!(fs::read(destination).unwrap(), vec![3_u8; 4096]);

    // A bundle for an unrelated key is refused against this database by id,
    // before the passphrase is even read.
    let other_bundle = workspace.path().join("other-bundle.txt");
    let mut export = Command::new(binary)
        .args([
            "portal",
            "key",
            "export-recovery",
            "--passphrase-stdin",
            "--master-key",
        ])
        .arg(&other_key)
        .arg("--out")
        .arg(&other_bundle)
        .args(floor)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .spawn()
        .unwrap();
    export
        .stdin
        .take()
        .unwrap()
        .write_all(passphrase.as_bytes())
        .unwrap();
    assert!(export.wait().unwrap().success());
    let mismatch = Command::new(binary)
        .args(["portal", "key", "restore", "--verify", "--bundle"])
        .arg(&other_bundle)
        .arg("--master-key")
        .arg(workspace.path().join("unused.key"))
        .arg("--database")
        .arg(&database)
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert!(!mismatch.status.success());
    assert!(String::from_utf8_lossy(&mismatch.stderr).contains("is bound to key"));

    let show = Command::new(binary)
        .args(["portal", "key", "show-id", "--master-key"])
        .arg(&master_key)
        .output()
        .unwrap();
    assert!(show.status.success());
    let shown = String::from_utf8(show.stdout).unwrap().trim().to_owned();
    assert_eq!(shown.len(), 32);
    assert!(String::from_utf8_lossy(&verified.stdout).contains(&shown));
}
