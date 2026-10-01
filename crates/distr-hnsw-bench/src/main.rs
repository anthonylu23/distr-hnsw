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
    hnsw::{Hnsw, HnswParams, SearchMode},
    oracle::{recall, recall_by_distance, search},
    vector::FlatVectors,
    Hit, Metric, RecordId,
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
    /// Bulk-load a dataset into an on-disk partition, then measure snapshot
    /// size, recovery time, and recall after recovery.
    Persist {
        dataset: PathBuf,
        /// Directory for the partition (created; must not hold one already).
        #[arg(long)]
        partition_dir: PathBuf,
        #[arg(long, default_value_t = 10)]
        k: usize,
        #[arg(long, default_value_t = 100)]
        ef: usize,
        #[arg(long, default_value_t = 16)]
        m: usize,
        #[arg(long, default_value_t = 200)]
        ef_construction: usize,
        #[arg(long, default_value_t = 1000)]
        queries: usize,
        /// Append this many upserts after the snapshot so recovery also
        /// replays a WAL tail.
        #[arg(long, default_value_t = 1000)]
        tail: usize,
        /// Use only the first N base vectors (default: all).
        #[arg(long)]
        limit: Option<usize>,
        #[arg(long)]
        threads: Option<usize>,
        #[arg(long)]
        out: Option<PathBuf>,
    },
    /// Bulk-load a dataset, delete a fraction of it, then measure two-phase
    /// compaction: rebuild time, swap time, snapshot shrinkage, and recall
    /// before and after.
    Compact {
        dataset: PathBuf,
        #[arg(long)]
        partition_dir: PathBuf,
        #[arg(long, default_value_t = 10)]
        k: usize,
        #[arg(long, default_value_t = 100)]
        ef: usize,
        #[arg(long, default_value_t = 16)]
        m: usize,
        #[arg(long, default_value_t = 200)]
        ef_construction: usize,
        #[arg(long, default_value_t = 1000)]
        queries: usize,
        /// Fraction of the base to delete before compacting.
        #[arg(long, default_value_t = 0.2)]
        delete_fraction: f64,
        /// Upserts appended between `begin_compaction` and `finish_compaction`
        /// so the catch-up replay is exercised.
        #[arg(long, default_value_t = 1000)]
        concurrent: usize,
        #[arg(long)]
        limit: Option<usize>,
        #[arg(long)]
        threads: Option<usize>,
        #[arg(long)]
        out: Option<PathBuf>,
    },
    /// Build an HNSW graph, derive synthetic filters at several selectivities,
    /// and measure the brute-force and masked-traversal routes against exact
    /// filtered ground truth to set the cutover.
    Filtered {
        dataset: PathBuf,
        #[arg(long, default_value_t = 10)]
        k: usize,
        #[arg(long, default_value_t = 100)]
        ef: usize,
        #[arg(long, default_value_t = 16)]
        m: usize,
        #[arg(long, default_value_t = 200)]
        ef_construction: usize,
        /// Comma-separated selectivities as fractions of the base.
        #[arg(long, default_value = "0.001,0.01,0.1,0.5")]
        selectivities: String,
        #[arg(long, default_value_t = 1000)]
        queries: usize,
        #[arg(long)]
        threads: Option<usize>,
        #[arg(long)]
        out: Option<PathBuf>,
    },
    /// Build an HNSW graph over a dataset and report recall and latency at
    /// one or more `ef_search` values.
    Hnsw {
        dataset: PathBuf,
        #[arg(long, default_value_t = 10)]
        k: usize,
        /// Comma-separated ef_search values.
        #[arg(long, default_value = "50,100,200")]
        ef: String,
        #[arg(long, default_value_t = 16)]
        m: usize,
        #[arg(long, default_value_t = 200)]
        ef_construction: usize,
        /// Use only the first N base vectors (default: all).
        #[arg(long)]
        limit: Option<usize>,
        /// Use only the first N queries (default: all).
        #[arg(long)]
        queries: Option<usize>,
        #[arg(long)]
        threads: Option<usize>,
        /// Insert sequentially (the WAL apply path) instead of the parallel
        /// bulk build.
        #[arg(long)]
        sequential: bool,
        /// Traversal modes to measure on the same graph: `f32`, `int8`, or
        /// `both`.
        #[arg(long, default_value = "both")]
        modes: String,
        /// Int8 rescoring depth (default max(4k, 100) per the contract).
        #[arg(long)]
        rescore: Option<usize>,
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
struct HnswReportV1 {
    report_type: &'static str,
    version: u16,
    dataset: String,
    metric: String,
    dims: usize,
    base_count: usize,
    queries_evaluated: usize,
    k: usize,
    parameters: BTreeMap<String, serde_json::Value>,
    build_seconds: f64,
    build_vectors_per_second: f64,
    accounted_bytes: usize,
    accounted_bytes_per_vector: f64,
    accounted_f32_bytes: usize,
    accounted_int8_bytes: usize,
    accounted_graph_bytes: usize,
    rss_bytes_after_build: u64,
    max_level: u8,
    points: Vec<EfPoint>,
    host: String,
    source_revision: String,
}

#[derive(Serialize)]
struct EfPoint {
    mode: String,
    rescore: Option<usize>,
    ef_search: usize,
    recall_at_k: f64,
    recall_min: f64,
    id_recall_at_k: f64,
    latency_single_thread_ms: Percentiles,
    batch_queries_per_second: f64,
    batch_threads: usize,
}

#[derive(Serialize)]
struct PersistReportV1 {
    report_type: &'static str,
    version: u16,
    dataset: String,
    metric: String,
    dims: usize,
    base_count: usize,
    k: usize,
    ef_search: usize,
    parameters: BTreeMap<String, serde_json::Value>,
    build_seconds: f64,
    snapshot_write_seconds: f64,
    snapshot_bytes: u64,
    snapshot_bytes_per_vector: f64,
    wal_tail_entries: usize,
    wal_tail_seconds: f64,
    wal_tail_bytes: u64,
    recovery_seconds: f64,
    recovery_replayed: u64,
    resident_bytes_after_recovery: usize,
    mapped_bytes_after_recovery: usize,
    rss_bytes_after_recovery: u64,
    recall_before: f64,
    recall_after: f64,
    results_identical: bool,
    host: String,
    source_revision: String,
}

#[derive(Serialize)]
struct CompactReportV1 {
    report_type: &'static str,
    version: u16,
    dataset: String,
    metric: String,
    dims: usize,
    base_count: usize,
    deleted: usize,
    concurrent_upserts: usize,
    k: usize,
    ef_search: usize,
    parameters: BTreeMap<String, serde_json::Value>,
    tombstone_ratio_before: f64,
    slots_before: usize,
    slots_after: usize,
    delete_seconds: f64,
    begin_seconds: f64,
    finish_seconds: f64,
    caught_up_entries: u64,
    snapshot_bytes_before: u64,
    snapshot_bytes_after: u64,
    resident_bytes_before: usize,
    resident_bytes_after: usize,
    query_p50_us_before: f64,
    query_p50_us_after: f64,
    recall_before: f64,
    recall_after: f64,
    deleted_keys_returned_after: usize,
    host: String,
    source_revision: String,
}

#[derive(Serialize)]
struct FilteredReportV1 {
    report_type: &'static str,
    version: u16,
    dataset: String,
    metric: String,
    dims: usize,
    base_count: usize,
    queries_evaluated: usize,
    k: usize,
    ef_search: usize,
    parameters: BTreeMap<String, serde_json::Value>,
    levels: Vec<SelectivityPoint>,
    /// Allowed-set size at which brute force and masked traversal cost the
    /// same, interpolated on log scale; the cutover `c` in `T = c · k`.
    crossover_allowed: Option<f64>,
    crossover_c: Option<f64>,
    host: String,
    source_revision: String,
}

#[derive(Serialize)]
struct SelectivityPoint {
    selectivity: f64,
    allowed: u64,
    brute_force_latency_ms: Percentiles,
    masked_recall_at_k: f64,
    masked_recall_min: f64,
    masked_latency_ms: Percentiles,
    /// Masked traversal with ef scaled to keep recall up under selective
    /// filters: ef × 4.
    masked_ef4_recall_at_k: f64,
    masked_ef4_latency_ms: Percentiles,
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
        Command::Persist {
            dataset,
            partition_dir,
            k,
            ef,
            m,
            ef_construction,
            queries,
            tail,
            limit,
            threads,
            out,
        } => {
            use distr_hnsw_index::partition::{Partition, PartitionConfig};
            let loaded = load(&dataset)?;
            let threads = threads.unwrap_or_else(num_threads);
            rayon::ThreadPoolBuilder::new()
                .num_threads(threads)
                .build_global()
                .ok();
            let count = queries.min(loaded.queries.len());
            let base_count = limit.unwrap_or(loaded.base.len()).min(loaded.base.len());
            let params = HnswParams {
                m,
                m0: 2 * m,
                ef_construction,
                seed: 0x5eed,
            };
            let config = PartitionConfig::new(
                uuid::Uuid::new_v4(),
                loaded.base.dims(),
                loaded.metric,
                params,
            );
            let mut subset = FlatVectors::with_capacity(loaded.base.dims(), base_count);
            for slot in 0..base_count {
                subset.push(loaded.base.get(slot as RecordId).unwrap(), Metric::L2)?;
            }
            eprintln!(
                "bulk loading {base_count} vectors into {}",
                partition_dir.display()
            );
            let started = Instant::now();
            let (mut partition, snapshot_path) = Partition::bulk_load(
                &partition_dir,
                config,
                subset,
                (0..base_count as u64).map(|i| i.to_le_bytes().to_vec()),
            )?;
            let build_and_snapshot = started.elapsed().as_secs_f64();
            // Split build time from snapshot time by writing one more snapshot.
            let started = Instant::now();
            let second = partition.snapshot()?;
            let snapshot_write_seconds = started.elapsed().as_secs_f64();
            // Both snapshots sit at high-water mark 0 and share one name; the
            // second atomically replaced the first with identical content.
            debug_assert_eq!(second, snapshot_path);
            let snapshot_bytes = fs::metadata(&second)?.len();
            let build_seconds = (build_and_snapshot - snapshot_write_seconds).max(0.0);
            eprintln!(
                "built in {build_seconds:.1}s, snapshot {:.2} GiB written in {snapshot_write_seconds:.1}s",
                snapshot_bytes as f64 / 1073741824.0
            );

            // WAL tail: re-upsert the first `tail` rows under new keys.
            let started = Instant::now();
            for i in 0..tail.min(base_count) {
                let mut op = [0_u8; 16];
                op[..8].copy_from_slice(&(i as u64).to_le_bytes());
                op[8] = 1;
                partition.upsert(
                    op,
                    format!("tail-{i}").as_bytes(),
                    loaded.base.get(i as RecordId).unwrap(),
                    b"{}",
                )?;
            }
            let wal_tail_seconds = started.elapsed().as_secs_f64();
            let wal_tail_bytes: u64 = fs::read_dir(partition_dir.join("wal"))?
                .filter_map(|e| e.ok())
                .filter_map(|e| e.metadata().ok())
                .map(|m| m.len())
                .sum();
            let search_all = |p: &Partition| -> Vec<Vec<(Vec<u8>, f32)>> {
                (0..count)
                    .into_par_iter()
                    .map(|q| {
                        let query = loaded.queries.get(q as RecordId).unwrap();
                        p.search(query, k, ef, None)
                            .unwrap()
                            .into_iter()
                            .map(|h| (h.key, h.distance))
                            .collect()
                    })
                    .collect()
            };
            let recall_of = |results: &[Vec<(Vec<u8>, f32)>]| -> f64 {
                let total: f64 = (0..count)
                    .map(|q| {
                        let query = loaded.queries.get(q as RecordId).unwrap();
                        let truth = &loaded.groundtruth[q][..k];
                        let kth = truth
                            .iter()
                            .map(|id| distance(loaded.metric, query, loaded.base.get(*id).unwrap()))
                            .fold(f32::MIN, f32::max);
                        let hits: Vec<Hit> = results[q]
                            .iter()
                            .map(|(_, d)| Hit {
                                id: 0,
                                distance: *d,
                            })
                            .collect();
                        recall_by_distance(kth, &hits, k)
                    })
                    .sum();
                total / count.max(1) as f64
            };
            let before = search_all(&partition);
            let recall_before = recall_of(&before);
            drop(partition);

            let started = Instant::now();
            let (recovered, report) = Partition::open(&partition_dir)?;
            let recovery_seconds = started.elapsed().as_secs_f64();
            let rss = rss_bytes();
            let after = search_all(&recovered);
            let recall_after = recall_of(&after);
            eprintln!(
                "recovered in {recovery_seconds:.2}s (replayed {}), recall before {recall_before:.4} after {recall_after:.4}",
                report.wal_entries_replayed
            );

            let mut parameters = BTreeMap::new();
            parameters.insert("m".to_owned(), serde_json::json!(m));
            parameters.insert(
                "ef_construction".to_owned(),
                serde_json::json!(ef_construction),
            );
            let out_report = PersistReportV1 {
                report_type: "PersistReportV1",
                version: 1,
                dataset: loaded.manifest.name.clone(),
                metric: loaded.metric.as_str().to_owned(),
                dims: loaded.base.dims(),
                base_count,
                k,
                ef_search: ef,
                parameters,
                build_seconds,
                snapshot_write_seconds,
                snapshot_bytes,
                snapshot_bytes_per_vector: snapshot_bytes as f64 / base_count.max(1) as f64,
                wal_tail_entries: tail.min(base_count),
                wal_tail_seconds,
                wal_tail_bytes,
                recovery_seconds,
                recovery_replayed: report.wal_entries_replayed,
                resident_bytes_after_recovery: recovered.resident_bytes(),
                mapped_bytes_after_recovery: recovered.mapped_bytes(),
                rss_bytes_after_recovery: rss,
                recall_before,
                recall_after,
                results_identical: before == after,
                host: hostname(),
                source_revision: option_env!("DISTR_HNSW_REV")
                    .unwrap_or("unknown")
                    .to_owned(),
            };
            let json = serde_json::to_string_pretty(&out_report)?;
            if let Some(out) = out {
                fs::write(&out, &json)?;
            }
            println!("{json}");
            Ok(())
        }
        Command::Compact {
            dataset,
            partition_dir,
            k,
            ef,
            m,
            ef_construction,
            queries,
            delete_fraction,
            concurrent,
            limit,
            threads,
            out,
        } => {
            use distr_hnsw_index::partition::{Partition, PartitionConfig};
            let loaded = load(&dataset)?;
            let threads = threads.unwrap_or_else(num_threads);
            rayon::ThreadPoolBuilder::new()
                .num_threads(threads)
                .build_global()
                .ok();
            let count = queries.min(loaded.queries.len());
            let base_count = limit.unwrap_or(loaded.base.len()).min(loaded.base.len());
            let params = HnswParams {
                m,
                m0: 2 * m,
                ef_construction,
                seed: 0x5eed,
            };
            let config = PartitionConfig::new(
                uuid::Uuid::new_v4(),
                loaded.base.dims(),
                loaded.metric,
                params,
            );
            let mut subset = FlatVectors::with_capacity(loaded.base.dims(), base_count);
            for slot in 0..base_count {
                subset.push(loaded.base.get(slot as RecordId).unwrap(), Metric::L2)?;
            }
            eprintln!("bulk loading {base_count} vectors");
            let (mut partition, snapshot_before) = Partition::bulk_load(
                &partition_dir,
                config,
                subset,
                (0..base_count as u64).map(|i| i.to_le_bytes().to_vec()),
            )?;
            let snapshot_bytes_before = fs::metadata(&snapshot_before)?.len();

            // Delete every `stride`-th key until the fraction is reached.
            let deleted_count = ((base_count as f64) * delete_fraction).round() as usize;
            let stride = (base_count / deleted_count.max(1)).max(1);
            let deleted: Vec<u64> = (0..base_count as u64)
                .step_by(stride)
                .take(deleted_count)
                .collect();
            let started = Instant::now();
            for (n, key) in deleted.iter().enumerate() {
                let mut op = [0_u8; 16];
                op[..8].copy_from_slice(&(n as u64).to_le_bytes());
                op[8] = 2;
                partition.delete(op, &key.to_le_bytes())?;
            }
            let delete_seconds = started.elapsed().as_secs_f64();
            let deleted_set: std::collections::HashSet<Vec<u8>> =
                deleted.iter().map(|k| k.to_le_bytes().to_vec()).collect();

            type Results = Vec<Vec<(Vec<u8>, f32)>>;
            let timed_search = |p: &Partition| -> (Results, f64) {
                let mut latencies: Vec<f64> = Vec::with_capacity(count);
                let mut results = Vec::with_capacity(count);
                for q in 0..count {
                    let query = loaded.queries.get(q as RecordId).unwrap();
                    let started = Instant::now();
                    let hits = p.search(query, k, ef, None).unwrap();
                    latencies.push(started.elapsed().as_secs_f64() * 1e6);
                    results.push(
                        hits.into_iter()
                            .map(|h| (h.key, h.distance))
                            .collect::<Vec<_>>(),
                    );
                }
                latencies.sort_by(|a, b| a.total_cmp(b));
                let p50 = latencies.get(count / 2).copied().unwrap_or(0.0);
                (results, p50)
            };
            // Ground truth against the surviving base only.
            let recall_of = |results: &Results| -> f64 {
                let total: f64 = (0..count)
                    .into_par_iter()
                    .map(|q| {
                        let query = loaded.queries.get(q as RecordId).unwrap();
                        let mut truth: Vec<f32> = (0..base_count)
                            .filter(|slot| !(slot % stride == 0 && slot / stride < deleted_count))
                            .map(|slot| {
                                distance(
                                    loaded.metric,
                                    query,
                                    loaded.base.get(slot as RecordId).unwrap(),
                                )
                            })
                            .collect();
                        truth.sort_by(|a, b| a.total_cmp(b));
                        let kth = truth[k.min(truth.len()) - 1];
                        let hits: Vec<Hit> = results[q]
                            .iter()
                            .map(|(_, d)| Hit {
                                id: 0,
                                distance: *d,
                            })
                            .collect();
                        recall_by_distance(kth, &hits, k)
                    })
                    .sum();
                total / count.max(1) as f64
            };
            let (before, p50_before) = timed_search(&partition);
            let recall_before = recall_of(&before);
            let tombstone_ratio_before = partition.tombstone_ratio();
            let slots_before = partition.slot_count();
            let resident_bytes_before = partition.resident_bytes();
            eprintln!(
                "deleted {} ({tombstone_ratio_before:.3} tombstones); recall before {recall_before:.4}, p50 {p50_before:.0}us",
                deleted.len()
            );

            let started = Instant::now();
            let plan = partition.begin_compaction()?;
            let begin_seconds = started.elapsed().as_secs_f64();
            // Writes that land while the rebuild is "in flight".
            for i in 0..concurrent.min(base_count) {
                let mut op = [0_u8; 16];
                op[..8].copy_from_slice(&(i as u64).to_le_bytes());
                op[8] = 3;
                partition.upsert(
                    op,
                    format!("concurrent-{i}").as_bytes(),
                    loaded.base.get(i as RecordId).unwrap(),
                    b"{}",
                )?;
            }
            let started = Instant::now();
            let report = partition.finish_compaction(plan)?;
            let finish_seconds = started.elapsed().as_secs_f64();
            let snapshot_bytes_after = fs::metadata(&report.snapshot)?.len();
            eprintln!(
                "compacted {} -> {} slots: begin {begin_seconds:.1}s, finish {finish_seconds:.2}s (caught up {})",
                report.slots_before, report.slots_after, report.caught_up_entries
            );

            let (after, p50_after) = timed_search(&partition);
            // Recall after includes the concurrent upserts as duplicates of
            // live base rows; recall_by_distance tolerates that.
            let recall_after = recall_of(&after);
            let deleted_keys_returned_after = after
                .iter()
                .flatten()
                .filter(|(key, _)| deleted_set.contains(key))
                .count();

            let mut parameters = BTreeMap::new();
            parameters.insert("m".to_owned(), serde_json::json!(m));
            parameters.insert(
                "ef_construction".to_owned(),
                serde_json::json!(ef_construction),
            );
            let out_report = CompactReportV1 {
                report_type: "CompactReportV1",
                version: 1,
                dataset: loaded.manifest.name.clone(),
                metric: loaded.metric.as_str().to_owned(),
                dims: loaded.base.dims(),
                base_count,
                deleted: deleted.len(),
                concurrent_upserts: concurrent.min(base_count),
                k,
                ef_search: ef,
                parameters,
                tombstone_ratio_before,
                slots_before,
                slots_after: report.slots_after,
                delete_seconds,
                begin_seconds,
                finish_seconds,
                caught_up_entries: report.caught_up_entries,
                snapshot_bytes_before,
                snapshot_bytes_after,
                resident_bytes_before,
                resident_bytes_after: partition.resident_bytes(),
                query_p50_us_before: p50_before,
                query_p50_us_after: p50_after,
                recall_before,
                recall_after,
                deleted_keys_returned_after,
                host: hostname(),
                source_revision: option_env!("DISTR_HNSW_REV")
                    .unwrap_or("unknown")
                    .to_owned(),
            };
            let json = serde_json::to_string_pretty(&out_report)?;
            if let Some(out) = out {
                fs::write(&out, &json)?;
            }
            println!("{json}");
            Ok(())
        }
        Command::Filtered {
            dataset,
            k,
            ef,
            m,
            ef_construction,
            selectivities,
            queries,
            threads,
            out,
        } => {
            use roaring::RoaringBitmap;
            let loaded = load(&dataset)?;
            let threads = threads.unwrap_or_else(num_threads);
            rayon::ThreadPoolBuilder::new()
                .num_threads(threads)
                .build_global()
                .ok();
            let levels: Vec<f64> = selectivities
                .split(',')
                .map(|v| v.trim().parse::<f64>())
                .collect::<Result<_, _>>()
                .context("parsing --selectivities")?;
            let count = queries.min(loaded.queries.len());
            let base_count = loaded.base.len();
            let params = HnswParams {
                m,
                m0: 2 * m,
                ef_construction,
                seed: 0x5eed,
            };
            eprintln!("building HNSW over {base_count} vectors (m={m})");
            let mut subset = FlatVectors::with_capacity(loaded.base.dims(), base_count);
            for slot in 0..base_count {
                subset.push(loaded.base.get(slot as RecordId).unwrap(), Metric::L2)?;
            }
            let index = Hnsw::build_parallel(subset, loaded.metric, params);

            let mut points = Vec::new();
            for &selectivity in &levels {
                // Deterministic pseudo-random subset: splitmix over the slot.
                let threshold = (selectivity * u32::MAX as f64) as u64;
                let allowed: RoaringBitmap = (0..base_count as u32)
                    .filter(|&slot| {
                        let mut x =
                            (u64::from(slot) + 0x9E37_79B9).wrapping_mul(0xBF58_476D_1CE4_E5B9);
                        x ^= x >> 31;
                        (x & 0xFFFF_FFFF) < threshold
                    })
                    .collect();
                let n = allowed.len();
                eprintln!("selectivity {selectivity}: {n} allowed slots");
                // Exact filtered ground truth from the oracle over the set.
                let truth: Vec<Vec<Hit>> = (0..count)
                    .into_par_iter()
                    .map(|q| {
                        let query = loaded.queries.get(q as RecordId).unwrap();
                        search(&loaded.base, loaded.metric, query, k, |id| {
                            allowed.contains(id as u32)
                        })
                    })
                    .collect();
                let sample = count.min(200);
                let time_route =
                    |route: &(dyn Fn(&[f32]) -> Vec<Hit> + Sync)| -> (Percentiles, f64, f64) {
                        let mut latencies = Vec::with_capacity(sample);
                        for q in 0..sample {
                            let query = loaded.queries.get(q as RecordId).unwrap();
                            let started = Instant::now();
                            std::hint::black_box(route(query));
                            latencies.push(started.elapsed().as_secs_f64() * 1e3);
                        }
                        let recalls: Vec<f64> = (0..count)
                            .into_par_iter()
                            .map(|q| {
                                let query = loaded.queries.get(q as RecordId).unwrap();
                                let hits = route(query);
                                let kth = truth[q].last().map(|h| h.distance).unwrap_or(0.0);
                                recall_by_distance(kth, &hits, truth[q].len().min(k))
                            })
                            .collect();
                        let mean = recalls.iter().sum::<f64>() / recalls.len().max(1) as f64;
                        let min = recalls.iter().cloned().fold(1.0, f64::min);
                        (percentiles(latencies), mean, min)
                    };
                let (brute_latency, brute_recall, _) =
                    time_route(&|query| index.search_exact_over(query, k, &allowed));
                anyhow::ensure!(
                    brute_recall > 0.999,
                    "brute-force route must be exact ({brute_recall})"
                );
                let mode = SearchMode::int8_default(k);
                let (masked_latency, masked_recall, masked_min) =
                    time_route(&|query| index.search_filtered(query, k, ef, mode, &allowed, 0));
                let (ef4_latency, ef4_recall, _) =
                    time_route(&|query| index.search_filtered(query, k, ef * 4, mode, &allowed, 0));
                eprintln!(
                    "  brute p50={:.3}ms | masked ef={ef} recall={masked_recall:.4} p50={:.3}ms | masked ef={} recall={ef4_recall:.4} p50={:.3}ms",
                    brute_latency.p50, masked_latency.p50, ef * 4, ef4_latency.p50
                );
                points.push(SelectivityPoint {
                    selectivity,
                    allowed: n,
                    brute_force_latency_ms: brute_latency,
                    masked_recall_at_k: masked_recall,
                    masked_recall_min: masked_min,
                    masked_latency_ms: masked_latency,
                    masked_ef4_recall_at_k: ef4_recall,
                    masked_ef4_latency_ms: ef4_latency,
                });
            }
            // Crossover: first interval where brute force becomes slower
            // than masked traversal; interpolate on log(allowed).
            let mut crossover = None;
            for pair in points.windows(2) {
                let (a, b) = (&pair[0], &pair[1]);
                let fa = a.brute_force_latency_ms.p50 - a.masked_latency_ms.p50;
                let fb = b.brute_force_latency_ms.p50 - b.masked_latency_ms.p50;
                if fa <= 0.0 && fb > 0.0 {
                    let la = (a.allowed as f64).ln();
                    let lb = (b.allowed as f64).ln();
                    let t = fa / (fa - fb);
                    crossover = Some((la + t * (lb - la)).exp());
                    break;
                }
            }
            let mut parameters = BTreeMap::new();
            parameters.insert("m".to_owned(), serde_json::json!(m));
            parameters.insert(
                "ef_construction".to_owned(),
                serde_json::json!(ef_construction),
            );
            parameters.insert("rescore".to_owned(), serde_json::json!((4 * k).max(100)));
            let report = FilteredReportV1 {
                report_type: "FilteredReportV1",
                version: 1,
                dataset: loaded.manifest.name.clone(),
                metric: loaded.metric.as_str().to_owned(),
                dims: loaded.base.dims(),
                base_count,
                queries_evaluated: count,
                k,
                ef_search: ef,
                parameters,
                levels: points,
                crossover_allowed: crossover,
                crossover_c: crossover.map(|x| x / k as f64),
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
        Command::Hnsw {
            dataset,
            k,
            ef,
            m,
            ef_construction,
            limit,
            queries,
            threads,
            sequential,
            modes,
            rescore,
            out,
        } => {
            let loaded = load(&dataset)?;
            if k > loaded.manifest.groundtruth_k {
                bail!(
                    "k={k} exceeds ground-truth depth {}",
                    loaded.manifest.groundtruth_k
                );
            }
            let base_count = limit.unwrap_or(loaded.base.len()).min(loaded.base.len());
            if base_count < loaded.base.len() {
                eprintln!(
                    "note: ground truth covers the full base; recall against a {base_count}-row prefix is a lower bound"
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
            let efs: Vec<usize> = ef
                .split(',')
                .map(|v| v.trim().parse::<usize>())
                .collect::<Result<_, _>>()
                .context("parsing --ef")?;

            let params = HnswParams {
                m,
                m0: 2 * m,
                ef_construction,
                seed: 0x5eed,
            };
            eprintln!(
                "building HNSW over {base_count} vectors (m={m}, ef_construction={ef_construction}, {})",
                if sequential { "sequential" } else { "parallel" }
            );
            let started = Instant::now();
            let index = if sequential {
                let mut index = Hnsw::new(loaded.base.dims(), loaded.metric, params);
                for slot in 0..base_count {
                    index.insert(loaded.base.get(slot as RecordId).unwrap())?;
                    if slot % 100_000 == 99_999 {
                        eprintln!(
                            "  {} inserted, {:.0} vectors/s",
                            slot + 1,
                            (slot + 1) as f64 / started.elapsed().as_secs_f64()
                        );
                    }
                }
                index
            } else {
                let mut subset = FlatVectors::with_capacity(loaded.base.dims(), base_count);
                for slot in 0..base_count {
                    // Rows were normalized on load for cosine; store as given.
                    subset.push(loaded.base.get(slot as RecordId).unwrap(), Metric::L2)?;
                }
                Hnsw::build_parallel(subset, loaded.metric, params)
            };
            let build_seconds = started.elapsed().as_secs_f64();
            let rss = rss_bytes();
            eprintln!(
                "built in {build_seconds:.1}s; max level {}",
                index.max_level()
            );

            let mode_list: Vec<SearchMode> = match modes.as_str() {
                "f32" => vec![SearchMode::F32],
                "int8" => vec![SearchMode::Int8 {
                    rescore: rescore.unwrap_or((4 * k).max(100)),
                }],
                "both" => vec![
                    SearchMode::F32,
                    SearchMode::Int8 {
                        rescore: rescore.unwrap_or((4 * k).max(100)),
                    },
                ],
                other => bail!("unknown --modes {other}"),
            };
            let mut points = Vec::new();
            for mode in mode_list {
                for &ef_search in &efs {
                    let sample = count.min(200);
                    let mut latencies = Vec::with_capacity(sample);
                    for index_q in 0..sample {
                        let query = loaded.queries.get(index_q as RecordId).unwrap();
                        let started = Instant::now();
                        let hits = index.search_with(query, k, ef_search, mode, None);
                        latencies.push(started.elapsed().as_secs_f64() * 1e3);
                        std::hint::black_box(hits);
                    }
                    let started = Instant::now();
                    let per_query: Vec<(f64, f64)> = (0..count)
                        .into_par_iter()
                        .map(|index_q| {
                            let query = loaded.queries.get(index_q as RecordId).unwrap();
                            let hits = index.search_with(query, k, ef_search, mode, None);
                            recalls(&loaded, index_q, query, &hits, k)
                        })
                        .collect();
                    let batch_seconds = started.elapsed().as_secs_f64();
                    let n = per_query.len().max(1) as f64;
                    let (mode_name, rescore_depth) = match mode {
                        SearchMode::F32 => ("f32".to_owned(), None),
                        SearchMode::Int8 { rescore } => ("int8".to_owned(), Some(rescore)),
                    };
                    points.push(EfPoint {
                        mode: mode_name.clone(),
                        rescore: rescore_depth,
                        ef_search,
                        recall_at_k: per_query.iter().map(|r| r.0).sum::<f64>() / n,
                        recall_min: per_query.iter().map(|r| r.0).fold(1.0, f64::min),
                        id_recall_at_k: per_query.iter().map(|r| r.1).sum::<f64>() / n,
                        latency_single_thread_ms: percentiles(latencies),
                        batch_queries_per_second: count as f64 / batch_seconds,
                        batch_threads: threads,
                    });
                    eprintln!(
                        "  {mode_name} ef={ef_search}: recall@{k}={:.4} p50={:.3}ms qps={:.0}",
                        points.last().unwrap().recall_at_k,
                        points.last().unwrap().latency_single_thread_ms.p50,
                        points.last().unwrap().batch_queries_per_second
                    );
                }
            }
            let (f32_bytes, int8_bytes, graph_bytes) = index.accounted_breakdown();

            let mut parameters = BTreeMap::new();
            parameters.insert("m".to_owned(), serde_json::json!(m));
            parameters.insert("m0".to_owned(), serde_json::json!(2 * m));
            parameters.insert(
                "ef_construction".to_owned(),
                serde_json::json!(ef_construction),
            );
            parameters.insert("seed".to_owned(), serde_json::json!(params.seed));
            parameters.insert(
                "build".to_owned(),
                serde_json::json!(if sequential { "sequential" } else { "parallel" }),
            );
            let report = HnswReportV1 {
                report_type: "HnswReportV1",
                version: 1,
                dataset: loaded.manifest.name.clone(),
                metric: loaded.metric.as_str().to_owned(),
                dims: loaded.base.dims(),
                base_count,
                queries_evaluated: count,
                k,
                parameters,
                build_seconds,
                build_vectors_per_second: base_count as f64 / build_seconds,
                accounted_bytes: index.accounted_bytes(),
                accounted_bytes_per_vector: index.accounted_bytes() as f64
                    / base_count.max(1) as f64,
                accounted_f32_bytes: f32_bytes,
                accounted_int8_bytes: int8_bytes,
                accounted_graph_bytes: graph_bytes,
                rss_bytes_after_build: rss,
                max_level: index.max_level(),
                points,
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

fn rss_bytes() -> u64 {
    fs::read_to_string("/proc/self/statm")
        .ok()
        .and_then(|s| {
            s.split_whitespace()
                .nth(1)
                .and_then(|v| v.parse::<u64>().ok())
        })
        .map(|pages| pages * 4096)
        .unwrap_or(0)
}

/// Distance-based and id-based recall for one query against the reference.
fn recalls(loaded: &Dataset, index: usize, query: &[f32], hits: &[Hit], k: usize) -> (f64, f64) {
    let truth = &loaded.groundtruth[index][..k];
    let kth = truth
        .iter()
        .map(|id| {
            let row = loaded.base.get(*id).expect("ground-truth id in range");
            distance(loaded.metric, query, row)
        })
        .fold(f32::MIN, f32::max);
    (recall_by_distance(kth, hits, k), recall(truth, hits))
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
