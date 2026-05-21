//! Single-writer SQLite actor.
//!
//! Every mutating SQL statement flows through one dedicated OS thread that
//! owns the writer [`rusqlite::Connection`]. Callers send [`WriteCmd`]
//! variants over an mpsc channel and receive results back through a
//! `oneshot`. This pattern eliminates the `database is locked` failure
//! mode that bit cognee (#2717) — there is exactly one writer at all
//! times, by construction.

use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};

use ai_memory_core::{NewPage, PageId, ProjectId, WorkspaceId};
use rusqlite::Connection;
use tokio::sync::{mpsc, oneshot};

use crate::error::{StoreError, StoreResult};
use crate::ops;

/// Commands accepted by the writer thread.
pub(crate) enum WriteCmd {
    EnsureWorkspace {
        id: WorkspaceId,
        name: String,
        reply: oneshot::Sender<StoreResult<()>>,
    },
    EnsureProject {
        id: ProjectId,
        workspace_id: WorkspaceId,
        name: String,
        repo_path: Option<String>,
        reply: oneshot::Sender<StoreResult<()>>,
    },
    UpsertPage {
        page: NewPage,
        reply: oneshot::Sender<StoreResult<PageId>>,
    },
    Shutdown,
}

/// Cheap, cloneable handle that submits commands to the writer.
#[derive(Clone)]
pub struct WriterHandle {
    inner: Arc<WriterInner>,
}

struct WriterInner {
    tx: mpsc::Sender<WriteCmd>,
    join: Mutex<Option<JoinHandle<()>>>,
}

impl WriterHandle {
    /// Take ownership of `conn` and spawn the writer thread.
    pub(crate) fn spawn(conn: Connection) -> Self {
        let (tx, rx) = mpsc::channel(1024);
        let handle = thread::Builder::new()
            .name("ai-memory-writer".into())
            .spawn(move || worker_loop(conn, rx))
            .expect("spawn writer thread");

        Self {
            inner: Arc::new(WriterInner {
                tx,
                join: Mutex::new(Some(handle)),
            }),
        }
    }

    /// Ensure a workspace row exists.
    ///
    /// # Errors
    /// Returns [`StoreError::WriterClosed`] if the actor has shut down, or
    /// propagates the SQL error from [`ops::ensure_workspace`].
    pub async fn ensure_workspace(
        &self,
        id: WorkspaceId,
        name: impl Into<String>,
    ) -> StoreResult<()> {
        let (tx, rx) = oneshot::channel();
        self.send(WriteCmd::EnsureWorkspace {
            id,
            name: name.into(),
            reply: tx,
        })
        .await?;
        rx.await.map_err(|_| StoreError::WriterClosed)?
    }

    /// Ensure a project row exists under the given workspace.
    ///
    /// # Errors
    /// Returns [`StoreError::WriterClosed`] if the actor has shut down, or
    /// propagates the SQL error from [`ops::ensure_project`].
    pub async fn ensure_project(
        &self,
        id: ProjectId,
        workspace_id: WorkspaceId,
        name: impl Into<String>,
        repo_path: Option<String>,
    ) -> StoreResult<()> {
        let (tx, rx) = oneshot::channel();
        self.send(WriteCmd::EnsureProject {
            id,
            workspace_id,
            name: name.into(),
            repo_path,
            reply: tx,
        })
        .await?;
        rx.await.map_err(|_| StoreError::WriterClosed)?
    }

    /// Upsert a page (creating it or superseding the existing latest
    /// version when the body has changed).
    ///
    /// # Errors
    /// Returns [`StoreError::WriterClosed`] if the actor has shut down, or
    /// propagates the SQL error from [`ops::upsert_page`].
    pub async fn upsert_page(&self, page: NewPage) -> StoreResult<PageId> {
        let (tx, rx) = oneshot::channel();
        self.send(WriteCmd::UpsertPage { page, reply: tx }).await?;
        rx.await.map_err(|_| StoreError::WriterClosed)?
    }

    async fn send(&self, cmd: WriteCmd) -> StoreResult<()> {
        self.inner
            .tx
            .send(cmd)
            .await
            .map_err(|_| StoreError::WriterClosed)
    }
}

impl Drop for WriterInner {
    fn drop(&mut self) {
        let _ = self.tx.try_send(WriteCmd::Shutdown);
        if let Some(handle) = self.join.lock().expect("writer join mutex poisoned").take() {
            let _ = handle.join();
        }
    }
}

fn worker_loop(mut conn: Connection, mut rx: mpsc::Receiver<WriteCmd>) {
    while let Some(cmd) = rx.blocking_recv() {
        match cmd {
            WriteCmd::Shutdown => break,
            WriteCmd::EnsureWorkspace { id, name, reply } => {
                let result = ops::ensure_workspace(&mut conn, &id, &name);
                let _ = reply.send(result);
            }
            WriteCmd::EnsureProject {
                id,
                workspace_id,
                name,
                repo_path,
                reply,
            } => {
                let result =
                    ops::ensure_project(&mut conn, &id, &workspace_id, &name, repo_path.as_deref());
                let _ = reply.send(result);
            }
            WriteCmd::UpsertPage { page, reply } => {
                let result = ops::upsert_page(&mut conn, &page);
                let _ = reply.send(result);
            }
        }
    }
    tracing::debug!("writer thread exiting cleanly");
}
