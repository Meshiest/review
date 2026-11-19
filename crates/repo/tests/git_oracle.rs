//! The diff oracle: for 200 commit pairs, `repo` must produce what
//! `git diff --no-color -U3 --no-renames <old> <new>` prints, file by file
//! and hunk by hunk (header and every line, including `\ No newline` marks).

mod support;

use std::collections::BTreeSet;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Instant;

use repo::{ChangeKind, ContentKind, FileChange, FileListOptions, FileMode, ObjectId, Store};
use support::{
    Mode, Op, Rng, build_repo, del, git, git_bytes, oracle_dir, put, put_mode, workspace_root,
};

const PAIRS: usize = 200;
const RIPGREP_URL: &str = "https://github.com/BurntSushi/ripgrep";
const RIPGREP_TAG: &str = "14.1.1";
/// Bump when `synthetic_commits` changes so a stale repository is rebuilt.
const SYNTHETIC_VERSION: &str = "3";
const EMPTY_TREE: &str = "4b825dc642cb6eb9a060e54bf8d69288fbee4904";

#[derive(Clone, Debug)]
struct Pair {
    label: String,
    repo: PathBuf,
    old: Option<String>,
    new: String,
}

/// What both sides are reduced to before comparing.
#[derive(Clone, Debug, PartialEq, Eq)]
struct FileDump {
    path: String,
    status: &'static str,
    old_mode: Option<u32>,
    new_mode: Option<u32>,
    binary: bool,
    hunks: Vec<HunkDump>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct HunkDump {
    header: String,
    body: Vec<u8>,
}

#[derive(Default)]
struct Summary {
    pairs: usize,
    files: usize,
    hunks: usize,
    binary: usize,
    mode_changes: usize,
    no_trailing_newline: usize,
    crlf: usize,
    empty: usize,
    submodules: usize,
    symlinks: usize,
    differences: Vec<String>,
    difference_count: usize,
}

#[test]
fn git_oracle() {
    let mut pairs = Vec::new();
    let mut notes = Vec::new();

    let own = own_history_pairs();
    let synthetic = synthetic_pairs();
    let remaining = PAIRS.saturating_sub(own.len() + synthetic.len());
    match ripgrep_pairs(remaining) {
        Ok(rg) => pairs.extend(rg),
        Err(reason) => {
            assert!(
                std::env::var_os("REVIEW_ORACLE_REQUIRE_RIPGREP").is_none(),
                "ripgrep fixture required but unavailable: {reason}"
            );
            notes.push(format!(
                "SKIPPED the ripgrep pairs ({reason}); set REVIEW_ORACLE_REQUIRE_RIPGREP=1 to fail instead"
            ));
        }
    }
    pairs.extend(own);
    pairs.extend(synthetic);

    let mut summary = Summary::default();
    for pair in &pairs {
        check_pair(pair, &mut summary);
    }

    println!(
        "ORACLE pairs={} files={} hunks={} differences={} (binary={} mode_changes={} no_trailing_newline={} crlf={} empty={} submodules={} symlinks={})",
        summary.pairs,
        summary.files,
        summary.hunks,
        summary.difference_count,
        summary.binary,
        summary.mode_changes,
        summary.no_trailing_newline,
        summary.crlf,
        summary.empty,
        summary.submodules,
        summary.symlinks,
    );
    for note in &notes {
        println!("ORACLE {note}");
    }
    for difference in summary.differences.iter().take(500) {
        println!("ORACLE difference: {difference}");
    }
    assert_eq!(
        summary.difference_count, 0,
        "repo disagrees with git diff; see the ORACLE lines above"
    );
    assert!(summary.pairs > 0, "no oracle pairs ran");
}

/// Times the file list of a 2,600-file pair in the three configurations the
/// app uses. Run with
/// `cargo test -p repo --release --test git_oracle bench -- --ignored --nocapture`.
#[test]
#[ignore = "timing benchmark; run in release under the bench lock"]
fn bench_file_list_2000() {
    let dir = oracle_dir().join("bench2000");
    let (old, new) = bench_commits(&dir);

    // (metric, options, p95 budget in ms): the first result is a tree diff
    // with binary detection; rename pairing follows in the background.
    let configs = [
        (
            "repo.file_list_2000.first_result",
            FileListOptions {
                renames: false,
                detect_binary: true,
            },
            150.0,
        ),
        (
            "repo.file_list_2000.tree_only",
            FileListOptions {
                renames: false,
                detect_binary: false,
            },
            150.0,
        ),
        (
            "repo.file_list_2000.with_renames",
            FileListOptions {
                renames: true,
                detect_binary: true,
            },
            500.0,
        ),
    ];
    let mut samples: Vec<perf::Samples> =
        configs.iter().map(|_| perf::Samples::default()).collect();
    let mut files = 0;
    for round in 0..21 {
        let store = Store::open(&dir).unwrap();
        for ((_, options, _), samples) in configs.iter().zip(&mut samples) {
            let started = Instant::now();
            let list = store.file_list(Some(old), new, *options).unwrap();
            let elapsed = started.elapsed();
            files = list.files.len();
            // The first round warms the OS file cache.
            if round > 0 {
                samples.push(elapsed);
            }
        }
    }
    assert!(files >= 2000, "bench pair has only {files} files");
    for ((metric, _, budget), samples) in configs.iter().zip(&samples) {
        samples
            .report(metric, 95.0, *budget)
            .with("files", files)
            .print();
    }
}

fn bench_commits(dir: &Path) -> (ObjectId, ObjectId) {
    let marker = dir.join("bench-version");
    let shas = if std::fs::read_to_string(&marker).is_ok_and(|v| v == "1") {
        git(dir, &["rev-list", "--reverse", "refs/heads/main"])
            .lines()
            .map(str::to_owned)
            .collect()
    } else {
        let path = |i: usize| format!("pkg{:02}/mod{:03}/file{i}.rs", i % 40, i % 97);
        let body = |i: usize, rev: usize| {
            (0..30)
                .map(|l| format!("fn f{i}_{l}() -> usize {{ {} }}\n", l * i + rev))
                .collect::<String>()
        };
        let first: Vec<Op> = (0..2600).map(|i| put(&path(i), body(i, 0))).collect();
        let mut second: Vec<Op> = (0..2200).map(|i| put(&path(i), body(i, 1))).collect();
        second.extend((2200..2400).map(|i| del(&path(i))));
        second.extend((2600..2800).map(|i| put(&path(i), body(i, 0))));
        let shas = build_repo(dir, &[first, second]);
        std::fs::write(&marker, "1").unwrap();
        shas
    };
    (shas[0].parse().unwrap(), shas[1].parse().unwrap())
}

fn check_pair(pair: &Pair, summary: &mut Summary) {
    summary.pairs += 1;
    let expected = git_dump(pair);
    let actual = match own_dump(pair) {
        Ok(actual) => actual,
        Err(error) => {
            summary.record(format!("{}: repo failed: {error}", pair.label));
            return;
        }
    };

    for file in &expected {
        summary.files += 1;
        summary.hunks += file.hunks.len();
        summary.binary += usize::from(file.binary);
        summary.mode_changes += usize::from(
            file.old_mode.is_some() && file.new_mode.is_some() && file.old_mode != file.new_mode,
        );
        let bodies = file.hunks.iter().map(|h| &h.body);
        summary.no_trailing_newline += usize::from(
            bodies
                .clone()
                .any(|b| find(b, b"\\ No newline at end of file")),
        );
        summary.crlf += usize::from(bodies.clone().any(|b| find(b, b"\r\n")));
        let has_mode = |mode: u32| file.old_mode == Some(mode) || file.new_mode == Some(mode);
        summary.submodules += usize::from(has_mode(0o160_000));
        summary.symlinks += usize::from(has_mode(0o120_000));
        summary.empty +=
            usize::from(file.hunks.is_empty() && !file.binary && file.status != "modified");
    }

    if expected.len() != actual.len() {
        let paths = |dump: &[FileDump]| {
            dump.iter()
                .map(|f| format!("{}:{}", f.status, f.path))
                .collect::<Vec<_>>()
        };
        summary.record(format!(
            "{}: git lists {} files, repo {}\n  git:  {:?}\n  repo: {:?}",
            pair.label,
            expected.len(),
            actual.len(),
            paths(&expected),
            paths(&actual)
        ));
        return;
    }
    for (git_file, our_file) in expected.iter().zip(&actual) {
        if git_file != our_file {
            summary.record(format!(
                "{}: {}",
                pair.label,
                describe_difference(git_file, our_file)
            ));
        }
    }
}

impl Summary {
    fn record(&mut self, difference: String) {
        self.difference_count += 1;
        if self.differences.len() < 500 {
            self.differences.push(difference);
        }
    }
}

fn find(haystack: &[u8], needle: &[u8]) -> bool {
    haystack.windows(needle.len()).any(|w| w == needle)
}

fn describe_difference(git_file: &FileDump, our_file: &FileDump) -> String {
    let mut text = format!("file {} ({}) differs", git_file.path, git_file.status);
    if git_file.path != our_file.path || git_file.status != our_file.status {
        let _ = write!(text, "; repo has {} ({})", our_file.path, our_file.status);
    }
    if (git_file.old_mode, git_file.new_mode) != (our_file.old_mode, our_file.new_mode) {
        let _ = write!(
            text,
            "; modes git {:?}->{:?} repo {:?}->{:?}",
            git_file.old_mode, git_file.new_mode, our_file.old_mode, our_file.new_mode
        );
    }
    if git_file.binary != our_file.binary {
        let _ = write!(
            text,
            "; binary git {} repo {}",
            git_file.binary, our_file.binary
        );
    }
    if git_file.hunks.len() != our_file.hunks.len() {
        let _ = write!(
            text,
            "; hunks git {} repo {}",
            git_file.hunks.len(),
            our_file.hunks.len()
        );
    }
    if let Some((g, o)) = git_file
        .hunks
        .iter()
        .zip(&our_file.hunks)
        .find(|(g, o)| g != o)
    {
        let _ = write!(
            text,
            "\n  first differing hunk: git `{}` repo `{}`",
            g.header, o.header
        );
        if g.body != o.body {
            let split = |b: &[u8]| -> Vec<String> {
                b.split(|&c| c == b'\n')
                    .map(|l| String::from_utf8_lossy(l).into_owned())
                    .collect()
            };
            let (gl, ol) = (split(&g.body), split(&o.body));
            let at = gl
                .iter()
                .zip(&ol)
                .position(|(a, b)| a != b)
                .unwrap_or(gl.len().min(ol.len()));
            let from = at.saturating_sub(2);
            let show = |lines: &[String]| lines[from..lines.len().min(at + 6)].join("\n");
            let _ = write!(
                text,
                "\n  bodies differ at line {at}\n  git:\n{}\n  repo:\n{}",
                show(&gl),
                show(&ol)
            );
        }
    }
    text
}

// ---- git side ----------------------------------------------------------

fn git_dump(pair: &Pair) -> Vec<FileDump> {
    let old = pair.old.as_deref().unwrap_or(EMPTY_TREE);
    let output = git_bytes(
        &pair.repo,
        &[
            "-c",
            "core.quotepath=off",
            "-c",
            "diff.algorithm=myers",
            "-c",
            "diff.indentHeuristic=true",
            "-c",
            "diff.mnemonicPrefix=false",
            "-c",
            "diff.noprefix=false",
            "-c",
            "diff.renames=false",
            "diff",
            "--no-color",
            "-U3",
            "--no-renames",
            "--no-ext-diff",
            "--no-textconv",
            "--submodule=short",
            "--src-prefix=a/",
            "--dst-prefix=b/",
            old,
            &pair.new,
        ],
    );
    parse_git_diff(&output)
}

fn parse_git_diff(output: &[u8]) -> Vec<FileDump> {
    let mut lines: Vec<&[u8]> = output.split(|&b| b == b'\n').collect();
    if lines.last().is_some_and(|l| l.is_empty()) {
        lines.pop();
    }
    let mut files = Vec::new();
    let mut ix = 0;
    while ix < lines.len() {
        let line = lines[ix];
        let rest = line.strip_prefix(b"diff --git ").unwrap_or_else(|| {
            panic!(
                "unexpected git output line: {}",
                String::from_utf8_lossy(line)
            )
        });
        let mut file = FileDump {
            path: header_path(rest),
            status: "modified",
            old_mode: None,
            new_mode: None,
            binary: false,
            hunks: Vec::new(),
        };
        ix += 1;
        while ix < lines.len() && !lines[ix].starts_with(b"diff --git ") {
            let line = lines[ix];
            if line.starts_with(b"@@ ") {
                ix = parse_hunk(&lines, ix, &mut file);
                continue;
            }
            let text = String::from_utf8_lossy(line);
            if let Some(mode) = text.strip_prefix("new file mode ") {
                file.status = "added";
                file.new_mode = u32::from_str_radix(mode, 8).ok();
            } else if let Some(mode) = text.strip_prefix("deleted file mode ") {
                file.status = "deleted";
                file.old_mode = u32::from_str_radix(mode, 8).ok();
            } else if let Some(mode) = text.strip_prefix("old mode ") {
                file.old_mode = u32::from_str_radix(mode, 8).ok();
            } else if let Some(mode) = text.strip_prefix("new mode ") {
                file.new_mode = u32::from_str_radix(mode, 8).ok();
            } else if let Some(index) = text.strip_prefix("index ") {
                // `index a..b 100644` carries the mode only when it did not change.
                if let Some((_, mode)) = index.split_once(' ') {
                    let mode = u32::from_str_radix(mode, 8).ok();
                    file.old_mode = file.old_mode.or(mode);
                    file.new_mode = file.new_mode.or(mode);
                }
            } else if text.starts_with("Binary files ") {
                file.binary = true;
            } else if !(text.starts_with("--- ") || text.starts_with("+++ ")) {
                panic!("unexpected git header line: {text}");
            }
            ix += 1;
        }
        files.push(file);
    }
    files
}

/// `a/P b/P` (no renames, so both halves are equal), possibly C-quoted.
fn header_path(rest: &[u8]) -> String {
    if rest.starts_with(b"\"") {
        let end = rest[1..]
            .iter()
            .position(|&b| b == b'"')
            .expect("closing quote")
            + 1;
        return unquote(&rest[1..end])
            .strip_prefix("a/")
            .expect("a/ prefix")
            .to_owned();
    }
    let len = (rest.len() - 5) / 2;
    String::from_utf8_lossy(&rest[2..2 + len]).into_owned()
}

fn unquote(quoted: &[u8]) -> String {
    let mut out = Vec::new();
    let mut bytes = quoted.iter().copied();
    while let Some(b) = bytes.next() {
        if b != b'\\' {
            out.push(b);
            continue;
        }
        match bytes.next() {
            Some(b'n') => out.push(b'\n'),
            Some(b't') => out.push(b'\t'),
            Some(b'"') => out.push(b'"'),
            Some(b'\\') => out.push(b'\\'),
            Some(d @ b'0'..=b'7') => {
                let mut value = u32::from(d - b'0');
                for _ in 0..2 {
                    value = value * 8 + u32::from(bytes.next().unwrap() - b'0');
                }
                out.push(value as u8);
            }
            other => panic!("unsupported escape {other:?}"),
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Consumes one hunk starting at `lines[ix]`; returns the next line index.
fn parse_hunk(lines: &[&[u8]], mut ix: usize, file: &mut FileDump) -> usize {
    let header_line = String::from_utf8_lossy(lines[ix]).into_owned();
    let counts = |sign: char| -> u32 {
        let field = header_line
            .split_whitespace()
            .find(|f| f.starts_with(sign))
            .unwrap_or_else(|| panic!("bad hunk header {header_line}"));
        match field[1..].split_once(',') {
            Some((_, len)) => len.parse().unwrap(),
            None => 1,
        }
    };
    let (mut old_left, mut new_left) = (counts('-'), counts('+'));
    let header = match header_line[2..].find("@@") {
        Some(end) => header_line[..end + 4].to_owned(),
        None => panic!("bad hunk header {header_line}"),
    };
    ix += 1;
    let mut body = Vec::new();
    while old_left > 0 || new_left > 0 || lines.get(ix).is_some_and(|l| l.starts_with(b"\\")) {
        let line = lines[ix];
        match line.first() {
            Some(b' ') => {
                old_left -= 1;
                new_left -= 1;
            }
            Some(b'-') => old_left -= 1,
            Some(b'+') => new_left -= 1,
            Some(b'\\') => {}
            _ => panic!("unexpected hunk line {}", String::from_utf8_lossy(line)),
        }
        body.extend_from_slice(line);
        body.push(b'\n');
        ix += 1;
    }
    file.hunks.push(HunkDump { header, body });
    ix
}

// ---- repo side ---------------------------------------------------------

fn own_dump(pair: &Pair) -> Result<Vec<FileDump>, repo::Error> {
    let store = Store::open(&pair.repo)?;
    let parse = |sha: &str| sha.parse::<ObjectId>().expect("hex id");
    let list = store.file_list(
        pair.old.as_deref().map(parse),
        parse(&pair.new),
        FileListOptions {
            renames: false,
            detect_binary: true,
        },
    )?;
    list.files
        .iter()
        .map(|change| dump_change(&store, change))
        .collect()
}

fn dump_change(store: &Store, change: &FileChange) -> Result<FileDump, repo::Error> {
    let content = store.file_content(change)?;
    let binary = content.kind == ContentKind::Binary;
    assert_eq!(
        binary,
        change.binary == Some(true) && change.content_changed(),
        "file list and content disagree about {}",
        change.path
    );
    let hunks = content
        .hunks
        .iter()
        .map(|hunk| {
            let rendered = repo::render_hunk(&content.old, &content.new, hunk);
            let split = rendered.iter().position(|&b| b == b'\n').unwrap() + 1;
            HunkDump {
                header: String::from_utf8(rendered[..split - 1].to_vec()).unwrap(),
                body: rendered[split..].to_vec(),
            }
        })
        .collect();
    let mode = |entry: Option<repo::Entry>| {
        entry.map(|e| match e.mode {
            FileMode::Regular => 0o100_644,
            FileMode::Executable => 0o100_755,
            FileMode::Symlink => 0o120_000,
            FileMode::Submodule => 0o160_000,
        })
    };
    Ok(FileDump {
        path: change.path.clone(),
        status: match change.kind {
            ChangeKind::Added => "added",
            ChangeKind::Deleted => "deleted",
            ChangeKind::Modified => "modified",
            ChangeKind::Renamed => "renamed",
        },
        old_mode: mode(change.old),
        new_mode: mode(change.new),
        binary,
        hunks,
    })
}

// ---- fixtures ----------------------------------------------------------

fn own_history_pairs() -> Vec<Pair> {
    let root = workspace_root();
    let shas: Vec<String> = git(&root, &["rev-list", "HEAD"])
        .lines()
        .map(str::to_owned)
        .collect();
    let pair = |label: String, old: Option<&String>, new: &String| Pair {
        label,
        repo: root.clone(),
        old: old.cloned(),
        new: new.clone(),
    };
    let mut pairs = Vec::new();
    for (ix, sha) in shas.iter().enumerate().take(20) {
        let parent = shas.get(ix + 1);
        pairs.push(pair(
            format!("review {}~{ix}", short(&shas[0])),
            parent,
            sha,
        ));
    }
    // Wider ranges, ending at the oldest commit.
    for span in [3, 6, 10] {
        if let (Some(old), Some(new)) = (shas.get(span), shas.first()) {
            pairs.push(pair(format!("review wide {span}"), Some(old), new));
        }
    }
    if let Some(oldest) = shas.last() {
        pairs.push(pair("review root..HEAD".into(), None, &shas[0]));
        pairs.push(pair(format!("review root {}", short(oldest)), None, oldest));
    }
    dedupe(pairs)
}

fn short(sha: &str) -> &str {
    &sha[..8]
}

fn dedupe(pairs: Vec<Pair>) -> Vec<Pair> {
    let mut seen = BTreeSet::new();
    pairs
        .into_iter()
        .filter(|p| seen.insert((p.repo.clone(), p.old.clone(), p.new.clone())))
        .collect()
}

type Pick = fn(&Commit) -> bool;

#[derive(Default)]
struct Commit {
    sha: String,
    renamed: bool,
    mode_or_type_change: bool,
    binary: bool,
}

fn ripgrep_pairs(count: usize) -> Result<Vec<Pair>, String> {
    let dir = oracle_dir().join("ripgrep");
    if !dir.join("HEAD").is_file() {
        let temp = oracle_dir().join("ripgrep.tmp");
        let _ = std::fs::remove_dir_all(&temp);
        std::fs::create_dir_all(oracle_dir()).map_err(|e| e.to_string())?;
        let status = Command::new("git")
            .args(["clone", "--bare", "--quiet", RIPGREP_URL])
            .arg(&temp)
            .status()
            .map_err(|e| format!("cannot run git: {e}"))?;
        if !status.success() {
            return Err(format!("{} is absent and cloning it failed", dir.display()));
        }
        std::fs::rename(&temp, &dir).map_err(|e| e.to_string())?;
    }

    // `--first-parent` keeps one parent per commit so `<sha>^` is the diff base.
    let log = git(
        &dir,
        &[
            "log",
            "--first-parent",
            "--format=%x01%H",
            "--raw",
            "--numstat",
            "-M",
            "--no-abbrev",
            RIPGREP_TAG,
        ],
    );
    let mut commits: Vec<Commit> = Vec::new();
    for line in log.lines() {
        if let Some(sha) = line.strip_prefix('\u{1}') {
            commits.push(Commit {
                sha: sha.to_owned(),
                ..Commit::default()
            });
            continue;
        }
        let Some(current) = commits.last_mut() else {
            continue;
        };
        if line.starts_with(':') {
            let fields: Vec<&str> = line.split_whitespace().collect();
            let status = fields.get(4).copied().unwrap_or("");
            current.renamed |= status.starts_with('R');
            current.mode_or_type_change |=
                status.starts_with('T') || (status == "M" && fields[0][1..] != *fields[1]);
        } else if line.starts_with("-\t-\t") {
            current.binary = true;
        }
    }

    let mut chosen: Vec<String> = Vec::new();
    let singles = count.saturating_sub(count / 8);
    let categories: [(usize, Pick); 4] = [
        (singles / 5, |c| c.renamed),
        (singles / 8, |c| c.mode_or_type_change),
        (singles / 6, |c| c.binary),
        (singles, |_| true),
    ];
    for (wanted, pick) in categories {
        let wanted = wanted.min(singles.saturating_sub(chosen.len()));
        let matching: Vec<_> = commits.iter().filter(|c| pick(c)).collect();
        let step = (matching.len() / wanted.max(1)).max(1);
        let fresh = matching
            .into_iter()
            .step_by(step)
            .filter(|c| !chosen.contains(&c.sha))
            .take(wanted)
            .map(|c| c.sha.clone())
            .collect::<Vec<_>>();
        chosen.extend(fresh);
    }

    let mut pairs: Vec<Pair> = chosen
        .iter()
        .map(|sha| Pair {
            label: format!("ripgrep {}", short(sha)),
            repo: dir.clone(),
            old: git_opt(&dir, &format!("{sha}^")),
            new: sha.clone(),
        })
        .collect();
    // Wider ranges between sampled commits.
    let wide = count - pairs.len();
    for (i, pair) in pairs.clone().iter().enumerate().take(wide) {
        let span = [20, 60, 150, 400][i % 4];
        if let Some(old) = git_opt(&dir, &format!("{}~{span}", pair.new)) {
            pairs.push(Pair {
                label: format!("ripgrep {}~{span}..{}", short(&old), short(&pair.new)),
                repo: dir.clone(),
                old: Some(old),
                new: pair.new.clone(),
            });
        }
    }
    // Wide ranges that fell off the start of history, or duplicates, leave a gap.
    for commit in commits.iter().skip(1).step_by(7) {
        if pairs.len() >= count {
            break;
        }
        pairs.push(Pair {
            label: format!("ripgrep {}", short(&commit.sha)),
            repo: dir.clone(),
            old: git_opt(&dir, &format!("{}^", commit.sha)),
            new: commit.sha.clone(),
        });
        pairs = dedupe(pairs);
    }
    pairs.truncate(count);
    Ok(dedupe(pairs))
}

fn git_opt(dir: &Path, rev: &str) -> Option<String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(["rev-parse", "--verify", "--quiet", rev])
        .output()
        .ok()?;
    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).trim().to_owned())
}

fn synthetic_pairs() -> Vec<Pair> {
    let dir = oracle_dir().join("synthetic");
    let marker = dir.join("synthetic-version");
    let shas: Vec<String> =
        if std::fs::read_to_string(&marker).is_ok_and(|v| v == SYNTHETIC_VERSION) {
            git(&dir, &["rev-list", "--reverse", "refs/heads/main"])
                .lines()
                .map(str::to_owned)
                .collect()
        } else {
            let shas = build_repo(&dir, &synthetic_commits());
            std::fs::write(&marker, SYNTHETIC_VERSION).unwrap();
            shas
        };
    let pair = |old: Option<usize>, new: usize| Pair {
        label: format!(
            "synthetic {}..{new}",
            old.map_or_else(|| "root".to_owned(), |o| o.to_string())
        ),
        repo: dir.clone(),
        old: old.map(|o| shas[o].clone()),
        new: shas[new].clone(),
    };
    let mut pairs = vec![pair(None, 0)];
    pairs.extend((1..shas.len()).map(|i| pair(Some(i - 1), i)));
    for (old, new) in [
        (0, 3),
        (0, 4),
        (1, 4),
        (4, 14),
        (4, 24),
        (14, 34),
        (0, shas.len() - 1),
    ] {
        pairs.push(pair(Some(old), new));
    }
    pairs.push(pair(None, shas.len() - 1));
    dedupe(pairs)
}

// ---- synthetic history -------------------------------------------------

fn numbered(count: usize, eol: &str) -> String {
    (1..=count).map(|i| format!("line {i}{eol}")).collect()
}

fn code_file(variant: usize) -> String {
    let mut text = String::new();
    for f in 0..6 {
        let _ = write!(
            text,
            "fn func_{variant}_{f}(x: i32) -> i32 {{\n    if x > {f} {{\n        return x * 2;\n    }}\n    x + 1\n}}\n\n"
        );
    }
    text
}

fn synthetic_commits() -> Vec<Vec<Op>> {
    let sub_a = "a".repeat(40);
    let sub_b = "b".repeat(40);
    let binary: Vec<u8> = (0..300u32).map(|i| (i * 7 % 251) as u8).collect();
    let mut binary_edited = binary.clone();
    binary_edited[150] ^= 0xff;

    let mut mixed = numbered(12, "\n");
    mixed.push_str("crlf tail\r\nmore\r\n");

    let mut commits: Vec<Vec<Op>> = Vec::new();
    commits.push(vec![
        put("lf.txt", numbered(40, "\n")),
        put("crlf.txt", numbered(30, "\r\n")),
        put("nonl.txt", "a\nb\nc"),
        put("empty.txt", ""),
        put("bin.dat", binary.clone()),
        put_mode("run.sh", Mode::Exec, "#!/bin/sh\necho hi\n"),
        put("src/main.rs", code_file(0)),
        put("src/lib.rs", code_file(1)),
        put("old_name.txt", numbered(40, "\n")),
        put("to_link.txt", "target\n"),
        Op::Gitlink("sub".into(), sub_a.clone()),
        put(
            "unicode.txt",
            "caf\u{e9}\n\u{65e5}\u{672c}\u{8a9e}\nsecond\n",
        ),
        put("latin1.txt", b"caf\xe9\nbad \xff byte\nend\n".to_vec()),
        put("mixed.txt", mixed.clone()),
        put("dir/gone/x.txt", "x\n"),
        put("just_newline.txt", "\n"),
        put("ws.txt", "a \n\tb\n  c\n"),
    ]);

    let mut lf_edit = numbered(40, "\n");
    for (old, new) in [
        ("line 5\n", "line five\n"),
        ("line 12\n", "line twelve\n"),
        ("line 19\n", "line nineteen\n"),
        ("line 27\n", "line twenty-seven\n"),
    ] {
        lf_edit = lf_edit.replacen(old, new, 1);
    }
    let mut renamed = numbered(40, "\n");
    renamed = renamed.replacen("line 3\n", "line three\n", 1);
    renamed = renamed.replacen("line 33\n", "line thirty-three\n", 1);
    commits.push(vec![
        put("lf.txt", lf_edit),
        put(
            "crlf.txt",
            numbered(30, "\r\n").replacen("line 3\r\n", "line three\r\n", 1),
        ),
        put("nonl.txt", "a\nb\nc\nd"),
        put("empty.txt", "now has content\n"),
        put("bin.dat", binary_edited),
        put_mode("run.sh", Mode::File, "#!/bin/sh\necho hi\n"),
        del("old_name.txt"),
        put("new_name.txt", renamed),
        del("dir/gone/x.txt"),
        put("new_empty.txt", ""),
        put("added_crlf.txt", numbered(5, "\r\n")),
        del("to_link.txt"),
        put_mode("to_link.txt", Mode::Link, "target"),
        Op::Gitlink("sub".into(), sub_b),
        put(
            "src/main.rs",
            format!(
                "{}{}",
                code_file(0),
                &code_file(0)[..code_file(0).len() / 2]
            ),
        ),
        put("mixed.txt", mixed.replace("line 4\n", "line four\n")),
        put("ws.txt", "a\n\tb\n  c\n"),
    ]);

    commits.push(vec![
        put("nonl.txt", "a\nb\nc\nd\n"),
        put(
            "crlf.txt",
            numbered(30, "\n").replacen("line 3\n", "line three\n", 1),
        ),
        put("lf.txt", numbered(40, "\r\n")),
        put("empty.txt", ""),
        del("to_link.txt"),
        put("to_link.txt", "regular again\n"),
        put_mode("run.sh", Mode::Exec, "#!/bin/sh\necho changed\n"),
        del("sub"),
        put("bin2.dat", binary),
        put(
            "unicode.txt",
            "caf\u{e9}\n\u{65e5}\u{672c}\n\u{30c6}\u{30b9}\u{30c8}\nsecond\n",
        ),
        put("latin1.txt", b"caf\xe9\nbad \xfe byte\nend\n".to_vec()),
        put("just_newline.txt", ""),
    ]);

    let big: String = (0..1000).map(|i| format!("row {i}\n")).collect();
    commits.push(vec![put("big.txt", big.clone())]);
    let edited: String = (0..1000)
        .map(|i| {
            if i % 5 == 0 {
                format!("row {i} edited\n")
            } else {
                format!("row {i}\n")
            }
        })
        .collect();
    commits.push(vec![
        put("big.txt", edited),
        put("one_line_no_nl.txt", "x"),
        put("first_last.txt", numbered(20, "\n")),
    ]);
    commits.push(vec![
        put(
            "first_last.txt",
            format!("new first\n{}new last\n", numbered(20, "\n")),
        ),
        put("one_line_no_nl.txt", ""),
        put(
            "big.txt",
            (0..1000).map(|i| format!("row {i}\n")).collect::<String>(),
        ),
    ]);

    // Random edits to three files whose shapes stress different parts of the
    // algorithm: a tiny vocabulary (many repeated lines), indented code
    // (slider placement) and mostly unique lines.
    let mut rng = Rng::new(0x5eed);
    let mut files = [
        RandomFile::new(&mut rng, Style::Repeats, 120, "\n"),
        RandomFile::new(&mut rng, Style::Code, 160, "\n"),
        RandomFile::new(&mut rng, Style::Unique, 200, "\r\n"),
    ];
    commits.push(files.iter().map(RandomFile::op).collect());
    for _ in 0..40 {
        for file in &mut files {
            if rng.chance(75) {
                file.mutate(&mut rng);
            }
        }
        commits.push(files.iter().map(RandomFile::op).collect());
    }
    commits
}

#[derive(Clone, Copy)]
enum Style {
    Repeats,
    Code,
    Unique,
}

struct RandomFile {
    style: Style,
    path: String,
    lines: Vec<String>,
    eol: &'static str,
    final_newline: bool,
    serial: usize,
}

impl RandomFile {
    fn new(rng: &mut Rng, style: Style, count: usize, eol: &'static str) -> Self {
        let path = match style {
            Style::Repeats => "rand/repeats.txt",
            Style::Code => "rand/code.rs",
            Style::Unique => "rand/unique.txt",
        };
        let mut file = Self {
            style,
            path: path.to_owned(),
            lines: Vec::new(),
            eol,
            final_newline: true,
            serial: 0,
        };
        file.lines = (0..count).map(|_| file.line(rng)).collect();
        file
    }

    fn line(&mut self, rng: &mut Rng) -> String {
        self.serial += 1;
        match self.style {
            Style::Repeats => ["a", "b", "c", "", "}", "a b"][rng.below(6)].to_owned(),
            Style::Code => {
                let indent = "    ".repeat(rng.below(4));
                let text = ["x();", "}", "{", "", "if y {", "return;", "} else {"][rng.below(7)];
                format!("{indent}{text}")
            }
            Style::Unique => format!("unique line {} {}", self.serial, rng.below(1000)),
        }
    }

    fn mutate(&mut self, rng: &mut Rng) {
        for _ in 0..=rng.below(5) {
            let len = self.lines.len();
            match rng.below(6) {
                0 => {
                    let at = rng.below(len + 1);
                    let line = self.line(rng);
                    self.lines.insert(at, line);
                }
                1 if len > 10 => {
                    self.lines.remove(rng.below(len));
                }
                2 if len > 10 => {
                    let at = rng.below(len);
                    self.lines[at] = self.line(rng);
                }
                3 if len > 20 => {
                    // Duplicate a block somewhere else: ambiguous slider positions.
                    let from = rng.below(len - 6);
                    let block: Vec<String> = self.lines[from..from + 1 + rng.below(5)].to_vec();
                    let at = rng.below(len + 1);
                    self.lines.splice(at..at, block);
                }
                4 if len > 20 => {
                    let from = rng.below(len - 6);
                    let end = from + 1 + rng.below(5);
                    self.lines.drain(from..end);
                }
                _ => self.final_newline ^= rng.chance(20),
            }
        }
    }

    fn op(&self) -> Op {
        let mut text = self.lines.join(self.eol);
        if self.final_newline && !self.lines.is_empty() {
            text.push_str(self.eol);
        }
        put(&self.path, text)
    }
}
