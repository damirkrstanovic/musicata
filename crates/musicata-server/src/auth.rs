//! Multi-user authentication: argon2 password hashing, opaque cookie sessions, a per-user API
//! token (for Subsonic / programmatic clients), and the middleware that guards `/api/*`.
//!
//! Posture (LAN-first, defense-in-depth — see roadmap M12): when no users exist the app is in
//! **setup mode** (the API is open so the create-admin flow works); once an account exists, the
//! native API requires a session cookie (or a `?token=`/`Bearer` API token). The Subsonic
//! `/rest` surface keeps its own auth (`subsonic.rs`), authenticated against the same accounts.
//! Login passwords are argon2-hashed; session tokens are sha256-hashed at rest; the API token is
//! stored cleartext (Subsonic salted-token auth must recompute it).

use std::time::{SystemTime, UNIX_EPOCH};

use argon2::password_hash::{PasswordHash, PasswordHasher, PasswordVerifier, SaltString};
use argon2::Argon2;
use axum::extract::{Request, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use axum::{Extension, Json};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use musicata_storage::UserRecord;

use crate::{AppError, AppState};

pub const SESSION_COOKIE: &str = "musicata_session";
/// Sessions live 30 days; the cookie carries the same Max-Age.
const SESSION_TTL_SECONDS: i64 = 30 * 24 * 60 * 60;

pub const ROLE_ADMIN: &str = "admin";
pub const ROLE_LISTENER: &str = "listener";

fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or_default()
}

fn to_hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Cryptographically-random bytes (via the OS). Used for the argon2 salt and session/API tokens.
/// Direct `getrandom` rather than argon2's re-exported `OsRng`, which isn't available under
/// `--no-default-features` (nothing enables `rand_core`'s getrandom there).
fn random_bytes<const N: usize>() -> [u8; N] {
    let mut bytes = [0u8; N];
    getrandom::getrandom(&mut bytes).expect("OS RNG unavailable");
    bytes
}

/// Argon2id hash of a password as a PHC string.
fn hash_password(password: &str) -> Result<String, AppError> {
    let salt = SaltString::encode_b64(&random_bytes::<16>())
        .map_err(|error| AppError::internal(format!("salt: {error}")))?;
    Argon2::default()
        .hash_password(password.as_bytes(), &salt)
        .map(|hash| hash.to_string())
        .map_err(|error| AppError::internal(format!("hash password: {error}")))
}

fn verify_password(password: &str, hash: &str) -> bool {
    match PasswordHash::new(hash) {
        Ok(parsed) => Argon2::default()
            .verify_password(password.as_bytes(), &parsed)
            .is_ok(),
        Err(_) => false,
    }
}

/// A fresh 256-bit random secret as hex (session cookie value or API token).
fn generate_token() -> String {
    to_hex(&random_bytes::<32>())
}

/// sha256 of a session token — what we store, so a DB leak yields no live sessions.
fn hash_token(token: &str) -> String {
    to_hex(&Sha256::digest(token.as_bytes()))
}

/// The authenticated user, injected as a request extension by [`require_auth`].
#[derive(Clone, Debug)]
pub struct CurrentUser {
    pub id: String,
    pub username: String,
    pub role: String,
}

impl CurrentUser {
    pub fn is_admin(&self) -> bool {
        self.role == ROLE_ADMIN
    }
}

#[derive(Serialize)]
pub(crate) struct UserView {
    id: String,
    username: String,
    role: String,
}

impl From<&UserRecord> for UserView {
    fn from(user: &UserRecord) -> Self {
        Self {
            id: user.id.clone(),
            username: user.username.clone(),
            role: user.role.clone(),
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Request helpers
// ---------------------------------------------------------------------------------------------

fn cookie_value(headers: &HeaderMap, name: &str) -> Option<String> {
    let cookies = headers.get(header::COOKIE)?.to_str().ok()?;
    cookies.split(';').find_map(|pair| {
        let (key, value) = pair.split_once('=')?;
        (key.trim() == name).then(|| value.trim().to_string())
    })
}

fn query_token(uri: &axum::http::Uri) -> Option<String> {
    let query = uri.query()?;
    query.split('&').find_map(|pair| {
        let (key, value) = pair.split_once('=')?;
        (key == "token").then(|| value.to_string())
    })
}

fn bearer_token(headers: &HeaderMap) -> Option<String> {
    let value = headers.get(header::AUTHORIZATION)?.to_str().ok()?;
    value.strip_prefix("Bearer ").map(|t| t.trim().to_string())
}

/// Resolve credentials to a user via (1) the session cookie, then (2) an API token. Takes owned
/// strings (not `&Request`) so the future stays `Send` — holding a `&Request` across an await
/// would require `Request: Sync`, which its body isn't.
async fn resolve_user(
    state: &AppState,
    cookie: Option<String>,
    api_token: Option<String>,
) -> Option<CurrentUser> {
    let to_current = |user: UserRecord| CurrentUser {
        id: user.id,
        username: user.username,
        role: user.role,
    };
    if let Some(token) = cookie
        && let Ok(Some(user)) = state.database.session_user(&hash_token(&token), now()).await
    {
        return Some(to_current(user));
    }
    if let Some(token) = api_token
        && let Ok(Some(user)) = state.database.user_by_api_token(&token).await
    {
        return Some(to_current(user));
    }
    None
}

/// Admin-only API surfaces — user management and the configuration panels. The player
/// (listener) app never calls these; everything it needs only requires authentication.
fn is_admin_path(path: &str) -> bool {
    path.starts_with("/api/users")
        || path.starts_with("/api/sources")
        || path == "/api/settings"
        || path == "/api/history"
}

/// Always-open endpoints (the SPA must reach these before a session exists).
fn is_open_path(path: &str) -> bool {
    matches!(
        path,
        "/api/health" | "/api/auth/status" | "/api/auth/login" | "/api/auth/setup"
    )
}

/// If `path` is a player's own endpoint channel — `/api/players/{id}/{state,commands,ws}` —
/// return its player id. These are the channels a self-registered endpoint authenticates on
/// with its per-player token (M10); player *management* paths (PATCH/DELETE `/api/players/{id}`)
/// are excluded and still require a user.
fn player_channel_id(path: &str) -> Option<&str> {
    let rest = path.strip_prefix("/api/players/")?;
    let (id, tail) = rest.split_once('/')?;
    matches!(tail, "state" | "commands" | "ws").then_some(id)
}

/// Whether `path` is a track audio stream — `/api/tracks/{id}/stream`. A native endpoint
/// must fetch these to play, so a valid endpoint token authorizes them (M10).
fn is_stream_path(path: &str) -> bool {
    path.strip_prefix("/api/tracks/")
        .and_then(|rest| rest.split_once('/'))
        .is_some_and(|(_, tail)| tail == "stream")
}

/// Mint a per-player endpoint auth token (M10): returns `(cleartext, sha256_hash)`. The
/// cleartext is shown to the registrant once; only the hash is stored.
pub fn issue_player_token() -> (String, String) {
    let token = generate_token();
    let hash = hash_token(&token);
    (token, hash)
}

// ---------------------------------------------------------------------------------------------
// Middleware
// ---------------------------------------------------------------------------------------------

/// Guard `/api/*`. Non-API paths (the SPA, static assets, the Subsonic `/rest` surface) pass
/// through untouched. In setup mode (no users) the API is open so the first admin can be
/// created. Otherwise a valid session/token is required, and admin-only paths need the admin
/// role. The resolved [`CurrentUser`] is injected as a request extension.
pub async fn require_auth(state: AppState, mut request: Request, next: Next) -> Response {
    let path = request.uri().path().to_string();
    if !path.starts_with("/api/") || is_open_path(&path) {
        return next.run(request).await;
    }
    // Extract credentials up front (owned), so no `&Request` is held across an await below.
    let cookie = cookie_value(request.headers(), SESSION_COOKIE);
    let api_token = query_token(request.uri()).or_else(|| bearer_token(request.headers()));
    // Setup mode: no accounts yet — let the create-admin flow (and a first look) through. Fail
    // CLOSED on a DB error (only an explicit `Ok(0)` opens the API) so a transient query failure
    // can't drop auth on an instance that actually has users.
    if matches!(state.database.count_users().await, Ok(0)) {
        return next.run(request).await;
    }
    let Some(user) = resolve_user(&state, cookie, api_token.clone()).await else {
        // Fall back to per-player endpoint auth (M10): a self-registered endpoint authenticates
        // with just its player token (no user session) for (a) its own channels and (b) the
        // audio streams it must fetch to play. Only for players that actually have a token;
        // everything else stays user-gated.
        if let Some(token) = api_token {
            let hashed = hash_token(&token);
            if let Some(player_id) = player_channel_id(&path)
                && let Ok(Some(stored)) = state.database.player_auth_token_hash(player_id).await
                && stored == hashed
            {
                return next.run(request).await;
            }
            if is_stream_path(&path)
                && state.database.player_token_exists(&hashed).await.unwrap_or(false)
            {
                return next.run(request).await;
            }
        }
        return AppError::unauthorized("sign in to continue").into_response();
    };
    if is_admin_path(&path) && !user.is_admin() {
        return AppError::forbidden("administrator access required").into_response();
    }
    request.extensions_mut().insert(user);
    next.run(request).await
}

// ---------------------------------------------------------------------------------------------
// Cookies
// ---------------------------------------------------------------------------------------------

// SameSite=Lax + HttpOnly; no `Secure` so it works over plain http on the LAN (the documented
// deployment is LAN + VPN, not raw internet — see roadmap M12). A TLS reverse proxy can add it.
fn session_cookie(token: &str) -> String {
    format!("{SESSION_COOKIE}={token}; HttpOnly; SameSite=Lax; Path=/; Max-Age={SESSION_TTL_SECONDS}")
}

fn cleared_cookie() -> String {
    format!("{SESSION_COOKIE}=; HttpOnly; SameSite=Lax; Path=/; Max-Age=0")
}

async fn start_session(
    state: &AppState,
    user_id: &str,
    headers: &HeaderMap,
) -> Result<String, AppError> {
    let token = generate_token();
    let user_agent = headers
        .get(header::USER_AGENT)
        .and_then(|v| v.to_str().ok())
        .map(|s| s.chars().take(200).collect::<String>());
    let created = now();
    state
        .database
        .create_session(
            &hash_token(&token),
            user_id,
            created,
            created + SESSION_TTL_SECONDS,
            user_agent.as_deref(),
        )
        .await
        .map_err(|error| AppError::internal(error.to_string()))?;
    Ok(token)
}

// ---------------------------------------------------------------------------------------------
// Handlers
// ---------------------------------------------------------------------------------------------

#[derive(Serialize)]
pub struct AuthStatus {
    /// True when no accounts exist yet — the UI shows the create-admin screen.
    setup_required: bool,
}

/// Open: does the instance need its first admin created?
pub async fn auth_status(State(state): State<AppState>) -> Result<Json<AuthStatus>, AppError> {
    let count = state
        .database
        .count_users()
        .await
        .map_err(|error| AppError::internal(error.to_string()))?;
    Ok(Json(AuthStatus {
        setup_required: count == 0,
    }))
}

#[derive(Deserialize)]
pub struct Credentials {
    username: String,
    password: String,
}

fn validate_credentials(username: &str, password: &str) -> Result<(), AppError> {
    if username.trim().is_empty() {
        return Err(AppError::bad_request("username is required"));
    }
    if password.len() < 8 {
        return Err(AppError::bad_request("password must be at least 8 characters"));
    }
    Ok(())
}

#[derive(Serialize)]
pub struct LoginResponse {
    user: UserView,
}

/// Open, but only valid while no users exist: create the first account as an admin and sign in.
pub async fn setup(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<Credentials>,
) -> Result<Response, AppError> {
    if state.database.count_users().await.map_err(internal)? != 0 {
        return Err(AppError::bad_request("setup already completed"));
    }
    let username = body.username.trim();
    validate_credentials(username, &body.password)?;
    let user = UserRecord {
        id: generate_token(),
        username: username.to_string(),
        password_hash: hash_password(&body.password)?,
        role: ROLE_ADMIN.to_string(),
        api_token: generate_token(),
        created_at_unix_seconds: now(),
    };
    state.database.create_user(&user).await.map_err(internal)?;
    let token = start_session(&state, &user.id, &headers).await?;
    Ok((
        [(header::SET_COOKIE, session_cookie(&token))],
        Json(LoginResponse {
            user: UserView::from(&user),
        }),
    )
        .into_response())
}

/// Open: verify credentials and start a session.
pub async fn login(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<Credentials>,
) -> Result<Response, AppError> {
    // Opportunistic cleanup of expired sessions.
    let _ = state.database.prune_expired_sessions(now()).await;
    let user = state
        .database
        .user_by_username(body.username.trim())
        .await
        .map_err(internal)?;
    let user = match user {
        Some(user) if verify_password(&body.password, &user.password_hash) => user,
        _ => return Err(AppError::unauthorized("wrong username or password")),
    };
    let token = start_session(&state, &user.id, &headers).await?;
    Ok((
        [(header::SET_COOKIE, session_cookie(&token))],
        Json(LoginResponse {
            user: UserView::from(&user),
        }),
    )
        .into_response())
}

/// End the current session and clear the cookie.
pub async fn logout(State(state): State<AppState>, headers: HeaderMap) -> Response {
    if let Some(token) = cookie_value(&headers, SESSION_COOKIE) {
        let _ = state.database.delete_session(&hash_token(&token)).await;
    }
    ([(header::SET_COOKIE, cleared_cookie())], StatusCode::NO_CONTENT).into_response()
}

/// The signed-in user.
pub async fn me(Extension(user): Extension<CurrentUser>) -> Json<UserView> {
    Json(UserView {
        id: user.id,
        username: user.username,
        role: user.role,
    })
}

#[derive(Serialize)]
pub struct TokenResponse {
    /// The user's API token — paste into a Subsonic client (as the password) or `?token=`.
    api_token: String,
}

/// Reveal the current user's API token (for setting up a Subsonic/OpenSubsonic client).
pub async fn my_token(
    State(state): State<AppState>,
    Extension(user): Extension<CurrentUser>,
) -> Result<Json<TokenResponse>, AppError> {
    let record = state
        .database
        .user_by_id(&user.id)
        .await
        .map_err(internal)?
        .ok_or_else(|| AppError::unauthorized("session no longer valid"))?;
    Ok(Json(TokenResponse {
        api_token: record.api_token,
    }))
}

/// Rotate (regenerate) the current user's API token — invalidates the old one immediately.
pub async fn rotate_token(
    State(state): State<AppState>,
    Extension(user): Extension<CurrentUser>,
) -> Result<Json<TokenResponse>, AppError> {
    let token = generate_token();
    state
        .database
        .set_user_api_token(&user.id, &token)
        .await
        .map_err(internal)?;
    Ok(Json(TokenResponse { api_token: token }))
}

#[derive(Deserialize)]
pub struct ChangePassword {
    current_password: String,
    new_password: String,
}

/// Change the current user's password (verifying the old one).
pub async fn change_password(
    State(state): State<AppState>,
    Extension(user): Extension<CurrentUser>,
    Json(body): Json<ChangePassword>,
) -> Result<StatusCode, AppError> {
    let record = state
        .database
        .user_by_id(&user.id)
        .await
        .map_err(internal)?
        .ok_or_else(|| AppError::unauthorized("session no longer valid"))?;
    if !verify_password(&body.current_password, &record.password_hash) {
        return Err(AppError::unauthorized("current password is wrong"));
    }
    if body.new_password.len() < 8 {
        return Err(AppError::bad_request("password must be at least 8 characters"));
    }
    let hash = hash_password(&body.new_password)?;
    state.database.set_user_password(&user.id, &hash).await.map_err(internal)?;
    Ok(StatusCode::NO_CONTENT)
}

// ---- User management (admin-only, gated by the middleware on /api/users) ----

#[derive(Serialize)]
pub struct UserListResponse {
    users: Vec<UserView>,
}

pub async fn list_users(State(state): State<AppState>) -> Result<Json<UserListResponse>, AppError> {
    let users = state.database.list_users().await.map_err(internal)?;
    Ok(Json(UserListResponse {
        users: users.iter().map(UserView::from).collect(),
    }))
}

#[derive(Deserialize)]
pub struct CreateUser {
    username: String,
    password: String,
    role: Option<String>,
}

pub async fn create_user(
    State(state): State<AppState>,
    Json(body): Json<CreateUser>,
) -> Result<Json<UserView>, AppError> {
    let username = body.username.trim();
    validate_credentials(username, &body.password)?;
    if state
        .database
        .user_by_username(username)
        .await
        .map_err(internal)?
        .is_some()
    {
        return Err(AppError::bad_request("a user with that name already exists"));
    }
    let role = match body.role.as_deref() {
        Some(ROLE_ADMIN) => ROLE_ADMIN,
        _ => ROLE_LISTENER,
    };
    let user = UserRecord {
        id: generate_token(),
        username: username.to_string(),
        password_hash: hash_password(&body.password)?,
        role: role.to_string(),
        api_token: generate_token(),
        created_at_unix_seconds: now(),
    };
    state.database.create_user(&user).await.map_err(internal)?;
    Ok(Json(UserView::from(&user)))
}

#[derive(Deserialize)]
pub struct UpdateUser {
    role: Option<String>,
    password: Option<String>,
}

pub async fn update_user(
    State(state): State<AppState>,
    axum::extract::Path(id): axum::extract::Path<String>,
    Json(body): Json<UpdateUser>,
) -> Result<StatusCode, AppError> {
    let target = state
        .database
        .user_by_id(&id)
        .await
        .map_err(internal)?
        .ok_or_else(|| AppError::not_found("unknown user"))?;
    if let Some(role) = body.role.as_deref() {
        let role = if role == ROLE_ADMIN { ROLE_ADMIN } else { ROLE_LISTENER };
        // Don't let the last admin demote themselves into a lockout.
        if target.role == ROLE_ADMIN && role == ROLE_LISTENER && admin_count(&state).await? <= 1 {
            return Err(AppError::bad_request("can't demote the only administrator"));
        }
        state.database.set_user_role(&id, role).await.map_err(internal)?;
    }
    if let Some(password) = body.password {
        if password.len() < 8 {
            return Err(AppError::bad_request("password must be at least 8 characters"));
        }
        let hash = hash_password(&password)?;
        state.database.set_user_password(&id, &hash).await.map_err(internal)?;
    }
    Ok(StatusCode::NO_CONTENT)
}

pub async fn delete_user(
    State(state): State<AppState>,
    Extension(current): Extension<CurrentUser>,
    axum::extract::Path(id): axum::extract::Path<String>,
) -> Result<StatusCode, AppError> {
    let target = state
        .database
        .user_by_id(&id)
        .await
        .map_err(internal)?
        .ok_or_else(|| AppError::not_found("unknown user"))?;
    if target.role == ROLE_ADMIN && admin_count(&state).await? <= 1 {
        return Err(AppError::bad_request("can't remove the only administrator"));
    }
    if target.id == current.id {
        return Err(AppError::bad_request("you can't remove your own account"));
    }
    state.database.delete_user(&id).await.map_err(internal)?;
    Ok(StatusCode::NO_CONTENT)
}

async fn admin_count(state: &AppState) -> Result<usize, AppError> {
    let users = state.database.list_users().await.map_err(internal)?;
    Ok(users.iter().filter(|u| u.role == ROLE_ADMIN).count())
}

fn internal(error: impl std::fmt::Display) -> AppError {
    AppError::internal(error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn password_hash_round_trips() {
        let hash = hash_password("correct horse battery staple").unwrap();
        assert!(verify_password("correct horse battery staple", &hash));
        assert!(!verify_password("wrong", &hash));
    }

    #[test]
    fn tokens_are_unique_and_hashable() {
        let a = generate_token();
        let b = generate_token();
        assert_ne!(a, b);
        assert_eq!(a.len(), 64); // 32 bytes hex
        assert_eq!(hash_token(&a), hash_token(&a));
        assert_ne!(hash_token(&a), hash_token(&b));
    }

    #[test]
    fn admin_and_open_path_classification() {
        assert!(is_admin_path("/api/users"));
        assert!(is_admin_path("/api/sources/x"));
        assert!(is_admin_path("/api/settings"));
        assert!(!is_admin_path("/api/tracks"));
        assert!(!is_admin_path("/api/playlists"));
        assert!(is_open_path("/api/auth/login"));
        assert!(!is_open_path("/api/auth/me"));
    }

    #[test]
    fn player_channel_classification() {
        assert_eq!(player_channel_id("/api/players/mpd/state"), Some("mpd"));
        assert_eq!(player_channel_id("/api/players/mpd/commands"), Some("mpd"));
        assert_eq!(player_channel_id("/api/players/abc-1/ws"), Some("abc-1"));
        // Management paths and unknown tails are NOT endpoint channels (stay user-gated).
        assert_eq!(player_channel_id("/api/players/mpd"), None);
        assert_eq!(player_channel_id("/api/players"), None);
        assert_eq!(player_channel_id("/api/players/mpd/rename"), None);
        assert_eq!(player_channel_id("/api/tracks"), None);
    }

    #[test]
    fn stream_path_classification() {
        assert!(is_stream_path("/api/tracks/abc-1/stream"));
        assert!(!is_stream_path("/api/tracks/abc-1"));
        assert!(!is_stream_path("/api/tracks"));
        assert!(!is_stream_path("/api/players/mpd/stream"));
    }

    #[test]
    fn issued_player_token_hash_matches() {
        let (token, hash) = issue_player_token();
        assert_eq!(token.len(), 64);
        assert_eq!(hash, hash_token(&token));
        assert_ne!(token, hash);
    }

    #[test]
    fn cookie_parsing() {
        let mut headers = HeaderMap::new();
        headers.insert(header::COOKIE, "foo=bar; musicata_session=abc123; x=y".parse().unwrap());
        assert_eq!(cookie_value(&headers, SESSION_COOKIE), Some("abc123".to_string()));
        assert_eq!(cookie_value(&headers, "missing"), None);
    }
}
