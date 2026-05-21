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
use workspace::pane::SaveIntent;
use workspace::notifications::NotificationId;
use workspace::Toast;

// Toast markers (one per action so toasts replace each other rather than stack).
struct SubmitToast;
struct RunToast;
struct TestToast;

fn toast(msg: std::borrow::Cow<'static, str>, marker: NotificationId) -> Toast {
    Toast::new(marker, msg).autohide()
}

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
    /// Z-W16: ping for the Lesson Panel to re-fetch /reports/me/lesson-status.
    /// Declared here so lesson_runner can dispatch without depending on
    /// lesson_panel (which would create a cycle).
    RefreshLessonStatus,
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
    // Pack the entire command into the `command` field with EMPTY args.
    // Zed's prepare_task_for_spawn computes command_label from `task.command`
    // alone (ignoring `task.args`), so passing args separately produces a
    // misleading label like `pwsh -C 'python'`. Inlining the args keeps the
    // displayed label correct.
    let full = format!("python {filename}");
    Some((
        full.clone(),
        full,
        Vec::new(),
        parent,
    ))
}

// Test uses std::process::Command directly (NOT the terminal), so it needs
// separate command+args (otherwise Command::new("python -m pytest ...") would
// look for a literal binary by that name).
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

/// Extract `"submission_id":"<value>"` from the BFF JSON reply.
/// Cheap manual extraction so we don't have to introduce serde_json::from_str
/// on a string we already log verbatim.
fn parse_submission_id(json: &str) -> Option<String> {
    let key = "\"submission_id\":\"";
    let start = json.find(key)? + key.len();
    let rest = &json[start..];
    let end = rest.find('"')?;
    Some(rest[..end].to_string())
}

/// Parse a pytest stdout summary line. Two formats are common:
///   • TTY mode:     `===== 2 passed in 0.05s =====`
///   • non-TTY mode: `2 passed in 0.05s`  (no equals wrapping — what we get
///                    when run via std::process::Command).
/// Returns (passed, failed). `failed` includes both "failed" and "errors".
fn parse_pytest_summary(stdout: &str) -> (u32, u32) {
    // The summary line is the LAST non-empty line. pytest always prints
    // exactly one final summary line regardless of TTY mode.
    let summary = stdout
        .lines()
        .rev()
        .find(|l| {
            let t = l.trim();
            !t.is_empty()
                && (t.contains("passed") || t.contains("failed") || t.contains("error"))
        })
        .unwrap_or("");
    let mut passed = 0u32;
    let mut failed = 0u32;
    // Scan for "<N> passed" / "<N> failed" / "<N> error(s)"
    let tokens: Vec<&str> = summary.split_whitespace().collect();
    for win in tokens.windows(2) {
        let n: u32 = match win[0].parse() {
            Ok(n) => n,
            Err(_) => continue,
        };
        match win[1] {
            "passed" | "passed," => passed += n,
            "failed" | "failed," => failed += n,
            "error" | "errors" | "error," | "errors," => failed += n,
            _ => {}
        }
    }
    (passed, failed)
}

#[derive(Serialize)]
struct EventBatchBody<'a> {
    session_id: &'a str,
    events: &'a [serde_json::Value],
}

/// Z-W19c: drain editor's paste log and POST each as a `paste` event.
/// Piggy-backs on the existing /events/batch infrastructure.
async fn flush_paste_log(
    http: Arc<HttpClientWithUrl>,
    jwt: &str,
    lesson_id: &str,
    course_id: &str,
) -> Result<usize> {
    let records = editor::drain_paste_log();
    if records.is_empty() {
        return Ok(0);
    }
    let events: Vec<serde_json::Value> = records
        .iter()
        .map(|r| {
            serde_json::json!({
                "ts": r.ts_ms as u64,
                "type": "paste",
                "lesson_id": lesson_id,
                "course_id": course_id,
                "kc": [],
                "details": {
                    "bytes": r.bytes,
                    "lines": r.lines,
                    "blocked": r.was_blocked,
                }
            })
        })
        .collect();
    let session_id = format!("paste-{}", records[0].ts_ms);
    let payload = serde_json::to_string(&EventBatchBody {
        session_id: &session_id,
        events: &events,
    })?;
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
        return Err(anyhow!("/events/batch (paste flush) returned {status}: {body}"));
    }
    breadcrumb(
        "flush_paste_log",
        format!("flushed {} paste records", records.len()),
    );
    Ok(records.len())
}

async fn post_run_event(
    http: Arc<HttpClientWithUrl>,
    jwt: &str,
    lesson_id: &str,
    course_id: &str,
    command: &str,
    code: Option<&str>,
) -> Result<String> {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);
    let session_id = format!("run-{now}");
    let events = vec![serde_json::json!({
        "ts": now,
        "type": "run",
        "lesson_id": lesson_id,
        "course_id": course_id,
        "kc": [],
        "details": {
            "command": command,
            "code": code,
        }
    })];
    let payload = serde_json::to_string(&EventBatchBody {
        session_id: &session_id,
        events: &events,
    })?;
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

async fn post_test_event(
    http: Arc<HttpClientWithUrl>,
    jwt: &str,
    lesson_id: &str,
    course_id: &str,
    passed: u32,
    failed: u32,
    total: u32,
    code: Option<&str>,
) -> Result<String> {
    let session_id = format!(
        "test-{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis())
            .unwrap_or(0)
    );
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);
    let events = vec![serde_json::json!({
        "ts": now,
        "type": "test",
        "lesson_id": lesson_id,
        "course_id": course_id,
        "kc": [],
        "details": {
            "command": "pytest",
            "passed": passed,
            "failed": failed,
            "total": total,
            "code": code,
        }
    })];
    let payload = serde_json::to_string(&EventBatchBody {
        session_id: &session_id,
        events: &events,
    })?;
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
            workspace.save_active_item(SaveIntent::Save, window, cx).detach();

            let http = workspace.app_state().client.http_client();
            let cred = zed_credentials_provider::global(cx);

            let Some(path) = active_file_path(workspace, cx) else {
                breadcrumb("Run", "no active file -> noop");
                workspace.show_toast(
                    toast("Open a Python lesson file before pressing Run.".into(), NotificationId::unique::<RunToast>()),
                    cx,
                );
                return;
            };
            let Some((label, command, args, cwd)) = build_run_command(&path) else {
                breadcrumb("Run", format!("not runnable: {}", path.display()));
                workspace.show_toast(
                    toast(format!("Can't Run this file: {}", path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default()).into(), NotificationId::unique::<RunToast>()),
                    cx,
                );
                return;
            };
            let (lesson_id, course_id, _rel) = derive_submission_context(&path);
            let path_for_run_event = path.clone();
            let Some(terminal_panel) = workspace.panel::<TerminalPanel>(cx) else {
                breadcrumb("Run", "TerminalPanel unavailable");
                return;
            };
            workspace.show_toast(
                toast(format!("▶ Running {label} in terminal…").into(), NotificationId::unique::<RunToast>()),
                cx,
            );
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
            // Capture the file content NOW so the event reflects what was run.
            let code_for_run = std::fs::read_to_string(&path_for_run_event).ok();
            let label_for_run = label.clone();

            window.spawn(cx, async move |cx| {
                breadcrumb("Run", "deferred: calling terminal_panel.spawn_task");
                let result = terminal_panel.update_in(cx, |panel, window, cx| {
                    panel.spawn_task(&spawn, window, cx)
                });

                // POST a run event (with code snapshot) to BFF for the
                // teacher dashboard. Best-effort; failures don't surface.
                let read = cred.read_credentials(BFF_URL, cx).await.ok().flatten();
                if let Some((_, jwt_bytes)) = read
                    && let Ok(jwt) = String::from_utf8(jwt_bytes)
                {
                    match post_run_event(
                        http.clone(),
                        &jwt,
                        &lesson_id,
                        &course_id,
                        &label_for_run,
                        code_for_run.as_deref(),
                    )
                    .await
                    {
                        Ok(body) => breadcrumb("Run", format!("POST /events/batch OK: {body}")),
                        Err(e) => breadcrumb("Run", format!("POST /events/batch FAILED: {e}")),
                    }
                    // Drain any pasted text that accumulated.
                    if let Err(e) = flush_paste_log(http, &jwt, &lesson_id, &course_id).await {
                        breadcrumb("Run", format!("flush_paste_log FAILED: {e}"));
                    }
                } else {
                    breadcrumb("Run", "no JWT — skipping event POST");
                }

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
            workspace.save_active_item(SaveIntent::Save, window, cx).detach();
            let http = workspace.app_state().client.http_client();
            let cred = zed_credentials_provider::global(cx);
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
            let (lesson_id, course_id, _rel_path) = derive_submission_context(&path);
            let workspace_weak = cx.entity().downgrade();
            breadcrumb(
                "Test",
                format!(
                    "running `{label}` in {} (captured + visible)",
                    cwd.display()
                ),
            );

            // ALSO spawn pytest in the terminal panel so the student sees
            // the full pass/fail trace, not just a count. The captured
            // child below produces the toast + telemetry; this terminal
            // spawn is purely for visibility.
            if let Some(terminal_panel) = workspace.panel::<TerminalPanel>(cx) {
                // Build a verbose `python -m pytest -v <name>` task — flat
                // command string so Zed's command label renders correctly.
                let test_name = args.last().cloned().unwrap_or_default();
                let visible_label = format!("python -m pytest -v {test_name}");
                let visible_spawn = build_spawn_task(
                    "wolfcode-test-visible",
                    &visible_label,
                    &visible_label,
                    visible_label.clone(),
                    Vec::new(),
                    cwd.clone(),
                );
                let weak_terminal = terminal_panel.downgrade();
                window.spawn(cx, async move |cx| {
                    let _ = weak_terminal.update_in(cx, |panel, window, cx| {
                        panel.spawn_task(&visible_spawn, window, cx).detach();
                    });
                }).detach();
            }

            window.spawn(cx, async move |cx| {
                let push_toast = |msg: std::borrow::Cow<'static, str>,
                                   cx: &mut gpui::AsyncWindowContext| {
                    let _ = workspace_weak.update(cx, |ws, cx| {
                        ws.show_toast(toast(msg, NotificationId::unique::<TestToast>()), cx);
                    });
                };

                // Run pytest as a captured child process. std::process::Command
                // blocks the executor briefly while pytest runs (usually <2s for
                // a single lesson). v0.2 will move this to a background thread.
                let output = match std::process::Command::new(&command)
                    .args(&args)
                    .current_dir(&cwd)
                    .output()
                {
                    Ok(o) => o,
                    Err(e) => {
                        breadcrumb("Test", format!("spawn FAILED: {e}"));
                        push_toast(format!("Test failed: couldn't run pytest — {e}").into(), cx);
                        return;
                    }
                };
                let stdout = String::from_utf8_lossy(&output.stdout).to_string();
                let _stderr = String::from_utf8_lossy(&output.stderr).to_string();
                let exit_code = output.status.code().unwrap_or(-1);
                breadcrumb(
                    "Test",
                    format!(
                        "exit={exit_code} stdout={}B stderr={}B",
                        stdout.len(),
                        _stderr.len()
                    ),
                );
                // Dump the actual stdout content so we can diagnose parse failures.
                breadcrumb("Test", format!("stdout_dump: {}", stdout.replace('\n', " ¶ ")));

                let (passed, failed) = parse_pytest_summary(&stdout);
                let total = passed + failed;
                breadcrumb(
                    "Test",
                    format!("parsed: passed={passed} failed={failed} total={total}"),
                );
                // Toast result.
                if total == 0 {
                    push_toast(
                        format!("Test ran (exit {exit_code}) but no pass/fail summary found. Check the file pattern.").into(),
                        cx,
                    );
                } else if failed == 0 {
                    push_toast(format!("✓ All {passed} test{} passed!", if passed == 1 { "" } else { "s" }).into(), cx);
                    // Wipe stale failure state from any previous run.
                    lesson_tutor::clear_test_run();
                    let lesson_id_clone = lesson_id.clone();
                    let _ = workspace_weak.update_in(cx, |ws, window, cx| {
                        if let Some(panel) = ws.panel::<lesson_tutor::TutorPanel>(cx) {
                            panel.update(cx, |p, cx| p.celebrate_test_pass(lesson_id_clone, passed, cx));
                        }
                        // Refresh lesson-status badges in outline tree.
                        window.dispatch_action(Box::new(RefreshLessonStatus), cx);
                    });
                } else {
                    push_toast(
                        format!("✗ {failed} failed / {passed} passed (of {total}) — click 💡 in Tutor for an explanation").into(),
                        cx,
                    );

                    // Z-W15: stash pytest output for AI explanation + dispatch.
                    // Resolve student .py path (path may be the .test.py if
                    // student had it open; we want the student's code file).
                    let path_str = path.to_string_lossy().to_string();
                    let student_path = if path_str.ends_with(".test.py") {
                        let stem = path_str.trim_end_matches(".test.py").to_string();
                        PathBuf::from(format!("{stem}.py"))
                    } else {
                        path.clone()
                    };
                    let test_path = if path_str.ends_with(".test.py") {
                        path.clone()
                    } else {
                        let stem = path_str.trim_end_matches(".py").to_string();
                        PathBuf::from(format!("{stem}.test.py"))
                    };
                    let student_code = std::fs::read_to_string(&student_path).unwrap_or_default();
                    let test_code = std::fs::read_to_string(&test_path).ok();
                    let snap = lesson_tutor::TestRunSnapshot {
                        lesson_id: lesson_id.clone(),
                        student_code,
                        student_filename: student_path
                            .file_name()
                            .map(|n| n.to_string_lossy().into_owned())
                            .unwrap_or_default(),
                        test_code,
                        test_filename: Some(
                            test_path
                                .file_name()
                                .map(|n| n.to_string_lossy().into_owned())
                                .unwrap_or_default(),
                        ),
                        test_output: stdout.clone(),
                        exit_code,
                    };
                    lesson_tutor::stash_test_run(snap);
                    breadcrumb("Test", "stashed test run; dispatching ExplainLastTestFailure");
                    let _ = workspace_weak.update_in(cx, |_, window, cx| {
                        window.dispatch_action(Box::new(lesson_tutor::ExplainLastTestFailure), cx);
                    });
                }

                // POST to BFF /events/batch (best-effort; auth required).
                let read = cred.read_credentials(BFF_URL, cx).await.ok().flatten();
                let Some((_, jwt_bytes)) = read else {
                    breadcrumb("Test", "no JWT (skipping BFF event post)");
                    return;
                };
                let Ok(jwt) = String::from_utf8(jwt_bytes) else {
                    breadcrumb("Test", "JWT not UTF-8");
                    return;
                };
                // Capture code snapshot for the dashboard. Read from disk
                // (file was auto-saved at the top of Test).
                let path_for_code = {
                    let s = path.to_string_lossy().to_string();
                    if s.ends_with(".test.py") {
                        PathBuf::from(s.trim_end_matches(".test.py").to_string() + ".py")
                    } else {
                        path.clone()
                    }
                };
                let code_for_event = std::fs::read_to_string(&path_for_code).ok();
                match post_test_event(http.clone(), &jwt, &lesson_id, &course_id, passed, failed, total, code_for_event.as_deref()).await {
                    Ok(body) => breadcrumb("Test", format!("POST /events/batch OK: {body}")),
                    Err(e) => breadcrumb("Test", format!("POST /events/batch FAILED: {e}")),
                }
                if let Err(e) = flush_paste_log(http, &jwt, &lesson_id, &course_id).await {
                    breadcrumb("Test", format!("flush_paste_log FAILED: {e}"));
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
            workspace.save_active_item(SaveIntent::Save, window, cx).detach();
            let http = workspace.app_state().client.http_client();
            let cred = zed_credentials_provider::global(cx);
            let workspace_weak = cx.entity().downgrade();
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
                    // Helper to show a toast back on the workspace (UI feedback).
                    let toast_id = || NotificationId::unique::<SubmitToast>();
                    let push_toast = |msg: std::borrow::Cow<'static, str>,
                                       cx: &mut gpui::AsyncWindowContext| {
                        let _ = workspace_weak.update(cx, |ws, cx| {
                            ws.show_toast(toast(msg, toast_id()), cx);
                        });
                    };

                    let read = match cred.read_credentials(BFF_URL, cx).await {
                        Ok(opt) => opt,
                        Err(e) => {
                            breadcrumb("Submit", format!("keychain read FAILED: {e}"));
                            push_toast(format!("Submit failed: keychain — {e}").into(), cx);
                            return;
                        }
                    };
                    let Some((_, jwt_bytes)) = read else {
                        breadcrumb("Submit", "no JWT in keychain (run SignInFromFile first)");
                        push_toast(
                            "Not signed in. Run `WolfCode: Sign In From File` first.".into(),
                            cx,
                        );
                        return;
                    };
                    let jwt = match String::from_utf8(jwt_bytes) {
                        Ok(s) => s,
                        Err(e) => {
                            breadcrumb("Submit", format!("JWT not UTF-8: {e}"));
                            push_toast(format!("Submit failed: bad JWT — {e}").into(), cx);
                            return;
                        }
                    };
                    let content = match std::fs::read_to_string(&path) {
                        Ok(s) => s,
                        Err(e) => {
                            breadcrumb("Submit", format!("read file FAILED: {e}"));
                            push_toast(format!("Submit failed: couldn't read file — {e}").into(), cx);
                            return;
                        }
                    };
                    let bytes = content.len();
                    breadcrumb("Submit", format!("read content: {} bytes", bytes));

                    // Flush pastes BEFORE submit so they're recorded under
                    // this lesson rather than the next one the student opens.
                    if let Err(e) = flush_paste_log(http.clone(), &jwt, &lesson_id, &course_id).await {
                        breadcrumb("Submit", format!("flush_paste_log FAILED: {e}"));
                    }
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
                        Ok(reply) => {
                            breadcrumb("Submit", format!("OK: {reply}"));
                            // Reply looks like: {"submission_id":"<uuid>", ...}
                            let id_short = parse_submission_id(&reply)
                                .map(|s| s.chars().take(8).collect::<String>())
                                .unwrap_or_else(|| "?".to_string());
                            push_toast(
                                format!(
                                    "✓ Submitted {bytes} bytes · id={id_short}… · lesson={lesson_id}"
                                )
                                .into(),
                                cx,
                            );
                            // Refresh lesson-status badges in outline tree.
                            let _ = workspace_weak.update_in(cx, |_, window, cx| {
                                window.dispatch_action(Box::new(RefreshLessonStatus), cx);
                            });
                        }
                        Err(e) => {
                            breadcrumb("Submit", format!("FAILED: {e}"));
                            push_toast(format!("Submit failed: {e}").into(), cx);
                        }
                    }
                })
                .detach();
        });

        breadcrumb("init", "4 actions registered (Run / Test / Explain / Submit)");
    })
    .detach();
}
