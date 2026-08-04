// SPDX-License-Identifier: MIT
//! Bridge between the Lore client library and the TUI.
//!
//! Every Lore entry point is event driven: it takes a `LoreEventCallback`,
//! streams `LoreEvent`s into it while it runs, and reports the terminal status
//! on `Complete`. Each operation here runs on its own thread so the render loop
//! never blocks, writes progress into a shared log buffer the UI can read
//! mid-flight, and finally hands back a plain snapshot that carries no Lore
//! types into the drawing code.

use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use std::sync::mpsc::Sender;

use lore::interface::*;
use parking_lot::Mutex;

/// Metadata keys attached to a revision — see `lore_revision::metadata`.
const META_MESSAGE: &str = "message";
const META_TIMESTAMP: &str = "timestamp";
const META_COMMITTED_BY: &str = "committed-by";
const META_CREATED_BY: &str = "created-by";

/// Live progress buffer, drained by the UI on every frame.
pub type LogSink = Arc<Mutex<Vec<String>>>;

/// Cap on retained log lines; the pane only ever shows the tail.
const LOG_LIMIT: usize = 500;

pub fn log_push(sink: &LogSink, line: impl Into<String>) {
    let mut sink = sink.lock();
    if sink.len() >= LOG_LIMIT {
        let excess = sink.len() - LOG_LIMIT + 1;
        sink.drain(0..excess);
    }
    sink.push(line.into());
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Section {
    Staged,
    Unmerged,
    Unstaged,
}

#[derive(Clone, Debug)]
pub struct FileEntry {
    /// Display path. A moved file reads `from -> to`.
    pub path: String,
    pub action: char,
    pub size: u64,
    pub conflict: bool,
}

impl FileEntry {
    /// The path Lore operates on, which for a move is the destination.
    pub fn target(&self) -> &str {
        self.path.rsplit(" -> ").next().unwrap_or(&self.path)
    }
}

#[derive(Clone, Debug, Default)]
pub struct StatusSnapshot {
    pub repository: String,
    pub branch_name: String,
    pub revision: String,
    pub revision_number: u64,
    pub remote_revision_number: u64,
    pub remote_available: bool,
    pub remote_authorized: bool,
    pub remote_branch_exist: bool,
    pub local_ahead: bool,
    pub remote_ahead: bool,
    pub staged: Vec<FileEntry>,
    pub unmerged: Vec<FileEntry>,
    pub unstaged: Vec<FileEntry>,
}

impl StatusSnapshot {
    /// Where the local branch sits against the remote. The phrase says which
    /// side is which: "behind" alone, under a "remote" label, reads as though
    /// the remote is the one lagging.
    pub fn remote_state(&self) -> &'static str {
        if !self.remote_available {
            "remote unavailable"
        } else if !self.remote_authorized {
            "remote not authorized"
        } else if !self.remote_branch_exist {
            "remote branch missing"
        } else if self.local_ahead && self.remote_ahead {
            "diverged from remote"
        } else if self.local_ahead {
            "ahead of remote"
        } else if self.remote_ahead {
            "behind remote"
        } else {
            "in sync with remote"
        }
    }
}

#[derive(Clone, Debug, Default)]
pub struct HistoryEntry {
    pub number: u64,
    pub revision: String,
    pub message: String,
    pub author: String,
    pub timestamp: u64,
    pub parents: Vec<String>,
}

impl HistoryEntry {
    pub fn subject(&self) -> &str {
        self.message.lines().next().unwrap_or("")
    }
}

/// A push notification from the server.
#[derive(Clone, Debug)]
pub enum Notice {
    Subscribed,
    BranchPushed { revision_number: u64, user: String },
    BranchCreated,
    BranchDeleted,
    Locked { paths: Vec<String>, user: String },
    Unlocked { paths: Vec<String>, user: String },
}

/// Where push notifications accumulate between frames.
pub type NoticeSink = Arc<Mutex<Vec<Notice>>>;

/// A file lock held on the current branch.
#[derive(Clone, Debug)]
pub struct LockEntry {
    pub path: String,
    pub owner: String,
    /// Milliseconds since the Unix epoch.
    pub locked_at: u64,
}

/// A unified diff for one file, as Lore renders it.
#[derive(Clone, Debug)]
pub struct DiffEntry {
    pub path: String,
    pub action: char,
    pub patch: String,
}

/// One repository as the server lists it. The listing carries nothing but an
/// identity — everything else comes from a per-repository `info` call.
#[derive(Clone, Debug)]
pub struct RepoEntry {
    pub id: String,
    pub name: String,
}

/// Detail for one repository. Identity is not repeated here — the listing
/// entry it is keyed by already carries the name and id.
#[derive(Clone, Debug, Default)]
pub struct RepoInfo {
    pub description: String,
    pub default_branch: String,
    pub creator: String,
    /// Milliseconds since the Unix epoch. Upstream documents this field as
    /// seconds, but the server sends milliseconds — reading it as seconds puts
    /// every repository in the year 58558.
    pub created: u64,
    pub remote_url: String,
}

/// Result of a finished background operation.
pub enum OpResult {
    Status(Result<StatusSnapshot, String>),
    History(Result<Vec<HistoryEntry>, String>),
    Diff(Result<Vec<DiffEntry>, String>),
    Locks(Result<Vec<LockEntry>, String>),
    RemoteUrl(Result<String, String>),
    Repos(Result<Vec<RepoEntry>, String>),
    RepoInfo {
        name: String,
        result: Result<RepoInfo, String>,
    },
    /// A mutating action; `Ok` carries the line to show on success.
    Action {
        name: &'static str,
        result: Result<String, String>,
        /// Refresh history as well as status once this lands.
        reload_history: bool,
    },
}

/// Terminal status of a call, captured from the `Complete` event.
#[derive(Default)]
struct Outcome {
    status: i32,
    error: String,
}

impl Outcome {
    fn into_result(self, ok: String) -> Result<String, String> {
        if self.status == 0 {
            Ok(ok)
        } else if self.error.is_empty() {
            Err(format!("failed with status {}", self.status))
        } else {
            Err(self.error)
        }
    }
}

/// Wrap a per-operation handler with the handling every call needs: log events
/// go to the UI buffer, `Complete` is recorded so the caller can tell success
/// from failure. Lore reports terminal failures through `Complete.error`, and
/// routes the human-readable text through a `Log` event at error level, so both
/// are kept.
fn callback<F>(sink: &LogSink, outcome: &Arc<Mutex<Outcome>>, handler: F) -> LoreEventCallback
where
    F: Fn(&LoreEvent) + Send + Sync + 'static,
{
    let sink = sink.clone();
    let outcome = outcome.clone();

    Some(Box::new(move |event: &LoreEvent| match event {
        LoreEvent::Log(data) => {
            if data.level >= LoreLogLevel::Info {
                let level = match data.level {
                    LoreLogLevel::Error => "error",
                    LoreLogLevel::Warn => "warn",
                    _ => "info",
                };
                log_push(&sink, format!("[{level}] {}", data.message.as_str()));
            }
        }
        LoreEvent::Error(data) => {
            log_push(&sink, format!("[error] {}", data.error_inner.as_str()));
        }
        LoreEvent::Maintenance(data) => {
            log_push(
                &sink,
                format!("[maintenance] {}", data.message.as_str()),
            );
        }
        LoreEvent::Complete(data) => {
            let mut outcome = outcome.lock();
            outcome.status = data.status;
            outcome.error = data.error.message.as_str().to_string();
        }
        LoreEvent::End(_) => {}
        other => handler(other),
    }))
}

fn action_char(action: LoreFileAction) -> char {
    match action {
        LoreFileAction::Add => 'A',
        LoreFileAction::Delete => 'D',
        LoreFileAction::Move => 'R',
        LoreFileAction::Copy => 'C',
        LoreFileAction::Keep => 'M',
    }
}

/// Read the repository state, optionally reconciling against the filesystem.
///
/// Without `scan` Lore reports only what is already flagged dirty, so edits
/// made outside Lore stay invisible — the TUI scans on its explicit refresh and
/// on first load so the file lists are not silently stale.
fn status(globals: LoreGlobalArgs, scan: bool, sink: &LogSink) -> Result<StatusSnapshot, String> {
    let snapshot = Arc::new(Mutex::new(StatusSnapshot::default()));
    let outcome = Arc::new(Mutex::new(Outcome::default()));

    let collector = snapshot.clone();
    let cb = callback(sink, &outcome, move |event| match event {
        LoreEvent::RepositoryStatusRevision(data) => {
            let mut snapshot = collector.lock();
            snapshot.repository = data.repository.to_string();
            snapshot.branch_name = data.branch_name.as_str().to_string();
            snapshot.revision = data.revision.to_string();
            snapshot.revision_number = data.revision_number;
            snapshot.remote_revision_number = data.revision_remote_number;
            snapshot.remote_available = data.remote_available != 0;
            snapshot.remote_authorized = data.remote_authorized != 0;
            snapshot.remote_branch_exist = data.remote_branch_exist != 0;
            snapshot.local_ahead = data.is_local_ahead > 0;
            snapshot.remote_ahead = data.is_remote_ahead > 0;
        }
        LoreEvent::RepositoryStatusFile(data) => {
            // The event data types are not re-exported from `lore::interface`,
            // so the entry is built here where the type is inferred.
            let path = if data.from_path.is_empty() {
                data.path.as_str().to_string()
            } else {
                format!("{} -> {}", data.from_path.as_str(), data.path.as_str())
            };
            let entry = FileEntry {
                path,
                action: action_char(data.action),
                size: data.size,
                conflict: data.flag_conflict_unresolved != 0,
            };

            let mut snapshot = collector.lock();
            if data.flag_staged != 0 {
                if data.flag_conflict_unresolved == 0 {
                    snapshot.staged.push(entry);
                } else {
                    snapshot.unmerged.push(entry);
                }
            } else {
                snapshot.unstaged.push(entry);
            }
        }
        _ => {}
    });

    let args = LoreRepositoryStatusArgs {
        staged: 1,
        scan: scan as u8,
        check_dirty: 1,
        reset: 0,
        sync_point: 0,
        revision_only: 0,
        count: 0,
        paths: LoreArray::default(),
    };

    lore::runtime().block_on(lore::repository::status(globals, args, cb));

    let outcome = std::mem::take(&mut *outcome.lock());
    outcome.into_result(String::new())?;

    let snapshot = std::mem::take(&mut *snapshot.lock());
    Ok(snapshot)
}

/// Read the revision log. Metadata arrives as separate events that belong to
/// the most recently emitted entry, so they are folded into the entry as they
/// stream in.
fn history(globals: LoreGlobalArgs, length: u32, sink: &LogSink) -> Result<Vec<HistoryEntry>, String> {
    let entries: Arc<Mutex<Vec<HistoryEntry>>> = Arc::new(Mutex::new(Vec::new()));
    let outcome = Arc::new(Mutex::new(Outcome::default()));

    let collector = entries.clone();
    let cb = callback(sink, &outcome, move |event| match event {
        LoreEvent::RevisionHistoryEntry(data) => {
            let parents = data
                .parent
                .iter()
                .filter(|hash| !hash.is_zero())
                .map(ToString::to_string)
                .collect();

            collector.lock().push(HistoryEntry {
                number: data.revision_number,
                revision: data.revision.to_string(),
                parents,
                ..Default::default()
            });
        }
        LoreEvent::Metadata(data) => {
            let mut entries = collector.lock();
            let Some(entry) = entries.last_mut() else {
                return;
            };
            match (data.key.as_str(), &data.value) {
                (META_MESSAGE, LoreMetadata::String(value)) => {
                    entry.message = value.as_str().to_string();
                }
                (META_TIMESTAMP, LoreMetadata::Numeric(value)) => {
                    entry.timestamp = *value;
                }
                // `committed-by` is the more specific attribution and wins when
                // both are present, whichever order they arrive in.
                (META_COMMITTED_BY, LoreMetadata::String(value)) => {
                    entry.author = value.as_str().to_string();
                }
                (META_CREATED_BY, LoreMetadata::String(value)) if entry.author.is_empty() => {
                    entry.author = value.as_str().to_string();
                }
                _ => {}
            }
        }
        _ => {}
    });

    let args = LoreRevisionHistoryArgs {
        revision: LoreString::from(""),
        branch: LoreString::from(""),
        date: 0,
        length,
        only_branch: 0,
    };

    lore::runtime().block_on(lore::revision::history(globals, args, cb));

    let outcome = std::mem::take(&mut *outcome.lock());
    outcome.into_result(String::new())?;

    let entries = std::mem::take(&mut *entries.lock());
    Ok(entries)
}

fn stage(
    globals: LoreGlobalArgs,
    paths: Vec<String>,
    scan: bool,
    sink: &LogSink,
) -> Result<String, String> {
    let staged = Arc::new(Mutex::new(0u64));
    let outcome = Arc::new(Mutex::new(Outcome::default()));

    let counter = staged.clone();
    let sink_clone = sink.clone();
    let cb = callback(sink, &outcome, move |event| {
        if let LoreEvent::FileStageFile(data) = event {
            *counter.lock() += 1;
            log_push(&sink_clone, format!("staged {}", data.path.as_str()));
        }
    });

    let args = LoreFileStageArgs {
        paths: LoreArray::from_vec(paths.iter().map(LoreString::from).collect()),
        case_change: 0,
        scan: scan as u8,
    };

    lore::runtime().block_on(lore::file::stage(globals, args, cb));

    let count = *staged.lock();
    let outcome = std::mem::take(&mut *outcome.lock());
    outcome.into_result(format!("staged {count} file(s)"))
}

fn unstage(globals: LoreGlobalArgs, paths: Vec<String>, sink: &LogSink) -> Result<String, String> {
    let outcome = Arc::new(Mutex::new(Outcome::default()));
    let count = paths.len();

    let sink_clone = sink.clone();
    let cb = callback(sink, &outcome, move |event| {
        if let LoreEvent::FileUnstageFile(data) = event {
            log_push(&sink_clone, format!("unstaged {}", data.path.as_str()));
        }
    });

    let args = LoreFileUnstageArgs {
        paths: LoreArray::from_vec(paths.iter().map(LoreString::from).collect()),
    };

    lore::runtime().block_on(lore::file::unstage(globals, args, cb));

    let outcome = std::mem::take(&mut *outcome.lock());
    outcome.into_result(format!("unstaged {count} path(s)"))
}

fn commit(globals: LoreGlobalArgs, message: String, sink: &LogSink) -> Result<String, String> {
    let committed = Arc::new(Mutex::new(None));
    let outcome = Arc::new(Mutex::new(Outcome::default()));

    let collector = committed.clone();
    let cb = callback(sink, &outcome, move |event| {
        if let LoreEvent::RevisionCommitRevision(data) = event {
            *collector.lock() = Some((data.revision_number, data.revision.to_string()));
        }
    });

    let args = LoreRevisionCommitArgs {
        message: LoreString::from(&message),
        link: LoreString::from(""),
        link_paths: LoreArray::default(),
        link_messages: LoreArray::default(),
        layer: LoreString::from(""),
        layer_paths: LoreArray::default(),
        layer_messages: LoreArray::default(),
        stats: 0,
    };

    lore::runtime().block_on(lore::revision::commit(globals, args, cb));

    let committed = committed.lock().take();
    let outcome = std::mem::take(&mut *outcome.lock());
    let summary = match committed {
        Some((number, revision)) => {
            let short: String = revision.chars().take(12).collect();
            format!("committed revision {number} ({short})")
        }
        None => "commit complete".to_string(),
    };

    outcome.into_result(summary)
}

fn push(globals: LoreGlobalArgs, sink: &LogSink) -> Result<String, String> {
    let pushed = Arc::new(Mutex::new(None));
    let outcome = Arc::new(Mutex::new(Outcome::default()));

    let collector = pushed.clone();
    let cb = callback(sink, &outcome, move |event| {
        if let LoreEvent::BranchPush(data) = event {
            *collector.lock() = Some((
                data.branch_name.as_str().to_string(),
                data.local_history,
                data.flag_already_pushed != 0,
            ));
        }
    });

    let args = LoreBranchPushArgs {
        branch: LoreString::from(""),
        fast_forward_merge: 0,
    };

    lore::runtime().block_on(lore::branch::push(globals, args, cb));

    let pushed = pushed.lock().take();
    let outcome = std::mem::take(&mut *outcome.lock());
    let summary = match pushed {
        Some((branch, _, true)) => format!("{branch} already up to date"),
        Some((branch, count, false)) => format!("pushed {count} revision(s) to {branch}"),
        None => "push complete".to_string(),
    };

    outcome.into_result(summary)
}

fn sync(globals: LoreGlobalArgs, sink: &LogSink) -> Result<String, String> {
    let synced = Arc::new(Mutex::new(None));
    let files = Arc::new(Mutex::new(0u64));
    let outcome = Arc::new(Mutex::new(Outcome::default()));

    let collector = synced.clone();
    let counter = files.clone();
    let sink_clone = sink.clone();
    let cb = callback(sink, &outcome, move |event| match event {
        LoreEvent::RevisionSyncRevision(data) => {
            *collector.lock() = Some((
                data.revision_number,
                data.flag_merge != 0,
                data.flag_conflict != 0,
            ));
        }
        LoreEvent::RevisionSyncFile(data) => {
            *counter.lock() += 1;
            log_push(&sink_clone, format!("sync {}", data.path.as_str()));
        }
        _ => {}
    });

    let args = LoreRevisionSyncArgs {
        revision: LoreString::from(""),
        forward_changes: 0,
        reset: 0,
        root_files: LoreArray::default(),
        dependency_tags: LoreArray::default(),
        dependency_recursive: 0,
        dependency_depth_limit: 0,
    };

    lore::runtime().block_on(lore::revision::sync(globals, args, cb));

    let synced = synced.lock().take();
    let count = *files.lock();
    let outcome = std::mem::take(&mut *outcome.lock());
    let summary = match synced {
        Some((_, _, true)) => format!("synced {count} file(s), merge has conflicts"),
        Some((_, true, false)) => format!("synced {count} file(s), staged a merge"),
        Some((number, false, false)) => format!("synced to revision {number}, {count} file(s)"),
        None => format!("sync complete, {count} file(s)"),
    };

    outcome.into_result(summary)
}

/// Number of unchanged context lines around each hunk. Matches Lore's own
/// `DEFAULT_CONTEXT_LINES`, which is not re-exported through `lore::interface`.
const DIFF_CONTEXT_LINES: u32 = 3;

/// Diff the given paths. Both revisions are left empty, which Lore reads as
/// "the current revision" and "the current filesystem state" — the working copy
/// change, which is what the status list is showing.
fn diff(
    globals: LoreGlobalArgs,
    paths: Vec<String>,
    sink: &LogSink,
) -> Result<Vec<DiffEntry>, String> {
    let entries: Arc<Mutex<Vec<DiffEntry>>> = Arc::new(Mutex::new(Vec::new()));
    let outcome = Arc::new(Mutex::new(Outcome::default()));

    let collector = entries.clone();
    let cb = callback(sink, &outcome, move |event| {
        if let LoreEvent::FileDiff(data) = event {
            collector.lock().push(DiffEntry {
                path: data.path.as_str().to_string(),
                action: action_char(data.action),
                patch: data.patch.as_str().to_string(),
            });
        }
    });

    let args = LoreFileDiffArgs {
        paths: LoreArray::from_vec(paths.iter().map(LoreString::from).collect()),
        source_revision: LoreString::from(""),
        target_revision: LoreString::from(""),
        diff3: 0,
        context_lines: DIFF_CONTEXT_LINES,
        ignore_whitespace_eol: 0,
        ignore_whitespace_inline: 0,
    };

    lore::runtime().block_on(lore::file::diff(globals, args, cb));

    let outcome = std::mem::take(&mut *outcome.lock());
    outcome.into_result(String::new())?;

    let entries = std::mem::take(&mut *entries.lock());
    Ok(entries)
}

/// Every lock on the current branch. Querying with all filters empty is one
/// round trip for the whole repository, where `lock::file_status` would need
/// the paths up front and answer only for those.
fn locks(globals: LoreGlobalArgs, sink: &LogSink) -> Result<Vec<LockEntry>, String> {
    let entries: Arc<Mutex<Vec<LockEntry>>> = Arc::new(Mutex::new(Vec::new()));
    let outcome = Arc::new(Mutex::new(Outcome::default()));

    let collector = entries.clone();
    let cb = callback(sink, &outcome, move |event| {
        if let LoreEvent::LockFileQuery(data) = event {
            collector.lock().push(LockEntry {
                path: data.path.as_str().to_string(),
                owner: data.owner.as_str().to_string(),
                locked_at: data.locked_at,
            });
        }
    });

    let args = LoreLockFileQueryArgs {
        branch: LoreString::from(""),
        owner: LoreString::from(""),
        path: LoreString::from(""),
    };

    lore::runtime().block_on(lore::lock::file_query(globals, args, cb));

    let outcome = std::mem::take(&mut *outcome.lock());
    outcome.into_result(String::new())?;

    let entries = std::mem::take(&mut *entries.lock());
    Ok(entries)
}

fn lock(globals: LoreGlobalArgs, paths: Vec<String>, sink: &LogSink) -> Result<String, String> {
    let acquired = Arc::new(Mutex::new(0u64));
    let ignored = Arc::new(Mutex::new(false));
    let outcome = Arc::new(Mutex::new(Outcome::default()));

    let counter = acquired.clone();
    let already = ignored.clone();
    let sink_clone = sink.clone();
    let cb = callback(sink, &outcome, move |event| match event {
        LoreEvent::LockFileAcquireBegin(data) => {
            if data.ignored != 0 {
                *already.lock() = true;
            }
        }
        LoreEvent::LockFileAcquire(data) => {
            *counter.lock() += 1;
            log_push(&sink_clone, format!("locked {}", data.path.as_str()));
        }
        _ => {}
    });

    let args = LoreLockFileAcquireArgs {
        paths: LoreArray::from_vec(paths.iter().map(LoreString::from).collect()),
        branch: LoreString::from(""),
    };

    lore::runtime().block_on(lore::lock::file_acquire(globals, args, cb));

    let count = *acquired.lock();
    let ignored = *ignored.lock();
    let outcome = std::mem::take(&mut *outcome.lock());
    let summary = if ignored {
        format!("already held the lock on {count} file(s)")
    } else {
        format!("locked {count} file(s)")
    };

    outcome.into_result(summary)
}

/// Release locks. An empty owner means "mine", which is the only release the
/// TUI offers — taking someone else's lock is not a keystroke decision.
fn unlock(globals: LoreGlobalArgs, paths: Vec<String>, sink: &LogSink) -> Result<String, String> {
    let released = Arc::new(Mutex::new(0u64));
    let missing = Arc::new(Mutex::new(false));
    let outcome = Arc::new(Mutex::new(Outcome::default()));

    let counter = released.clone();
    let not_found = missing.clone();
    let sink_clone = sink.clone();
    let cb = callback(sink, &outcome, move |event| match event {
        LoreEvent::LockFileReleaseBegin(data) => {
            if data.not_found != 0 {
                *not_found.lock() = true;
            }
        }
        LoreEvent::LockFileRelease(data) => {
            *counter.lock() += 1;
            log_push(&sink_clone, format!("unlocked {}", data.path.as_str()));
        }
        _ => {}
    });

    let args = LoreLockFileReleaseArgs {
        paths: LoreArray::from_vec(paths.iter().map(LoreString::from).collect()),
        branch: LoreString::from(""),
        owner: LoreString::from(""),
        owner_id: LoreString::from(""),
    };

    lore::runtime().block_on(lore::lock::file_release(globals, args, cb));

    let count = *released.lock();
    let missing = *missing.lock();
    let outcome = std::mem::take(&mut *outcome.lock());
    let summary = if missing {
        "no lock held on those files".to_string()
    } else {
        format!("unlocked {count} file(s)")
    };

    outcome.into_result(summary)
}

/// Subscribe to server push notifications.
///
/// The call returns as soon as the subscription is registered; it does not
/// block for the lifetime of the stream. Lore keeps delivering events to this
/// callback afterwards because `LORE_CONTEXT` — which carries the event
/// dispatcher, and therefore this callback — is a task-local that Lore
/// propagates into the background task reading the notification stream. So the
/// callback outlives the call, and writes into a sink the UI drains per frame
/// rather than into anything tied to this request.
fn subscribe(
    globals: LoreGlobalArgs,
    notices: NoticeSink,
    sink: &LogSink,
) -> Result<String, String> {
    let outcome = Arc::new(Mutex::new(Outcome::default()));

    let cb = callback(sink, &outcome, move |event| {
        let notice = match event {
            LoreEvent::NotificationSubscribed(_) => Notice::Subscribed,
            LoreEvent::NotificationBranchPushed(data) => Notice::BranchPushed {
                revision_number: data.revision_number,
                user: data.user_id.as_str().to_string(),
            },
            LoreEvent::NotificationBranchCreated(_) => Notice::BranchCreated,
            LoreEvent::NotificationBranchDeleted(_) => Notice::BranchDeleted,
            LoreEvent::NotificationResourceLocked(data) => Notice::Locked {
                paths: data
                    .paths
                    .as_slice()
                    .iter()
                    .map(|path| path.as_str().to_string())
                    .collect(),
                user: data.user_id.as_str().to_string(),
            },
            LoreEvent::NotificationResourceUnlocked(data) => Notice::Unlocked {
                paths: data
                    .paths
                    .as_slice()
                    .iter()
                    .map(|path| path.as_str().to_string())
                    .collect(),
                user: data.user_id.as_str().to_string(),
            },
            _ => return,
        };

        notices.lock().push(notice);
    });

    let args = LoreNotificationSubscribeArgs {};

    lore::runtime().block_on(lore::notification::subscribe(globals, args, cb));

    let outcome = std::mem::take(&mut *outcome.lock());
    outcome.into_result("subscribed to server notifications".to_string())
}

/// Read the remote this working copy was created against, which is also the
/// server whose repositories the browser lists. Empty when the repository has
/// no remote configured.
fn remote_url(globals: LoreGlobalArgs, sink: &LogSink) -> Result<String, String> {
    let url = Arc::new(Mutex::new(String::new()));
    let outcome = Arc::new(Mutex::new(Outcome::default()));

    let collector = url.clone();
    let cb = callback(sink, &outcome, move |event| {
        if let LoreEvent::RepositoryConfigGet(data) = event {
            *collector.lock() = data.value.as_str().to_string();
        }
    });

    let args = LoreRepositoryConfigGetArgs {
        key: LoreString::from("remote_url"),
    };

    lore::runtime().block_on(lore::repository::config_get(globals, args, cb));

    let outcome = std::mem::take(&mut *outcome.lock());
    outcome.into_result(String::new())?;

    let url = std::mem::take(&mut *url.lock());
    Ok(url)
}

fn repos(globals: LoreGlobalArgs, url: String, sink: &LogSink) -> Result<Vec<RepoEntry>, String> {
    let entries: Arc<Mutex<Vec<RepoEntry>>> = Arc::new(Mutex::new(Vec::new()));
    let outcome = Arc::new(Mutex::new(Outcome::default()));

    let collector = entries.clone();
    let cb = callback(sink, &outcome, move |event| {
        if let LoreEvent::RepositoryListEntry(data) = event {
            collector.lock().push(RepoEntry {
                id: data.id.to_string(),
                name: data.name.as_str().to_string(),
            });
        }
    });

    let args = LoreRepositoryListArgs {
        url: LoreString::from(&url),
    };

    lore::runtime().block_on(lore::repository::list(globals, args, cb));

    let outcome = std::mem::take(&mut *outcome.lock());
    outcome.into_result(String::new())?;

    let mut entries = std::mem::take(&mut *entries.lock());
    entries.sort_by(|left, right| left.name.cmp(&right.name));
    Ok(entries)
}

fn repo_info(globals: LoreGlobalArgs, url: String, sink: &LogSink) -> Result<RepoInfo, String> {
    let info = Arc::new(Mutex::new(RepoInfo::default()));
    let outcome = Arc::new(Mutex::new(Outcome::default()));

    let collector = info.clone();
    let cb = callback(sink, &outcome, move |event| {
        if let LoreEvent::RepositoryData(data) = event {
            *collector.lock() = RepoInfo {
                description: data.description.as_str().to_string(),
                default_branch: data.default_branch_name.as_str().to_string(),
                creator: data.creator.as_str().to_string(),
                created: data.created,
                remote_url: data.remote_url.as_str().to_string(),
            };
        }
    });

    let args = LoreRepositoryInfoArgs {
        repository_url: LoreString::from(&url),
    };

    lore::runtime().block_on(lore::repository::info(globals, args, cb));

    let outcome = std::mem::take(&mut *outcome.lock());
    outcome.into_result(String::new())?;

    let info = std::mem::take(&mut *info.lock());
    Ok(info)
}

/// Join a server URL and a repository name into the URL Lore expects.
pub fn repository_url(remote: &str, name: &str) -> String {
    format!("{}/{}", remote.trim_end_matches('/'), name)
}

/// What the UI asks the worker to do.
pub enum Request {
    Status { scan: bool },
    History,
    Diff { paths: Vec<String> },
    Locks,
    Lock { paths: Vec<String> },
    Unlock { paths: Vec<String> },
    Subscribe { notices: NoticeSink },
    RemoteUrl,
    Repos { url: String },
    RepoInfo { url: String, name: String },
    Stage { paths: Vec<String>, scan: bool },
    Unstage { paths: Vec<String> },
    Commit { message: String },
    Push,
    Sync,
}

impl Request {
    pub fn name(&self) -> &'static str {
        match self {
            Request::Status { .. } => "status",
            Request::History => "history",
            Request::Diff { .. } => "diff",
            Request::Locks => "locks",
            Request::Lock { .. } => "lock",
            Request::Unlock { .. } => "unlock",
            Request::Subscribe { .. } => "subscribe",
            Request::RemoteUrl => "remote",
            Request::Repos { .. } => "repositories",
            Request::RepoInfo { .. } => "repository info",
            Request::Stage { .. } => "stage",
            Request::Unstage { .. } => "unstage",
            Request::Commit { .. } => "commit",
            Request::Push => "push",
            Request::Sync => "sync",
        }
    }
}

/// Run `request` on a background thread and deliver the result over `tx`.
///
/// `busy` is cleared before the result is sent so the UI never observes an idle
/// worker with a result still in flight.
pub fn spawn(
    request: Request,
    globals: LoreGlobalArgs,
    sink: LogSink,
    tx: Sender<OpResult>,
    busy: Arc<AtomicBool>,
) {
    busy.store(true, Ordering::SeqCst);
    log_push(&sink, format!("> {}", request.name()));

    std::thread::spawn(move || {
        let name = request.name();
        let result = match request {
            Request::Status { scan } => OpResult::Status(status(globals, scan, &sink)),
            Request::History => OpResult::History(history(globals, 0, &sink)),
            Request::Diff { paths } => OpResult::Diff(diff(globals, paths, &sink)),
            Request::Locks => OpResult::Locks(locks(globals, &sink)),
            Request::Lock { paths } => OpResult::Action {
                name,
                result: lock(globals, paths, &sink),
                reload_history: false,
            },
            Request::Unlock { paths } => OpResult::Action {
                name,
                result: unlock(globals, paths, &sink),
                reload_history: false,
            },
            Request::Subscribe { notices } => OpResult::Action {
                name,
                result: subscribe(globals, notices, &sink),
                reload_history: false,
            },
            Request::RemoteUrl => OpResult::RemoteUrl(remote_url(globals, &sink)),
            Request::Repos { url } => OpResult::Repos(repos(globals, url, &sink)),
            Request::RepoInfo { url, name } => OpResult::RepoInfo {
                result: repo_info(globals, url, &sink),
                name,
            },
            Request::Stage { paths, scan } => OpResult::Action {
                name,
                result: stage(globals, paths, scan, &sink),
                reload_history: false,
            },
            Request::Unstage { paths } => OpResult::Action {
                name,
                result: unstage(globals, paths, &sink),
                reload_history: false,
            },
            Request::Commit { message } => OpResult::Action {
                name,
                result: commit(globals, message, &sink),
                reload_history: true,
            },
            Request::Push => OpResult::Action {
                name,
                result: push(globals, &sink),
                reload_history: true,
            },
            Request::Sync => OpResult::Action {
                name,
                result: sync(globals, &sink),
                reload_history: true,
            },
        };

        busy.store(false, Ordering::SeqCst);
        let _ = tx.send(result);
    });
}
