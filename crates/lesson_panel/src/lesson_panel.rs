//! WolfCode Lesson Panel.
//!
//! Displays metadata for the currently active `*.lesson.json` file:
//! title, KC (knowledge component) chips, estimated minutes.
//!
//! Z-W2 scope (this version):
//! - Detect when the active editor's file ends in `.lesson.json`
//! - Read and parse it
//! - Show title, KC list, estimated minutes
//! - Empty state when no lesson is active
//!
//! Future Z-W3 will add sibling-file matching (open the `.py` and the
//! panel still tracks the lesson) and a Run/Test/Submit action bar.

use std::io::Write;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::Result;
use gpui::{
    AnyElement, App, AsyncWindowContext, Context, Entity, EventEmitter, FocusHandle, Focusable,
    IntoElement, Pixels, Render, Subscription, WeakEntity, Window, actions, div, px,
};
use project::Fs;
use serde::Deserialize;
use ui::prelude::*;
use workspace::Workspace;
use workspace::dock::{DockPosition, Panel, PanelEvent};

/// Hard-coded trace path for self-verification during Z-W2 development.
/// Each launch appends; the test harness truncates before launch.
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
        let _ = writeln!(f, "{ts_ms} [lesson_panel::{component}] {}", msg.as_ref());
    }
    // Also emit to log for completeness.
    log::info!(target: "lesson_panel", "[{component}] {}", msg.as_ref());
}

actions!(lesson_panel, [
    /// Toggle the WolfCode Lesson panel.
    ToggleFocus
]);

pub const LESSON_PANEL_KEY: &str = "LessonPanel";

#[derive(Debug, Clone, Deserialize)]
pub struct Lesson {
    pub id: String,
    pub title: String,
    #[serde(default)]
    pub kc: Vec<String>,
    #[serde(default)]
    pub language: Option<String>,
    #[serde(default)]
    pub entry: Option<String>,
    #[serde(default)]
    pub test: Option<String>,
    #[serde(default)]
    pub run: Option<String>,
    #[serde(default)]
    pub estimated_minutes: Option<u32>,
    #[serde(default)]
    pub tests_pass_required: Option<u32>,
}

pub struct LessonPanel {
    workspace: WeakEntity<Workspace>,
    fs: Arc<dyn Fs>,
    focus_handle: FocusHandle,
    current: Option<Lesson>,
    width: Option<Pixels>,
    active: bool,
    _subs: Vec<Subscription>,
}

impl EventEmitter<PanelEvent> for LessonPanel {}

impl Focusable for LessonPanel {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl LessonPanel {
    pub fn load(
        workspace: WeakEntity<Workspace>,
        cx: AsyncWindowContext,
    ) -> gpui::Task<Result<Entity<Self>>> {
        breadcrumb("load", "called");
        cx.spawn(async move |cx| {
            breadcrumb("load", "inside spawn closure");
            let result = workspace.update_in(cx, |workspace, window, cx| {
                breadcrumb("load", "inside workspace.update_in");
                let fs = workspace.app_state().fs.clone();
                let weak_workspace = cx.entity().downgrade();
                let ws_entity = cx.entity();
                cx.new(|panel_cx| Self::new(weak_workspace, ws_entity, fs, window, panel_cx))
            });
            match &result {
                Ok(_) => breadcrumb("load", "Entity<LessonPanel> created OK"),
                Err(e) => breadcrumb("load", format!("update_in FAILED: {e}")),
            }
            result
        })
    }

    fn new(
        workspace: WeakEntity<Workspace>,
        workspace_entity: Entity<Workspace>,
        fs: Arc<dyn Fs>,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        breadcrumb("new", "constructor entered");
        let focus_handle = cx.focus_handle();

        let sub = cx.subscribe(
            &workspace_entity,
            |this, workspace, event: &workspace::Event, cx| {
                breadcrumb("subscribe", "workspace event received");
                if matches!(event, workspace::Event::ActiveItemChanged) {
                    breadcrumb("subscribe", "ActiveItemChanged matched");
                    // Extract the active file's absolute path while we
                    // still hold the workspace's immutable borrow. After
                    // the block ends, cx is free for `&mut Self` use.
                    let abs_path: Option<std::path::PathBuf> = {
                        let ws = workspace.read(cx);
                        ws.active_item(cx)
                            .and_then(|item| item.project_path(cx))
                            .and_then(|pp| {
                                ws.project()
                                    .read(cx)
                                    .worktree_for_id(pp.worktree_id, cx)
                                    .map(|wt| wt.read(cx).absolutize(&pp.path))
                            })
                    };
                    breadcrumb("subscribe", format!("abs_path = {abs_path:?}"));
                    this.refresh_lesson(abs_path, cx);
                }
            },
        );
        breadcrumb("new", "subscribed to workspace events");

        // Initial refresh is skipped: workspace_entity is currently being
        // mutated by the caller's update_in scope; reading it here would
        // double-lease. The first refresh fires on the next ActiveItemChanged
        // event. For v0.1 the panel is empty until the user clicks a file.
        let _ = workspace_entity;
        let _ = cx;
        Self {
            workspace,
            fs,
            focus_handle,
            current: None,
            width: None,
            active: false,
            _subs: vec![sub],
        }
    }

    fn refresh_lesson(
        &mut self,
        abs_path: Option<std::path::PathBuf>,
        cx: &mut Context<Self>,
    ) {
        breadcrumb("refresh_lesson", format!("abs_path = {abs_path:?}"));
        let Some(abs_path) = abs_path else {
            breadcrumb("refresh_lesson", "no abs_path -> clear lesson");
            self.set_lesson(None, cx);
            return;
        };
        if !abs_path
            .to_string_lossy()
            .to_string()
            .ends_with(".lesson.json")
        {
            breadcrumb("refresh_lesson", "not a .lesson.json file -> clear lesson");
            self.set_lesson(None, cx);
            return;
        }
        breadcrumb("refresh_lesson", "is a .lesson.json -> spawn fs load");

        let fs = self.fs.clone();
        cx.spawn(async move |this, cx| {
            breadcrumb("fs_load", "awaiting fs.load_bytes");
            let bytes = match fs.load_bytes(&abs_path).await {
                Ok(b) => {
                    breadcrumb("fs_load", format!("read {} bytes", b.len()));
                    b
                }
                Err(err) => {
                    breadcrumb("fs_load", format!("FAILED: {err}"));
                    log::warn!("lesson_panel: failed to read {abs_path:?}: {err}");
                    return;
                }
            };
            let lesson: Option<Lesson> = match serde_json::from_slice::<Lesson>(&bytes) {
                Ok(l) => {
                    breadcrumb("fs_load", format!("parsed lesson id={} title={:?}", l.id, l.title));
                    Some(l)
                }
                Err(err) => {
                    breadcrumb("fs_load", format!("parse FAILED: {err}"));
                    log::warn!("lesson_panel: failed to parse {abs_path:?}: {err}");
                    None
                }
            };
            let upd = this.update(cx, |this, cx| this.set_lesson(lesson, cx));
            breadcrumb("fs_load", format!("set_lesson update: {}", if upd.is_ok() {"OK"} else {"WeakEntity dropped"}));
        })
        .detach();
    }

    fn set_lesson(&mut self, lesson: Option<Lesson>, cx: &mut Context<Self>) {
        breadcrumb("set_lesson", format!("called with lesson.is_some()={}", lesson.is_some()));
        let changed = match (&self.current, &lesson) {
            (Some(a), Some(b)) => a.id != b.id,
            (None, None) => false,
            _ => true,
        };
        self.current = lesson;
        if changed {
            breadcrumb("set_lesson", "changed -> cx.notify()");
            cx.notify();
        }
    }
}

impl Panel for LessonPanel {
    fn persistent_name() -> &'static str {
        LESSON_PANEL_KEY
    }

    fn panel_key() -> &'static str {
        LESSON_PANEL_KEY
    }

    fn position(&self, _: &Window, _: &App) -> DockPosition {
        DockPosition::Left
    }

    fn position_is_valid(&self, position: DockPosition) -> bool {
        matches!(position, DockPosition::Left | DockPosition::Right)
    }

    fn set_position(&mut self, _: DockPosition, _: &mut Window, _: &mut Context<Self>) {
        // Position is currently fixed; will wire to settings in a later step.
    }

    fn default_size(&self, _: &Window, _: &App) -> Pixels {
        self.width.unwrap_or_else(|| px(280.))
    }

    fn icon(&self, _: &Window, _: &App) -> Option<IconName> {
        Some(IconName::Book)
    }

    fn icon_tooltip(&self, _: &Window, _: &App) -> Option<&'static str> {
        Some("WolfCode Lesson")
    }

    fn toggle_action(&self) -> Box<dyn gpui::Action> {
        Box::new(ToggleFocus)
    }

    fn activation_priority(&self) -> u32 {
        8
    }

    fn icon_label(&self, _: &Window, _: &App) -> Option<String> {
        None
    }

    fn set_active(&mut self, active: bool, _: &mut Window, _: &mut Context<Self>) {
        self.active = active;
    }

    fn starts_open(&self, _: &Window, _: &App) -> bool {
        false
    }
}

impl Render for LessonPanel {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        breadcrumb("render", format!("called, current={:?}", self.current.as_ref().map(|l| &l.id)));
        v_flex()
            .key_context("LessonPanel")
            .id("lesson-panel")
            .size_full()
            .p_3()
            .gap_3()
            .child(
                Label::new("WolfCode Lesson")
                    .size(LabelSize::Default)
                    .color(Color::Accent),
            )
            .child(match self.current.clone() {
                Some(l) => render_lesson(l),
                None => render_empty(),
            })
    }
}

fn render_lesson(l: Lesson) -> AnyElement {
    let kc_chips = l
        .kc
        .iter()
        .map(|kc| {
            div()
                .px_1p5()
                .py_0p5()
                .rounded_sm()
                .border_1()
                .child(Label::new(kc.clone()).size(LabelSize::XSmall))
                .into_any_element()
        })
        .collect::<Vec<_>>();

    let mins_label = l
        .estimated_minutes
        .map(|m| Label::new(format!("~{m} min")).color(Color::Muted))
        .unwrap_or_else(|| Label::new(String::new()));

    v_flex()
        .gap_2()
        .child(Label::new(l.title).size(LabelSize::Large))
        .child(h_flex().gap_1().flex_wrap().children(kc_chips))
        .child(mins_label)
        .into_any_element()
}

fn render_empty() -> AnyElement {
    Label::new("Open a *.lesson.json file to start.")
        .color(Color::Muted)
        .into_any_element()
}

pub fn init(cx: &mut App) {
    breadcrumb("init", "called");
    cx.observe_new(|workspace: &mut Workspace, _, _| {
        breadcrumb("init", "observe_new fired for new Workspace");
        workspace.register_action(|workspace, _: &ToggleFocus, window, cx| {
            breadcrumb("init", "ToggleFocus action handler invoked");
            workspace.toggle_panel_focus::<LessonPanel>(window, cx);
        });
        breadcrumb("init", "ToggleFocus action registered on workspace");
    })
    .detach();
}
