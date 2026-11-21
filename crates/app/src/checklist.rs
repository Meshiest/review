//! The numbered steps a launch tab shows while it makes the worktree and
//! starts the program: the step under way spins, finished ones turn into
//! check marks, and a failed one shows its error. The worktree job reports
//! the steps it reaches ([`crate::worktrees::Progress`]); a step it skips
//! (a worktree already at the head) counts as done once a later one starts.
//! A canceled launch keeps its step under way until the job has stopped and
//! what it made is removed, then fails it with what happened.

use crate::worktrees::Step;

/// Where one step stands.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum StepState {
    Waiting,
    Running,
    Done,
    Failed(String),
    /// Waiting for the user, who is told what to do.
    Blocked(String),
}

/// The steps of one launch, in order.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Checklist {
    steps: Vec<(Step, StepState)>,
    /// Whether the work reported a step yet: until then the first step
    /// shows as under way, but nothing has run.
    reported: bool,
    /// The user canceled: the step under way is being stopped.
    canceling: bool,
}

/// What a launch is for, which picks its steps.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Purpose {
    /// "Review with agent": the context file and the agent.
    Review,
    /// "Open on machine": the machine's command.
    Command,
}

impl Checklist {
    /// The steps of a launch on this PC or (`remote`) over SSH, the first
    /// one under way.
    #[must_use]
    pub fn plan(purpose: Purpose, remote: bool) -> Self {
        let mut steps = Vec::with_capacity(6);
        if remote {
            steps.push(Step::Connect);
        }
        steps.extend([Step::Fetch, Step::Worktree, Step::Config]);
        steps.push(match purpose {
            Purpose::Review => Step::Context,
            Purpose::Command => Step::StartCommand,
        });
        if purpose == Purpose::Review {
            steps.push(Step::StartAgent);
        }
        let mut list = Self {
            steps: steps
                .into_iter()
                .map(|step| (step, StepState::Waiting))
                .collect(),
            reported: false,
            canceling: false,
        };
        if let Some((_, state)) = list.steps.first_mut() {
            *state = StepState::Running;
        }
        list
    }

    #[must_use]
    pub fn steps(&self) -> &[(Step, StepState)] {
        &self.steps
    }

    /// The step the work reached: those before it are done. A step the
    /// plan lacks goes in after the last one reported, or first.
    pub fn reach(&mut self, step: Step) {
        let at = match self.steps.iter().position(|(s, _)| *s == step) {
            Some(at) => at,
            None => {
                let at = if self.reported {
                    self.steps
                        .iter()
                        .rposition(|(_, state)| *state != StepState::Waiting)
                        .map_or(0, |last| last + 1)
                } else {
                    0
                };
                self.steps.insert(at, (step, StepState::Waiting));
                at
            }
        };
        self.reported = true;
        for (ix, (_, state)) in self.steps.iter_mut().enumerate() {
            *state = match ix.cmp(&at) {
                std::cmp::Ordering::Less => StepState::Done,
                std::cmp::Ordering::Equal => StepState::Running,
                std::cmp::Ordering::Greater => StepState::Waiting,
            };
        }
    }

    /// The step under way failed with `error`; with none under way, the
    /// first that has not finished. A cancel ends with it.
    pub fn fail(&mut self, error: String) {
        self.canceling = false;
        if let Some((_, state)) = self.current_mut() {
            *state = StepState::Failed(error);
        }
    }

    /// `step` waits for the user, who is told `what` to do.
    pub fn block(&mut self, step: Step, what: String) {
        self.reach(step);
        if let Some((_, state)) = self.steps.iter_mut().find(|(s, _)| *s == step) {
            *state = StepState::Blocked(what);
        }
    }

    /// The user canceled the launch; [`Self::fail`] says how it ended.
    pub fn cancel(&mut self) {
        self.canceling = true;
    }

    /// Whether a cancel is under way.
    #[must_use]
    pub fn canceling(&self) -> bool {
        self.canceling
    }

    /// The failed step's error, if one failed.
    #[must_use]
    pub fn error(&self) -> Option<&str> {
        self.steps.iter().find_map(|(_, state)| match state {
            StepState::Failed(error) => Some(error.as_str()),
            _ => None,
        })
    }

    /// Whether it stopped: a step failed or waits for the user.
    #[must_use]
    pub fn stopped(&self) -> bool {
        self.steps
            .iter()
            .any(|(_, state)| matches!(state, StepState::Failed(_) | StepState::Blocked(_)))
    }

    fn current_mut(&mut self) -> Option<&mut (Step, StepState)> {
        self.steps
            .iter_mut()
            .find(|(_, state)| !matches!(state, StepState::Done))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn states(list: &Checklist) -> Vec<(Step, &str)> {
        list.steps()
            .iter()
            .map(|(step, state)| {
                let state = match state {
                    StepState::Waiting => "waiting",
                    StepState::Running => "running",
                    StepState::Done => "done",
                    StepState::Failed(_) => "failed",
                    StepState::Blocked(_) => "blocked",
                };
                (*step, state)
            })
            .collect()
    }

    #[test]
    fn a_review_plans_its_steps_and_a_remote_one_connects_first() {
        let local = Checklist::plan(Purpose::Review, false);
        assert_eq!(
            states(&local),
            [
                (Step::Fetch, "running"),
                (Step::Worktree, "waiting"),
                (Step::Config, "waiting"),
                (Step::Context, "waiting"),
                (Step::StartAgent, "waiting"),
            ]
        );
        let remote = Checklist::plan(Purpose::Command, true);
        let order: Vec<Step> = remote.steps().iter().map(|(step, _)| *step).collect();
        assert_eq!(
            order,
            [
                Step::Connect,
                Step::Fetch,
                Step::Worktree,
                Step::Config,
                Step::StartCommand
            ]
        );
    }

    #[test]
    fn reaching_a_step_finishes_the_ones_before_it() {
        let mut list = Checklist::plan(Purpose::Review, false);
        list.reach(Step::Config);
        assert_eq!(
            states(&list)[..4],
            [
                (Step::Fetch, "done"),
                (Step::Worktree, "done"),
                (Step::Config, "running"),
                (Step::Context, "waiting"),
            ]
        );
        assert!(!list.stopped());
    }

    #[test]
    fn a_step_out_of_the_plan_goes_after_the_last_started() {
        let mut list = Checklist::plan(Purpose::Command, true);
        list.reach(Step::Connect);
        list.reach(Step::Clone);
        let order: Vec<Step> = list.steps().iter().map(|(step, _)| *step).collect();
        assert_eq!(order[..3], [Step::Connect, Step::Clone, Step::Fetch]);
        assert_eq!(states(&list)[1], (Step::Clone, "running"));
    }

    #[test]
    fn a_failure_marks_the_step_under_way() {
        let mut list = Checklist::plan(Purpose::Review, false);
        list.reach(Step::Worktree);
        list.fail("no space left".into());
        assert_eq!(states(&list)[1], (Step::Worktree, "failed"));
        assert_eq!(list.error(), Some("no space left"));
        assert!(list.stopped());
    }

    #[test]
    fn a_clone_waits_for_the_user() {
        let mut list = Checklist::plan(Purpose::Review, false);
        list.block(Step::CloneInTerminal, "clone first".into());
        assert_eq!(states(&list)[0], (Step::CloneInTerminal, "blocked"));
        assert!(list.stopped());
        assert_eq!(list.error(), None);
    }

    #[test]
    fn a_cancel_keeps_the_step_under_way_until_it_has_stopped() {
        let mut list = Checklist::plan(Purpose::Review, false);
        list.reach(Step::Worktree);
        list.cancel();
        assert!(list.canceling());
        assert!(!list.stopped(), "a new launch waits for it");
        assert_eq!(states(&list)[1], (Step::Worktree, "running"));
        list.fail("You canceled the launch.".into());
        assert!(!list.canceling());
        assert!(list.stopped());
        assert_eq!(states(&list)[1], (Step::Worktree, "failed"));
    }
}
