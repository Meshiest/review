//! The forge side of the lifecycle round trip: draft, merge and close on two
//! scratch pull requests, each read back through the same backend. Shared by
//! `examples/lifecycle` (live) and the replay tests, so a replay sends exactly
//! the requests that were recorded.
//!
//! The first pull request is drafted, made ready and squash-merged with the
//! expected head and `delete_branch`. The second is refused a merge on a stale
//! head, then closed, reopened and closed again.

use std::fmt;
use std::time::Duration;

use forge::model::{
    MergeBlocker, MergeMethod, MergeOptions, Mergeability, PrNumber, PrState, PullRequest, RepoRef,
    Sha,
};
use forge::{Capabilities, Forge};

/// How many times a state the forge computes in the background is read
/// before the step gives up.
const LOOKS: usize = 30;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Status {
    Pass(String),
    Fail(String),
    /// The capability the forge lacks, or why the step is not run.
    Skip(String),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Outcome {
    pub step: &'static str,
    pub status: Status,
}

impl Outcome {
    #[must_use]
    pub fn failed(&self) -> bool {
        matches!(self.status, Status::Fail(_))
    }
}

impl fmt::Display for Outcome {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.status {
            Status::Pass(detail) => write!(f, "PASS {}: {detail}", self.step),
            Status::Fail(reason) => write!(f, "FAIL {}: {reason}", self.step),
            Status::Skip(why) => write!(f, "SKIP {}: {why}", self.step),
        }
    }
}

type Step = Result<String, String>;

/// What the steps need from outside the forge.
pub struct Hooks<'a> {
    /// Whether the remote branch still exists, asked of git.
    pub branch_exists: &'a dyn Fn(&str) -> Result<bool, String>,
    /// The wait between two looks at something the forge finishes later;
    /// zero when replaying.
    pub settle: Duration,
}

fn ensure(ok: bool, message: impl FnOnce() -> String) -> Result<(), String> {
    if ok { Ok(()) } else { Err(message()) }
}

fn err(error: forge::Error) -> String {
    error.to_string()
}

/// The two scratch pull requests of a run.
#[derive(Clone, Copy, Debug)]
pub struct Scratch {
    /// Drafted, made ready and merged.
    pub merged: PrNumber,
    /// Closed, reopened and closed again; also the stale-head merge target.
    pub closed: PrNumber,
}

/// The commit text the merge asks for.
pub const MERGE_TITLE: &str = "Lifecycle scratch merge";
pub const MERGE_BODY: &str = "Scratch change of the lifecycle round trip.";

pub async fn run(
    forge: &dyn Forge,
    repo: &RepoRef,
    scratch: Scratch,
    hooks: &Hooks<'_>,
    mut report: impl FnMut(&Outcome),
) -> Vec<Outcome> {
    let lifecycle = Lifecycle {
        forge,
        repo,
        caps: forge.capabilities(),
        hooks,
    };
    let mut outcomes = Vec::new();
    let mut push = |step: &'static str, status: Status| {
        let outcome = Outcome { step, status };
        report(&outcome);
        outcomes.push(outcome);
    };
    let status = |result: Step| match result {
        Ok(detail) => Status::Pass(detail),
        Err(reason) => Status::Fail(reason),
    };
    let caps = lifecycle.caps;

    if caps.merge {
        push("methods", status(lifecycle.methods().await));
        push(
            "mergeability",
            status(lifecycle.fresh_mergeability(scratch.merged).await),
        );
    } else {
        push("methods", Status::Skip("merge".into()));
        push("mergeability", Status::Skip("merge".into()));
    }
    if caps.set_draft {
        push("draft", status(lifecycle.draft(scratch.merged).await));
        push("ready", status(lifecycle.ready(scratch.merged).await));
    } else {
        push("draft", Status::Skip("set_draft".into()));
        push("ready", Status::Skip("set_draft".into()));
    }
    if caps.merge {
        push(
            "merge-stale-head",
            status(lifecycle.stale_head(scratch.closed).await),
        );
        push("merge", status(lifecycle.merge(scratch.merged).await));
    } else {
        push("merge-stale-head", Status::Skip("merge".into()));
        push("merge", Status::Skip("merge".into()));
    }
    if caps.close_reopen {
        push("close", status(lifecycle.close(scratch.closed).await));
        push("reopen", status(lifecycle.reopen(scratch.closed).await));
        push("close-again", status(lifecycle.close(scratch.closed).await));
    } else {
        for step in ["close", "reopen", "close-again"] {
            push(step, Status::Skip("close_reopen".into()));
        }
    }
    if caps.auto_merge {
        push(
            "auto-merge",
            Status::Skip(
                "enabling it can merge at once; the request shapes are replay-tested".into(),
            ),
        );
    } else {
        push("auto-merge", Status::Skip("auto_merge".into()));
    }
    outcomes
}

struct Lifecycle<'a> {
    forge: &'a dyn Forge,
    repo: &'a RepoRef,
    caps: Capabilities,
    hooks: &'a Hooks<'a>,
}

impl Lifecycle<'_> {
    async fn pull(&self, number: PrNumber) -> Result<PullRequest, String> {
        self.forge
            .pull_request(self.repo, number)
            .await
            .map_err(err)
    }

    /// The mergeability once the forge has computed it.
    async fn settled(&self, number: PrNumber) -> Result<Mergeability, String> {
        let mut last = None;
        for look in 0..LOOKS {
            if look > 0 {
                tokio::time::sleep(self.hooks.settle).await;
            }
            let read = self
                .forge
                .mergeability(self.repo, number)
                .await
                .map_err(err)?;
            if read.mergeable.is_some() {
                return Ok(read);
            }
            last = Some(read);
        }
        Err(format!("still computing after {LOOKS} looks: {last:?}"))
    }

    async fn methods(&self) -> Step {
        let settings = self.forge.merge_methods(self.repo).await.map_err(err)?;
        ensure(!settings.allowed.is_empty(), || "no merge method".into())?;
        ensure(settings.allowed.contains(&settings.default), || {
            format!("default {:?} is not allowed", settings.default)
        })?;
        Ok(format!(
            "allowed {:?}, default {:?}",
            settings.allowed, settings.default
        ))
    }

    async fn fresh_mergeability(&self, number: PrNumber) -> Step {
        let read = self.settled(number).await?;
        ensure(
            read.mergeable == Some(true) && read.reasons.is_empty() && !read.auto_merge,
            || format!("a fresh pull request reads {read:?}"),
        )?;
        Ok("mergeable, no blockers".into())
    }

    async fn draft(&self, number: PrNumber) -> Step {
        let drafted = self
            .forge
            .set_draft(self.repo, number, true)
            .await
            .map_err(err)?;
        ensure(drafted.state == PrState::Draft, || {
            format!("the answer reads {:?}", drafted.state)
        })?;
        let again = self.pull(number).await?;
        ensure(again.state == PrState::Draft, || {
            format!("read back as {:?}", again.state)
        })?;
        let read = self.settled(number).await?;
        ensure(
            read.mergeable == Some(false) && read.reasons.contains(&MergeBlocker::Draft),
            || format!("a draft reads {read:?}"),
        )?;
        Ok(format!("draft, blocked by {:?}", read.reasons))
    }

    async fn ready(&self, number: PrNumber) -> Step {
        let ready = self
            .forge
            .set_draft(self.repo, number, false)
            .await
            .map_err(err)?;
        ensure(ready.state == PrState::Open, || {
            format!("the answer reads {:?}", ready.state)
        })?;
        let again = self.pull(number).await?;
        ensure(again.state == PrState::Open, || {
            format!("read back as {:?}", again.state)
        })?;
        if self.caps.merge {
            let read = self.settled(number).await?;
            ensure(
                read.mergeable == Some(true) && read.reasons.is_empty(),
                || format!("a ready pull request reads {read:?}"),
            )?;
        }
        Ok("open again".into())
    }

    /// A merge that names a head the pull request never had is refused and
    /// leaves it open.
    async fn stale_head(&self, number: PrNumber) -> Step {
        let stale = MergeOptions::new(MergeMethod::Merge, Sha::new("0".repeat(40)));
        let refused = self.forge.merge(self.repo, number, &stale).await;
        let after = self.pull(number).await?;
        let Err(error) = refused else {
            return Err(format!(
                "a merge on a stale head was accepted; the pull request is {:?}",
                after.state
            ));
        };
        ensure(after.state.is_open(), || {
            format!("refused, but the pull request is {:?}", after.state)
        })?;
        Ok(format!("refused ({error}), still open"))
    }

    async fn merge(&self, number: PrNumber) -> Step {
        let before = self.pull(number).await?;
        let settings = self.forge.merge_methods(self.repo).await.map_err(err)?;
        let method = if settings.allowed.contains(&MergeMethod::Squash) {
            MergeMethod::Squash
        } else {
            settings.default
        };
        let options = MergeOptions {
            commit_title: Some(MERGE_TITLE.to_owned()),
            commit_message: Some(MERGE_BODY.to_owned()),
            delete_branch: true,
            ..MergeOptions::new(method, before.head.sha.clone())
        };
        let merged = self
            .forge
            .merge(self.repo, number, &options)
            .await
            .map_err(err)?;
        ensure(merged.state == PrState::Merged, || {
            format!("the answer reads {:?}", merged.state)
        })?;
        let again = self.pull(number).await?;
        ensure(again.state == PrState::Merged, || {
            format!("read back as {:?}", again.state)
        })?;
        ensure(again.head.sha == before.head.sha, || {
            "the head moved".to_owned()
        })?;
        let read = self.settled(number).await?;
        ensure(read.reasons == [MergeBlocker::NotOpen], || {
            format!("a merged pull request reads {read:?}")
        })?;
        self.branch_gone(&before.head.name).await?;
        Ok(format!(
            "{method:?} merge, branch {} gone",
            before.head.name
        ))
    }

    async fn branch_gone(&self, branch: &str) -> Result<(), String> {
        for look in 0..LOOKS {
            if look > 0 {
                tokio::time::sleep(self.hooks.settle).await;
            }
            if !(self.hooks.branch_exists)(branch)? {
                return Ok(());
            }
        }
        Err(format!("branch {branch} still exists after the merge"))
    }

    async fn close(&self, number: PrNumber) -> Step {
        let closed = self.forge.close(self.repo, number).await.map_err(err)?;
        ensure(closed.state == PrState::Closed, || {
            format!("the answer reads {:?}", closed.state)
        })?;
        let again = self.pull(number).await?;
        ensure(again.state == PrState::Closed, || {
            format!("read back as {:?}", again.state)
        })?;
        Ok("closed".into())
    }

    async fn reopen(&self, number: PrNumber) -> Step {
        let reopened = self.forge.reopen(self.repo, number).await.map_err(err)?;
        ensure(reopened.state.is_open(), || {
            format!("the answer reads {:?}", reopened.state)
        })?;
        let again = self.pull(number).await?;
        ensure(again.state.is_open(), || {
            format!("read back as {:?}", again.state)
        })?;
        Ok("open again".into())
    }
}
