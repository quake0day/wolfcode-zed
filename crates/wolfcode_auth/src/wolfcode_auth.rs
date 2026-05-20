//! WolfCode Auth.
//!
//! Z-W4 v0.1: 3 actions for sign-in / sign-out / whoami against the
//! WolfCode BFF Gateway.
//!
//! - `SignInFromFile`: reads `wolfcode-jwt.txt`, verifies via BFF `/me`,
//!   then stores the JWT in the OS keychain via Zed's credentials_provider.
//! - `SignOut`: deletes the stored JWT from the keychain.
//! - `WhoAmI`: reads JWT from keychain, calls BFF `/me`, breadcrumbs the
//!   user info.
//!
//! Future v0.2 will add a magic-link request flow (POST /auth/magic-link)
//! and a custom URL scheme handler so the IDE can receive the JWT
//! callback directly from the verify page.

use std::io::Write;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, anyhow};
use futures::AsyncReadExt as _;
use gpui::{App, actions};
use http_client::{AsyncBody, HttpClient, HttpClientWithUrl, Method, Request};
use serde::Deserialize;
use workspace::Workspace;

const TRACE_PATH: &str = r"C:\Users\Quake\Projects\ai-editor\lesson-panel.trace";
const BFF_URL: &str = "https://wolfcode-bff.quake0day.workers.dev";
const JWT_FILE: &str = r"C:\Users\Quake\Projects\ai-editor\wolfcode-jwt.txt";

fn breadcrumb(component: &str, msg: impl AsRef<str>) {
    let ts_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(TRACE_PATH)
    {
        let _ = writeln!(f, "{ts_ms} [wolfcode_auth::{component}] {}", msg.as_ref());
    }
    log::info!(target: "wolfcode_auth", "[{component}] {}", msg.as_ref());
}

actions!(wolfcode_auth, [
    /// Read JWT from `wolfcode-jwt.txt`, verify via BFF `/me`, and store in keychain.
    SignInFromFile,
    /// Clear the stored JWT.
    SignOut,
    /// Call BFF `/me` with the stored JWT and breadcrumb the user info.
    WhoAmI,
]);

#[derive(Debug, Deserialize)]
struct MeResponse {
    #[serde(default)]
    user: Option<UserInfo>,
}

#[derive(Debug, Deserialize)]
struct UserInfo {
    id: String,
    email: String,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    role: Option<String>,
}

async fn get_me(http: Arc<HttpClientWithUrl>, jwt: &str) -> Result<UserInfo> {
    let req = Request::builder()
        .method(Method::GET)
        .uri(format!("{BFF_URL}/me"))
        .header("Authorization", format!("Bearer {jwt}"))
        .body(AsyncBody::empty())?;
    let mut resp = http.send(req).await?;
    if !resp.status().is_success() {
        let status = resp.status();
        let mut body = String::new();
        let _ = resp.body_mut().read_to_string(&mut body).await;
        return Err(anyhow!("BFF /me returned {status}: {body}"));
    }
    let mut body = String::new();
    resp.body_mut().read_to_string(&mut body).await?;
    let parsed: MeResponse = serde_json::from_str(&body)
        .with_context(|| format!("invalid /me response body: {body}"))?;
    parsed
        .user
        .ok_or_else(|| anyhow!("/me response missing `user` field: {body}"))
}

pub fn init(cx: &mut App) {
    breadcrumb("init", "called");
    cx.observe_new(|workspace: &mut Workspace, _, _| {
        breadcrumb("init", "observe_new fired");

        workspace.register_action(|workspace, _: &SignInFromFile, window, cx| {
            breadcrumb("SignIn", "action invoked");
            let http = workspace.app_state().client.http_client();
            let cred = zed_credentials_provider::global(cx);
            window
                .spawn(cx, async move |cx| {
                    let jwt = match std::fs::read_to_string(JWT_FILE) {
                        Ok(s) => s.trim().to_string(),
                        Err(e) => {
                            breadcrumb("SignIn", format!("read {JWT_FILE} FAILED: {e}"));
                            return;
                        }
                    };
                    if jwt.is_empty() {
                        breadcrumb("SignIn", "JWT file empty");
                        return;
                    }
                    breadcrumb("SignIn", format!("read JWT ({} chars)", jwt.len()));

                    match get_me(http, &jwt).await {
                        Ok(user) => {
                            breadcrumb(
                                "SignIn",
                                format!("/me OK: id={} email={}", user.id, user.email),
                            );
                            match cred
                                .write_credentials(BFF_URL, "token", jwt.as_bytes(), cx)
                                .await
                            {
                                Ok(()) => breadcrumb("SignIn", "JWT stored in keychain"),
                                Err(e) => {
                                    breadcrumb("SignIn", format!("keychain write FAILED: {e}"))
                                }
                            }
                        }
                        Err(e) => breadcrumb("SignIn", format!("/me FAILED: {e}")),
                    }
                })
                .detach();
        });

        workspace.register_action(|workspace, _: &SignOut, window, cx| {
            breadcrumb("SignOut", "action invoked");
            let cred = zed_credentials_provider::global(cx);
            window
                .spawn(cx, async move |cx| {
                    match cred.delete_credentials(BFF_URL, cx).await {
                        Ok(()) => breadcrumb("SignOut", "JWT deleted from keychain"),
                        Err(e) => breadcrumb("SignOut", format!("delete FAILED: {e}")),
                    }
                })
                .detach();
        });

        workspace.register_action(|workspace, _: &WhoAmI, window, cx| {
            breadcrumb("WhoAmI", "action invoked");
            let http = workspace.app_state().client.http_client();
            let cred = zed_credentials_provider::global(cx);
            window
                .spawn(cx, async move |cx| {
                    let read = match cred.read_credentials(BFF_URL, cx).await {
                        Ok(opt) => opt,
                        Err(e) => {
                            breadcrumb("WhoAmI", format!("keychain read FAILED: {e}"));
                            return;
                        }
                    };
                    let Some((_, jwt_bytes)) = read else {
                        breadcrumb("WhoAmI", "no JWT in keychain (sign in first)");
                        return;
                    };
                    let jwt = match String::from_utf8(jwt_bytes) {
                        Ok(s) => s,
                        Err(e) => {
                            breadcrumb("WhoAmI", format!("JWT not valid UTF-8: {e}"));
                            return;
                        }
                    };
                    match get_me(http, &jwt).await {
                        Ok(user) => breadcrumb(
                            "WhoAmI",
                            format!(
                                "OK: id={} email={} name={:?} role={:?}",
                                user.id, user.email, user.name, user.role
                            ),
                        ),
                        Err(e) => breadcrumb("WhoAmI", format!("/me FAILED: {e}")),
                    }
                })
                .detach();
        });

        breadcrumb(
            "init",
            "3 actions registered (SignInFromFile / SignOut / WhoAmI)",
        );
    })
    .detach();
}
