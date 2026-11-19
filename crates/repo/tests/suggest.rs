//! Suggestion commits from git objects and their fast-forward push, against
//! a bare "forge" repository built in `.scratch/oracle/suggest-tests/`.

mod support;

use std::path::{Path, PathBuf};
use std::time::Duration;

use repo::{
    Error, FetchEvent, FetchRequest, LineEdit, ObjectId, PrRefs, PushTarget, Store, pr_head_ref,
};
use support::{build_repo, git, oracle_dir, put};

fn fresh_dir(name: &str) -> PathBuf {
    let dir = oracle_dir().join("suggest-tests").join(name);
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// A remote whose branch `topic` (also pull request 7) is one commit on
/// `main`, and an app-owned store that fetched it. The store commits
/// unsigned as a test identity, whatever the user's git config says.
fn setup(name: &str) -> (PathBuf, Store, ObjectId) {
    let dir = fresh_dir(name);
    let remote = dir.join("remote.git");
    let shas = build_repo(
        &remote,
        &[
            vec![put("docs/intro.md", "one\ntwo\nthree\nfour\n")],
            vec![
                put("docs/intro.md", "one\ntwo, changed\nthree\nfour\n"),
                put("src/lib.rs", "fn a() {}\r\nfn b() {}\r\n"),
            ],
        ],
    );
    git(&remote, &["update-ref", "refs/heads/main", &shas[0]]);
    git(&remote, &["update-ref", "refs/heads/topic", &shas[1]]);
    git(&remote, &["update-ref", "refs/pull/7/head", &shas[1]]);
    let store = Store::open_or_create_bare(&dir, "local").unwrap();
    for (key, value) in [
        ("user.name", "Suggest Test"),
        ("user.email", "suggest@example.com"),
        ("commit.gpgsign", "false"),
    ] {
        git(store.git_dir(), &["config", key, value]);
    }
    let handle = store
        .fetch_pr(&FetchRequest {
            remote_url: remote.to_string_lossy().into_owned(),
            refs: PrRefs::Pull,
            number: 7,
            base_branch: "main".into(),
            timeout: Duration::from_secs(60),
        })
        .unwrap();
    loop {
        match handle.events.recv_timeout(Duration::from_secs(90)).unwrap() {
            FetchEvent::Progress(_) => {}
            FetchEvent::Finished(result) => {
                result.unwrap();
                break;
            }
        }
    }
    let head = store.resolve_commit(&pr_head_ref(7)).unwrap();
    (remote, store, head)
}

fn target(remote: &Path) -> PushTarget {
    PushTarget {
        remote_url: remote.to_string_lossy().into_owned(),
        branch: "topic".to_owned(),
        timeout: Duration::from_secs(60),
    }
}

fn edit(path: &str, first: u32, last: u32, lines: &[&str]) -> LineEdit {
    LineEdit {
        path: path.to_owned(),
        first,
        last,
        lines: lines.iter().map(|&l| l.to_owned()).collect(),
    }
}

#[test]
fn suggestions_become_one_commit_on_the_head_and_push_as_a_fast_forward() {
    let (remote, store, head) = setup("apply");
    let edits = [
        edit("docs/intro.md", 2, 2, &["two, suggested"]),
        edit("docs/intro.md", 4, 4, &["four", "five"]),
        edit("src/lib.rs", 2, 2, &["fn b() -> u8 { 1 }"]),
    ];
    let commit = store
        .commit_edits(head, &edits, "Apply 3 suggestions from code review")
        .unwrap();
    let local = store.git_dir().to_owned();
    assert_eq!(
        git(&local, &["rev-parse", &format!("{commit}^")]),
        head.to_string(),
        "one commit, on the head"
    );
    assert_eq!(
        git(
            &local,
            &["log", "-1", "--format=%an <%ae>%n%s", &commit.to_string()]
        ),
        "Suggest Test <suggest@example.com>\nApply 3 suggestions from code review"
    );
    // Only the two files changed, nothing else in the tree.
    assert_eq!(
        git(
            &local,
            &[
                "diff",
                "--name-only",
                &head.to_string(),
                &commit.to_string()
            ]
        ),
        "docs/intro.md\nsrc/lib.rs"
    );

    assert_eq!(store.remote_head(&target(&remote)).unwrap(), Some(head));
    store
        .push_fast_forward(&target(&remote), head, commit)
        .unwrap();
    assert_eq!(
        git(&remote, &["rev-parse", "refs/heads/topic"]),
        commit.to_string()
    );
    assert_eq!(
        git(&remote, &["show", "topic:docs/intro.md"]),
        "one\ntwo, suggested\nthree\nfour\nfive"
    );
    let lib = support::git_bytes(&remote, &["show", "topic:src/lib.rs"]);
    assert_eq!(lib, b"fn a() {}\r\nfn b() -> u8 { 1 }\r\n", "CRLF is kept");
    // The pull request ref and main are untouched.
    assert_eq!(
        git(&remote, &["rev-parse", "refs/pull/7/head"]),
        head.to_string()
    );
}

#[test]
fn a_push_is_refused_when_the_remote_branch_moved() {
    let (remote, store, head) = setup("moved");
    // Someone pushes to the branch after the fetch.
    let tree = git(&remote, &["rev-parse", "topic^{tree}"]);
    let moved = git(
        &remote,
        &[
            "-c",
            "user.name=Other",
            "-c",
            "user.email=o@example.com",
            "-c",
            "commit.gpgsign=false",
            "commit-tree",
            &tree,
            "-p",
            &head.to_string(),
            "-m",
            "someone else",
        ],
    );
    git(&remote, &["update-ref", "refs/heads/topic", &moved]);

    let commit = store
        .commit_edits(
            head,
            &[edit("docs/intro.md", 1, 1, &["ONE"])],
            "Apply suggestion",
        )
        .unwrap();
    let err = store
        .push_fast_forward(&target(&remote), head, commit)
        .unwrap_err();
    assert!(matches!(err, Error::RemoteMoved { .. }), "{err}");
    assert_eq!(
        git(&remote, &["rev-parse", "refs/heads/topic"]),
        moved,
        "not overwritten"
    );

    // A commit that is not on the expected head is never pushed either.
    let err = store
        .push_fast_forward(&target(&remote), commit, head)
        .unwrap_err();
    assert!(matches!(err, Error::InvalidArgument(_)), "{err}");
}

#[test]
fn edits_on_missing_files_lines_or_odd_paths_are_refused_before_any_write() {
    let (_remote, store, head) = setup("refused");
    for edits in [
        vec![edit("docs/missing.md", 1, 1, &["x"])],
        vec![edit("docs/intro.md", 9, 9, &["x"])],
        vec![edit("docs", 1, 1, &["x"])],
        vec![edit("../docs/intro.md", 1, 1, &["x"])],
        vec![edit("docs/intro.md", 2, 2, &["two, changed"])],
        vec![],
    ] {
        let err = store.commit_edits(head, &edits, "x").unwrap_err();
        assert!(matches!(err, Error::InvalidArgument(_)), "{edits:?}: {err}");
    }
}
