//! Every recorded fixture is checked for credentials, whatever recorded it:
//! no word starting with a known token prefix, no auth header or personal
//! address, no real avatar hash, and every key that names a credential holds
//! the scrubbed marker or nothing.

use std::path::{Path, PathBuf};

use forge::record::{TOKEN_PREFIXES, is_secret_key};
use serde_json::Value;

/// Text no recording may hold: an auth header or a personal address.
const PERSONAL: [&str; 3] = ["Bearer ", "@gmail.", "@users.noreply"];
const GRAVATAR: &str = "gravatar.com/avatar/";

fn fixtures(dir: &Path, out: &mut Vec<PathBuf>) {
    for entry in std::fs::read_dir(dir).expect("fixtures dir") {
        let path = entry.expect("entry").path();
        if path.is_dir() {
            fixtures(&path, out);
        } else if path.extension().is_some_and(|ext| ext == "json") {
            out.push(path);
        }
    }
}

fn secret_keys(value: &Value, path: &str, found: &mut Vec<String>) {
    match value {
        Value::Object(map) => {
            for (key, item) in map {
                let at = format!("{path}.{key}");
                if let Value::String(text) = item
                    && is_secret_key(key)
                    && !text.is_empty()
                    && text != "***"
                {
                    found.push(at.clone());
                }
                secret_keys(item, &at, found);
            }
        }
        Value::Array(items) => {
            for (i, item) in items.iter().enumerate() {
                secret_keys(item, &format!("{path}[{i}]"), found);
            }
        }
        _ => {}
    }
}

#[test]
fn recorded_fixtures_hold_no_credentials() {
    let mut files = Vec::new();
    fixtures(
        &Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures"),
        &mut files,
    );
    assert!(!files.is_empty(), "no fixtures found");
    let mut problems = Vec::new();
    for file in &files {
        let text = std::fs::read_to_string(file).expect("read fixture");
        for prefix in TOKEN_PREFIXES {
            for (at, _) in text.match_indices(prefix) {
                let word: String = text[at..]
                    .chars()
                    .take_while(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-'))
                    .collect();
                if word.len() >= prefix.len() + 16 {
                    problems.push(format!("{}: token-shaped {prefix}...", file.display()));
                }
            }
        }
        for needle in PERSONAL {
            if text.contains(needle) {
                problems.push(format!("{}: contains {needle:?}", file.display()));
            }
        }
        for (at, _) in text.match_indices(GRAVATAR) {
            let hash = text[at + GRAVATAR.len()..]
                .chars()
                .take_while(char::is_ascii_hexdigit);
            if hash.clone().any(|c| c != '0') {
                problems.push(format!("{}: real avatar hash", file.display()));
            }
        }
        let value: Value = serde_json::from_str(&text).expect("fixture is JSON");
        let mut found = Vec::new();
        secret_keys(&value, "", &mut found);
        problems.extend(
            found
                .into_iter()
                .map(|at| format!("{}: {at}", file.display())),
        );
    }
    assert!(
        problems.is_empty(),
        "credentials in fixtures:\n{}",
        problems.join("\n")
    );
}
