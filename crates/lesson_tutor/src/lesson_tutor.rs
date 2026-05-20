//! WolfCode Lesson Tutor.
//!
//! Z-W5 v0.1:
//!   - `AskTutor` action: derives the current lesson context, reads JWT
//!     from keychain, POSTs to BFF `/tutor/chat`, reads the SSE response,
//!     and breadcrumbs each text chunk plus the full reply.
//!
//! v0.1 hardcodes a generic question (hint level L1). v0.2 will add a
//! prompt for the user's question, a dedicated Tutor side panel, and
//! per-token streaming into the panel.

use std::io::Write;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, anyhow};
use futures::AsyncReadExt as _;
use gpui::{App, actions};
use http_client::{AsyncBody, HttpClient, HttpClientWithUrl, Method, Request};
use serde::Serialize;
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
        let _ = writeln!(f, "{ts_ms} [lesson_tutor::{component}] {}", msg.as_ref());
    }
    log::info!(target: "lesson_tutor", "[{component}] {}", msg.as_ref());
}

actions!(lesson_tutor, [
    /// Ask the AI Tutor about the current lesson (hint level L1).
    AskTutor,
]);

#[derive(Debug, Serialize)]
struct TutorRequest<'a> {
    lesson_id: &'a str,
    level: &'a str,
    user_question: &'a str,
    current_code: &'a str,
    allow_solution: bool,
}

/// Find the active editor's absolute file path.
fn active_file_path(workspace: &Workspace, cx: &App) -> Option<PathBuf> {
    let item = workspace.active_item(cx)?;
    let pp = item.project_path(cx)?;
    let project = workspace.project().read(cx);
    let worktree = project.worktree_for_id(pp.worktree_id, cx)?;
    Some(worktree.read(cx).absolutize(&pp.path))
}

/// Best-effort derivation of a "lesson_id" hint to send to the BFF.
///
/// If the active file is a `.lesson.json`, use its filename stem.
/// Otherwise (.py / .test.py), strip extensions and reuse the stem.
/// kara-wiki accepts `section_id` as a soft hint; nothing fatal if
/// it doesn't match a known section.
fn derive_lesson_id(path: &PathBuf) -> String {
    let s = path.to_string_lossy();
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| "unknown".to_string());
    if let Some(stem) = s.strip_suffix(".lesson.json") {
        return PathBuf::from(stem)
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or(name);
    }
    if let Some(stem) = s.strip_suffix(".test.py") {
        return PathBuf::from(stem)
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or(name);
    }
    if let Some(stem) = s.strip_suffix(".py") {
        return PathBuf::from(stem)
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or(name);
    }
    name
}

async fn read_file_to_string(path: &PathBuf) -> Result<String> {
    // Lesson files are small (<10KB); a blocking read inside this async
    // task is acceptable for v0.1. Future iterations can route through
    // project::Fs for remote-host correctness.
    let bytes = std::fs::read(path).with_context(|| {
        format!("failed to read lesson file: {}", path.display())
    })?;
    Ok(String::from_utf8_lossy(&bytes).to_string())
}

async fn ask_tutor(
    http: Arc<HttpClientWithUrl>,
    jwt: &str,
    lesson_id: &str,
    current_code: &str,
) -> Result<String> {
    let body = TutorRequest {
        lesson_id,
        level: "L1",
        user_question: "I'm not sure what to do next. Pinpoint where I'm stuck.",
        current_code,
        allow_solution: false,
    };
    let payload = serde_json::to_string(&body)?;
    breadcrumb(
        "AskTutor",
        format!("POSTing /tutor/chat ({} bytes payload)", payload.len()),
    );
    let req = Request::builder()
        .method(Method::POST)
        .uri(format!("{BFF_URL}/tutor/chat"))
        .header("Authorization", format!("Bearer {jwt}"))
        .header("Content-Type", "application/json")
        .header("Accept", "text/event-stream")
        .body(AsyncBody::from(payload))?;
    let mut resp = http.send(req).await?;
    if !resp.status().is_success() {
        let status = resp.status();
        let mut err_body = String::new();
        let _ = resp.body_mut().read_to_string(&mut err_body).await;
        return Err(anyhow!("/tutor/chat returned {status}: {err_body}"));
    }
    breadcrumb("AskTutor", format!("HTTP {} -- reading SSE body", resp.status()));

    // Read the full body (kara-wiki emits short responses; for true
    // chunk-by-chunk streaming we'd loop over body.read into a buffer).
    let mut body_text = String::new();
    resp.body_mut().read_to_string(&mut body_text).await?;
    breadcrumb(
        "AskTutor",
        format!("SSE body length = {} bytes", body_text.len()),
    );

    // kara-wiki SSE shape: `data: <plain text>` lines, ending with
    // `event: done\ndata: [DONE]`. Concatenate non-DONE data lines.
    let mut chunks = 0usize;
    let mut combined = String::new();
    for line in body_text.lines() {
        let Some(payload) = line.strip_prefix("data:") else {
            continue;
        };
        let payload = payload.trim();
        if payload.is_empty() || payload == "[DONE]" {
            continue;
        }
        chunks += 1;
        // Try JSON first (OpenAI delta shape), fall back to plain text.
        let chunk_text = match serde_json::from_str::<serde_json::Value>(payload) {
            Ok(v) => v
                .get("delta")
                .or_else(|| v.get("content"))
                .or_else(|| v.get("text"))
                .and_then(|x| x.as_str())
                .map(|s| s.to_string())
                .unwrap_or_else(|| payload.to_string()),
            Err(_) => payload.to_string(),
        };
        combined.push_str(&chunk_text);
    }
    breadcrumb(
        "AskTutor",
        format!("parsed {chunks} SSE chunks, combined {} chars", combined.len()),
    );
    Ok(combined)
}

pub fn init(cx: &mut App) {
    breadcrumb("init", "called");
    cx.observe_new(|workspace: &mut Workspace, _, _| {
        breadcrumb("init", "observe_new fired");

        workspace.register_action(|workspace, _: &AskTutor, window, cx| {
            breadcrumb("AskTutor", "action invoked");
            let http = workspace.app_state().client.http_client();
            let cred = zed_credentials_provider::global(cx);
            let Some(path) = active_file_path(workspace, cx) else {
                breadcrumb("AskTutor", "no active file -> abort");
                return;
            };
            let lesson_id = derive_lesson_id(&path);
            breadcrumb(
                "AskTutor",
                format!(
                    "active file: {} -> lesson_id={}",
                    path.display(),
                    lesson_id
                ),
            );

            window
                .spawn(cx, async move |cx| {
                    let read = match cred.read_credentials(BFF_URL, cx).await {
                        Ok(opt) => opt,
                        Err(e) => {
                            breadcrumb("AskTutor", format!("keychain read FAILED: {e}"));
                            return;
                        }
                    };
                    let Some((_, jwt_bytes)) = read else {
                        breadcrumb("AskTutor", "no JWT in keychain (run SignInFromFile first)");
                        return;
                    };
                    let jwt = match String::from_utf8(jwt_bytes) {
                        Ok(s) => s,
                        Err(e) => {
                            breadcrumb("AskTutor", format!("JWT not valid UTF-8: {e}"));
                            return;
                        }
                    };
                    let code = match read_file_to_string(&path).await {
                        Ok(s) => s,
                        Err(e) => {
                            breadcrumb("AskTutor", format!("read file FAILED: {e}"));
                            String::new()
                        }
                    };
                    breadcrumb(
                        "AskTutor",
                        format!("current_code: {} bytes", code.len()),
                    );

                    match ask_tutor(http, &jwt, &lesson_id, &code).await {
                        Ok(reply) => {
                            // Cap log output so trace stays readable.
                            let preview: String = reply.chars().take(200).collect();
                            breadcrumb(
                                "AskTutor",
                                format!("REPLY ({} chars): {preview}", reply.len()),
                            );
                        }
                        Err(e) => breadcrumb("AskTutor", format!("FAILED: {e}")),
                    }
                })
                .detach();
        });

        breadcrumb("init", "1 action registered (AskTutor)");
    })
    .detach();
}
