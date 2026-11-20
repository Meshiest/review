//! The settings panel: theme family, mode, avatars, notifications, the diff,
//! the interface and buffer fonts, and the workspaces "Review with agent"
//! runs in. Every appearance control applies live through
//! [`appearance::update`], which also saves; workspaces are checked and saved
//! to `config.toml` with their own button.

use std::cell::Cell;
use std::rc::Rc;

use forge::model::RepoRef;
use gpui::{
    Anchor, AnyElement, App, AppContext as _, Bounds, Context, ElementId, Entity, FocusHandle,
    Focusable as _, FontFallbacks, Global, InteractiveElement as _, IntoElement, MouseDownEvent,
    ParentElement, PathPromptOptions, Pixels, Render, ScrollHandle, SharedString,
    StatefulInteractiveElement as _, Styled, Subscription, Task, Window, actions, anchored, canvas,
    deferred, div, point, prelude::FluentBuilder as _, px,
};
use gpui_kit::assets::IconName;
use gpui_kit::component::{
    Icon,
    dock::{Panel, panel_handle, register_panel},
    h_flex,
    input::{Input, InputEvent, InputState, Textarea, TextareaState},
    v_flex,
};
use theme::{FontSettings, ModeSetting, Theme};

use crate::appearance::{self, Appearance};
use crate::config::{Config, WorkspaceEntry};
use crate::dir_browser::{self, Listing};
use crate::machines::{self, Machine};
use crate::panels::{dock_panel, header};
use crate::session::Session;
use crate::settings::{
    FontOverride, LINE_HEIGHT_RANGE, NotificationSettings, SIZE_RANGE, Settings,
};
use crate::terminals::{self, Terminals};
use crate::ui::{self, Button, ButtonKind, TextExt as _};
use crate::workspaces::{self, SaveState, Workspaces};

pub const SETTINGS: &str = "Settings";
/// The font list's key context; its search box holds focus while it is open.
pub const PICKER_CONTEXT: &str = "FontPicker";

actions!(
    settings,
    [
        /// Esc in the font list: closes it.
        ClosePicker
    ]
);

/// Rows the font list draws at once; the search box narrows the rest.
const PICKER_ROWS: usize = 120;
const PICKER_HEIGHT: f32 = 220.0;
/// The search box, the gap and the padding around the font list.
const PICKER_CHROME: f32 = 52.0;
const LABEL_WIDTH: f32 = 90.0;
/// The Workspaces section's place among the scrolled sections.
const WORKSPACES_SECTION: usize = 8;
const INTERFACE_PREVIEW: &str =
    "Review requested: the quick brown fox jumps over the lazy dog 0123456789";
const BUFFER_PREVIEW: &str = "fn main() { let n = 0x1F; } // 0O1lI| -> != >=";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum FontRole {
    Interface,
    Buffer,
}

impl FontRole {
    fn title(self) -> &'static str {
        match self {
            Self::Interface => "Interface font",
            Self::Buffer => "Buffer font",
        }
    }

    fn hint(self) -> &'static str {
        match self {
            Self::Interface => "Labels, buttons and prose",
            Self::Buffer => "Diff, log, terminal and composer",
        }
    }

    fn id(self) -> &'static str {
        match self {
            Self::Interface => "interface-font",
            Self::Buffer => "buffer-font",
        }
    }

    fn index(self) -> usize {
        match self {
            Self::Interface => 0,
            Self::Buffer => 1,
        }
    }

    fn size_id(self) -> &'static str {
        match self {
            Self::Interface => "interface-size",
            Self::Buffer => "buffer-size",
        }
    }

    fn line_height_id(self) -> &'static str {
        match self {
            Self::Interface => "interface-line-height",
            Self::Buffer => "buffer-line-height",
        }
    }

    fn edit(self, settings: &mut Settings) -> &mut FontOverride {
        match self {
            Self::Interface => &mut settings.interface_font,
            Self::Buffer => &mut settings.buffer_font,
        }
    }

    fn effective(self, theme: &Theme) -> &FontSettings {
        match self {
            Self::Interface => &theme.fonts.interface,
            Self::Buffer => &theme.fonts.buffer,
        }
    }

    fn preview(self) -> &'static str {
        match self {
            Self::Interface => INTERFACE_PREVIEW,
            Self::Buffer => BUFFER_PREVIEW,
        }
    }
}

/// The one settings panel, so the title bar button and `Ctrl+,` find the
/// panel a restored layout already holds.
struct Existing(Entity<SettingsPanel>);

impl Global for Existing {}

pub struct SettingsPanel {
    focus: FocusHandle,
    search: Entity<InputState>,
    /// Which font's family list is open.
    picker: Option<FontRole>,
    /// Installed and bundled families, once the platform has listed them.
    families: Vec<SharedString>,
    /// Where each family button was drawn last, which its list hangs from.
    anchors: [Rc<Cell<Bounds<Pixels>>>; 2],
    scroll: ScrollHandle,
    workspace_rows: Vec<WorkspaceRow>,
    /// The workspaces' epoch the rows were built from.
    workspaces_epoch: u64,
    /// The folder list open under one row's directory on an SSH machine.
    browser: Option<DirBrowser>,
    _subscriptions: Vec<Subscription>,
}

dock_panel!(SettingsPanel, "Settings");

pub fn register(cx: &mut App) {
    register_panel(cx, SETTINGS, |_, window, cx| {
        panel_handle(SettingsPanel::new(window, cx))
    });
}

impl SettingsPanel {
    pub fn new(window: &mut Window, cx: &mut App) -> Entity<Self> {
        let panel = cx.new(|cx| {
            let search = cx.new(|cx| InputState::new(window, cx).placeholder("Search fonts"));
            let mut subscriptions = vec![
                cx.observe_global::<theme::ActiveTheme>(|_: &mut Self, cx| cx.notify()),
                cx.observe(&search, |_, _, cx| cx.notify()),
                cx.subscribe_in(
                    &search,
                    window,
                    |this, _, event: &InputEvent, window, cx| {
                        if let InputEvent::PressEnter { .. } = event {
                            this.pick_first_match(window, cx);
                        }
                    },
                ),
            ];
            if let Some(workspaces) = Workspaces::global(cx) {
                subscriptions.push(cx.observe_in(
                    &workspaces,
                    window,
                    |this: &mut Self, _, window, cx| this.sync_workspaces(window, cx),
                ));
            }
            let mut panel = Self {
                focus: cx.focus_handle(),
                search,
                picker: None,
                families: Vec::new(),
                anchors: Default::default(),
                scroll: ScrollHandle::new(),
                workspace_rows: Vec::new(),
                workspaces_epoch: u64::MAX,
                browser: None,
                _subscriptions: subscriptions,
            };
            panel.sync_workspaces(window, cx);
            panel
        });
        panel.update(cx, |this, cx| this.list_families(cx));
        cx.set_global(Existing(panel.clone()));
        panel
    }

    /// The panel `new` built last, if any.
    pub fn existing(cx: &App) -> Option<Entity<Self>> {
        cx.try_global::<Existing>()
            .map(|existing| existing.0.clone())
    }

    /// Which font's list is open: `interface-font` or `buffer-font`.
    #[must_use]
    pub fn open_picker(&self) -> Option<&'static str> {
        self.picker.map(FontRole::id)
    }

    /// Whether the font list's search box has the keyboard.
    #[must_use]
    pub fn search_has_focus(&self, window: &Window, cx: &App) -> bool {
        self.search.read(cx).focus_handle(cx).is_focused(window)
    }

    /// Opens the interface font's list, as a click on its button does.
    pub fn open_interface_picker(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.picker != Some(FontRole::Interface) {
            self.toggle_picker(FontRole::Interface, window, cx);
        }
    }

    fn close_picker(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.picker.take().is_some() {
            window.focus(&self.focus, cx);
            cx.notify();
        }
    }

    /// The platform lists its fonts off the main thread; the bundled families
    /// are always offered.
    fn list_families(&mut self, cx: &mut Context<Self>) {
        let text_system = cx.text_system().clone();
        cx.spawn(async move |this, cx| {
            let installed = cx
                .background_executor()
                .spawn(async move { text_system.all_font_names() })
                .await;
            this.update(cx, |this, cx| {
                let mut names: Vec<SharedString> = installed.into_iter().map(Into::into).collect();
                names.extend(
                    [theme::fonts::GEIST, theme::fonts::GEIST_MONO].map(SharedString::from),
                );
                names.sort_by_cached_key(|name| name.to_lowercase());
                names.dedup();
                this.families = names;
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn toggle_picker(&mut self, role: FontRole, window: &mut Window, cx: &mut Context<Self>) {
        if self.picker == Some(role) {
            self.picker = None;
        } else {
            self.picker = Some(role);
            self.search
                .update(cx, |search, cx| search.set_value(String::new(), window, cx));
            window.focus(&self.search.read(cx).focus_handle(cx), cx);
        }
        cx.notify();
    }

    fn matches(&self, cx: &App) -> Vec<SharedString> {
        let query = self.search.read(cx).value().trim().to_lowercase();
        self.families
            .iter()
            .filter(|name| query.is_empty() || name.to_lowercase().contains(&query))
            .take(PICKER_ROWS)
            .cloned()
            .collect()
    }

    fn pick_first_match(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(role) = self.picker else {
            return;
        };
        if let Some(name) = self.matches(cx).into_iter().next() {
            self.pick_family(role, &name, window, cx);
        }
    }

    fn pick_family(
        &mut self,
        role: FontRole,
        name: &SharedString,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        appearance::update(cx, |settings| {
            role.edit(settings).family = Some(name.to_string());
        });
        self.picker = None;
        window.focus(&self.focus, cx);
        cx.notify();
    }

    fn section(&self, cx: &App, title: &'static str, hint: &'static str) -> gpui::Div {
        let t = ui::theme(cx);
        v_flex().gap(px(8.0)).child(
            h_flex()
                .gap(px(8.0))
                .items_baseline()
                .child(
                    div()
                        .font_weight(gpui::FontWeight::SEMIBOLD)
                        .text_color(t.colors.text)
                        .child(title),
                )
                .child(div().tiny(&t).text_color(t.colors.text_muted).child(hint)),
        )
    }

    fn theme_section(&self, t: &Theme, settings: &Settings, cx: &mut Context<Self>) -> AnyElement {
        let families = cx.global::<Appearance>().registry.families();
        let current = families
            .iter()
            .find(|family| family.name.eq_ignore_ascii_case(&settings.theme))
            .map(|family| family.name.clone());
        let options = families
            .iter()
            .map(|family| (family.name.clone(), SharedString::from(family.name.clone())))
            .collect();
        let modes = [
            (ModeSetting::System, "System"),
            (ModeSetting::Dark, "Dark"),
            (ModeSetting::Light, "Light"),
        ]
        .map(|(mode, label)| (mode, SharedString::from(label)))
        .to_vec();
        let mode = settings.mode;
        self.section(cx, "Theme", "Built-in and from the themes folder")
            .child(ui::segmented(
                cx,
                "theme-family",
                options,
                move |name| current.as_deref() == Some(name.as_str()),
                |name, _, cx| appearance::update(cx, |s| s.theme.clone_from(name)),
            ))
            .child(
                h_flex()
                    .gap(px(12.0))
                    .items_center()
                    .child(
                        div()
                            .w(px(LABEL_WIDTH))
                            .small(t)
                            .text_color(t.colors.text_secondary)
                            .child("Mode"),
                    )
                    .child(ui::segmented(
                        cx,
                        "theme-mode",
                        modes,
                        move |m| *m == mode,
                        |m, _, cx| appearance::update(cx, |s| s.mode = *m),
                    ))
                    .child(
                        div()
                            .tiny(t)
                            .text_color(t.colors.text_muted)
                            .child(format!("Showing {} {}", t.name, t.mode)),
                    ),
            )
            .into_any_element()
    }

    fn images_section(&self, settings: &Settings, cx: &App) -> AnyElement {
        let t = ui::theme(cx);
        self.section(
            cx,
            "Avatars",
            "Loaded from the forge's own host and its avatar CDN",
        )
        .child(
            ui::checkbox("load-gravatar")
                .label("Load Gravatar images")
                .checked(settings.load_gravatar)
                .on_click(|checked: &bool, _, cx| {
                    appearance::update(cx, |s| s.load_gravatar = *checked);
                }),
        )
        .child(
            div()
                .tiny(&t)
                .text_color(t.colors.text_muted)
                .child("Gravatar is a third party. GitLab uses it for accounts without an upload."),
        )
        .into_any_element()
    }

    fn notifications_section(&self, settings: &Settings, cx: &App) -> AnyElement {
        let t = ui::theme(cx);
        let n = settings.notifications;
        let toggle = |id: &'static str,
                      label: &'static str,
                      on: bool,
                      set: fn(&mut NotificationSettings, bool)| {
            ui::checkbox(id)
                .label(label)
                .checked(on)
                .on_click(move |checked: &bool, _, cx| {
                    let checked = *checked;
                    appearance::update(cx, |s| set(&mut s.notifications, checked));
                })
        };
        self.section(
            cx,
            "Notifications",
            "System toasts while the app runs; the activity list keeps every change",
        )
        .child(toggle(
            "notify-enabled",
            "Show notifications",
            n.enabled,
            |s, on| s.enabled = on,
        ))
        .child(toggle(
            "notify-watch-mine",
            "Watch pull requests I opened or review",
            n.watch_mine,
            |s, on| s.watch_mine = on,
        ))
        .child(
            v_flex()
                .pl(px(18.0))
                .gap(px(6.0))
                .child(toggle(
                    "notify-ci-failed",
                    "Checks failed",
                    n.ci_failed,
                    |s, on| {
                        s.ci_failed = on;
                    },
                ))
                .child(toggle(
                    "notify-ci-recovered",
                    "Checks pass again",
                    n.ci_recovered,
                    |s, on| s.ci_recovered = on,
                ))
                .child(toggle(
                    "notify-ci-finished",
                    "Checks finished and passed",
                    n.ci_finished,
                    |s, on| s.ci_finished = on,
                ))
                .child(toggle(
                    "notify-reviews",
                    "A review on my pull request",
                    n.reviews,
                    |s, on| s.reviews = on,
                ))
                .child(toggle(
                    "notify-comments",
                    "A comment on my pull request",
                    n.comments,
                    |s, on| s.comments = on,
                ))
                .child(toggle(
                    "notify-merged",
                    "Merged or closed",
                    n.merged_closed,
                    |s, on| s.merged_closed = on,
                )),
        )
        .child(
            div().tiny(&t).text_color(t.colors.text_muted).child(
                "At most one toast per pull request every 10 seconds; later changes join it.",
            ),
        )
        .into_any_element()
    }

    fn inbox_section(&self, settings: &Settings, cx: &App) -> AnyElement {
        type Field = fn(&mut Settings) -> &mut bool;
        let rows: [(&'static str, &'static str, bool, Field); 4] = [
            (
                "inbox-mark-seen",
                "Mark a pull request seen when it is opened",
                settings.inbox.mark_seen_on_open,
                |s| &mut s.inbox.mark_seen_on_open,
            ),
            (
                "inbox-mark-reviewed",
                "Mark it reviewed once every file is viewed",
                settings.inbox.mark_reviewed_when_viewed,
                |s| &mut s.inbox.mark_reviewed_when_viewed,
            ),
            (
                "inbox-unread-first",
                "List unread pull requests first",
                settings.inbox.unread_first,
                |s| &mut s.inbox.unread_first,
            ),
            (
                "inbox-dim-reviewed",
                "Fade reviewed pull requests with nothing new",
                settings.inbox.dim_reviewed,
                |s| &mut s.inbox.dim_reviewed,
            ),
        ];
        rows.into_iter()
            .fold(
                self.section(
                    cx,
                    "Inbox",
                    "Review marks kept on this computer, never sent to the forge",
                ),
                |section, (id, label, checked, field)| {
                    section.child(ui::checkbox(id).label(label).checked(checked).on_click(
                        move |checked: &bool, _, cx| {
                            appearance::update(cx, |s| *field(s) = *checked);
                        },
                    ))
                },
            )
            .into_any_element()
    }

    fn diff_section(&self, settings: &Settings, cx: &App) -> AnyElement {
        self.section(cx, "Diff", "How files show in a pull request's diff")
            .child(
                ui::checkbox("diff-collapse-viewed")
                    .label("Collapse a file when it is marked viewed")
                    .checked(settings.diff.collapse_viewed)
                    .on_click(|checked: &bool, _, cx| {
                        appearance::update(cx, |s| s.diff.collapse_viewed = *checked);
                    }),
            )
            .into_any_element()
    }

    fn font_section(&mut self, role: FontRole, t: &Theme, cx: &mut Context<Self>) -> AnyElement {
        let c = &t.colors;
        let font = role.effective(t).clone();
        let open = self.picker == Some(role);
        let family_button = h_flex()
            .id(ElementId::from((role.id(), 0usize)))
            .w(px(260.0))
            .h(px(26.0))
            .px(px(10.0))
            .gap(px(8.0))
            .items_center()
            .justify_between()
            .rounded(t.radius.md)
            .border_1()
            .border_color(if open { c.focus } else { c.control_border })
            .bg(c.field)
            .text_color(c.text)
            .cursor_pointer()
            .on_click(cx.listener(move |this, _, window, cx| {
                this.toggle_picker(role, window, cx);
            }))
            .child(div().truncate().child(font.family()))
            .child(
                Icon::new(IconName::ChevronDown)
                    .size(px(14.0))
                    .text_color(c.text_muted),
            );
        let anchor = self.anchors[role.index()].clone();
        let recorded = anchor.clone();
        let panel = cx.weak_entity();
        let family_button = div()
            .relative()
            .flex_none()
            .debug_selector(move || format!("family-{}", role.id()))
            .child(family_button)
            .child(
                canvas(
                    move |bounds, _, cx| {
                        // The list hangs from where the button is now.
                        if recorded.replace(bounds) != bounds && open {
                            panel.update(cx, |_, cx| cx.notify()).ok();
                        }
                    },
                    |_, (), _, _| {},
                )
                .absolute()
                .top_0()
                .left_0()
                .size_full(),
            );
        let (size, line_height) = (font.size, font.line_height);
        let step_size = move |delta: f32| {
            move |_: &gpui::ClickEvent, _: &mut Window, cx: &mut App| {
                appearance::update(cx, |s| {
                    role.edit(s).size = Some((size + delta).clamp(SIZE_RANGE.0, SIZE_RANGE.1));
                });
            }
        };
        let step_line = move |delta: i32| {
            move |_: &gpui::ClickEvent, _: &mut Window, cx: &mut App| {
                appearance::update(cx, |s| {
                    role.edit(s).line_height = Some(
                        line_height
                            .saturating_add_signed(delta)
                            .clamp(LINE_HEIGHT_RANGE.0, LINE_HEIGHT_RANGE.1),
                    );
                });
            }
        };
        let preview = div()
            .w_full()
            .px(px(10.0))
            .py(px(6.0))
            .rounded(t.radius.md)
            .border_1()
            .border_color(c.border)
            .bg(c.surface_raised)
            .text_color(c.text)
            .font_family(font.family())
            .text_size(font.size_px())
            .line_height(font.line_height_px())
            .child(role.preview());
        self.section(cx, role.title(), role.hint())
            .child(
                h_flex()
                    .gap(px(6.0))
                    .items_center()
                    .child(
                        div()
                            .w(px(LABEL_WIDTH))
                            .small(t)
                            .text_color(c.text_secondary)
                            .child("Family"),
                    )
                    .child(family_button),
            )
            .when(open, |this| {
                this.child(self.picker_overlay(role, &font.family, anchor.get(), t, cx))
            })
            .child(ui::stepper(
                cx,
                role.size_id(),
                "Size",
                format!("{}", font.size),
                step_size(-1.0),
                step_size(1.0),
            ))
            .child(ui::stepper(
                cx,
                role.line_height_id(),
                "Line height",
                format!("{}", font.line_height),
                step_line(-1),
                step_line(1),
            ))
            .child(preview)
            .into_any_element()
    }

    /// The font list, hung under the family button (over it when there is
    /// no room below) and drawn above the form, so opening it moves
    /// nothing. Esc or a click outside closes it; focus stays in its search
    /// box.
    fn picker_overlay(
        &self,
        role: FontRole,
        current: &str,
        button: Bounds<Pixels>,
        t: &Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let list = self.picker_list(role, current, t, cx);
        let close_outside = cx.listener(move |this, event: &MouseDownEvent, window, cx| {
            // A click on the family button toggles the list itself.
            if !button.contains(&event.position) {
                this.close_picker(window, cx);
            }
        });
        deferred(
            anchored()
                .attach(button, Anchor::BottomLeft)
                .offset(point(px(0.0), px(4.0)))
                .child(
                    div()
                        .id("font-picker")
                        .debug_selector(|| "font-picker".into())
                        .key_context(PICKER_CONTEXT)
                        .occlude()
                        .on_action(cx.listener(|this, _: &ClosePicker, window, cx| {
                            this.close_picker(window, cx);
                        }))
                        .on_mouse_down_out(close_outside)
                        .child(list),
                ),
        )
        .with_priority(1)
        .into_any_element()
    }

    fn picker_list(
        &self,
        role: FontRole,
        current: &str,
        t: &Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let c = &t.colors;
        let matches = self.matches(cx);
        let hover = c.hover;
        let interface = t.fonts.interface.family();
        let faces: Vec<gpui::Font> = matches
            .iter()
            .map(|name| own_face(name, &interface))
            .collect();
        let rows = matches
            .into_iter()
            .zip(faces)
            .enumerate()
            .map(|(ix, (name, face))| {
                let chosen = name.as_ref() == current;
                let pick = name.clone();
                h_flex()
                    .id(ElementId::from(("font-row", ix)))
                    .h(px(24.0))
                    .flex_none()
                    .px(px(8.0))
                    .gap(px(6.0))
                    .items_center()
                    .cursor_pointer()
                    .text_color(if chosen { c.text } else { c.text_secondary })
                    .hover(move |s| s.bg(hover))
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.pick_family(role, &pick, window, cx);
                    }))
                    .child(div().w(px(14.0)).flex_none().children(chosen.then(|| {
                        Icon::new(IconName::Check)
                            .size(px(14.0))
                            .text_color(c.accent_fg)
                    })))
                    .child(div().truncate().font(face).child(name))
            });
        let list = if self.families.is_empty() {
            div()
                .p(px(8.0))
                .text_color(c.text_muted)
                .child("Listing installed fonts")
                .into_any_element()
        } else {
            v_flex().children(rows).into_any_element()
        };
        v_flex()
            .w(px(350.0))
            .gap(px(6.0))
            .p(px(8.0))
            .rounded(t.radius.lg)
            .border_1()
            .border_color(c.border_strong)
            .bg(c.surface_menu)
            // A fixed height, so where it hangs (and whether it flips up)
            // does not wait on its rows being measured.
            .h(px(PICKER_HEIGHT + PICKER_CHROME))
            .child(Input::new(&self.search))
            .child(
                div()
                    .id(ElementId::from((role.id(), 1usize)))
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .small(t)
                    .child(list),
            )
            .into_any_element()
    }
}

/// One workspace being edited on the settings page.
struct WorkspaceRow {
    /// Kept as the config or the request had it; the row does not edit it.
    host: Option<String>,
    repo: Entity<InputState>,
    machine: String,
    path: Entity<InputState>,
    workdir: Entity<InputState>,
    command: Entity<InputState>,
    prompt: Entity<TextareaState>,
    allow_commands: bool,
    error: Option<String>,
    /// Draws the panel again as the repository or directory is typed, which
    /// the line about cloning shows.
    _watch: [Subscription; 2],
}

/// Which of a row's directories a folder button fills.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DirField {
    /// The repository's directory.
    Repository,
    /// The agent's working directory.
    Workdir,
}

impl DirField {
    /// The prefix of the field's selectors: `<id>-browse-<row>` for its
    /// folder button, `<id>-input-<row>` for its input.
    #[must_use]
    pub fn id(self) -> &'static str {
        match self {
            Self::Repository => "workspace-path",
            Self::Workdir => "workspace-workdir",
        }
    }
}

/// The folder list under one row's directory, on an SSH machine.
struct DirBrowser {
    row: usize,
    field: DirField,
    machine: Machine,
    /// The last folder listed.
    listing: Option<Listing>,
    /// The folder being listed now.
    loading: Option<String>,
    error: Option<String>,
    _task: Option<Task<()>>,
}

impl WorkspaceRow {
    fn new(entry: &WorkspaceEntry, window: &mut Window, cx: &mut Context<SettingsPanel>) -> Self {
        let mut input = |value: &str, placeholder: &'static str, window: &mut Window| {
            let value = value.to_owned();
            cx.new(|cx| {
                InputState::new(window, cx)
                    .placeholder(placeholder)
                    .default_value(value)
            })
        };
        let repo = input(&entry.repo, "owner/name", window);
        let path = input(
            &entry.path,
            "the clone, or an empty folder to clone into",
            window,
        );
        let workdir = input(
            entry.workdir.as_deref().unwrap_or_default(),
            "optional: your project, where the agent starts",
            window,
        );
        let command = input(
            entry.command.as_deref().unwrap_or_default(),
            machines::DEFAULT_COMMAND,
            window,
        );
        let prompt = entry.prompt.clone().unwrap_or_default();
        let watch = [&repo, &path].map(|state| cx.observe(state, |_, _, cx| cx.notify()));
        Self {
            host: entry.host.clone(),
            repo,
            machine: entry.machine.clone(),
            path,
            workdir,
            command,
            prompt: cx.new(|cx| {
                TextareaState::new(window, cx)
                    .auto_grow(2, 6)
                    .placeholder("Default: review the pull request and add comments with the tool")
                    .default_value(prompt)
            }),
            allow_commands: entry.allow_commands,
            error: None,
            _watch: watch,
        }
    }

    fn dir_input(&self, field: DirField) -> &Entity<InputState> {
        match field {
            DirField::Repository => &self.path,
            DirField::Workdir => &self.workdir,
        }
    }

    /// The row as the config writes it.
    fn entry(&self, cx: &App) -> WorkspaceEntry {
        let text = |state: &Entity<InputState>| state.read(cx).value().trim().to_owned();
        let optional = |text: String| (!text.is_empty()).then_some(text);
        let prompt = self.prompt.read(cx).value().trim().to_owned();
        WorkspaceEntry {
            host: self.host.clone(),
            repo: text(&self.repo),
            machine: self.machine.clone(),
            path: text(&self.path),
            workdir: optional(text(&self.workdir)),
            command: optional(text(&self.command)),
            prompt: optional(prompt),
            allow_commands: self.allow_commands,
        }
    }
}

impl SettingsPanel {
    /// Follows the workspaces: rows built again when they load, and a row
    /// added when "Review with agent" asks for one.
    fn sync_workspaces(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(workspaces) = Workspaces::global(cx) else {
            return;
        };
        let (epoch, entries) = {
            let workspaces = workspaces.read(cx);
            (workspaces.epoch(), workspaces.entries().to_vec())
        };
        if epoch != self.workspaces_epoch {
            self.workspaces_epoch = epoch;
            self.browser = None;
            self.workspace_rows = entries
                .iter()
                .map(|entry| WorkspaceRow::new(entry, window, cx))
                .collect();
        }
        if let Some(entry) = workspaces.update(cx, |workspaces, _| workspaces.take_request()) {
            let row = WorkspaceRow::new(&entry, window, cx);
            let path = row.path.clone();
            self.workspace_rows.push(row);
            self.reveal_workspaces(window, cx);
            path.update(cx, |path, cx| path.focus(window, cx));
        }
        cx.notify();
    }

    /// Scrolls the Workspaces section into view.
    pub fn reveal_workspaces(&mut self, _: &mut Window, cx: &mut Context<Self>) {
        self.scroll.scroll_to_top_of_item(WORKSPACES_SECTION);
        cx.notify();
    }

    /// The workspaces as the rows hold them now, for tests.
    #[must_use]
    pub fn workspace_entries(&self, cx: &App) -> Vec<WorkspaceEntry> {
        self.workspace_rows
            .iter()
            .map(|row| row.entry(cx))
            .collect()
    }

    /// Each row's problem, for tests.
    #[must_use]
    pub fn workspace_errors(&self) -> Vec<Option<String>> {
        self.workspace_rows
            .iter()
            .map(|row| row.error.clone())
            .collect()
    }

    /// The line under row `ix`'s directory about cloning into it, for tests.
    #[must_use]
    pub fn clone_note(&self, ix: usize, cx: &App) -> Option<String> {
        self.workspace_rows
            .get(ix)
            .map(|row| Self::clone_line(&row.entry(cx), cx))
    }

    /// The open folder list's error line, for tests: `Some(None)` while it
    /// is open without one.
    #[must_use]
    pub fn browser_error(&self) -> Option<Option<String>> {
        self.browser.as_ref().map(|browser| browser.error.clone())
    }

    /// Shows `listing` in the open folder list as if the machine had sent
    /// it, for tests.
    pub fn show_listing(&mut self, listing: Listing, cx: &mut Context<Self>) {
        if let Some(browser) = &mut self.browser {
            browser.loading = None;
            browser.error = None;
            browser.listing = Some(listing);
            cx.notify();
        }
    }

    /// This PC and every listed host.
    fn machines(cx: &App) -> Vec<Machine> {
        Terminals::global(cx)
            .map(|terminals| terminals.read(cx).machines(cx))
            .unwrap_or_else(|| vec![Machine::this_pc(None)])
    }

    /// The machine a row names by its key.
    fn find_machine(key: &str, cx: &App) -> Option<Machine> {
        Self::machines(cx)
            .into_iter()
            .find(|machine| workspaces::machine_key(machine) == key)
    }

    /// The URL git clones `entry`'s repository from, when its forge is
    /// named: the config's, or the default one's in a fixtures run.
    fn remote_url(entry: &WorkspaceEntry, cx: &App) -> Option<String> {
        let host = entry.host.as_deref()?;
        let (owner, name) = entry.repo.trim().trim_matches('/').rsplit_once('/')?;
        if owner.is_empty() || name.is_empty() {
            return None;
        }
        let repo = RepoRef::new(host, owner, name);
        let session = Session::try_global(cx)?;
        Some(match session.read(cx).services() {
            Some(services) => services.config.remote_url(&repo),
            None => Config::default().remote_url(&repo),
        })
    }

    /// What "Review with agent" does with an empty directory: the terminal
    /// it opens and the command that terminal runs, when it is known.
    fn clone_line(entry: &WorkspaceEntry, cx: &App) -> String {
        let machine = Self::find_machine(&entry.machine, cx);
        let (name, over) = match &machine {
            Some(machine) if machine.is_local() => (machine.name.clone(), ""),
            Some(machine) => (machine.name.clone(), " over SSH"),
            None => (entry.machine.clone(), ""),
        };
        let command = machine
            .as_ref()
            .zip(Self::remote_url(entry, cx))
            .and_then(|(machine, url)| terminals::clone_command(machine, &url, &entry.path).ok())
            .unwrap_or_else(|| "git clone".to_owned());
        format!(
            "When the directory is empty, Review with agent opens a terminal on {name}{over} \
             that runs `{command}`; you can keep typing in it."
        )
    }

    /// The folder button of row `ix`'s `field`: the system's folder picker
    /// for this PC, the folder list for an SSH machine. A second press
    /// closes the list.
    pub fn browse(
        &mut self,
        ix: usize,
        field: DirField,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self
            .browser
            .as_ref()
            .is_some_and(|browser| browser.row == ix && browser.field == field)
        {
            self.browser = None;
            cx.notify();
            return;
        }
        let Some(row) = self.workspace_rows.get_mut(ix) else {
            return;
        };
        let input = row.dir_input(field).clone();
        let Some(machine) = Self::find_machine(&row.machine, cx) else {
            row.error = Some(format!("{} is not a machine listed here.", row.machine));
            cx.notify();
            return;
        };
        if machine.is_local() {
            self.browser = None;
            Self::pick_local(ix, input, window, cx);
        } else {
            let start = input.read(cx).value().trim().to_owned();
            self.browser = Some(DirBrowser {
                row: ix,
                field,
                machine,
                listing: None,
                loading: None,
                error: None,
                _task: None,
            });
            self.list_folder(start, cx);
        }
        cx.notify();
    }

    /// The system's folder picker; the folder chosen fills `input`.
    fn pick_local(
        ix: usize,
        input: Entity<InputState>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let paths = cx.prompt_for_paths(PathPromptOptions {
            files: false,
            directories: true,
            multiple: false,
            prompt: Some("Use this folder".into()),
        });
        cx.spawn_in(window, async move |this, cx| {
            let chosen = match paths.await {
                Ok(Ok(paths)) => paths.and_then(|paths| paths.into_iter().next()),
                Ok(Err(error)) => {
                    this.update(cx, |this, cx| {
                        if let Some(row) = this.workspace_rows.get_mut(ix) {
                            row.error = Some(format!("The folder picker failed: {error}"));
                            cx.notify();
                        }
                    })
                    .ok();
                    None
                }
                // Closed without an answer.
                Err(_) => None,
            };
            if let Some(path) = chosen {
                let path = path.to_string_lossy().into_owned();
                input
                    .update_in(cx, |input, window, cx| input.set_value(path, window, cx))
                    .ok();
            }
        })
        .detach();
    }

    /// Lists `dir` on the open folder list's machine, off the UI thread;
    /// an answer for a folder no longer asked for is dropped.
    fn list_folder(&mut self, dir: String, cx: &mut Context<Self>) {
        let Some(browser) = &mut self.browser else {
            return;
        };
        let machine = browser.machine.clone();
        browser.loading = Some(dir.clone());
        browser.error = None;
        let asked = dir.clone();
        browser._task = Some(cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move { dir_browser::list_remote(&machine, &dir) })
                .await;
            this.update(cx, |this, cx| {
                let Some(browser) = &mut this.browser else {
                    return;
                };
                if browser.loading.as_deref() != Some(asked.as_str()) {
                    return;
                }
                browser.loading = None;
                match result {
                    Ok(listing) => browser.listing = Some(listing),
                    Err(error) => browser.error = Some(error),
                }
                cx.notify();
            })
            .ok();
        }));
        cx.notify();
    }

    /// The folder list moves to `to(shown folder)`.
    fn browse_to(&mut self, to: impl FnOnce(&str) -> String, cx: &mut Context<Self>) {
        let Some(dir) = self
            .browser
            .as_ref()
            .and_then(|browser| browser.listing.as_ref())
            .map(|listing| to(&listing.dir))
        else {
            return;
        };
        self.list_folder(dir, cx);
    }

    /// "Use this folder": the folder shown fills the row's input.
    fn use_browsed(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(browser) = self.browser.take() else {
            return;
        };
        let (Some(listing), Some(row)) = (browser.listing, self.workspace_rows.get(browser.row))
        else {
            return;
        };
        let chosen = listing.chosen();
        row.dir_input(browser.field)
            .update(cx, |input, cx| input.set_value(chosen, window, cx));
        cx.notify();
    }

    /// The machine names a row may pick: this PC and every listed host.
    fn machine_choices(cx: &App) -> Vec<(String, SharedString)> {
        let machines = Self::machines(cx);
        machines
            .iter()
            .map(|machine| {
                (
                    workspaces::machine_key(machine),
                    SharedString::from(machine.name.clone()),
                )
            })
            .collect()
    }

    pub fn add_workspace(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let entry = WorkspaceEntry {
            machine: machines::LOCAL_HOST.to_owned(),
            ..WorkspaceEntry::default()
        };
        self.workspace_rows
            .push(WorkspaceRow::new(&entry, window, cx));
        cx.notify();
    }

    /// Checks every row and, when all are good, saves them.
    pub fn save_workspaces(&mut self, cx: &mut Context<Self>) {
        let names: Vec<String> = Self::machine_choices(cx)
            .into_iter()
            .map(|(name, _)| name)
            .collect();
        let entries: Vec<WorkspaceEntry> = self
            .workspace_rows
            .iter()
            .map(|row| row.entry(cx))
            .collect();
        let mut good = true;
        for (row, entry) in self.workspace_rows.iter_mut().zip(&entries) {
            row.error = workspaces::validate(entry, &names).err();
            good &= row.error.is_none();
        }
        if good && let Some(workspaces) = Workspaces::global(cx) {
            workspaces.update(cx, |workspaces, cx| workspaces.save(entries, cx));
        }
        cx.notify();
    }

    /// A directory input with its folder button on the left.
    fn dir_field(
        &self,
        t: &Theme,
        ix: usize,
        field: DirField,
        state: &Entity<InputState>,
        cx: &Context<Self>,
    ) -> gpui::Div {
        let side = px(ui::CONTROL_HEIGHT + 2.0);
        let button = ui::icon_button(
            cx,
            (field.id(), ix),
            IconName::FolderOpen,
            cx.listener(move |this, _, window, cx| this.browse(ix, field, window, cx)),
        )
        .w(side)
        .h(side)
        .debug_selector(move || format!("{}-browse-{ix}", field.id()));
        h_flex().gap(px(6.0)).items_center().child(button).child(
            div()
                .flex_1()
                .min_w_0()
                .debug_selector(move || format!("{}-input-{ix}", field.id()))
                .child(
                    Input::new(state)
                        .h(side)
                        .text_size(t.fonts.interface.size_px()),
                ),
        )
    }

    /// The folder list of an SSH machine, under the directory it fills.
    fn browser_view(&self, browser: &DirBrowser, t: &Theme, cx: &Context<Self>) -> AnyElement {
        let c = &t.colors;
        let hover = c.hover;
        let on = &browser.machine.name;
        let title = match (&browser.loading, &browser.listing) {
            (Some(dir), _) if dir.is_empty() => format!("Listing the home folder on {on}..."),
            (Some(dir), _) => format!("Listing {dir} on {on}..."),
            (None, Some(listing)) => format!("{} on {on}", listing.dir),
            (None, None) => format!("Folders on {on}"),
        };
        let entry = |id: ElementId, label: &str, selector: String| {
            h_flex()
                .id(id)
                .debug_selector(move || selector)
                .h(px(22.0))
                .flex_none()
                .px(px(6.0))
                .gap(px(6.0))
                .items_center()
                .rounded(t.radius.sm)
                .cursor_pointer()
                .text_color(c.text)
                .hover(move |s| s.bg(hover))
                .child(
                    Icon::new(IconName::Folder)
                        .size(px(14.0))
                        .text_color(c.text_muted),
                )
                .child(div().truncate().child(label.to_owned()))
        };
        let mut list = v_flex().gap(px(1.0));
        if let Some(listing) = &browser.listing {
            if listing.dir != "/" {
                list = list.child(
                    entry(
                        "workspace-browser-up".into(),
                        "..",
                        "workspace-browser-up".to_owned(),
                    )
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.browse_to(dir_browser::parent, cx);
                    })),
                );
            }
            for (i, name) in listing.folders.iter().enumerate() {
                let into = name.clone();
                list = list.child(
                    entry(
                        ("workspace-browser-folder", i).into(),
                        name,
                        format!("workspace-browser-folder-{i}"),
                    )
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.browse_to(|dir| dir_browser::child(dir, &into), cx);
                    })),
                );
            }
            if listing.folders.is_empty() {
                list = list.child(
                    div()
                        .px(px(6.0))
                        .text_color(c.text_muted)
                        .child("No folders here"),
                );
            }
        }
        let ready = browser.loading.is_none() && browser.listing.is_some();
        v_flex()
            .debug_selector(|| "workspace-browser".into())
            .gap(px(6.0))
            .p(px(8.0))
            .rounded(t.radius.md)
            .border_1()
            .border_color(c.border)
            .bg(c.field)
            .small(t)
            .child(
                h_flex()
                    .gap(px(8.0))
                    .items_center()
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .text_color(c.text_secondary)
                            .child(title),
                    )
                    .child(
                        Button::new(
                            "workspace-browser-use",
                            "Use this folder",
                            ButtonKind::Secondary,
                        )
                        .disabled(!ready)
                        .on_click(cx.listener(|this, _, window, cx| this.use_browsed(window, cx))),
                    )
                    .child(
                        Button::new("workspace-browser-cancel", "Cancel", ButtonKind::Ghost)
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.browser = None;
                                cx.notify();
                            })),
                    ),
            )
            .children(browser.error.clone().map(|error| {
                div()
                    .debug_selector(|| "workspace-browser-error".into())
                    .tiny(t)
                    .text_color(c.danger)
                    .child(error)
            }))
            .child(
                div()
                    .id("workspace-browser-list")
                    .max_h(px(200.0))
                    .overflow_y_scroll()
                    .child(list),
            )
            .into_any_element()
    }

    fn workspace_field(t: &Theme, label: &'static str, field: impl IntoElement) -> gpui::Div {
        h_flex()
            .gap(px(12.0))
            .items_center()
            .child(
                div()
                    .w(px(LABEL_WIDTH))
                    .flex_none()
                    .small(t)
                    .text_color(t.colors.text_secondary)
                    .child(label),
            )
            .child(div().flex_1().min_w_0().child(field))
    }

    fn workspaces_section(&self, t: &Theme, cx: &mut Context<Self>) -> AnyElement {
        let c = &t.colors;
        let choices = Self::machine_choices(cx);
        let input = |state: &Entity<InputState>| {
            Input::new(state)
                .h(px(ui::CONTROL_HEIGHT + 2.0))
                .text_size(t.fonts.interface.size_px())
        };
        let panel = cx.weak_entity();
        let rows = self.workspace_rows.iter().enumerate().map(|(ix, row)| {
            let mut options = choices.clone();
            if !options.iter().any(|(name, _)| *name == row.machine) {
                options.push((
                    row.machine.clone(),
                    format!("{} (not found)", row.machine).into(),
                ));
            }
            let chosen = row.machine.clone();
            let pick = panel.clone();
            let machine = ui::segmented(
                cx,
                "workspace-machine",
                options,
                move |name| *name == chosen,
                move |name, _, cx| {
                    let name = name.clone();
                    pick.update(cx, |this, cx| {
                        if let Some(row) = this.workspace_rows.get_mut(ix) {
                            row.machine = name;
                            if this.browser.as_ref().is_some_and(|b| b.row == ix) {
                                this.browser = None;
                            }
                            cx.notify();
                        }
                    })
                    .ok();
                },
            );
            let open = self.browser.as_ref().filter(|browser| browser.row == ix);
            let browser = |field: DirField| {
                open.filter(|browser| browser.field == field)
                    .map(|browser| Self::workspace_field(t, "", self.browser_view(browser, t, cx)))
            };
            let host = row
                .host
                .as_deref()
                .map(|host| format!("on {host}"))
                .unwrap_or_else(|| "on any forge".to_owned());
            let prompt = div()
                .rounded(t.radius.md)
                .border_1()
                .border_color(c.control_border)
                .bg(c.field)
                .overflow_hidden()
                .child(
                    Textarea::new(&row.prompt)
                        .appearance(false)
                        .bordered(false)
                        .text_size(t.fonts.interface.size_px())
                        .line_height(t.fonts.interface.line_height_px()),
                );
            v_flex()
                .debug_selector(move || format!("workspace-row-{ix}"))
                .gap(px(6.0))
                .p(px(10.0))
                .rounded(t.radius.md)
                .border_1()
                .border_color(if row.error.is_some() {
                    c.danger
                } else {
                    c.border
                })
                .bg(c.surface_raised)
                .child(
                    h_flex()
                        .gap(px(8.0))
                        .items_center()
                        .child(div().w(px(260.0)).child(input(&row.repo)))
                        .child(div().tiny(t).text_color(c.text_muted).child(host))
                        .child(div().flex_1())
                        .child(
                            Button::new(("workspace-remove", ix), "Remove", ButtonKind::Ghost)
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    if ix < this.workspace_rows.len() {
                                        this.workspace_rows.remove(ix);
                                        this.browser = None;
                                        cx.notify();
                                    }
                                })),
                        ),
                )
                .child(Self::workspace_field(t, "Machine", machine))
                .child(Self::workspace_field(
                    t,
                    "Directory",
                    self.dir_field(t, ix, DirField::Repository, &row.path, cx),
                ))
                .children(browser(DirField::Repository))
                .child(Self::workspace_field(
                    t,
                    "",
                    div()
                        .debug_selector(move || format!("workspace-clone-note-{ix}"))
                        .tiny(t)
                        .text_color(c.text_muted)
                        .child(Self::clone_line(&row.entry(cx), cx)),
                ))
                .child(Self::workspace_field(
                    t,
                    "Working dir",
                    self.dir_field(t, ix, DirField::Workdir, &row.workdir, cx),
                ))
                .children(browser(DirField::Workdir))
                .child(Self::workspace_field(t, "Command", input(&row.command)))
                .child(Self::workspace_field(t, "Prompt", prompt))
                .child(Self::workspace_field(
                    t,
                    "",
                    v_flex()
                        .gap(px(2.0))
                        .child(
                            ui::checkbox(("workspace-allow-commands", ix))
                                .label("Let the agent run commands and edit files")
                                .checked(row.allow_commands)
                                .on_click(cx.listener(move |this, checked: &bool, _, cx| {
                                    if let Some(row) = this.workspace_rows.get_mut(ix) {
                                        row.allow_commands = *checked;
                                        cx.notify();
                                    }
                                })),
                        )
                        .child(div().tiny(t).text_color(c.warn).child(
                            "This gives the agent your full file and network access while the \
                             pull request's text, which can ask it for anything, is in its \
                             context.",
                        )),
                ))
                .children(row.error.clone().map(|error| {
                    div()
                        .debug_selector(move || format!("workspace-error-{ix}"))
                        .tiny(t)
                        .text_color(c.danger)
                        .child(error)
                }))
                .into_any_element()
        });
        let state = Workspaces::global(cx).map(|workspaces| {
            let workspaces = workspaces.read(cx);
            (workspaces.state().clone(), workspaces.saves_to_file())
        });
        let status = match state {
            Some((SaveState::Saving, _)) => "Saving...".to_owned(),
            Some((SaveState::Saved, true)) => "Saved to config.toml".to_owned(),
            Some((SaveState::Saved, false)) => "Kept for this run".to_owned(),
            Some((SaveState::Failed(error), _)) => format!("Not saved: {error}"),
            Some((SaveState::Idle, _)) | None => String::new(),
        };
        let empty = self.workspace_rows.is_empty();
        self.section(
            cx,
            "Workspaces",
            "Where Review with agent runs, per repository; saved in config.toml",
        )
        .debug_selector(|| "workspaces-section".into())
        .child(div().tiny(t).text_color(c.text_muted).child(
            "The prompt may use {number}, {owner}, {name}, {repo}, {host}, {url}, \
                 {context} and {worktree}; nothing the pull request's author wrote.",
        ))
        .when(empty, |el| {
            el.child(
                div()
                    .small(t)
                    .text_color(c.text_secondary)
                    .child("No workspaces yet."),
            )
        })
        .children(rows)
        .child(
            h_flex()
                .gap(px(8.0))
                .items_center()
                .child(
                    Button::new("workspace-add", "Add workspace", ButtonKind::Secondary).on_click(
                        cx.listener(|this, _, window, cx| this.add_workspace(window, cx)),
                    ),
                )
                .child(
                    Button::new("workspace-save", "Save workspaces", ButtonKind::Primary)
                        .on_click(cx.listener(|this, _, _, cx| this.save_workspaces(cx))),
                )
                .child(
                    div()
                        .debug_selector(|| "workspace-status".into())
                        .tiny(t)
                        .text_color(c.text_muted)
                        .child(status),
                ),
        )
        .into_any_element()
    }
}

/// `name` drawn in its own family, with the interface font for glyphs it
/// lacks. Every listed name is one the platform listed or the app bundles,
/// so the family itself loads.
fn own_face(name: &SharedString, interface: &SharedString) -> gpui::Font {
    let mut face = gpui::font(name.clone());
    face.fallbacks = Some(FontFallbacks::from_fonts(vec![interface.to_string()]));
    face
}

/// `--demo-font-picker`: the interface font's list open over the form.
pub fn demo_font_picker(window: &mut Window, cx: &mut App) {
    if let Some(panel) = SettingsPanel::existing(cx) {
        panel.update(cx, |panel, cx| panel.open_interface_picker(window, cx));
    }
}

impl Panel for SettingsPanel {
    fn title(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let close = crate::panels::close_button(cx.entity_id(), "close-Settings".into(), cx);
        h_flex()
            .cursor_pointer()
            .gap(px(6.0))
            .items_center()
            .child(crate::panels::tab_label(div()).child(SETTINGS))
            .child(close)
    }

    fn inner_padding(&self, _: &App) -> bool {
        false
    }
}

impl Render for SettingsPanel {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let t = ui::theme(cx);
        let c = &t.colors;
        let state = cx.global::<Appearance>();
        let settings = state.settings.clone();
        let saved = match state.file() {
            Some(file) if file.writable() => file.path().display().to_string(),
            Some(_) => "settings.toml has errors: changes are not saved".to_owned(),
            None => "changes are not saved in this run".to_owned(),
        };
        let theme = self.theme_section(&t, &settings, cx);
        let images = self.images_section(&settings, cx);
        let notifications = self.notifications_section(&settings, cx);
        let diff = self.diff_section(&settings, cx);
        let inbox = self.inbox_section(&settings, cx);
        let interface = self.font_section(FontRole::Interface, &t, cx);
        let buffer = self.font_section(FontRole::Buffer, &t, cx);
        let workspaces = self.workspaces_section(&t, cx);
        v_flex()
            .id("settings-panel")
            .track_focus(&self.focus)
            .size_full()
            .bg(c.surface)
            .child(header(
                cx,
                div().child("Changes apply as you make them"),
                div().truncate().child(saved),
            ))
            .child(
                // The sections are the scrolled element's own children, so
                // the Workspaces one can be scrolled to.
                v_flex()
                    .id("settings-scroll")
                    .debug_selector(|| "settings-scroll".into())
                    .track_scroll(&self.scroll)
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .gap(px(22.0))
                    .p(px(16.0))
                    .child(theme)
                    .child(images)
                    .child(notifications)
                    .child(interface)
                    .child(buffer)
                    .child(
                        h_flex().child(
                            Button::new(
                                "reset-fonts",
                                "Reset to theme defaults",
                                ButtonKind::Secondary,
                            )
                            .on_click(|_, _, cx| {
                                appearance::update(cx, Settings::reset_fonts);
                            }),
                        ),
                    )
                    .child(diff)
                    .child(inbox)
                    .child(workspaces),
            )
    }
}
