set windows-shell := ["powershell", "-NoProfile", "-Command"]

# Linux distributions ship python3 with no `python` alias.
python := if os_family() == "windows" { "python" } else { "python3" }

# List the recipes.
default:
    @just --list

# Build everything (debug).
build:
    cargo build --workspace

# Run the app.
run:
    cargo run -p app --bin review

# Run the app in release.
run-release:
    cargo run -p app --bin review --release

# Unit and integration tests.
test:
    cargo test --workspace

# Format check, clippy and the theme-literal lint for the whole workspace.
check:
    cargo fmt --all --check
    cargo clippy --workspace --all-targets -- -D warnings
    {{python}} scripts/lint_literals.py

# Format check and clippy for one crate.
check-crate crate:
    cargo fmt -p {{crate}} --check
    cargo clippy -p {{crate}} --all-targets -- -D warnings

# Format everything.
fmt:
    cargo fmt --all

# PNG of a running process's main window (exe name without .exe).
shot process out:
    powershell -NoProfile -File scripts/shot.ps1 -Process {{process}} -Out {{out}}

# Timing benchmarks in release builds; they open windows. `--only NAME` runs one.
bench *args:
    {{python}} scripts/checks.py bench {{args}}

# Checks against the forge sandboxes (docs/sandboxes.md). `--only NAME` runs one.
live *args:
    {{python}} scripts/checks.py live {{args}}

# Size of the build output; run `just clean-debug` when it grows large.
[windows]
target-size:
    powershell -NoProfile -Command "'{0:N1} GB' -f ((Get-ChildItem target -Recurse -File -ErrorAction SilentlyContinue | Measure-Object Length -Sum).Sum / 1GB)"

# Size of the build output; run `just clean-debug` when it grows large.
[unix]
target-size:
    du -sh target

# Drop debug build output (regenerable); release builds stay.
clean-debug:
    cargo clean --profile dev
