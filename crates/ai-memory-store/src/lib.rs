//! SQLite storage layer for ai-memory.
//!
//! The crate owns a single SQLite file under `<data_dir>/db/memory.sqlite`,
//! opens it in WAL mode with foreign keys on, runs all pending migrations
//! at startup, and exposes a [`WriterHandle`] that serialises every mutation
//! through a dedicated OS thread.
//!
//! Reader-side APIs land in milestone M1-B; the writer + migrations are
//! sufficient for M1-A's "drop a page in, see it persisted" demo.

use std::path::{Path, PathBuf};

use rusqlite::Connection;

mod error;
mod migrations;
mod ops;
mod writer;

pub use error::{StoreError, StoreResult};
pub use writer::WriterHandle;

/// Filename used inside the data dir's `db/` subdirectory.
pub const DB_FILENAME: &str = "memory.sqlite";

/// Open (and migrate) a [`Store`] rooted at the given data directory.
pub struct Store {
    /// Cloneable handle to submit mutations.
    pub writer: WriterHandle,
    db_path: PathBuf,
}

impl Store {
    /// Open the SQLite file at `<data_dir>/db/memory.sqlite`, applying any
    /// outstanding migrations, then spawn the writer thread.
    ///
    /// # Errors
    /// Returns [`StoreError`] if the file cannot be opened, migrations
    /// cannot be applied, or the writer thread fails to start.
    pub fn open(data_dir: &Path) -> StoreResult<Self> {
        let db_dir = data_dir.join("db");
        std::fs::create_dir_all(&db_dir)?;
        let db_path = db_dir.join(DB_FILENAME);

        let mut conn = Connection::open(&db_path)?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        conn.pragma_update(None, "foreign_keys", "ON")?;
        conn.pragma_update(None, "busy_timeout", 5_000)?; // ms

        migrations::run(&mut conn)?;

        let writer = WriterHandle::spawn(conn);
        Ok(Self { writer, db_path })
    }

    /// Path of the SQLite file on disk.
    #[must_use]
    pub fn db_path(&self) -> &Path {
        &self.db_path
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ai_memory_core::{NewPage, PagePath, ProjectId, Tier, WorkspaceId};
    use tempfile::TempDir;

    fn sample_page(ws: WorkspaceId, proj: ProjectId, path: &str, body: &str) -> NewPage {
        NewPage {
            workspace_id: ws,
            project_id: proj,
            path: PagePath::new(path).unwrap(),
            title: "test".into(),
            body: body.into(),
            tier: Tier::Semantic,
            frontmatter_json: serde_json::json!({}),
            pinned: false,
        }
    }

    #[tokio::test]
    async fn open_and_upsert_page() {
        let tmp = TempDir::new().unwrap();
        let store = Store::open(tmp.path()).unwrap();
        let ws = WorkspaceId::new();
        let proj = ProjectId::new();
        store.writer.ensure_workspace(ws, "default").await.unwrap();
        store
            .writer
            .ensure_project(proj, ws, "ai-memory", None)
            .await
            .unwrap();
        let id_a = store
            .writer
            .upsert_page(sample_page(ws, proj, "foo.md", "hello"))
            .await
            .unwrap();
        // Same body again: returns the same id, no supersession.
        let id_b = store
            .writer
            .upsert_page(sample_page(ws, proj, "foo.md", "hello"))
            .await
            .unwrap();
        assert_eq!(id_a, id_b);
        // Different body: supersession produces a new id.
        let id_c = store
            .writer
            .upsert_page(sample_page(ws, proj, "foo.md", "hello world"))
            .await
            .unwrap();
        assert_ne!(id_b, id_c);
    }

    #[tokio::test]
    async fn serialises_parallel_writes() {
        let tmp = TempDir::new().unwrap();
        let store = Store::open(tmp.path()).unwrap();
        let ws = WorkspaceId::new();
        let proj = ProjectId::new();
        store.writer.ensure_workspace(ws, "default").await.unwrap();
        store
            .writer
            .ensure_project(proj, ws, "ai-memory", None)
            .await
            .unwrap();
        // Spawn 16 concurrent writes; the writer must serialise them.
        let mut handles = Vec::new();
        for i in 0..16 {
            let w = store.writer.clone();
            handles.push(tokio::spawn(async move {
                w.upsert_page(sample_page(
                    ws,
                    proj,
                    &format!("p{i}.md"),
                    &format!("body-{i}"),
                ))
                .await
            }));
        }
        for h in handles {
            h.await.unwrap().unwrap();
        }
    }
}
