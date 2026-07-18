use std::collections::HashSet;
use std::path::Path;
use std::time::SystemTime;

use anyhow::{Context, Result};
use walkdir::WalkDir;

use crate::chunk;
use crate::db::Db;
use crate::extract::{self, ExtractStatus};

pub const EXTRACTOR_ID: &str = "v0-inprocess";

pub struct PrepareStats {
    pub seen: u64,
    pub indexed: u64,
    pub skipped_unchanged: u64,
    pub excluded: u64,
    pub needs_ocr: u64,
    pub failed: u64,
    pub chunks: u64,
    pub pruned: u64,
}

fn fingerprint(max_file_bytes: u64, chunk_chars: usize, chunk_overlap: usize) -> String {
    format!(
        "extractor={EXTRACTOR_ID};max_file_bytes={max_file_bytes};chunk_chars={chunk_chars};chunk_overlap={chunk_overlap}"
    )
}

/// Prune only when the corpus walk completed without errors.
pub(crate) fn prune_if_complete(walk_ok: bool, db: &Db, keep: &[String]) -> Result<u64> {
    if !walk_ok {
        anyhow::bail!("refusing to prune: corpus walk did not complete cleanly");
    }
    db.delete_files_not_in(keep)
}

fn persist_prepare_meta(
    db: &Db,
    corpus_s: &str,
    fp: &str,
    max_file_bytes: u64,
    chunk_chars: usize,
    chunk_overlap: usize,
) -> Result<()> {
    db.set_meta("corpus_root", corpus_s)?;
    db.set_meta("prepare_fingerprint", fp)?;
    db.set_meta("prepared_at", &chrono::Utc::now().to_rfc3339())?;
    db.set_meta("chunk_chars", &chunk_chars.to_string())?;
    db.set_meta("chunk_overlap", &chunk_overlap.to_string())?;
    db.set_meta("max_file_bytes", &max_file_bytes.to_string())?;
    db.set_meta("extractor_id", EXTRACTOR_ID)?;
    Ok(())
}

pub fn prepare_corpus(
    db: &Db,
    corpus: &Path,
    max_file_bytes: u64,
    chunk_chars: usize,
    chunk_overlap: usize,
) -> Result<PrepareStats> {
    let corpus = corpus
        .canonicalize()
        .with_context(|| format!("canonicalize corpus {}", corpus.display()))?;
    let corpus_s = corpus.to_string_lossy().to_string();
    let fp = fingerprint(max_file_bytes, chunk_chars, chunk_overlap);

    let prev_fp = db.get_meta("prepare_fingerprint")?;
    let fingerprint_ok = prev_fp.as_deref() == Some(fp.as_str());

    // Do NOT write prepare_fingerprint / prepared_at until after a successful
    // commit and prune. Absolute paths from a previous corpus root are removed by
    // the same successful prune, so a failed new-root walk preserves the old index.

    let mut stats = PrepareStats {
        seen: 0,
        indexed: 0,
        skipped_unchanged: 0,
        excluded: 0,
        needs_ocr: 0,
        failed: 0,
        chunks: 0,
        pruned: 0,
    };

    let mut visited: HashSet<String> = HashSet::new();
    let mut walk_errors: Vec<String> = Vec::new();
    let tx = db.conn.unchecked_transaction()?;

    for entry in WalkDir::new(&corpus).follow_links(false).into_iter() {
        let entry = match entry {
            Ok(e) => e,
            Err(err) => {
                walk_errors.push(err.to_string());
                continue;
            }
        };
        if !entry.file_type().is_file() {
            continue;
        }
        let path = entry.path();
        if !extract::is_supported_extension(path) {
            continue;
        }
        // Skip Obsidian/editor metadata directories.
        if path.components().any(|c| {
            matches!(
                c.as_os_str().to_str(),
                Some(".obsidian" | ".git" | ".trash" | "node_modules")
            )
        }) {
            continue;
        }
        stats.seen += 1;

        let rel = path
            .strip_prefix(&corpus)
            .unwrap_or(path)
            .to_string_lossy()
            .to_string();
        let abs = path.to_string_lossy().to_string();
        visited.insert(abs.clone());
        let name = path
            .file_name()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_else(|| rel.clone());

        let meta = match std::fs::metadata(path) {
            Ok(m) => m,
            Err(err) => {
                stats.failed += 1;
                let _ = Db::upsert_file_on(
                    &tx,
                    &abs,
                    &name,
                    0,
                    0,
                    "",
                    ExtractStatus::Failed.as_str(),
                    None,
                    Some(&err.to_string()),
                );
                continue;
            }
        };
        let mtime = meta
            .modified()
            .ok()
            .and_then(|t| t.duration_since(SystemTime::UNIX_EPOCH).ok())
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);
        let size = meta.len() as i64;
        let hash = match hash_file(path) {
            Ok(h) => h,
            Err(err) => {
                stats.failed += 1;
                let _ = Db::upsert_file_on(
                    &tx,
                    &abs,
                    &name,
                    mtime,
                    size,
                    "",
                    ExtractStatus::Failed.as_str(),
                    None,
                    Some(&err.to_string()),
                );
                continue;
            }
        };

        if fingerprint_ok {
            if let Ok(Some(existing)) = file_by_path_on(&tx, &abs) {
                if existing.content_hash == hash && existing.extract_status == "extracted" {
                    stats.skipped_unchanged += 1;
                    continue;
                }
            }
        }

        let extracted = extract::extract_file(path, max_file_bytes)?;
        match extracted.status {
            ExtractStatus::Excluded => stats.excluded += 1,
            ExtractStatus::NeedsOcr => stats.needs_ocr += 1,
            ExtractStatus::Failed => stats.failed += 1,
            ExtractStatus::Extracted => {}
        }

        let file_id = Db::upsert_file_on(
            &tx,
            &abs,
            &name,
            mtime,
            size,
            &hash,
            extracted.status.as_str(),
            extracted.text.as_deref(),
            extracted.error.as_deref(),
        )?;

        tx.execute(
            "DELETE FROM chunks WHERE file_id = ?1",
            rusqlite::params![file_id],
        )?;

        if extracted.status == ExtractStatus::Extracted {
            if let Some(text) = &extracted.text {
                let chunks = chunk::chunk_text(text, chunk_chars, chunk_overlap);
                for c in &chunks {
                    tx.execute(
                        "INSERT INTO chunks(file_id, ordinal, text, char_start, char_end)
                         VALUES (?1, ?2, ?3, ?4, ?5)",
                        rusqlite::params![
                            file_id,
                            c.ordinal as i64,
                            c.text,
                            c.char_start as i64,
                            c.char_end as i64
                        ],
                    )?;
                    stats.chunks += 1;
                }
                stats.indexed += 1;
            }
        }
    }

    if !walk_errors.is_empty() {
        // Drop the transaction without committing so partial upserts are discarded,
        // and never prune (would delete rows for unreadable subtrees).
        drop(tx);
        anyhow::bail!(
            "corpus walk incomplete ({} error(s)); refusing to commit or prune. First: {}",
            walk_errors.len(),
            walk_errors[0]
        );
    }

    tx.commit()?;

    let keep: Vec<String> = visited.into_iter().collect();
    stats.pruned = prune_if_complete(true, db, &keep)?;

    persist_prepare_meta(
        db,
        &corpus_s,
        &fp,
        max_file_bytes,
        chunk_chars,
        chunk_overlap,
    )?;

    Ok(stats)
}

fn hash_file(path: &Path) -> Result<String> {
    let bytes = std::fs::read(path).with_context(|| format!("hash read {}", path.display()))?;
    Ok(blake3::hash(&bytes).to_hex().to_string())
}

#[derive(Debug)]
struct ExistingFile {
    content_hash: String,
    extract_status: String,
}

fn file_by_path_on(conn: &rusqlite::Transaction<'_>, path: &str) -> Result<Option<ExistingFile>> {
    use rusqlite::OptionalExtension;
    let row = conn
        .query_row(
            "SELECT content_hash, extract_status FROM files WHERE path = ?1",
            rusqlite::params![path],
            |row| {
                Ok(ExistingFile {
                    content_hash: row.get(0)?,
                    extract_status: row.get(1)?,
                })
            },
        )
        .optional()?;
    Ok(row)
}

impl Db {
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn upsert_file_on(
        conn: &rusqlite::Transaction<'_>,
        path: &str,
        name: &str,
        mtime: i64,
        size: i64,
        content_hash: &str,
        extract_status: &str,
        extracted_text: Option<&str>,
        error: Option<&str>,
    ) -> Result<i64> {
        conn.execute(
            "INSERT INTO files(path, name, mtime, size, content_hash, extract_status, extracted_text, error)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
             ON CONFLICT(path) DO UPDATE SET
                name = excluded.name,
                mtime = excluded.mtime,
                size = excluded.size,
                content_hash = excluded.content_hash,
                extract_status = excluded.extract_status,
                extracted_text = excluded.extracted_text,
                error = excluded.error",
            rusqlite::params![
                path,
                name,
                mtime,
                size,
                content_hash,
                extract_status,
                extracted_text,
                error
            ],
        )?;
        let id: i64 = conn.query_row(
            "SELECT id FROM files WHERE path = ?1",
            rusqlite::params![path],
            |row| row.get(0),
        )?;
        Ok(id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn prune_removes_deleted_corpus_files() {
        let dir = tempdir().unwrap();
        let corpus = dir.path().join("corpus");
        std::fs::create_dir_all(&corpus).unwrap();
        let a = corpus.join("a.md");
        let b = corpus.join("b.md");
        std::fs::write(&a, "alpha").unwrap();
        std::fs::write(&b, "beta").unwrap();

        let work = dir.path().join("work");
        let db = Db::open(&work).unwrap();
        let stats = prepare_corpus(&db, &corpus, 8 * 1024 * 1024, 2000, 200).unwrap();
        assert_eq!(stats.seen, 2);
        assert_eq!(db.count_files().unwrap(), 2);

        std::fs::remove_file(&b).unwrap();
        let stats2 = prepare_corpus(&db, &corpus, 8 * 1024 * 1024, 2000, 200).unwrap();
        assert_eq!(stats2.pruned, 1);
        assert_eq!(db.count_files().unwrap(), 1);
        assert!(db
            .file_by_path(&a.canonicalize().unwrap().to_string_lossy())
            .unwrap()
            .is_some());
    }

    #[test]
    fn chunk_param_change_forces_rechunk() {
        let dir = tempdir().unwrap();
        let corpus = dir.path().join("corpus");
        std::fs::create_dir_all(&corpus).unwrap();
        // Long enough to produce >1 chunk at small window.
        let text = "word ".repeat(400);
        std::fs::write(corpus.join("long.md"), &text).unwrap();

        let work = dir.path().join("work");
        let db = Db::open(&work).unwrap();
        let s1 = prepare_corpus(&db, &corpus, 8 * 1024 * 1024, 2000, 200).unwrap();
        assert!(s1.chunks >= 1);
        let chunks_before = db.count_chunks().unwrap();

        let s2 = prepare_corpus(&db, &corpus, 8 * 1024 * 1024, 100, 10).unwrap();
        assert_eq!(s2.skipped_unchanged, 0);
        assert!(s2.chunks > 0);
        let chunks_after = db.count_chunks().unwrap();
        assert!(
            chunks_after > chunks_before,
            "expected more chunks after smaller window: before={chunks_before} after={chunks_after}"
        );
    }

    #[test]
    fn fingerprint_written_only_after_successful_prepare() {
        let dir = tempdir().unwrap();
        let corpus = dir.path().join("corpus");
        std::fs::create_dir_all(&corpus).unwrap();
        std::fs::write(corpus.join("a.md"), "hello").unwrap();
        let work = dir.path().join("work");
        let db = Db::open(&work).unwrap();
        assert!(db.get_meta("prepare_fingerprint").unwrap().is_none());
        prepare_corpus(&db, &corpus, 8 * 1024 * 1024, 2000, 200).unwrap();
        let fp = db.get_meta("prepare_fingerprint").unwrap().unwrap();
        assert_eq!(fp, fingerprint(8 * 1024 * 1024, 2000, 200));
    }

    #[test]
    fn prune_if_complete_refuses_incomplete_walk() {
        let dir = tempdir().unwrap();
        let db = Db::open(dir.path()).unwrap();
        let err = prune_if_complete(false, &db, &[]).unwrap_err();
        assert!(err.to_string().contains("refusing to prune"));
    }

    #[test]
    fn successful_root_change_replaces_old_rows_during_final_prune() {
        let dir = tempdir().unwrap();
        let old_corpus = dir.path().join("old-corpus");
        let new_corpus = dir.path().join("new-corpus");
        std::fs::create_dir_all(&old_corpus).unwrap();
        std::fs::create_dir_all(&new_corpus).unwrap();
        let old_file = old_corpus.join("old.md");
        let new_file = new_corpus.join("new.md");
        std::fs::write(&old_file, "old corpus text").unwrap();
        std::fs::write(&new_file, "new corpus text").unwrap();

        let work = dir.path().join("work");
        let db = Db::open(&work).unwrap();
        prepare_corpus(&db, &old_corpus, 8 * 1024 * 1024, 2000, 200).unwrap();
        assert_eq!(db.count_files().unwrap(), 1);

        let stats = prepare_corpus(&db, &new_corpus, 8 * 1024 * 1024, 2000, 200).unwrap();
        assert_eq!(stats.pruned, 1);
        assert_eq!(db.count_files().unwrap(), 1);
        assert!(db
            .file_by_path(&new_file.canonicalize().unwrap().to_string_lossy())
            .unwrap()
            .is_some());
        assert!(db
            .file_by_path(&old_file.canonicalize().unwrap().to_string_lossy())
            .unwrap()
            .is_none());
        assert_eq!(
            db.get_meta("corpus_root").unwrap().as_deref(),
            Some(
                new_corpus
                    .canonicalize()
                    .unwrap()
                    .to_string_lossy()
                    .as_ref()
            )
        );
    }
}
