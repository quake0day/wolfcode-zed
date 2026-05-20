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
use std::time::{SystemTime, UNIX_EPOCH};

use gpui::{App, actions};
use workspace::Workspace;

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
/// Returns `Some((command, cwd))` if we can derive a sensible action,
/// or `None` if the active file isn't a Python file we can run.
fn build_run_command(path: &PathBuf) -> Option<(String, PathBuf)> {
    let s = path.to_string_lossy();
    if !s.ends_with(".py") {
        return None;
    }
    // For pytest-style test files, prefer pytest at the course root so
    // pytest.ini's `--import-mode=importlib` takes effect.
    let parent = path.parent()?.to_path_buf();
    let filename = path.file_name()?.to_string_lossy().to_string();
    Some((format!("python {filename}"), parent))
}

fn build_test_command(path: &PathBuf) -> Option<(String, PathBuf)> {
    let s = path.to_string_lossy();
    let test_path: PathBuf = if s.ends_with(".test.py") {
        path.clone()
    } else if s.ends_with(".py") {
        // Convert `xxx.py` -> `xxx.test.py` if such a sibling exists.
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
    // pytest is run from the course root so pytest.ini is honored.
    // For now we just run from the lesson directory; pytest.ini at course
    // root is found by pytest walking up.
    Some((format!("python -m pytest {name}"), parent))
}

pub fn init(cx: &mut App) {
    breadcrumb("init", "called");
    cx.observe_new(|workspace: &mut Workspace, _, _| {
        breadcrumb("init", "observe_new fired");

        workspace.register_action(|workspace, _: &Run, _window, cx| {
            breadcrumb("Run", "action invoked");
            let Some(path) = active_file_path(workspace, cx) else {
                breadcrumb("Run", "no active file -> noop");
                return;
            };
            match build_run_command(&path) {
                Some((cmd, cwd)) => breadcrumb(
                    "Run",
                    format!("would spawn `{cmd}` in {}", cwd.display()),
                ),
                None => breadcrumb(
                    "Run",
                    format!("active file not runnable: {}", path.display()),
                ),
            }
        });

        workspace.register_action(|workspace, _: &Test, _window, cx| {
            breadcrumb("Test", "action invoked");
            let Some(path) = active_file_path(workspace, cx) else {
                breadcrumb("Test", "no active file -> noop");
                return;
            };
            match build_test_command(&path) {
                Some((cmd, cwd)) => breadcrumb(
                    "Test",
                    format!("would spawn `{cmd}` in {}", cwd.display()),
                ),
                None => breadcrumb(
                    "Test",
                    format!("no test file matches: {}", path.display()),
                ),
            }
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

        workspace.register_action(|workspace, _: &Submit, _window, cx| {
            breadcrumb("Submit", "action invoked");
            let Some(path) = active_file_path(workspace, cx) else {
                breadcrumb("Submit", "no active file -> noop");
                return;
            };
            breadcrumb(
                "Submit",
                format!("would POST BFF /submissions for {}", path.display()),
            );
            // TODO Z-W7: implement BFF submission upload
        });

        breadcrumb("init", "4 actions registered (Run / Test / Explain / Submit)");
    })
    .detach();
}
