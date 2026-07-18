use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};

#[derive(Debug, Clone, Parser)]
#[command(
    name = "distr-hnsw-validate",
    about = "Phase-0 semantic search validation prototype (disposable)",
    version
)]
pub struct Cli {
    /// Working directory for SQLite DB and reports
    #[arg(long, env = "DISTR_HNSW_WORK_DIR", global = true)]
    pub work_dir: Option<PathBuf>,

    /// Ollama base URL
    #[arg(
        long,
        env = "OLLAMA_HOST",
        global = true,
        default_value = "http://127.0.0.1:11434"
    )]
    pub ollama_url: String,

    #[command(subcommand)]
    pub command: Command,
}

#[derive(Debug, Clone, Subcommand)]
pub enum Command {
    /// Walk corpus, extract text, chunk, write SQLite
    Prepare {
        /// Root directory of files to index
        #[arg(long)]
        corpus: PathBuf,

        /// Skip files larger than this many bytes
        #[arg(long, default_value_t = 8 * 1024 * 1024)]
        max_file_bytes: u64,

        /// Approximate chunk size in characters (~512 tokens)
        #[arg(long, default_value_t = 2000)]
        chunk_chars: usize,

        /// Chunk overlap in characters
        #[arg(long, default_value_t = 200)]
        chunk_overlap: usize,

        /// Delete validate.db (+ wal/shm) before preparing
        #[arg(long, default_value_t = false)]
        fresh: bool,
    },

    /// Embed chunks via Ollama for one model/dims pair
    Embed {
        /// Ollama model name (e.g. nomic-embed-text)
        #[arg(long)]
        model: String,

        /// Target dimensionality (truncate + re-normalize if below native)
        #[arg(long)]
        dims: usize,

        /// Max in-flight embedding requests
        #[arg(long, default_value_t = 2)]
        concurrency: usize,

        /// Texts per Ollama request
        #[arg(long, default_value_t = 8)]
        batch_size: usize,

        /// Re-embed chunks that already have vectors for this model/dims
        #[arg(long, default_value_t = false)]
        force: bool,
    },

    /// One-shot semantic + baseline ranking for a query string
    Query {
        /// Query text
        #[arg(long)]
        text: String,

        /// Ollama model name
        #[arg(long)]
        model: String,

        /// Embedding dimensionality
        #[arg(long)]
        dims: usize,

        /// Results to show per ranking
        #[arg(long, default_value_t = 10)]
        k: usize,
    },

    /// Evaluate labeled queries and write a report
    Eval {
        /// Path to queries.json
        #[arg(long)]
        queries: PathBuf,

        /// Model/dims pairs as `model:dims` (repeatable)
        #[arg(long = "model", required = true)]
        models: Vec<String>,

        /// Top-k for metrics
        #[arg(long, default_value_t = 10)]
        k: usize,

        /// Optional report stem (timestamp used if omitted)
        #[arg(long)]
        out: Option<String>,
    },

    /// Print corpus / embedding status
    Status,
}

impl Cli {
    pub fn resolve_work_dir(&self) -> Result<PathBuf> {
        if let Some(dir) = &self.work_dir {
            return Ok(dir.clone());
        }
        let home = std::env::var_os("HOME").context("HOME is not set; pass --work-dir")?;
        Ok(PathBuf::from(home).join("distr-hnsw-proto"))
    }
}

pub fn parse_model_dims(spec: &str) -> Result<(String, usize)> {
    let (model, dims) = spec
        .rsplit_once(':')
        .with_context(|| format!("expected model:dims, got {spec:?}"))?;
    let dims: usize = dims
        .parse()
        .with_context(|| format!("invalid dims in {spec:?}"))?;
    anyhow::ensure!(!model.is_empty(), "empty model in {spec:?}");
    anyhow::ensure!(dims > 0, "dims must be > 0");
    Ok((model.to_string(), dims))
}
