//! WolfCode Lesson Panel + Course Outline.
//!
//! One left-dock panel, two vertical sections:
//!   • Top:    Course Outline tree — chapters → kind sections → lessons.
//!             Clicking an item opens the lesson's entry .py file.
//!   • Bottom: Current lesson detail + action buttons (Run / Test / Submit /
//!             Hint L1 / L2 / L3).
//!
//! Outline is built by scanning `<workspace_root>/chapters/*/lessons/*.lesson.json`
//! once on construction. Future iteration will watch the FS for changes.

use std::collections::{BTreeMap, HashMap};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::Result;
use futures::AsyncReadExt as _;
use gpui::{
    AnyElement, App, AsyncWindowContext, Context, Entity, EventEmitter, FocusHandle, Focusable,
    InteractiveElement as _, IntoElement, ParentElement as _, Pixels, Render, StatefulInteractiveElement as _,
    Subscription, WeakEntity, Window, actions, div, px,
};
use http_client::{AsyncBody, HttpClient, HttpClientWithUrl, Method, Request};
use project::Fs;
use serde::Deserialize;
use ui::prelude::*;
use ui::{Button, ButtonStyle, Tooltip};
use workspace::{OpenOptions, Workspace};
use workspace::dock::{DockPosition, Panel, PanelEvent};

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
        let _ = writeln!(f, "{ts_ms} [lesson_panel::{component}] {}", msg.as_ref());
    }
    log::info!(target: "lesson_panel", "[{component}] {}", msg.as_ref());
}

actions!(lesson_panel, [
    /// Toggle the WolfCode Lesson panel.
    ToggleFocus,
]);
// Z-W16: status-refresh action lives in lesson_runner to avoid a crate
// dependency cycle (lesson_runner needs to dispatch it from Test/Submit,
// but already exports actions consumed by lesson_panel).

#[derive(Debug, Clone, Default, Deserialize)]
struct LessonStatusFlags {
    #[serde(default)]
    tested_pass: bool,
    #[serde(default)]
    submitted_at: Option<i64>,
    #[serde(default)]
    test_count: u32,
    #[serde(default)]
    submit_count: u32,
}

#[derive(Debug, Deserialize)]
struct LessonStatusReply {
    by_lesson: HashMap<String, LessonStatusFlags>,
}

pub const LESSON_PANEL_KEY: &str = "LessonPanel";

// ----- Lesson JSON ----------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LessonKind {
    Example,
    Exercise,
    Homework,
    Challenge,
}

impl Default for LessonKind {
    fn default() -> Self {
        LessonKind::Exercise
    }
}

impl LessonKind {
    fn icon(self) -> &'static str {
        match self {
            LessonKind::Example => "📘",
            LessonKind::Exercise => "✏️",
            LessonKind::Homework => "📝",
            LessonKind::Challenge => "🧠",
        }
    }
    fn label(self) -> &'static str {
        match self {
            LessonKind::Example => "Examples",
            LessonKind::Exercise => "Exercises",
            LessonKind::Homework => "Homework",
            LessonKind::Challenge => "Challenges",
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct Lesson {
    pub id: String,
    pub title: String,
    #[serde(default)]
    pub title_en: Option<String>,
    #[serde(default)]
    pub kind: Option<LessonKind>,
    #[serde(default)]
    pub no_paste: bool,
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

// ----- Outline data --------------------------------------------------------

#[derive(Debug, Clone)]
struct OutlineItem {
    id: String,
    title: String,
    kind: LessonKind,
    no_paste: bool,
    entry_path: PathBuf,
}

#[derive(Debug, Default)]
struct ChapterGroup {
    slug: String,
    display: String,
    sections: BTreeMap<LessonKind, Vec<OutlineItem>>,
}

fn title_case_slug(slug: &str) -> String {
    // "01-variables-types" -> "Variables Types"
    let trimmed = slug
        .trim_start_matches(|c: char| c.is_ascii_digit() || c == '-')
        .replace('-', " ");
    trimmed
        .split_whitespace()
        .map(|w| {
            let mut chars = w.chars();
            match chars.next() {
                Some(c) => c.to_ascii_uppercase().to_string() + chars.as_str(),
                None => String::new(),
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

// ----- Panel ---------------------------------------------------------------

pub struct LessonPanel {
    workspace: WeakEntity<Workspace>,
    fs: Arc<dyn Fs>,
    focus_handle: FocusHandle,
    current: Option<Lesson>,
    current_entry_path: Option<PathBuf>,
    outline: Vec<ChapterGroup>,
    /// Keyed by filename-stem lesson id (matches what lesson_runner sends
    /// in /events/batch + /submissions). e.g. "01-first-variable".
    lesson_status: HashMap<String, LessonStatusFlags>,
    width: Option<Pixels>,
    active: bool,
    _subs: Vec<Subscription>,
}

/// Derive the BFF lesson_id from the entry .py path — same scheme that
/// lesson_runner's `derive_submission_context` uses (filename stem).
fn lesson_id_for_status(entry_path: &Path) -> String {
    entry_path
        .file_name()
        .and_then(|n| n.to_str())
        .map(|n| {
            n.strip_suffix(".py")
                .unwrap_or(n)
                .to_string()
        })
        .unwrap_or_default()
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
                // Resolve http + cred HERE while workspace is the active
                // borrow; we don't want LessonPanel::new to read the
                // workspace entity (would double-borrow the same lease).
                let http = workspace.app_state().client.http_client();
                let cred = zed_credentials_provider::global(cx);
                let weak_workspace = cx.entity().downgrade();

                let worktree_root = workspace
                    .project()
                    .read(cx)
                    .visible_worktrees(cx)
                    .next()
                    .map(|wt| wt.read(cx).abs_path().to_path_buf());

                cx.new(|panel_cx| {
                    Self::new(
                        weak_workspace,
                        fs,
                        http,
                        cred,
                        worktree_root,
                        window,
                        panel_cx,
                    )
                })
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
        fs: Arc<dyn Fs>,
        http: Arc<HttpClientWithUrl>,
        cred: Arc<dyn credentials_provider::CredentialsProvider>,
        worktree_root: Option<PathBuf>,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let focus_handle = cx.focus_handle();
        // Re-upgrade WeakEntity to subscribe to workspace events. This is
        // safe — workspace is being constructed; we just get an Entity
        // handle, not a lease.
        let workspace_entity = workspace
            .upgrade()
            .expect("workspace entity must exist during LessonPanel::new");
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

        // Kick off async outline scan if we have a worktree root.
        if let Some(root) = worktree_root {
            let fs_for_scan = fs.clone();
            cx.spawn(async move |this, cx| {
                let outline = scan_outline(fs_for_scan, &root).await;
                breadcrumb(
                    "scan_outline",
                    format!(
                        "found {} chapters, {} items total",
                        outline.len(),
                        outline.iter().flat_map(|c| c.sections.values()).map(|v| v.len()).sum::<usize>()
                    ),
                );
                let _ = this.update(cx, |this, cx| {
                    this.outline = outline;
                    cx.notify();
                });
            })
            .detach();
        }

        // Z-W16: also fetch lesson status badges on construction (uses the
        // http + cred resolved in load() so we never read workspace from here).
        let http_for_init = http.clone();
        let cred_for_init = cred.clone();
        cx.spawn(async move |this, cx| {
            let _ = this.update(cx, |this, cx| {
                this.fetch_status_with(http_for_init, cred_for_init, cx)
            });
        }).detach();

        Self {
            workspace,
            fs,
            focus_handle,
            current: None,
            current_entry_path: None,
            outline: Vec::new(),
            lesson_status: HashMap::new(),
            width: None,
            active: false,
            _subs: vec![sub],
        }
    }

    /// Caller pre-resolves http + cred so we don't read the workspace entity
    /// while it may be held mutably by an action handler upstream.
    fn fetch_status_with(
        &self,
        http: Arc<HttpClientWithUrl>,
        cred: Arc<dyn credentials_provider::CredentialsProvider>,
        cx: &mut Context<Self>,
    ) {
        cx.spawn(async move |this, cx| {
            let read = cred.read_credentials(BFF_URL, cx).await.ok().flatten();
            let Some((_, jwt_bytes)) = read else {
                breadcrumb("fetch_status", "no JWT — skipping");
                return;
            };
            let Ok(jwt) = String::from_utf8(jwt_bytes) else { return };
            let req = Request::builder()
                .method(Method::GET)
                .uri(format!("{BFF_URL}/reports/me/lesson-status"))
                .header("Authorization", format!("Bearer {jwt}"))
                .body(AsyncBody::empty());
            let Ok(req) = req else { return };
            let resp = http.send(req).await;
            let Ok(mut resp) = resp else {
                breadcrumb("fetch_status", "http error");
                return;
            };
            if !resp.status().is_success() {
                breadcrumb("fetch_status", format!("status={}", resp.status()));
                return;
            }
            let mut body = String::new();
            if resp.body_mut().read_to_string(&mut body).await.is_err() {
                return;
            }
            let parsed: LessonStatusReply = match serde_json::from_str(&body) {
                Ok(v) => v,
                Err(e) => {
                    breadcrumb("fetch_status", format!("parse FAILED: {e}"));
                    return;
                }
            };
            breadcrumb(
                "fetch_status",
                format!("got {} entries", parsed.by_lesson.len()),
            );
            let _ = this.update(cx, |this, cx| {
                this.lesson_status = parsed.by_lesson;
                cx.notify();
            });
        }).detach();
    }

    fn refresh_lesson(
        &mut self,
        abs_path: Option<std::path::PathBuf>,
        cx: &mut Context<Self>,
    ) {
        let Some(abs_path) = abs_path else {
            return;
        };

        // If a .py is active and it matches an outline item, surface its lesson.
        let p_str = abs_path.to_string_lossy().to_string();
        if !p_str.ends_with(".lesson.json") {
            // Try to find an outline item whose entry_path matches.
            let matched = self.outline.iter()
                .flat_map(|c| c.sections.values())
                .flat_map(|v| v.iter())
                .find(|it| it.entry_path == abs_path)
                .cloned();
            if let Some(it) = matched {
                if self.current.as_ref().map(|l| &l.id) != Some(&it.id) {
                    self.current_entry_path = Some(it.entry_path.clone());
                    // Load the JSON to populate full Lesson struct.
                    self.load_lesson_from_outline_item(it, cx);
                }
            }
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

            // Auto-open the entry .py file so the student sees code, not JSON.
            let mut opened_entry: Option<PathBuf> = None;
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
                                OpenOptions::default(),
                                window,
                                cx,
                            )
                            .detach_and_log_err(cx);
                    });
                    opened_entry = Some(entry_path);
                } else {
                    breadcrumb("fs_load", format!("entry not found: {}", entry_path.display()));
                }
            }

            let _ = this.update(cx, |this, cx| {
                this.current_entry_path = opened_entry;
                this.set_lesson(lesson, cx);
            });
        })
        .detach();
    }

    fn load_lesson_from_outline_item(&mut self, item: OutlineItem, cx: &mut Context<Self>) {
        // Item.entry_path is the .py. The .lesson.json lives next to it.
        // We need the JSON to populate KCs, title, etc., for the detail view.
        // For now, synthesize a Lesson stub from the OutlineItem.
        let lesson = Lesson {
            id: item.id.clone(),
            title: item.title.clone(),
            title_en: None,
            kind: Some(item.kind),
            no_paste: item.no_paste,
            kc: Vec::new(),
            language: Some("python".to_string()),
            entry: item.entry_path.file_name().map(|n| n.to_string_lossy().into_owned()),
            test: None,
            run: None,
            estimated_minutes: None,
            tests_pass_required: None,
        };
        self.set_lesson(Some(lesson), cx);
    }

    fn set_lesson(&mut self, lesson: Option<Lesson>, cx: &mut Context<Self>) {
        let changed = match (&self.current, &lesson) {
            (Some(a), Some(b)) => a.id != b.id,
            (None, None) => false,
            _ => true,
        };
        if lesson.is_some() {
            self.current = lesson;
        }
        // Z-W19a: tell the editor whether to block paste for this lesson.
        let block = self.current.as_ref().map(|l| l.no_paste).unwrap_or(false);
        editor::set_paste_blocked(block);
        breadcrumb("set_lesson", format!("paste_blocked={block}"));
        if changed {
            cx.notify();
        }
    }

    fn open_outline_item(&mut self, item: &OutlineItem, window: &mut Window, cx: &mut Context<Self>) {
        breadcrumb("open_outline_item", format!("id={} path={}", item.id, item.entry_path.display()));
        let Some(workspace) = self.workspace.upgrade() else {
            breadcrumb("open_outline_item", "workspace dead");
            return;
        };
        let path = item.entry_path.clone();
        workspace.update(cx, |ws, cx| {
            ws.open_abs_path(path, OpenOptions::default(), window, cx)
                .detach_and_log_err(cx);
        });
    }
}

// Async scan for `<root>/chapters/*/lessons/*.lesson.json`.
async fn scan_outline(fs: Arc<dyn Fs>, root: &Path) -> Vec<ChapterGroup> {
    let chapters_dir = root.join("chapters");
    let mut chapters: BTreeMap<String, ChapterGroup> = BTreeMap::new();

    let Ok(mut dir) = fs.read_dir(&chapters_dir).await else {
        breadcrumb(
            "scan_outline",
            format!("no chapters dir at {}", chapters_dir.display()),
        );
        return Vec::new();
    };
    // futures::StreamExt is needed for `next` — pull it in.
    use futures::StreamExt as _;
    while let Some(entry) = dir.next().await {
        let Ok(chapter_path) = entry else { continue };
        let Some(slug_os) = chapter_path.file_name() else { continue };
        let slug = slug_os.to_string_lossy().to_string();
        // Skip non-directories.
        if !fs.is_dir(&chapter_path).await {
            continue;
        }
        let lessons_dir = chapter_path.join("lessons");
        let Ok(mut lessons_iter) = fs.read_dir(&lessons_dir).await else {
            continue;
        };
        let group = chapters.entry(slug.clone()).or_insert_with(|| ChapterGroup {
            slug: slug.clone(),
            display: title_case_slug(&slug),
            sections: BTreeMap::new(),
        });
        while let Some(lesson_entry) = lessons_iter.next().await {
            let Ok(lesson_path) = lesson_entry else { continue };
            let name = lesson_path
                .file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_default();
            if !name.ends_with(".lesson.json") {
                continue;
            }
            let bytes = match fs.load_bytes(&lesson_path).await {
                Ok(b) => b,
                Err(_) => continue,
            };
            let lesson: Lesson = match serde_json::from_slice(&bytes) {
                Ok(l) => l,
                Err(e) => {
                    breadcrumb(
                        "scan_outline",
                        format!("skip {}: {}", lesson_path.display(), e),
                    );
                    continue;
                }
            };
            let entry_name = match lesson.entry.as_ref() {
                Some(e) => e,
                None => continue,
            };
            let entry_path = lesson_path
                .parent()
                .map(|p| p.join(entry_name))
                .unwrap_or_else(|| lesson_path.clone());
            let kind = lesson.kind.unwrap_or_default();
            let item = OutlineItem {
                id: lesson.id.clone(),
                title: lesson.title.clone(),
                kind,
                no_paste: lesson.no_paste,
                entry_path,
            };
            group.sections.entry(kind).or_default().push(item);
        }
    }
    // Sort items within each section by id for deterministic order.
    let mut out: Vec<ChapterGroup> = chapters.into_values().collect();
    for c in &mut out {
        for items in c.sections.values_mut() {
            items.sort_by(|a, b| a.id.cmp(&b.id));
        }
    }
    out
}

// ----- Render --------------------------------------------------------------

impl Panel for LessonPanel {
    fn persistent_name() -> &'static str { LESSON_PANEL_KEY }
    fn panel_key() -> &'static str { LESSON_PANEL_KEY }
    fn position(&self, _: &Window, _: &App) -> DockPosition { DockPosition::Left }
    fn position_is_valid(&self, position: DockPosition) -> bool {
        matches!(position, DockPosition::Left | DockPosition::Right)
    }
    fn set_position(&mut self, _: DockPosition, _: &mut Window, _: &mut Context<Self>) {}
    fn default_size(&self, _: &Window, _: &App) -> Pixels { self.width.unwrap_or_else(|| px(340.)) }
    fn icon(&self, _: &Window, _: &App) -> Option<IconName> { Some(IconName::Book) }
    fn icon_tooltip(&self, _: &Window, _: &App) -> Option<&'static str> { Some("WolfCode Lesson") }
    fn toggle_action(&self) -> Box<dyn gpui::Action> { Box::new(ToggleFocus) }
    fn activation_priority(&self) -> u32 { 8 }
    fn icon_label(&self, _: &Window, _: &App) -> Option<String> { None }
    fn set_active(&mut self, active: bool, _: &mut Window, _: &mut Context<Self>) { self.active = active; }
    fn starts_open(&self, _: &Window, _: &App) -> bool { true }
}

impl Render for LessonPanel {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        breadcrumb(
            "render",
            format!(
                "current={} outline_chapters={}",
                self.current.as_ref().map(|l| l.id.as_str()).unwrap_or("none"),
                self.outline.len()
            ),
        );

        v_flex()
            .key_context("LessonPanel")
            .id("lesson-panel")
            .size_full()
            .p_3()
            .gap_3()
            .child(
                Label::new("WolfCode")
                    .size(LabelSize::Default)
                    .color(Color::Accent),
            )
            .child(self.render_outline(cx))
            .child(div().h_px().bg(cx.theme().colors().border).w_full())
            .child(match self.current.clone() {
                Some(l) => render_lesson(l),
                None => render_empty(),
            })
    }
}

impl LessonPanel {
    fn render_outline(&self, cx: &mut Context<Self>) -> AnyElement {
        let current_id = self.current.as_ref().map(|l| l.id.clone());
        let mut col = v_flex().gap_1().child(
            Label::new("Course Outline")
                .size(LabelSize::XSmall)
                .color(Color::Muted),
        );

        if self.outline.is_empty() {
            col = col.child(
                Label::new("Scanning chapters…")
                    .size(LabelSize::Small)
                    .color(Color::Muted),
            );
            return col.into_any_element();
        }

        for chapter in &self.outline {
            col = col.child(
                Label::new(format!("📂 {}", chapter.display))
                    .size(LabelSize::Small)
                    .color(Color::Accent),
            );
            for (kind, items) in &chapter.sections {
                col = col.child(
                    Label::new(format!("  {} {} ({})", kind.icon(), kind.label(), items.len()))
                        .size(LabelSize::XSmall)
                        .color(Color::Muted),
                );
                for item in items {
                    let is_current = current_id.as_deref() == Some(item.id.as_str());
                    let bullet = if is_current { "● " } else { "○ " };
                    let label_color = if is_current { Color::Accent } else { Color::Default };
                    let item_clone = item.clone();

                    // Z-W16: status badges. Key is the .py file stem.
                    let status_key = lesson_id_for_status(&item.entry_path);
                    let status = self.lesson_status.get(&status_key).cloned().unwrap_or_default();
                    let mut row = h_flex().items_center().gap_1();
                    row = row.child(
                        Label::new(format!("{bullet}{}", item.title))
                            .size(LabelSize::Small)
                            .color(label_color),
                    );
                    if status.tested_pass {
                        row = row.child(
                            div()
                                .px_1()
                                .rounded_sm()
                                .child(
                                    Label::new("✓test")
                                        .size(LabelSize::XSmall)
                                        .color(Color::Success),
                                ),
                        );
                    }
                    if status.submitted_at.is_some() {
                        row = row.child(
                            div()
                                .px_1()
                                .rounded_sm()
                                .child(
                                    Label::new("✓sub")
                                        .size(LabelSize::XSmall)
                                        .color(Color::Info),
                                ),
                        );
                    }

                    col = col.child(
                        div()
                            .id(gpui::ElementId::Name(item.id.clone().into()))
                            .pl_4()
                            .px_1()
                            .py_0p5()
                            .rounded_sm()
                            .cursor_pointer()
                            .hover(|s| s.bg(cx.theme().colors().element_hover))
                            .child(row)
                            .on_click(cx.listener(move |this, _, window, cx| {
                                let item = item_clone.clone();
                                this.open_outline_item(&item, window, cx);
                            })),
                    );
                }
            }
        }

        col.into_any_element()
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

    fn action_btn<A: gpui::Action + Clone>(
        id: &'static str,
        label: &'static str,
        action: A,
    ) -> Button {
        let action_for_tooltip = action.clone();
        let id_for_log: &'static str = id;
        Button::new(id, label)
            .style(ButtonStyle::Filled)
            .full_width()
            .tooltip(move |_, cx| Tooltip::for_action(label, &action_for_tooltip, cx))
            .on_click(move |_, window, cx| {
                breadcrumb("action_btn", format!("click: id={id_for_log}"));
                window.dispatch_action(Box::new(action.clone()), cx);
            })
    }

    let no_paste = l.no_paste;
    v_flex()
        .gap_3()
        .child(Label::new(l.title.clone()).size(LabelSize::Large))
        .when_some(l.title_en, |this, en| {
            this.child(Label::new(en).color(Color::Muted).size(LabelSize::Small))
        })
        .child(h_flex().gap_1().flex_wrap().children(kc_chips))
        .child(mins_label)
        .when(no_paste, |this| {
            this.child(
                div()
                    .px_2()
                    .py_1()
                    .rounded_sm()
                    .border_1()
                    .child(
                        Label::new("🔒 No-paste mode: type it out, don't paste AI answers.")
                            .size(LabelSize::Small)
                            .color(Color::Warning),
                    ),
            )
        })
        .child(
            Label::new("任务说明在编辑器里的注释中 / Task description is in the code comments.")
                .size(LabelSize::Small)
                .color(Color::Muted),
        )
        .child(
            v_flex()
                .gap_1()
                .child(Label::new("Run / Test").size(LabelSize::XSmall).color(Color::Muted))
                .child(action_btn("wolf-run", "▶  Run", lesson_runner::Run))
                .child(action_btn("wolf-test", "✓  Test", lesson_runner::Test))
                .child(action_btn("wolf-submit", "↗  Submit", lesson_runner::Submit)),
        )
        .child(
            v_flex()
                .gap_1()
                .child(Label::new("Stuck? Ask Tutor").size(LabelSize::XSmall).color(Color::Muted))
                .child(action_btn("wolf-l1", "💡  Hint Level 1 (where)", lesson_tutor::AskL1))
                .child(action_btn("wolf-l2", "💡  Hint Level 2 (concept)", lesson_tutor::AskL2))
                .child(action_btn("wolf-l3", "💡  Hint Level 3 (analogy)", lesson_tutor::AskL3)),
        )
        .into_any_element()
}

fn render_empty() -> AnyElement {
    v_flex()
        .gap_2()
        .child(Label::new("Pick a lesson from the outline above.").color(Color::Muted))
        .into_any_element()
}

pub fn init(cx: &mut App) {
    breadcrumb("init", "called");
    cx.observe_new(|workspace: &mut Workspace, _, _| {
        workspace.register_action(|workspace, _: &ToggleFocus, window, cx| {
            workspace.toggle_panel_focus::<LessonPanel>(window, cx);
        });
        workspace.register_action(|workspace, _: &lesson_runner::RefreshLessonStatus, _window, cx| {
            breadcrumb("RefreshLessonStatus", "action invoked");
            let http = workspace.app_state().client.http_client();
            let cred = zed_credentials_provider::global(cx);
            if let Some(panel) = workspace.panel::<LessonPanel>(cx) {
                panel.update(cx, |p, cx| p.fetch_status_with(http, cred, cx));
            }
        });
        // Z-W19a: surface a toast when paste is blocked.
        workspace.register_action(|workspace, _: &editor::PasteWasBlocked, _window, cx| {
            breadcrumb("PasteWasBlocked", "action invoked");
            workspace.show_toast(
                workspace::Toast::new(
                    workspace::notifications::NotificationId::unique::<PasteBlockedToast>(),
                    "🔒 Paste is disabled for this lesson. Type it out yourself — the AI Tutor (💡) can hint you in your own words.",
                )
                .autohide(),
                cx,
            );
        });
    })
    .detach();
}

struct PasteBlockedToast;
