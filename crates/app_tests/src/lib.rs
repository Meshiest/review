//! Headless UI tests of the `review` shell, on gpui's test platform.
//!
//! The tests and their harness live in `tests/ui/`, one test binary. This
//! library is empty on purpose: the crate's dependencies on gpui's
//! `test-support` feature are dev-dependencies, so they apply to builds of
//! these tests only and never to `app`'s binary or benchmarks. See
//! `docs/testing.md`.
