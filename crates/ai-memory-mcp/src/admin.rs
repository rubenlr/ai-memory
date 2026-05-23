//! Admin HTTP routes — state-touching operations invoked by the CLI
//! over plain HTTP (not MCP). Currently exposes:
//!
//! - `POST /admin/bootstrap`    — ingest a pre-collected source bundle
//!   into seed wiki pages via the configured LLM provider.
//! - `GET  /admin/status`       — lifetime counts + server data-dir info.
//! - `GET  /admin/search?q=`    — FTS5 hits against the wiki index.
//! - `POST /admin/reorg`        — retro-fit sessions to per-cwd projects.
//! - `POST /admin/lint`         — run the M8 lint pass.
//! - `POST /admin/forget-sweep` — run the M8 retention sweep.
//! - `POST /admin/embed`        — backfill embeddings for latest pages.
//! - `POST /admin/commit`       — stage + commit the wiki tree via git.
//!
//! The CLI is responsible for filesystem access (collecting sources from
//! the project repo, rendering output for humans); the server is
//! responsible for all state reads/writes against the wiki + SQLite.

use std::sync::Arc;

use std::path::PathBuf;

use ai_memory_consolidate::{
    Bootstrap, BootstrapConfig, BootstrapOutcome, BootstrapSource, SourceCounts, run_lint,
    run_sweep,
};
use ai_memory_core::{ProjectId, SessionId, WorkspaceId};
use ai_memory_llm::{Embedder, LlmProvider};
use ai_memory_store::{DecayParams, ReaderPool, StoreError, WriterHandle, f32_vec_to_bytes};
use ai_memory_wiki::Wiki;
use axum::Json;
use axum::Router;
use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::{get, post};
use serde::{Deserialize, Serialize};
use tracing::warn;

/// Shared state for the admin router.
#[derive(Clone)]
pub struct AdminState {
    /// Writer actor handle — used to get-or-create workspace/project.
    pub writer: WriterHandle,
    /// Reader pool — used by the idempotency check inside Bootstrap.
    pub reader: ReaderPool,
    /// Wiki handle — pages are written here.
    pub wiki: Wiki,
    /// Optional LLM provider. When `None`, bootstrap returns 503.
    pub llm: Option<Arc<dyn LlmProvider>>,
    /// Optional embedder. When `None`, `/admin/embed` returns 503.
    pub embedder: Option<Arc<dyn Embedder>>,
    /// Retention-decay parameters forwarded from server config.
    pub decay_params: DecayParams,
    /// Server's resolved data directory (e.g. `/data` in the docker
    /// image). Surfaced via `/admin/status` so the CLI can report
    /// "where the wiki + db actually live".
    pub data_dir: PathBuf,
    /// Resolved SQLite path inside `data_dir`. Same purpose as above.
    pub db_path: PathBuf,
    /// Server's bind address — informational, surfaced in /admin/status.
    pub bind: String,
}

/// JSON request body for `POST /admin/bootstrap`.
#[derive(Deserialize)]
struct BootstrapRequest {
    /// Workspace name (auto-created if it doesn't exist).
    workspace: String,
    /// Project name (auto-created if it doesn't exist).
    project: String,
    /// Sources pre-collected on the client side.
    sources: Vec<BootstrapSource>,
    /// Maximum input tokens for LLM call.
    #[serde(default = "default_max_input_tokens")]
    max_input_tokens: usize,
    /// Skip the LLM call and page writes — returns a dry-run outcome.
    #[serde(default)]
    dry_run: bool,
    /// Allow re-bootstrap when `wiki/bootstrap.md` already exists.
    #[serde(default)]
    force: bool,
}

fn default_max_input_tokens() -> usize {
    50_000
}

/// Build the admin axum [`Router`]. Mounts:
/// - `POST /admin/bootstrap`
/// - `GET  /admin/status`
/// - `GET  /admin/search`
/// - `POST /admin/reorg`
/// - `POST /admin/lint`
/// - `POST /admin/forget-sweep`
/// - `POST /admin/embed`
/// - `POST /admin/commit`
pub fn admin_router(state: AdminState) -> Router {
    Router::new()
        .route("/admin/bootstrap", post(handle_bootstrap))
        .route("/admin/status", get(handle_status))
        .route("/admin/search", get(handle_search))
        .route("/admin/reorg", post(handle_reorg))
        .route("/admin/lint", post(handle_lint))
        .route("/admin/forget-sweep", post(handle_forget_sweep))
        .route("/admin/embed", post(handle_embed))
        .route("/admin/commit", post(handle_commit))
        .with_state(Arc::new(state))
}

// ---------------------------------------------------------------------
// status
// ---------------------------------------------------------------------

/// JSON response body for `GET /admin/status`. The CLI's `status`
/// subcommand renders this either as JSON (`--json`) or as a small
/// human-friendly text block.
#[derive(Debug, Serialize)]
pub struct StatusReport {
    /// Server binary version (Cargo package version).
    pub version: String,
    /// Absolute data directory the server uses (server-side path).
    pub data_dir: String,
    /// Bind address the HTTP transport is listening on.
    pub bind: String,
    /// Absolute SQLite path inside `data_dir`.
    pub db_path: String,
    /// Lifetime counts: pages_latest, pages_all, sessions, observations.
    pub counts: ai_memory_store::StatusCounts,
}

async fn handle_status(State(state): State<Arc<AdminState>>) -> impl IntoResponse {
    match state.reader.status_counts().await {
        Ok(counts) => {
            let report = StatusReport {
                version: env!("CARGO_PKG_VERSION").to_string(),
                data_dir: state.data_dir.display().to_string(),
                bind: state.bind.clone(),
                db_path: state.db_path.display().to_string(),
                counts,
            };
            (
                StatusCode::OK,
                Json(serde_json::to_value(&report).unwrap_or_else(|_| serde_json::json!({}))),
            )
        }
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({ "error": e.to_string() })),
        ),
    }
}

// ---------------------------------------------------------------------
// search
// ---------------------------------------------------------------------

/// Query string for `GET /admin/search?q=…&limit=…`.
#[derive(Debug, Deserialize)]
struct SearchQuery {
    /// FTS5 query expression.
    q: String,
    /// Max number of hits to return. Capped at 100 server-side.
    #[serde(default = "default_search_limit")]
    limit: usize,
}

fn default_search_limit() -> usize {
    10
}

async fn handle_search(
    State(state): State<Arc<AdminState>>,
    Query(query): Query<SearchQuery>,
) -> impl IntoResponse {
    let limit = query.limit.clamp(1, 100);
    match state.reader.search_pages(query.q, limit).await {
        Ok(hits) => (
            StatusCode::OK,
            Json(serde_json::to_value(&hits).unwrap_or_else(|_| serde_json::json!([]))),
        ),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({ "error": e.to_string() })),
        ),
    }
}

// ---------------------------------------------------------------------
// Internal helpers
// ---------------------------------------------------------------------

/// Build a 500 response carrying the given message.
fn internal_err(msg: impl Into<String>) -> (StatusCode, Json<serde_json::Value>) {
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        Json(serde_json::json!({ "error": msg.into() })),
    )
}

/// Resolve workspace + project IDs, creating them if absent. Returns
/// either the IDs or a ready-to-return error response.
async fn resolve_ws_proj(
    state: &AdminState,
    workspace: &str,
    project: &str,
) -> Result<(WorkspaceId, ProjectId), (StatusCode, Json<serde_json::Value>)> {
    let ws = state
        .writer
        .get_or_create_workspace(workspace.to_string())
        .await
        .map_err(|e| internal_err(format!("workspace: {e}")))?;
    let proj = state
        .writer
        .get_or_create_project(ws, project.to_string(), None)
        .await
        .map_err(|e| internal_err(format!("project: {e}")))?;
    Ok((ws, proj))
}

// ---------------------------------------------------------------------
// bootstrap
// ---------------------------------------------------------------------

async fn handle_bootstrap(
    State(state): State<Arc<AdminState>>,
    Json(req): Json<BootstrapRequest>,
) -> Result<impl IntoResponse, (StatusCode, Json<serde_json::Value>)> {
    // LLM is required only for live runs. Dry-runs never call the LLM
    // so we handle them directly here without constructing Bootstrap.
    if !req.dry_run && state.llm.is_none() {
        return Err((
            StatusCode::SERVICE_UNAVAILABLE,
            Json(serde_json::json!({
                "error": "LLM provider not configured on server"
            })),
        ));
    }

    // Resolve workspace + project — create if absent.
    let (ws, proj) = resolve_ws_proj(&state, &req.workspace, &req.project).await?;

    // Dry-run with no LLM: compute the budget-pruned source counts and
    // return early without constructing Bootstrap (which requires an LLM).
    if req.dry_run && state.llm.is_none() {
        return dry_run_outcome(req.sources, req.max_input_tokens);
    }

    let llm = Arc::clone(
        state
            .llm
            .as_ref()
            .expect("llm is Some: checked above (non-dry-run without LLM returns 503)"),
    );

    let cfg = BootstrapConfig {
        // repo_path is unused by process_sources — the path field is
        // only consumed by collect_sources on the client side.
        repo_path: std::path::PathBuf::new(),
        workspace_id: ws,
        project_id: proj,
        max_input_tokens: req.max_input_tokens,
        // The individual include_* flags don't matter here: sources
        // are already collected; process_sources ignores them.
        include_git: true,
        include_readme: true,
        include_docs: true,
        include_code: true,
        since: None,
        dry_run: req.dry_run,
        force: req.force,
    };

    let bootstrap = Bootstrap {
        reader: state.reader.clone(),
        wiki: state.wiki.clone(),
        llm,
    };

    match bootstrap.process_sources(&cfg, req.sources).await {
        Ok(outcome) => Ok((
            StatusCode::OK,
            Json(serde_json::to_value(&outcome).unwrap_or_else(|_| serde_json::json!({}))),
        )),
        Err(e) => Err(bootstrap_error_response(e)),
    }
}

/// Build a dry-run [`BootstrapOutcome`] without an LLM by applying the
/// same budget-pruning logic that `Bootstrap::process_sources` would use.
/// Map a [`BootstrapError`] to the appropriate HTTP status code.
///
/// - `NoSources` / `AlreadyBootstrapped` → 422 (validation failures).
/// - `Llm` → 502 (upstream provider failure).
/// - Everything else → 500 (unexpected server-side error).
fn bootstrap_error_response(
    e: ai_memory_consolidate::BootstrapError,
) -> (StatusCode, Json<serde_json::Value>) {
    use ai_memory_consolidate::BootstrapError;
    let status = match &e {
        BootstrapError::NoSources | BootstrapError::AlreadyBootstrapped => {
            StatusCode::UNPROCESSABLE_ENTITY
        }
        BootstrapError::Llm(_) => StatusCode::BAD_GATEWAY,
        _ => StatusCode::INTERNAL_SERVER_ERROR,
    };
    (status, Json(serde_json::json!({ "error": e.to_string() })))
}

/// Build a dry-run [`BootstrapOutcome`] without an LLM by applying the
/// same budget-pruning logic that `Bootstrap::process_sources` would use.
fn dry_run_outcome(
    sources: Vec<BootstrapSource>,
    max_input_tokens: usize,
) -> Result<(StatusCode, Json<serde_json::Value>), (StatusCode, Json<serde_json::Value>)> {
    use ai_memory_consolidate::BootstrapError;
    if sources.is_empty() {
        return Err((
            StatusCode::UNPROCESSABLE_ENTITY,
            Json(serde_json::json!({
                "error": BootstrapError::NoSources.to_string()
            })),
        ));
    }
    // Mirror the prune logic: sort by drop_priority desc, drop until
    // under budget. We replicate the constants here rather than
    // exposing prune_to_budget publicly (it's an implementation detail
    // of the consolidation pipeline).
    const CHARS_PER_TOKEN: usize = 4;
    let collected = sources.len();
    let usable = max_input_tokens.saturating_sub(1_000);
    let mut sorted = sources;
    sorted.sort_by_key(|s| std::cmp::Reverse(s.kind.drop_priority()));
    let mut total: usize = sorted
        .iter()
        .map(|s| (s.label.len() + s.text.len() + 16).div_ceil(CHARS_PER_TOKEN))
        .sum();
    while total > usable && !sorted.is_empty() {
        let victim_tokens =
            (sorted[0].label.len() + sorted[0].text.len() + 16).div_ceil(CHARS_PER_TOKEN);
        total = total.saturating_sub(victim_tokens);
        sorted.remove(0);
    }
    let kept = &sorted;
    let dropped = collected - kept.len();
    let counts = SourceCounts::from_sources(kept);
    let outcome = BootstrapOutcome {
        sources_collected: collected,
        sources_sent: kept.len(),
        sources_dropped: dropped,
        sources_by_kind: counts,
        estimated_input_tokens: total,
        pages_written: Vec::new(),
        rationale: "(dry-run; LLM not invoked)".to_string(),
        dry_run: true,
    };
    Ok((
        StatusCode::OK,
        Json(serde_json::to_value(&outcome).unwrap_or_else(|_| serde_json::json!({}))),
    ))
}

// ---------------------------------------------------------------------
// reorg
// ---------------------------------------------------------------------

/// JSON request body for `POST /admin/reorg`.
#[derive(Deserialize)]
struct ReorgRequest {
    /// Show what would change without writing.
    #[serde(default)]
    dry_run: bool,
}

/// One entry in the reorg plan (serialised in the response).
#[derive(Debug, Serialize)]
pub struct ReorgPlanEntry {
    /// Session UUID string.
    pub session_id: String,
    /// Working directory the session was started in.
    pub cwd: String,
    /// Basename-derived project name the session will move to.
    pub new_project: String,
}

/// Summary counts returned alongside the plan.
#[derive(Debug, Serialize)]
pub struct ReorgSummaryJson {
    /// Sessions whose `project_id` was changed.
    pub sessions_moved: usize,
    /// Observations updated to match their session's new project.
    pub observations_updated: usize,
    /// `is_latest=1` pages marked `is_latest=0` (mash-up graveyard).
    pub pages_graveyarded: usize,
    /// Number of distinct per-cwd projects referenced in the plan.
    pub distinct_new_projects: usize,
}

/// Full response for `POST /admin/reorg`.
#[derive(Debug, Serialize)]
pub struct ReorgReport {
    /// `true` when `dry_run` was requested.
    pub dry_run: bool,
    /// All sessions that need (or needed) moving, with their target project.
    pub plan: Vec<ReorgPlanEntry>,
    /// Counts after execution (zeros when `dry_run=true`).
    pub summary: ReorgSummaryJson,
}

async fn handle_reorg(
    State(state): State<Arc<AdminState>>,
    Json(req): Json<ReorgRequest>,
) -> impl IntoResponse {
    // Step 1: ensure the default workspace exists.
    let ws = match state.writer.get_or_create_workspace("default").await {
        Ok(id) => id,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({ "error": format!("workspace: {e}") })),
            );
        }
    };

    // Step 2: read all sessions with a non-NULL, non-empty cwd.
    let sessions_with_cwd: Vec<(SessionId, ProjectId, String)> = match state
        .reader
        .with_conn(move |conn| {
            let mut stmt = conn.prepare(
                "SELECT id, project_id, cwd \
                     FROM sessions \
                     WHERE cwd IS NOT NULL AND cwd != '' \
                     ORDER BY started_at",
            )?;
            let rows = stmt.query_map([], |row| {
                let id_bytes: Vec<u8> = row.get(0)?;
                let proj_bytes: Vec<u8> = row.get(1)?;
                let cwd: String = row.get(2)?;
                Ok((id_bytes, proj_bytes, cwd))
            })?;
            let mut out = Vec::new();
            for r in rows {
                let (id_bytes, proj_bytes, cwd) = r?;
                let sid = SessionId::from_slice(&id_bytes).map_err(StoreError::Memory)?;
                let pid = ProjectId::from_slice(&proj_bytes).map_err(StoreError::Memory)?;
                out.push((sid, pid, cwd));
            }
            Ok(out)
        })
        .await
    {
        Ok(v) => v,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({ "error": e.to_string() })),
            );
        }
    };

    if sessions_with_cwd.is_empty() {
        let report = ReorgReport {
            dry_run: req.dry_run,
            plan: Vec::new(),
            summary: ReorgSummaryJson {
                sessions_moved: 0,
                observations_updated: 0,
                pages_graveyarded: 0,
                distinct_new_projects: 0,
            },
        };
        return (
            StatusCode::OK,
            Json(serde_json::to_value(&report).unwrap_or_else(|_| serde_json::json!({}))),
        );
    }

    // Step 3: resolve target project per distinct cwd (basename-derived).
    let mut cwd_to_proj: std::collections::HashMap<String, (WorkspaceId, ProjectId, String)> =
        std::collections::HashMap::new();
    for (_, _, cwd) in &sessions_with_cwd {
        if cwd_to_proj.contains_key(cwd.as_str()) {
            continue;
        }
        let project_name = std::path::Path::new(cwd.as_str())
            .file_name()
            .and_then(|s| s.to_str())
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .unwrap_or_else(|| "unknown".to_string());
        let proj = match state
            .writer
            .get_or_create_project(ws, project_name.clone(), Some(cwd.clone()))
            .await
        {
            Ok(id) => id,
            Err(e) => {
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(serde_json::json!({ "error": format!("project: {e}") })),
                );
            }
        };
        cwd_to_proj.insert(cwd.clone(), (ws, proj, project_name));
    }

    // Step 4: build plan — sessions whose project_id already matches are skipped.
    let mut plan_entries: Vec<ReorgPlanEntry> = Vec::new();
    let mut writer_plan: Vec<(SessionId, ProjectId)> = Vec::new();
    for (session_id, old_project_id, cwd) in &sessions_with_cwd {
        let (_, new_project_id, project_name) = &cwd_to_proj[cwd.as_str()];
        if *new_project_id == *old_project_id {
            continue;
        }
        plan_entries.push(ReorgPlanEntry {
            session_id: session_id.to_string(),
            cwd: cwd.clone(),
            new_project: project_name.clone(),
        });
        writer_plan.push((*session_id, *new_project_id));
    }

    let distinct_new_projects: std::collections::HashSet<ProjectId> =
        writer_plan.iter().map(|(_, pid)| *pid).collect();

    // Step 5: dry-run → return the plan without writing.
    if req.dry_run || writer_plan.is_empty() {
        let report = ReorgReport {
            dry_run: req.dry_run,
            plan: plan_entries,
            summary: ReorgSummaryJson {
                sessions_moved: 0,
                observations_updated: 0,
                pages_graveyarded: 0,
                distinct_new_projects: distinct_new_projects.len(),
            },
        };
        return (
            StatusCode::OK,
            Json(serde_json::to_value(&report).unwrap_or_else(|_| serde_json::json!({}))),
        );
    }

    // Step 6: execute.
    let summary = match state.writer.reorg_sessions(writer_plan).await {
        Ok(s) => s,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({ "error": e.to_string() })),
            );
        }
    };

    let report = ReorgReport {
        dry_run: false,
        plan: plan_entries,
        summary: ReorgSummaryJson {
            sessions_moved: summary.sessions_moved,
            observations_updated: summary.observations_updated,
            pages_graveyarded: summary.pages_graveyarded,
            distinct_new_projects: distinct_new_projects.len(),
        },
    };
    (
        StatusCode::OK,
        Json(serde_json::to_value(&report).unwrap_or_else(|_| serde_json::json!({}))),
    )
}

// ---------------------------------------------------------------------
// lint
// ---------------------------------------------------------------------

/// JSON request body for `POST /admin/lint`.
#[derive(Deserialize)]
struct LintRequest {
    /// Workspace name (auto-created if absent).
    #[serde(default = "default_workspace")]
    workspace: String,
    /// Project name (auto-created if absent).
    #[serde(default = "default_project")]
    project: String,
    /// Don't write the lint report page.
    #[serde(default)]
    dry_run: bool,
}

fn default_workspace() -> String {
    "default".to_string()
}

fn default_project() -> String {
    "scratch".to_string()
}

async fn handle_lint(
    State(state): State<Arc<AdminState>>,
    Json(req): Json<LintRequest>,
) -> Result<impl IntoResponse, (StatusCode, Json<serde_json::Value>)> {
    let (ws, proj) = resolve_ws_proj(&state, &req.workspace, &req.project).await?;

    run_lint(
        &state.reader,
        &state.wiki,
        state.llm.as_ref(),
        ws,
        proj,
        req.dry_run,
    )
    .await
    .map(|report| {
        (
            StatusCode::OK,
            Json(serde_json::to_value(&report).unwrap_or_else(|_| serde_json::json!({}))),
        )
    })
    .map_err(|e| internal_err(e.to_string()))
}

// ---------------------------------------------------------------------
// forget-sweep
// ---------------------------------------------------------------------

/// JSON request body for `POST /admin/forget-sweep`.
#[derive(Deserialize)]
struct ForgetSweepRequest {
    /// Workspace name (auto-created if absent).
    #[serde(default = "default_workspace")]
    workspace: String,
    /// Project name (auto-created if absent).
    #[serde(default = "default_project")]
    project: String,
    /// Report what would be evicted without actually mutating.
    #[serde(default)]
    dry_run: bool,
}

async fn handle_forget_sweep(
    State(state): State<Arc<AdminState>>,
    Json(req): Json<ForgetSweepRequest>,
) -> Result<impl IntoResponse, (StatusCode, Json<serde_json::Value>)> {
    let (ws, proj) = resolve_ws_proj(&state, &req.workspace, &req.project).await?;

    run_sweep(
        &state.reader,
        &state.writer,
        ws,
        proj,
        &state.decay_params,
        req.dry_run,
    )
    .await
    .map(|report| {
        (
            StatusCode::OK,
            Json(serde_json::to_value(&report).unwrap_or_else(|_| serde_json::json!({}))),
        )
    })
    .map_err(|e| internal_err(e.to_string()))
}

// ---------------------------------------------------------------------
// embed
// ---------------------------------------------------------------------

/// JSON request body for `POST /admin/embed`.
#[derive(Deserialize)]
struct EmbedRequest {
    /// Workspace name (auto-created if absent).
    #[serde(default = "default_workspace")]
    workspace: String,
    /// Project name (auto-created if absent).
    #[serde(default = "default_project")]
    project: String,
    /// When true, regenerates embeddings even for pages that already
    /// have one matching the current (provider, model, dim).
    #[serde(default)]
    reembed: bool,
    /// When true, count pages that would be embedded/skipped without
    /// calling the embedder or writing anything.
    #[serde(default)]
    dry_run: bool,
}

/// Summary response from `POST /admin/embed`.
#[derive(Debug, Serialize)]
pub struct EmbedReport {
    /// Pages that were actually embedded (zero in dry-run).
    pub embedded: usize,
    /// Pages skipped because a matching embedding already existed.
    pub skipped: usize,
    /// Pages that failed to embed (read error or provider error).
    pub failed: usize,
    /// Pages that would be embedded in a live run (only meaningful
    /// when `dry_run` was requested).
    pub would_embed: usize,
    /// Provider name.
    pub provider: String,
    /// Model identifier.
    pub model: String,
    /// Embedding dimensionality.
    pub dim: u32,
}

async fn handle_embed(
    State(state): State<Arc<AdminState>>,
    Json(req): Json<EmbedRequest>,
) -> Result<impl IntoResponse, (StatusCode, Json<serde_json::Value>)> {
    let embedder = match state.embedder.clone() {
        Some(e) => e,
        None => {
            return Err((
                StatusCode::SERVICE_UNAVAILABLE,
                Json(serde_json::json!({
                    "error": "embedder not configured on server"
                })),
            ));
        }
    };

    let provider = embedder.provider().to_string();
    let model = embedder.model().to_string();
    let dim = embedder.dim();

    let (ws, proj) = resolve_ws_proj(&state, &req.workspace, &req.project).await?;

    let candidates = state
        .reader
        .decay_candidates(ws, proj)
        .await
        .map_err(|e| internal_err(e.to_string()))?;

    // Build the set of page ids that already have a matching embedding.
    let already: std::collections::HashSet<_> = if req.reembed {
        std::collections::HashSet::new()
    } else {
        state
            .reader
            .load_embeddings(ws, proj, provider.clone(), model.clone(), dim)
            .await
            .map_err(|e| internal_err(e.to_string()))?
            .into_iter()
            .map(|s| s.id)
            .collect()
    };

    let mut embedded = 0_usize;
    let mut skipped = 0_usize;
    let mut failed = 0_usize;
    let mut would_embed = 0_usize;

    for cand in candidates {
        if !req.reembed && already.contains(&cand.id) {
            skipped += 1;
            continue;
        }
        if req.dry_run {
            would_embed += 1;
            continue;
        }
        let md = match state.wiki.read_page(&cand.path) {
            Ok(m) => m,
            Err(e) => {
                warn!(path = %cand.path, error = %e, "embed: skip unreadable page");
                failed += 1;
                continue;
            }
        };
        let vec = match embedder.embed(&md.body).await {
            Ok(v) => v,
            Err(e) => {
                warn!(path = %cand.path, error = %e, "embed: provider call failed");
                failed += 1;
                continue;
            }
        };
        let bytes = f32_vec_to_bytes(&vec);
        if let Err(e) = state
            .writer
            .store_embedding(cand.id, bytes, provider.clone(), model.clone(), dim)
            .await
        {
            warn!(path = %cand.path, error = %e, "embed: store_embedding failed");
            failed += 1;
            continue;
        }
        embedded += 1;
    }

    let report = EmbedReport {
        embedded,
        skipped,
        failed,
        would_embed,
        provider,
        model,
        dim,
    };
    Ok((
        StatusCode::OK,
        Json(serde_json::to_value(&report).unwrap_or_else(|_| serde_json::json!({}))),
    ))
}

// ---------------------------------------------------------------------
// commit
// ---------------------------------------------------------------------

/// JSON request body for `POST /admin/commit`.
#[derive(Deserialize)]
struct CommitRequest {
    /// Commit message.
    message: String,
}

async fn handle_commit(
    State(state): State<Arc<AdminState>>,
    Json(req): Json<CommitRequest>,
) -> impl IntoResponse {
    match state.wiki.commit_all(&req.message) {
        Ok(Some(oid)) => (
            StatusCode::OK,
            Json(serde_json::json!({
                "committed": true,
                "oid": oid.to_string(),
            })),
        ),
        Ok(None) => (
            StatusCode::OK,
            Json(serde_json::json!({
                "committed": false,
                "reason": "nothing to commit",
            })),
        ),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({ "error": e.to_string() })),
        ),
    }
}
