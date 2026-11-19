//! The lines a suggestion would replace are read from the head commit in the
//! object store, not from the diff on screen.

use std::path::{Path, PathBuf};
use std::process::Command;

use app::suggestions::read_current_lines;
use repo::{LineEdit, Store};

const NUMBER: u64 = 7;

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

fn edit(path: &str, first: u32, last: u32) -> LineEdit {
    LineEdit {
        path: path.to_owned(),
        first,
        last,
        lines: vec!["new".to_owned()],
    }
}

#[test]
fn lines_come_from_the_blob_at_the_head() {
    let root = PathBuf::from(env!("CARGO_TARGET_TMPDIR"))
        .join(format!("current_lines_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).unwrap();
    git(&root, &["init", "-q", "-b", "main"]);
    std::fs::write(root.join("a.txt"), "one\ntwo\nthree\n").unwrap();
    std::fs::write(root.join("keep.txt"), "same\n").unwrap();
    std::fs::write(root.join("bin.dat"), [0xff, 0xfe, b'\n']).unwrap();
    git(&root, &["add", "-A"]);
    git(&root, &["commit", "-q", "-m", "base"]);
    let base = git(&root, &["rev-parse", "HEAD"]);
    std::fs::write(root.join("a.txt"), "one\nTWO\nthree  \nfour\n").unwrap();
    std::fs::write(root.join("bin.dat"), [0xff, 0xfe, b'\n', b'x']).unwrap();
    git(&root, &["add", "-A"]);
    git(&root, &["commit", "-q", "-m", "change"]);
    let head = git(&root, &["rev-parse", "HEAD"]);
    git(&root, &["update-ref", &repo::pr_head_ref(NUMBER), &head]);
    git(&root, &["update-ref", &repo::pr_base_ref(NUMBER), &base]);
    // The working tree says something else; it must not matter.
    std::fs::write(root.join("a.txt"), "edited\nafter\nthe\nfact\n").unwrap();

    let store = Store::open(&root).unwrap();
    let edits = [
        edit("a.txt", 2, 3),
        edit("a.txt", 4, 4),
        edit("a.txt", 4, 5),
        edit("a.txt", 0, 1),
        edit("keep.txt", 1, 1),
        edit("missing.txt", 1, 1),
        edit("bin.dat", 1, 1),
    ];
    let read = read_current_lines(&store, NUMBER, &head, &edits);
    let lines = |lines: &[&str]| Some(lines.iter().map(|&l| l.to_owned()).collect::<Vec<_>>());
    assert_eq!(
        read,
        [
            lines(&["TWO", "three  "]),
            lines(&["four"]),
            None, // past the end
            None, // line 0
            None, // not a file this pull request changes
            None, // not in the tree
            None, // not text
        ]
    );

    // A head that is not in the store reads nothing.
    let unknown = read_current_lines(&store, NUMBER, &"0".repeat(40), &edits[..1]);
    assert_eq!(unknown, [None]);
}
