//! WolfCode Lesson Tutor.
//!
//! Z-W9 v0.1:
//!   - 3 hint-level actions: `AskL1`, `AskL2`, `AskL3` (Tutor Policy is
//!     enforced server-side; level just selects which system prompt the
//!     BFF injects)
//!   - `TutorPanel` (Right dock, Hubot icon) — displays the question,
//!     current status, and the final reply text. Streaming-into-panel is
//!     v0.2; v0.1 renders the reply once it completes.
//!   - `ToggleFocus` action to show/hide the panel
//!
//! Server still does the actual streaming + Tutor Policy. We're just
//! adding a visible surface for the response on the client.

use std::io::Write;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Result, anyhow};
use futures::AsyncReadExt as _;
use gpui::{
    App, AsyncWindowContext, Context as GpuiContext, Entity, EventEmitter, FocusHandle, Focusable,
    IntoElement, Pixels, Render, Subscription, Task, WeakEntity, Window, actions, px,
};
use http_client::{AsyncBody, HttpClient, HttpClientWithUrl, Method, Request};
use serde::Serialize;
use ui::prelude::*;
use ui::{Button, ButtonStyle};
use workspace::Workspace;
use workspace::dock::{DockPosition, Panel, PanelEvent};

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
    /// Ask the AI Tutor at hint level L1 (points to where you're stuck).
    AskL1,
    /// Ask the AI Tutor at hint level L2 (explains the concept).
    AskL2,
    /// Ask the AI Tutor at hint level L3 (gives pseudo-code / analogy).
    AskL3,
    /// Show/hide the WolfCode Tutor side panel.
    ToggleFocus,
]);

pub const TUTOR_PANEL_KEY: &str = "TutorPanel";

#[derive(Debug, Serialize)]
struct TutorRequest<'a> {
    lesson_id: &'a str,
    level: &'a str,
    user_question: &'a str,
    current_code: &'a str,
    allow_solution: bool,
}

fn active_file_path(workspace: &Workspace, cx: &App) -> Option<PathBuf> {
    let item = workspace.active_item(cx)?;
    let pp = item.project_path(cx)?;
    let project = workspace.project().read(cx);
    let worktree = project.worktree_for_id(pp.worktree_id, cx)?;
    Some(worktree.read(cx).absolutize(&pp.path))
}

fn derive_lesson_id(path: &PathBuf) -> String {
    let s = path.to_string_lossy();
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| "unknown".to_string());
    for suffix in [".lesson.json", ".test.py", ".py"] {
        if let Some(stem) = s.strip_suffix(suffix) {
            return PathBuf::from(stem)
                .file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or(name.clone());
        }
    }
    name
}

async fn ask_tutor(
    http: Arc<HttpClientWithUrl>,
    jwt: &str,
    lesson_id: &str,
    level: &str,
    user_question: &str,
    current_code: &str,
) -> Result<String> {
    let body = TutorRequest {
        lesson_id,
        level,
        user_question,
        current_code,
        allow_solution: false,
    };
    let payload = serde_json::to_string(&body)?;
    breadcrumb(
        "ask_tutor",
        format!("level={level} lesson={lesson_id} bytes={}", payload.len()),
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
    let mut body_text = String::new();
    resp.body_mut().read_to_string(&mut body_text).await?;
    breadcrumb("ask_tutor", format!("SSE body = {} bytes", body_text.len()));

    let mut combined = String::new();
    for line in body_text.lines() {
        let Some(payload) = line.strip_prefix("data:") else {
            continue;
        };
        // Preserve the single space that SSE protocol mandates after "data:",
        // but only ONE — kara-wiki sends tokens with significant leading
        // whitespace (the original v0.1 trim() collapsed inter-token spaces
        // into one mushed string).
        let payload = payload.strip_prefix(' ').unwrap_or(payload);
        if payload.is_empty() || payload == "[DONE]" {
            continue;
        }
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
    breadcrumb("ask_tutor", format!("combined = {} chars", combined.len()));
    Ok(combined)
}

// ----- TutorPanel ----------------------------------------------------------

pub struct TutorPanel {
    workspace: WeakEntity<Workspace>,
    focus_handle: FocusHandle,
    current_lesson_id: Option<String>,
    current_level: Option<&'static str>,
    status: PanelStatus,
    reply: Option<String>,
    width: Option<Pixels>,
    active: bool,
    _subs: Vec<Subscription>,
}

#[derive(Clone)]
enum PanelStatus {
    Idle,
    Asking,
    Replied(usize), // char count
    Error(String),
}

impl EventEmitter<PanelEvent> for TutorPanel {}

impl Focusable for TutorPanel {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl TutorPanel {
    pub fn load(
        workspace: WeakEntity<Workspace>,
        cx: AsyncWindowContext,
    ) -> Task<Result<Entity<Self>>> {
        breadcrumb("TutorPanel::load", "called");
        cx.spawn(async move |cx| {
            workspace.update_in(cx, |_workspace, _window, cx| {
                cx.new(|panel_cx| Self::new(workspace.clone(), panel_cx))
            })
        })
    }

    fn new(workspace: WeakEntity<Workspace>, cx: &mut GpuiContext<Self>) -> Self {
        breadcrumb("TutorPanel::new", "constructor entered");
        let focus_handle = cx.focus_handle();
        Self {
            workspace,
            focus_handle,
            current_lesson_id: None,
            current_level: None,
            status: PanelStatus::Idle,
            reply: None,
            width: None,
            active: false,
            _subs: vec![],
        }
    }

    fn start_request(
        &mut self,
        lesson_id: String,
        level: &'static str,
        cx: &mut GpuiContext<Self>,
    ) {
        breadcrumb(
            "TutorPanel::start_request",
            format!("lesson={lesson_id} level={level}"),
        );
        self.current_lesson_id = Some(lesson_id);
        self.current_level = Some(level);
        self.status = PanelStatus::Asking;
        self.reply = None;
        cx.notify();
    }

    fn finish_request(&mut self, reply: String, cx: &mut GpuiContext<Self>) {
        breadcrumb(
            "TutorPanel::finish_request",
            format!("reply = {} chars", reply.len()),
        );
        self.status = PanelStatus::Replied(reply.len());
        self.reply = Some(reply);
        cx.notify();
    }

    fn fail_request(&mut self, err: String, cx: &mut GpuiContext<Self>) {
        breadcrumb("TutorPanel::fail_request", &err);
        self.status = PanelStatus::Error(err);
        cx.notify();
    }
}

impl Panel for TutorPanel {
    fn persistent_name() -> &'static str {
        TUTOR_PANEL_KEY
    }

    fn panel_key() -> &'static str {
        TUTOR_PANEL_KEY
    }

    fn position(&self, _: &Window, _: &App) -> DockPosition {
        DockPosition::Right
    }

    fn position_is_valid(&self, position: DockPosition) -> bool {
        matches!(position, DockPosition::Left | DockPosition::Right)
    }

    fn set_position(&mut self, _: DockPosition, _: &mut Window, _: &mut GpuiContext<Self>) {}

    fn default_size(&self, _: &Window, _: &App) -> Pixels {
        self.width.unwrap_or_else(|| px(360.))
    }

    fn icon(&self, _: &Window, _: &App) -> Option<IconName> {
        Some(IconName::AiZed)
    }

    fn icon_tooltip(&self, _: &Window, _: &App) -> Option<&'static str> {
        Some("WolfCode Tutor")
    }

    fn toggle_action(&self) -> Box<dyn gpui::Action> {
        Box::new(ToggleFocus)
    }

    fn activation_priority(&self) -> u32 {
        7
    }

    fn icon_label(&self, _: &Window, _: &App) -> Option<String> {
        None
    }

    fn set_active(&mut self, active: bool, _: &mut Window, _: &mut GpuiContext<Self>) {
        self.active = active;
    }

    fn starts_open(&self, _: &Window, _: &App) -> bool {
        true
    }
}

impl Render for TutorPanel {
    fn render(&mut self, _window: &mut Window, _cx: &mut GpuiContext<Self>) -> impl IntoElement {
        breadcrumb(
            "TutorPanel::render",
            format!(
                "status={:?} reply_len={:?}",
                match &self.status {
                    PanelStatus::Idle => "idle",
                    PanelStatus::Asking => "asking",
                    PanelStatus::Replied(_) => "replied",
                    PanelStatus::Error(_) => "error",
                },
                self.reply.as_ref().map(|r| r.len())
            ),
        );

        let header = h_flex()
            .gap_2()
            .child(
                Label::new("WolfCode Tutor")
                    .size(LabelSize::Default)
                    .color(Color::Accent),
            )
            .child(match &self.status {
                PanelStatus::Idle => Label::new("idle").color(Color::Muted),
                PanelStatus::Asking => Label::new("asking...").color(Color::Info),
                PanelStatus::Replied(n) => {
                    Label::new(format!("replied · {n} chars")).color(Color::Success)
                }
                PanelStatus::Error(_) => Label::new("error").color(Color::Error),
            });

        let context_line = h_flex().gap_2().children(self.current_lesson_id.clone().map(|id| {
            Label::new(format!("lesson: {id}"))
                .size(LabelSize::Small)
                .color(Color::Muted)
        })).children(self.current_level.map(|l| {
            Label::new(format!("level: {l}"))
                .size(LabelSize::Small)
                .color(Color::Accent)
        }));

        let body: AnyElement = match (&self.status, self.reply.clone()) {
            (PanelStatus::Idle, _) => Label::new(
                "Open a Python lesson and run\n\
                 `WolfCode Tutor: Ask L1` from the command palette.",
            )
            .color(Color::Muted)
            .into_any_element(),
            (PanelStatus::Asking, _) => Label::new("Waiting for the AI Tutor...")
                .color(Color::Muted)
                .into_any_element(),
            (PanelStatus::Replied(_), Some(reply)) => Label::new(reply).into_any_element(),
            (PanelStatus::Replied(_), None) => {
                Label::new("(empty reply)").color(Color::Muted).into_any_element()
            }
            (PanelStatus::Error(e), _) => Label::new(e.clone()).color(Color::Error).into_any_element(),
        };

        let ask_buttons = v_flex()
            .gap_1()
            .child(
                Label::new("Ask again")
                    .size(LabelSize::XSmall)
                    .color(Color::Muted),
            )
            .child(
                Button::new("tutor-l1", "💡  Hint Level 1 — where am I stuck?")
                    .style(ButtonStyle::Filled)
                    .full_width()
                    .on_click(|_, window, cx| {
                        window.dispatch_action(Box::new(AskL1), cx);
                    }),
            )
            .child(
                Button::new("tutor-l2", "💡  Hint Level 2 — explain the concept")
                    .style(ButtonStyle::Filled)
                    .full_width()
                    .on_click(|_, window, cx| {
                        window.dispatch_action(Box::new(AskL2), cx);
                    }),
            )
            .child(
                Button::new("tutor-l3", "💡  Hint Level 3 — pseudo-code / analogy")
                    .style(ButtonStyle::Filled)
                    .full_width()
                    .on_click(|_, window, cx| {
                        window.dispatch_action(Box::new(AskL3), cx);
                    }),
            );

        v_flex()
            .key_context("TutorPanel")
            .id("tutor-panel")
            .size_full()
            .p_3()
            .gap_3()
            .child(header)
            .child(context_line)
            .child(ask_buttons)
            .child(body)
    }
}

// ----- Action dispatch helper ---------------------------------------------

fn run_ask(workspace: &mut Workspace, level: &'static str, window: &mut Window, cx: &mut GpuiContext<Workspace>) {
    breadcrumb("Ask", format!("level={level} invoked"));
    let http = workspace.app_state().client.http_client();
    let cred = zed_credentials_provider::global(cx);
    let Some(path) = active_file_path(workspace, cx) else {
        breadcrumb("Ask", "no active file");
        return;
    };
    let lesson_id = derive_lesson_id(&path);
    breadcrumb("Ask", format!("lesson_id={lesson_id} file={}", path.display()));

    let panel = workspace.panel::<TutorPanel>(cx);
    if let Some(p) = panel.clone() {
        p.update(cx, |panel, cx| panel.start_request(lesson_id.clone(), level, cx));
        workspace.toggle_panel_focus::<TutorPanel>(window, cx);
    }

    window
        .spawn(cx, async move |cx| {
            let read = match cred.read_credentials(BFF_URL, cx).await {
                Ok(opt) => opt,
                Err(e) => {
                    if let Some(p) = &panel {
                        let _ = p.update(cx, |panel, cx| panel.fail_request(format!("keychain read failed: {e}"), cx));
                    }
                    return;
                }
            };
            let Some((_, jwt_bytes)) = read else {
                if let Some(p) = &panel {
                    let _ = p.update(cx, |panel, cx| panel.fail_request("no JWT — run SignInFromFile".to_string(), cx));
                }
                return;
            };
            let jwt = match String::from_utf8(jwt_bytes) {
                Ok(s) => s,
                Err(e) => {
                    if let Some(p) = &panel {
                        let _ = p.update(cx, |panel, cx| panel.fail_request(format!("JWT not UTF-8: {e}"), cx));
                    }
                    return;
                }
            };
            let code = match std::fs::read_to_string(&path) {
                Ok(s) => s,
                Err(_) => String::new(),
            };
            match ask_tutor(http, &jwt, &lesson_id, level, "I'm not sure what to do next. Help me at this hint level.", &code).await {
                Ok(reply) => {
                    if let Some(p) = &panel {
                        let _ = p.update(cx, |panel, cx| panel.finish_request(reply, cx));
                    }
                }
                Err(e) => {
                    if let Some(p) = &panel {
                        let _ = p.update(cx, |panel, cx| panel.fail_request(format!("{e}"), cx));
                    }
                }
            }
        })
        .detach();
}

pub fn init(cx: &mut App) {
    breadcrumb("init", "called");
    cx.observe_new(|workspace: &mut Workspace, _, _| {
        breadcrumb("init", "observe_new fired");

        workspace.register_action(|w, _: &AskL1, window, cx| run_ask(w, "L1", window, cx));
        workspace.register_action(|w, _: &AskL2, window, cx| run_ask(w, "L2", window, cx));
        workspace.register_action(|w, _: &AskL3, window, cx| run_ask(w, "L3", window, cx));

        workspace.register_action(|workspace, _: &ToggleFocus, window, cx| {
            workspace.toggle_panel_focus::<TutorPanel>(window, cx);
        });

        breadcrumb("init", "4 actions registered (AskL1 / AskL2 / AskL3 / ToggleFocus)");
    })
    .detach();
}
