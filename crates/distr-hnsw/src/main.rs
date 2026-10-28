use std::{
    io::{self, Read, Write},
    path::{Path, PathBuf},
    str::FromStr,
};

use anyhow::Context;
use clap::{Args, Parser, Subcommand};
use distr_hnsw::{
    access,
    agent::{
        bind_and_serve_agent, AgentBind, AgentIdentity, CapacityConfig, JoinToken,
        TailnetAddressChanged,
    },
    agent_client::{join_agent, AgentClient},
    backup::{self, BackupTargetSpec},
    crypto::MasterKey,
    durability::StoreFailpoint,
    lifecycle::{self, GcOptions},
    metadata::{Database, JobMode, KeyBinding},
    object::{ObjectHash, ObjectKind},
    portal::{prepare_agents, AgentTarget, Failpoint, FailpointAction, Portal},
    reconcile::{health_report, scrub},
    recovery_bundle::{self, KdfParams},
    tailscale::{self, BindSpec, LocalApi},
};
use uuid::Uuid;
use zeroize::Zeroizing;

#[derive(Parser)]
#[command(name = "distr-hnsw", version, about)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Run an opaque-object agent on this node's tailnet address.
    Agent {
        #[arg(long)]
        id: String,
        #[arg(long)]
        failure_domain: String,
        /// `tailnet:<port>` (this node's tailnet IPv4 address), or an explicit
        /// address that is one of this node's tailnet addresses. Wildcards
        /// are refused; loopback needs --insecure-loopback.
        #[arg(long)]
        bind: BindSpec,
        /// Allow a loopback --bind for tests and single-host development.
        /// Health reports the mode; peers have no tailnet identity.
        #[arg(long)]
        insecure_loopback: bool,
        /// Seconds to wait for tailscaled at startup.
        #[arg(long, default_value_t = 120)]
        tailnet_wait_seconds: u64,
        /// Milliseconds between checks that this node still owns the bound
        /// tailnet address; on a change the agent exits with status 75.
        #[arg(long, default_value_t = 5000, hide = true)]
        tailnet_poll_ms: u64,
        #[arg(long)]
        volume: PathBuf,
        /// File holding the single-use join token from `portal agent token`
        /// (or set DISTR_HNSW_JOIN_TOKEN). Needed only until the agent has
        /// joined; a joined agent ignores it.
        #[arg(long)]
        join_token_file: Option<PathBuf>,
        /// Forget the portal this volume trusts and wait for a new join with
        /// the given token. For a portal rebuilt without its signing keys
        /// (a lost database with no snapshot); needs a join token.
        #[arg(long)]
        reset_trust: bool,
        /// Maximum bytes this volume may hold (default: filesystem size).
        #[arg(long)]
        quota_bytes: Option<u64>,
        /// Headroom regular chunks may not consume (default max(10%, 1 GiB)).
        #[arg(long)]
        reserve_bytes: Option<u64>,
        /// Headroom even control objects and repair may not consume
        /// (default max(1%, 256 MiB)).
        #[arg(long)]
        hard_floor_bytes: Option<u64>,
    },
    /// Run portal metadata and file operations.
    Portal {
        #[command(subcommand)]
        command: PortalCommand,
    },
    /// Archive and restore vector-partition state through the blob plane.
    Index {
        #[command(subcommand)]
        command: IndexCommand,
    },
}

#[derive(Subcommand)]
enum IndexCommand {
    /// Commit a partition's manifest, newest snapshot, and WAL segments as
    /// blob-plane files; `--truncate-wal` removes covered segments after
    /// their archive copy is committed.
    Archive {
        #[arg(long)]
        database: PathBuf,
        #[arg(long)]
        master_key: PathBuf,
        #[arg(long = "agent", required = true)]
        agents: Vec<AgentTarget>,
        #[arg(long)]
        partition_dir: PathBuf,
        #[arg(long)]
        truncate_wal: bool,
    },
    /// Download every archived part of a partition into an empty directory
    /// and recover it.
    Restore {
        #[arg(long)]
        database: PathBuf,
        #[arg(long)]
        master_key: PathBuf,
        #[arg(long = "agent", required = true)]
        agents: Vec<AgentTarget>,
        #[arg(long)]
        partition_id: Uuid,
        #[arg(long)]
        destination: PathBuf,
    },
}

#[derive(Subcommand)]
enum PortalCommand {
    /// Initialize the SQLite database and file-backed master key, bind the
    /// key identifier, and emit the recovery bundle once.
    Init {
        #[arg(long)]
        database: PathBuf,
        #[arg(long)]
        master_key: PathBuf,
        /// Skip the recovery bundle (development only; export one later with
        /// `key export-recovery`).
        #[arg(long)]
        no_recovery_bundle: bool,
        #[command(flatten)]
        passphrase: PassphraseArgs,
        #[command(flatten)]
        kdf: KdfArgs,
    },
    /// Master-key custody: recovery bundle export, verification, and restore.
    Key {
        #[command(subcommand)]
        command: KeyCommand,
    },
    /// Commit a seekable regular file to RF2.
    Put {
        #[arg(long)]
        database: PathBuf,
        #[arg(long)]
        master_key: PathBuf,
        #[arg(long = "agent", required = true)]
        agents: Vec<AgentTarget>,
        #[arg(long)]
        idempotency_key: String,
        source: PathBuf,
    },
    /// Download a committed regular file.
    Get {
        #[arg(long)]
        database: PathBuf,
        #[arg(long)]
        master_key: PathBuf,
        #[arg(long = "agent", required = true)]
        agents: Vec<AgentTarget>,
        file_id: Uuid,
        destination: PathBuf,
    },
    /// Durably mark a committed file as logically deleted.
    Delete {
        #[arg(long)]
        database: PathBuf,
        #[arg(long)]
        master_key: PathBuf,
        #[arg(long = "agent", required = true)]
        agents: Vec<AgentTarget>,
        #[arg(long)]
        idempotency_key: String,
        file_id: Uuid,
    },
    /// Plan recovery from agent inventories, optionally repairing and applying it.
    Recover {
        #[arg(long)]
        database: PathBuf,
        #[arg(long)]
        master_key: PathBuf,
        #[arg(long = "agent", required = true)]
        agents: Vec<AgentTarget>,
        #[arg(long)]
        apply: bool,
    },
    /// Verify every required object on every agent and report durability
    /// health. The master key unseals the key that signs agent grants.
    /// `--repair` restores copies copy-first; nothing is ever deleted.
    Scrub {
        #[arg(long)]
        database: PathBuf,
        #[arg(long)]
        master_key: PathBuf,
        #[arg(long = "agent", required = true)]
        agents: Vec<AgentTarget>,
        #[arg(long)]
        repair: bool,
        /// Repeat continuously, sleeping this many seconds between jobs.
        #[arg(long)]
        interval: Option<u64>,
    },
    /// Print persisted lifecycle and backup health without contacting agents.
    Health {
        #[arg(long)]
        database: PathBuf,
    },
    /// Copy every historical object to a backup target, verify each copy,
    /// ship a SQLite snapshot, and write a catalog. The master key unseals
    /// the key that signs agent grants; objects stay ciphertext.
    Backup {
        #[arg(long)]
        database: PathBuf,
        #[arg(long)]
        master_key: PathBuf,
        #[arg(long = "agent", required = true)]
        agents: Vec<AgentTarget>,
        /// Backup target: `dir:/mnt/backup` or `s3:<bucket>[/<prefix>]` (S3
        /// endpoint and credentials come from the environment).
        #[arg(long)]
        target: BackupTargetSpec,
        /// Repeat continuously, sleeping this many seconds between jobs.
        #[arg(long)]
        interval: Option<u64>,
        /// Re-download and hash this many previously verified objects each
        /// run (integrity sampling).
        #[arg(long, default_value_t = 0)]
        verify_sample: usize,
    },
    /// Join agents: issue single-use tokens and send joined agents the
    /// portal's key set.
    Agent {
        #[command(subcommand)]
        command: AgentCommand,
    },
    /// Rebuild a portal from a backup set.
    Restore {
        #[command(subcommand)]
        command: RestoreCommand,
    },
    /// Record restore-drill evidence that `recovery_ready` is derived from.
    Drill {
        #[command(subcommand)]
        command: DrillCommand,
    },
    /// Move an agent's copies elsewhere copy-first, then orphan its placements.
    Drain {
        #[arg(long)]
        database: PathBuf,
        #[arg(long)]
        master_key: PathBuf,
        #[arg(long = "agent", required = true)]
        agents: Vec<AgentTarget>,
        #[arg(long)]
        agent_id: String,
        #[arg(long)]
        dry_run: bool,
    },
    /// Retire an agent's incarnation once every object meets RF2 without it.
    Retire {
        #[arg(long)]
        database: PathBuf,
        #[arg(long)]
        master_key: PathBuf,
        #[arg(long = "agent", required = true)]
        agents: Vec<AgentTarget>,
        #[arg(long)]
        agent_id: String,
    },
    /// Plan garbage collection with per-object proofs; `--apply` deletes
    /// only objects whose proof re-derives unchanged.
    Gc {
        #[arg(long)]
        database: PathBuf,
        #[arg(long)]
        master_key: PathBuf,
        #[arg(long = "agent", required = true)]
        agents: Vec<AgentTarget>,
        #[arg(long)]
        apply: bool,
        /// Seconds a deleted generation is retained live (default 30 days).
        #[arg(long, default_value_t = 30 * 24 * 3600)]
        retention_seconds: i64,
        /// Seconds an uncommitted upload is kept before its objects are
        /// collectible (default 7 days).
        #[arg(long, default_value_t = 7 * 24 * 3600)]
        staging_grace_seconds: i64,
    },
}

#[derive(Subcommand)]
enum DrillCommand {
    /// Store a `RestoreDrillReportV1` (from scripts/restore-drill.sh) for
    /// this deployment. Only a passing drill within the quarterly window that
    /// ran against a configured backup target makes the deployment
    /// recovery ready.
    Record {
        #[arg(long)]
        database: PathBuf,
        #[arg(long)]
        report: PathBuf,
    },
}

#[derive(Subcommand)]
enum RestoreCommand {
    /// Restore the latest (or named) SQLite snapshot into a new database file.
    Metadata {
        #[arg(long)]
        target: BackupTargetSpec,
        #[arg(long)]
        database: PathBuf,
        #[arg(long)]
        snapshot: Option<String>,
    },
    /// Copy every backed-up object into the agents at RF2, read-back verified.
    /// Join the new agents first; run `portal recover --apply` afterwards to
    /// rebuild file records.
    Objects {
        #[arg(long)]
        target: BackupTargetSpec,
        #[arg(long)]
        database: PathBuf,
        #[arg(long)]
        master_key: PathBuf,
        #[arg(long = "agent", required = true)]
        agents: Vec<AgentTarget>,
    },
}

#[derive(Clone, Copy, clap::ValueEnum)]
enum AgentRequest {
    Health,
    Inventory,
    Get,
    Put,
}

#[derive(Subcommand)]
enum AgentCommand {
    /// Print a single-use join token for an agent id (valid 24 hours). Start
    /// the agent with it (DISTR_HNSW_JOIN_TOKEN or --join-token-file), then
    /// run `portal agent join`.
    Token {
        #[arg(long)]
        database: PathBuf,
        #[arg(long)]
        master_key: PathBuf,
        #[arg(long)]
        agent_id: String,
    },
    /// Send one granted request to an agent and print the HTTP status
    /// (for drills and diagnosis). The body goes to `--out`. There is no
    /// delete: deletes happen only through `portal gc --apply`.
    Request {
        #[arg(long)]
        database: PathBuf,
        #[arg(long)]
        master_key: PathBuf,
        #[arg(long)]
        agent: AgentTarget,
        #[arg(value_enum)]
        operation: AgentRequest,
        #[arg(long)]
        kind: Option<ObjectKind>,
        #[arg(long)]
        hash: Option<ObjectHash>,
        /// Inventory cursor.
        #[arg(long)]
        after: Option<ObjectHash>,
        /// Inventory page size.
        #[arg(long, default_value_t = 1000)]
        limit: usize,
        /// Body for `put`.
        #[arg(long)]
        body_file: Option<PathBuf>,
        /// Write the response body here.
        #[arg(long)]
        out: Option<PathBuf>,
    },
    /// Join an agent started with a token: send it the portal's signed key
    /// set, record its tailnet node, and burn the token.
    Join {
        #[arg(long)]
        database: PathBuf,
        #[arg(long)]
        master_key: PathBuf,
        #[arg(long)]
        agent: AgentTarget,
        /// The token from `portal agent token`.
        #[arg(long, env = "DISTR_HNSW_JOIN_TOKEN", hide_env_values = true)]
        token: String,
    },
}

#[derive(Subcommand)]
enum KeyCommand {
    /// Print the grant-signing keys, the grant epoch, and the key-set
    /// version.
    ShowSigning {
        #[arg(long)]
        database: PathBuf,
        #[arg(long)]
        master_key: PathBuf,
    },
    /// Make a new grant-signing key active. The old key keeps verifying as
    /// `retiring`; agents receive the new key set on their next request.
    RotateSigning {
        #[arg(long)]
        database: PathBuf,
        #[arg(long)]
        master_key: PathBuf,
    },
    /// Drop a retiring signing key from the key set once agents use the
    /// active key. `--revoke` also raises the grant epoch, invalidating every
    /// outstanding grant.
    RetireSigning {
        #[arg(long)]
        database: PathBuf,
        #[arg(long)]
        master_key: PathBuf,
        #[arg(long)]
        key_id: String,
        #[arg(long)]
        revoke: bool,
    },
    /// Print the non-secret identifier of a master key.
    ShowId {
        #[arg(long)]
        master_key: PathBuf,
    },
    /// Wrap the master key in a passphrase-protected recovery bundle.
    ExportRecovery {
        #[arg(long)]
        master_key: PathBuf,
        /// Write the armored bundle here instead of stdout.
        #[arg(long)]
        out: Option<PathBuf>,
        #[command(flatten)]
        passphrase: PassphraseArgs,
        #[command(flatten)]
        kdf: KdfArgs,
    },
    /// Recover a master key from a bundle. Reads the passphrase from stdin.
    /// With `--database`, refuses a bundle whose key does not match the
    /// database. With `--verify`, decrypts and compares without writing.
    Restore {
        #[arg(long)]
        bundle: PathBuf,
        #[arg(long)]
        master_key: PathBuf,
        #[arg(long)]
        database: Option<PathBuf>,
        #[arg(long)]
        verify: bool,
    },
}

#[derive(Args)]
struct PassphraseArgs {
    /// Read the passphrase from stdin instead of generating one.
    #[arg(long)]
    passphrase_stdin: bool,
}

#[derive(Args)]
struct KdfArgs {
    /// Argon2id memory cost in KiB (default 65536; floor 19456).
    #[arg(long)]
    kdf_memory_kib: Option<u32>,
    /// Argon2id iterations (default 3; floor 2).
    #[arg(long)]
    kdf_time: Option<u32>,
    /// Argon2id parallelism (default 4; floor 1).
    #[arg(long)]
    kdf_parallelism: Option<u32>,
}

impl KdfArgs {
    fn params(&self) -> KdfParams {
        KdfParams {
            m_cost_kib: self.kdf_memory_kib.unwrap_or(KdfParams::DEFAULT.m_cost_kib),
            t_cost: self.kdf_time.unwrap_or(KdfParams::DEFAULT.t_cost),
            p_cost: self.kdf_parallelism.unwrap_or(KdfParams::DEFAULT.p_cost),
        }
    }
}

fn read_passphrase_stdin() -> anyhow::Result<Zeroizing<String>> {
    let mut input = Zeroizing::new(String::new());
    io::stdin()
        .read_to_string(&mut input)
        .context("reading passphrase from stdin")?;
    let trimmed = input.trim_end_matches(['\r', '\n']).to_owned();
    Ok(Zeroizing::new(trimmed))
}

/// Process-level crash injection for the commit-spine tests. Only debug
/// builds and builds with the `failpoints` feature honor
/// `DISTR_HNSW_FAILPOINT`; a release build refuses to run with it set rather
/// than silently ignoring a test's request.
fn with_env_failpoint(portal: Portal) -> anyhow::Result<Portal> {
    let Ok(value) = std::env::var("DISTR_HNSW_FAILPOINT") else {
        return Ok(portal);
    };
    if cfg!(any(debug_assertions, feature = "failpoints")) {
        Ok(portal.with_failpoint(Failpoint::from_str(&value)?, FailpointAction::ExitProcess))
    } else {
        anyhow::bail!(
            "DISTR_HNSW_FAILPOINT is set, but this build has no failpoints \
             (build with --features failpoints)"
        )
    }
}

/// The agent's counterpart to [`with_env_failpoint`], read from
/// `DISTR_HNSW_AGENT_FAILPOINT` so a variable meant for a portal command
/// never reaches an agent, with the same refusal in builds without
/// failpoints.
fn now_seconds() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_secs() as i64)
        .unwrap_or(0)
}

/// Load the master key and check it against the database's bound key, so a
/// wrong key fails before it unseals or seals a signing key.
fn bound_master_key(database: &mut Database, path: &Path) -> anyhow::Result<MasterKey> {
    let key = MasterKey::load(path)?;
    database.bind_master_key_id(&key.key_id_hex())?;
    Ok(key)
}

/// The grant-signing client for a command that contacts agents.
fn agent_client(database: &mut Database, master_key: &Path) -> anyhow::Result<AgentClient> {
    let key = bound_master_key(database, master_key)?;
    Ok(AgentClient::open(database, &key)?)
}

/// The agent's join token: `--join-token-file`, else DISTR_HNSW_JOIN_TOKEN.
fn read_join_token(file: Option<&Path>) -> anyhow::Result<Option<String>> {
    if let Some(file) = file {
        let token = std::fs::read_to_string(file)
            .with_context(|| format!("reading join token {}", file.display()))?;
        return Ok(Some(token.trim().to_owned()));
    }
    Ok(std::env::var("DISTR_HNSW_JOIN_TOKEN")
        .ok()
        .map(|token| token.trim().to_owned())
        .filter(|token| !token.is_empty()))
}

fn env_agent_failpoint() -> anyhow::Result<Option<StoreFailpoint>> {
    let Ok(value) = std::env::var("DISTR_HNSW_AGENT_FAILPOINT") else {
        return Ok(None);
    };
    if cfg!(any(debug_assertions, feature = "failpoints")) {
        Ok(Some(value.parse().map_err(anyhow::Error::msg)?))
    } else {
        anyhow::bail!(
            "DISTR_HNSW_AGENT_FAILPOINT is set, but this build has no failpoints \
             (build with --features failpoints)"
        )
    }
}

/// Emit the recovery bundle for `key`, verifying that it round-trips before
/// anything is printed. Returns the armored text.
fn emit_recovery_bundle(
    key: &MasterKey,
    passphrase: &PassphraseArgs,
    kdf: &KdfArgs,
    out: Option<&Path>,
) -> anyhow::Result<String> {
    let (secret, generated) = if passphrase.passphrase_stdin {
        (read_passphrase_stdin()?, false)
    } else {
        (recovery_bundle::generate_passphrase(), true)
    };
    let armored = recovery_bundle::seal(key, secret.as_bytes(), kdf.params())?;
    let reopened = recovery_bundle::open(&armored, secret.as_bytes())?;
    anyhow::ensure!(
        reopened.key.bytes() == key.bytes(),
        "recovery bundle did not round-trip; refusing to continue"
    );
    match out {
        Some(path) => {
            // Never replace an existing bundle: it may be the only copy that
            // opens the key it wraps.
            let mut file = std::fs::OpenOptions::new()
                .create_new(true)
                .write(true)
                .open(path)
                .with_context(|| format!("creating recovery bundle {}", path.display()))?;
            std::io::Write::write_all(&mut file, armored.as_bytes())
                .and_then(|()| file.sync_all())
                .with_context(|| format!("writing recovery bundle {}", path.display()))?;
            println!("recovery bundle written to {}", path.display());
        }
        None => {
            print!("{armored}");
        }
    }
    if generated {
        println!("recovery passphrase: {}", secret.as_str());
    }
    println!(
        "Store the bundle and passphrase separately, off this cluster. Losing both the \
         passphrase and every cluster disk loses the data. Verify at any time with:\n  \
         distr-hnsw portal key restore --verify --bundle <file> --master-key <unused-path> \
         --database <db>  (passphrase on stdin)"
    );
    io::stdout().flush()?;
    Ok(armored)
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Command::Agent {
            id,
            failure_domain,
            bind,
            insecure_loopback,
            tailnet_wait_seconds,
            tailnet_poll_ms,
            volume,
            join_token_file,
            reset_trust,
            quota_bytes,
            reserve_bytes,
            hard_floor_bytes,
        } => {
            let served = bind_and_serve_agent(
                AgentBind {
                    spec: bind,
                    insecure_loopback,
                    api: LocalApi::from_env(),
                    tailnet_wait: std::time::Duration::from_secs(tailnet_wait_seconds),
                    address_poll: std::time::Duration::from_millis(tailnet_poll_ms),
                },
                volume,
                AgentIdentity { id, failure_domain },
                CapacityConfig {
                    quota_bytes,
                    reserve_bytes,
                    hard_floor_bytes,
                },
                env_agent_failpoint()?,
                match (read_join_token(join_token_file.as_deref())?, reset_trust) {
                    (Some(token), reset_trust) => Some(JoinToken { token, reset_trust }),
                    (None, true) => anyhow::bail!("--reset-trust needs a join token"),
                    (None, false) => None,
                },
            )
            .await;
            if let Err(error) = &served {
                if error.downcast_ref::<TailnetAddressChanged>().is_some() {
                    eprintln!("error: {error}");
                    std::process::exit(tailscale::EXIT_TAILNET_ADDRESS_CHANGED);
                }
            }
            served
        }
        Command::Index { command } => match command {
            IndexCommand::Archive {
                database,
                master_key,
                agents,
                partition_dir,
                truncate_wal,
            } => {
                let key = MasterKey::load(&master_key)?;
                let mut portal = Portal::open(&database, key, agents)?;
                let report =
                    distr_hnsw::index_archive::archive(&mut portal, &partition_dir, truncate_wal)
                        .await?;
                println!("{}", serde_json::to_string_pretty(&report)?);
                Ok(())
            }
            IndexCommand::Restore {
                database,
                master_key,
                agents,
                partition_id,
                destination,
            } => {
                let key = MasterKey::load(&master_key)?;
                let portal = Portal::open(&database, key, agents)?;
                let report =
                    distr_hnsw::index_archive::restore(&portal, partition_id, &destination).await?;
                println!("{}", serde_json::to_string_pretty(&report)?);
                if report.snapshots_rejected > 0 {
                    std::process::exit(2);
                }
                Ok(())
            }
        },
        Command::Portal { command } => match command {
            PortalCommand::Init {
                database,
                master_key,
                no_recovery_bundle,
                passphrase,
                kdf,
            } => {
                if database.exists() && !master_key.exists() {
                    anyhow::bail!(
                        "database exists but master key is missing; refusing to generate an unrelated key"
                    );
                }
                let (key, created) = if master_key.exists() {
                    let key = MasterKey::load(&master_key).with_context(|| {
                        format!("validating existing master key {}", master_key.display())
                    })?;
                    (key, false)
                } else {
                    let key = MasterKey::create(&master_key)
                        .with_context(|| format!("creating master key {}", master_key.display()))?;
                    (key, true)
                };
                let mut db = Database::open(&database)
                    .with_context(|| format!("initializing database {}", database.display()))?;
                let binding = db.bind_master_key_id(&key.key_id_hex())?;
                // Create the grant-signing key now, so every metadata
                // snapshot (and every restore from one) carries it.
                access::GrantMaterial::load_or_create(&mut db, &key)?;
                println!("initialized");
                println!(
                    "master key id: {} ({})",
                    key.key_id_hex(),
                    match binding {
                        KeyBinding::Bound => "bound to database",
                        KeyBinding::Matched => "matches database",
                    }
                );
                // The bundle is owed from key creation (or first binding) until
                // it is emitted, so an init that failed in between still emits
                // it on the next run instead of silently skipping it.
                if created || binding == KeyBinding::Bound {
                    db.set_recovery_bundle_owed(true)?;
                }
                if db.recovery_bundle_owed()? {
                    if no_recovery_bundle {
                        println!(
                            "no recovery bundle emitted; run `portal key export-recovery` before this \
                             key protects any data you cannot lose"
                        );
                    } else {
                        emit_recovery_bundle(&key, &passphrase, &kdf, None)?;
                    }
                    db.set_recovery_bundle_owed(false)?;
                }
                Ok(())
            }
            PortalCommand::Key { command } => match command {
                KeyCommand::ShowSigning {
                    database,
                    master_key,
                } => {
                    let mut database = Database::open(&database)?;
                    let key = bound_master_key(&mut database, &master_key)?;
                    let material = access::GrantMaterial::load_or_create(&mut database, &key)?;
                    let set = material.signed_key_set().key_set;
                    println!(
                        "{}",
                        serde_json::to_string_pretty(&serde_json::json!({
                            "epoch": set.epoch,
                            "key_set_version": set.version,
                            "keys": set.keys.iter().map(|k| serde_json::json!({
                                "key_id": k.key_id.to_string(),
                                "status": format!("{:?}", k.status).to_lowercase(),
                            })).collect::<Vec<_>>(),
                        }))?
                    );
                    Ok(())
                }
                KeyCommand::RotateSigning {
                    database,
                    master_key,
                } => {
                    let mut database = Database::open(&database)?;
                    let key = bound_master_key(&mut database, &master_key)?;
                    let key_id = access::rotate_signing_key(&mut database, &key)?;
                    println!("{key_id}");
                    Ok(())
                }
                KeyCommand::RetireSigning {
                    database,
                    master_key,
                    key_id,
                    revoke,
                } => {
                    let mut database = Database::open(&database)?;
                    bound_master_key(&mut database, &master_key)?;
                    access::end_signing_key(&mut database, &key_id, revoke)?;
                    Ok(())
                }
                KeyCommand::ShowId { master_key } => {
                    let key = MasterKey::load(&master_key)?;
                    println!("{}", key.key_id_hex());
                    Ok(())
                }
                KeyCommand::ExportRecovery {
                    master_key,
                    out,
                    passphrase,
                    kdf,
                } => {
                    let key = MasterKey::load(&master_key)?;
                    emit_recovery_bundle(&key, &passphrase, &kdf, out.as_deref())?;
                    Ok(())
                }
                KeyCommand::Restore {
                    bundle,
                    master_key,
                    database,
                    verify,
                } => {
                    let armored = std::fs::read_to_string(&bundle)
                        .with_context(|| format!("reading recovery bundle {}", bundle.display()))?;
                    let header = recovery_bundle::inspect(&armored)?;
                    if let Some(database) = &database {
                        let db = Database::open(database)?;
                        match db.master_key_id()? {
                            Some(bound) if bound != header.key_id_hex() => anyhow::bail!(
                                "recovery bundle wraps key {} but database {} is bound to key {bound}",
                                header.key_id_hex(),
                                database.display()
                            ),
                            Some(_) => {}
                            None => anyhow::bail!(
                                "database {} has no bound master key; refusing to guess",
                                database.display()
                            ),
                        }
                    }
                    let passphrase = read_passphrase_stdin()?;
                    let opened = recovery_bundle::open(&armored, passphrase.as_bytes())?;
                    if verify {
                        println!("verified: bundle recovers key {}", opened.key.key_id_hex());
                        return Ok(());
                    }
                    opened
                        .key
                        .write_new(&master_key)
                        .with_context(|| format!("writing master key {}", master_key.display()))?;
                    println!(
                        "restored key {} to {}",
                        opened.key.key_id_hex(),
                        master_key.display()
                    );
                    Ok(())
                }
            },
            PortalCommand::Put {
                database,
                master_key,
                agents,
                idempotency_key,
                source,
            } => {
                let key = MasterKey::load(&master_key)?;
                let mut portal = with_env_failpoint(Portal::open(&database, key, agents)?)?;
                match portal.upload(&source, &idempotency_key).await {
                    Ok(file_id) => {
                        println!("{file_id}");
                        Ok(())
                    }
                    Err(error @ distr_hnsw::portal::PortalError::InsufficientCapacity { .. }) => {
                        eprintln!("error: {error}");
                        std::process::exit(3);
                    }
                    Err(error) => Err(error.into()),
                }
            }
            PortalCommand::Get {
                database,
                master_key,
                agents,
                file_id,
                destination,
            } => {
                let key = MasterKey::load(&master_key)?;
                let portal = Portal::open(&database, key, agents)?;
                portal.download(file_id, &destination).await?;
                println!("{}", destination.display());
                Ok(())
            }
            PortalCommand::Delete {
                database,
                master_key,
                agents,
                idempotency_key,
                file_id,
            } => {
                let key = MasterKey::load(&master_key)?;
                let mut portal = with_env_failpoint(Portal::open(&database, key, agents)?)?;
                let operation = portal.delete(file_id, &idempotency_key).await?;
                println!("{}", operation.marker_hash);
                Ok(())
            }
            PortalCommand::Recover {
                database,
                master_key,
                agents,
                apply,
            } => {
                let key = MasterKey::load(&master_key)?;
                let mut portal = Portal::open(&database, key, agents)?;
                let report = portal.recover(apply).await?;
                println!("{}", serde_json::to_string_pretty(&report)?);
                if report.exit_code() == 2 {
                    std::process::exit(2);
                }
                Ok(())
            }
            PortalCommand::Scrub {
                database,
                master_key,
                agents,
                repair,
                interval,
            } => {
                let agents = prepare_agents(agents)?;
                let mut database = Database::open(&database)?;
                let client = agent_client(&mut database, &master_key)?;
                let mode = if repair {
                    JobMode::Repair
                } else {
                    JobMode::Verify
                };
                loop {
                    let report = scrub(&mut database, &agents, &client, mode).await?;
                    println!("{}", serde_json::to_string_pretty(&report)?);
                    match interval {
                        Some(seconds) => {
                            tokio::time::sleep(std::time::Duration::from_secs(seconds)).await;
                        }
                        None => {
                            if report.exit_code() == 2 {
                                std::process::exit(2);
                            }
                            return Ok(());
                        }
                    }
                }
            }
            PortalCommand::Drill {
                command: DrillCommand::Record { database, report },
            } => {
                let json = std::fs::read_to_string(&report)
                    .with_context(|| format!("reading drill report {}", report.display()))?;
                let mut database = Database::open(&database)?;
                let record = distr_hnsw::backup::record_restore_drill(&mut database, &json)?;
                let status = distr_hnsw::backup::backup_status(&database)?;
                println!(
                    "{}",
                    serde_json::to_string_pretty(&serde_json::json!({
                        "recorded": record,
                        "recovery_ready": status.recovery_ready,
                        "recovery_ready_reason": status.recovery_ready_reason,
                    }))?
                );
                Ok(())
            }
            PortalCommand::Health { database } => {
                let database = Database::open(&database)?;
                let report = health_report(&database)?;
                println!("{}", serde_json::to_string_pretty(&report)?);
                if report.exit_code() == 2 {
                    std::process::exit(2);
                }
                Ok(())
            }
            PortalCommand::Backup {
                database: database_path,
                master_key,
                agents,
                target,
                interval,
                verify_sample,
            } => {
                let agents = prepare_agents(agents)?;
                let mut database = Database::open(&database_path)?;
                let client = agent_client(&mut database, &master_key)?;
                loop {
                    let report = backup::backup_with_sample(
                        &mut database,
                        &agents,
                        &client,
                        &target,
                        &database_path,
                        verify_sample,
                    )
                    .await?;
                    println!("{}", serde_json::to_string_pretty(&report)?);
                    match interval {
                        Some(seconds) => {
                            tokio::time::sleep(std::time::Duration::from_secs(seconds)).await;
                        }
                        None => {
                            if report.exit_code() == 2 {
                                std::process::exit(2);
                            }
                            return Ok(());
                        }
                    }
                }
            }
            PortalCommand::Drain {
                database,
                master_key,
                agents,
                agent_id,
                dry_run,
            } => {
                let agents = prepare_agents(agents)?;
                let mut database = Database::open(&database)?;
                let client = agent_client(&mut database, &master_key)?;
                let report =
                    lifecycle::drain(&mut database, &agents, &client, &agent_id, dry_run).await?;
                println!("{}", serde_json::to_string_pretty(&report)?);
                if report.exit_code() == 2 {
                    std::process::exit(2);
                }
                Ok(())
            }
            PortalCommand::Retire {
                database,
                master_key,
                agents,
                agent_id,
            } => {
                let agents = prepare_agents(agents)?;
                let mut database = Database::open(&database)?;
                let client = agent_client(&mut database, &master_key)?;
                let report = lifecycle::retire(&mut database, &agents, &client, &agent_id).await?;
                println!("{}", serde_json::to_string_pretty(&report)?);
                if report.exit_code() == 2 {
                    std::process::exit(2);
                }
                Ok(())
            }
            PortalCommand::Gc {
                database,
                master_key,
                agents,
                apply,
                retention_seconds,
                staging_grace_seconds,
            } => {
                let agents = prepare_agents(agents)?;
                let mut database = Database::open(&database)?;
                let client = agent_client(&mut database, &master_key)?;
                let report = lifecycle::gc(
                    &mut database,
                    &agents,
                    &client,
                    GcOptions {
                        retention_seconds,
                        staging_grace_seconds,
                        apply,
                    },
                )
                .await?;
                println!("{}", serde_json::to_string_pretty(&report)?);
                if report.exit_code() == 2 {
                    std::process::exit(2);
                }
                Ok(())
            }
            PortalCommand::Agent { command } => match command {
                AgentCommand::Token {
                    database,
                    master_key,
                    agent_id,
                } => {
                    let mut database = Database::open(&database)?;
                    let key = bound_master_key(&mut database, &master_key)?;
                    access::GrantMaterial::load_or_create(&mut database, &key)?;
                    let token = access::create_join_token(&mut database, &agent_id, now_seconds())?;
                    println!("{token}");
                    Ok(())
                }
                AgentCommand::Request {
                    database,
                    master_key,
                    agent,
                    operation,
                    kind,
                    hash,
                    after,
                    limit,
                    body_file,
                    out,
                } => {
                    let mut database = Database::open(&database)?;
                    let client = agent_client(&mut database, &master_key)?;
                    drop(database);
                    let object = || -> anyhow::Result<(ObjectKind, ObjectHash)> {
                        Ok((
                            kind.context("--kind is required")?,
                            hash.clone().context("--hash is required")?,
                        ))
                    };
                    let response = match operation {
                        AgentRequest::Health => client.health(&agent).await?,
                        AgentRequest::Inventory => {
                            let kind = kind.context("--kind is required")?;
                            client
                                .inventory(&agent, kind, after.as_ref(), limit)
                                .await?
                        }
                        AgentRequest::Get => {
                            let (kind, hash) = object()?;
                            client.get(&agent, kind, &hash).await?
                        }
                        AgentRequest::Put => {
                            let (kind, hash) = object()?;
                            let path = body_file.as_deref().context("--body-file is required")?;
                            let body = std::fs::read(path)
                                .with_context(|| format!("reading {}", path.display()))?;
                            client.put(&agent, kind, &hash, body.into()).await?
                        }
                    };
                    let status = response.status().as_u16();
                    let body = response.bytes().await?;
                    if let Some(out) = out {
                        std::fs::write(&out, &body)
                            .with_context(|| format!("writing {}", out.display()))?;
                    }
                    println!("{status}");
                    Ok(())
                }
                AgentCommand::Join {
                    database,
                    master_key,
                    agent,
                    token,
                } => {
                    let mut database = Database::open(&database)?;
                    let key = bound_master_key(&mut database, &master_key)?;
                    let outcome =
                        join_agent(&mut database, &key, &agent, &token, &LocalApi::from_env())
                            .await?;
                    println!(
                        "{}",
                        serde_json::to_string_pretty(&serde_json::json!({
                            "agent_id": outcome.agent_id,
                            "agent_node": outcome.agent_node,
                            "portal_node_pinned": outcome.portal_node_pinned,
                            "rejoined": outcome.rejoined,
                        }))?
                    );
                    Ok(())
                }
            },
            PortalCommand::Restore { command } => match command {
                RestoreCommand::Metadata {
                    target,
                    database,
                    snapshot,
                } => {
                    let report =
                        backup::restore_metadata(&target, &database, snapshot.as_deref()).await?;
                    println!("{}", serde_json::to_string_pretty(&report)?);
                    Ok(())
                }
                RestoreCommand::Objects {
                    target,
                    database,
                    master_key,
                    agents,
                } => {
                    let agents = prepare_agents(agents)?;
                    let mut database = Database::open(&database)?;
                    let client = agent_client(&mut database, &master_key)?;
                    drop(database);
                    let report = backup::restore_objects(&target, &agents, &client).await?;
                    println!("{}", serde_json::to_string_pretty(&report)?);
                    if report.exit_code() == 2 {
                        std::process::exit(2);
                    }
                    Ok(())
                }
            },
        },
    }
}
