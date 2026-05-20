//! WolfCode Lesson Panel.
//!
//! Displays metadata + action buttons for the currently active lesson.
//! Surfaces:
//!   - Lesson title, KC chips, estimated minutes
//!   - 6 visible action buttons: Run / Test / Submit / Hint L1 / L2 / L3
//!     (each dispatches an action from lesson_runner or lesson_tutor)
//!   - Auto-opens the entry `.py` file when the student clicks the
//!     `*.lesson.json` in the file tree, so they see the task description
//!     (embedded in the Python comments) and the editable starter code
//!     instead of raw JSON.

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
use ui::{Button, ButtonStyle, Tooltip};
use workspace::Workspace;
use workspace::dock::{DockPosition, Panel, PanelEvent};

/// Hard-coded trace path for self-verification.
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
    pub title_en: Option<String>,
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
            let result = workspace.update_in(cx, |workspace, window, cx| {
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
        let focus_handle = cx.focus_handle();
        let sub = cx.subscribe(
            &workspace_entity,
            |this, workspace, event: &workspace::Event, cx| {
                if matches!(event, workspace::Event::ActiveItemChanged) {
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
                    this.refresh_lesson(abs_path, cx);
                }
            },
        );

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
        let Some(abs_path) = abs_path else {
            self.set_lesson(None, cx);
            return;
        };
        if !abs_path
            .to_string_lossy()
            .to_string()
            .ends_with(".lesson.json")
        {
            // The active file might be the entry .py — keep showing whatever
            // lesson is already loaded.
            return;
        }
        breadcrumb("refresh_lesson", format!("loading {}", abs_path.display()));

        let fs = self.fs.clone();
        let weak_workspace = self.workspace.clone();
        cx.spawn(async move |this, cx| {
            let bytes = match fs.load_bytes(&abs_path).await {
                Ok(b) => b,
                Err(err) => {
                    breadcrumb("fs_load", format!("FAILED: {err}"));
                    return;
                }
            };
            let lesson: Option<Lesson> = match serde_json::from_slice::<Lesson>(&bytes) {
                Ok(l) => {
                    breadcrumb("fs_load", format!("parsed lesson id={}", l.id));
                    Some(l)
                }
                Err(err) => {
                    breadcrumb("fs_load", format!("parse FAILED: {err}"));
                    None
                }
            };

            // Z-W13: auto-open the entry .py file so the student sees code
            // + task description (in comments), not raw JSON.
            if let Some(l) = lesson.as_ref()
                && let Some(entry) = l.entry.as_ref()
                && let Some(lesson_dir) = abs_path.parent()
            {
                let entry_path = lesson_dir.join(entry);
                if entry_path.exists() {
                    breadcrumb("fs_load", format!("auto-opening entry: {}", entry_path.display()));
                    let _ = weak_workspace.update_in(cx, |workspace, window, cx| {
                        workspace
                            .open_abs_path(
                                entry_path.clone(),
                                workspace::OpenOptions::default(),
                                window,
                                cx,
                            )
                            .detach_and_log_err(cx);
                    });
                } else {
                    breadcrumb("fs_load", format!("entry not found: {}", entry_path.display()));
                }
            }

            let _ = this.update(cx, |this, cx| this.set_lesson(lesson, cx));
        })
        .detach();
    }

    fn set_lesson(&mut self, lesson: Option<Lesson>, cx: &mut Context<Self>) {
        let changed = match (&self.current, &lesson) {
            (Some(a), Some(b)) => a.id != b.id,
            (None, None) => false,
            _ => true,
        };
        // Only overwrite if a new lesson is provided. Clearing to None is rare
        // and would happen e.g. if a non-lesson file is the only thing open;
        // we want to keep the panel useful when student is editing the .py.
        if lesson.is_some() {
            self.current = lesson;
        } else if self.current.is_none() {
            // nothing to do
        }
        if changed {
            cx.notify();
        }
    }
}

impl Panel for LessonPanel {
    fn persistent_name() -> &'static str { LESSON_PANEL_KEY }
    fn panel_key() -> &'static str { LESSON_PANEL_KEY }
    fn position(&self, _: &Window, _: &App) -> DockPosition { DockPosition::Left }
    fn position_is_valid(&self, position: DockPosition) -> bool {
        matches!(position, DockPosition::Left | DockPosition::Right)
    }
    fn set_position(&mut self, _: DockPosition, _: &mut Window, _: &mut Context<Self>) {}
    fn default_size(&self, _: &Window, _: &App) -> Pixels { self.width.unwrap_or_else(|| px(320.)) }
    fn icon(&self, _: &Window, _: &App) -> Option<IconName> { Some(IconName::Book) }
    fn icon_tooltip(&self, _: &Window, _: &App) -> Option<&'static str> { Some("WolfCode Lesson") }
    fn toggle_action(&self) -> Box<dyn gpui::Action> { Box::new(ToggleFocus) }
    fn activation_priority(&self) -> u32 { 8 }
    fn icon_label(&self, _: &Window, _: &App) -> Option<String> { None }
    fn set_active(&mut self, active: bool, _: &mut Window, _: &mut Context<Self>) { self.active = active; }
    fn starts_open(&self, _: &Window, _: &App) -> bool { true }
}

impl Render for LessonPanel {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        breadcrumb(
            "render",
            format!(
                "called, current={}",
                self.current.as_ref().map(|l| l.id.as_str()).unwrap_or("none")
            ),
        );
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

    let mins_label: AnyElement = l
        .estimated_minutes
        .map(|m| Label::new(format!("~{m} min")).color(Color::Muted).into_any_element())
        .unwrap_or_else(|| div().into_any_element());

    // Helper: a labeled, full-width button that dispatches the given action.
    fn action_btn<A: gpui::Action + Clone>(
        id: &'static str,
        label: &'static str,
        action: A,
    ) -> Button {
        let action_for_tooltip = action.clone();
        Button::new(id, label)
            .style(ButtonStyle::Filled)
            .full_width()
            .tooltip(move |_, cx| Tooltip::for_action(label, &action_for_tooltip, cx))
            .on_click(move |_, window, cx| {
                window.dispatch_action(Box::new(action.clone()), cx);
            })
    }

    v_flex()
        .gap_3()
        .child(Label::new(l.title.clone()).size(LabelSize::Large))
        .when_some(l.title_en, |this, en| {
            this.child(Label::new(en).color(Color::Muted).size(LabelSize::Small))
        })
        .child(h_flex().gap_1().flex_wrap().children(kc_chips))
        .child(mins_label)
        .child(
            Label::new("任务说明在编辑器里的注释中 / Task description is in the code comments.")
                .size(LabelSize::Small)
                .color(Color::Muted),
        )
        // Action group 1: run code
        .child(
            v_flex()
                .gap_1()
                .child(
                    Label::new("Run / Test").size(LabelSize::XSmall).color(Color::Muted),
                )
                .child(action_btn("wolf-run", "▶  Run", lesson_runner::Run))
                .child(action_btn("wolf-test", "✓  Test", lesson_runner::Test))
                .child(action_btn("wolf-submit", "↗  Submit", lesson_runner::Submit)),
        )
        // Action group 2: ask tutor (hint ladder)
        .child(
            v_flex()
                .gap_1()
                .child(
                    Label::new("Stuck? Ask Tutor").size(LabelSize::XSmall).color(Color::Muted),
                )
                .child(action_btn("wolf-l1", "💡  Hint Level 1 (where)", lesson_tutor::AskL1))
                .child(action_btn("wolf-l2", "💡  Hint Level 2 (concept)", lesson_tutor::AskL2))
                .child(action_btn("wolf-l3", "💡  Hint Level 3 (analogy)", lesson_tutor::AskL3)),
        )
        .into_any_element()
}

fn render_empty() -> AnyElement {
    v_flex()
        .gap_2()
        .child(
            Label::new("No lesson loaded.")
                .color(Color::Muted),
        )
        .child(
            Label::new("打开一个 *.lesson.json 文件，编辑器会自动跳到对应的 Python 起步代码。")
                .size(LabelSize::Small)
                .color(Color::Muted),
        )
        .into_any_element()
}

pub fn init(cx: &mut App) {
    breadcrumb("init", "called");
    cx.observe_new(|workspace: &mut Workspace, _, _| {
        workspace.register_action(|workspace, _: &ToggleFocus, window, cx| {
            workspace.toggle_panel_focus::<LessonPanel>(window, cx);
        });
    })
    .detach();
}
