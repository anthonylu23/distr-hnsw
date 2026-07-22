use std::{net::SocketAddr, path::PathBuf, str::FromStr};

use anyhow::Context;
use clap::{Parser, Subcommand};
use distr_hnsw::{
    agent::{bind_and_serve_agent, AgentIdentity},
    crypto::MasterKey,
    metadata::Database,
    portal::{AgentTarget, Failpoint, FailpointAction, Portal},
};
use uuid::Uuid;

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
    /// Initialize the SQLite database and file-backed master key.
    Init {
        #[arg(long)]
        database: PathBuf,
        #[arg(long)]
        master_key: PathBuf,
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
            } => {
                if database.exists() && !master_key.exists() {
                    anyhow::bail!(
                        "database exists but master key is missing; refusing to generate an unrelated key"
                    );
                }
                if master_key.exists() {
                    MasterKey::load(&master_key).with_context(|| {
                        format!("validating existing master key {}", master_key.display())
                    })?;
                } else {
                    MasterKey::create(&master_key)
                        .with_context(|| format!("creating master key {}", master_key.display()))?;
                }
                Database::open(&database)
                    .with_context(|| format!("initializing database {}", database.display()))?;
                println!("initialized");
                Ok(())
            }
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
        },
    }
}
