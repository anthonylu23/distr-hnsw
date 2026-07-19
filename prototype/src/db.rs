use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use rusqlite::{params, Connection, OptionalExtension};

pub const SCHEMA_VERSION: &str = "2";

pub struct Db {
    pub conn: Connection,
    pub path: PathBuf,
}

#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct FileRow {
    pub id: i64,
    pub path: String,
    pub name: String,
    pub mtime: i64,
    pub size: i64,
    pub content_hash: String,
    pub extract_status: String,
    pub extracted_text: Option<String>,
    pub error: Option<String>,
}

#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct ChunkRow {
    pub id: i64,
    pub file_id: i64,
    pub ordinal: i64,
    pub text: String,
    pub char_start: i64,
    pub char_end: i64,
}

impl Db {
    pub fn open(work_dir: &Path) -> Result<Self> {
        std::fs::create_dir_all(work_dir)
            .with_context(|| format!("create work dir {}", work_dir.display()))?;
        let path = work_dir.join("validate.db");
        let conn =
            Connection::open(&path).with_context(|| format!("open sqlite {}", path.display()))?;
        conn.execute_batch(
            "
            PRAGMA journal_mode = WAL;
            PRAGMA foreign_keys = ON;
            ",
        )?;
        let db = Self { conn, path };
        db.migrate()?;
        Ok(db)
    }

    /// Remove SQLite DB files under `work_dir` (for `prepare --fresh`).
    pub fn wipe(work_dir: &Path) -> Result<()> {
        for name in ["validate.db", "validate.db-wal", "validate.db-shm"] {
            let p = work_dir.join(name);
            if p.exists() {
                std::fs::remove_file(&p).with_context(|| format!("remove {}", p.display()))?;
            }
        }
        Ok(())
    }

    /// Delete files whose paths are not in `keep` (cascades chunks/embeddings).
    pub fn delete_files_not_in(&self, keep: &[String]) -> Result<u64> {
        if keep.is_empty() {
            let n = self.conn.execute("DELETE FROM files", [])?;
            return Ok(n as u64);
        }
        // Temp table for visited paths to avoid giant IN clauses.
        self.conn.execute_batch(
            "CREATE TEMP TABLE IF NOT EXISTS _keep_paths (path TEXT PRIMARY KEY);
             DELETE FROM _keep_paths;",
        )?;
        {
            let mut stmt = self
                .conn
                .prepare("INSERT OR IGNORE INTO _keep_paths(path) VALUES (?1)")?;
            for p in keep {
                stmt.execute(params![p])?;
            }
        }
        let n = self.conn.execute(
            "DELETE FROM files WHERE path NOT IN (SELECT path FROM _keep_paths)",
            [],
        )?;
        self.conn
            .execute_batch("DROP TABLE IF EXISTS _keep_paths;")?;
        Ok(n as u64)
    }

    /// Fraction of files that share the corpus-wide maximum mtime (0.0..=1.0).
    pub fn max_mtime_collision_fraction(&self) -> Result<(f64, i64, i64)> {
        let total: i64 = self
            .conn
            .query_row("SELECT COUNT(*) FROM files", [], |r| r.get(0))?;
        if total == 0 {
            return Ok((0.0, 0, 0));
        }
        let max_mtime: i64 =
            self.conn
                .query_row("SELECT COALESCE(MAX(mtime), 0) FROM files", [], |r| {
                    r.get(0)
                })?;
        let at_max: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM files WHERE mtime = ?1",
            params![max_mtime],
            |r| r.get(0),
        )?;
        Ok((at_max as f64 / total as f64, at_max, total))
    }

    fn migrate(&self) -> Result<()> {
        self.conn.execute_batch(
            "
            CREATE TABLE IF NOT EXISTS meta (
                key TEXT PRIMARY KEY,
                value TEXT NOT NULL
            );

            CREATE TABLE IF NOT EXISTS files (
                id INTEGER PRIMARY KEY,
                path TEXT NOT NULL UNIQUE,
                name TEXT NOT NULL,
                mtime INTEGER NOT NULL,
                size INTEGER NOT NULL,
                content_hash TEXT NOT NULL,
                extract_status TEXT NOT NULL,
                extracted_text TEXT,
                error TEXT
            );

            CREATE INDEX IF NOT EXISTS idx_files_hash ON files(content_hash);
            CREATE INDEX IF NOT EXISTS idx_files_name ON files(name);
            CREATE INDEX IF NOT EXISTS idx_files_mtime ON files(mtime);

            CREATE TABLE IF NOT EXISTS chunks (
                id INTEGER PRIMARY KEY,
                file_id INTEGER NOT NULL REFERENCES files(id) ON DELETE CASCADE,
                ordinal INTEGER NOT NULL,
                text TEXT NOT NULL,
                char_start INTEGER NOT NULL,
                char_end INTEGER NOT NULL,
                UNIQUE(file_id, ordinal)
            );

            CREATE INDEX IF NOT EXISTS idx_chunks_file ON chunks(file_id);

            CREATE TABLE IF NOT EXISTS embeddings (
                chunk_id INTEGER NOT NULL REFERENCES chunks(id) ON DELETE CASCADE,
                model TEXT NOT NULL,
                dims INTEGER NOT NULL,
                vector BLOB NOT NULL,
                PRIMARY KEY (chunk_id, model, dims)
            );

            CREATE INDEX IF NOT EXISTS idx_embeddings_model_dims
                ON embeddings(model, dims, chunk_id);

            CREATE TABLE IF NOT EXISTS embedding_configs (
                model TEXT NOT NULL,
                dims INTEGER NOT NULL,
                ollama_digest TEXT NOT NULL,
                embedded_at TEXT NOT NULL,
                PRIMARY KEY (model, dims)
            );

            CREATE VIRTUAL TABLE IF NOT EXISTS files_fts USING fts5(
                name,
                extracted_text,
                content='files',
                content_rowid='id',
                tokenize='porter unicode61'
            );

            CREATE TRIGGER IF NOT EXISTS files_ai AFTER INSERT ON files BEGIN
                INSERT INTO files_fts(rowid, name, extracted_text)
                VALUES (new.id, new.name, coalesce(new.extracted_text, ''));
            END;

            CREATE TRIGGER IF NOT EXISTS files_ad AFTER DELETE ON files BEGIN
                INSERT INTO files_fts(files_fts, rowid, name, extracted_text)
                VALUES ('delete', old.id, old.name, coalesce(old.extracted_text, ''));
            END;

            CREATE TRIGGER IF NOT EXISTS files_au AFTER UPDATE ON files BEGIN
                INSERT INTO files_fts(files_fts, rowid, name, extracted_text)
                VALUES ('delete', old.id, old.name, coalesce(old.extracted_text, ''));
                INSERT INTO files_fts(rowid, name, extracted_text)
                VALUES (new.id, new.name, coalesce(new.extracted_text, ''));
            END;
            ",
        )?;
        self.set_meta("schema_version", SCHEMA_VERSION)?;
        Ok(())
    }

    pub fn set_meta(&self, key: &str, value: &str) -> Result<()> {
        self.conn.execute(
            "INSERT INTO meta(key, value) VALUES (?1, ?2)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            params![key, value],
        )?;
        Ok(())
    }

    pub fn get_meta(&self, key: &str) -> Result<Option<String>> {
        let v = self
            .conn
            .query_row(
                "SELECT value FROM meta WHERE key = ?1",
                params![key],
                |row| row.get(0),
            )
            .optional()?;
        Ok(v)
    }

    #[allow(dead_code)]
    #[allow(clippy::too_many_arguments)]
    pub fn upsert_file(
        &self,
        path: &str,
        name: &str,
        mtime: i64,
        size: i64,
        content_hash: &str,
        extract_status: &str,
        extracted_text: Option<&str>,
        error: Option<&str>,
    ) -> Result<i64> {
        self.conn.execute(
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
            params![
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
        let id: i64 = self.conn.query_row(
            "SELECT id FROM files WHERE path = ?1",
            params![path],
            |row| row.get(0),
        )?;
        Ok(id)
    }

    #[allow(dead_code)]
    pub fn delete_chunks_for_file(&self, file_id: i64) -> Result<()> {
        self.conn
            .execute("DELETE FROM chunks WHERE file_id = ?1", params![file_id])?;
        Ok(())
    }

    #[allow(dead_code)]
    pub fn insert_chunk(
        &self,
        file_id: i64,
        ordinal: i64,
        text: &str,
        char_start: i64,
        char_end: i64,
    ) -> Result<i64> {
        self.conn.execute(
            "INSERT INTO chunks(file_id, ordinal, text, char_start, char_end)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![file_id, ordinal, text, char_start, char_end],
        )?;
        Ok(self.conn.last_insert_rowid())
    }

    pub fn file_by_path(&self, path: &str) -> Result<Option<FileRow>> {
        self.conn
            .query_row(
                "SELECT id, path, name, mtime, size, content_hash, extract_status, extracted_text, error
                 FROM files WHERE path = ?1",
                params![path],
                |row| {
                    Ok(FileRow {
                        id: row.get(0)?,
                        path: row.get(1)?,
                        name: row.get(2)?,
                        mtime: row.get(3)?,
                        size: row.get(4)?,
                        content_hash: row.get(5)?,
                        extract_status: row.get(6)?,
                        extracted_text: row.get(7)?,
                        error: row.get(8)?,
                    })
                },
            )
            .optional()
            .map_err(Into::into)
    }

    pub fn file_by_id(&self, id: i64) -> Result<Option<FileRow>> {
        self.conn
            .query_row(
                "SELECT id, path, name, mtime, size, content_hash, extract_status, extracted_text, error
                 FROM files WHERE id = ?1",
                params![id],
                |row| {
                    Ok(FileRow {
                        id: row.get(0)?,
                        path: row.get(1)?,
                        name: row.get(2)?,
                        mtime: row.get(3)?,
                        size: row.get(4)?,
                        content_hash: row.get(5)?,
                        extract_status: row.get(6)?,
                        extracted_text: row.get(7)?,
                        error: row.get(8)?,
                    })
                },
            )
            .optional()
            .map_err(Into::into)
    }

    pub fn count_files(&self) -> Result<i64> {
        Ok(self
            .conn
            .query_row("SELECT COUNT(*) FROM files", [], |r| r.get(0))?)
    }

    pub fn count_chunks(&self) -> Result<i64> {
        Ok(self
            .conn
            .query_row("SELECT COUNT(*) FROM chunks", [], |r| r.get(0))?)
    }

    pub fn count_embeddings(&self, model: &str, dims: usize) -> Result<i64> {
        Ok(self.conn.query_row(
            "SELECT COUNT(*) FROM embeddings WHERE model = ?1 AND dims = ?2",
            params![model, dims as i64],
            |r| r.get(0),
        )?)
    }

    pub fn list_embedding_configs(&self) -> Result<Vec<(String, usize, i64)>> {
        let mut stmt = self.conn.prepare(
            "SELECT model, dims, COUNT(*) FROM embeddings GROUP BY model, dims ORDER BY model, dims",
        )?;
        let rows = stmt.query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, i64>(1)? as usize,
                row.get(2)?,
            ))
        })?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r?);
        }
        Ok(out)
    }

    pub fn chunks_needing_embed(
        &self,
        model: &str,
        dims: usize,
        force: bool,
    ) -> Result<Vec<ChunkRow>> {
        let sql = if force {
            "SELECT id, file_id, ordinal, text, char_start, char_end FROM chunks ORDER BY id"
                .to_string()
        } else {
            "SELECT c.id, c.file_id, c.ordinal, c.text, c.char_start, c.char_end
                 FROM chunks c
                 WHERE NOT EXISTS (
                    SELECT 1 FROM embeddings e
                    WHERE e.chunk_id = c.id AND e.model = ?1 AND e.dims = ?2
                 )
                 ORDER BY c.id"
                .to_string()
        };
        let mut stmt = self.conn.prepare(&sql)?;
        let mapped = if force {
            stmt.query_map([], map_chunk)?
        } else {
            stmt.query_map(params![model, dims as i64], map_chunk)?
        };
        let mut out = Vec::new();
        for r in mapped {
            out.push(r?);
        }
        Ok(out)
    }

    pub fn upsert_embedding(
        &self,
        chunk_id: i64,
        model: &str,
        dims: usize,
        vector: &[u8],
    ) -> Result<()> {
        self.conn.execute(
            "INSERT INTO embeddings(chunk_id, model, dims, vector)
             VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT(chunk_id, model, dims) DO UPDATE SET vector = excluded.vector",
            params![chunk_id, model, dims as i64, vector],
        )?;
        Ok(())
    }

    pub fn delete_embeddings_for_config(&self, model: &str, dims: usize) -> Result<usize> {
        Ok(self.conn.execute(
            "DELETE FROM embeddings WHERE model = ?1 AND dims = ?2",
            params![model, dims as i64],
        )?)
    }

    pub fn upsert_embedding_config(
        &self,
        model: &str,
        dims: usize,
        ollama_digest: &str,
    ) -> Result<()> {
        self.conn.execute(
            "INSERT INTO embedding_configs(model, dims, ollama_digest, embedded_at)
             VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT(model, dims) DO UPDATE SET
                ollama_digest = excluded.ollama_digest,
                embedded_at = excluded.embedded_at",
            params![
                model,
                dims as i64,
                ollama_digest,
                chrono::Utc::now().to_rfc3339()
            ],
        )?;
        Ok(())
    }

    pub fn embedding_config_digest(&self, model: &str, dims: usize) -> Result<Option<String>> {
        self.conn
            .query_row(
                "SELECT ollama_digest FROM embedding_configs WHERE model = ?1 AND dims = ?2",
                params![model, dims as i64],
                |row| row.get(0),
            )
            .optional()
            .map_err(Into::into)
    }

    pub fn load_embeddings(&self, model: &str, dims: usize) -> Result<Vec<(i64, i64, Vec<f32>)>> {
        let mut stmt = self.conn.prepare(
            "SELECT e.chunk_id, c.file_id, e.vector
             FROM embeddings e
             JOIN chunks c ON c.id = e.chunk_id
             WHERE e.model = ?1 AND e.dims = ?2",
        )?;
        let rows = stmt.query_map(params![model, dims as i64], |row| {
            let chunk_id: i64 = row.get(0)?;
            let file_id: i64 = row.get(1)?;
            let blob: Vec<u8> = row.get(2)?;
            Ok((chunk_id, file_id, blob))
        })?;
        let mut out = Vec::new();
        for r in rows {
            let (chunk_id, file_id, blob) = r?;
            let vec = crate::embed::bytes_to_f32s(&blob)?;
            out.push((chunk_id, file_id, vec));
        }
        Ok(out)
    }

    pub fn status_by_extract(&self) -> Result<Vec<(String, i64)>> {
        let mut stmt = self
            .conn
            .prepare("SELECT extract_status, COUNT(*) FROM files GROUP BY extract_status")?;
        let rows = stmt.query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r?);
        }
        Ok(out)
    }
}

fn map_chunk(row: &rusqlite::Row<'_>) -> rusqlite::Result<ChunkRow> {
    Ok(ChunkRow {
        id: row.get(0)?,
        file_id: row.get(1)?,
        ordinal: row.get(2)?,
        text: row.get(3)?,
        char_start: row.get(4)?,
        char_end: row.get(5)?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn embedding_config_digest_roundtrips() {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open(dir.path()).unwrap();
        assert!(db
            .embedding_config_digest("nomic-embed-text", 512)
            .unwrap()
            .is_none());
        db.upsert_embedding_config("nomic-embed-text", 512, "digest-a")
            .unwrap();
        assert_eq!(
            db.embedding_config_digest("nomic-embed-text", 512)
                .unwrap()
                .as_deref(),
            Some("digest-a")
        );
        db.upsert_embedding_config("nomic-embed-text", 512, "digest-b")
            .unwrap();
        assert_eq!(
            db.embedding_config_digest("nomic-embed-text", 512)
                .unwrap()
                .as_deref(),
            Some("digest-b")
        );
    }

    #[test]
    fn deleting_one_embedding_config_preserves_others() {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open(dir.path()).unwrap();
        let file_id = db
            .upsert_file(
                "/corpus/note.md",
                "note.md",
                0,
                1,
                "hash",
                "extracted",
                Some("text"),
                None,
            )
            .unwrap();
        let chunk_id = db.insert_chunk(file_id, 0, "text", 0, 4).unwrap();
        db.upsert_embedding(chunk_id, "model-a", 512, &[1, 2, 3, 4])
            .unwrap();
        db.upsert_embedding(chunk_id, "model-a", 768, &[5, 6, 7, 8])
            .unwrap();

        assert_eq!(db.delete_embeddings_for_config("model-a", 512).unwrap(), 1);
        assert_eq!(db.count_embeddings("model-a", 512).unwrap(), 0);
        assert_eq!(db.count_embeddings("model-a", 768).unwrap(), 1);
    }
}
