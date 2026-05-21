//! Mutating SQL operations executed on the writer thread.
//!
//! Each operation is one transaction. Calling them from anywhere other than
//! the writer thread would violate the single-writer invariant (see
//! [`crate::writer`]).

use ai_memory_core::{NewPage, PageId};
use jiff::Timestamp;
use rusqlite::{Connection, OptionalExtension, params};
use sha2::{Digest, Sha256};

use crate::error::StoreResult;

/// Upsert a page by path, superseding any existing latest version when the
/// content (sha256 of body) has changed.
///
/// Returns the id of the page row that should now be considered current.
pub fn upsert_page(conn: &mut Connection, page: &NewPage) -> StoreResult<PageId> {
    let body_sha256: [u8; 32] = {
        let mut hasher = Sha256::new();
        hasher.update(page.body.as_bytes());
        hasher.finalize().into()
    };
    let frontmatter_str = serde_json::to_string(&page.frontmatter_json)?;
    let now = Timestamp::now().as_microsecond();
    let tier_str = page.tier.as_str();

    let tx = conn.transaction()?;

    let existing: Option<(Vec<u8>, Vec<u8>)> = tx
        .query_row(
            "SELECT id, body_sha256 FROM pages \
             WHERE workspace_id = ?1 AND project_id = ?2 AND path = ?3 AND is_latest = 1",
            params![
                page.workspace_id.as_bytes(),
                page.project_id.as_bytes(),
                page.path.as_str(),
            ],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?;

    let result_id = if let Some((existing_id, existing_sha)) = existing {
        if existing_sha == body_sha256 {
            // Content unchanged; touch updated_at only and return existing id.
            tx.execute(
                "UPDATE pages SET updated_at = ?1 WHERE id = ?2",
                params![now, existing_id],
            )?;
            PageId::from_slice(&existing_id)?
        } else {
            let new_id = PageId::new();
            tx.execute(
                "UPDATE pages SET is_latest = 0 WHERE id = ?1",
                params![existing_id],
            )?;
            tx.execute(
                "INSERT INTO pages \
                 (id, workspace_id, project_id, path, title, tier, body, body_sha256, \
                  frontmatter_json, is_latest, supersedes, pinned, created_at, updated_at) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, 1, ?10, ?11, ?12, ?12)",
                params![
                    new_id.as_bytes(),
                    page.workspace_id.as_bytes(),
                    page.project_id.as_bytes(),
                    page.path.as_str(),
                    page.title,
                    tier_str,
                    page.body,
                    body_sha256.as_slice(),
                    frontmatter_str,
                    existing_id,
                    i64::from(page.pinned),
                    now,
                ],
            )?;
            audit(
                &tx,
                "supersede_page",
                Some(page.workspace_id.as_bytes()),
                Some(page.project_id.as_bytes()),
                Some(new_id.as_bytes()),
                now,
            )?;
            new_id
        }
    } else {
        let new_id = PageId::new();
        tx.execute(
            "INSERT INTO pages \
             (id, workspace_id, project_id, path, title, tier, body, body_sha256, \
              frontmatter_json, is_latest, pinned, created_at, updated_at) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, 1, ?10, ?11, ?11)",
            params![
                new_id.as_bytes(),
                page.workspace_id.as_bytes(),
                page.project_id.as_bytes(),
                page.path.as_str(),
                page.title,
                tier_str,
                page.body,
                body_sha256.as_slice(),
                frontmatter_str,
                i64::from(page.pinned),
                now,
            ],
        )?;
        audit(
            &tx,
            "create_page",
            Some(page.workspace_id.as_bytes()),
            Some(page.project_id.as_bytes()),
            Some(new_id.as_bytes()),
            now,
        )?;
        new_id
    };

    tx.commit()?;
    Ok(result_id)
}

/// Ensure a workspace exists; insert a stub row keyed on `name` otherwise.
pub fn ensure_workspace(
    conn: &mut Connection,
    id: &ai_memory_core::WorkspaceId,
    name: &str,
) -> StoreResult<()> {
    conn.execute(
        "INSERT INTO workspaces (id, name, created_at) VALUES (?1, ?2, ?3) \
         ON CONFLICT(name) DO NOTHING",
        params![id.as_bytes(), name, Timestamp::now().as_microsecond()],
    )?;
    Ok(())
}

/// Ensure a project exists under the given workspace.
pub fn ensure_project(
    conn: &mut Connection,
    id: &ai_memory_core::ProjectId,
    workspace_id: &ai_memory_core::WorkspaceId,
    name: &str,
    repo_path: Option<&str>,
) -> StoreResult<()> {
    conn.execute(
        "INSERT INTO projects (id, workspace_id, name, repo_path, created_at) \
         VALUES (?1, ?2, ?3, ?4, ?5) \
         ON CONFLICT(workspace_id, name) DO NOTHING",
        params![
            id.as_bytes(),
            workspace_id.as_bytes(),
            name,
            repo_path,
            Timestamp::now().as_microsecond()
        ],
    )?;
    Ok(())
}

fn audit(
    tx: &rusqlite::Transaction<'_>,
    op: &str,
    workspace_id: Option<&[u8; 16]>,
    project_id: Option<&[u8; 16]>,
    page_id: Option<&[u8; 16]>,
    at: i64,
) -> StoreResult<()> {
    tx.execute(
        "INSERT INTO audit_log (at, op, workspace_id, project_id, page_id, detail) \
         VALUES (?1, ?2, ?3, ?4, ?5, '{}')",
        params![
            at,
            op,
            workspace_id.map(|b| &b[..]),
            project_id.map(|b| &b[..]),
            page_id.map(|b| &b[..])
        ],
    )?;
    Ok(())
}
