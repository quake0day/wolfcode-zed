//! WolfCode Lesson Telemetry.
//!
//! Z-W6 v0.1: POST a synthetic batch of events to BFF `/events/batch`
//! when the `EmitTestEvent` action is dispatched. This validates the
//! whole pipeline (JWT auth -> BFF -> cerebro fan-out) before we wire
//! to real editor events in v0.2.
//!
//! Event shape matches BFF `events.ts`:
//!   { session_id, events: [{ ts, type, lesson_id?, course_id?, kc?, details? }] }

use std::io::Write;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Result, anyhow};
use futures::AsyncReadExt as _;
use gpui::{App, actions};
use http_client::{AsyncBody, HttpClient, HttpClientWithUrl, Method, Request};
use serde::Serialize;
use serde_json::{Value, json};
use workspace::Workspace;

const TRACE_PATH: &str = r"C:\Users\Quake\Projects\ai-editor\lesson-panel.trace";
const BFF_URL: &str = "https://wolfcode-bff.quake0day.workers.dev";

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
        let _ = writeln!(f, "{ts_ms} [lesson_telemetry::{component}] {}", msg.as_ref());
    }
    log::info!(target: "lesson_telemetry", "[{component}] {}", msg.as_ref());
}

actions!(lesson_telemetry, [
    /// Emit a synthetic batch of events to the BFF for end-to-end validation.
    EmitTestEvent,
]);

#[derive(Debug, Serialize)]
struct EventBatch<'a> {
    session_id: &'a str,
    events: &'a [Value],
}

fn now_ms() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0)
}

fn random_session_id() -> String {
    // Borrow the same alphabet/length as wolfcode-bff's randomToken().
    const ABC: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789";
    let mut buf = [0u8; 22];
    // gpui apps don't ship `rand` by default; use SystemTime to seed a
    // simple LCG. Good enough for a session id.
    let seed = now_ms() as u64;
    let mut state = seed.wrapping_mul(0x9E3779B97F4A7C15);
    for slot in buf.iter_mut() {
        state = state.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        *slot = ABC[(state >> 33) as usize % ABC.len()];
    }
    String::from_utf8_lossy(&buf).to_string()
}

async fn post_events_batch(
    http: Arc<HttpClientWithUrl>,
    jwt: &str,
    session_id: &str,
    events: &[Value],
) -> Result<String> {
    let payload = serde_json::to_string(&EventBatch { session_id, events })?;
    breadcrumb(
        "POST",
        format!(
            "/events/batch session={session_id} events={} payload={} bytes",
            events.len(),
            payload.len()
        ),
    );
    let req = Request::builder()
        .method(Method::POST)
        .uri(format!("{BFF_URL}/events/batch"))
        .header("Authorization", format!("Bearer {jwt}"))
        .header("Content-Type", "application/json")
        .body(AsyncBody::from(payload))?;
    let mut resp = http.send(req).await?;
    let status = resp.status();
    let mut body = String::new();
    resp.body_mut().read_to_string(&mut body).await?;
    if !status.is_success() {
        return Err(anyhow!("/events/batch returned {status}: {body}"));
    }
    Ok(body)
}

pub fn init(cx: &mut App) {
    breadcrumb("init", "called");
    cx.observe_new(|workspace: &mut Workspace, _, _| {
        breadcrumb("init", "observe_new fired");

        workspace.register_action(|workspace, _: &EmitTestEvent, window, cx| {
            breadcrumb("EmitTestEvent", "action invoked");
            let http = workspace.app_state().client.http_client();
            let cred = zed_credentials_provider::global(cx);
            let session_id = random_session_id();
            breadcrumb("EmitTestEvent", format!("session_id={session_id}"));

            // Build a small synthetic batch covering edit / run / test / hint.
            let now = now_ms() as u64;
            let lesson_id = "csc141-01-first-variable";
            let course_id = "csc141-fall2026";
            let events: Vec<Value> = vec![
                json!({
                    "ts": now,
                    "type": "edit",
                    "lesson_id": lesson_id,
                    "course_id": course_id,
                    "kc": ["python-variables"],
                    "details": { "added_chars": 12, "deleted_chars": 0 }
                }),
                json!({
                    "ts": now + 100,
                    "type": "run",
                    "lesson_id": lesson_id,
                    "course_id": course_id,
                    "kc": ["python-variables"],
                    "details": { "command": "python 01-first-variable.py" }
                }),
                json!({
                    "ts": now + 200,
                    "type": "test",
                    "lesson_id": lesson_id,
                    "course_id": course_id,
                    "kc": ["python-variables"],
                    "details": { "command": "pytest", "passed": 2, "failed": 0, "total": 2 }
                }),
                json!({
                    "ts": now + 300,
                    "type": "hint",
                    "lesson_id": lesson_id,
                    "course_id": course_id,
                    "kc": ["python-variables"],
                    "details": { "level": "L1", "length": 132 }
                }),
            ];

            window
                .spawn(cx, async move |cx| {
                    let read = match cred.read_credentials(BFF_URL, cx).await {
                        Ok(opt) => opt,
                        Err(e) => {
                            breadcrumb("EmitTestEvent", format!("keychain read FAILED: {e}"));
                            return;
                        }
                    };
                    let Some((_, jwt_bytes)) = read else {
                        breadcrumb(
                            "EmitTestEvent",
                            "no JWT in keychain (run SignInFromFile first)",
                        );
                        return;
                    };
                    let jwt = match String::from_utf8(jwt_bytes) {
                        Ok(s) => s,
                        Err(e) => {
                            breadcrumb("EmitTestEvent", format!("JWT not UTF-8: {e}"));
                            return;
                        }
                    };

                    match post_events_batch(http, &jwt, &session_id, &events).await {
                        Ok(body) => breadcrumb(
                            "EmitTestEvent",
                            format!("OK: response body={body}"),
                        ),
                        Err(e) => breadcrumb("EmitTestEvent", format!("FAILED: {e}")),
                    }
                })
                .detach();
        });

        breadcrumb("init", "1 action registered (EmitTestEvent)");
    })
    .detach();
}
