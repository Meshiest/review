//! Opening a pull request: cache first, then the forge and git.
//!
//! [`open_pr`] reads the cached snapshot and viewed marks, paints the diff
//! from local objects when they are there, and only then waits on the
//! network: the forge snapshot and the git fetch run alongside, and a fetch
//! that moves the head rebuilds the diff. Every result goes out as an
//! [`OpenUpdate`] on a channel; nothing here touches the UI.

use std::sync::Arc;

use diff::{BlobId, FileDiff, Segment, Text};
use forge::model::{Snapshot, Stack, ViewedFile};
use futures::StreamExt as _;
use futures::channel::{mpsc, oneshot};
use gpui::SharedString;
use repo::{ChangeKind, FileChange, FileList, FileListOptions, ObjectId};
use store::PrKey;

use crate::model::{ChangedFile, FileStatus};
use crate::services::Services;

/// The first diff holds full content for files up to about this many
/// changed lines; the rest come in a second pass a moment later.
const FIRST_PASS_LINES: u32 = 400;

#[derive(Clone, Debug)]
pub struct OpenRequest {
    pub key: PrKey,
    /// The base branch, when the inbox row already says it.
    pub base_branch: Option<String>,
    /// Ask the forge for its viewed state (GitHub).
    pub forge_viewed: bool,
}

/// Where opening stands, for the Diff panel's header.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum OpenStatus {
    Loading,
    /// `git fetch` is running; the last progress line, if any.
    Fetching(Option<SharedString>),
    Building,
    Ready,
    /// The fetch failed; the diff on screen is from local objects.
    Stale(SharedString),
    Failed(SharedString),
}

/// A diff ready to show, with the file list that goes with it.
#[derive(Clone, Debug)]
pub struct DiffUpdate {
    pub head: ObjectId,
    pub diff: Arc<diff::Diff>,
    pub files: Vec<ChangedFile>,
    /// False while later files are only headers.
    pub complete: bool,
}

#[derive(Clone, Debug)]
pub enum OpenUpdate {
    /// `fresh` is false for the cached copy.
    Snapshot {
        snapshot: Arc<Snapshot>,
        fresh: bool,
    },
    Viewed(store::ViewedSet),
    /// The forge's own viewed state, by path.
    ForgeViewed(Vec<ViewedFile>),
    Stack(Stack),
    Status(OpenStatus),
    Diff(DiffUpdate),
}

/// Runs the whole open; drop the future to stop it.
pub async fn open_pr(
    services: Services,
    request: OpenRequest,
    updates: mpsc::UnboundedSender<OpenUpdate>,
) {
    let send = |update| {
        let _ = updates.unbounded_send(update);
    };
    let key = &request.key;
    send(OpenUpdate::Status(OpenStatus::Loading));

    let (cached, viewed) = match &services.store {
        Some(store) => {
            let (cached, viewed) = futures::join!(store.snapshot(key), store.viewed_for(key));
            (cached.ok().flatten(), viewed.ok())
        }
        None => (None, None),
    };
    if let Some(viewed) = viewed {
        send(OpenUpdate::Viewed(viewed));
    }
    let mut base = request.base_branch.clone();
    if let Some(cached) = cached {
        base = base.or_else(|| Some(cached.snapshot.pull_request.base.name.clone()));
        send(OpenUpdate::Snapshot {
            snapshot: Arc::new(cached.snapshot),
            fresh: false,
        });
    }

    let (base_tx, base_rx) = oneshot::channel::<Option<String>>();
    let network = async {
        match services.snapshot(key).await {
            Ok(snapshot) => {
                let _ = base_tx.send(Some(snapshot.pull_request.base.name.clone()));
                send(OpenUpdate::Snapshot {
                    snapshot,
                    fresh: true,
                });
            }
            Err(_) => {
                let _ = base_tx.send(None);
            }
        }
        if let Ok(stack) = services.stack(key).await {
            send(OpenUpdate::Stack(stack));
        }
        if request.forge_viewed {
            let (repo, number) = (key.repo.clone(), key.number);
            let viewed = services
                .forge(&key.repo.host, move |forge| async move {
                    forge.viewed_files(&repo, number).await
                })
                .await;
            if let Ok(files) = viewed {
                send(OpenUpdate::ForgeViewed(files));
            }
        }
    };
    let git = async {
        let resolved = {
            let (objects, repo) = (services.objects.clone(), key.repo.clone());
            services.run_blocking(move || objects.resolve(&repo)).await
        };
        let objects = match resolved {
            Some(Ok(objects)) => objects,
            Some(Err(error)) => {
                send(OpenUpdate::Status(OpenStatus::Failed(error.into())));
                return;
            }
            None => return,
        };
        let number = key.number.0;
        let local = {
            let objects = objects.clone();
            services
                .run_blocking(move || objects.pr_range(number).ok())
                .await
                .flatten()
        };
        let mut shown = None;
        if let Some((_, head)) = local
            && build_and_send(&services, &objects, number, head, &send).await
        {
            shown = Some(head);
        }

        let base = match base {
            Some(base) => Some(base),
            None => base_rx.await.ok().flatten(),
        };
        let Some(base) = base else {
            if shown.is_none() {
                send(OpenUpdate::Status(OpenStatus::Failed(
                    "the forge did not say which branch this merges into".into(),
                )));
            }
            return;
        };
        send(OpenUpdate::Status(OpenStatus::Fetching(None)));
        let (progress_tx, mut progress) = mpsc::unbounded();
        let fetch = services.fetches.fetch(
            &objects,
            services.fetch_request(&key.repo, key.number, &base),
            Some(progress_tx),
        );
        let forward = async {
            while let Some(line) = progress.next().await {
                send(OpenUpdate::Status(OpenStatus::Fetching(Some(line.into()))));
            }
        };
        let (fetched, ()) = futures::join!(fetch, forward);
        match fetched {
            Ok(fetched) if Some(fetched.head) == shown => {
                send(OpenUpdate::Status(OpenStatus::Ready));
            }
            Ok(fetched) => {
                build_and_send(&services, &objects, number, fetched.head, &send).await;
            }
            Err(error) if shown.is_some() => {
                send(OpenUpdate::Status(OpenStatus::Stale(error.into())));
            }
            Err(error) => send(OpenUpdate::Status(OpenStatus::Failed(error.into()))),
        }
    };
    futures::join!(network, git);
}

/// File list, first diff, full diff, then renames. Returns whether a diff
/// went out.
async fn build_and_send(
    services: &Services,
    objects: &repo::Store,
    number: u64,
    head: ObjectId,
    send: &impl Fn(OpenUpdate),
) -> bool {
    send(OpenUpdate::Status(OpenStatus::Building));
    let options = |renames| FileListOptions {
        renames,
        detect_binary: false,
    };
    let list = {
        let objects = objects.clone();
        services
            .run_blocking(move || objects.pr_file_list(number, options(false)))
            .await
    };
    let list = match list {
        Some(Ok(list)) => Arc::new(list),
        Some(Err(error)) => {
            send(OpenUpdate::Status(OpenStatus::Failed(
                error.to_string().into(),
            )));
            return false;
        }
        None => return false,
    };
    let build = |list: Arc<FileList>, budget: Option<u32>| {
        let objects = objects.clone();
        services.run_blocking(move || build_diff(&objects, &list, budget))
    };
    let Some(first) = build(list.clone(), Some(FIRST_PASS_LINES)).await else {
        return false;
    };
    let complete = first.complete;
    send(OpenUpdate::Diff(first.into_update(head)));
    if !complete && let Some(full) = build(list.clone(), None).await {
        send(OpenUpdate::Diff(full.into_update(head)));
    }
    send(OpenUpdate::Status(OpenStatus::Ready));

    let may_rename = list.files.iter().any(|f| f.kind == ChangeKind::Added)
        && list.files.iter().any(|f| f.kind == ChangeKind::Deleted);
    if may_rename {
        let renamed = {
            let objects = objects.clone();
            services
                .run_blocking(move || objects.pr_file_list(number, options(true)))
                .await
        };
        if let Some(Ok(renamed)) = renamed
            && renamed.files.iter().any(|f| f.kind == ChangeKind::Renamed)
            && let Some(full) = build(Arc::new(renamed), None).await
        {
            send(OpenUpdate::Diff(full.into_update(head)));
        }
    }
    true
}

/// A diff built from a file list.
pub struct Built {
    pub diff: diff::Diff,
    pub files: Vec<ChangedFile>,
    pub complete: bool,
}

impl Built {
    fn into_update(self, head: ObjectId) -> DiffUpdate {
        DiffUpdate {
            head,
            diff: Arc::new(self.diff),
            files: self.files,
            complete: self.complete,
        }
    }
}

/// Reads blobs and computes hunks, in file-list order. With a `budget`,
/// files after the first ones that reach it get only a header; `complete`
/// says whether any did. Blocks on git.
pub fn build_diff(objects: &repo::Store, list: &FileList, budget: Option<u32>) -> Built {
    let mut diff = diff::Diff::default();
    let mut files = Vec::with_capacity(list.files.len());
    let mut lines = 0u32;
    let mut complete = true;
    for change in &list.files {
        let within = budget.is_none_or(|budget| lines < budget);
        complete &= within;
        let (file, (added, removed)) = if within {
            let file = file_diff(objects, change);
            let counts = counts(&file);
            lines = lines.saturating_add(counts.0 + counts.1);
            (file, counts)
        } else {
            (header_only(change), (0, 0))
        };
        files.push(changed_file(change, added, removed));
        diff.files.push(file);
    }
    Built {
        diff,
        files,
        complete,
    }
}

fn header_only(change: &FileChange) -> FileDiff {
    FileDiff {
        path: change.path.clone(),
        ..FileDiff::default()
    }
}

fn file_diff(objects: &repo::Store, change: &FileChange) -> FileDiff {
    let Ok(content) = objects.file_content(change) else {
        return header_only(change);
    };
    FileDiff {
        path: change.path.clone(),
        old: text(&content.old, change.old.map(|e| e.id)),
        new: text(&content.new, change.new.map(|e| e.id)),
        hunks: content.hunks,
    }
}

/// The blob as text without a copy, keyed by its object id so highlights
/// are shared by every diff that shows it.
fn text(blob: &repo::Blob, id: Option<ObjectId>) -> Text {
    let id = id.map_or_else(BlobId::unique, |id| BlobId::from_oid(id.as_bytes()));
    Text::from_parts_with_id(
        id,
        blob.shared_bytes().clone(),
        blob.shared_line_starts().clone(),
    )
}

fn counts(file: &FileDiff) -> (u32, u32) {
    file.hunks
        .iter()
        .flat_map(|hunk| &hunk.segments)
        .fold((0, 0), |(added, removed), segment| match *segment {
            Segment::Change {
                added: a,
                removed: r,
            } => (added + a, removed + r),
            Segment::Context(_) => (added, removed),
        })
}

fn changed_file(change: &FileChange, added: u32, removed: u32) -> ChangedFile {
    let blob = change
        .new
        .or(change.old)
        .map(|entry| entry.id.to_string())
        .unwrap_or_default();
    ChangedFile {
        path: change.path.clone().into(),
        blob: blob.into(),
        status: match change.kind {
            ChangeKind::Added => FileStatus::Added,
            ChangeKind::Deleted => FileStatus::Deleted,
            ChangeKind::Modified => FileStatus::Modified,
            ChangeKind::Renamed => FileStatus::Renamed,
        },
        added,
        removed,
    }
}
