mod chunk;
mod config;
mod db;
mod embed;
mod eval;
mod extract;
mod prepare;
mod report;
mod search;

use std::sync::Arc;

use anyhow::{Context, Result};
use clap::Parser;
use tokio::task::JoinSet;

use crate::config::{parse_model_dims, Cli, Command};
use crate::db::Db;
use crate::embed::EmbedClient;

type EmbedBatchResult = Result<(Vec<i64>, Vec<Vec<f32>>)>;

fn spawn_embed_batch(
    join_set: &mut JoinSet<EmbedBatchResult>,
    client: Arc<EmbedClient>,
    model: Arc<String>,
    prefix: Option<&'static str>,
    batch: Vec<(i64, String)>,
) {
    join_set.spawn(async move {
        let batch_context = batch
            .iter()
            .map(|(id, text)| format!("{id}({} chars)", text.chars().count()))
            .collect::<Vec<_>>()
            .join(", ");
        let texts: Vec<&str> = batch.iter().map(|(_, text)| text.as_str()).collect();
        let vectors = client
            .embed_batch_with_prefix(model.as_str(), &texts, prefix)
            .await
            .with_context(|| format!("embed chunk batch [{batch_context}]"))?;
        if vectors.len() != batch.len() {
            anyhow::bail!(
                "embedding count mismatch for chunk batch [{batch_context}]: got {} want {}",
                vectors.len(),
                batch.len()
            );
        }
        let ids = batch.iter().map(|(id, _)| *id).collect();
        Ok((ids, vectors))
    });
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    let work_dir = cli.resolve_work_dir()?;
    std::fs::create_dir_all(&work_dir)
        .with_context(|| format!("create work dir {}", work_dir.display()))?;

    match cli.command {
        Command::Prepare {
            corpus,
            max_file_bytes,
            chunk_chars,
            chunk_overlap,
            fresh,
        } => {
            if fresh {
                Db::wipe(&work_dir)?;
                println!("wiped existing validate.db under {}", work_dir.display());
            }
            let db = Db::open(&work_dir)?;
            println!("work_dir: {}", work_dir.display());
            println!("corpus:   {}", corpus.display());
            let stats =
                prepare::prepare_corpus(&db, &corpus, max_file_bytes, chunk_chars, chunk_overlap)?;
            println!(
                "prepare done: seen={} indexed={} skipped_unchanged={} excluded={} needs_ocr={} failed={} chunks={} pruned={}",
                stats.seen,
                stats.indexed,
                stats.skipped_unchanged,
                stats.excluded,
                stats.needs_ocr,
                stats.failed,
                stats.chunks,
                stats.pruned
            );
        }
        Command::Embed {
            model,
            dims,
            concurrency,
            batch_size,
            force,
        } => {
            let db = Db::open(&work_dir)?;
            let client = EmbedClient::new(&cli.ollama_url)?;
            client.ping().await.context("ollama unreachable")?;
            run_embed(&db, &client, &model, dims, concurrency, batch_size, force).await?;
        }
        Command::Query {
            text,
            model,
            dims,
            k,
        } => {
            let db = Db::open(&work_dir)?;
            let client = EmbedClient::new(&cli.ollama_url)?;
            client.ping().await.context("ollama unreachable")?;
            print_query(&db, &client, &text, &model, dims, k).await?;
        }
        Command::Eval {
            queries,
            models,
            k,
            out,
        } => {
            let db = Db::open(&work_dir)?;
            let client = EmbedClient::new(&cli.ollama_url)?;
            client.ping().await.context("ollama unreachable")?;
            let parsed: Vec<(String, usize)> = models
                .iter()
                .map(|s| parse_model_dims(s))
                .collect::<Result<_>>()?;
            let specs = eval::load_queries(&queries)?;
            let report = eval::run_eval(&db, &client, &queries, &specs, &parsed, k).await?;
            let stem =
                out.unwrap_or_else(|| chrono::Utc::now().format("eval-%Y%m%dT%H%M%SZ").to_string());
            let (md, html) = eval::write_reports(&work_dir, &stem, &report)?;
            println!("verdict: {}", report.go_no_go);
            println!("{}", report.recommendation);
            println!("wrote {}", md.display());
            println!("wrote {}", html.display());
            println!(
                "provenance: rev={} queries_blake3={}",
                report.provenance.source_revision, report.provenance.query_set_blake3
            );
        }
        Command::Status => {
            let db = Db::open(&work_dir)?;
            print_status(&db, &work_dir)?;
        }
    }
    Ok(())
}

async fn run_embed(
    db: &Db,
    client: &EmbedClient,
    model: &str,
    dims: usize,
    concurrency: usize,
    batch_size: usize,
    force: bool,
) -> Result<()> {
    let provider_digest = client.model_digest(model).await?;
    let existing_count = db.count_embeddings(model, dims)?;
    let stored_digest = db.embedding_config_digest(model, dims)?;
    if existing_count > 0 && stored_digest.as_deref() != Some(provider_digest.as_str()) && !force {
        let stored = stored_digest.as_deref().unwrap_or("(missing)");
        anyhow::bail!(
            "embedding provenance mismatch for model={model} dims={dims}: stored={stored} current={provider_digest}; rerun with --force"
        );
    }

    if force {
        let deleted = db.delete_embeddings_for_config(model, dims)?;
        if deleted > 0 {
            println!("cleared {deleted} existing vectors for model={model} dims={dims}");
        }
    }

    let chunks = db.chunks_needing_embed(model, dims, false)?;
    println!(
        "embedding {} chunks with model={} dims={} digest={} concurrency={} batch_size={}",
        chunks.len(),
        model,
        dims,
        provider_digest,
        concurrency,
        batch_size
    );
    if chunks.is_empty() {
        println!("nothing to do");
        return Ok(());
    }

    // Persist provider identity before the first vector so an interrupted run
    // can safely resume. Forced reruns cleared the old configuration above,
    // preventing a failed rebuild from leaving mixed-provider vectors.
    db.upsert_embedding_config(model, dims, &provider_digest)?;

    let concurrency = concurrency.max(1);
    let batch_size = batch_size.max(1);
    let total = chunks.len();

    let batches: Vec<Vec<(i64, String)>> = chunks
        .chunks(batch_size)
        .map(|batch| {
            batch
                .iter()
                .map(|c| (c.id, c.text.clone()))
                .collect::<Vec<_>>()
        })
        .collect();

    let client = Arc::new(client.clone());
    let model_owned = Arc::new(model.to_string());
    let prefix = embed::document_prefix(model);
    let mut batches = batches.into_iter();
    let mut join_set = JoinSet::new();

    for _ in 0..concurrency {
        let Some(batch) = batches.next() else {
            break;
        };
        spawn_embed_batch(
            &mut join_set,
            Arc::clone(&client),
            Arc::clone(&model_owned),
            prefix,
            batch,
        );
    }

    let mut done = 0usize;
    while let Some(joined) = join_set.join_next().await {
        let (ids, vectors) = joined.context("embed task join")??;
        let n = ids.len();
        for (chunk_id, vec) in ids.into_iter().zip(vectors) {
            let normalized = embed::truncate_and_normalize(vec, dims)?;
            let blob = embed::f32s_to_bytes(&normalized);
            db.upsert_embedding(chunk_id, model, dims, &blob)?;
        }
        done += n;
        println!("embedded {done}/{total}");

        if let Some(batch) = batches.next() {
            spawn_embed_batch(
                &mut join_set,
                Arc::clone(&client),
                Arc::clone(&model_owned),
                prefix,
                batch,
            );
        }
    }

    db.upsert_embedding_config(model, dims, &provider_digest)?;

    println!(
        "embed complete: {} vectors for model={} dims={}",
        db.count_embeddings(model, dims)?,
        model,
        dims
    );
    Ok(())
}

async fn print_query(
    db: &Db,
    client: &EmbedClient,
    text: &str,
    model: &str,
    dims: usize,
    k: usize,
) -> Result<()> {
    println!("query: {text}");
    println!("\n== semantic ({model} @ {dims}) ==");
    for r in search::semantic_search(db, client, model, dims, text, k).await? {
        println!("{:>7.4}  id={}  {}", r.score, r.file.id, r.file.name);
    }
    println!("\n== name ==");
    for r in search::name_search(db, text, k)? {
        println!("{:>7.4}  id={}  {}", r.score, r.file.id, r.file.name);
    }
    println!("\n== recency ==");
    for r in search::recency_search(db, k)? {
        println!("{:>7.0}  id={}  {}", r.score, r.file.id, r.file.name);
    }
    println!("\n== keyword text ==");
    for r in search::keyword_text_search(db, text, k)? {
        println!("{:>7.4}  id={}  {}", r.score, r.file.id, r.file.name);
    }
    Ok(())
}

fn print_status(db: &Db, work_dir: &std::path::Path) -> Result<()> {
    println!("work_dir: {}", work_dir.display());
    println!("db:       {}", db.path.display());
    if let Some(corpus) = db.get_meta("corpus_root")? {
        println!("corpus:   {corpus}");
    }
    if let Some(at) = db.get_meta("prepared_at")? {
        println!("prepared: {at}");
    }
    println!("files:    {}", db.count_files()?);
    println!("chunks:   {}", db.count_chunks()?);
    println!("extract status:");
    for (status, n) in db.status_by_extract()? {
        println!("  {status}: {n}");
    }
    println!("embeddings:");
    let configs = db.list_embedding_configs()?;
    if configs.is_empty() {
        println!("  (none)");
    } else {
        for (model, dims, n) in configs {
            let digest = db
                .embedding_config_digest(&model, dims)?
                .unwrap_or_else(|| "(missing provenance)".into());
            println!("  {model} @ {dims}: {n} digest={digest}");
        }
    }
    Ok(())
}
