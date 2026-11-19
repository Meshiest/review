//! What each forge can do, mirroring the capability matrix in
//! `docs/design.md`. The UI hides what a forge lacks instead of failing.

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

use crate::model::ForgeKind;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReplyMode {
    /// A reply API on the thread ([G2], [L2]).
    Native,
    /// No reply API: a new review comment on the same path and line joins
    /// the thread ([F2]).
    SamePathAndLine,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ResolveMode {
    /// Read and write ([G3], [L3]).
    ReadWrite,
    /// State is readable, writing needs a web session ([F3]).
    ReadOnly,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Suggestions {
    /// Shown as plain fenced code by the forge ([F4]); the app renders them.
    NotRendered,
    /// The forge renders suggestion blocks; applying is a local commit.
    Rendered,
    /// Rendered, and the forge can apply them ([L4]).
    RenderedAndApplicable,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ViewedSync {
    /// Read and written through the forge ([G4]).
    Forge,
    /// No API ([L4], [F5]): kept locally by path and blob id.
    Local,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StackSupport {
    /// A native stack API, with the base-branch rule as fallback when it
    /// reports none ([G9], [L9]).
    Native,
    /// Base-branch rule only.
    BaseBranch,
}

/// One row of the matrix per field.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Capabilities {
    /// Queued (pending) review submitted with a verdict ([G1], [L1], [F1]).
    pub queued_review: bool,
    /// "Request changes" is a verdict the forge accepts.
    pub request_changes: bool,
    /// Comments may span several lines (Forgejo from v17 through `extra_lines_count`, [F1]).
    pub multi_line_comments: bool,
    /// Comments on a whole file rather than a line (GitHub `subjectType:
    /// FILE`, GitLab `position_type: file`; Forgejo has no file position).
    pub file_comments: bool,
    /// The author may approve or request changes on their own pull request.
    /// GitHub and Forgejo refuse both [F1]; GitLab allows approving your
    /// own merge request unless the project forbids it.
    pub own_pull_request_verdicts: bool,
    /// A queued comment's text can be changed in place (GitHub
    /// `updatePullRequestReviewComment`, GitLab `PUT .../draft_notes/:id`).
    /// Forgejo has no edit for a review comment: delete and add it again.
    pub edit_queued: bool,
    pub reply: ReplyMode,
    pub resolve: ResolveMode,
    pub suggestions: Suggestions,
    pub viewed: ViewedSync,
    /// Check or pipeline status ([G5], [L5], [F6]).
    pub checks: bool,
    /// Steps inside a job ([G6]; none on GitLab [L6]; Forgejo from v17 [F7]).
    pub job_steps: bool,
    /// Job log through the API ([G6], [L6]; Forgejo from v16 [F7]).
    pub job_logs: bool,
    /// Re-run a job ([G7], [L7]; Forgejo from v17 [F8]).
    pub retry_job: bool,
    /// Inbox "review requested from me" and "authored by me" ([G8], [L8], [F9]).
    pub inbox_queries: bool,
    /// Merge a pull request through the API ([`crate::Forge::merge`]).
    pub merge: bool,
    /// The forge merges once its requirements are met
    /// ([`crate::Forge::set_auto_merge`]): GitHub `enablePullRequestAutoMerge`,
    /// GitLab "merge when pipeline succeeds". Forgejo has an equivalent in
    /// `merge_when_checks_succeed` that the backend does not use yet.
    pub auto_merge: bool,
    /// Toggle draft state ([`crate::Forge::set_draft`]).
    pub set_draft: bool,
    /// Close and reopen ([`crate::Forge::close`], [`crate::Forge::reopen`]).
    pub close_reopen: bool,
    /// A team can be asked to review ([`crate::Forge::request_reviewers`]'s
    /// `teams`): GitHub and Forgejo organizations. GitLab has no review
    /// requests to groups.
    pub team_reviewers: bool,
    pub stacks: StackSupport,
    /// List endpoints send ETags, so polling can be conditional. Forgejo's
    /// `/pulls` does not; poll it with `updated_at` / `since`.
    pub etag_polling: bool,
}

impl Capabilities {
    pub const GITHUB: Self = Self {
        queued_review: true,
        request_changes: true,
        multi_line_comments: true,
        file_comments: true,
        own_pull_request_verdicts: false,
        edit_queued: true,
        reply: ReplyMode::Native,
        resolve: ResolveMode::ReadWrite,
        suggestions: Suggestions::Rendered,
        viewed: ViewedSync::Forge,
        checks: true,
        job_steps: true,
        job_logs: true,
        retry_job: true,
        inbox_queries: true,
        merge: true,
        auto_merge: true,
        set_draft: true,
        close_reopen: true,
        team_reviewers: true,
        stacks: StackSupport::Native,
        etag_polling: true,
    };

    pub const GITLAB: Self = Self {
        queued_review: true,
        request_changes: true,
        multi_line_comments: true,
        file_comments: true,
        own_pull_request_verdicts: true,
        edit_queued: true,
        reply: ReplyMode::Native,
        resolve: ResolveMode::ReadWrite,
        suggestions: Suggestions::RenderedAndApplicable,
        viewed: ViewedSync::Local,
        checks: true,
        job_steps: false,
        job_logs: true,
        retry_job: true,
        inbox_queries: true,
        merge: true,
        auto_merge: true,
        set_draft: true,
        close_reopen: true,
        team_reviewers: false,
        stacks: StackSupport::Native,
        etag_polling: true,
    };

    /// Forgejo, gated on the server version (`GET /api/v1/version`): v16 adds
    /// job lists and logs, v17 re-run. Reply and resolve APIs exist only on
    /// the unreleased v18 branch, so they are not assumed from a version;
    /// the backend upgrades them only after probing the endpoint.
    #[must_use]
    pub fn forgejo(version: ForgejoVersion) -> Self {
        Self {
            queued_review: true,
            request_changes: true,
            multi_line_comments: version.major >= 17,
            file_comments: false,
            own_pull_request_verdicts: false,
            edit_queued: false,
            reply: ReplyMode::SamePathAndLine,
            resolve: ResolveMode::ReadOnly,
            suggestions: Suggestions::NotRendered,
            viewed: ViewedSync::Local,
            checks: true,
            job_steps: version.major >= 17,
            job_logs: version.major >= 16,
            retry_job: version.major >= 17,
            inbox_queries: true,
            merge: true,
            auto_merge: false,
            set_draft: true,
            close_reopen: true,
            team_reviewers: true,
            stacks: StackSupport::BaseBranch,
            etag_polling: false,
        }
    }

    /// The matrix row for a kind, with Forgejo at the oldest supported
    /// version until the backend has probed the server.
    #[must_use]
    pub fn baseline(kind: ForgeKind) -> Self {
        match kind {
            ForgeKind::GitHub => Self::GITHUB,
            ForgeKind::GitLab => Self::GITLAB,
            ForgeKind::Forgejo => Self::forgejo(ForgejoVersion::OLDEST_SUPPORTED),
        }
    }
}

/// A Forgejo server version as `/api/v1/version` reports it
/// (`15.0.9+gitea-1.22.0`); the build suffix is ignored.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct ForgejoVersion {
    pub major: u32,
    pub minor: u32,
    pub patch: u32,
}

impl ForgejoVersion {
    /// The version the matrix was settled against.
    pub const OLDEST_SUPPORTED: Self = Self {
        major: 15,
        minor: 0,
        patch: 9,
    };
}

impl fmt::Display for ForgejoVersion {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}.{}.{}", self.major, self.minor, self.patch)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("not a Forgejo version: {0:?}")]
pub struct ParseVersionError(String);

impl FromStr for ForgejoVersion {
    type Err = ParseVersionError;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        let fail = || ParseVersionError(text.to_owned());
        let core = text
            .trim()
            .trim_start_matches('v')
            .split(['+', '-'])
            .next()
            .ok_or_else(fail)?;
        let mut parts = core.split('.').map(str::parse::<u32>);
        let mut next = |required: bool| match parts.next() {
            Some(Ok(n)) => Ok(n),
            None if !required => Ok(0),
            _ => Err(fail()),
        };
        Ok(Self {
            major: next(true)?,
            minor: next(false)?,
            patch: next(false)?,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_forgejo_versions() {
        let cases = [
            ("15.0.9+gitea-1.22.0", (15, 0, 9)),
            ("v16.0.7", (16, 0, 7)),
            ("17.1", (17, 1, 0)),
            ("18.0.0-dev-123-abc", (18, 0, 0)),
        ];
        for (text, (major, minor, patch)) in cases {
            let parsed: ForgejoVersion = text.parse().unwrap();
            assert_eq!(
                parsed,
                ForgejoVersion {
                    major,
                    minor,
                    patch
                },
                "{text}"
            );
        }
        assert!("".parse::<ForgejoVersion>().is_err());
        assert!("x.1".parse::<ForgejoVersion>().is_err());
    }

    #[test]
    fn forgejo_capabilities_follow_the_version() {
        let v = |major| ForgejoVersion {
            major,
            minor: 0,
            patch: 0,
        };
        let v15 = Capabilities::forgejo(v(15));
        assert!(!v15.job_logs && !v15.retry_job && !v15.job_steps);
        let v17 = Capabilities::forgejo(v(17));
        assert!(v17.job_logs && v17.retry_job && v17.job_steps);
        assert_eq!(v15.resolve, ResolveMode::ReadOnly);
        assert_eq!(v15.reply, ReplyMode::SamePathAndLine);
        let v16 = Capabilities::forgejo(v(16));
        assert!(v16.job_logs && !v16.retry_job);
        let v17 = Capabilities::forgejo(v(17));
        assert!(v17.job_logs && v17.retry_job);
    }

    #[test]
    fn baseline_matches_the_matrix() {
        assert_eq!(
            Capabilities::baseline(ForgeKind::GitHub).viewed,
            ViewedSync::Forge
        );
        assert_eq!(
            Capabilities::baseline(ForgeKind::GitLab).viewed,
            ViewedSync::Local
        );
        assert!(!Capabilities::baseline(ForgeKind::GitLab).job_steps);
        assert_eq!(
            Capabilities::baseline(ForgeKind::Forgejo).stacks,
            StackSupport::BaseBranch
        );
    }
}
