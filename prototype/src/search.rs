use std::collections::HashMap;

use anyhow::{Context, Result};
use rusqlite::{params, Connection};

use crate::db::{Db, FileRow};
use crate::embed::{self, EmbedClient};

#[derive(Debug, Clone)]
pub struct RankedFile {
    pub file: FileRow,
    pub score: f32,
}

pub async fn semantic_search(
    db: &Db,
    client: &EmbedClient,
    model: &str,
    dims: usize,
    query: &str,
    k: usize,
) -> Result<Vec<RankedFile>> {
    let mut qvecs = client
        .embed_batch_with_prefix(model, &[query], embed::query_prefix(model))
        .await?;
    let mut q = qvecs
        .pop()
        .context("embedding provider returned no vector")?;
    q = embed::truncate_and_normalize(q, dims)?;

    let embeddings = db.load_embeddings(model, dims)?;
    if embeddings.is_empty() {
        anyhow::bail!("no embeddings for model={model} dims={dims}; run embed first");
    }

    let mut best: HashMap<i64, f32> = HashMap::new();
    for (_chunk_id, file_id, vec) in embeddings {
        if vec.len() != dims {
            continue;
        }
        let score = embed::cosine(&q, &vec);
        best.entry(file_id)
            .and_modify(|s| {
                if score > *s {
                    *s = score;
                }
            })
            .or_insert(score);
    }

    let mut ranked: Vec<(i64, f32)> = best.into_iter().collect();
    ranked.sort_by(|a, b| {
        b.1.partial_cmp(&a.1)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.0.cmp(&b.0))
    });
    ranked.truncate(k);
    hydrate(db, ranked)
}

pub fn name_search(db: &Db, query: &str, k: usize) -> Result<Vec<RankedFile>> {
    let q = query.trim();
    if q.is_empty() {
        return Ok(Vec::new());
    }

    // Prefer FTS on names; fall back to LIKE scoring.
    let fts_q = sanitize_fts_query(q);
    let mut ranked = Vec::new();
    if !fts_q.is_empty() {
        let mut stmt = db.conn.prepare(
            "SELECT f.id, bm25(files_fts) AS score
             FROM files_fts
             JOIN files f ON f.id = files_fts.rowid
             WHERE files_fts MATCH ?1
             ORDER BY score
             LIMIT ?2",
        )?;
        // Parentheses required: `{name} : a OR b` only scopes the first term.
        let match_q = format!("{{name}} : ({fts_q})");
        let rows = stmt.query_map(params![match_q, k as i64], |row| {
            Ok((row.get::<_, i64>(0)?, row.get::<_, f64>(1)? as f32))
        });
        if let Ok(rows) = rows {
            for r in rows {
                let (id, bm25) = r?;
                // bm25 is lower-is-better; invert for display consistency.
                ranked.push((id, -bm25));
            }
        }
    }

    if ranked.is_empty() {
        ranked = like_name_rank(&db.conn, q, k)?;
    }

    hydrate(db, ranked)
}

pub fn recency_search(db: &Db, k: usize) -> Result<Vec<RankedFile>> {
    let mut stmt = db
        .conn
        .prepare("SELECT id, mtime FROM files ORDER BY mtime DESC, id ASC LIMIT ?1")?;
    let rows = stmt.query_map(params![k as i64], |row| {
        Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)? as f32))
    })?;
    let mut ranked = Vec::new();
    for r in rows {
        ranked.push(r?);
    }
    hydrate(db, ranked)
}

pub fn keyword_text_search(db: &Db, query: &str, k: usize) -> Result<Vec<RankedFile>> {
    let fts_q = sanitize_fts_query(query.trim());
    if fts_q.is_empty() {
        return Ok(Vec::new());
    }
    let mut stmt = db.conn.prepare(
        "SELECT f.id, bm25(files_fts) AS score
         FROM files_fts
         JOIN files f ON f.id = files_fts.rowid
         WHERE files_fts MATCH ?1
         ORDER BY score
         LIMIT ?2",
    )?;
    let match_q = format!("{{extracted_text}} : ({fts_q})");
    let rows = stmt.query_map(params![match_q, k as i64], |row| {
        Ok((row.get::<_, i64>(0)?, -(row.get::<_, f64>(1)? as f32)))
    });
    let mut ranked = Vec::new();
    if let Ok(rows) = rows {
        for r in rows {
            ranked.push(r?);
        }
    }
    hydrate(db, ranked)
}

fn like_name_rank(conn: &Connection, query: &str, k: usize) -> Result<Vec<(i64, f32)>> {
    let tokens: Vec<String> = query
        .split_whitespace()
        .map(|t| t.to_ascii_lowercase())
        .filter(|t| !t.is_empty())
        .collect();
    let mut stmt = conn.prepare("SELECT id, name FROM files")?;
    let rows = stmt.query_map([], |row| {
        Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?))
    })?;
    let mut scored = Vec::new();
    for r in rows {
        let (id, name) = r?;
        let lower = name.to_ascii_lowercase();
        let mut score = 0f32;
        if lower.contains(&query.to_ascii_lowercase()) {
            score += 10.0;
        }
        for t in &tokens {
            if lower.contains(t) {
                score += 1.0;
            }
        }
        if score > 0.0 {
            scored.push((id, score));
        }
    }
    scored.sort_by(|a, b| {
        b.1.partial_cmp(&a.1)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.0.cmp(&b.0))
    });
    scored.truncate(k);
    Ok(scored)
}

fn hydrate(db: &Db, ranked: Vec<(i64, f32)>) -> Result<Vec<RankedFile>> {
    let mut out = Vec::with_capacity(ranked.len());
    for (id, score) in ranked {
        if let Some(file) = db.file_by_id(id)? {
            out.push(RankedFile { file, score });
        }
    }
    Ok(out)
}

/// Build a simple FTS5 OR query from whitespace tokens.
pub fn sanitize_fts_query(query: &str) -> String {
    query
        .split_whitespace()
        .filter_map(|tok| {
            let cleaned: String = tok
                .chars()
                .filter(|c| c.is_alphanumeric() || *c == '_' || *c == '-' || *c == '.')
                .collect();
            if cleaned.is_empty() {
                None
            } else {
                Some(format!("\"{cleaned}\"*"))
            }
        })
        .collect::<Vec<_>>()
        .join(" OR ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::Db;
    use tempfile::tempdir;

    fn seed_db() -> (tempfile::TempDir, Db) {
        let dir = tempdir().unwrap();
        let db = Db::open(dir.path()).unwrap();
        db.upsert_file(
            "/a/invoice-4021.pdf",
            "invoice-4021.pdf",
            100,
            10,
            "h1",
            "extracted",
            Some("invoice total due for consulting"),
            None,
        )
        .unwrap();
        db.upsert_file(
            "/a/tax-notes.md",
            "tax-notes.md",
            200,
            10,
            "h2",
            "extracted",
            Some("spring tax documents and deductions"),
            None,
        )
        .unwrap();
        db.upsert_file(
            "/a/readme.md",
            "readme.md",
            300,
            10,
            "h3",
            "extracted",
            Some("project readme"),
            None,
        )
        .unwrap();
        (dir, db)
    }

    #[test]
    fn name_finds_invoice() {
        let (_dir, db) = seed_db();
        let hits = name_search(&db, "invoice 4021", 5).unwrap();
        assert!(!hits.is_empty());
        assert!(hits[0].file.name.contains("invoice"));
    }

    #[test]
    fn recency_orders_mtime() {
        let (_dir, db) = seed_db();
        let hits = recency_search(&db, 3).unwrap();
        assert_eq!(hits[0].file.name, "readme.md");
    }

    #[test]
    fn keyword_finds_tax() {
        let (_dir, db) = seed_db();
        let hits = keyword_text_search(&db, "deductions", 5).unwrap();
        assert!(!hits.is_empty());
        assert!(hits[0].file.name.contains("tax"));
    }

    #[test]
    fn name_search_does_not_match_body_only_token() {
        let dir = tempdir().unwrap();
        let db = Db::open(dir.path()).unwrap();
        // Token "zephyr" only in the filename.
        db.upsert_file(
            "/a/zephyr-notes.md",
            "zephyr-notes.md",
            100,
            10,
            "h-name",
            "extracted",
            Some("completely unrelated body text about cooking"),
            None,
        )
        .unwrap();
        // Token "zephyr" only in body text.
        db.upsert_file(
            "/a/cooking.md",
            "cooking.md",
            200,
            10,
            "h-body",
            "extracted",
            Some("a long note that mentions zephyr once in the body"),
            None,
        )
        .unwrap();

        let hits = name_search(&db, "zephyr notes", 10).unwrap();
        assert!(!hits.is_empty(), "expected at least the filename hit");
        assert!(
            hits.iter().all(|h| h.file.name.contains("zephyr")),
            "name search leaked body-only matches: {:?}",
            hits.iter().map(|h| &h.file.name).collect::<Vec<_>>()
        );
        assert_eq!(hits[0].file.name, "zephyr-notes.md");
    }
}
