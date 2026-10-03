//! SQLite persistence: schema, dedup, retention/eviction, FTS5 search.
//!
//! Layout (XDG): `~/.local/share/clipvault/history.db` (WAL, mode 0600) and
//! `~/.local/share/clipvault/images/<blake3-hex>.png` for image payloads —
//! files, not BLOBs, so the DB stays small and thumbnails can lazy-load.

use crate::config::Config;
use crate::types::{ClipItem, ClipKind, NewClip};
use rusqlite::{params, Connection, OptionalExtension};
use std::path::{Path, PathBuf};

const SCHEMA: &str = r#"
PRAGMA journal_mode = WAL;
PRAGMA foreign_keys = ON;

CREATE TABLE IF NOT EXISTS items (
  id            INTEGER PRIMARY KEY,
  hash          BLOB NOT NULL UNIQUE,          -- blake3-256, 32 bytes
  kind          TEXT NOT NULL CHECK (kind IN ('text','html','image')),
  text_content  TEXT,
  html_content  TEXT,
  image_path    TEXT,
  byte_size     INTEGER NOT NULL,
  source_app    TEXT,
  pinned        INTEGER NOT NULL DEFAULT 0,
  created_at    INTEGER NOT NULL,
  last_used_at  INTEGER NOT NULL,
  use_count     INTEGER NOT NULL DEFAULT 1
);
CREATE INDEX IF NOT EXISTS idx_items_recency ON items(pinned DESC, last_used_at DESC);

CREATE VIRTUAL TABLE IF NOT EXISTS items_fts USING fts5(
  text_content, content='items', content_rowid='id'
);

CREATE TRIGGER IF NOT EXISTS items_ai AFTER INSERT ON items BEGIN
  INSERT INTO items_fts(rowid, text_content) VALUES (new.id, new.text_content);
END;
CREATE TRIGGER IF NOT EXISTS items_ad AFTER DELETE ON items BEGIN
  INSERT INTO items_fts(items_fts, rowid, text_content)
    VALUES ('delete', old.id, old.text_content);
END;
CREATE TRIGGER IF NOT EXISTS items_au AFTER UPDATE OF text_content ON items BEGIN
  INSERT INTO items_fts(items_fts, rowid, text_content)
    VALUES ('delete', old.id, old.text_content);
  INSERT INTO items_fts(rowid, text_content) VALUES (new.id, new.text_content);
END;
"#;

pub struct Storage {
    conn: Connection,
    images_dir: PathBuf,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InsertOutcome {
    /// Fresh row created; carries the new id.
    New(i64),
    /// Known hash — bumped to top and use_count incremented.
    Bumped(i64),
}

impl Storage {
    pub fn open(db_path: &Path, images_dir: &Path) -> Result<Self, StorageError> {
        if let Some(dir) = db_path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        std::fs::create_dir_all(images_dir)?;

        let conn = Connection::open(db_path)?;
        conn.execute_batch(SCHEMA)?;
        set_private_permissions(db_path)?;

        Ok(Self {
            conn,
            images_dir: images_dir.to_path_buf(),
        })
    }

    /// In-memory database for tests (images go to a temp dir the caller owns).
    #[cfg(test)]
    fn open_in_memory(images_dir: &Path) -> Result<Self, StorageError> {
        let conn = Connection::open_in_memory()?;
        conn.execute_batch(SCHEMA)?;
        Ok(Self {
            conn,
            images_dir: images_dir.to_path_buf(),
        })
    }

    /// Insert a clip, deduplicating by content hash. Re-copies bump the
    /// existing row to the top instead of duplicating (Windows behavior).
    pub fn insert(&mut self, clip: &NewClip) -> Result<InsertOutcome, StorageError> {
        let hash = clip.hash();
        let now = unix_now();

        if let Some(id) = self
            .conn
            .query_row(
                "SELECT id FROM items WHERE hash = ?1",
                params![hash.as_slice()],
                |r| r.get::<_, i64>(0),
            )
            .optional()?
        {
            self.conn.execute(
                "UPDATE items SET last_used_at = ?2, use_count = use_count + 1 WHERE id = ?1",
                params![id, now],
            )?;
            return Ok(InsertOutcome::Bumped(id));
        }

        let image_path = match (&clip.kind, &clip.image_png) {
            (ClipKind::Image, Some(png)) => {
                let name = format!("{}.png", blake3::hash(png).to_hex());
                std::fs::write(self.images_dir.join(&name), png)?;
                Some(name)
            }
            _ => None,
        };

        self.conn.execute(
            "INSERT INTO items
               (hash, kind, text_content, html_content, image_path,
                byte_size, source_app, pinned, created_at, last_used_at, use_count)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, 0, ?8, ?8, 1)",
            params![
                hash.as_slice(),
                clip.kind.as_str(),
                clip.text_content,
                clip.html_content,
                image_path,
                clip.byte_size as i64,
                clip.source_app,
                now,
            ],
        )?;
        Ok(InsertOutcome::New(self.conn.last_insert_rowid()))
    }

    pub fn get(&self, id: i64) -> Result<Option<ClipItem>, StorageError> {
        self.conn
            .query_row("SELECT * FROM items WHERE id = ?1", params![id], row_to_item)
            .optional()
            .map_err(Into::into)
    }

    /// Pinned first, then most recently used. The popup's default order.
    pub fn list(&self, limit: u32) -> Result<Vec<ClipItem>, StorageError> {
        let mut stmt = self.conn.prepare(
            "SELECT * FROM items ORDER BY pinned DESC, last_used_at DESC LIMIT ?1",
        )?;
        let rows = stmt.query_map(params![limit], row_to_item)?;
        Ok(rows.collect::<Result<Vec<_>, _>>()?)
    }

    /// FTS5 search; falls back to LIKE for queries FTS can't parse
    /// (e.g. bare punctuation).
    pub fn search(&self, query: &str, limit: u32) -> Result<Vec<ClipItem>, StorageError> {
        let trimmed = query.trim();
        if trimmed.is_empty() {
            return self.list(limit);
        }
        let fts_query = trimmed
            .split_whitespace()
            .map(|w| format!("\"{}\"*", w.replace('"', "")))
            .collect::<Vec<_>>()
            .join(" ");
        let fts = self.search_inner(
            "SELECT i.* FROM items i
             JOIN items_fts f ON f.rowid = i.id
             WHERE items_fts MATCH ?1
             ORDER BY i.pinned DESC, i.last_used_at DESC LIMIT ?2",
            params![fts_query, limit],
        );
        match fts {
            Ok(v) => Ok(v),
            Err(_) => {
                let like = format!("%{}%", trimmed.replace('%', "\\%").replace('_', "\\_"));
                self.search_inner(
                    "SELECT * FROM items
                     WHERE text_content LIKE ?1 ESCAPE '\\'
                     ORDER BY pinned DESC, last_used_at DESC LIMIT ?2",
                    params![like, limit],
                )
            }
        }
    }

    fn search_inner(
        &self,
        sql: &str,
        params: impl rusqlite::Params,
    ) -> Result<Vec<ClipItem>, StorageError> {
        let mut stmt = self.conn.prepare(sql)?;
        let rows = stmt.query_map(params, row_to_item)?;
        Ok(rows.collect::<Result<Vec<_>, _>>()?)
    }

    pub fn set_pinned(&self, id: i64, pinned: bool) -> Result<(), StorageError> {
        self.conn.execute(
            "UPDATE items SET pinned = ?2 WHERE id = ?1",
            params![id, pinned as i64],
        )?;
        Ok(())
    }

    /// Bump recency + use counter when an item is pasted again.
    pub fn touch(&self, id: i64) -> Result<(), StorageError> {
        self.conn.execute(
            "UPDATE items SET last_used_at = ?2, use_count = use_count + 1 WHERE id = ?1",
            params![id, unix_now()],
        )?;
        Ok(())
    }

    pub fn delete(&self, id: i64) -> Result<(), StorageError> {
        if let Some(path) = self.image_rel_path(id)? {
            let _ = std::fs::remove_file(self.images_dir.join(path));
        }
        self.conn
            .execute("DELETE FROM items WHERE id = ?1", params![id])?;
        Ok(())
    }

    /// Clear history. Pinned rows survive unless `include_pinned`.
    pub fn clear(&self, include_pinned: bool) -> Result<u64, StorageError> {
        let doomed: Vec<String> = self
            .conn
            .prepare(if include_pinned {
                "SELECT image_path FROM items WHERE image_path IS NOT NULL"
            } else {
                "SELECT image_path FROM items WHERE image_path IS NOT NULL AND pinned = 0"
            })?
            .query_map([], |r| r.get(0))?
            .collect::<Result<_, _>>()?;
        let n = if include_pinned {
            self.conn.execute("DELETE FROM items", [])?
        } else {
            self.conn
                .execute("DELETE FROM items WHERE pinned = 0", [])?
        };
        for p in doomed {
            let _ = std::fs::remove_file(self.images_dir.join(p));
        }
        Ok(n as u64)
    }

    /// Evict oldest unpinned rows beyond `max_entries` and rows older than
    /// `max_age_days` (0 = forever). Returns rows removed.
    pub fn enforce_caps(&mut self, cfg: &Config) -> Result<u64, StorageError> {
        let mut removed = 0u64;

        if cfg.max_age_days > 0 {
            let cutoff = unix_now() - (cfg.max_age_days as i64) * 86_400;
            removed += self.delete_where(
                "pinned = 0 AND last_used_at < ?1",
                &[&cutoff],
            )?;
        }

        // Keep the newest max_entries unpinned rows; evict the rest.
        removed += self.delete_where(
            "pinned = 0 AND id NOT IN (
               SELECT id FROM items WHERE pinned = 0
               ORDER BY last_used_at DESC LIMIT ?1
             )",
            &[&(cfg.max_entries as i64)],
        )?;

        Ok(removed)
    }

    fn delete_where(
        &self,
        cond: &str,
        params: &[&dyn rusqlite::ToSql],
    ) -> Result<u64, StorageError> {
        let select = format!(
            "SELECT image_path FROM items WHERE image_path IS NOT NULL AND ({cond})"
        );
        let doomed: Vec<String> = self
            .conn
            .prepare(&select)?
            .query_map(params, |r| r.get(0))?
            .collect::<Result<_, _>>()?;

        let sql = format!("DELETE FROM items WHERE {cond}");
        let n = self.conn.execute(&sql, params)?;
        for p in doomed {
            let _ = std::fs::remove_file(self.images_dir.join(&p));
        }
        Ok(n as u64)
    }

    fn image_rel_path(&self, id: i64) -> Result<Option<String>, StorageError> {
        self.conn
            .query_row(
                "SELECT image_path FROM items WHERE id = ?1",
                params![id],
                |r| r.get(0),
            )
            .optional()
            .map_err(Into::into)
    }

    pub fn count(&self) -> Result<u64, StorageError> {
        let n: i64 = self
            .conn
            .query_row("SELECT COUNT(*) FROM items", [], |r| r.get(0))?;
        Ok(n as u64)
    }
}

fn row_to_item(row: &rusqlite::Row) -> rusqlite::Result<ClipItem> {
    let hash_vec: Vec<u8> = row.get("hash")?;
    let mut hash = [0u8; 32];
    hash.copy_from_slice(&hash_vec);
    let kind: String = row.get("kind")?;
    Ok(ClipItem {
        id: row.get("id")?,
        hash,
        kind: ClipKind::parse(&kind).unwrap_or(ClipKind::Text),
        text_content: row.get("text_content")?,
        html_content: row.get("html_content")?,
        image_path: row.get("image_path")?,
        byte_size: row.get::<_, i64>("byte_size")? as u64,
        source_app: row.get("source_app")?,
        pinned: row.get::<_, i64>("pinned")? != 0,
        created_at: row.get("created_at")?,
        last_used_at: row.get("last_used_at")?,
        use_count: row.get::<_, i64>("use_count")? as u64,
    })
}

fn unix_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// 0600 on the DB — history contains whatever the user copied.
#[cfg(unix)]
fn set_private_permissions(path: &Path) -> Result<(), StorageError> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    Ok(())
}

#[cfg(not(unix))]
fn set_private_permissions(_path: &Path) -> Result<(), StorageError> {
    Ok(())
}

#[derive(Debug, thiserror::Error)]
pub enum StorageError {
    #[error("sqlite: {0}")]
    Sqlite(String),
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
}

impl From<rusqlite::Error> for StorageError {
    fn from(e: rusqlite::Error) -> Self {
        Self::Sqlite(e.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text_clip(s: &str) -> NewClip {
        NewClip {
            kind: ClipKind::Text,
            text_content: Some(s.into()),
            html_content: None,
            image_png: None,
            byte_size: s.len() as u64,
            source_app: None,
        }
    }

    fn temp_store() -> (Storage, PathBuf) {
        let dir = std::env::temp_dir().join(format!(
            "clipvault-test-{}-{}",
            std::process::id(),
            unix_now_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let storage = Storage::open_in_memory(&dir).unwrap();
        (storage, dir)
    }

    fn unix_now_nanos() -> u128 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    }

    #[test]
    fn insert_and_get() {
        let (mut st, _dir) = temp_store();
        let id = match st.insert(&text_clip("hello")).unwrap() {
            InsertOutcome::New(id) => id,
            _ => panic!("expected New"),
        };
        let item = st.get(id).unwrap().unwrap();
        assert_eq!(item.text_content.as_deref(), Some("hello"));
        assert_eq!(item.use_count, 1);
        assert!(!item.pinned);
    }

    #[test]
    fn recopy_dedups_and_bumps() {
        let (mut st, _dir) = temp_store();
        let id1 = match st.insert(&text_clip("same")).unwrap() {
            InsertOutcome::New(id) => id,
            _ => panic!(),
        };
        st.insert(&text_clip("other")).unwrap();
        // Re-copying "same" must not create a row, and must move it to top.
        match st.insert(&text_clip("same")).unwrap() {
            InsertOutcome::Bumped(id) => assert_eq!(id, id1),
            _ => panic!("expected Bumped"),
        }
        assert_eq!(st.count().unwrap(), 2);
        let top = &st.list(10).unwrap()[0];
        assert_eq!(top.text_content.as_deref(), Some("same"));
        assert_eq!(top.use_count, 2);
    }

    #[test]
    fn pinned_sorts_first_and_survives_clear() {
        let (mut st, _dir) = temp_store();
        for s in ["aaa", "bbb", "ccc"] {
            st.insert(&text_clip(s)).unwrap();
        }
        let items = st.list(10).unwrap();
        let bbb = items.iter().find(|i| i.text_content.as_deref() == Some("bbb")).unwrap();
        st.set_pinned(bbb.id, true).unwrap();

        assert_eq!(st.list(10).unwrap()[0].text_content.as_deref(), Some("bbb"));

        let removed = st.clear(false).unwrap();
        assert_eq!(removed, 2);
        assert_eq!(st.count().unwrap(), 1);
        assert_eq!(st.list(10).unwrap()[0].text_content.as_deref(), Some("bbb"));

        assert_eq!(st.clear(true).unwrap(), 1);
        assert_eq!(st.count().unwrap(), 0);
    }

    #[test]
    fn max_entries_evicts_oldest_unpinned() {
        let (mut st, _dir) = temp_store();
        for i in 0..10 {
            st.insert(&text_clip(&format!("item-{i}"))).unwrap();
        }
        // Pin the oldest so eviction must skip it.
        let oldest = st.list(100).unwrap().last().unwrap().id;
        st.set_pinned(oldest, true).unwrap();

        let mut cfg = Config::default();
        cfg.max_entries = 5;
        st.enforce_caps(&cfg).unwrap();

        let items = st.list(100).unwrap();
        assert_eq!(items.len(), 6); // 5 unpinned + 1 pinned
        assert!(items.iter().any(|i| i.id == oldest));
    }

    #[test]
    fn search_finds_and_ranks() {
        let (mut st, _dir) = temp_store();
        st.insert(&text_clip("the quick brown fox")).unwrap();
        st.insert(&text_clip("lazy dog")).unwrap();
        st.insert(&text_clip("quick silver")).unwrap();

        let hits = st.search("quick", 10).unwrap();
        assert_eq!(hits.len(), 2);
        assert!(hits.iter().all(|i| i.text_content.as_deref().unwrap().contains("quick")));

        assert_eq!(st.search("nonexistent-term", 10).unwrap().len(), 0);
        // FTS-hostile input must not error (LIKE fallback).
        assert!(st.search("***", 10).is_ok());
    }

    #[test]
    fn image_clip_writes_file_and_delete_removes_it() {
        let (mut st, dir) = temp_store();
        let png = vec![0x89u8, 0x50, 0x4E, 0x47]; // fake PNG header
        let clip = NewClip {
            kind: ClipKind::Image,
            text_content: None,
            html_content: None,
            image_png: Some(png.clone()),
            byte_size: png.len() as u64,
            source_app: None,
        };
        let id = match st.insert(&clip).unwrap() {
            InsertOutcome::New(id) => id,
            _ => panic!(),
        };
        let item = st.get(id).unwrap().unwrap();
        let rel = item.image_path.clone().unwrap();
        assert!(dir.join(&rel).exists());

        st.delete(id).unwrap();
        assert!(!dir.join(&rel).exists());
        assert!(st.get(id).unwrap().is_none());
    }
}
