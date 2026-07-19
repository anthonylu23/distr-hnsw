use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::Serialize;

#[derive(Debug, Clone, Serialize)]
pub struct SideBySide {
    pub rank: usize,
    pub file_id: i64,
    pub name: String,
    pub path: String,
    pub score: f32,
}

#[derive(Debug, Clone, Serialize)]
pub struct QueryBreakdown {
    pub query_id: String,
    pub query_text: String,
    pub category: String,
    pub model: String,
    pub dims: usize,
    pub semantic: Vec<SideBySide>,
    pub name: Vec<SideBySide>,
    pub recency: Vec<SideBySide>,
    pub keyword_text: Vec<SideBySide>,
    pub semantic_recall_at_k: Option<f64>,
    pub semantic_ndcg_at_k: Option<f64>,
}

#[derive(Debug, Clone, Serialize)]
pub struct CategoryMetrics {
    pub category: String,
    pub judged_queries: u32,
    pub vs_name: BaselineWlt,
    pub vs_recency: BaselineWlt,
    pub vs_keyword: BaselineWlt,
    pub mean_recall_at_k: f64,
    pub mean_ndcg_at_k: f64,
}

#[derive(Debug, Clone, Serialize)]
pub struct BaselineWlt {
    pub wins: u32,
    pub losses: u32,
    pub ties: u32,
    pub win_rate: f64,
}

#[derive(Debug, Clone, Serialize)]
pub struct ConfigMetrics {
    pub model: String,
    pub dims: usize,
    pub judged_queries: u32,
    pub vs_name: BaselineWlt,
    pub vs_recency: BaselineWlt,
    pub vs_keyword: BaselineWlt,
    /// Backward-compatible aliases used in recommendation text / go gate.
    pub wins_vs_name: u32,
    pub losses_vs_name: u32,
    pub ties_vs_name: u32,
    pub win_rate_vs_name: f64,
    pub win_rate_vs_recency: f64,
    pub win_rate_vs_keyword: f64,
    pub mean_recall_at_k: f64,
    pub mean_ndcg_at_k: f64,
    /// Warm-query arithmetic mean retained for JSON consumers.
    pub mean_latency_ms: f64,
    /// Provider/model cold start, recorded once for the first config per model.
    pub cold_latency_ms: Option<f64>,
    pub warm_p50_latency_ms: f64,
    pub warm_p95_latency_ms: f64,
    pub categories: Vec<CategoryMetrics>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ModelProvenance {
    pub name: String,
    pub dims: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ollama_digest: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub current_ollama_digest: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Provenance {
    pub binary: String,
    pub source_revision: String,
    pub source_tree_blake3: Option<String>,
    pub executable_blake3: Option<String>,
    pub corpus_index_blake3: String,
    pub prepare_fingerprint: Option<String>,
    pub prepared_at: Option<String>,
    pub query_set_path: String,
    pub query_set_blake3: String,
    pub models: Vec<ModelProvenance>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Report {
    pub generated_at: String,
    pub k: usize,
    pub corpus: String,
    pub file_count: u64,
    pub chunk_count: u64,
    /// Fraction of files sharing the maximum mtime (recency collision).
    pub recency_max_mtime_fraction: f64,
    pub recency_at_max_mtime: i64,
    pub provenance: Provenance,
    pub configs: Vec<ConfigMetrics>,
    pub queries: Vec<QueryBreakdown>,
    pub recommendation: String,
    pub go_no_go: String,
    pub dims_locked: bool,
}

pub fn write_reports(work_dir: &Path, stem: &str, report: &Report) -> Result<(PathBuf, PathBuf)> {
    let reports_dir = work_dir.join("reports");
    std::fs::create_dir_all(&reports_dir)?;
    let md_path = reports_dir.join(format!("{stem}.md"));
    let html_path = reports_dir.join(format!("{stem}.html"));
    let json_path = reports_dir.join(format!("{stem}.json"));

    std::fs::write(&md_path, render_markdown(report))
        .with_context(|| format!("write {}", md_path.display()))?;
    std::fs::write(&html_path, render_html(report))
        .with_context(|| format!("write {}", html_path.display()))?;
    std::fs::write(&json_path, serde_json::to_string_pretty(report)?)
        .with_context(|| format!("write {}", json_path.display()))?;

    Ok((md_path, html_path))
}

pub fn render_markdown(report: &Report) -> String {
    let mut out = String::new();
    out.push_str("# Phase-0 validation report\n\n");
    out.push_str(&format!("- Generated: {}\n", report.generated_at));
    out.push_str(&format!("- Corpus: `{}`\n", report.corpus));
    out.push_str(&format!(
        "- Files / chunks: {} / {}\n",
        report.file_count, report.chunk_count
    ));
    out.push_str(&format!("- Top-k: {}\n", report.k));
    out.push_str(&format!(
        "- Recency mtime collision: {:.1}% of files share max mtime ({}/{})\n\n",
        report.recency_max_mtime_fraction * 100.0,
        report.recency_at_max_mtime,
        report.file_count
    ));

    out.push_str("## Provenance\n\n");
    out.push_str(&format!("- Binary: `{}`\n", report.provenance.binary));
    out.push_str(&format!(
        "- Source revision: `{}`\n",
        report.provenance.source_revision
    ));
    out.push_str(&format!(
        "- Source tree blake3: `{}`\n",
        report
            .provenance
            .source_tree_blake3
            .as_deref()
            .unwrap_or("(unavailable)")
    ));
    out.push_str(&format!(
        "- Executable blake3: `{}`\n",
        report
            .provenance
            .executable_blake3
            .as_deref()
            .unwrap_or("(unavailable)")
    ));
    out.push_str(&format!(
        "- Corpus index blake3: `{}`\n",
        report.provenance.corpus_index_blake3
    ));
    out.push_str(&format!(
        "- Prepare fingerprint: `{}`\n",
        report
            .provenance
            .prepare_fingerprint
            .as_deref()
            .unwrap_or("(none)")
    ));
    out.push_str(&format!(
        "- Prepared at: `{}`\n",
        report.provenance.prepared_at.as_deref().unwrap_or("(none)")
    ));
    out.push_str(&format!(
        "- Query set: `{}` (blake3 `{}`)\n",
        report.provenance.query_set_path, report.provenance.query_set_blake3
    ));
    out.push_str("- Models:\n");
    for m in &report.provenance.models {
        match &m.ollama_digest {
            Some(d) => out.push_str(&format!(
                "  - `{}` @ {}d embedded_digest=`{}` current_digest=`{}`\n",
                m.name,
                m.dims,
                d,
                m.current_ollama_digest
                    .as_deref()
                    .unwrap_or("(unavailable)")
            )),
            None => out.push_str(&format!("  - `{}` @ {}d\n", m.name, m.dims)),
        }
    }
    out.push('\n');

    out.push_str(&format!("## Verdict: **{}**\n\n", report.go_no_go));
    out.push_str(&format!("{}\n\n", report.recommendation));

    out.push_str("## Config summary\n\n");
    out.push_str(
        "| model | dims | judged | vs name | vs recency | vs keyword | mean recall | mean nDCG | cold ms | warm p50 ms | warm p95 ms |\n",
    );
    out.push_str("|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|\n");
    for c in &report.configs {
        out.push_str(&format!(
            "| `{}` | {} | {} | {:.1}% ({}/{}/{}) | {:.1}% ({}/{}/{}) | {:.1}% ({}/{}/{}) | {:.3} | {:.3} | {} | {:.0} | {:.0} |\n",
            c.model,
            c.dims,
            c.judged_queries,
            c.win_rate_vs_name * 100.0,
            c.vs_name.wins,
            c.vs_name.losses,
            c.vs_name.ties,
            c.win_rate_vs_recency * 100.0,
            c.vs_recency.wins,
            c.vs_recency.losses,
            c.vs_recency.ties,
            c.win_rate_vs_keyword * 100.0,
            c.vs_keyword.wins,
            c.vs_keyword.losses,
            c.vs_keyword.ties,
            c.mean_recall_at_k,
            c.mean_ndcg_at_k,
            c.cold_latency_ms
                .map(|v| format!("{v:.0}"))
                .unwrap_or_else(|| "—".into()),
            c.warm_p50_latency_ms,
            c.warm_p95_latency_ms
        ));
    }
    out.push('\n');

    out.push_str("## Category summary\n\n");
    out.push_str(
        "| model | dims | category | judged | vs name | vs recency | vs keyword | mean recall | mean nDCG |\n",
    );
    out.push_str("|---|---:|---|---:|---:|---:|---:|---:|---:|\n");
    for c in &report.configs {
        for category in &c.categories {
            out.push_str(&format!(
                "| `{}` | {} | `{}` | {} | {:.1}% ({}/{}/{}) | {:.1}% ({}/{}/{}) | {:.1}% ({}/{}/{}) | {:.3} | {:.3} |\n",
                c.model,
                c.dims,
                category.category,
                category.judged_queries,
                category.vs_name.win_rate * 100.0,
                category.vs_name.wins,
                category.vs_name.losses,
                category.vs_name.ties,
                category.vs_recency.win_rate * 100.0,
                category.vs_recency.wins,
                category.vs_recency.losses,
                category.vs_recency.ties,
                category.vs_keyword.win_rate * 100.0,
                category.vs_keyword.wins,
                category.vs_keyword.losses,
                category.vs_keyword.ties,
                category.mean_recall_at_k,
                category.mean_ndcg_at_k,
            ));
        }
    }
    out.push('\n');

    out.push_str("## Per-query side-by-side\n\n");
    for q in &report.queries {
        out.push_str(&format!(
            "### {} — `{}` @ {}d\n\nCategory: `{}`  \nQuery: {}\n\n",
            q.query_id, q.model, q.dims, q.category, q.query_text
        ));
        if let Some(r) = q.semantic_recall_at_k {
            out.push_str(&format!("Recall@{}: {:.3}  \n", report.k, r));
        }
        if let Some(n) = q.semantic_ndcg_at_k {
            out.push_str(&format!("nDCG@{}: {:.3}\n\n", report.k, n));
        }
        out.push_str("#### Semantic\n\n");
        out.push_str(&table_ranks(&q.semantic));
        out.push_str("\n#### Name baseline\n\n");
        out.push_str(&table_ranks(&q.name));
        out.push_str("\n#### Recency baseline\n\n");
        out.push_str(&table_ranks(&q.recency));
        out.push_str("\n#### Keyword text baseline\n\n");
        out.push_str(&table_ranks(&q.keyword_text));
        out.push('\n');
    }
    out
}

fn table_ranks(rows: &[SideBySide]) -> String {
    if rows.is_empty() {
        return "_no hits_\n".into();
    }
    let mut out = String::from("| rank | score | name | path |\n|---:|---:|---|---|\n");
    for r in rows {
        out.push_str(&format!(
            "| {} | {:.4} | `{}` | `{}` |\n",
            r.rank, r.score, r.name, r.path
        ));
    }
    out
}

pub fn render_html(report: &Report) -> String {
    let md_like = render_markdown(report);
    format!(
        "<!DOCTYPE html>\n<html><head><meta charset=\"utf-8\"><title>Phase-0 validation</title>\n\
         <style>body{{font-family:ui-sans-serif,system-ui,sans-serif;max-width:960px;margin:2rem auto;padding:0 1rem;line-height:1.45}}\n\
         table{{border-collapse:collapse;width:100%;margin:1rem 0}} th,td{{border:1px solid #ccc;padding:0.35rem 0.5rem;text-align:left}}\n\
         code{{font-family:ui-monospace,monospace}}</style></head><body>\n<pre style=\"white-space:pre-wrap;font-family:inherit\">{}</pre>\n</body></html>\n",
        html_escape(&md_like)
    )
}

fn html_escape(s: &str) -> String {
    s.chars()
        .map(|c| match c {
            '&' => "&amp;".into(),
            '<' => "&lt;".into(),
            '>' => "&gt;".into(),
            _ => c.to_string(),
        })
        .collect()
}
