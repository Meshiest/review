//! The commit walk against `git log`: a range's commits, their order,
//! parents, titles, authors and dates must be what git prints, on a history
//! with merges, branches of equal dates and author dates older than their
//! commit dates, and on this repository's own history. A commit's file list
//! must be git's diff from its first parent.

mod support;

use std::io::Write as _;
use std::path::Path;
use std::process::{Command, Stdio};

use repo::{ChangeKind, CommitInfo, FileListOptions, ObjectId, Store};
use support::{git, oracle_dir, workspace_root};

const EMPTY_TREE: &str = "4b825dc642cb6eb9a060e54bf8d69288fbee4904";

/// One commit of the synthetic history.
struct Spec {
    mark: u32,
    branch: &'static str,
    /// Marks of the parents, first parent first.
    parents: &'static [u32],
    author: &'static str,
    authored: u64,
    committed: u64,
    message: &'static str,
    file: &'static str,
}

/// A commit authored by Ada when it was committed, at `time`.
const fn spec(
    mark: u32,
    branch: &'static str,
    parents: &'static [u32],
    time: u64,
    message: &'static str,
    file: &'static str,
) -> Spec {
    Spec {
        mark,
        branch,
        parents,
        author: "Ada Lovelace",
        authored: time,
        committed: time,
        message,
        file,
    }
}

/// `main` with three commits; `feature` off its second, with `main` merged
/// in, a side branch merged back, two branches of the same date and a
/// commit authored long before it was committed.
#[rustfmt::skip]
fn history() -> Vec<Spec> {
    vec![
        spec(1, "main", &[], 1000, "Start\n", "a.txt"),
        spec(2, "main", &[1], 1100, "Second on main\n", "a.txt"),
        spec(3, "feature", &[2], 1200, "Feature one\n", "f.txt"),
        spec(4, "feature", &[3], 1300, "Feature two\n\nWith a body.\n", "f.txt"),
        spec(5, "main", &[2], 1250, "Third on main\n", "b.txt"),
        spec(6, "feature", &[4, 5], 1400, "Merge main into feature\n", "m.txt"),
        Spec {
            author: "Grace Hopper",
            authored: 900,
            ..spec(7, "feature", &[6], 1500, "Old idea\r\nsecond line\n", "g.txt")
        },
        spec(8, "side", &[3], 1350, "Side work\n", "s.txt"),
        spec(9, "feature", &[7, 8], 1600, "Merge side\n", "m.txt"),
        spec(10, "tie", &[9], 1700, "Tie one\n", "t1.txt"),
        spec(11, "feature", &[9], 1700, "Tie two\n", "t2.txt"),
        spec(12, "feature", &[11, 10], 1800, "\n\nMerge tie\n", "m.txt"),
    ]
}

/// Builds `history` into a bare repository at `dir` with `git fast-import`.
fn build(dir: &Path, history: &[Spec]) {
    if dir.exists() {
        std::fs::remove_dir_all(dir).expect("remove stale repository");
    }
    std::fs::create_dir_all(dir).expect("create repository dir");
    git(dir, &["init", "--bare", "--quiet", "--initial-branch=main"]);
    let mut stream = Vec::new();
    for commit in history {
        let Spec {
            mark,
            branch,
            author,
            authored,
            committed,
            message,
            file,
            ..
        } = commit;
        write!(
            stream,
            "commit refs/heads/{branch}\nmark :{mark}\n\
             author {author} <a@example.com> {authored} +0000\n\
             committer C <c@example.com> {committed} +0000\ndata {}\n{message}\n",
            message.len()
        )
        .unwrap();
        for (ix, parent) in commit.parents.iter().enumerate() {
            let verb = if ix == 0 { "from" } else { "merge" };
            writeln!(stream, "{verb} :{parent}").unwrap();
        }
        let content = format!("commit {mark}\n");
        write!(
            stream,
            "M 100644 inline {file}\ndata {}\n{content}\n\n",
            content.len()
        )
        .unwrap();
    }
    stream.extend_from_slice(b"done\n");
    let mut child = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(["fast-import", "--quiet", "--done"])
        .stdin(Stdio::piped())
        .spawn()
        .expect("git fast-import runs");
    child
        .stdin
        .take()
        .expect("stdin is piped")
        .write_all(&stream)
        .expect("write stream");
    assert!(child.wait().expect("wait").success(), "fast-import failed");
}

/// What `git log base..head` says, reduced to what `CommitInfo` holds.
fn git_log(dir: &Path, base: &str, head: &str) -> Vec<CommitInfo> {
    let range = format!("{base}..{head}");
    let out = git(
        dir,
        &["log", "--format=%H%x1f%P%x1f%an%x1f%at%x1f%B%x1e", &range],
    );
    out.split('\x1e')
        .map(|record| record.trim_start_matches('\n'))
        .filter(|record| !record.is_empty())
        .map(|record| {
            let fields: Vec<&str> = record.splitn(5, '\x1f').collect();
            let id = |hex: &str| ObjectId::from_hex(hex.as_bytes()).expect("hex id");
            CommitInfo {
                id: id(fields[0]),
                parents: fields[1].split_whitespace().map(id).collect(),
                author: fields[2].to_owned(),
                authored: fields[3].parse().expect("seconds"),
                title: fields[4]
                    .lines()
                    .map(|line| line.trim_end_matches('\r'))
                    .find(|line| !line.trim().is_empty())
                    .unwrap_or_default()
                    .to_owned(),
            }
        })
        .collect()
}

fn rev(dir: &Path, name: &str) -> ObjectId {
    ObjectId::from_hex(git(dir, &["rev-parse", name]).as_bytes()).expect("hex id")
}

/// Compares the walk with `git log` for each `(base, head)`, and returns how
/// many commits were compared.
fn check_ranges(dir: &Path, ranges: &[(String, String)]) -> usize {
    let store = Store::open(dir).unwrap();
    let mut compared = 0;
    for (base, head) in ranges {
        let ours = store.commits(rev(dir, base), rev(dir, head)).unwrap();
        let theirs = git_log(dir, base, head);
        assert_eq!(ours, theirs, "{base}..{head} in {}", dir.display());
        compared += ours.len();
    }
    compared
}

#[test]
fn the_walk_matches_git_log_on_merges_and_ties() {
    let dir = oracle_dir().join("commits").join("synthetic.git");
    build(&dir, &history());
    let ranges: Vec<(String, String)> = [
        ("main", "feature"),
        ("main~1", "feature"),
        ("main~2", "feature"),
        ("side", "feature"),
        ("tie", "feature"),
        ("feature~1", "feature"),
        ("feature", "main"),
        ("feature", "feature"),
    ]
    .map(|(base, head)| (base.to_owned(), head.to_owned()))
    .into();
    let compared = check_ranges(&dir, &ranges);
    println!(
        "COMMITS synthetic ranges={} commits={compared}",
        ranges.len()
    );

    // The pull request's range: the merge base of base and head, to head.
    let store = Store::open(&dir).unwrap();
    let (main, feature) = (rev(&dir, "main"), rev(&dir, "feature"));
    let base = store.merge_base(main, feature).unwrap();
    let commits = store.commits(base, feature).unwrap();
    let titles: Vec<&str> = commits.iter().map(|c| c.title.as_str()).collect();
    assert_eq!(
        titles,
        [
            "Merge tie",
            "Tie two",
            "Tie one",
            "Merge side",
            "Old idea",
            "Merge main into feature",
            "Side work",
            "Feature two",
            "Feature one"
        ]
    );
    assert_eq!(commits.iter().filter(|c| c.is_merge()).count(), 3);
    let old = &commits[4];
    assert_eq!((old.author.as_str(), old.authored), ("Grace Hopper", 900));
}

#[test]
fn the_walk_matches_git_log_on_this_repository() {
    let root = workspace_root();
    let shas: Vec<String> = git(&root, &["rev-list", "--max-count=60", "HEAD"])
        .lines()
        .map(str::to_owned)
        .collect();
    let head = shas.first().expect("a commit").clone();
    let last = shas.len() - 1;
    let ranges: Vec<(String, String)> = [1, 5, last / 2, last]
        .into_iter()
        .filter_map(|back| shas.get(back))
        .map(|base| (base.clone(), head.clone()))
        .chain(shas.get(last / 2).map(|mid| (head.clone(), mid.clone())))
        .collect();
    let compared = check_ranges(&root, &ranges);
    println!("COMMITS own ranges={} commits={compared}", ranges.len());
}

#[test]
fn a_commit_diffs_against_its_first_parent() {
    let dir = oracle_dir().join("commits").join("diffs.git");
    build(&dir, &history());
    let store = Store::open(&dir).unwrap();
    let options = FileListOptions {
        renames: false,
        detect_binary: false,
    };
    let all = git(&dir, &["rev-list", "--all"]);
    for sha in all.lines() {
        let id = ObjectId::from_hex(sha.as_bytes()).unwrap();
        let list = store.commit_file_list(id, options).unwrap();
        let parent = git(&dir, &["rev-list", "--parents", "-n", "1", sha])
            .split_whitespace()
            .nth(1)
            .unwrap_or(EMPTY_TREE)
            .to_owned();
        let theirs = git(
            &dir,
            &["diff", "--no-renames", "--name-status", &parent, sha],
        );
        let ours: Vec<String> = list
            .files
            .iter()
            .map(|file| {
                let status = match file.kind {
                    ChangeKind::Added => "A",
                    ChangeKind::Deleted => "D",
                    ChangeKind::Modified => "M",
                    ChangeKind::Renamed => "R",
                };
                format!("{status}\t{}", file.path)
            })
            .collect();
        assert_eq!(ours.join("\n"), theirs, "commit {sha}");
        let expected_parent = (parent != EMPTY_TREE).then(|| rev(&dir, &parent));
        assert_eq!(list.old_commit, expected_parent, "commit {sha}");
    }
}
