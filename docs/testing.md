# Testing the UI headless

`crates/app_tests` drives the real shell in a window of gpui's test
platform: no OS window, no OS input. Run it with `cargo test -p app_tests`
(`just test` includes it) in a debug build: the `debug_selector`s it clicks
are not recorded in release.

## How it works

`#[gpui::test]` builds a `TestAppContext` on a deterministic scheduler and a
fake platform (`gpui/test-support`). Windows are drawn at the end of every
effect cycle; keys, clicks and resizes are delivered to the window by the
test (`simulate_keystrokes`, `simulate_input`, `simulate_click`), so key
bindings, focus, hit testing and the app's listeners all run as they do for
a user. Timers fire only when the test advances the clock.

`app_tests` takes `gpui/test-support` as a dev-dependency only. A build of
`app` (`cargo build -p app ...`, every benchmark run) does not enable it;
check with `cargo tree -p app -e features,normal,build,dev -i gpui-pre`.
Workspace-wide builds that include dev targets (`cargo test --workspace`,
`cargo build --workspace --examples`) do unify it in.

## Writing a test

Add a module to `crates/app_tests/tests/ui/` and list it in `main.rs`. One
test binary keeps the link to one.

```rust
#[gpui::test]
fn j_opens_the_next_pull_request(cx: &mut TestAppContext) {
    let mut shell = open_shell(cx, Options::default());
    shell.keys("j enter");
    // read state through the entities: shell.session(), shell.diff_view(), ...
}
```

The harness (`tests/ui/harness.rs`):

- `open_shell(cx, Options)`: `app::init` on fixtures and the main window with
  the shell, as `--fixtures` starts. `Options` picks the start layout (a preset
  or a saved `DockAreaState`), a file the shell saves layout changes to, the
  theme, a store file (drafts stay in memory without one), the synthetic diff
  size and a pull request to open. Toasts are dropped, never shown.
- `ShellWindow`: `keys("] f")`, `type_text`, `click_at`, `click(selector)` for
  an element with a `debug_selector`, `click_element(id)` for the
  gpui-component buttons, checkboxes and tabs that register themselves
  (`element(id)` reads their checked state and bounds), `settle()` to run
  every task, `advance(duration)` to fire debounces, `settle_until(what, f)`
  for work on threads outside gpui (store, forge hub, highlighter), and
  getters for the session, dock, panels, writing and diff view.
- `Scratch`: a directory under `.scratch/app-tests/`, removed on drop. Every
  file a test writes goes there, never in the user's config or data dir.
- `fake_services` and `go_live`: the session on `Services` over a real
  `ForgeHub` with fake backends (`FakeForge` serves CI runs and a log and
  records re-runs); the config registers each repository with a clone that is
  not a repository, so no open ever fetches.

The registered-element lookup is gpui-base's `test_support` module, the
layer gpui-kit's test API is built on; gpui-kit's own `test-support`
feature cannot be turned on here (it pulls gpui-fast's renderer, whose
`wgpu` pin conflicts). gpui-component widgets register themselves, and the
app's own `ui::Button` registers too, so button tests can find it by id.

To click one of the app's own elements, give it a
`.debug_selector(|| "name".into())`: it costs nothing outside test builds.
Assert on what the user would see change: the entity state a view renders
from, the dock's own `dump()`, the registered element's checked state.

## What it cannot check

- Frame timing and real rendering: text is shaped by a fake text system
  (every glyph the same width) and nothing is rasterized. Use the benchmarks
  (`just bench`: the `*_bench` examples and the ignored timing tests of
  `repo` and `store`, in release builds) and screenshots.
- OS integration: toasts, the clipboard service, IME, window chrome,
  focus between OS windows, display scaling. Check those by hand.
- The app's threads (store, forge hub, highlighter) run for real, so the
  harness allows their wakeups; tests wait on outcomes with `settle_until`,
  never on elapsed time.

Tests never send OS input: no `SendInput`, no synthetic events outside the
test window.
