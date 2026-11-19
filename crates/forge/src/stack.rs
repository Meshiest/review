use crate::model::{PullRequest, Stack, StackLink, StackSource};

/// The stack rule every forge shares: pull request A sits on B when A's base
/// branch is B's head branch, in A's repository (a fork's branch cannot be
/// a base). Only open pull requests take part. Where a branch has several
/// children, the lowest-numbered one continues the stack.
///
/// `candidates` are the repository's open pull requests; `target` need not
/// be among them. Bounded by `candidates.len()`, so a cycle in bad data
/// cannot loop.
#[must_use]
pub fn stack_by_base_branch(target: &PullRequest, candidates: &[PullRequest]) -> Stack {
    let same_repo_head = |pr: &PullRequest| pr.head.repo.as_ref() == Some(&pr.repo);
    let others: Vec<&PullRequest> = candidates
        .iter()
        .filter(|pr| pr.repo == target.repo && pr.number != target.number && pr.state.is_open())
        .collect();
    let mut seen = vec![target.number];
    let mut below: Vec<&PullRequest> = Vec::new();
    let mut current = target;
    for _ in 0..others.len() {
        let Some(parent) = others
            .iter()
            .copied()
            .filter(|pr| same_repo_head(pr) && pr.head.name == current.base.name)
            .filter(|pr| !seen.contains(&pr.number))
            .min_by_key(|pr| pr.number)
        else {
            break;
        };
        seen.push(parent.number);
        below.push(parent);
        current = parent;
    }
    let mut above: Vec<&PullRequest> = Vec::new();
    let mut current = target;
    for _ in 0..others.len() {
        if !same_repo_head(current) {
            break;
        }
        let Some(child) = others
            .iter()
            .copied()
            .filter(|pr| pr.base.name == current.head.name)
            .filter(|pr| !seen.contains(&pr.number))
            .min_by_key(|pr| pr.number)
        else {
            break;
        };
        seen.push(child.number);
        above.push(child);
        current = child;
    }
    let links = below
        .into_iter()
        .rev()
        .chain(std::iter::once(target))
        .chain(above)
        .map(link)
        .collect();
    Stack {
        source: StackSource::BaseBranch,
        links,
    }
}

fn link(pr: &PullRequest) -> StackLink {
    StackLink {
        number: pr.number,
        title: pr.title.clone(),
        state: pr.state,
        head: pr.head.name.clone(),
        base: pr.base.name.clone(),
    }
}
