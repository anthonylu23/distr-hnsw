//! Benchmark harness for the distr-hnsw vector engine. Loads a pinned dataset
//! (verifying every file's BLAKE3 against its manifest), runs a search path,
//! and writes a `BenchReportV1` with provenance, recall, and latency
//! percentiles. Pass 1 provides the exact oracle only.

use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
    time::Instant,
};

use anyhow::{bail, Context};
use clap::{Parser, Subcommand};
use distr_hnsw_index::{
    distance::{distance, normalize},
    oracle::{recall, recall_by_distance, search},
    vector::FlatVectors,
    Metric, RecordId,
};
use rayon::prelude::*;
use serde::{Deserialize, Serialize};

#[derive(Parser)]
#[command(name = "distr-hnsw-bench", version, about)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Verify a dataset directory against its manifest.
    Verify { dataset: PathBuf },
    /// Run the exact oracle over a dataset and report recall and latency.
    Oracle {
        dataset: PathBuf,
        #[arg(long, default_value_t = 10)]
        k: usize,
        /// Use only the first N queries (default: all).
        #[arg(long)]
        queries: Option<usize>,
        /// Threads for the batch run (default: all cores). Latency percentiles
        /// are always measured single-threaded on a sample.
        #[arg(long)]
        threads: Option<usize>,
        #[arg(long)]
        out: Option<PathBuf>,
    },
}

#[derive(Debug, Deserialize)]
struct Manifest {
    name: String,
    metric: String,
    dims: usize,
    base_count: usize,
    query_count: usize,
    groundtruth_k: usize,
    files: BTreeMap<String, FileEntry>,
}

#[derive(Debug, Deserialize)]
struct FileEntry {
    bytes: u64,
    blake3: String,
}

struct Dataset {
    manifest: Manifest,
    metric: Metric,
    base: FlatVectors,
    queries: FlatVectors,
    groundtruth: Vec<Vec<RecordId>>,
}

fn verify_file(dir: &Path, name: &str, entry: &FileEntry) -> anyhow::Result<Vec<u8>> {
    let path = dir.join(name);
    let bytes = fs::read(&path).with_context(|| format!("reading {}", path.display()))?;
    if bytes.len() as u64 != entry.bytes {
        bail!(
            "{name}: {} bytes on disk, manifest says {}",
            bytes.len(),
            entry.bytes
        );
    }
    let digest = blake3::hash(&bytes).to_hex().to_string();
    if digest != entry.blake3 {
        bail!(
            "{name}: BLAKE3 {digest} does not match manifest {}",
            entry.blake3
        );
    }
    Ok(bytes)
}

fn f32le(bytes: &[u8]) -> Vec<f32> {
    bytes
        .chunks_exact(4)
        .map(|chunk| f32::from_le_bytes(chunk.try_into().expect("4 bytes")))
        .collect()
}

fn load(dir: &Path) -> anyhow::Result<Dataset> {
    let manifest: Manifest = serde_json::from_slice(
        &fs::read(dir.join("manifest.json")).context("reading manifest.json")?,
    )?;
    let metric = Metric::parse(&manifest.metric)
        .with_context(|| format!("unsupported metric {}", manifest.metric))?;
    let read = |name: &str| -> anyhow::Result<Vec<u8>> {
        let entry = manifest
            .files
            .get(name)
            .with_context(|| format!("manifest lacks {name}"))?;
        verify_file(dir, name, entry)
    };
    let base = FlatVectors::from_vec(manifest.dims, f32le(&read("base.f32le")?), metric)?;
    let queries = FlatVectors::from_vec(manifest.dims, f32le(&read("queries.f32le")?), metric)?;
    let truth = read("groundtruth.i32le")?;
    let groundtruth: Vec<Vec<RecordId>> = truth
        .chunks_exact(4 * manifest.groundtruth_k)
        .map(|row| {
            row.chunks_exact(4)
                .map(|chunk| i32::from_le_bytes(chunk.try_into().expect("4 bytes")) as RecordId)
                .collect()
        })
        .collect();
    if base.len() != manifest.base_count
        || queries.len() != manifest.query_count
        || groundtruth.len() != manifest.query_count
    {
        bail!("dataset shapes do not match the manifest");
    }
    Ok(Dataset {
        manifest,
        metric,
        base,
        queries,
        groundtruth,
    })
}

#[derive(Serialize)]
struct BenchReportV1 {
    report_type: &'static str,
    version: u16,
    dataset: String,
    metric: String,
    dims: usize,
    base_count: usize,
    queries_evaluated: usize,
    k: usize,
    path: &'static str,
    parameters: BTreeMap<String, serde_json::Value>,
    /// Distance-based recall@k (see `oracle::recall_by_distance`).
    recall_at_k: f64,
    recall_min: f64,
    /// Id-based recall@k, lower on datasets with duplicate vectors.
    id_recall_at_k: f64,
    latency_single_thread_ms: Percentiles,
    batch_queries_per_second: f64,
    batch_threads: usize,
    host: String,
    source_revision: String,
}

#[derive(Serialize)]
struct Percentiles {
    samples: usize,
    p50: f64,
    p95: f64,
    p99: f64,
    max: f64,
}

fn percentiles(mut samples: Vec<f64>) -> Percentiles {
    samples.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let pick = |q: f64| {
        if samples.is_empty() {
            0.0
        } else {
            samples[((samples.len() - 1) as f64 * q).round() as usize]
        }
    };
    Percentiles {
        samples: samples.len(),
        p50: pick(0.50),
        p95: pick(0.95),
        p99: pick(0.99),
        max: samples.last().copied().unwrap_or(0.0),
    }
}

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Command::Verify { dataset } => {
            let loaded = load(&dataset)?;
            println!(
                "{}: {} x {} ({}), {} queries, k={} verified",
                loaded.manifest.name,
                loaded.base.len(),
                loaded.base.dims(),
                loaded.metric.as_str(),
                loaded.queries.len(),
                loaded.manifest.groundtruth_k
            );
            Ok(())
        }
        Command::Oracle {
            dataset,
            k,
            queries,
            threads,
            out,
        } => {
            let loaded = load(&dataset)?;
            if k > loaded.manifest.groundtruth_k {
                bail!(
                    "k={k} exceeds ground-truth depth {}",
                    loaded.manifest.groundtruth_k
                );
            }
            let count = queries
                .unwrap_or(loaded.queries.len())
                .min(loaded.queries.len());
            let threads = threads.unwrap_or_else(num_threads);
            rayon::ThreadPoolBuilder::new()
                .num_threads(threads)
                .build_global()
                .ok();

            // Single-threaded latency on a sample of up to 200 queries.
            let sample = count.min(200);
            let mut latencies = Vec::with_capacity(sample);
            for index in 0..sample {
                let mut query = loaded.queries.get(index as RecordId).unwrap().to_vec();
                if loaded.metric.normalizes() {
                    normalize(&mut query);
                }
                let started = Instant::now();
                let hits = search(&loaded.base, loaded.metric, &query, k, |_| true);
                latencies.push(started.elapsed().as_secs_f64() * 1e3);
                std::hint::black_box(hits);
            }

            // Recall over all requested queries, in parallel.
            let started = Instant::now();
            let recalls: Vec<(f64, f64)> = (0..count)
                .into_par_iter()
                .map(|index| {
                    let query = loaded.queries.get(index as RecordId).unwrap();
                    let hits = search(&loaded.base, loaded.metric, query, k, |_| true);
                    let truth = &loaded.groundtruth[index][..k];
                    // k-th true distance from the reference ids, computed
                    // with our own kernel so the comparison is consistent.
                    let kth = truth
                        .iter()
                        .map(|id| {
                            let row = loaded.base.get(*id).expect("ground-truth id in range");
                            distance(loaded.metric, query, row)
                        })
                        .fold(f32::MIN, f32::max);
                    (recall_by_distance(kth, &hits, k), recall(truth, &hits))
                })
                .collect();
            let batch_seconds = started.elapsed().as_secs_f64();
            let recall_at_k =
                recalls.iter().map(|r| r.0).sum::<f64>() / recalls.len().max(1) as f64;
            let recall_min = recalls.iter().map(|r| r.0).fold(1.0, f64::min);
            let id_recall_at_k =
                recalls.iter().map(|r| r.1).sum::<f64>() / recalls.len().max(1) as f64;

            let report = BenchReportV1 {
                report_type: "BenchReportV1",
                version: 1,
                dataset: loaded.manifest.name.clone(),
                metric: loaded.metric.as_str().to_owned(),
                dims: loaded.base.dims(),
                base_count: loaded.base.len(),
                queries_evaluated: count,
                k,
                path: "oracle",
                parameters: BTreeMap::new(),
                recall_at_k,
                recall_min,
                id_recall_at_k,
                latency_single_thread_ms: percentiles(latencies),
                batch_queries_per_second: count as f64 / batch_seconds,
                batch_threads: threads,
                host: hostname(),
                source_revision: option_env!("DISTR_HNSW_REV")
                    .unwrap_or("unknown")
                    .to_owned(),
            };
            let json = serde_json::to_string_pretty(&report)?;
            if let Some(out) = out {
                fs::write(&out, &json)?;
            }
            println!("{json}");
            Ok(())
        }
    }
}

fn num_threads() -> usize {
    std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1)
}

fn hostname() -> String {
    fs::read_to_string("/etc/hostname")
        .map(|s| s.trim().to_owned())
        .unwrap_or_else(|_| "unknown".to_owned())
}
