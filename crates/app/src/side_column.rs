//! The Conversation's side column, as on a forge's pull request page:
//! reviewers with their state, assignees, labels in their forge colors and
//! the milestone, with pickers that request and remove reviewers, assign
//! people and toggle labels. Writes go through the session (see
//! [`crate::people`]) and patch the pull request in place; a failure shows
//! under its section.
//!
//! The people picker searches the repository's collaborators as the user
//! types, after a short pause; the label picker lists the repository's
//! labels and filters them here.

use std::time::Duration;

use forge::Capabilities;
use forge::model::{AccountKind, ForgeKind, Label, PullRequest, ReviewState, User};
use gpui::{
    AnyElement, App, AppContext as _, Context, Entity, FontWeight, InteractiveElement as _,
    IntoElement, ParentElement as _, Render, SharedString, StatefulInteractiveElement as _, Styled,
    Subscription, Task, Window, anchored, deferred, div, prelude::FluentBuilder as _, px,
};
use gpui_kit::assets::IconName;
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::{Icon, h_flex, v_flex};
use theme::Theme;

use crate::lifecycle::Load;
use crate::model::PrKey;
use crate::people::{Field, PeopleState, PeopleWrite, matches, review_request_blocked};
use crate::session::Session;
use crate::ui::{self, TextExt as _};

/// How long typing must pause before the collaborators are searched.
pub const SEARCH_DEBOUNCE: Duration = Duration::from_millis(250);
/// The side column's width when it sits beside the conversation.
pub const WIDTH: f32 = 248.0;

/// A picker the column can start with open, for screenshots.
#[derive(Clone, Copy, Default)]
pub struct DemoPicker(pub Option<Field>);

impl gpui::Global for DemoPicker {}

impl Field {
    /// `reviewers`, `assignees` or `labels`, as `--demo-picker` names them.
    #[must_use]
    pub fn from_name(name: &str) -> Option<Self> {
        match name {
            "reviewers" => Some(Self::Reviewers),
            "assignees" => Some(Self::Assignees),
            "labels" => Some(Self::Labels),
            _ => None,
        }
    }

    fn id(self) -> &'static str {
        match self {
            Self::Reviewers => "reviewers",
            Self::Assignees => "assignees",
            Self::Labels => "labels",
        }
    }

    fn title(self) -> &'static str {
        match self {
            Self::Reviewers => "Reviewers",
            Self::Assignees => "Assignees",
            Self::Labels => "Labels",
        }
    }
}

/// What a reviewer's state reads as, and its color.
fn review_style(t: &Theme, state: ReviewState) -> (&'static str, IconName, gpui::Hsla) {
    let c = &t.colors;
    match state {
        ReviewState::Requested | ReviewState::Pending => {
            ("requested", IconName::CircleDashed, c.warn)
        }
        ReviewState::Approved => ("approved", IconName::CircleCheck, c.ok),
        ReviewState::ChangesRequested => ("changes requested", IconName::FileDiff, c.danger),
        ReviewState::Commented => ("commented", IconName::MessageSquare, c.info),
        ReviewState::Dismissed => ("dismissed", IconName::Ban, c.text_muted),
    }
}

pub struct SideColumn {
    session: Entity<Session>,
    pr: Option<PrKey>,
    picker: Option<Field>,
    search: Entity<InputState>,
    /// The query `people` answers.
    query: String,
    people: Load<Vec<User>>,
    labels: Load<Vec<Label>>,
    search_task: Option<Task<()>>,
    labels_task: Option<Task<()>>,
    _subscriptions: Vec<Subscription>,
}

impl SideColumn {
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let session = Session::global(cx);
        let search = cx.new(|cx| InputState::new(window, cx).placeholder("Search"));
        let typed = cx.subscribe(&search, |this: &mut Self, state, event: &InputEvent, cx| {
            if matches!(event, InputEvent::Change) {
                let query = state.read(cx).value().to_string();
                this.search_changed(query, cx);
            }
        });
        let observed = cx.observe(&session, |_: &mut Self, _, cx| cx.notify());
        let picker = cx.try_global::<DemoPicker>().and_then(|demo| demo.0);
        Self {
            session,
            pr: None,
            picker,
            search,
            query: String::new(),
            people: Load::Idle,
            labels: Load::Idle,
            search_task: None,
            labels_task: None,
            _subscriptions: vec![typed, observed],
        }
    }

    /// Shows `pr` from now on; an open picker closes when it changes.
    pub fn bind(&mut self, pr: &PrKey, cx: &mut Context<Self>) {
        if self.pr.as_ref() == Some(pr) {
            return;
        }
        let first = self.pr.is_none();
        self.pr = Some(pr.clone());
        self.people = Load::Idle;
        self.labels = Load::Idle;
        self.query.clear();
        if !first {
            self.picker = None;
        }
        if let Some(field) = self.picker {
            self.load(field, cx);
        }
    }

    /// The open picker, for tests.
    pub fn picker(&self) -> Option<Field> {
        self.picker
    }

    /// The people the picker lists now.
    pub fn people(&self) -> &Load<Vec<User>> {
        &self.people
    }

    /// The repository's labels, once read.
    pub fn labels(&self) -> &Load<Vec<Label>> {
        &self.labels
    }

    /// Opens `field`'s picker, or closes it when it is the open one.
    pub fn toggle_picker(&mut self, field: Field, window: &mut Window, cx: &mut Context<Self>) {
        if self.picker == Some(field) {
            self.picker = None;
            cx.notify();
            return;
        }
        self.picker = Some(field);
        self.search
            .update(cx, |search, cx| search.set_value("", window, cx));
        self.query.clear();
        self.search
            .update(cx, |search, cx| search.focus(window, cx));
        self.load(field, cx);
        cx.notify();
    }

    fn load(&mut self, field: Field, cx: &mut Context<Self>) {
        match field {
            Field::Labels => self.load_labels(cx),
            Field::Reviewers | Field::Assignees => self.search_now(String::new(), cx),
        }
    }

    fn search_changed(&mut self, query: String, cx: &mut Context<Self>) {
        if self.picker == Some(Field::Labels) {
            self.query = query;
            cx.notify();
            return;
        }
        // Each keystroke restarts the wait; the last one searches.
        self.search_task = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(SEARCH_DEBOUNCE).await;
            this.update(cx, |this, cx| this.search_now(query, cx)).ok();
        }));
    }

    /// Asks for the collaborators matching `query`: the forge's search, or
    /// the fixture people.
    fn search_now(&mut self, query: String, cx: &mut Context<Self>) {
        let Some(key) = self.pr.clone() else {
            return;
        };
        self.query.clone_from(&query);
        let services = self.session.read(cx).services().cloned();
        let Some(services) = services else {
            let found = crate::fixtures::collaborators()
                .into_iter()
                .filter(|user| matches(user, &query))
                .collect();
            self.people = Load::Ready(found);
            cx.notify();
            return;
        };
        self.people = Load::Loading;
        cx.notify();
        let repo = key.repo.clone();
        let asked = query.clone();
        let call = services.forge(&key.repo.host, move |forge| async move {
            forge.collaborators(&repo, &asked).await
        });
        self.search_task = Some(cx.spawn(async move |this, cx| {
            let result = call.await;
            this.update(cx, |this, cx| {
                // An answer to an older query is dropped.
                if this.query == query && this.pr.as_ref() == Some(&key) {
                    this.people = loaded(result);
                    cx.notify();
                }
            })
            .ok();
        }));
    }

    fn load_labels(&mut self, cx: &mut Context<Self>) {
        let Some(key) = self.pr.clone() else {
            return;
        };
        if matches!(self.labels, Load::Ready(_) | Load::Loading) {
            return;
        }
        let services = self.session.read(cx).services().cloned();
        let Some(services) = services else {
            self.labels = Load::Ready(crate::fixtures::labels());
            cx.notify();
            return;
        };
        self.labels = Load::Loading;
        let repo = key.repo.clone();
        let call = services.forge(&key.repo.host, move |forge| async move {
            forge.labels(&repo).await
        });
        self.labels_task = Some(cx.spawn(async move |this, cx| {
            let result = call.await;
            this.update(cx, |this, cx| {
                if this.pr.as_ref() == Some(&key) {
                    this.labels = loaded(result);
                    cx.notify();
                }
            })
            .ok();
        }));
    }

    fn write(&self, write: PeopleWrite, cx: &mut App) {
        let Some(key) = self.pr.clone() else {
            return;
        };
        self.session
            .update(cx, |session, cx| session.people_write(&key, write, cx));
    }
}

fn loaded<T>(result: forge::Result<T>) -> Load<T> {
    match result {
        Ok(value) => Load::Ready(value),
        Err(forge::Error::Unsupported { .. }) => Load::Unsupported,
        Err(error) => Load::Failed(error.to_string().into()),
    }
}

/// What a render needs from the session.
struct Facts {
    pr: PullRequest,
    forge: ForgeKind,
    caps: Capabilities,
    state: PeopleState,
}

impl SideColumn {
    fn facts(&self, cx: &App) -> Option<Facts> {
        let key = self.pr.as_ref()?;
        let session = self.session.read(cx);
        let open = session.pr(key)?;
        Some(Facts {
            pr: open.forge_pr()?.clone(),
            forge: open.pr.forge,
            caps: session.capabilities_for(key)?,
            state: open.people.clone(),
        })
    }

    /// A section: its title with the picker button, its body, and its error.
    fn section(
        &self,
        field: Field,
        body: AnyElement,
        facts: &Facts,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let t = ui::theme(cx);
        let c = &t.colors;
        let hover = c.hover;
        let open = self.picker == Some(field);
        let busy = facts.state.busy == Some(field);
        let error = facts
            .state
            .error
            .as_ref()
            .filter(|(at, _)| *at == field)
            .map(|(_, text)| text.clone());
        let id = field.id();
        let picker = open.then(|| self.picker_popover(field, facts, cx));
        v_flex()
            .debug_selector(move || format!("side-{id}"))
            .gap(px(6.0))
            .pb(px(12.0))
            .border_b_1()
            .border_color(c.border)
            .child(
                h_flex()
                    .relative()
                    .items_center()
                    .gap(px(6.0))
                    .child(
                        div()
                            .flex_1()
                            .small(&t)
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_color(c.text_secondary)
                            .child(field.title()),
                    )
                    .when(busy, |el| {
                        el.child(div().tiny(&t).text_color(c.text_muted).child("Saving"))
                    })
                    .child(
                        div()
                            .id(SharedString::from(format!("picker-{id}")))
                            .debug_selector(move || format!("picker-{id}"))
                            .size(px(20.0))
                            .flex()
                            .items_center()
                            .justify_center()
                            .rounded(t.radius.sm)
                            .cursor_pointer()
                            .when(open, |el| el.bg(c.selected))
                            .hover(move |s| s.bg(hover))
                            .on_click(cx.listener(move |this, _, window, cx| {
                                this.toggle_picker(field, window, cx);
                            }))
                            .child(
                                Icon::new(IconName::Settings)
                                    .size(px(13.0))
                                    .text_color(c.text_muted),
                            ),
                    )
                    .children(picker),
            )
            .child(body)
            .children(error.map(|error| {
                div()
                    .debug_selector(move || format!("side-error-{id}"))
                    .tiny(&t)
                    .text_color(c.danger)
                    .child(error)
            }))
            .into_any_element()
    }

    fn reviewers(&self, facts: &Facts, cx: &mut Context<Self>) -> AnyElement {
        let t = ui::theme(cx);
        let c = &t.colors;
        let host = facts.pr.repo.host.clone();
        if facts.pr.reviewers.is_empty() {
            return div()
                .small(&t)
                .text_color(c.text_muted)
                .child("No reviewers")
                .into_any_element();
        }
        v_flex()
            .gap(px(4.0))
            .children(facts.pr.reviewers.iter().map(|reviewer| {
                let (words, icon, color) = review_style(&t, reviewer.state);
                let user = reviewer.user.clone();
                let login = user.login.clone();
                let selector = format!("reviewer-{login}");
                let avatar = if reviewer.is_team() {
                    Icon::new(IconName::Users)
                        .size(px(ui::avatar::ROW))
                        .text_color(c.text_muted)
                        .into_any_element()
                } else {
                    ui::avatar(cx, &host, &reviewer.user, ui::avatar::ROW)
                };
                let profile = crate::references::user_url(&host, &login);
                h_flex()
                    .debug_selector(move || selector)
                    .gap(px(6.0))
                    .items_center()
                    .small(&t)
                    .child(avatar)
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .text_color(c.text)
                            .child(ui::web_link(
                                SharedString::from(format!("reviewer-link-{login}")),
                                login.clone(),
                                profile,
                                cx,
                            )),
                    )
                    .child(
                        h_flex()
                            .flex_none()
                            .gap(px(3.0))
                            .items_center()
                            .text_color(color)
                            .tiny(&t)
                            .child(Icon::new(icon).size(px(12.0)).text_color(color))
                            .child(words),
                    )
                    .child(
                        ui::icon_button(
                            cx,
                            SharedString::from(format!("remove-reviewer-{login}")),
                            IconName::Close,
                            cx.listener(move |this, _, _, cx| {
                                this.write(PeopleWrite::RemoveReviewer(user.clone()), cx);
                            }),
                        )
                        .size(px(18.0))
                        .debug_selector({
                            let login = login.clone();
                            move || format!("remove-reviewer-{login}")
                        }),
                    )
                    .into_any_element()
            }))
            .into_any_element()
    }

    fn assignees(&self, facts: &Facts, cx: &mut Context<Self>) -> AnyElement {
        let t = ui::theme(cx);
        let c = &t.colors;
        let host = facts.pr.repo.host.clone();
        if facts.pr.assignees.is_empty() {
            return div()
                .small(&t)
                .text_color(c.text_muted)
                .child("No one assigned")
                .into_any_element();
        }
        v_flex()
            .gap(px(4.0))
            .children(facts.pr.assignees.iter().map(|user| {
                let login = user.login.clone();
                let selector = format!("assignee-{login}");
                h_flex()
                    .debug_selector(move || selector)
                    .gap(px(6.0))
                    .items_center()
                    .small(&t)
                    .text_color(c.text)
                    .child(ui::avatar(cx, &host, user, ui::avatar::ROW))
                    .child(ui::web_link(
                        SharedString::from(format!("assignee-link-{login}")),
                        login.clone(),
                        crate::references::user_url(&host, &login),
                        cx,
                    ))
                    .into_any_element()
            }))
            .into_any_element()
    }

    fn labels_body(&self, facts: &Facts, cx: &mut Context<Self>) -> AnyElement {
        let t = ui::theme(cx);
        if facts.pr.labels.is_empty() {
            return div()
                .small(&t)
                .text_color(t.colors.text_muted)
                .child("None yet")
                .into_any_element();
        }
        h_flex()
            .flex_wrap()
            .gap(px(4.0))
            .children(facts.pr.labels.iter().map(|label| {
                let name = label.name.clone();
                ui::label_chip(cx, label.name.clone(), label.color.as_deref())
                    .debug_selector(move || format!("label-{name}"))
            }))
            .into_any_element()
    }

    /// The open picker, anchored under its section's title.
    fn picker_popover(&self, field: Field, facts: &Facts, cx: &mut Context<Self>) -> AnyElement {
        let t = ui::theme(cx);
        let c = &t.colors;
        let rows = match field {
            Field::Labels => self.label_rows(facts, cx),
            Field::Reviewers | Field::Assignees => self.people_rows(field, facts, cx),
        };
        deferred(
            anchored().child(
                v_flex()
                    .id("side-picker")
                    .debug_selector(|| "side-picker".into())
                    .occlude()
                    .mt(px(24.0))
                    .w(px(280.0))
                    .max_h(px(360.0))
                    .p(px(6.0))
                    .gap(px(4.0))
                    .rounded(t.radius.lg)
                    .border_1()
                    .border_color(c.border_strong)
                    .bg(c.surface_menu)
                    .small(&t)
                    .on_mouse_down_out(cx.listener(|this, _, _, cx| {
                        this.picker = None;
                        cx.notify();
                    }))
                    .child(Input::new(&self.search))
                    .child(
                        v_flex()
                            .id("side-picker-list")
                            .flex_1()
                            .min_h_0()
                            .overflow_y_scroll()
                            .children(rows),
                    ),
            ),
        )
        .with_priority(1)
        .into_any_element()
    }

    fn note(t: &Theme, text: impl Into<SharedString>) -> AnyElement {
        div()
            .px(px(6.0))
            .py(px(4.0))
            .text_color(t.colors.text_muted)
            .child(text.into())
            .into_any_element()
    }

    /// One pickable row: a check when chosen, the reason when it cannot be
    /// picked.
    #[expect(clippy::too_many_arguments, reason = "one row's parts")]
    fn pick_row(
        id: String,
        lead: AnyElement,
        label: SharedString,
        detail: Option<SharedString>,
        chosen: bool,
        blocked: Option<String>,
        on_pick: impl Fn(&mut Self, &mut Context<Self>) + 'static,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let t = ui::theme(cx);
        let c = &t.colors;
        let hover = c.hover;
        let disabled = blocked.is_some();
        let selector = id.clone();
        v_flex()
            .id(SharedString::from(id))
            .debug_selector(move || selector)
            .px(px(6.0))
            .py(px(4.0))
            .rounded(t.radius.sm)
            .when(!disabled, |el| {
                el.cursor_pointer()
                    .hover(move |s| s.bg(hover))
                    .on_click(cx.listener(move |this, _, _, cx| on_pick(this, cx)))
            })
            .child(
                h_flex()
                    .gap(px(6.0))
                    .items_center()
                    .text_color(if disabled { c.text_faint } else { c.text })
                    .child(div().size(px(14.0)).flex_none().when(chosen, |el| {
                        el.child(
                            Icon::new(IconName::Check)
                                .size(px(14.0))
                                .text_color(c.accent_fg),
                        )
                    }))
                    .child(lead)
                    .child(div().flex_none().child(label))
                    .children(detail.map(|detail| {
                        div()
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .text_color(c.text_muted)
                            .child(detail)
                    })),
            )
            .children(blocked.map(|why| div().pl(px(20.0)).tiny(&t).text_color(c.warn).child(why)))
            .into_any_element()
    }

    fn people_rows(&self, field: Field, facts: &Facts, cx: &mut Context<Self>) -> Vec<AnyElement> {
        let t = ui::theme(cx);
        let users = match &self.people {
            Load::Ready(users) => users.clone(),
            Load::Idle | Load::Loading => return vec![Self::note(&t, "Searching")],
            Load::Failed(error) => return vec![Self::note(&t, format!("Not read: {error}"))],
            Load::Unsupported => {
                return vec![Self::note(
                    &t,
                    format!("{} cannot list people here", facts.forge.label()),
                )];
            }
        };
        let host = facts.pr.repo.host.clone();
        let chosen = |user: &User| match field {
            Field::Reviewers => facts
                .pr
                .reviewers
                .iter()
                .any(|r| r.user.login.eq_ignore_ascii_case(&user.login)),
            _ => facts
                .pr
                .assignees
                .iter()
                .any(|a| a.login.eq_ignore_ascii_case(&user.login)),
        };
        let mut rows: Vec<AnyElement> = Vec::new();
        // A team is named in full; only forges with team requests take one.
        if field == Field::Reviewers && self.query.contains('/') {
            let team = User {
                kind: AccountKind::Team,
                ..User::new(self.query.trim())
            };
            let blocked =
                review_request_blocked(&team, &facts.pr.author.login, &facts.caps, facts.forge);
            let ask = team.clone();
            rows.push(Self::pick_row(
                "pick-team".to_owned(),
                Icon::new(IconName::Users)
                    .size(px(ui::avatar::ROW))
                    .into_any_element(),
                format!("Team {}", team.login).into(),
                None,
                false,
                blocked,
                move |this, cx| this.write(PeopleWrite::RequestReviewer(ask.clone()), cx),
                cx,
            ));
        }
        if users.is_empty() {
            rows.push(Self::note(&t, "No one matches"));
        }
        for user in users {
            let on = chosen(&user);
            let blocked = (field == Field::Reviewers && !on)
                .then(|| {
                    review_request_blocked(&user, &facts.pr.author.login, &facts.caps, facts.forge)
                })
                .flatten();
            let lead = ui::avatar(cx, &host, &user, ui::avatar::ROW);
            let login: SharedString = user.login.clone().into();
            let detail = user.name.clone().map(SharedString::from);
            let current = facts.pr.assignees.clone();
            let picked = user.clone();
            let on_pick = move |this: &mut Self, cx: &mut Context<Self>| {
                let write = match (field, on) {
                    (Field::Reviewers, true) => PeopleWrite::RemoveReviewer(picked.clone()),
                    (Field::Reviewers, false) => PeopleWrite::RequestReviewer(picked.clone()),
                    (_, true) => PeopleWrite::SetAssignees(
                        current
                            .iter()
                            .filter(|a| !a.login.eq_ignore_ascii_case(&picked.login))
                            .cloned()
                            .collect(),
                    ),
                    (_, false) => PeopleWrite::SetAssignees(
                        current.iter().cloned().chain([picked.clone()]).collect(),
                    ),
                };
                this.write(write, cx);
            };
            rows.push(Self::pick_row(
                format!("pick-{}-{}", field.id(), user.login),
                lead,
                login,
                detail,
                on,
                blocked,
                on_pick,
                cx,
            ));
        }
        rows
    }

    fn label_rows(&self, facts: &Facts, cx: &mut Context<Self>) -> Vec<AnyElement> {
        let t = ui::theme(cx);
        let labels = match &self.labels {
            Load::Ready(labels) => labels.clone(),
            Load::Idle | Load::Loading => return vec![Self::note(&t, "Loading labels")],
            Load::Failed(error) => return vec![Self::note(&t, format!("Not read: {error}"))],
            Load::Unsupported => {
                return vec![Self::note(
                    &t,
                    format!("{} cannot list labels here", facts.forge.label()),
                )];
            }
        };
        let query = self.query.trim().to_lowercase();
        let current: Vec<String> = facts.pr.labels.iter().map(|l| l.name.clone()).collect();
        let shown: Vec<Label> = labels
            .into_iter()
            .filter(|label| query.is_empty() || label.name.to_lowercase().contains(&query))
            .collect();
        if shown.is_empty() {
            return vec![Self::note(&t, "No label matches")];
        }
        shown
            .into_iter()
            .map(|label| {
                let on = current
                    .iter()
                    .any(|name| name.eq_ignore_ascii_case(&label.name));
                let colors = ui::label::label_colors(
                    &t,
                    ui::label::forge_color(label.color.as_deref()),
                    t.colors.surface_menu,
                );
                let dot = div()
                    .size(px(10.0))
                    .flex_none()
                    .rounded_full()
                    .bg(colors.text)
                    .into_any_element();
                let names = current.clone();
                let name = label.name.clone();
                let on_pick = move |this: &mut Self, cx: &mut Context<Self>| {
                    let next = if on {
                        names
                            .iter()
                            .filter(|n| !n.eq_ignore_ascii_case(&name))
                            .cloned()
                            .collect()
                    } else {
                        names.iter().cloned().chain([name.clone()]).collect()
                    };
                    this.write(PeopleWrite::SetLabels(next), cx);
                };
                Self::pick_row(
                    format!("pick-label-{}", label.name),
                    dot,
                    label.name.clone().into(),
                    label.description.clone().map(SharedString::from),
                    on,
                    None,
                    on_pick,
                    cx,
                )
            })
            .collect()
    }
}

impl Render for SideColumn {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let Some(facts) = self.facts(cx) else {
            return div().into_any_element();
        };
        let t = ui::theme(cx);
        let c = &t.colors;
        let reviewers = self.reviewers(&facts, cx);
        let assignees = self.assignees(&facts, cx);
        let labels = self.labels_body(&facts, cx);
        let milestone = facts.pr.milestone.as_ref().map_or_else(
            || {
                div()
                    .text_color(c.text_muted)
                    .child("No milestone")
                    .into_any_element()
            },
            |milestone| {
                h_flex()
                    .gap(px(6.0))
                    .items_center()
                    .text_color(c.text)
                    .child(
                        Icon::new(IconName::Flag)
                            .size(px(13.0))
                            .text_color(c.text_muted),
                    )
                    .child(milestone.title.clone())
                    .into_any_element()
            },
        );
        v_flex()
            .debug_selector(|| "people-column".into())
            .w_full()
            .gap(px(12.0))
            .child(self.section(Field::Reviewers, reviewers, &facts, cx))
            .child(self.section(Field::Assignees, assignees, &facts, cx))
            .child(self.section(Field::Labels, labels, &facts, cx))
            .child(
                v_flex()
                    .debug_selector(|| "side-milestone".into())
                    .gap(px(6.0))
                    .small(&t)
                    .child(
                        div()
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_color(c.text_secondary)
                            .child("Milestone"),
                    )
                    .child(milestone),
            )
            .into_any_element()
    }
}
