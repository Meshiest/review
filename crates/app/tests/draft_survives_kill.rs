//! A comment typed into a composer is in the store shortly after the last
//! edit: the app is started on the fixtures with a scripted composer, killed
//! with TerminateProcess (no shutdown code runs) just past the draft
//! debounce, and the store is reopened.
//!
//! The wait is the debounce window plus margin, not a measurement of speed:
//! the draft saver writes 150 ms after the last edit.

use std::io::{BufRead as _, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use app::drafts::DEBOUNCE;
use app::fixtures::Fixtures;
use forge::model::{LineRef, Side};
use store::{DraftAnchor, Store};

const PR: u64 = 41;
/// Keys the diff panel binds when no text box has focus: typing them into a
/// composer must insert them, not run the action.
const TEXT: &str = "Swap the sort here: n+1 per x? See [f] and c.\nThen s, p and t.";
/// Past the debounce, and within 200 ms of the last edit.
const KILL_AFTER_LAST_EDIT: Duration = Duration::from_millis(175);
const STARTUP: Duration = Duration::from_secs(180);

struct Scratch(PathBuf);

impl Scratch {
    fn new(name: &str) -> Self {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../.scratch/app-tests")
            .join(format!("{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("create the scratch dir");
        Self(dir)
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[test]
fn draft_survives_kill() {
    assert!(KILL_AFTER_LAST_EDIT > DEBOUNCE);
    assert!(KILL_AFTER_LAST_EDIT < Duration::from_millis(200));
    let scratch = Scratch::new("kill");
    let db = scratch.0.join("cache.db");

    let mut child = Command::new(env!("CARGO_BIN_EXE_review"))
        .args(["--fixtures", "--layout", "default", "--unthrottled"])
        .args(["--open-pr", &PR.to_string()])
        .arg("--store")
        .arg(&db)
        .args(["--compose", TEXT, "--announce-edits"])
        // The app exits by itself if this test dies before killing it.
        .args(["--exit-after", "300"])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("start the app");

    // `EDIT n` after each key, `TYPED` after the last.
    let (tx, rx) = mpsc::channel();
    let stdout = child.stdout.take().expect("piped stdout");
    thread::spawn(move || {
        for line in BufReader::new(stdout).lines().map_while(Result::ok) {
            if tx.send((line, Instant::now())).is_err() {
                break;
            }
        }
    });
    let mut edits = 0;
    let last_edit = loop {
        let (line, at) = rx
            .recv_timeout(STARTUP)
            .unwrap_or_else(|_| panic!("the app typed {edits} of {} keys", TEXT.chars().count()));
        if line == "TYPED" {
            break at;
        }
        assert!(line.starts_with("EDIT "), "unexpected output: {line}");
        edits += 1;
    };
    assert_eq!(edits, TEXT.chars().count());

    let wait = KILL_AFTER_LAST_EDIT.saturating_sub(last_edit.elapsed());
    thread::sleep(wait);
    // `kill` is TerminateProcess on Windows: nothing in the app gets to run.
    child.kill().expect("kill the app");
    child.wait().expect("reap the app");

    let store = Store::open(&db).expect("reopen the store");
    let key = Fixtures::load()
        .inbox
        .prs
        .iter()
        .find(|pr| pr.number() == PR)
        .map(|pr| pr.key.clone())
        .expect("the fixture exists");
    let drafts = store.drafts_for(&key).wait().expect("read the drafts");
    assert_eq!(drafts.len(), 1, "{drafts:?}");
    let draft = &drafts[0];
    let DraftAnchor::Line { line, start, .. } = &draft.anchor else {
        panic!("a line draft, got {:?}", draft.anchor);
    };
    assert_eq!(line.side, Side::New);
    assert_eq!(*start, None::<LineRef>);
    assert_eq!(draft.body, TEXT);
}
