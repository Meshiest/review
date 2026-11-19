//! Opening a pull request whose snapshot is cached and whose objects are
//! local paints its diff without the network: the fake forge never answers
//! and the git remote does not exist, yet the diff arrives, and only after
//! it does the fetch fail.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use app::config::Config;
use app::opener::{OpenRequest, OpenStatus, OpenUpdate, open_pr};
use app::services::{Services, Spawn};
use async_trait::async_trait;
use chrono::Utc;
use forge::model::{
    BranchRef, CheckRun, ForgeKind, InboxSection, PrNumber, PrState, PullRequest, PullRequestId,
    RepoRef, ReviewDecision, Sha, Snapshot, Stack, Thread, User,
};
use forge::{Capabilities, Forge, ForgeHub, Result};
use futures::FutureExt as _;
use futures::StreamExt as _;
use futures::channel::mpsc;
use store::PrKey;

const NUMBER: u64 = 7;

/// A forge that is asked but never answers: any update that needed it
/// would never arrive.
struct Silent {
    calls: Arc<AtomicUsize>,
}

impl Silent {
    async fn hang<T>(&self) -> Result<T> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        futures::future::pending().await
    }
}

#[async_trait]
impl Forge for Silent {
    fn kind(&self) -> ForgeKind {
        ForgeKind::GitHub
    }

    fn host(&self) -> &str {
        "github.com"
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities::GITHUB
    }

    async fn whoami(&self) -> Result<User> {
        self.hang().await
    }

    async fn inbox(&self, _: &InboxSection) -> Result<Vec<PullRequest>> {
        self.hang().await
    }

    async fn pull_request(&self, _: &RepoRef, _: PrNumber) -> Result<PullRequest> {
        self.hang().await
    }

    async fn threads(&self, _: &RepoRef, _: PrNumber) -> Result<Vec<Thread>> {
        self.hang().await
    }

    async fn checks(&self, _: &RepoRef, _: PrNumber) -> Result<Vec<CheckRun>> {
        self.hang().await
    }

    async fn stack(&self, _: &RepoRef, _: PrNumber) -> Result<Stack> {
        self.hang().await
    }

    async fn snapshot(&self, _: &RepoRef, _: PrNumber) -> Result<Snapshot> {
        self.hang().await
    }
}

fn git(dir: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(["-c", "user.name=t", "-c", "user.email=t@example.org"])
        .args(["-c", "commit.gpgsign=false", "-c", "core.autocrlf=false"])
        .args(args)
        .output()
        .expect("git runs");
    assert!(
        output.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).trim().to_owned()
}

/// A clone with `main` and a pull request branch, its refs where a fetch
/// would have put them.
fn clone_with_pr(root: &Path) -> PathBuf {
    let dir = root.join("clone");
    std::fs::create_dir_all(&dir).unwrap();
    git(&dir, &["init", "-q", "-b", "main"]);
    std::fs::create_dir_all(dir.join("src")).unwrap();
    std::fs::write(dir.join("src/lib.rs"), "fn a() {}\nfn b() {}\nfn c() {}\n").unwrap();
    std::fs::write(dir.join("README.md"), "# demo\n").unwrap();
    std::fs::write(dir.join("old.txt"), "going away\n").unwrap();
    git(&dir, &["add", "-A"]);
    git(&dir, &["commit", "-q", "-m", "base"]);
    git(&dir, &["checkout", "-q", "-b", "feature"]);
    std::fs::write(
        dir.join("src/lib.rs"),
        "fn a() {}\nfn b2() {}\nfn c() {}\nfn d() {}\n",
    )
    .unwrap();
    std::fs::write(dir.join("src/new.rs"), "pub struct New;\n").unwrap();
    std::fs::remove_file(dir.join("old.txt")).unwrap();
    git(&dir, &["add", "-A"]);
    git(&dir, &["commit", "-q", "-m", "change"]);
    let head = git(&dir, &["rev-parse", "HEAD"]);
    let base = git(&dir, &["rev-parse", "main"]);
    git(&dir, &["update-ref", &repo::pr_head_ref(NUMBER), &head]);
    git(&dir, &["update-ref", &repo::pr_base_ref(NUMBER), &base]);
    git(&dir, &["checkout", "-q", "main"]);
    dir
}

fn snapshot(repo: &RepoRef, head: &str) -> Snapshot {
    let branch = |name: &str, sha: &str| BranchRef {
        repo: Some(repo.clone()),
        name: name.to_owned(),
        sha: Sha::new(sha),
    };
    Snapshot {
        pull_request: PullRequest {
            id: PullRequestId::new("PR_7"),
            repo: repo.clone(),
            number: PrNumber(NUMBER),
            title: "Rename b".to_owned(),
            body: String::new(),
            author: User::new("cora"),
            state: PrState::Open,
            base: branch("main", &"0".repeat(40)),
            head: branch("feature", head),
            merge_base: None,
            created_at: Utc::now(),
            updated_at: Utc::now(),
            review_decision: ReviewDecision::ReviewRequired,
            reviewers: Vec::new(),
            ci: None,
            comment_count: None,
            unresolved_threads: None,
            labels: Vec::new(),
            diff_stat: None,
            assignees: Vec::new(),
            milestone: None,
            web_url: String::new(),
            head_writable: true,
        },
        threads: Vec::new(),
        checks: Vec::new(),
    }
}

fn thread_spawn() -> Spawn {
    Arc::new(|future| {
        std::thread::spawn(move || futures::executor::block_on(future));
    })
}

#[test]
fn a_cached_pull_request_paints_without_the_network() {
    let root =
        PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(format!("open_pr_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).unwrap();
    let clone = clone_with_pr(&root);
    let head = git(&clone, &["rev-parse", &repo::pr_head_ref(NUMBER)]);
    let missing_remote = root.join("no-such-remote");

    let toml_path = |path: &Path| path.display().to_string().replace('\\', "/");
    let config = Config::parse(&format!(
        "data_dir = \"{}\"\n\n[[forges]]\nkind = \"github\"\nhost = \"github.com\"\n\n\
         [[repos]]\nhost = \"github.com\"\nrepo = \"cora/demo\"\nclone = \"{}\"\nremote = \"{}\"\n",
        toml_path(&root.join("data")),
        toml_path(&clone),
        toml_path(&missing_remote),
    ))
    .unwrap();
    let repo = RepoRef::new("github.com", "cora", "demo");
    let key = PrKey::new(repo.clone(), PrNumber(NUMBER));

    let store = store::Store::open(root.join("cache.sqlite")).unwrap();
    store.put_snapshot(snapshot(&repo, &head)).wait().unwrap();
    store
        .set_viewed(&key, "README.md", "unused", true)
        .wait()
        .unwrap();

    let calls = Arc::new(AtomicUsize::new(0));
    let silent: Arc<dyn Forge> = Arc::new(Silent {
        calls: calls.clone(),
    });
    let hub = ForgeHub::with_backends(vec![silent]).unwrap();
    let services = Services::new(
        Arc::new(config),
        Some(store),
        Some(Arc::new(hub)),
        thread_spawn(),
    );

    let (tx, mut rx) = mpsc::unbounded();
    let request = OpenRequest {
        key: key.clone(),
        base_branch: None,
        forge_viewed: false,
    };
    let started = Instant::now();
    std::thread::spawn(move || futures::executor::block_on(open_pr(services, request, tx)));

    let deadline = Instant::now() + Duration::from_secs(60);
    let mut seen = Vec::new();
    let mut first_paint = None;
    let mut fetch_failed = false;
    while Instant::now() < deadline && !fetch_failed {
        let Some(update) = rx.next().now_or_never() else {
            std::thread::sleep(Duration::from_millis(5));
            continue;
        };
        let update = update.expect("the open keeps running while the forge hangs");
        match &update {
            OpenUpdate::Snapshot { fresh, .. } => {
                assert!(!fresh, "the silent forge never answers");
                seen.push("cached snapshot");
            }
            OpenUpdate::Diff(diff) if first_paint.is_none() => {
                assert!(
                    !seen.contains(&"fetching"),
                    "the first diff came before any fetch started"
                );
                let paths: Vec<&str> = diff.files.iter().map(|f| f.path.as_ref()).collect();
                assert_eq!(paths, ["old.txt", "src/lib.rs", "src/new.rs"]);
                assert_eq!(diff.head.to_string(), head);
                let lib = &diff.files[1];
                assert_eq!((lib.added, lib.removed), (2, 1));
                first_paint = Some(started.elapsed());
                seen.push("diff");
            }
            OpenUpdate::Status(OpenStatus::Fetching(_)) => seen.push("fetching"),
            OpenUpdate::Status(OpenStatus::Stale(_)) => fetch_failed = true,
            OpenUpdate::Status(OpenStatus::Failed(error)) => panic!("open failed: {error}"),
            _ => {}
        }
    }
    let first_paint = first_paint.expect("the diff painted from local objects");
    assert!(
        fetch_failed,
        "the fetch from the missing remote failed after the paint"
    );
    assert_eq!(
        seen[0], "cached snapshot",
        "the cache is read first: {seen:?}"
    );
    assert!(
        calls.load(Ordering::SeqCst) >= 1,
        "the forge was asked, and its answer was not needed"
    );
    eprintln!("first paint from cache and local objects: {first_paint:?}");
    let _ = std::fs::remove_dir_all(&root);
}
