//! Stores, fetching, merge base, renames and worktrees against local
//! repositories built in `.scratch/oracle/`.

mod support;

use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use repo::{
    ChangeKind, ContentKind, Error, FetchEvent, FetchRequest, FileListOptions, PrRefs, RemoveMode,
    Store, StoreKind, add_worktree, checkout_detached, has_changes, list_worktrees, pr_head_ref,
    remove_worktree,
};
use support::{build_repo, git, oracle_dir, put};

fn fresh_dir(name: &str) -> PathBuf {
    let dir = oracle_dir().join("store-tests").join(name);
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn text(lines: usize, edit: Option<usize>) -> String {
    (0..lines)
        .map(|i| {
            if Some(i) == edit {
                "edited line\n".to_owned()
            } else {
                format!("line {i}\n")
            }
        })
        .collect()
}

/// A bare "forge" repository: `main` at the base commit, pull request 7 and
/// merge request 9 one commit ahead of it.
fn forge_remote(name: &str) -> (PathBuf, String, String) {
    let dir = fresh_dir(name).join("remote.git");
    let shas = build_repo(
        &dir,
        &[
            vec![put("a.txt", text(40, None)), put("old.txt", text(30, None))],
            vec![
                put("a.txt", text(40, Some(10))),
                put("new.txt", text(30, Some(3))),
                support::del("old.txt"),
            ],
        ],
    );
    git(&dir, &["update-ref", "refs/heads/main", &shas[0]]);
    git(&dir, &["update-ref", "refs/pull/7/head", &shas[1]]);
    git(
        &dir,
        &["update-ref", "refs/merge-requests/9/head", &shas[1]],
    );
    (dir, shas[0].clone(), shas[1].clone())
}

fn request(remote: &Path, refs: PrRefs, number: u64) -> FetchRequest {
    FetchRequest {
        remote_url: remote.to_string_lossy().into_owned(),
        refs,
        number,
        base_branch: "main".into(),
        timeout: Duration::from_secs(60),
    }
}

fn run_fetch(store: &Store, request: &FetchRequest) -> (Vec<String>, repo::Result<repo::Fetched>) {
    let handle = store.fetch_pr(request).unwrap();
    let mut progress = Vec::new();
    loop {
        match handle.events.recv_timeout(Duration::from_secs(90)).unwrap() {
            FetchEvent::Progress(line) => progress.push(line),
            FetchEvent::Finished(result) => return (progress, result),
        }
    }
}

#[test]
fn fetch_into_app_owned_store_then_diff() {
    let (remote, base, head) = forge_remote("fetch-app");
    let data = fresh_dir("fetch-app-data");
    let store = Store::open_or_create_bare(&data, "owner-name").unwrap();
    assert_eq!(store.kind(), StoreKind::AppOwned);

    let (_, fetched) = run_fetch(&store, &request(&remote, PrRefs::Pull, 7));
    let fetched = fetched.unwrap();
    assert_eq!(fetched.head.to_string(), head);
    assert_eq!(fetched.base.to_string(), base);
    assert_eq!(store.resolve_commit(&pr_head_ref(7)).unwrap(), fetched.head);

    // Reopening finds the same repository.
    let reopened = Store::open_or_create_bare(&data, "owner-name").unwrap();
    assert_eq!(reopened.pr_range(7).unwrap(), (fetched.base, fetched.head));

    // Renames are paired by default, with their similarity.
    let list = store.pr_file_list(7, FileListOptions::default()).unwrap();
    let paths: Vec<_> = list
        .files
        .iter()
        .map(|f| (f.kind, f.path.as_str()))
        .collect();
    assert_eq!(
        paths,
        [
            (ChangeKind::Modified, "a.txt"),
            (ChangeKind::Renamed, "new.txt")
        ]
    );
    let renamed = &list.files[1];
    assert_eq!(renamed.old_path.as_deref(), Some("old.txt"));
    assert!(renamed.similarity.is_some_and(|s| (90..100).contains(&s)));
    let content = store.file_content(renamed).unwrap();
    assert_eq!(content.kind, ContentKind::Text);
    assert_eq!(content.hunks.len(), 1);

    // With renames off the same change is a deletion and an addition.
    let plain = store
        .pr_file_list(
            7,
            FileListOptions {
                renames: false,
                detect_binary: true,
            },
        )
        .unwrap();
    let kinds: Vec<_> = plain
        .files
        .iter()
        .map(|f| (f.kind, f.path.as_str()))
        .collect();
    assert_eq!(
        kinds,
        [
            (ChangeKind::Modified, "a.txt"),
            (ChangeKind::Added, "new.txt"),
            (ChangeKind::Deleted, "old.txt")
        ]
    );
    assert!(plain.files.iter().all(|f| f.binary == Some(false)));
}

#[test]
fn gitlab_merge_request_refs() {
    let (remote, _, head) = forge_remote("fetch-gitlab");
    let store = Store::open_or_create_bare(&fresh_dir("fetch-gitlab-data"), "g").unwrap();
    let (_, fetched) = run_fetch(&store, &request(&remote, PrRefs::MergeRequests, 9));
    assert_eq!(fetched.unwrap().head.to_string(), head);
}

#[test]
fn registered_clone_is_left_alone() {
    let (remote, _, _) = forge_remote("fetch-clone");
    let dir = fresh_dir("fetch-clone-work").join("clone");
    git(
        dir.parent().unwrap(),
        &["clone", "--quiet", &remote.to_string_lossy(), "clone"],
    );
    let head_before = git(&dir, &["rev-parse", "HEAD"]);
    let branch_before = git(&dir, &["symbolic-ref", "HEAD"]);
    let store = Store::open(&dir).unwrap();
    assert_eq!(store.kind(), StoreKind::Registered);

    let (_, fetched) = run_fetch(&store, &request(&remote, PrRefs::Pull, 7));
    fetched.unwrap();
    assert_eq!(git(&dir, &["rev-parse", "HEAD"]), head_before);
    assert_eq!(git(&dir, &["symbolic-ref", "HEAD"]), branch_before);
    assert_eq!(git(&dir, &["status", "--porcelain"]), "");
    assert!(!git(&dir, &["for-each-ref", "refs/review/"]).is_empty());
    // No tracking branches or tags appeared.
    assert_eq!(
        git(
            &dir,
            &["for-each-ref", "refs/tags", "refs/remotes/origin/pr"]
        ),
        ""
    );
}

#[test]
fn worktree_in_registered_clone_needs_an_ignore_rule() {
    let (remote, _, head) = forge_remote("worktree-clone");
    let dir = fresh_dir("worktree-clone-work").join("clone");
    git(
        dir.parent().unwrap(),
        &["clone", "--quiet", &remote.to_string_lossy(), "clone"],
    );
    let store = Store::open(&dir).unwrap();
    run_fetch(&store, &request(&remote, PrRefs::Pull, 7))
        .1
        .unwrap();

    let path = store.worktree_path(7);
    assert!(path.ends_with(".claude/worktrees/pr-7") || path.ends_with(".claude\\worktrees\\pr-7"));
    let refused = add_worktree(&store, &pr_head_ref(7), &path).unwrap_err();
    assert!(
        matches!(refused, Error::WorktreeNotIgnored { .. }),
        "{refused}"
    );
    assert!(refused.to_string().contains(".gitignore"));
    assert!(!path.exists());

    std::fs::write(dir.join(".gitignore"), ".claude/worktrees/\n").unwrap();
    add_worktree(&store, &pr_head_ref(7), &path).unwrap();
    assert!(path.join("new.txt").is_file());
    assert_eq!(git(&path, &["rev-parse", "HEAD"]), head);
    // The clone's own checkout is untouched.
    assert_eq!(git(&dir, &["status", "--porcelain"]), "?? .gitignore");

    std::fs::write(path.join("scratch.txt"), "dirty").unwrap();
    assert!(remove_worktree(&store, &path, RemoveMode::Clean).is_err());
    remove_worktree(&store, &path, RemoveMode::Discard).unwrap();
    assert!(!path.exists());
}

#[test]
fn worktree_for_app_owned_store() {
    let (remote, _, head) = forge_remote("worktree-app");
    let data = fresh_dir("worktree-app-data");
    let store = Store::open_or_create_bare(&data, "o-r").unwrap();
    run_fetch(&store, &request(&remote, PrRefs::Pull, 7))
        .1
        .unwrap();
    let path = store.worktree_path(7);
    assert!(path.starts_with(&data));
    add_worktree(&store, &pr_head_ref(7), &path).unwrap();
    assert_eq!(git(&path, &["rev-parse", "HEAD"]), head);

    let listed = list_worktrees(&store).unwrap();
    assert!(listed[0].bare, "the store's own entry comes first");
    let entry = listed
        .iter()
        .find(|entry| entry.is_at(&path.to_string_lossy()))
        .expect("the new worktree is listed");
    assert_eq!(entry.head.as_deref(), Some(head.as_str()));
    assert!(entry.detached);

    assert!(!has_changes(&path).unwrap());
    std::fs::write(path.join("scratch.txt"), "dirty").unwrap();
    assert!(has_changes(&path).unwrap());
    std::fs::remove_file(path.join("scratch.txt")).unwrap();
    checkout_detached(&path, &format!("{head}~1")).unwrap();
    assert_ne!(git(&path, &["rev-parse", "HEAD"]), head);
    checkout_detached(&path, &pr_head_ref(7)).unwrap();
    assert_eq!(git(&path, &["rev-parse", "HEAD"]), head);

    remove_worktree(&store, &path, RemoveMode::Clean).unwrap();
    assert!(!path.exists());
    assert_eq!(list_worktrees(&store).unwrap().len(), 1);
}

#[test]
fn fetch_times_out_and_stops_git() {
    // A server that accepts and never answers.
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let _keep_open = std::thread::spawn(move || {
        let held: Vec<_> = listener.incoming().take(4).collect();
        std::thread::sleep(Duration::from_secs(10));
        drop(held);
    });
    let store = Store::open_or_create_bare(&fresh_dir("timeout-data"), "t").unwrap();
    let started = Instant::now();
    let (_, result) = run_fetch(
        &store,
        &FetchRequest {
            remote_url: format!("git://127.0.0.1:{port}/repo.git"),
            refs: PrRefs::Pull,
            number: 1,
            base_branch: "main".into(),
            timeout: Duration::from_secs(2),
        },
    );
    assert!(
        matches!(result, Err(Error::GitTimedOut { .. })),
        "{result:?}"
    );
    assert!(started.elapsed() < Duration::from_secs(8));
}

#[test]
fn fetch_can_be_canceled() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let _keep_open = std::thread::spawn(move || {
        let held: Vec<_> = listener.incoming().take(4).collect();
        std::thread::sleep(Duration::from_secs(10));
        drop(held);
    });
    let store = Store::open_or_create_bare(&fresh_dir("cancel-data"), "c").unwrap();
    let handle = store
        .fetch_pr(&FetchRequest {
            remote_url: format!("git://127.0.0.1:{port}/repo.git"),
            refs: PrRefs::Pull,
            number: 1,
            base_branch: "main".into(),
            timeout: Duration::from_secs(60),
        })
        .unwrap();
    std::thread::sleep(Duration::from_millis(300));
    handle.cancel();
    loop {
        match handle.events.recv_timeout(Duration::from_secs(10)).unwrap() {
            FetchEvent::Progress(_) => {}
            FetchEvent::Finished(result) => {
                assert!(
                    matches!(result, Err(Error::GitCanceled { .. })),
                    "{result:?}"
                );
                break;
            }
        }
    }
}

#[test]
fn rejects_unusable_arguments() {
    let store = Store::open_or_create_bare(&fresh_dir("args-data"), "a").unwrap();
    let base = request(Path::new("/nowhere"), PrRefs::Pull, 1);
    for bad in [
        FetchRequest {
            remote_url: "--upload-pack=evil".into(),
            ..base.clone()
        },
        FetchRequest {
            base_branch: "-x".into(),
            ..base.clone()
        },
        FetchRequest {
            base_branch: "a..b".into(),
            ..base.clone()
        },
        FetchRequest {
            base_branch: "has space".into(),
            ..base.clone()
        },
    ] {
        assert!(
            matches!(store.fetch_pr(&bad), Err(Error::InvalidArgument(_))),
            "{bad:?}"
        );
    }
    let data = fresh_dir("name-data");
    for name in ["", "..", "a/b", "../x", "/abs"] {
        assert!(
            matches!(
                Store::open_or_create_bare(&data, name),
                Err(Error::InvalidArgument(_))
            ),
            "{name:?}"
        );
    }
    assert!(Store::open(&data.join("missing")).is_err());
}

#[test]
fn unknown_revisions_and_pull_requests_fail_cleanly() {
    let store = Store::open_or_create_bare(&fresh_dir("missing-data"), "m").unwrap();
    assert!(matches!(
        store.resolve_commit("refs/review/pr/99"),
        Err(Error::RevisionNotFound(_))
    ));
    assert!(store.pr_range(99).is_err());
}

#[test]
fn symlinks_lists_every_link_with_its_target() {
    use support::{Mode, put_mode};
    let dir = fresh_dir("symlinks").join("links.git");
    let shas = build_repo(
        &dir,
        &[vec![
            put("README.md", "hi\n"),
            put_mode("docs/key", Mode::Link, "../../.ssh/id_ed25519"),
            put_mode("latest", Mode::Link, "docs/README.md"),
        ]],
    );
    let store = Store::open(&dir).unwrap();
    let head = store.resolve_commit(&shas[0]).unwrap();
    let mut links = store.symlinks(head).unwrap();
    links.sort();
    assert_eq!(
        links,
        [
            ("docs/key".to_owned(), "../../.ssh/id_ed25519".to_owned()),
            ("latest".to_owned(), "docs/README.md".to_owned()),
        ]
    );
}
