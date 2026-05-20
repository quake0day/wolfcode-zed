//! WolfCode Lesson Runner.
//!
//! Provides 4 workspace actions for working with the active lesson:
//!   - Run     — run the lesson's entry file (typically `python <entry>`)
//!   - Test    — run pytest on the lesson's test file
//!   - Explain — ask the AI Tutor (BFF /tutor/chat) — stub for now
//!   - Submit  — submit the lesson (BFF /submissions) — stub for now
//!
//! Z-W3 v0.1 scope:
//!   - Register actions; dispatchable via command palette and (later) keybindings
//!   - Resolve the "current lesson" from the active editor:
//!       * if active file ends with `.py` -> run it directly
//!       * if active file ends with `.test.py` (or `Test` is pressed on a `.py`)
//!         -> run pytest on the sibling test file
//!   - Build the command string and log it via breadcrumb (no terminal spawn yet)
//!
//! Z-W3 v0.2 (next iteration) will actually spawn the command through Zed's
//! terminal crate. Splitting the logic from the integration lets us verify
//! the action wiring via trace alone.

use std::io::Write;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Result, anyhow};
use collections::HashMap;
use futures::AsyncReadExt as _;
use gpui::{App, actions};
use http_client::{AsyncBody, HttpClient, HttpClientWithUrl, Method, Request};
use serde::Serialize;
use task::{
    HideStrategy, RevealStrategy, RevealTarget, SaveStrategy, Shell, SpawnInTerminal, TaskId,
};
use terminal_view::terminal_panel::TerminalPanel;
use workspace::Workspace;

const BFF_URL: &str = "https://wolfcode-bff.quake0day.workers.dev";

const TRACE_PATH: &str = r"C:\Users\Quake\Projects\ai-editor\lesson-panel.trace";

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
        let _ = writeln!(f, "{ts_ms} [lesson_runner::{component}] {}", msg.as_ref());
    }
    log::info!(target: "lesson_runner", "[{component}] {}", msg.as_ref());
}

actions!(lesson_runner, [
    /// Run the current lesson's entry file.
    Run,
    /// Run pytest on the current lesson's test file.
    Test,
    /// Ask the AI Tutor about the current lesson.
    Explain,
    /// Submit the current lesson.
    Submit,
]);

/// Get the absolute path of the active editor's file, if any.
fn active_file_path(workspace: &Workspace, cx: &App) -> Option<PathBuf> {
    let item = workspace.active_item(cx)?;
    let pp = item.project_path(cx)?;
    let project = workspace.project().read(cx);
    let worktree = project.worktree_for_id(pp.worktree_id, cx)?;
    Some(worktree.read(cx).absolutize(&pp.path))
}

/// Map an active file path to a "what to run" pair.
///
/// Returns `Some((command_label, command, args, cwd))` if we can derive
/// a sensible action, or `None` otherwise.
fn build_run_command(
    path: &PathBuf,
) -> Option<(String, String, Vec<String>, PathBuf)> {
    let s = path.to_string_lossy();
    if !s.ends_with(".py") {
        return None;
    }
    let parent = path.parent()?.to_path_buf();
    let filename = path.file_name()?.to_string_lossy().to_string();
    Some((
        format!("python {filename}"),
        "python".to_string(),
        vec![filename],
        parent,
    ))
}

fn build_test_command(
    path: &PathBuf,
) -> Option<(String, String, Vec<String>, PathBuf)> {
    let s = path.to_string_lossy();
    let test_path: PathBuf = if s.ends_with(".test.py") {
        path.clone()
    } else if s.ends_with(".py") {
        let stem = s.strip_suffix(".py")?;
        PathBuf::from(format!("{stem}.test.py"))
    } else {
        return None;
    };
    if !test_path.exists() {
        return None;
    }
    let parent = test_path.parent()?.to_path_buf();
    let name = test_path.file_name()?.to_string_lossy().to_string();
    Some((
        format!("python -m pytest {name}"),
        "python".to_string(),
        vec!["-m".to_string(), "pytest".to_string(), name],
        parent,
    ))
}

/// Walk up from a lesson file to find a `course.json` and return
/// (lesson_id_hint, course_id, relative_path_inside_course).
fn derive_submission_context(path: &PathBuf) -> (String, String, String) {
    let filename = path
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| "unknown".to_string());
    let lesson_id = filename
        .strip_suffix(".test.py")
        .or_else(|| filename.strip_suffix(".lesson.json"))
        .or_else(|| filename.strip_suffix(".py"))
        .unwrap_or(filename.as_str())
        .to_string();

    // Walk up looking for course.json, capped at 10 levels.
    let mut dir = path.parent().map(|p| p.to_path_buf());
    let mut course_root: Option<PathBuf> = None;
    for _ in 0..10 {
        let Some(d) = dir.clone() else {
            break;
        };
        if d.join("course.json").exists() {
            course_root = Some(d);
            break;
        }
        dir = d.parent().map(|p| p.to_path_buf());
    }

    let (course_id, rel_path) = if let Some(root) = course_root {
        let cid = read_course_id(&root.join("course.json")).unwrap_or_else(|| {
            root.file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_else(|| "unknown".to_string())
        });
        let rel = path
            .strip_prefix(&root)
            .map(|p| p.to_string_lossy().to_string())
            .unwrap_or_else(|_| filename.clone());
        (cid, rel)
    } else {
        ("unknown".to_string(), filename.clone())
    };

    (lesson_id, course_id, rel_path)
}

fn read_course_id(course_json: &PathBuf) -> Option<String> {
    let bytes = std::fs::read(course_json).ok()?;
    let v: serde_json::Value = serde_json::from_slice(&bytes).ok()?;
    v.get("id")?.as_str().map(|s| s.to_string())
}

#[derive(Serialize)]
struct SubmitBody<'a> {
    content: &'a str,
    course_id: &'a str,
    file_path: &'a str,
}

async fn post_submission(
    http: Arc<HttpClientWithUrl>,
    jwt: &str,
    lesson_id: &str,
    course_id: &str,
    rel_path: &str,
    content: &str,
) -> Result<String> {
    let body = SubmitBody {
        content,
        course_id,
        file_path: rel_path,
    };
    let payload = serde_json::to_string(&body)?;
    let url = format!(
        "{BFF_URL}/submissions/{}",
        urlencoding_minimal(lesson_id)
    );
    let req = Request::builder()
        .method(Method::POST)
        .uri(&url)
        .header("Authorization", format!("Bearer {jwt}"))
        .header("Content-Type", "application/json")
        .body(AsyncBody::from(payload))?;
    let mut resp = http.send(req).await?;
    let status = resp.status();
    let mut reply = String::new();
    resp.body_mut().read_to_string(&mut reply).await?;
    if !status.is_success() {
        return Err(anyhow!("/submissions returned {status}: {reply}"));
    }
    Ok(reply)
}

/// Tiny URL-encode for path components (avoids pulling in a urlencoding crate).
/// Only encodes characters that would break a URL path segment.
fn urlencoding_minimal(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            'A'..='Z' | 'a'..='z' | '0'..='9' | '-' | '_' | '.' | '~' => out.push(c),
            _ => {
                let mut buf = [0u8; 4];
                for b in c.encode_utf8(&mut buf).bytes() {
                    out.push_str(&format!("%{:02X}", b));
                }
            }
        }
    }
    out
}

fn build_spawn_task(
    id: &str,
    label: &str,
    command_label: &str,
    command: String,
    args: Vec<String>,
    cwd: PathBuf,
) -> SpawnInTerminal {
    SpawnInTerminal {
        id: TaskId(id.to_string()),
        full_label: label.to_string(),
        label: label.to_string(),
        command_label: command_label.to_string(),
        command: Some(command),
        args,
        cwd: Some(cwd),
        env: HashMap::default(),
        use_new_terminal: true,
        allow_concurrent_runs: true,
        hide: HideStrategy::Never,
        reveal: RevealStrategy::Always,
        reveal_target: RevealTarget::Dock,
        shell: Shell::System,
        save: SaveStrategy::default(),
        show_summary: true,
        show_command: true,
        show_rerun: true,
    }
}

pub fn init(cx: &mut App) {
    breadcrumb("init", "called");
    cx.observe_new(|workspace: &mut Workspace, _, _| {
        breadcrumb("init", "observe_new fired");

        workspace.register_action(|workspace, _: &Run, window, cx| {
            breadcrumb("Run", "action invoked");
            let Some(path) = active_file_path(workspace, cx) else {
                breadcrumb("Run", "no active file -> noop");
                return;
            };
            let Some((label, command, args, cwd)) = build_run_command(&path) else {
                breadcrumb("Run", format!("not runnable: {}", path.display()));
                return;
            };
            let Some(terminal_panel) = workspace.panel::<TerminalPanel>(cx) else {
                breadcrumb("Run", "TerminalPanel unavailable");
                return;
            };
            breadcrumb("Run", format!("spawning `{label}` in {}", cwd.display()));
            let spawn = build_spawn_task(
                "wolfcode-run",
                &label,
                &label,
                command,
                args,
                cwd,
            );
            // Defer the spawn to after this action handler returns. The
            // handler holds the workspace in update mode; spawn_task reads
            // it internally and would double-lease.
            window.spawn(cx, async move |cx| {
                breadcrumb("Run", "deferred: calling terminal_panel.spawn_task");
                let result = terminal_panel.update_in(cx, |panel, window, cx| {
                    panel.spawn_task(&spawn, window, cx)
                });
                match result {
                    Ok(task) => {
                        match task.await {
                            Ok(_terminal) => breadcrumb("Run", "spawn_task succeeded"),
                            Err(e) => breadcrumb("Run", format!("spawn_task failed: {e}")),
                        }
                    }
                    Err(e) => breadcrumb("Run", format!("update_in failed: {e}")),
                }
            }).detach();
        });

        workspace.register_action(|workspace, _: &Test, window, cx| {
            breadcrumb("Test", "action invoked");
            let Some(path) = active_file_path(workspace, cx) else {
                breadcrumb("Test", "no active file -> noop");
                return;
            };
            let Some((label, command, args, cwd)) = build_test_command(&path) else {
                breadcrumb(
                    "Test",
                    format!("no test file matches: {}", path.display()),
                );
                return;
            };
            let Some(terminal_panel) = workspace.panel::<TerminalPanel>(cx) else {
                breadcrumb("Test", "TerminalPanel unavailable");
                return;
            };
            breadcrumb("Test", format!("spawning `{label}` in {}", cwd.display()));
            let spawn_test = build_spawn_task(
                "wolfcode-test",
                &label,
                &label,
                command,
                args,
                cwd,
            );
            window.spawn(cx, async move |cx| {
                breadcrumb("Test", "deferred: calling terminal_panel.spawn_task");
                let result = terminal_panel.update_in(cx, |panel, window, cx| {
                    panel.spawn_task(&spawn_test, window, cx)
                });
                match result {
                    Ok(task) => {
                        match task.await {
                            Ok(_terminal) => breadcrumb("Test", "spawn_task succeeded"),
                            Err(e) => breadcrumb("Test", format!("spawn_task failed: {e}")),
                        }
                    }
                    Err(e) => breadcrumb("Test", format!("update_in failed: {e}")),
                }
            }).detach();
        });

        workspace.register_action(|workspace, _: &Explain, _window, cx| {
            breadcrumb("Explain", "action invoked");
            let Some(path) = active_file_path(workspace, cx) else {
                breadcrumb("Explain", "no active file -> noop");
                return;
            };
            breadcrumb(
                "Explain",
                format!("would POST BFF /tutor/chat for {}", path.display()),
            );
            // TODO Z-W5: implement BFF tutor stream
        });

        workspace.register_action(|workspace, _: &Submit, window, cx| {
            breadcrumb("Submit", "action invoked");
            let http = workspace.app_state().client.http_client();
            let cred = zed_credentials_provider::global(cx);
            let Some(path) = active_file_path(workspace, cx) else {
                breadcrumb("Submit", "no active file -> noop");
                return;
            };
            let (lesson_id, course_id, rel_path) = derive_submission_context(&path);
            breadcrumb(
                "Submit",
                format!(
                    "lesson_id={lesson_id} course_id={course_id} rel_path={rel_path}"
                ),
            );

            window
                .spawn(cx, async move |cx| {
                    let read = match cred.read_credentials(BFF_URL, cx).await {
                        Ok(opt) => opt,
                        Err(e) => {
                            breadcrumb("Submit", format!("keychain read FAILED: {e}"));
                            return;
                        }
                    };
                    let Some((_, jwt_bytes)) = read else {
                        breadcrumb(
                            "Submit",
                            "no JWT in keychain (run SignInFromFile first)",
                        );
                        return;
                    };
                    let jwt = match String::from_utf8(jwt_bytes) {
                        Ok(s) => s,
                        Err(e) => {
                            breadcrumb("Submit", format!("JWT not UTF-8: {e}"));
                            return;
                        }
                    };
                    let content = match std::fs::read_to_string(&path) {
                        Ok(s) => s,
                        Err(e) => {
                            breadcrumb("Submit", format!("read file FAILED: {e}"));
                            return;
                        }
                    };
                    breadcrumb(
                        "Submit",
                        format!("read content: {} bytes", content.len()),
                    );

                    match post_submission(
                        http,
                        &jwt,
                        &lesson_id,
                        &course_id,
                        &rel_path,
                        &content,
                    )
                    .await
                    {
                        Ok(reply) => breadcrumb("Submit", format!("OK: {reply}")),
                        Err(e) => breadcrumb("Submit", format!("FAILED: {e}")),
                    }
                })
                .detach();
        });

        breadcrumb("init", "4 actions registered (Run / Test / Explain / Submit)");
    })
    .detach();
}
