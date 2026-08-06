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
    /// A merge is staged and waiting to be committed or aborted.
    pub merging: bool,
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

/// A branch, as `branch::list` reports it. The same name can appear twice, once
/// per location, when a local branch also exists on the server.
#[derive(Clone, Debug)]
pub struct BranchEntry {
    pub name: String,
    pub category: String,
    pub latest: String,
    pub creator: String,
    /// Milliseconds since the Unix epoch.
    pub created: u64,
    pub current: bool,
    pub archived: bool,
    pub remote: bool,
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

/// One entry in a revision's file tree.
#[derive(Clone, Debug)]
pub struct TreeEntry {
    pub name: String,
    pub directory: bool,
    pub size: u64,
}

/// One revision in which a given file changed.
#[derive(Clone, Debug, Default)]
pub struct FileHistoryEntry {
    pub number: u64,
    pub revision: String,
    pub action: char,
    pub size: u64,
    pub merge: bool,
    pub message: String,
    pub author: String,
    pub timestamp: u64,
}

impl FileHistoryEntry {
    pub fn subject(&self) -> &str {
        self.message.lines().next().unwrap_or("")
    }
}

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
    Diff {
        /// What to call the diff in the pane title. `None` names the files.
        label: Option<String>,
        result: Result<Vec<DiffEntry>, String>,
    },
    Locks(Result<Vec<LockEntry>, String>),
    FileHistory {
        path: String,
        result: Result<Vec<FileHistoryEntry>, String>,
    },
    Tree {
        path: String,
        result: Result<Vec<TreeEntry>, String>,
    },
    Branches(Result<Vec<BranchEntry>, String>),
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
            // A staged revision that is not the current one, with an incoming
            // revision recorded, is a merge waiting to be finished.
            snapshot.merging = !data.revision_staged.is_zero()
                && data.revision_staged != data.revision
                && !data.revision_merged.is_zero();
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

/// List one directory of a revision's file tree.
///
/// This rides on `repository::dump`, which Lore documents as diagnostic. There
/// is a principled alternative in `revision_tree::list_children`, but it works
/// on an open store handle and a loaded tree handle whose lifetime the caller
/// manages, which is a much larger integration than one call per directory.
///
/// The dump's node names are the contract here: a trailing `/` marks a
/// directory, and names are relative to the node that was dumped. Depth is
/// bounded so only one directory is read at a time — `max_depth` counts the
/// entry node itself, so listing a subdirectory needs two levels and listing
/// the root needs one.
fn tree(
    globals: LoreGlobalArgs,
    revision: String,
    path: String,
    sink: &LogSink,
) -> Result<Vec<TreeEntry>, String> {
    let names: Arc<Mutex<Vec<(String, u64)>>> = Arc::new(Mutex::new(Vec::new()));
    let outcome = Arc::new(Mutex::new(Outcome::default()));

    let collector = names.clone();
    let cb = callback(sink, &outcome, move |event| {
        if let LoreEvent::RepositoryStateDumpNode(data) = event {
            collector
                .lock()
                .push((data.name.as_str().to_string(), data.size));
        }
    });

    let at_root = path.is_empty();
    let args = LoreRepositoryDumpArgs {
        revision: LoreString::from(&revision),
        path: LoreString::from(&path),
        max_depth: if at_root { 1 } else { 2 },
    };

    lore::runtime().block_on(lore::repository::dump(globals, args, cb));

    let outcome = std::mem::take(&mut *outcome.lock());
    outcome.into_result(String::new())?;

    let names = std::mem::take(&mut *names.lock());
    Ok(tree_entries(names, at_root))
}

/// Turn dumped node names into one directory's listing.
///
/// At the root every node is a child of the listing. Below it, the dump leads
/// with the directory itself and prefixes its children with its name, so the
/// leading segment is dropped and the entry itself falls away with it.
fn tree_entries(names: Vec<(String, u64)>, at_root: bool) -> Vec<TreeEntry> {
    let mut entries: Vec<TreeEntry> = names
        .into_iter()
        .filter_map(|(name, size)| {
            let directory = name.ends_with('/');
            let name = name.trim_end_matches('/');

            let name = if at_root {
                name.to_string()
            } else {
                name.split_once('/')?.1.to_string()
            };

            (!name.is_empty()).then_some(TreeEntry {
                name,
                directory,
                size,
            })
        })
        .collect();

    // Directories first, then by name — the order a file browser is read in.
    entries.sort_by(|left, right| {
        right
            .directory
            .cmp(&left.directory)
            .then_with(|| left.name.cmp(&right.name))
    });
    entries
}

/// Revisions in which one file changed. Metadata events follow each entry and
/// belong to it, the same association the revision log uses.
fn file_history(
    globals: LoreGlobalArgs,
    path: String,
    sink: &LogSink,
) -> Result<Vec<FileHistoryEntry>, String> {
    let entries: Arc<Mutex<Vec<FileHistoryEntry>>> = Arc::new(Mutex::new(Vec::new()));
    let outcome = Arc::new(Mutex::new(Outcome::default()));

    let collector = entries.clone();
    let cb = callback(sink, &outcome, move |event| match event {
        LoreEvent::FileHistory(data) => {
            collector.lock().push(FileHistoryEntry {
                number: data.revision_number,
                revision: data.revision.to_string(),
                action: action_char(data.action),
                size: data.size,
                merge: !data.parent[1].is_zero(),
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

    let args = LoreFileHistoryArgs {
        path: LoreString::from(&path),
        revision: LoreString::from(""),
        branch: LoreString::from(""),
        length: 0,
        depth: 0,
    };

    lore::runtime().block_on(lore::file::history(globals, args, cb));

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

/// Replace the message of the most recent revision.
fn amend(globals: LoreGlobalArgs, message: String, sink: &LogSink) -> Result<String, String> {
    let outcome = Arc::new(Mutex::new(Outcome::default()));
    let cb = callback(sink, &outcome, |_| {});

    let args = LoreRevisionAmendArgs {
        message: LoreString::from(&message),
    };

    lore::runtime().block_on(lore::revision::amend(globals, args, cb));

    let outcome = std::mem::take(&mut *outcome.lock());
    outcome.into_result("amended the commit message".to_string())
}

/// Throw away local changes to the given paths, restoring them from the current
/// revision. Untracked files are left alone: `purge` would delete files Lore
/// never knew about, which is not what "undo my edits" should mean.
fn reset(globals: LoreGlobalArgs, paths: Vec<String>, sink: &LogSink) -> Result<String, String> {
    let counts = Arc::new(Mutex::new((0u64, 0u64)));
    let outcome = Arc::new(Mutex::new(Outcome::default()));

    let collector = counts.clone();
    let sink_clone = sink.clone();
    let cb = callback(sink, &outcome, move |event| match event {
        LoreEvent::FileResetFile(data) => {
            log_push(&sink_clone, format!("reset {}", data.path.as_str()));
        }
        LoreEvent::FileResetEnd(data) => {
            *collector.lock() = (data.count.file_reset_count, data.count.file_delete_count);
        }
        _ => {}
    });

    let args = LoreFileResetArgs {
        paths: LoreArray::from_vec(paths.iter().map(LoreString::from).collect()),
        revision: LoreString::from(""),
        purge: 0,
    };

    lore::runtime().block_on(lore::file::reset(globals, args, cb));

    let (restored, deleted) = *counts.lock();
    let outcome = std::mem::take(&mut *outcome.lock());
    let summary = if deleted > 0 {
        format!("reset {restored} file(s), removed {deleted}")
    } else {
        format!("reset {restored} file(s)")
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

/// Diff the given paths between two revisions. Empty revisions mean "the
/// current revision" and "the current filesystem state", which is the working
/// copy change the status list shows; naming a revision and its parent instead
/// gives what that revision changed. Empty paths cover everything that differs.
fn diff(
    globals: LoreGlobalArgs,
    paths: Vec<String>,
    source: String,
    target: String,
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
        source_revision: LoreString::from(&source),
        target_revision: LoreString::from(&target),
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

fn branches(globals: LoreGlobalArgs, sink: &LogSink) -> Result<Vec<BranchEntry>, String> {
    let entries: Arc<Mutex<Vec<BranchEntry>>> = Arc::new(Mutex::new(Vec::new()));
    let outcome = Arc::new(Mutex::new(Outcome::default()));

    let collector = entries.clone();
    let cb = callback(sink, &outcome, move |event| {
        if let LoreEvent::BranchListEntry(data) = event {
            collector.lock().push(BranchEntry {
                name: data.name.as_str().to_string(),
                category: data.category.as_str().to_string(),
                latest: data.latest.to_string(),
                creator: data.creator.as_str().to_string(),
                created: data.created,
                current: data.is_current != 0,
                archived: data.archived != 0,
                remote: data.location == LoreBranchLocation::Remote,
            });
        }
    });

    let args = LoreBranchListArgs { archived: 0 };

    lore::runtime().block_on(lore::branch::list(globals, args, cb));

    let outcome = std::mem::take(&mut *outcome.lock());
    outcome.into_result(String::new())?;

    let mut entries = std::mem::take(&mut *entries.lock());
    // Local first, then by name, so the branch you are on sits with the ones
    // you can switch to rather than among the server's copies.
    entries.sort_by(|left, right| {
        left.remote
            .cmp(&right.remote)
            .then_with(|| left.name.cmp(&right.name))
    });
    Ok(entries)
}

/// Merge `branch` into the current one. Lore commits the merge itself when
/// nothing conflicts; when something does, the conflicted files land in the
/// status view's unmerged section and wait to be resolved.
fn merge_start(
    globals: LoreGlobalArgs,
    branch: String,
    message: String,
    sink: &LogSink,
) -> Result<String, String> {
    let conflicts = Arc::new(Mutex::new(0u64));
    let outcome = Arc::new(Mutex::new(Outcome::default()));

    let counter = conflicts.clone();
    let sink_clone = sink.clone();
    let cb = callback(sink, &outcome, move |event| {
        if let LoreEvent::BranchMergeConflictFile(data) = event {
            *counter.lock() += 1;
            log_push(&sink_clone, format!("conflict {}", data.path.as_str()));
        }
    });

    let args = LoreBranchMergeStartArgs {
        branch: LoreString::from(&branch),
        message: LoreString::from(&message),
        no_commit: 0,
        link: LoreString::from(""),
        ignore_links: 0,
    };

    lore::runtime().block_on(lore::branch::merge_start(globals, args, cb));

    let conflicts = *conflicts.lock();
    let outcome = std::mem::take(&mut *outcome.lock());
    let summary = if conflicts > 0 {
        format!("merged {branch}, {conflicts} file(s) conflict")
    } else {
        format!("merged {branch}")
    };

    outcome.into_result(summary)
}

/// How a conflicted file is being settled.
#[derive(Clone, Copy)]
pub enum Resolution {
    /// Keep the local side.
    Mine,
    /// Take the incoming side.
    Theirs,
    /// Accept the file as it stands on disk, after a manual edit.
    AsIs,
}

impl Resolution {
    fn verb(self) -> &'static str {
        match self {
            Resolution::Mine => "kept",
            Resolution::Theirs => "took",
            Resolution::AsIs => "resolved",
        }
    }
}

fn resolve(
    globals: LoreGlobalArgs,
    paths: Vec<String>,
    how: Resolution,
    sink: &LogSink,
) -> Result<String, String> {
    let resolved = Arc::new(Mutex::new(0u64));
    let outcome = Arc::new(Mutex::new(Outcome::default()));

    let counter = resolved.clone();
    let sink_clone = sink.clone();
    // Two different events, because the three resolutions take two different
    // routes: keeping one side stages that side's content and reports
    // `FileStageFile`, while accepting the file as it stands only flags the
    // node and reports `BranchMergeResolveFile`.
    let cb = callback(sink, &outcome, move |event| {
        let path = match event {
            LoreEvent::BranchMergeResolveFile(data) => data.path.as_str(),
            LoreEvent::FileStageFile(data) => data.path.as_str(),
            _ => return,
        };

        *counter.lock() += 1;
        log_push(&sink_clone, format!("{} {path}", how.verb()));
    });

    let paths = LoreArray::from_vec(paths.iter().map(LoreString::from).collect());

    // The return code is redundant: `Complete` already recorded the outcome.
    match how {
        Resolution::Mine => lore::runtime().block_on(lore::branch::merge_resolve_mine(
            globals,
            LoreBranchMergeResolveMineArgs { paths },
            cb,
        )),
        Resolution::Theirs => lore::runtime().block_on(lore::branch::merge_resolve_theirs(
            globals,
            LoreBranchMergeResolveTheirsArgs { paths },
            cb,
        )),
        Resolution::AsIs => lore::runtime().block_on(lore::branch::merge_resolve(
            globals,
            LoreBranchMergeResolveArgs { paths },
            cb,
        )),
    };

    let count = *resolved.lock();
    let outcome = std::mem::take(&mut *outcome.lock());
    outcome.into_result(format!("{} {count} file(s)", how.verb()))
}

fn merge_abort(globals: LoreGlobalArgs, sink: &LogSink) -> Result<String, String> {
    let outcome = Arc::new(Mutex::new(Outcome::default()));
    let cb = callback(sink, &outcome, |_| {});

    let args = LoreBranchMergeAbortArgs {
        link: LoreString::from(""),
        ignore_links: 0,
    };

    lore::runtime().block_on(lore::branch::merge_abort(globals, args, cb));

    let outcome = std::mem::take(&mut *outcome.lock());
    outcome.into_result("merge aborted".to_string())
}

fn switch_branch(
    globals: LoreGlobalArgs,
    name: String,
    sink: &LogSink,
) -> Result<String, String> {
    let outcome = Arc::new(Mutex::new(Outcome::default()));
    let cb = callback(sink, &outcome, |_| {});

    let args = LoreBranchSwitchArgs {
        branch: LoreString::from(&name),
        revision: LoreString::from(""),
        // Local modifications are the user's work; a branch switch must not
        // quietly throw them away.
        reset: 0,
        bare: 0,
    };

    lore::runtime().block_on(lore::branch::switch(globals, args, cb));

    let outcome = std::mem::take(&mut *outcome.lock());
    outcome.into_result(format!("switched to {name}"))
}

fn create_branch(
    globals: LoreGlobalArgs,
    name: String,
    sink: &LogSink,
) -> Result<String, String> {
    let outcome = Arc::new(Mutex::new(Outcome::default()));
    let cb = callback(sink, &outcome, |_| {});

    let args = LoreBranchCreateArgs {
        branch: LoreString::from(&name),
        category: LoreString::from(""),
        id: LoreString::from(""),
    };

    lore::runtime().block_on(lore::branch::create(globals, args, cb));

    let outcome = std::mem::take(&mut *outcome.lock());
    outcome.into_result(format!("created branch {name}"))
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
    Diff {
        paths: Vec<String>,
        source: String,
        target: String,
        label: Option<String>,
    },
    FileHistory { path: String },
    Tree { revision: String, path: String },
    Locks,
    Lock { paths: Vec<String> },
    Unlock { paths: Vec<String> },
    Subscribe { notices: NoticeSink },
    Branches,
    SwitchBranch { name: String },
    CreateBranch { name: String },
    MergeStart { branch: String, message: String },
    Resolve { paths: Vec<String>, how: Resolution },
    MergeAbort,
    RemoteUrl,
    Repos { url: String },
    RepoInfo { url: String, name: String },
    Stage { paths: Vec<String>, scan: bool },
    Unstage { paths: Vec<String> },
    Commit { message: String },
    Amend { message: String },
    Reset { paths: Vec<String> },
    Push,
    Sync,
}

impl Request {
    pub fn name(&self) -> &'static str {
        match self {
            Request::Status { .. } => "status",
            Request::History => "history",
            Request::Diff { .. } => "diff",
            Request::FileHistory { .. } => "file history",
            Request::Tree { .. } => "tree",
            Request::Locks => "locks",
            Request::Lock { .. } => "lock",
            Request::Unlock { .. } => "unlock",
            Request::Subscribe { .. } => "subscribe",
            Request::Branches => "branches",
            Request::SwitchBranch { .. } => "switch branch",
            Request::CreateBranch { .. } => "create branch",
            Request::MergeStart { .. } => "merge",
            Request::Resolve { .. } => "resolve",
            Request::MergeAbort => "abort merge",
            Request::RemoteUrl => "remote",
            Request::Repos { .. } => "repositories",
            Request::RepoInfo { .. } => "repository info",
            Request::Stage { .. } => "stage",
            Request::Unstage { .. } => "unstage",
            Request::Commit { .. } => "commit",
            Request::Amend { .. } => "amend",
            Request::Reset { .. } => "reset",
            Request::Push => "push",
            Request::Sync => "sync",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(path: &str) -> FileEntry {
        FileEntry {
            path: path.to_string(),
            action: 'M',
            size: 0,
            conflict: false,
        }
    }

    #[test]
    fn file_target_is_the_destination_of_a_move() {
        assert_eq!(entry("notes.txt").target(), "notes.txt");
        assert_eq!(entry("old.txt -> new.txt").target(), "new.txt");
        // A path may legitimately contain the separator's characters.
        assert_eq!(entry("a -> b -> c").target(), "c");
    }

    #[test]
    fn root_listing_keeps_every_node() {
        let entries = tree_entries(
            vec![
                ("README.md".into(), 46),
                ("assets/".into(), 0),
                ("hello.txt".into(), 22),
            ],
            true,
        );

        let names: Vec<&str> = entries.iter().map(|entry| entry.name.as_str()).collect();
        // Directories first, then alphabetical.
        assert_eq!(names, ["assets", "README.md", "hello.txt"]);
        assert!(entries[0].directory);
        assert!(!entries[1].directory);
        assert_eq!(entries[1].size, 46);
    }

    #[test]
    fn subdirectory_listing_drops_the_directory_itself() {
        let entries = tree_entries(
            vec![
                // The dump leads with the directory that was asked for.
                ("assets/".into(), 0),
                ("assets/textures/".into(), 0),
                ("assets/notes.txt".into(), 39),
            ],
            false,
        );

        let names: Vec<&str> = entries.iter().map(|entry| entry.name.as_str()).collect();
        assert_eq!(names, ["textures", "notes.txt"]);
        assert!(entries[0].directory);
    }

    #[test]
    fn repository_url_joins_without_doubling_the_separator() {
        assert_eq!(
            repository_url("lore://host:41337", "repo"),
            "lore://host:41337/repo"
        );
        assert_eq!(
            repository_url("lore://host:41337/", "repo"),
            "lore://host:41337/repo"
        );
    }

    #[test]
    fn remote_state_reports_the_most_specific_problem_first() {
        let mut status = StatusSnapshot {
            remote_available: true,
            remote_authorized: true,
            remote_branch_exist: true,
            ..Default::default()
        };
        assert_eq!(status.remote_state(), "in sync with remote");

        status.local_ahead = true;
        assert_eq!(status.remote_state(), "ahead of remote");

        status.remote_ahead = true;
        assert_eq!(status.remote_state(), "diverged from remote");

        status.local_ahead = false;
        assert_eq!(status.remote_state(), "behind remote");

        // Being unreachable outranks anything the counters say.
        status.remote_branch_exist = false;
        assert_eq!(status.remote_state(), "remote branch missing");
        status.remote_authorized = false;
        assert_eq!(status.remote_state(), "remote not authorized");
        status.remote_available = false;
        assert_eq!(status.remote_state(), "remote unavailable");
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
            Request::Diff {
                paths,
                source,
                target,
                label,
            } => OpResult::Diff {
                result: diff(globals, paths, source, target, &sink),
                label,
            },
            Request::FileHistory { path } => OpResult::FileHistory {
                result: file_history(globals, path.clone(), &sink),
                path,
            },
            Request::Tree { revision, path } => OpResult::Tree {
                result: tree(globals, revision, path.clone(), &sink),
                path,
            },
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
            Request::Branches => OpResult::Branches(branches(globals, &sink)),
            Request::SwitchBranch { name: branch } => OpResult::Action {
                name,
                result: switch_branch(globals, branch, &sink),
                reload_history: true,
            },
            Request::CreateBranch { name: branch } => OpResult::Action {
                name,
                result: create_branch(globals, branch, &sink),
                reload_history: false,
            },
            Request::MergeStart { branch, message } => OpResult::Action {
                name,
                result: merge_start(globals, branch, message, &sink),
                // A clean merge commits itself, so the log has moved.
                reload_history: true,
            },
            Request::Resolve { paths, how } => OpResult::Action {
                name,
                result: resolve(globals, paths, how, &sink),
                reload_history: false,
            },
            Request::MergeAbort => OpResult::Action {
                name,
                result: merge_abort(globals, &sink),
                reload_history: true,
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
            Request::Amend { message } => OpResult::Action {
                name,
                result: amend(globals, message, &sink),
                reload_history: true,
            },
            Request::Reset { paths } => OpResult::Action {
                name,
                result: reset(globals, paths, &sink),
                reload_history: false,
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
