//! Authentication for OneDrive using OAuth2 PKCE + macOS Keychain.
//!
//! Strategy (matching the recommendation from the other AI):
//!   1. On every run, try a SILENT REFRESH from the macOS Keychain first.
//!   2. Only open the browser if the refresh fails or no token exists.
//!   3. Save the (possibly rotated) refresh token back to Keychain every time.
//!
//! This means the user sees a browser login ONCE, then never again unless
//! they revoke the app, change their password, or go 90+ days without syncing.

use anyhow::{anyhow, Result};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use rand::RngCore;
use reqwest::Client;
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use tracing::{info, warn};
use url::Url;

use crate::config::OneDriveConfig;

/// Microsoft identity platform endpoint for PERSONAL Microsoft accounts only.
/// (Outlook, Hotmail, Live, Microsoft 365 Family/Personal)
const BASE: &str = "https://login.microsoftonline.com/consumers/oauth2/v2.0";

/// Scopes needed: read/write files + long-lived refresh token
const SCOPE: &str = "Files.ReadWrite offline_access";

/// macOS Keychain service and account names for storing the refresh token
const KEYRING_SERVICE: &str = "cloudsync";
const KEYRING_USER: &str = "onedrive_refresh_token";

// ── Token response from Microsoft ────────────────────────────────────────────

#[derive(Deserialize)]
struct TokenResponse {
    access_token: String,
    /// May be absent if the refresh token did not rotate
    refresh_token: Option<String>,
}

// ── Keychain helpers ──────────────────────────────────────────────────────────

fn keyring_entry() -> Result<keyring::Entry> {
    keyring::Entry::new(KEYRING_SERVICE, KEYRING_USER)
        .map_err(|e| anyhow!("Keychain entry error: {}", e))
}

fn keyring_get() -> Option<String> {
    keyring_entry().ok()?.get_password().ok()
}

fn keyring_set(token: &str) {
    match keyring_entry() {
        Ok(e) => {
            if let Err(err) = e.set_password(token) {
                warn!("Could not save refresh token to Keychain: {}", err);
            } else {
                info!("Refresh token saved to macOS Keychain.");
            }
        }
        Err(e) => warn!("Could not open Keychain entry: {}", e),
    }
}

// ── Public API ────────────────────────────────────────────────────────────────

/// Get a valid access token.
///
/// Flow:
///   1. If a refresh token exists in the Keychain → try a silent refresh.
///   2. If refresh succeeds → return the access token (no browser needed).
///   3. If refresh fails (expired / revoked) → open browser for interactive login.
pub async fn get_access_token(cfg: &OneDriveConfig, http: &Client) -> Result<String> {
    if let Some(refresh_token) = keyring_get() {
        info!("Found stored refresh token, attempting silent refresh…");
        match try_refresh(cfg, http, &refresh_token).await {
            Ok(access) => {
                info!("Silent refresh succeeded — no browser login needed.");
                return Ok(access);
            }
            Err(e) => {
                warn!("Silent refresh failed ({}), starting browser login…", e);
            }
        }
    } else {
        info!("No stored token found, starting browser login…");
    }

    interactive_login(cfg, http).await
}

/// Remove the stored refresh token from Keychain (logout / revoke).
#[allow(dead_code)]
pub fn logout() -> Result<()> {
    if let Ok(entry) = keyring_entry() {
        entry.delete_password()
            .map_err(|e| anyhow!("Failed to delete Keychain entry: {}", e))?;
        info!("Logged out — refresh token removed from Keychain.");
    }
    Ok(())
}

// ── Internal helpers ──────────────────────────────────────────────────────────

/// Silent refresh: exchange the stored refresh token for a new access token.
/// Handles refresh token rotation — Microsoft may issue a NEW refresh token.
async fn try_refresh(cfg: &OneDriveConfig, http: &Client, refresh_token: &str) -> Result<String> {
    let resp = http
        .post(format!("{}/token", BASE))
        .form(&[
            ("client_id",      cfg.client_id.as_str()),
            ("grant_type",     "refresh_token"),
            ("refresh_token",  refresh_token),
            ("scope",          SCOPE),
        ])
        .send()
        .await?;

    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        return Err(anyhow!("Refresh HTTP {status}: {body}"));
    }

    let token: TokenResponse = resp.json().await?;

    // Refresh token ROTATION — Microsoft may hand back a new RT.
    // Always persist whatever we receive; the old one may stop working.
    if let Some(new_rt) = &token.refresh_token {
        keyring_set(new_rt);
    }

    Ok(token.access_token)
}

/// Full interactive PKCE login flow (browser-based).
/// Only called on first run or after token expiry/revocation.
async fn interactive_login(cfg: &OneDriveConfig, http: &Client) -> Result<String> {
    // ── 1. Build PKCE verifier + challenge ───────────────────────────────────
    let mut verifier_bytes = vec![0u8; 32];
    rand::thread_rng().fill_bytes(&mut verifier_bytes);
    let verifier  = URL_SAFE_NO_PAD.encode(&verifier_bytes);
    let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()));

    // ── 2. Generate CSRF state ───────────────────────────────────────────────
    let mut state_bytes = vec![0u8; 16];
    rand::thread_rng().fill_bytes(&mut state_bytes);
    let state = URL_SAFE_NO_PAD.encode(&state_bytes);

    // ── 3. Build the authorization URL ──────────────────────────────────────
    let auth_url = Url::parse_with_params(
        &format!("{}/authorize", BASE),
        &[
            ("client_id",             cfg.client_id.as_str()),
            ("response_type",         "code"),
            ("redirect_uri",          cfg.redirect_uri.as_str()),
            ("scope",                 SCOPE),
            ("code_challenge",        challenge.as_str()),
            ("code_challenge_method", "S256"),
            ("state",                 state.as_str()),
        ],
    )?;

    // ── 4. Open system browser ───────────────────────────────────────────────
    info!("Opening browser for OneDrive login…");
    open::that(auth_url.as_str())
        .map_err(|e| anyhow!("Failed to open browser: {}", e))?;

    // ── 5. Start tiny_http server to receive the redirect ───────────────────
    let redirect_url = Url::parse(&cfg.redirect_uri)?;
    let port = redirect_url.port().unwrap_or(8765);

    info!("Waiting for Microsoft to redirect back on port {}…", port);
    let server = tiny_http::Server::http(format!("127.0.0.1:{}", port))
        .map_err(|e| anyhow!("Failed to start callback server on port {port}: {e}"))?;

    // Blocking recv is fine here — we're waiting for the user to log in
    let request = tokio::task::spawn_blocking(move || server.recv())
        .await?
        .map_err(|e| anyhow!("Callback receive error: {}", e))?;

    // ── 6. Parse the callback URL ────────────────────────────────────────────
    let callback = Url::parse(&format!("http://localhost{}", request.url()))?;
    let params: HashMap<String, String> = callback.query_pairs().into_owned().collect();

    // Send a friendly success page back to the browser tab
    let html = "<html><body style='font-family:system-ui;max-width:480px;\
                margin:80px auto;text-align:center'>\
                <h2>✅ CloudSync — Signed in!</h2>\
                <p>You can close this tab and return to the app.</p>\
                </body></html>";
    let response = tiny_http::Response::from_string(html)
        .with_header("Content-Type: text/html".parse::<tiny_http::Header>()
            .expect("static header"));
    request.respond(response).ok();

    // ── 7. CSRF check ────────────────────────────────────────────────────────
    if params.get("state").map(|s| s.as_str()) != Some(state.as_str()) {
        return Err(anyhow!("CSRF state mismatch — aborting login"));
    }

    // ── 8. Check for Microsoft error responses ───────────────────────────────
    if let Some(error) = params.get("error") {
        let desc = params
            .get("error_description")
            .map(|s| s.as_str())
            .unwrap_or("no description");
        return Err(anyhow!("Microsoft auth error: {} — {}", error, desc));
    }

    let code = params
        .get("code")
        .ok_or_else(|| anyhow!("No authorization code in callback URL"))?;

    // ── 9. Exchange authorization code for tokens ────────────────────────────
    info!("Exchanging authorization code for tokens…");
    let resp = http
        .post(format!("{}/token", BASE))
        .form(&[
            ("client_id",      cfg.client_id.as_str()),
            ("grant_type",     "authorization_code"),
            ("code",           code.as_str()),
            ("redirect_uri",   cfg.redirect_uri.as_str()),
            ("code_verifier",  verifier.as_str()),
            ("scope",          SCOPE),
        ])
        .send()
        .await?;

    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        return Err(anyhow!("Token exchange failed ({status}): {body}"));
    }

    let token: TokenResponse = resp.json().await?;

    // ── 10. Persist refresh token to Keychain ────────────────────────────────
    if let Some(rt) = &token.refresh_token {
        keyring_set(rt);
    } else {
        warn!("Microsoft did not return a refresh token — offline_access scope may be missing.");
    }

    info!("Login successful.");
    Ok(token.access_token)
}
