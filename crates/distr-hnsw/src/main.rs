use std::{
    io::{self, Read, Write},
    net::SocketAddr,
    path::{Path, PathBuf},
    str::FromStr,
};

use anyhow::Context;
use clap::{Args, Parser, Subcommand};
use distr_hnsw::{
    agent::{bind_and_serve_agent, AgentIdentity},
    crypto::MasterKey,
    metadata::{Database, JobMode, KeyBinding},
    portal::{prepare_agents, AgentTarget, Failpoint, FailpointAction, Portal},
    reconcile::{health_report, scrub},
    recovery_bundle::{self, KdfParams},
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
    /// Run a loopback-only M1 opaque-object agent.
    Agent {
        #[arg(long)]
        id: String,
        #[arg(long)]
        failure_domain: String,
        #[arg(long)]
        bind: SocketAddr,
        #[arg(long)]
        volume: PathBuf,
    },
    /// Run portal metadata and file operations.
    Portal {
        #[command(subcommand)]
        command: PortalCommand,
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
    /// health. Needs no master key. `--repair` restores copies copy-first;
    /// nothing is ever deleted.
    Scrub {
        #[arg(long)]
        database: PathBuf,
        #[arg(long = "agent", required = true)]
        agents: Vec<AgentTarget>,
        #[arg(long)]
        repair: bool,
        /// Repeat continuously, sleeping this many seconds between jobs.
        #[arg(long)]
        interval: Option<u64>,
    },
    /// Print persisted lifecycle health without contacting agents.
    Health {
        #[arg(long)]
        database: PathBuf,
    },
}

#[derive(Subcommand)]
enum KeyCommand {
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
            std::fs::write(path, &armored)
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
            volume,
        } => bind_and_serve_agent(bind, volume, AgentIdentity { id, failure_domain }).await,
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
                println!("initialized");
                println!(
                    "master key id: {} ({})",
                    key.key_id_hex(),
                    match binding {
                        KeyBinding::Bound => "bound to database",
                        KeyBinding::Matched => "matches database",
                    }
                );
                if created && !no_recovery_bundle {
                    emit_recovery_bundle(&key, &passphrase, &kdf, None)?;
                } else if created {
                    println!(
                        "no recovery bundle emitted; run `portal key export-recovery` before this \
                         key protects any data you cannot lose"
                    );
                }
                Ok(())
            }
            PortalCommand::Key { command } => match command {
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
                let mut portal = Portal::open(&database, key, agents)?;
                if let Ok(value) = std::env::var("DISTR_HNSW_FAILPOINT") {
                    portal = portal
                        .with_failpoint(Failpoint::from_str(&value)?, FailpointAction::ExitProcess);
                }
                let file_id = portal.upload(&source, &idempotency_key).await?;
                println!("{file_id}");
                Ok(())
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
                let mut portal = Portal::open(&database, key, agents)?;
                if let Ok(value) = std::env::var("DISTR_HNSW_FAILPOINT") {
                    portal = portal
                        .with_failpoint(Failpoint::from_str(&value)?, FailpointAction::ExitProcess);
                }
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
                agents,
                repair,
                interval,
            } => {
                let agents = prepare_agents(agents)?;
                let mut database = Database::open(&database)?;
                let client = reqwest::Client::new();
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
            PortalCommand::Health { database } => {
                let database = Database::open(&database)?;
                let report = health_report(&database)?;
                println!("{}", serde_json::to_string_pretty(&report)?);
                if report.exit_code() == 2 {
                    std::process::exit(2);
                }
                Ok(())
            }
        },
    }
}
