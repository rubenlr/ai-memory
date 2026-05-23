//! Bearer-token authorization middleware for the HTTP server.
//!
//! When `[auth].bearer_token` (or the `AI_MEMORY_AUTH_TOKEN` env var)
//! is set, every request to `/mcp`, `/hook`, and `/handoff` must
//! carry an `Authorization: Bearer <token>` header that matches.
//!
//! When the token is *unset*, the middleware is a no-op — preserving
//! the zero-config local-development experience and keeping the
//! existing e2e + unit tests working.
//!
//! Comparison uses [`subtle::ConstantTimeEq`] so an attacker on the
//! same LAN cannot use response-time leaks to recover the token byte
//! by byte. The constant-time guarantee depends on both sides being
//! the same length; `subtle` returns a constant-cost `Choice::from(0)`
//! when lengths differ, which is the right thing here.
//!
//! Wire shape matches the MCP authorization spec
//! (modelcontextprotocol.io/specification/.../basic/authorization):
//! 401 responses include a `WWW-Authenticate: Bearer …` header so
//! conformant clients can detect missing/expired credentials.
//!
//! ## Why not OAuth
//!
//! The MCP spec mandates full OAuth 2.1 for HTTP-authenticated
//! servers. That's overkill for a single-user homelab and would
//! force every MCP client config to deal with authorization-server
//! discovery + PKCE + token refresh. A static bearer token is
//! wire-compatible with the spec's `Authorization: Bearer …` shape
//! (clients send the header the same way; they just don't run the
//! OAuth dance to obtain the token). Every supported client
//! (Claude Code, Codex, OpenCode, Cursor, Claude Desktop via
//! `mcp-remote`, Gemini CLI, OpenClaw) accepts a static
//! `Authorization` header in its config.

use std::sync::Arc;

use axum::extract::State;
use axum::http::{Method, Request, StatusCode, header};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use subtle::ConstantTimeEq;
use tracing::debug;

/// Cookie name used by the browser-friendly `?_token=` flow.
const AUTH_COOKIE: &str = "ai_memory_auth";
/// Query-param the browser flow accepts ONCE — middleware verifies
/// the value, sets the cookie, then redirects to the same URL with
/// `_token` stripped so the secret doesn't linger in history /
/// bookmarks / referer headers.
const AUTH_QUERY_PARAM: &str = "_token";

/// Shared auth state. Cheap to clone — just an `Arc` wrapping the
/// optional configured token.
#[derive(Clone, Debug)]
pub struct AuthState {
    expected: Option<String>,
}

impl AuthState {
    /// Build state from the (optional) configured token. `None` means
    /// "auth disabled, accept everything".
    #[must_use]
    pub fn new(expected: Option<String>) -> Self {
        Self { expected }
    }

    /// True when a token is configured (i.e. the middleware is doing
    /// anything). Useful for the startup log line so the operator
    /// sees whether their server is open or closed.
    #[must_use]
    pub fn enabled(&self) -> bool {
        self.expected.is_some()
    }
}

/// axum middleware closure. Wire with
/// `axum::middleware::from_fn_with_state(state, require_bearer)`.
///
/// Token sources, in priority order:
/// 1. `Authorization: Bearer <token>` header. Works for any method.
///    This is what the MCP + hook clients send.
/// 2. **GET only:** `ai_memory_auth` cookie. Set automatically by
///    the redirect path below; persists across navigation so the
///    browser doesn't need a header-rewriting extension.
/// 3. **GET only:** `?_token=<token>` query parameter. On match the
///    middleware sets the cookie + 303-redirects to the same URL
///    with `_token` stripped, so the secret doesn't get bookmarked,
///    logged in referer headers, or sit in browser history.
///
/// POST / PUT / DELETE / etc. require the header — that confines
/// cookie-bearing requests to read-only operations and keeps the
/// CSRF surface of cookie auth small (a malicious page on another
/// origin can ride the cookie into GETs, but `/mcp` and `/hook` are
/// POST-only, so the worst it can do is render /web pages the user
/// could already see).
pub async fn require_bearer(
    State(state): State<Arc<AuthState>>,
    req: Request<axum::body::Body>,
    next: Next,
) -> Response {
    let Some(expected) = state.expected.as_deref() else {
        return next.run(req).await;
    };

    let from_header = extract_bearer_header(&req);
    let is_get = req.method() == Method::GET;
    let from_cookie = if is_get { extract_cookie(&req) } else { None };
    let from_query = if is_get {
        extract_query_token(&req)
    } else {
        None
    };

    let provided = from_header
        .as_deref()
        .or(from_cookie.as_deref())
        .or(from_query.as_deref())
        .unwrap_or("");

    if !bool::from(provided.as_bytes().ct_eq(expected.as_bytes())) {
        debug!("auth rejected: invalid or missing bearer token");
        return unauthorized();
    }

    // Browser-friendly handoff: when the token arrived ONLY via the
    // query string, swap it for a cookie + redirect. Subsequent
    // navigation (and inlined assets like /static/tailwind.css) ride
    // the cookie, so the token never appears in the visible URL bar
    // after the first hop.
    if from_header.is_none() && from_cookie.is_none() && from_query.is_some() {
        return redirect_with_cookie(&req, provided);
    }

    next.run(req).await
}

fn extract_bearer_header(req: &Request<axum::body::Body>) -> Option<String> {
    let h = req.headers().get(header::AUTHORIZATION)?.to_str().ok()?;
    // Accept both "Bearer xxx" and "bearer xxx" (case-insensitive
    // scheme per RFC 7235 §2.1).
    let (scheme, value) = h.split_once(' ')?;
    if scheme.eq_ignore_ascii_case("Bearer") {
        Some(value.trim_start().to_string())
    } else {
        None
    }
}

fn extract_cookie(req: &Request<axum::body::Body>) -> Option<String> {
    let h = req.headers().get(header::COOKIE)?.to_str().ok()?;
    for pair in h.split(';') {
        let pair = pair.trim();
        if let Some(val) = pair.strip_prefix(&format!("{AUTH_COOKIE}=")) {
            return Some(val.to_string());
        }
    }
    None
}

fn extract_query_token(req: &Request<axum::body::Body>) -> Option<String> {
    let q = req.uri().query()?;
    for pair in q.split('&') {
        if let Some(val) = pair.strip_prefix(&format!("{AUTH_QUERY_PARAM}=")) {
            return Some(val.to_string());
        }
    }
    None
}

fn redirect_with_cookie(req: &Request<axum::body::Body>, token: &str) -> Response {
    let path = req.uri().path();
    let cleaned_query: String = req
        .uri()
        .query()
        .unwrap_or("")
        .split('&')
        .filter(|p| !p.starts_with(&format!("{AUTH_QUERY_PARAM}=")) && !p.is_empty())
        .collect::<Vec<_>>()
        .join("&");
    let target = if cleaned_query.is_empty() {
        path.to_string()
    } else {
        format!("{path}?{cleaned_query}")
    };
    // 30-day Max-Age — long enough that re-typing the bookmark URL
    // every month is rare. HttpOnly hides it from any inline JS;
    // SameSite=Lax keeps cross-site POSTs from riding it.
    // We deliberately don't set Secure: homelab deployments are
    // often plain HTTP on a LAN. A reverse proxy that terminates
    // TLS upstream is the right place to add Secure if exposed
    // publicly.
    let cookie = format!("{AUTH_COOKIE}={token}; HttpOnly; SameSite=Lax; Path=/; Max-Age=2592000");
    let mut resp = (StatusCode::SEE_OTHER, "").into_response();
    resp.headers_mut().insert(
        header::LOCATION,
        target.parse().expect("target path is a valid header value"),
    );
    resp.headers_mut().insert(
        header::SET_COOKIE,
        cookie
            .parse()
            .expect("cookie value is a valid header value"),
    );
    resp
}

fn unauthorized() -> Response {
    let mut resp = (StatusCode::UNAUTHORIZED, "auth required\n").into_response();
    resp.headers_mut().insert(
        header::WWW_AUTHENTICATE,
        // MCP spec wants this header; clients use it to disambiguate
        // "missing token" from "wrong token" and surface a helpful
        // diagnostic. The `realm` is informational.
        "Bearer realm=\"ai-memory\", error=\"invalid_token\""
            .parse()
            .expect("static header value is valid"),
    );
    resp
}

/// Generate a fresh random bearer token, hex-encoded.
///
/// `bytes` is the entropy budget; 32 bytes (256 bits) is plenty for
/// any conceivable threat model.
///
/// # Errors
/// Propagates failures from the OS RNG.
pub fn generate_token_hex(bytes: usize) -> Result<String, getrandom::Error> {
    let mut buf = vec![0u8; bytes];
    getrandom::fill(&mut buf)?;
    Ok(hex_encode(&buf))
}

fn hex_encode(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut s = String::with_capacity(bytes.len() * 2);
    for &b in bytes {
        s.push(HEX[(b >> 4) as usize] as char);
        s.push(HEX[(b & 0x0f) as usize] as char);
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::Router;
    use axum::body::Body;
    use axum::http::Request;
    use axum::routing::get;
    use tower::ServiceExt;

    fn router_with_auth(token: Option<&str>) -> Router {
        let state = Arc::new(AuthState::new(token.map(str::to_string)));
        Router::new()
            .route("/probe", get(|| async { "ok" }))
            .layer(axum::middleware::from_fn_with_state(state, require_bearer))
    }

    #[tokio::test]
    async fn no_token_configured_passes_anything_through() {
        let r = router_with_auth(None);
        let resp = r
            .oneshot(
                Request::builder()
                    .uri("/probe")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn missing_header_returns_401_with_www_authenticate() {
        let r = router_with_auth(Some("secret"));
        let resp = r
            .oneshot(
                Request::builder()
                    .uri("/probe")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
        let www = resp.headers().get(header::WWW_AUTHENTICATE).unwrap();
        assert!(www.to_str().unwrap().contains("Bearer"));
    }

    #[tokio::test]
    async fn wrong_token_returns_401() {
        let r = router_with_auth(Some("the-right-one"));
        let resp = r
            .oneshot(
                Request::builder()
                    .uri("/probe")
                    .header("Authorization", "Bearer the-wrong-one")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn right_token_returns_200() {
        let r = router_with_auth(Some("right-token"));
        let resp = r
            .oneshot(
                Request::builder()
                    .uri("/probe")
                    .header("Authorization", "Bearer right-token")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn lowercase_scheme_is_accepted() {
        let r = router_with_auth(Some("right-token"));
        let resp = r
            .oneshot(
                Request::builder()
                    .uri("/probe")
                    .header("Authorization", "bearer right-token")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn non_bearer_scheme_is_rejected() {
        let r = router_with_auth(Some("right-token"));
        let resp = r
            .oneshot(
                Request::builder()
                    .uri("/probe")
                    .header("Authorization", "Basic dXNlcjpwYXNz")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn cookie_with_right_token_passes_get() {
        let r = router_with_auth(Some("right-token"));
        let resp = r
            .oneshot(
                Request::builder()
                    .uri("/probe")
                    .header("Cookie", "ai_memory_auth=right-token")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn cookie_with_wrong_token_fails() {
        let r = router_with_auth(Some("right-token"));
        let resp = r
            .oneshot(
                Request::builder()
                    .uri("/probe")
                    .header("Cookie", "ai_memory_auth=wrong-token")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn cookie_ignored_on_post() {
        // POST routes must use Bearer header; cookie auth is GET-only
        // to keep the CSRF surface confined to read paths.
        let state = Arc::new(AuthState::new(Some("right-token".to_string())));
        let r = Router::new()
            .route("/probe", axum::routing::post(|| async { "ok" }))
            .layer(axum::middleware::from_fn_with_state(state, require_bearer));
        let resp = r
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/probe")
                    .header("Cookie", "ai_memory_auth=right-token")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn query_token_redirects_and_sets_cookie() {
        let r = router_with_auth(Some("right-token"));
        let resp = r
            .oneshot(
                Request::builder()
                    .uri("/probe?_token=right-token")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::SEE_OTHER);
        let loc = resp
            .headers()
            .get(header::LOCATION)
            .expect("redirect target")
            .to_str()
            .unwrap();
        assert_eq!(loc, "/probe");
        let cookie = resp
            .headers()
            .get(header::SET_COOKIE)
            .expect("set-cookie")
            .to_str()
            .unwrap();
        assert!(cookie.contains("ai_memory_auth=right-token"));
        assert!(cookie.contains("HttpOnly"));
        assert!(cookie.contains("SameSite=Lax"));
        assert!(cookie.contains("Path=/"));
    }

    #[tokio::test]
    async fn query_token_preserves_other_params() {
        let r = router_with_auth(Some("right-token"));
        let resp = r
            .oneshot(
                Request::builder()
                    .uri("/probe?foo=bar&_token=right-token&baz=qux")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::SEE_OTHER);
        let loc = resp
            .headers()
            .get(header::LOCATION)
            .unwrap()
            .to_str()
            .unwrap();
        // _token stripped; foo + baz preserved (order doesn't matter
        // but we filter sequentially so order is stable).
        assert_eq!(loc, "/probe?foo=bar&baz=qux");
    }

    #[tokio::test]
    async fn query_token_wrong_value_returns_401() {
        let r = router_with_auth(Some("right-token"));
        let resp = r
            .oneshot(
                Request::builder()
                    .uri("/probe?_token=wrong-token")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }

    #[test]
    fn generated_token_is_hex_and_correct_length() {
        let t = generate_token_hex(32).unwrap();
        assert_eq!(t.len(), 64);
        assert!(t.chars().all(|c| c.is_ascii_hexdigit()));
        // Distinct calls produce distinct tokens (modulo OS RNG bugs).
        let t2 = generate_token_hex(32).unwrap();
        assert_ne!(t, t2);
    }
}
