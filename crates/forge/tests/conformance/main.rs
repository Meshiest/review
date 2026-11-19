//! Conformance tests: each backend against recorded sandbox responses.
//! Record with `REVIEW_RECORD=1`; plain runs replay and never touch the network.

#[path = "../common/mod.rs"]
mod common;

#[path = "../../examples/round_trip/steps.rs"]
#[allow(dead_code, reason = "the example uses the rest")]
mod round_trip_steps;

#[path = "../../examples/ci_live/steps.rs"]
#[allow(dead_code, reason = "the example uses the rest")]
mod ci_steps;

#[path = "../../examples/lifecycle/steps.rs"]
#[allow(dead_code, reason = "the example uses the rest")]
mod lifecycle_steps;

#[path = "../../examples/people/steps.rs"]
#[allow(dead_code, reason = "the example uses the rest")]
mod people_steps;

mod ci;
mod forgejo;
mod forgejo_writes;
mod github;
mod github_writes;
mod gitlab;
mod gitlab_writes;
mod lifecycle;
mod people;
mod review_state;
mod stacks;
