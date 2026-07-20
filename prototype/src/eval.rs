use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::Deserialize;

use crate::db::Db;
use crate::embed::EmbedClient;
use crate::report::{
    self, BaselineWlt, CategoryMetrics, ConfigMetrics, ModelProvenance, Provenance, QueryBreakdown,
    Report, SideBySide,
};
use crate::search::{self, RankedFile};

const MIN_QUERIES_TO_LOCK_DIMS: u32 = 40;
const DIM_NDCG_EPS: f64 = 0.03;
const MAX_PRODUCT_DIMS: usize = 768;
const PREFERRED_LOCAL_MODEL: &str = "nomic-embed-text";
const PREFERRED_LOCAL_DIMS: usize = 512;

#[derive(Debug, Clone, Deserialize)]
pub struct QuerySpec {
    pub id: String,
    pub text: String,
    #[serde(default = "default_category")]
    pub category: String,
    #[serde(default)]
    pub relevant_file_ids: Vec<i64>,
    #[serde(default)]
    pub relevant_path_globs: Vec<String>,
    /// Optional graded relevance: path or file id string -> grade 1..=2 (or higher)
    #[serde(default)]
    pub grades: HashMap<String, u32>,
}

fn default_category() -> String {
    "uncategorized".into()
}

#[derive(Debug, Clone)]
struct Relevant {
    ids: HashSet<i64>,
    grades: HashMap<i64, u32>,
}

#[derive(Debug, Default, Clone)]
struct WltAccum {
    wins: u32,
    losses: u32,
    ties: u32,
}

impl WltAccum {
    fn record(&mut self, sem_hit: Option<usize>, base_hit: Option<usize>) {
        match (sem_hit, base_hit) {
            (Some(s), Some(n)) if s < n => self.wins += 1,
            (Some(_), None) => self.wins += 1,
            (Some(s), Some(n)) if s > n => self.losses += 1,
            (None, Some(_)) => self.losses += 1,
            _ => self.ties += 1,
        }
    }

    fn into_metrics(self) -> BaselineWlt {
        let decided = self.wins + self.losses;
        let win_rate = if decided == 0 {
            0.0
        } else {
            self.wins as f64 / decided as f64
        };
        BaselineWlt {
            wins: self.wins,
            losses: self.losses,
            ties: self.ties,
            win_rate,
        }
    }
}

#[derive(Debug, Default)]
struct CategoryAccum {
    judged: u32,
    vs_name: WltAccum,
    vs_recency: WltAccum,
    vs_keyword: WltAccum,
    recall_sum: f64,
    ndcg_sum: f64,
}

impl CategoryAccum {
    fn into_metrics(self, category: String) -> CategoryMetrics {
        let judged = self.judged;
        CategoryMetrics {
            category,
            judged_queries: judged,
            vs_name: self.vs_name.into_metrics(),
            vs_recency: self.vs_recency.into_metrics(),
            vs_keyword: self.vs_keyword.into_metrics(),
            mean_recall_at_k: if judged == 0 {
                0.0
            } else {
                self.recall_sum / judged as f64
            },
            mean_ndcg_at_k: if judged == 0 {
                0.0
            } else {
                self.ndcg_sum / judged as f64
            },
        }
    }
}

pub fn load_queries(path: &Path) -> Result<Vec<QuerySpec>> {
    let data = std::fs::read_to_string(path)
        .with_context(|| format!("read queries {}", path.display()))?;
    let queries: Vec<QuerySpec> =
        serde_json::from_str(&data).with_context(|| format!("parse {}", path.display()))?;
    anyhow::ensure!(!queries.is_empty(), "queries file is empty");
    Ok(queries)
}

pub async fn run_eval(
    db: &Db,
    client: &EmbedClient,
    queries_path: &Path,
    queries: &[QuerySpec],
    models: &[(String, usize)],
    k: usize,
) -> Result<Report> {
    let (mtime_frac, at_max, _total) = db.max_mtime_collision_fraction()?;
    let provenance = build_provenance(db, client, queries_path, models).await?;
    let mut report = Report {
        generated_at: chrono::Utc::now().to_rfc3339(),
        k,
        corpus: db.get_meta("corpus_root")?.unwrap_or_default(),
        file_count: db.count_files()? as u64,
        chunk_count: db.count_chunks()? as u64,
        recency_max_mtime_fraction: mtime_frac,
        recency_at_max_mtime: at_max,
        provenance,
        configs: Vec::new(),
        queries: Vec::new(),
        recommendation: String::new(),
        go_no_go: String::new(),
        dims_locked: false,
    };
    // Ollama can produce a different first-session ranking immediately after a
    // model reload. Run one complete, unreported sweep across every config and
    // query before collecting retrieval metrics. Only the first request per
    // model is retained as cold-start latency.
    let mut cold_latency_by_model: HashMap<String, f64> = HashMap::new();
    for (model, dims) in models {
        for q in queries {
            let t0 = std::time::Instant::now();
            search::semantic_search(db, client, model, *dims, &q.text, k).await?;
            if !cold_latency_by_model.contains_key(model) {
                cold_latency_by_model.insert(model.clone(), t0.elapsed().as_secs_f64() * 1000.0);
            }
        }
    }
    let mut cold_reported_models: HashSet<String> = HashSet::new();

    for (model, dims) in models {
        let mut vs_name = WltAccum::default();
        let mut vs_recency = WltAccum::default();
        let mut vs_keyword = WltAccum::default();
        let mut recall_sum = 0f64;
        let mut ndcg_sum = 0f64;
        let mut judged = 0u32;
        let mut latencies = Vec::new();
        let mut category_accums: BTreeMap<String, CategoryAccum> = BTreeMap::new();

        let cold_latency_ms = if cold_reported_models.insert(model.clone()) {
            cold_latency_by_model.get(model).copied()
        } else {
            None
        };

        for q in queries {
            let relevant = resolve_relevant(db, q)?;
            let t0 = std::time::Instant::now();
            let semantic = search::semantic_search(db, client, model, *dims, &q.text, k).await?;
            latencies.push(t0.elapsed().as_secs_f64() * 1000.0);

            let name = search::name_search(db, &q.text, k)?;
            let recency = search::recency_search(db, k)?;
            let keyword = search::keyword_text_search(db, &q.text, k)?;

            let sem_ids: Vec<i64> = semantic.iter().map(|r| r.file.id).collect();
            let name_ids: Vec<i64> = name.iter().map(|r| r.file.id).collect();
            let recency_ids: Vec<i64> = recency.iter().map(|r| r.file.id).collect();
            let keyword_ids: Vec<i64> = keyword.iter().map(|r| r.file.id).collect();

            if !relevant.ids.is_empty() {
                judged += 1;
                let sem_hit = first_hit_rank(&sem_ids, &relevant.ids);
                let name_hit = first_hit_rank(&name_ids, &relevant.ids);
                let recency_hit = first_hit_rank(&recency_ids, &relevant.ids);
                let keyword_hit = first_hit_rank(&keyword_ids, &relevant.ids);
                let recall = recall_at_k(&sem_ids, &relevant.ids, k);
                let ndcg = ndcg_at_k(&sem_ids, &relevant.grades, k);
                vs_name.record(sem_hit, name_hit);
                vs_recency.record(sem_hit, recency_hit);
                vs_keyword.record(sem_hit, keyword_hit);
                recall_sum += recall;
                ndcg_sum += ndcg;

                let category = normalized_category(&q.category);
                let category_accum = category_accums.entry(category).or_default();
                category_accum.judged += 1;
                category_accum.vs_name.record(sem_hit, name_hit);
                category_accum.vs_recency.record(sem_hit, recency_hit);
                category_accum.vs_keyword.record(sem_hit, keyword_hit);
                category_accum.recall_sum += recall;
                category_accum.ndcg_sum += ndcg;
            }

            report.queries.push(QueryBreakdown {
                query_id: q.id.clone(),
                query_text: q.text.clone(),
                category: normalized_category(&q.category),
                model: model.clone(),
                dims: *dims,
                semantic: side_by_side(&semantic),
                name: side_by_side(&name),
                recency: side_by_side(&recency),
                keyword_text: side_by_side(&keyword),
                semantic_recall_at_k: if relevant.ids.is_empty() {
                    None
                } else {
                    Some(recall_at_k(&sem_ids, &relevant.ids, k))
                },
                semantic_ndcg_at_k: if relevant.grades.is_empty() && relevant.ids.is_empty() {
                    None
                } else {
                    Some(ndcg_at_k(
                        &sem_ids,
                        &if relevant.grades.is_empty() {
                            relevant.ids.iter().map(|id| (*id, 1u32)).collect()
                        } else {
                            relevant.grades.clone()
                        },
                        k,
                    ))
                },
            });
        }

        let name_m = vs_name.into_metrics();
        let recency_m = vs_recency.into_metrics();
        let keyword_m = vs_keyword.into_metrics();
        let mean_latency = if latencies.is_empty() {
            0.0
        } else {
            latencies.iter().sum::<f64>() / latencies.len() as f64
        };
        let warm_p50_latency_ms = percentile(&latencies, 0.50);
        let warm_p95_latency_ms = percentile(&latencies, 0.95);

        report.configs.push(ConfigMetrics {
            model: model.clone(),
            dims: *dims,
            judged_queries: judged,
            wins_vs_name: name_m.wins,
            losses_vs_name: name_m.losses,
            ties_vs_name: name_m.ties,
            win_rate_vs_name: name_m.win_rate,
            win_rate_vs_recency: recency_m.win_rate,
            win_rate_vs_keyword: keyword_m.win_rate,
            vs_name: name_m,
            vs_recency: recency_m,
            vs_keyword: keyword_m,
            mean_recall_at_k: if judged == 0 {
                0.0
            } else {
                recall_sum / judged as f64
            },
            mean_ndcg_at_k: if judged == 0 {
                0.0
            } else {
                ndcg_sum / judged as f64
            },
            mean_latency_ms: mean_latency,
            cold_latency_ms,
            warm_p50_latency_ms,
            warm_p95_latency_ms,
            categories: category_accums
                .into_iter()
                .map(|(category, accum)| accum.into_metrics(category))
                .collect(),
        });
    }

    let (rec, go, dims_locked) = recommend(&report.configs);
    report.recommendation = rec;
    report.go_no_go = go;
    report.dims_locked = dims_locked;
    Ok(report)
}

fn normalized_category(category: &str) -> String {
    let category = category.trim();
    if category.is_empty() {
        default_category()
    } else {
        category.to_string()
    }
}

fn side_by_side(rows: &[RankedFile]) -> Vec<SideBySide> {
    rows.iter()
        .enumerate()
        .map(|(i, r)| SideBySide {
            rank: i + 1,
            file_id: r.file.id,
            name: r.file.name.clone(),
            path: r.file.path.clone(),
            score: r.score,
        })
        .collect()
}

async fn build_provenance(
    db: &Db,
    client: &EmbedClient,
    queries_path: &Path,
    models: &[(String, usize)],
) -> Result<Provenance> {
    let query_bytes = std::fs::read(queries_path)
        .with_context(|| format!("hash queries {}", queries_path.display()))?;
    let query_set_blake3 = blake3::hash(&query_bytes).to_hex().to_string();

    let current_digests = client.model_digests().await?;
    let mut model_prov = Vec::with_capacity(models.len());
    for (name, dims) in models {
        let current_digest = current_digests
            .get(name)
            .cloned()
            .or_else(|| current_digests.get(&format!("{name}:latest")).cloned())
            .with_context(|| format!("current Ollama digest missing for model={name}"))?;
        let embedded_digest = db.embedding_config_digest(name, *dims)?.with_context(|| {
            format!(
                "embedding provenance missing for model={name} dims={dims}; rerun embed --force"
            )
        })?;
        anyhow::ensure!(
            embedded_digest == current_digest,
            "embedding/query model digest mismatch for model={name} dims={dims}: embedded={embedded_digest} current={current_digest}; rerun embed --force"
        );
        model_prov.push(ModelProvenance {
            name: name.clone(),
            dims: *dims,
            ollama_digest: Some(embedded_digest),
            current_ollama_digest: Some(current_digest),
        });
    }

    Ok(Provenance {
        binary: format!("distr-hnsw-validate {}", env!("CARGO_PKG_VERSION")),
        source_revision: git_head_revision(),
        source_tree_blake3: source_tree_blake3().ok(),
        executable_blake3: executable_blake3().ok(),
        corpus_index_blake3: corpus_index_blake3(db)?,
        prepare_fingerprint: db.get_meta("prepare_fingerprint")?,
        prepared_at: db.get_meta("prepared_at")?,
        query_set_path: queries_path.display().to_string(),
        query_set_blake3,
        models: model_prov,
    })
}

fn corpus_index_blake3(db: &Db) -> Result<String> {
    let corpus_root = db.get_meta("corpus_root")?.unwrap_or_default();
    let root = Path::new(&corpus_root);
    let mut stmt = db.conn.prepare(
        "SELECT path, content_hash, extract_status, mtime, size FROM files ORDER BY path",
    )?;
    let rows = stmt.query_map([], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, String>(2)?,
            row.get::<_, i64>(3)?,
            row.get::<_, i64>(4)?,
        ))
    })?;
    let mut hasher = blake3::Hasher::new();
    for row in rows {
        let (path, content_hash, extract_status, mtime, size) = row?;
        let path = Path::new(&path);
        let relative = path.strip_prefix(root).unwrap_or(path);
        hasher.update(relative.to_string_lossy().as_bytes());
        hasher.update(&[0]);
        hasher.update(content_hash.as_bytes());
        hasher.update(&[0]);
        hasher.update(extract_status.as_bytes());
        hasher.update(&[0]);
        hasher.update(&mtime.to_le_bytes());
        hasher.update(&size.to_le_bytes());
    }
    Ok(hasher.finalize().to_hex().to_string())
}

fn git_head_revision() -> String {
    if let Some(revision) = option_env!("DISTR_HNSW_SOURCE_REVISION")
        .map(str::trim)
        .filter(|revision| !revision.is_empty() && *revision != "unknown")
    {
        return revision.to_string();
    }

    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let workspace_dir = manifest_dir.parent().unwrap_or(&manifest_dir);
    let head = std::process::Command::new("git")
        .current_dir(workspace_dir)
        .args(["rev-parse", "HEAD"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());
    let Some(mut head) = head else {
        return "unknown".into();
    };
    let dirty = std::process::Command::new("git")
        .current_dir(workspace_dir)
        .args(["status", "--porcelain", "--untracked-files=all"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .is_some_and(|o| !o.stdout.is_empty());
    if dirty {
        head.push_str("-dirty");
    }
    head
}

fn source_tree_blake3() -> Result<String> {
    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let workspace_dir = manifest_dir
        .parent()
        .context("prototype has no workspace parent")?;
    let mut files = vec![
        workspace_dir.join("Cargo.toml"),
        workspace_dir.join("Cargo.lock"),
        manifest_dir.join("Cargo.toml"),
    ];
    for entry in walkdir::WalkDir::new(manifest_dir.join("src"))
        .follow_links(false)
        .into_iter()
    {
        let entry = entry?;
        if entry.file_type().is_file() {
            files.push(entry.into_path());
        }
    }
    files.sort();

    let mut hasher = blake3::Hasher::new();
    for path in files {
        let rel = path.strip_prefix(workspace_dir).unwrap_or(&path);
        hasher.update(rel.to_string_lossy().as_bytes());
        hasher.update(&[0]);
        hasher.update(&std::fs::read(&path).with_context(|| format!("hash {}", path.display()))?);
        hasher.update(&[0]);
    }
    Ok(hasher.finalize().to_hex().to_string())
}

fn executable_blake3() -> Result<String> {
    let exe = std::env::current_exe().context("resolve current executable")?;
    let bytes = std::fs::read(&exe).with_context(|| format!("hash {}", exe.display()))?;
    Ok(blake3::hash(&bytes).to_hex().to_string())
}

fn percentile(values: &[f64], quantile: f64) -> f64 {
    if values.is_empty() {
        return 0.0;
    }
    let mut sorted = values.to_vec();
    sorted.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let index = ((sorted.len() - 1) as f64 * quantile.clamp(0.0, 1.0)).round() as usize;
    sorted[index]
}

fn resolve_relevant(db: &Db, q: &QuerySpec) -> Result<Relevant> {
    let mut ids: HashSet<i64> = q.relevant_file_ids.iter().copied().collect();
    for glob in &q.relevant_path_globs {
        let pattern = glob_to_like(glob);
        let mut stmt = db
            .conn
            .prepare("SELECT id FROM files WHERE path LIKE ?1 ESCAPE '\\'")?;
        let rows = stmt.query_map(rusqlite::params![pattern], |row| row.get::<_, i64>(0))?;
        for r in rows {
            ids.insert(r?);
        }
    }
    let mut grades: HashMap<i64, u32> = HashMap::new();
    for (key, grade) in &q.grades {
        if let Ok(id) = key.parse::<i64>() {
            grades.insert(id, *grade);
            ids.insert(id);
        } else if Path::new(key).is_absolute() {
            let file = db
                .file_by_path(key)?
                .with_context(|| format!("grade path does not exist for query {}: {key}", q.id))?;
            grades.insert(file.id, *grade);
            ids.insert(file.id);
        } else {
            let mut stmt = db.conn.prepare("SELECT id, path FROM files ORDER BY id")?;
            let rows = stmt.query_map([], |row| {
                Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?))
            })?;
            let mut matched = 0usize;
            for r in rows {
                let (id, path) = r?;
                if ids.contains(&id) && grade_selector_matches(key, &path) {
                    grades.insert(id, *grade);
                    matched += 1;
                }
            }
            anyhow::ensure!(
                matched > 0,
                "grade selector matched no declared relevant files for query {}: {key}",
                q.id
            );
        }
    }
    for id in &ids {
        grades.entry(*id).or_insert(1);
    }
    Ok(Relevant { ids, grades })
}

fn grade_selector_matches(selector: &str, path: &str) -> bool {
    let selector = selector.to_ascii_lowercase();
    let path = path.to_ascii_lowercase();
    path == selector
        || path.ends_with(&format!("/{selector}"))
        || path.contains(&format!("/{selector}/"))
        || Path::new(&path).file_stem().and_then(|stem| stem.to_str()) == Some(selector.as_str())
}

fn glob_to_like(glob: &str) -> String {
    let mut out = String::new();
    for c in glob.chars() {
        match c {
            '*' => out.push('%'),
            '?' => out.push('_'),
            '%' | '_' | '\\' => {
                out.push('\\');
                out.push(c);
            }
            other => out.push(other),
        }
    }
    out
}

fn first_hit_rank(ranked: &[i64], relevant: &HashSet<i64>) -> Option<usize> {
    ranked.iter().position(|id| relevant.contains(id))
}

pub fn recall_at_k(ranked: &[i64], relevant: &HashSet<i64>, k: usize) -> f64 {
    if relevant.is_empty() {
        return 0.0;
    }
    let hit = ranked
        .iter()
        .take(k)
        .filter(|id| relevant.contains(id))
        .count();
    hit as f64 / relevant.len() as f64
}

pub fn ndcg_at_k(ranked: &[i64], grades: &HashMap<i64, u32>, k: usize) -> f64 {
    if grades.is_empty() {
        return 0.0;
    }
    let mut dcg = 0f64;
    for (i, id) in ranked.iter().take(k).enumerate() {
        let rel = *grades.get(id).unwrap_or(&0) as f64;
        if rel > 0.0 {
            dcg += (2f64.powf(rel) - 1.0) / ((i as f64 + 2.0).log2());
        }
    }
    let mut ideal: Vec<u32> = grades.values().copied().collect();
    ideal.sort_by(|a, b| b.cmp(a));
    let mut idcg = 0f64;
    for (i, rel) in ideal.into_iter().take(k).enumerate() {
        if rel > 0 {
            idcg += (2f64.powf(rel as f64) - 1.0) / ((i as f64 + 2.0).log2());
        }
    }
    if idcg == 0.0 {
        0.0
    } else {
        dcg / idcg
    }
}

fn recommend(configs: &[ConfigMetrics]) -> (String, String, bool) {
    if configs.is_empty() {
        return (
            "No configs evaluated.".into(),
            "no-go (no configs); dims not locked".into(),
            false,
        );
    }
    // Configurations above the product cap are useful diagnostics, but cannot
    // choose a model, influence dimensionality confidence, or produce a go.
    let eligible: Vec<&ConfigMetrics> = configs
        .iter()
        .filter(|config| config.dims <= MAX_PRODUCT_DIMS)
        .collect();
    let Some(mut best) = eligible.first().copied() else {
        return (
            "No eligible configs at or below 768 dimensions; oversized configs are diagnostic only."
                .into(),
            "no-go (no configs satisfy product dimension cap); dims not locked".into(),
            false,
        );
    };
    for candidate in eligible.iter().copied().skip(1) {
        let better = candidate.mean_ndcg_at_k > best.mean_ndcg_at_k + 1e-9
            || ((candidate.mean_ndcg_at_k - best.mean_ndcg_at_k).abs() < 1e-9
                && candidate.win_rate_vs_name > best.win_rate_vs_name)
            || ((candidate.mean_ndcg_at_k - best.mean_ndcg_at_k).abs() < 1e-9
                && (candidate.win_rate_vs_name - best.win_rate_vs_name).abs() < 1e-9
                && candidate.dims < best.dims);
        if better {
            best = candidate;
        }
    }

    // Prefer 512 when it is within 0.03 nDCG of the best eligible result for
    // the selected model.
    let mut chosen = best;
    if let Some(c512) = eligible
        .iter()
        .copied()
        .find(|config| config.model == best.model && config.dims == 512)
    {
        if best.mean_ndcg_at_k - c512.mean_ndcg_at_k <= DIM_NDCG_EPS {
            chosen = c512;
        }
    }

    let model_dims: Vec<&ConfigMetrics> = eligible
        .iter()
        .copied()
        .filter(|config| config.model == chosen.model)
        .collect();
    let (min_ndcg, max_ndcg) = model_dims
        .iter()
        .fold((f64::INFINITY, f64::NEG_INFINITY), |(mn, mx), c| {
            (mn.min(c.mean_ndcg_at_k), mx.max(c.mean_ndcg_at_k))
        });
    let dims_spread = if model_dims.is_empty() {
        0.0
    } else {
        max_ndcg - min_ndcg
    };
    // The phase-0 policy predeclares nomic@512 as the capacity/quality
    // tie-break when at least two eligible dimensions were evaluated and it
    // is non-inferior to the best result. This is not evidence that 512d is
    // statistically superior; it makes the existing selection preference a
    // lockable decision once the independent sample is large enough.
    let preferred_noninferior = model_dims.len() >= 2
        && chosen.model == PREFERRED_LOCAL_MODEL
        && chosen.dims == PREFERRED_LOCAL_DIMS
        && max_ndcg - chosen.mean_ndcg_at_k <= DIM_NDCG_EPS;
    let dims_inconclusive = chosen.judged_queries < MIN_QUERIES_TO_LOCK_DIMS
        || (dims_spread < DIM_NDCG_EPS && !preferred_noninferior);

    let mut rec = format!(
        "Suggested model=`{}` dims={} (win-rate vs name={:.1}%, vs recency={:.1}%, mean nDCG={:.3}, warm latency p50/p95={:.0}/{:.0}ms)",
        chosen.model,
        chosen.dims,
        chosen.win_rate_vs_name * 100.0,
        chosen.win_rate_vs_recency * 100.0,
        chosen.mean_ndcg_at_k,
        chosen.warm_p50_latency_ms,
        chosen.warm_p95_latency_ms
    );
    if dims_inconclusive {
        rec.push_str(&format!(
            ". dims inconclusive — do not lock (judged={}, nDCG spread across dims={:.3})",
            chosen.judged_queries, dims_spread
        ));
    } else if preferred_noninferior {
        rec.push_str(&format!(
            ". dims locked by documented non-inferiority tie-break (512d within {:.3} nDCG of the best eligible dimension; observed gap={:.3})",
            DIM_NDCG_EPS,
            max_ndcg - chosen.mean_ndcg_at_k
        ));
    }

    let decided = chosen.wins_vs_name + chosen.losses_vs_name;
    let (mut go, is_go) = if chosen.judged_queries == 0 {
        (
            "no-go (no judged queries; add relevance labels)".to_string(),
            false,
        )
    } else if decided == 0 && chosen.mean_ndcg_at_k >= 0.9 {
        (
            "go (inconclusive vs name: all ties; strong nDCG)".to_string(),
            true,
        )
    } else if chosen.win_rate_vs_name >= 0.60 {
        ("go".to_string(), true)
    } else {
        (
            "no-go (semantic win-rate vs name baseline below 60%)".to_string(),
            false,
        )
    };
    let dims_locked = is_go && !dims_inconclusive;
    if !is_go && !dims_inconclusive {
        rec.push_str(". product no-go — do not lock dims");
    }
    if !dims_locked {
        go.push_str("; dims not locked");
    }

    (rec, go, dims_locked)
}

pub fn write_reports(
    work_dir: &Path,
    stem: &str,
    report: &Report,
) -> Result<(std::path::PathBuf, std::path::PathBuf)> {
    report::write_reports(work_dir, stem, report)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(model: &str, dims: usize, ndcg: f64, win: f64, judged: u32) -> ConfigMetrics {
        let wlt = BaselineWlt {
            wins: (win * 10.0) as u32,
            losses: ((1.0 - win) * 10.0) as u32,
            ties: 0,
            win_rate: win,
        };
        ConfigMetrics {
            model: model.into(),
            dims,
            judged_queries: judged,
            vs_name: wlt.clone(),
            vs_recency: wlt.clone(),
            vs_keyword: wlt.clone(),
            wins_vs_name: wlt.wins,
            losses_vs_name: wlt.losses,
            ties_vs_name: wlt.ties,
            win_rate_vs_name: win,
            win_rate_vs_recency: win,
            win_rate_vs_keyword: win,
            mean_recall_at_k: ndcg,
            mean_ndcg_at_k: ndcg,
            mean_latency_ms: 10.0,
            cold_latency_ms: None,
            warm_p50_latency_ms: 9.0,
            warm_p95_latency_ms: 12.0,
            categories: Vec::new(),
        }
    }

    #[test]
    fn recall_and_ndcg_basic() {
        let relevant = HashSet::from([1, 2]);
        let ranked = vec![3, 1, 4, 2];
        assert!((recall_at_k(&ranked, &relevant, 2) - 0.5).abs() < 1e-9);
        assert!((recall_at_k(&ranked, &relevant, 4) - 1.0).abs() < 1e-9);

        let grades = HashMap::from([(1, 2u32), (2, 1u32)]);
        let ndcg = ndcg_at_k(&ranked, &grades, 4);
        assert!(ndcg > 0.0 && ndcg <= 1.0);
    }

    #[test]
    fn percentile_uses_sorted_nearest_rank() {
        let values = vec![20.0, 10.0, 50.0, 30.0, 40.0];
        assert_eq!(percentile(&values, 0.50), 30.0);
        assert_eq!(percentile(&values, 0.95), 50.0);
        assert_eq!(percentile(&[], 0.50), 0.0);
    }

    #[test]
    fn prefer_512_when_within_eps_of_384_best() {
        let configs = vec![
            cfg("nomic-embed-text", 768, 0.680, 0.7, 50),
            cfg("nomic-embed-text", 512, 0.685, 0.7, 50),
            cfg("nomic-embed-text", 384, 0.693, 0.7, 50),
        ];
        let (rec, go, locked) = recommend(&configs);
        assert!(rec.contains("dims=512"), "expected prefer 512, got: {rec}");
        assert!(rec.contains("dims locked by documented non-inferiority tie-break"));
        assert!(rec.contains("observed gap=0.008"), "got: {rec}");
        assert_eq!(go, "go");
        assert!(locked);
    }

    #[test]
    fn low_query_count_marks_dims_inconclusive() {
        let configs = vec![
            cfg("nomic-embed-text", 768, 0.690, 0.7, 10),
            cfg("nomic-embed-text", 512, 0.685, 0.7, 10),
            cfg("nomic-embed-text", 384, 0.693, 0.7, 10),
        ];
        let (rec, go, locked) = recommend(&configs);
        assert!(rec.contains("dims=512"), "got: {rec}");
        assert!(rec.contains("dims inconclusive"));
        assert!(go.contains("dims not locked"));
        assert!(!locked);
    }

    #[test]
    fn no_go_prevents_lock_even_when_dims_are_distinguishable() {
        let configs = vec![
            cfg("nomic-embed-text", 768, 0.90, 0.5, 50),
            cfg("nomic-embed-text", 512, 0.80, 0.5, 50),
        ];
        let (rec, go, locked) = recommend(&configs);
        assert!(rec.contains("product no-go — do not lock dims"));
        assert!(go.ends_with("; dims not locked"));
        assert!(!locked);
    }

    #[test]
    fn strong_all_ties_go_can_lock_distinguishable_dims() {
        let mut configs = vec![
            cfg("nomic-embed-text", 768, 0.95, 0.0, 50),
            cfg("nomic-embed-text", 512, 0.80, 0.0, 50),
        ];
        for config in &mut configs {
            config.wins_vs_name = 0;
            config.losses_vs_name = 0;
            config.ties_vs_name = 50;
            config.vs_name = BaselineWlt {
                wins: 0,
                losses: 0,
                ties: 50,
                win_rate: 0.0,
            };
        }
        let (rec, go, locked) = recommend(&configs);
        assert!(rec.contains("dims=768"), "got: {rec}");
        assert!(go.starts_with("go (inconclusive vs name"), "got: {go}");
        assert!(locked);
    }

    #[test]
    fn selects_best_eligible_config_across_models() {
        let configs = vec![
            cfg("bge-m3", 1024, 0.95, 0.9, 50),
            cfg("bge-m3", 768, 0.60, 0.3, 50),
            cfg("nomic-embed-text", 768, 0.80, 0.7, 50),
        ];
        let (rec, go, locked) = recommend(&configs);
        assert!(
            rec.contains("model=`nomic-embed-text` dims=768"),
            "expected globally best eligible config, got: {rec}"
        );
        assert!(go.starts_with("go"));
        assert!(!locked, "one eligible dimension is not enough to lock dims");
    }

    #[test]
    fn preferred_dimension_requires_an_eligible_comparison() {
        let configs = vec![cfg("nomic-embed-text", 512, 0.80, 0.7, 50)];
        let (rec, go, locked) = recommend(&configs);
        assert!(rec.contains("dims=512"), "got: {rec}");
        assert!(rec.contains("dims inconclusive"), "got: {rec}");
        assert!(go.contains("dims not locked"), "got: {go}");
        assert!(!locked);
    }

    #[test]
    fn oversize_config_does_not_make_eligible_dim_spread_conclusive() {
        let configs = vec![
            cfg("bge-m3", 1024, 0.95, 0.7, 50),
            cfg("bge-m3", 768, 0.80, 0.7, 50),
            cfg("bge-m3", 512, 0.79, 0.7, 50),
        ];
        let (rec, go, locked) = recommend(&configs);
        assert!(rec.contains("dims=512"), "got: {rec}");
        assert!(rec.contains("dims inconclusive"), "got: {rec}");
        assert!(rec.contains("nDCG spread across dims=0.010"), "got: {rec}");
        assert!(go.starts_with("go"));
        assert!(go.contains("dims not locked"));
        assert!(!locked);
    }

    #[test]
    fn oversize_only_matrix_is_an_explicit_no_go() {
        let configs = vec![cfg("bge-m3", 1024, 0.95, 0.9, 50)];
        let (rec, go, locked) = recommend(&configs);
        assert_eq!(
            rec,
            "No eligible configs at or below 768 dimensions; oversized configs are diagnostic only."
        );
        assert_eq!(
            go,
            "no-go (no configs satisfy product dimension cap); dims not locked"
        );
        assert!(!locked);
    }

    #[test]
    fn relative_grade_selector_only_grades_declared_relevance() {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open(dir.path()).unwrap();
        let intended = db
            .upsert_file(
                "/corpus/a/lecture.pdf",
                "lecture.pdf",
                0,
                1,
                "hash-a",
                "extracted",
                Some("a"),
                None,
            )
            .unwrap();
        let unintended = db
            .upsert_file(
                "/corpus/b/lecture.pdf",
                "lecture.pdf",
                0,
                1,
                "hash-b",
                "extracted",
                Some("b"),
                None,
            )
            .unwrap();
        let query = QuerySpec {
            id: "q".into(),
            text: "lecture".into(),
            category: "pdf".into(),
            relevant_file_ids: vec![intended],
            relevant_path_globs: Vec::new(),
            grades: HashMap::from([("lecture.pdf".into(), 2)]),
        };
        let relevant = resolve_relevant(&db, &query).unwrap();
        assert_eq!(relevant.ids, HashSet::from([intended]));
        assert_eq!(relevant.grades, HashMap::from([(intended, 2)]));
        assert!(!relevant.ids.contains(&unintended));
    }

    #[test]
    fn unmatched_relative_grade_selector_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open(dir.path()).unwrap();
        let intended = db
            .upsert_file(
                "/corpus/a/lecture.pdf",
                "lecture.pdf",
                0,
                1,
                "hash-a",
                "extracted",
                Some("a"),
                None,
            )
            .unwrap();
        let query = QuerySpec {
            id: "q".into(),
            text: "lecture".into(),
            category: default_category(),
            relevant_file_ids: vec![intended],
            relevant_path_globs: Vec::new(),
            grades: HashMap::from([("missing.pdf".into(), 2)]),
        };
        let error = resolve_relevant(&db, &query).unwrap_err();
        assert!(error.to_string().contains("matched no declared relevant"));
    }

    #[test]
    fn filename_stem_grade_selector_grades_declared_relevance() {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open(dir.path()).unwrap();
        let intended = db
            .upsert_file(
                "/corpus/notes/01-Pointers.md",
                "01-Pointers.md",
                0,
                1,
                "hash-a",
                "extracted",
                Some("a"),
                None,
            )
            .unwrap();
        let query = QuerySpec {
            id: "q".into(),
            text: "pointers".into(),
            category: "study-notes".into(),
            relevant_file_ids: vec![intended],
            relevant_path_globs: Vec::new(),
            grades: HashMap::from([("01-pointers".into(), 2)]),
        };
        let relevant = resolve_relevant(&db, &query).unwrap();
        assert_eq!(relevant.ids, HashSet::from([intended]));
        assert_eq!(relevant.grades, HashMap::from([(intended, 2)]));
    }

    #[test]
    fn corpus_index_hash_captures_recency_input() {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open(dir.path()).unwrap();
        db.set_meta("corpus_root", "/corpus").unwrap();
        db.upsert_file(
            "/corpus/note.md",
            "note.md",
            1,
            10,
            "same-content",
            "extracted",
            Some("same text"),
            None,
        )
        .unwrap();
        let before = corpus_index_blake3(&db).unwrap();
        db.upsert_file(
            "/corpus/note.md",
            "note.md",
            2,
            10,
            "same-content",
            "extracted",
            Some("same text"),
            None,
        )
        .unwrap();
        let after = corpus_index_blake3(&db).unwrap();
        assert_ne!(before, after);
    }
}
