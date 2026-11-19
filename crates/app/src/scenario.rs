//! Scripted writing for screenshots and the kill test: opens composers on
//! the open pull request and types into them through the window's real key
//! path, one keystroke at a time.

use std::time::Duration;

use forge::model::{LineRef, Side};
use gpui::{
    AnyWindowHandle, App, AsyncApp, Keystroke, Modifiers, MouseButton, MouseDownEvent,
    MouseMoveEvent, MouseUpEvent, PlatformInput, ScrollDelta, ScrollEvent, point, px,
};
use store::DraftAnchor;

use crate::model::PrKey;
use crate::session::Session;
use crate::suggestions::{Offer, SuggestionKey};
use crate::writing::Writing;

/// What to write, in order: line comments, a reply, then queued comments.
#[derive(Clone, Debug, Default)]
pub struct Scenario {
    /// Text for a comment on the first changed file's first line.
    pub compose: Option<String>,
    /// Show that comment's Preview tab.
    pub preview: bool,
    /// Text for a reply on the first inline thread.
    pub reply: Option<String>,
    /// Comments added to the review, one line each.
    pub queue: Vec<String>,
    /// Submit the review as Comment once the comments are queued.
    pub submit: bool,
    /// Text of a reply, typed and sent in the thread the first queued
    /// comment opened (after the submit).
    pub send_reply: Option<String>,
    /// Resolve that thread at the end, the way its card's button does.
    pub resolve: bool,
    /// Keys pressed after the comment is typed, such as `escape`.
    pub press: Vec<String>,
    /// Pointer steps at the end, `move X Y`, `down X Y`, `up X Y` or
    /// `wheel X Y` (400 px down) in window pixels.
    pub mouse: Vec<String>,
    /// Print `EDIT n` after each typed character, for a parent process.
    pub announce: bool,
    /// Ask to apply the first suggestion on the pull request, as its card's
    /// button does, and leave the confirmation up.
    pub confirm_apply: bool,
}

const POLL: Duration = Duration::from_millis(40);
const KEY_GAP: Duration = Duration::from_millis(12);
const WAIT: Duration = Duration::from_secs(60);
const STEP: Duration = Duration::from_millis(250);

/// The keystroke that types `ch`.
#[must_use]
pub fn keystroke_for(ch: char) -> Keystroke {
    match ch {
        '\n' => Keystroke {
            modifiers: Modifiers::none(),
            key: "enter".into(),
            key_char: None,
        },
        ch if ch.is_uppercase() => Keystroke {
            modifiers: Modifiers::shift(),
            key: ch.to_lowercase().to_string(),
            key_char: Some(ch.to_string()),
        },
        ch => Keystroke {
            modifiers: Modifiers::none(),
            key: if ch == ' ' {
                "space".into()
            } else {
                ch.to_string()
            },
            key_char: Some(ch.to_string()),
        },
    }
}

/// Runs `scenario` in `window` once a pull request's diff is up.
pub fn start(scenario: Scenario, window: AnyWindowHandle, cx: &mut App) {
    cx.spawn(async move |cx| {
        if let Err(error) = run(&scenario, window, cx).await {
            eprintln!("review: scenario stopped: {error}");
        }
    })
    .detach();
}

async fn wait_for(
    cx: &AsyncApp,
    what: &str,
    mut ready: impl FnMut(&mut App) -> bool,
) -> Result<(), String> {
    let mut waited = Duration::ZERO;
    while !cx.update(&mut ready) {
        if waited > WAIT {
            return Err(format!("{what} did not happen within {WAIT:?}"));
        }
        cx.background_executor().timer(POLL).await;
        waited += POLL;
    }
    Ok(())
}

/// The first file's path and a line of it the diff shows, once a diff is up.
pub fn first_line(cx: &App) -> Option<(String, LineRef)> {
    let session = Session::global(cx);
    let open = session.read(cx).open_pr()?;
    let diff = open.diff.as_ref()?.diff.clone();
    let file = diff.files.iter().find(|f| !f.hunks.is_empty())?;
    let hunk = &file.hunks[0];
    Some((
        file.path.clone(),
        LineRef {
            side: Side::New,
            line: hunk.new_start + 1,
        },
    ))
}

/// The focused pull request and the key of the first suggestion it can
/// apply.
fn first_suggestion(session: &Session) -> Option<(PrKey, SuggestionKey)> {
    let open = session.open_pr()?;
    let pr = open.key.clone();
    let key = open
        .threads
        .iter()
        .flat_map(|thread| {
            thread.comments.iter().map(|comment| SuggestionKey {
                thread: thread.id.clone(),
                comment: comment.id.clone(),
            })
        })
        .find(|key| matches!(session.suggestion(&pr, key), Some(Offer::Apply(_))))?;
    Some((pr, key))
}

fn anchor(path: &str, line: LineRef) -> DraftAnchor {
    DraftAnchor::Line {
        path: path.to_owned(),
        line,
        start: None,
    }
}

async fn type_text(
    cx: &AsyncApp,
    window: AnyWindowHandle,
    text: &str,
    announce: bool,
) -> Result<(), String> {
    for (typed, ch) in text.chars().enumerate() {
        cx.update(|cx| {
            window
                .update(cx, |_, window, cx| {
                    window.dispatch_keystroke(keystroke_for(ch), cx);
                })
                .map_err(|error| error.to_string())
        })?;
        if announce {
            println!("EDIT {}", typed + 1);
        }
        cx.background_executor().timer(KEY_GAP).await;
    }
    Ok(())
}

/// Opens `anchor`'s composer and waits until its text box has focus.
async fn open_focused(
    cx: &AsyncApp,
    window: AnyWindowHandle,
    writing: &gpui::Entity<Writing>,
    anchor: DraftAnchor,
    suggestion: Option<String>,
) -> Result<(), String> {
    cx.update(|cx| writing.update(cx, |w, cx| w.open(anchor.clone(), suggestion, cx)));
    wait_for(cx, "the composer", |cx| {
        let Some(composer) = writing
            .read(cx)
            .slot(&anchor)
            .and_then(|slot| slot.composer.clone())
        else {
            return false;
        };
        let handle = composer.read(cx).input_focus(cx);
        window
            .update(cx, |_, window, _| handle.is_focused(window))
            .unwrap_or(false)
    })
    .await
}

async fn run(scenario: &Scenario, window: AnyWindowHandle, cx: &AsyncApp) -> Result<(), String> {
    wait_for(cx, "a diff", |cx| first_line(cx).is_some()).await?;
    let (path, line) = cx
        .update(|cx| first_line(cx))
        .ok_or_else(|| "no diff".to_owned())?;
    let writing = cx
        .update(|cx| Session::global(cx).read(cx).writing())
        .ok_or_else(|| "no pull request open".to_owned())?;

    if let Some(text) = &scenario.compose {
        let anchor = anchor(&path, line);
        open_focused(cx, window, &writing, anchor.clone(), None).await?;
        type_text(cx, window, text, scenario.announce).await?;
        if scenario.preview {
            cx.update(|cx| {
                if let Some(composer) = writing
                    .read(cx)
                    .slot(&anchor)
                    .and_then(|s| s.composer.clone())
                {
                    window
                        .update(cx, |_, window, cx| {
                            composer.update(cx, |c, cx| c.show_preview(window, cx));
                        })
                        .ok();
                }
            });
        }
    }

    if let Some(text) = &scenario.reply {
        let thread = cx.update(|cx| {
            Session::global(cx).read(cx).open_pr().and_then(|open| {
                open.threads
                    .iter()
                    .find(|t| t.anchor.is_some())
                    .map(|t| t.id.clone())
            })
        });
        let thread = thread.ok_or_else(|| "no thread to reply to".to_owned())?;
        open_focused(cx, window, &writing, DraftAnchor::Reply { thread }, None).await?;
        type_text(cx, window, text, scenario.announce).await?;
    }

    for (n, text) in scenario.queue.iter().enumerate() {
        let at = LineRef {
            side: Side::New,
            line: line.line + 1 + u32::try_from(n).unwrap_or(0),
        };
        let anchor = anchor(&path, at);
        open_focused(cx, window, &writing, anchor.clone(), None).await?;
        type_text(cx, window, text, scenario.announce).await?;
        let ctrl_enter = Keystroke {
            modifiers: Modifiers::control(),
            key: "enter".into(),
            key_char: None,
        };
        cx.update(|cx| {
            window.update(cx, |_, window, cx| {
                window.dispatch_keystroke(ctrl_enter, cx)
            })
        })
        .map_err(|error| error.to_string())?;
        wait_for(cx, "the comment to be queued", |cx| {
            writing.read(cx).slot(&anchor).is_none()
        })
        .await?;
    }
    if scenario.announce {
        println!("TYPED");
    }
    if scenario.submit || scenario.send_reply.is_some() || scenario.resolve {
        let result = review_steps(scenario, window, &writing, cx).await;
        match &result {
            Ok(()) => println!("SCENARIO done"),
            Err(error) => println!("SCENARIO failed: {error}"),
        }
        result?;
    }
    if scenario.confirm_apply {
        wait_for(cx, "a suggestion", |cx| {
            Session::global(cx).update(cx, |session, cx| {
                let Some((pr, key)) = first_suggestion(session) else {
                    return false;
                };
                session.request_apply(&pr, &[key], cx);
                true
            })
        })
        .await?;
    }
    for step in &scenario.mouse {
        let event = mouse_event(step)?;
        cx.update(|cx| window.update(cx, |_, window, cx| window.dispatch_event(event, cx)))
            .map_err(|error| error.to_string())?;
        cx.background_executor().timer(STEP).await;
    }
    for key in &scenario.press {
        let keystroke = Keystroke::parse(Box::leak(key.clone().into_boxed_str()))
            .map_err(|error| format!("--press {key}: {error}"))?;
        cx.update(|cx| window.update(cx, |_, window, cx| window.dispatch_keystroke(keystroke, cx)))
            .map_err(|error| error.to_string())?;
        cx.background_executor().timer(STEP).await;
    }
    Ok(())
}

/// Submit, reply and resolve, through the same calls the submit bar, the
/// composer and the card's buttons make. Prints a `SCENARIO` line per step
/// for the caller to check.
async fn review_steps(
    scenario: &Scenario,
    window: AnyWindowHandle,
    writing: &gpui::Entity<Writing>,
    cx: &AsyncApp,
) -> Result<(), String> {
    let session = cx.update(|cx| Session::global(cx));
    if scenario.submit {
        cx.update(|cx| writing.update(cx, |w, cx| w.submit_review(cx)));
        wait_for(cx, "the review to be submitted", |cx| {
            !writing.read(cx).submitting()
        })
        .await?;
        if let Some(error) = cx.update(|cx| writing.read(cx).review_error().cloned()) {
            return Err(error.to_string());
        }
        println!("SCENARIO submitted");
    }
    let first = scenario.queue.first().cloned();
    let find = move |cx: &App| -> Option<forge::model::Thread> {
        let first = first.as_deref()?;
        let open = Session::global(cx).read(cx).open_pr()?.threads.clone();
        open.iter()
            .find(|t| t.anchor.is_some() && t.comments.first().is_some_and(|c| c.body == first))
            .cloned()
    };
    wait_for(cx, "the submitted thread", {
        let find = find.clone();
        move |cx| find(cx).is_some()
    })
    .await?;
    let thread = cx
        .update(|cx| find(cx))
        .ok_or_else(|| "the submitted thread is gone".to_owned())?;
    println!("SCENARIO thread {}", thread.id);
    if let Some(text) = &scenario.send_reply {
        let anchor = DraftAnchor::Reply {
            thread: thread.id.clone(),
        };
        open_focused(cx, window, writing, anchor.clone(), None).await?;
        type_text(cx, window, text, false).await?;
        let ctrl_enter = Keystroke {
            modifiers: Modifiers::control(),
            key: "enter".into(),
            key_char: None,
        };
        cx.update(|cx| {
            window.update(cx, |_, window, cx| {
                window.dispatch_keystroke(ctrl_enter, cx)
            })
        })
        .map_err(|error| error.to_string())?;
        let id = thread.id.clone();
        let body = text.clone();
        wait_for(cx, "the reply", move |cx| {
            let replied = Session::global(cx).read(cx).open_pr().is_some_and(|open| {
                open.threads
                    .iter()
                    .any(|t| t.id == id && t.comments.iter().any(|c| c.body == body))
            });
            replied || writing.read(cx).slot(&anchor).is_none()
        })
        .await?;
        println!("SCENARIO replied");
    }
    if scenario.resolve {
        let current = cx
            .update(|cx| find(cx))
            .ok_or_else(|| "the thread to resolve is gone".to_owned())?;
        cx.update(|cx| writing.update(cx, |w, cx| w.set_resolved(&current, true, cx)));
        let id = current.id.clone();
        wait_for(cx, "the thread to resolve", move |cx| {
            session
                .read(cx)
                .open_pr()
                .is_some_and(|open| open.threads.iter().any(|t| t.id == id && t.is_resolved()))
                || writing
                    .read(cx)
                    .thread_state(&id)
                    .is_some_and(|s| matches!(s, crate::writing::ActionState::Failed(_)))
        })
        .await?;
        if let Some(crate::writing::ActionState::Failed(error)) =
            cx.update(|cx| writing.read(cx).thread_state(&current.id).cloned())
        {
            return Err(error.to_string());
        }
        println!("SCENARIO resolved {}", current.id);
    }
    Ok(())
}

/// `move 500 200`, `down 500 200`, `up 500 200`.
fn mouse_event(step: &str) -> Result<PlatformInput, String> {
    let parts: Vec<&str> = step.split_whitespace().collect();
    let [kind, x, y] = parts[..] else {
        return Err(format!("--mouse {step}: expected `move|down|up|wheel X Y`"));
    };
    let position = point(
        px(x.parse::<f32>().map_err(|e| e.to_string())?),
        px(y.parse::<f32>().map_err(|e| e.to_string())?),
    );
    let modifiers = Modifiers::none();
    match kind {
        "move" => Ok(PlatformInput::MouseMove(MouseMoveEvent {
            position,
            pressed_button: None,
            modifiers,
        })),
        "drag" => Ok(PlatformInput::MouseMove(MouseMoveEvent {
            position,
            pressed_button: Some(MouseButton::Left),
            modifiers,
        })),
        "down" => Ok(PlatformInput::MouseDown(MouseDownEvent {
            button: MouseButton::Left,
            position,
            modifiers,
            click_count: 1,
            first_mouse: false,
        })),
        // `wheel X Y`: turns the wheel over (X, Y) by 400 px down; a
        // negative step is not needed for screenshots.
        "wheel" => Ok(PlatformInput::Scroll(ScrollEvent {
            position,
            delta: ScrollDelta::Pixels(point(px(0.0), px(-400.0))),
            ..ScrollEvent::default()
        })),
        "up" => Ok(PlatformInput::MouseUp(MouseUpEvent {
            button: MouseButton::Left,
            position,
            modifiers,
            click_count: 1,
        })),
        other => Err(format!("--mouse {step}: unknown step {other}")),
    }
}
