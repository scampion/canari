use anyhow::Context as _;
use argon2::password_hash::{PasswordHash, PasswordHasher, PasswordVerifier, SaltString};
use argon2::Argon2;
use axum::extract::{Request, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::middleware::Next;
use axum::response::{IntoResponse, Redirect, Response};
use base64::Engine as _;
use sha2::{Digest, Sha256};
use sqlx::SqlitePool;

use crate::model::now;
use crate::state::AppState;

pub const SESSION_COOKIE: &str = "canari_session";
const SESSION_TTL: i64 = 30 * 86_400;
const PASSWORD_KEY: &str = "password_hash";

const B64: base64::engine::general_purpose::GeneralPurpose =
    base64::engine::general_purpose::URL_SAFE_NO_PAD;

/// Store the operator password. Argon2 is deliberately slow, so it runs on the
/// blocking pool rather than stalling an async worker.
pub async fn set_password(db: &SqlitePool, plain: &str) -> anyhow::Result<()> {
    if plain.len() < 8 {
        anyhow::bail!("password must be at least 8 characters");
    }

    let plain = plain.to_owned();
    // Salt drawn from the same CSPRNG as session tokens, rather than pulling in
    // argon2's optional rand feature for one call.
    let salt_bytes: [u8; 16] = rand::random();
    let hash = tokio::task::spawn_blocking(move || {
        let salt =
            SaltString::encode_b64(&salt_bytes).map_err(|e| anyhow::anyhow!("salt: {e}"))?;
        Argon2::default()
            .hash_password(plain.as_bytes(), &salt)
            .map(|h| h.to_string())
            .map_err(|e| anyhow::anyhow!("hashing password: {e}"))
    })
    .await??;

    sqlx::query(
        "INSERT INTO settings (key, value) VALUES (?, ?) \
         ON CONFLICT (key) DO UPDATE SET value = excluded.value",
    )
    .bind(PASSWORD_KEY)
    .bind(&hash)
    .execute(db)
    .await?;

    // Changing the password logs every existing session out.
    sqlx::query("DELETE FROM sessions").execute(db).await?;
    Ok(())
}

pub async fn password_is_set(db: &SqlitePool) -> anyhow::Result<bool> {
    Ok(stored_hash(db).await?.is_some())
}

async fn stored_hash(db: &SqlitePool) -> anyhow::Result<Option<String>> {
    let hash = sqlx::query_scalar::<_, String>("SELECT value FROM settings WHERE key = ?")
        .bind(PASSWORD_KEY)
        .fetch_optional(db)
        .await?;
    Ok(hash)
}

pub async fn verify_password(db: &SqlitePool, plain: &str) -> anyhow::Result<bool> {
    let Some(hash) = stored_hash(db).await? else {
        return Ok(false);
    };

    let plain = plain.to_owned();
    let ok = tokio::task::spawn_blocking(move || {
        let parsed = match PasswordHash::new(&hash) {
            Ok(parsed) => parsed,
            Err(err) => {
                tracing::error!(%err, "stored password hash is unreadable");
                return false;
            }
        };
        Argon2::default()
            .verify_password(plain.as_bytes(), &parsed)
            .is_ok()
    })
    .await?;

    Ok(ok)
}

/// Create a session and return the raw token to put in the cookie. Only its
/// hash is persisted.
pub async fn create_session(db: &SqlitePool) -> anyhow::Result<String> {
    let token = generate_secret();
    let ts = now();

    sqlx::query("INSERT INTO sessions (token_hash, created_at, expires_at) VALUES (?, ?, ?)")
        .bind(hash_secret(&token))
        .bind(ts)
        .bind(ts + SESSION_TTL)
        .execute(db)
        .await?;

    // Opportunistic cleanup; there is no separate reaper task to keep alive.
    sqlx::query("DELETE FROM sessions WHERE expires_at < ?")
        .bind(ts)
        .execute(db)
        .await?;

    Ok(token)
}

pub async fn session_is_valid(db: &SqlitePool, token: &str) -> anyhow::Result<bool> {
    let count = sqlx::query_scalar::<_, i64>(
        "SELECT COUNT(*) FROM sessions WHERE token_hash = ? AND expires_at > ?",
    )
    .bind(hash_secret(token))
    .bind(now())
    .fetch_one(db)
    .await?;
    Ok(count > 0)
}

pub async fn destroy_session(db: &SqlitePool, token: &str) -> anyhow::Result<()> {
    sqlx::query("DELETE FROM sessions WHERE token_hash = ?")
        .bind(hash_secret(token))
        .execute(db)
        .await?;
    Ok(())
}

/// Shared by session tokens and API keys: both are high-entropy secrets, so a
/// plain SHA-256 is enough — there is nothing to brute-force.
pub fn hash_secret(secret: &str) -> String {
    B64.encode(Sha256::digest(secret.as_bytes()))
}

/// Generate a URL-safe secret with 256 bits of entropy.
pub fn generate_secret() -> String {
    let bytes: [u8; 32] = rand::random();
    B64.encode(bytes)
}

/// Read our session cookie out of the request headers.
pub fn token_from_headers(headers: &HeaderMap) -> Option<String> {
    let cookies = headers.get(header::COOKIE)?.to_str().ok()?;
    cookies.split(';').find_map(|pair| {
        let (name, value) = pair.split_once('=')?;
        (name.trim() == SESSION_COOKIE).then(|| value.trim().to_owned())
    })
}

/// `Secure` is set only for https deployments — otherwise a local http install
/// would silently drop the cookie and never log anyone in.
pub fn session_cookie(token: &str, site_url: &str) -> String {
    let secure = if site_url.starts_with("https://") {
        "; Secure"
    } else {
        ""
    };
    format!(
        "{SESSION_COOKIE}={token}; Path=/; HttpOnly; SameSite=Lax; Max-Age={SESSION_TTL}{secure}"
    )
}

pub fn cleared_cookie() -> String {
    format!("{SESSION_COOKIE}=; Path=/; HttpOnly; SameSite=Lax; Max-Age=0")
}

/// Gate for every page that shows or changes data.
///
/// SameSite=Lax on the session cookie is what keeps form POSTs from being
/// forged cross-site: browsers withhold the cookie on cross-origin POSTs, so
/// an unauthenticated request lands here and is redirected.
pub async fn require_auth(State(state): State<AppState>, request: Request, next: Next) -> Response {
    match password_is_set(&state.db).await {
        Ok(true) => {}
        Ok(false) => {
            // Refuse to serve an unprotected admin UI.
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                [(header::CONTENT_TYPE, "text/plain; charset=utf-8")],
                "canari has no password yet.\n\n\
                 Set one with:  canari admin set-password\n",
            )
                .into_response();
        }
        Err(err) => {
            tracing::error!(?err, "cannot read password setting");
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
    }

    let authorized = match token_from_headers(request.headers()) {
        Some(token) => session_is_valid(&state.db, &token).await.unwrap_or(false),
        None => false,
    };

    if authorized {
        next.run(request).await
    } else {
        Redirect::to("/login").into_response()
    }
}

/// Bootstrap the password from the CLI.
pub async fn set_password_interactive(db: &SqlitePool, password: Option<&str>) -> anyhow::Result<()> {
    let password = match password {
        Some(p) => p.to_owned(),
        None => rpassword_prompt()?,
    };
    set_password(db, &password).await
}

/// Read a password from the terminal without echoing it.
///
/// Implemented with `stty` rather than a crate: it is the only place canari
/// needs terminal control, and it keeps the dependency list short.
fn rpassword_prompt() -> anyhow::Result<String> {
    use std::io::{BufRead, Write};

    let mut stdout = std::io::stdout();
    write!(stdout, "New password: ")?;
    stdout.flush()?;

    let echo_off = std::process::Command::new("stty")
        .arg("-echo")
        .stdin(std::process::Stdio::inherit())
        .status()
        .map(|s| s.success())
        .unwrap_or(false);

    let mut password = String::new();
    let read = std::io::stdin().lock().read_line(&mut password);

    if echo_off {
        let _ = std::process::Command::new("stty")
            .arg("echo")
            .stdin(std::process::Stdio::inherit())
            .status();
    }
    println!();
    read.context("reading password")?;

    let password = password.trim_end_matches(['\r', '\n']).to_string();
    if password.is_empty() {
        anyhow::bail!("empty password");
    }
    Ok(password)
}
