//! Helpers shared by the integration tests: running git, and building
//! repositories from scratch with `git fast-import` (no working tree, exact
//! modes, so the same bytes come out on every machine).

#![allow(dead_code)]

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

pub fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .expect("crate sits two levels below the workspace")
        .to_owned()
}

pub fn oracle_dir() -> PathBuf {
    workspace_root().join(".scratch").join("oracle")
}

pub fn git_bytes(dir: &Path, args: &[&str]) -> Vec<u8> {
    let output = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .env("GIT_TERMINAL_PROMPT", "0")
        .stdin(Stdio::null())
        .output()
        .expect("git runs");
    assert!(
        output.status.success(),
        "git {args:?} in {} failed: {}",
        dir.display(),
        String::from_utf8_lossy(&output.stderr)
    );
    output.stdout
}

pub fn git(dir: &Path, args: &[&str]) -> String {
    String::from_utf8(git_bytes(dir, args))
        .expect("utf-8 output")
        .trim_end()
        .to_owned()
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    File,
    Exec,
    Link,
}

impl Mode {
    fn octal(self) -> &'static str {
        match self {
            Self::File => "100644",
            Self::Exec => "100755",
            Self::Link => "120000",
        }
    }
}

pub enum Op {
    Put(String, Mode, Vec<u8>),
    Gitlink(String, String),
    Del(String),
}

pub fn put(path: &str, content: impl Into<Vec<u8>>) -> Op {
    Op::Put(path.to_owned(), Mode::File, content.into())
}

pub fn put_mode(path: &str, mode: Mode, content: impl Into<Vec<u8>>) -> Op {
    Op::Put(path.to_owned(), mode, content.into())
}

pub fn del(path: &str) -> Op {
    Op::Del(path.to_owned())
}

/// Creates a bare repository at `dir` (replacing what was there) with one
/// commit per entry of `commits` on `refs/heads/main`. Returns the commit
/// ids, oldest first.
pub fn build_repo(dir: &Path, commits: &[Vec<Op>]) -> Vec<String> {
    if dir.exists() {
        std::fs::remove_dir_all(dir).expect("remove stale repository");
    }
    std::fs::create_dir_all(dir).expect("create repository dir");
    git(dir, &["init", "--bare", "--quiet", "--initial-branch=main"]);

    let mut stream = Vec::new();
    for (ix, ops) in commits.iter().enumerate() {
        let message = format!("commit {ix}");
        let stamp = 1_700_000_000 + ix * 60;
        write!(
            stream,
            "commit refs/heads/main\nauthor T <t@example.com> {stamp} +0000\n\
             committer T <t@example.com> {stamp} +0000\ndata {}\n{message}\n",
            message.len()
        )
        .unwrap();
        for op in ops {
            match op {
                Op::Put(path, mode, bytes) => {
                    write!(
                        stream,
                        "M {} inline {path}\ndata {}\n",
                        mode.octal(),
                        bytes.len()
                    )
                    .unwrap();
                    stream.extend_from_slice(bytes);
                    stream.push(b'\n');
                }
                Op::Gitlink(path, sha) => {
                    writeln!(stream, "M 160000 {sha} {path}").unwrap();
                }
                Op::Del(path) => writeln!(stream, "D {path}").unwrap(),
            }
        }
        stream.push(b'\n');
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
    git(dir, &["rev-list", "--reverse", "refs/heads/main"])
        .lines()
        .map(str::to_owned)
        .collect()
}

pub struct Rng(u64);

impl Rng {
    pub fn new(seed: u64) -> Self {
        Self(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1)
    }

    pub fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }

    pub fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }

    pub fn chance(&mut self, percent: u64) -> bool {
        self.next() % 100 < percent
    }
}
