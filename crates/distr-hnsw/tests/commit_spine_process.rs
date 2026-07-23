#![cfg(unix)]

use std::{
    fs,
    net::{SocketAddr, TcpListener, TcpStream},
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    thread,
    time::{Duration, Instant},
};

use distr_hnsw::{portal::Failpoint, CHUNK_SIZE};
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
            .args(["portal", "init", "--database"])
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
            .args(["portal", "init", "--database"])
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
