//! What the reviewer, assignee and label code of every backend shares: the
//! few-minute cache behind the pickers, and the diffs that turn "this is the
//! set I want" into the adds and removes a forge takes.

use std::collections::HashMap;
use std::future::Future;
use std::sync::{Mutex, PoisonError};
use std::time::Duration;

use crate::error::{Error, Result};
use crate::model::{Label, RepoRef, User};

/// How long a picker list is reused before it is asked for again.
pub(crate) const PICKER_TTL: Duration = Duration::from_secs(180);

/// Most users one [`crate::Forge::collaborators`] call returns.
pub const COLLABORATOR_LIMIT: usize = 100;

struct Entry<V> {
    at: Duration,
    value: V,
}

/// A map whose entries expire `ttl` after they were stored. Time is the
/// HTTP clock's, so tests on a manual clock expire entries without waiting.
struct TtlCache<K, V> {
    ttl: Duration,
    entries: Mutex<HashMap<K, Entry<V>>>,
}

impl<K: std::hash::Hash + Eq, V: Clone> TtlCache<K, V> {
    fn new(ttl: Duration) -> Self {
        Self {
            ttl,
            entries: Mutex::default(),
        }
    }

    fn get(&self, key: &K, now: Duration) -> Option<V> {
        let entries = self.entries.lock().unwrap_or_else(PoisonError::into_inner);
        let entry = entries.get(key)?;
        (now.saturating_sub(entry.at) < self.ttl).then(|| entry.value.clone())
    }

    fn put(&self, key: K, now: Duration, value: V) {
        let mut entries = self.entries.lock().unwrap_or_else(PoisonError::into_inner);
        entries.retain(|_, entry| now.saturating_sub(entry.at) < self.ttl);
        entries.insert(key, Entry { at: now, value });
    }

    fn forget(&self, key: &K) {
        let mut entries = self.entries.lock().unwrap_or_else(PoisonError::into_inner);
        entries.remove(key);
    }
}

/// The collaborator and label lists of the repositories a backend was asked
/// about. A failed fetch is never stored.
pub(crate) struct PickerCache {
    collaborators: TtlCache<(RepoRef, String), Vec<User>>,
    labels: TtlCache<RepoRef, Vec<Label>>,
}

impl Default for PickerCache {
    fn default() -> Self {
        Self::new(PICKER_TTL)
    }
}

impl PickerCache {
    pub(crate) fn new(ttl: Duration) -> Self {
        Self {
            collaborators: TtlCache::new(ttl),
            labels: TtlCache::new(ttl),
        }
    }

    /// The users matching `query` (case-insensitive), fetched at most once
    /// per repository and query within the TTL.
    pub(crate) async fn collaborators<Fut>(
        &self,
        now: Duration,
        repo: &RepoRef,
        query: &str,
        fetch: impl FnOnce() -> Fut,
    ) -> Result<Vec<User>>
    where
        Fut: Future<Output = Result<Vec<User>>>,
    {
        let key = (repo.clone(), query.trim().to_lowercase());
        if let Some(users) = self.collaborators.get(&key, now) {
            return Ok(users);
        }
        let users = fetch().await?;
        self.collaborators.put(key, now, users.clone());
        Ok(users)
    }

    pub(crate) async fn labels<Fut>(
        &self,
        now: Duration,
        repo: &RepoRef,
        fetch: impl FnOnce() -> Fut,
    ) -> Result<Vec<Label>>
    where
        Fut: Future<Output = Result<Vec<Label>>>,
    {
        if let Some(labels) = self.labels.get(repo, now) {
            return Ok(labels);
        }
        let labels = fetch().await?;
        self.labels.put(repo.clone(), now, labels.clone());
        Ok(labels)
    }

    /// `wanted` as the repository spells the labels. A name the cached list
    /// lacks may be a label made a moment ago, so the list is asked for once
    /// more before the name is refused: several forges would otherwise
    /// create a label from a typo.
    pub(crate) async fn resolve_labels<Fut>(
        &self,
        now: Duration,
        repo: &RepoRef,
        wanted: &[String],
        fetch: impl Fn() -> Fut,
    ) -> Result<Vec<String>>
    where
        Fut: Future<Output = Result<Vec<Label>>>,
    {
        let known = self.labels(now, repo, &fetch).await?;
        if let Ok(names) = canonical_label_names(&known, wanted, repo) {
            return Ok(names);
        }
        self.labels.forget(repo);
        let known = self.labels(now, repo, &fetch).await?;
        canonical_label_names(&known, wanted, repo)
    }
}

/// Each wanted name as spelled in `known` (forges compare names without
/// regard to case), without repeats; the first unknown name is an error.
fn canonical_label_names(
    known: &[Label],
    wanted: &[String],
    repo: &RepoRef,
) -> Result<Vec<String>> {
    let mut names: Vec<String> = Vec::with_capacity(wanted.len());
    for name in wanted {
        let found = known
            .iter()
            .find(|label| label.name == *name)
            .or_else(|| {
                known
                    .iter()
                    .find(|label| label.name.eq_ignore_ascii_case(name))
            })
            .ok_or_else(|| Error::NotFound {
                request: format!("label {name:?} in {repo}"),
            })?;
        if !names.contains(&found.name) {
            names.push(found.name.clone());
        }
    }
    Ok(names)
}

/// What to add and what to remove so that `current` becomes `wanted`, by
/// name and in the order given.
pub(crate) fn plan_labels(current: &[Label], wanted: &[String]) -> (Vec<String>, Vec<String>) {
    let mut add: Vec<String> = Vec::new();
    for name in wanted {
        if !current.iter().any(|label| label.name == *name) && !add.contains(name) {
            add.push(name.clone());
        }
    }
    let remove = current
        .iter()
        .filter(|label| !wanted.contains(&label.name))
        .map(|label| label.name.clone())
        .collect();
    (add, remove)
}

/// What to add and what to remove so that `current` becomes `wanted`, by
/// login (case-insensitive).
pub(crate) fn plan_users(current: &[User], wanted: &[User]) -> (Vec<User>, Vec<User>) {
    let has = |list: &[User], user: &User| {
        list.iter()
            .any(|other| other.login.eq_ignore_ascii_case(&user.login))
    };
    let mut add: Vec<User> = Vec::new();
    for user in wanted {
        if !has(current, user) && !has(&add, user) {
            add.push(user.clone());
        }
    }
    let remove = current
        .iter()
        .filter(|user| !has(wanted, user))
        .cloned()
        .collect();
    (add, remove)
}

/// `users` without any of `drop`, by login.
pub(crate) fn without(users: &[User], drop: &[User]) -> Vec<User> {
    users
        .iter()
        .filter(|user| {
            !drop
                .iter()
                .any(|other| other.login.eq_ignore_ascii_case(&user.login))
        })
        .cloned()
        .collect()
}

/// A team's slug: `org/team` is addressed as `team` within `org`.
pub(crate) fn team_slug(login: &str) -> &str {
    login.rsplit_once('/').map_or(login, |(_, slug)| slug)
}

/// The picker's prefix search for a forge that cannot search itself: the
/// login, the display name or any word of it starts with `query`.
pub(crate) fn matches_query(user: &User, query: &str) -> bool {
    let query = query.trim().to_lowercase();
    if query.is_empty() {
        return true;
    }
    user.login.to_lowercase().starts_with(&query)
        || user.name.as_deref().is_some_and(|name| {
            name.to_lowercase()
                .split_whitespace()
                .any(|word| word.starts_with(&query))
        })
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;

    fn repo() -> RepoRef {
        RepoRef::new("github.com", "o", "r")
    }

    fn label(name: &str) -> Label {
        Label {
            name: name.to_owned(),
            color: None,
            description: None,
        }
    }

    fn names(list: &[&str]) -> Vec<String> {
        list.iter().map(|name| (*name).to_owned()).collect()
    }

    #[test]
    fn labels_are_planned_by_name() {
        let current = [label("bug"), label("docs")];
        let (add, remove) = plan_labels(&current, &names(&["docs", "ui", "ui"]));
        assert_eq!(add, ["ui"]);
        assert_eq!(remove, ["bug"]);
        assert_eq!(
            plan_labels(&current, &names(&["bug", "docs"])),
            (vec![], vec![])
        );
    }

    #[test]
    fn users_are_planned_by_login_without_regard_to_case() {
        let (a, b, c) = (User::new("Ann"), User::new("bob"), User::new("cy"));
        let (add, remove) = plan_users(
            &[a.clone(), b.clone()],
            &[User::new("ann"), c.clone(), c.clone()],
        );
        assert_eq!(add, [c]);
        assert_eq!(remove, [b]);
        assert_eq!(
            without(&[a.clone(), User::new("x")], &[User::new("ANN")]),
            [User::new("x")]
        );
    }

    #[test]
    fn names_resolve_to_the_repository_spelling() {
        let known = [label("Bug"), label("review-test")];
        let ok = canonical_label_names(&known, &names(&["bug", "review-test", "Bug"]), &repo());
        assert_eq!(ok.unwrap(), ["Bug", "review-test"]);
        let err = canonical_label_names(&known, &names(&["nope"]), &repo()).unwrap_err();
        assert!(matches!(err, Error::NotFound { .. }), "{err}");
    }

    #[test]
    fn prefix_search_reads_logins_and_name_words() {
        let mut user = User::new("lovelace");
        user.name = Some("Ada Byron".to_owned());
        for (query, hit) in [
            ("", true),
            ("lov", true),
            ("LOV", true),
            ("byr", true),
            ("ron", false),
            ("x", false),
        ] {
            assert_eq!(matches_query(&user, query), hit, "{query}");
        }
        assert_eq!(team_slug("org/team"), "team");
        assert_eq!(team_slug("team"), "team");
    }

    #[tokio::test]
    async fn lists_are_reused_until_the_ttl_and_failures_are_not_kept() {
        let cache = PickerCache::new(Duration::from_secs(10));
        let calls = AtomicUsize::new(0);
        let fetch = || async {
            calls.fetch_add(1, Ordering::SeqCst);
            Ok(vec![User::new("ann")])
        };
        let at = |secs| Duration::from_secs(secs);
        cache
            .collaborators(at(0), &repo(), "A", fetch)
            .await
            .unwrap();
        cache
            .collaborators(at(9), &repo(), " a ", fetch)
            .await
            .unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        cache
            .collaborators(at(10), &repo(), "a", fetch)
            .await
            .unwrap();
        cache
            .collaborators(at(10), &repo(), "b", fetch)
            .await
            .unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), 3);

        let failing = || async { Err::<Vec<Label>, _>(Error::Shutdown) };
        assert!(cache.labels(at(0), &repo(), failing).await.is_err());
        let ok = || async { Ok(vec![label("a")]) };
        assert_eq!(cache.labels(at(1), &repo(), ok).await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn an_unknown_label_asks_the_forge_once_more_before_it_is_refused() {
        let cache = PickerCache::new(Duration::from_secs(600));
        let calls = AtomicUsize::new(0);
        let fetch = || async {
            let call = calls.fetch_add(1, Ordering::SeqCst);
            Ok(if call == 0 {
                vec![label("a")]
            } else {
                vec![label("a"), label("b")]
            })
        };
        let now = Duration::ZERO;
        let got = cache
            .resolve_labels(now, &repo(), &names(&["b"]), fetch)
            .await
            .unwrap();
        assert_eq!(got, ["b"]);
        assert_eq!(calls.load(Ordering::SeqCst), 2);
        let err = cache
            .resolve_labels(now, &repo(), &names(&["zzz"]), fetch)
            .await
            .unwrap_err();
        assert!(matches!(err, Error::NotFound { .. }));
        assert_eq!(calls.load(Ordering::SeqCst), 3);
    }
}
