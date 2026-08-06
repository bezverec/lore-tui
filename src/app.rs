// SPDX-License-Identifier: MIT
//! TUI state and the key handling that drives it.

use std::collections::HashMap;
use std::collections::HashSet;
use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use std::sync::mpsc::Receiver;
use std::sync::mpsc::Sender;
use std::sync::mpsc::channel;

use lore::interface::LoreGlobalArgs;
use parking_lot::Mutex;
use ratatui::crossterm::event::KeyCode;
use ratatui::crossterm::event::KeyEvent;
use ratatui::crossterm::event::KeyModifiers;

use crate::ops;
use crate::ops::BranchEntry;
use crate::ops::DiffEntry;
use crate::ops::FileEntry;
use crate::ops::FileHistoryEntry;
use crate::ops::HistoryEntry;
use crate::ops::LockEntry;
use crate::ops::LogSink;
use crate::ops::Notice;
use crate::ops::NoticeSink;
use crate::ops::OpResult;
use crate::ops::RepoEntry;
use crate::ops::RepoInfo;
use crate::ops::Request;
use crate::ops::Resolution;
use crate::ops::Section;
use crate::ops::StatusSnapshot;
use crate::ops::TreeEntry;

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum View {
    Status,
    History,
    Branches,
    Repos,
    /// Reached with `d` rather than the view cycle, since it shows one specific
    /// diff rather than a standing list.
    Diff,
    /// Reached with `h` from the status view: the revisions of one file.
    FileHistory,
    /// Reached with `t` from the history view: the files of one revision.
    Tree,
}

/// How a diff line should read. Classified once when the diff lands, so
/// scrolling does not re-parse the patch on every frame.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DiffKind {
    File,
    Hunk,
    Add,
    Delete,
    Conflict,
    Context,
}

pub struct DiffLine {
    pub kind: DiffKind,
    pub text: String,
}

/// A rendered line in the status view. Headers are selectable targets for
/// nothing, so navigation steps over them.
#[derive(Clone, Copy)]
pub enum Row {
    Header(Section, usize),
    File(Section, usize),
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ops::FileEntry;

    fn file(path: &str) -> FileEntry {
        FileEntry {
            path: path.to_string(),
            action: 'M',
            size: 0,
            conflict: false,
        }
    }

    /// An app holding one status snapshot and nothing else.
    fn app_with(staged: &[&str], unmerged: &[&str], unstaged: &[&str]) -> App {
        let mut app = App::bare(LoreGlobalArgs::default(), String::new());
        app.status = Some(StatusSnapshot {
            staged: staged.iter().map(|path| file(path)).collect(),
            unmerged: unmerged.iter().map(|path| file(path)).collect(),
            unstaged: unstaged.iter().map(|path| file(path)).collect(),
            ..Default::default()
        });
        app.rebuild_rows();
        app
    }

    fn paths(targets: Targets) -> Vec<String> {
        match targets {
            Targets::Paths(paths) => paths,
            Targets::Empty => panic!("expected paths, got Empty"),
            Targets::Inapplicable => panic!("expected paths, got Inapplicable"),
        }
    }

    #[test]
    fn selection_skips_section_headers() {
        let app = app_with(&["a.txt"], &[], &["b.txt"]);

        // Row 0 is the "Staged" header, so the cursor starts on the file below.
        assert!(matches!(app.rows[0], Row::Header(..)));
        assert_eq!(app.selected_file().map(|(_, path)| path), Some("a.txt"));
    }

    #[test]
    fn targets_fall_back_to_the_selection_when_nothing_is_marked() {
        let app = app_with(&[], &[], &["a.txt", "b.txt"]);
        assert_eq!(paths(app.targets(&[Section::Unstaged])), ["a.txt"]);
    }

    #[test]
    fn targets_reject_a_selection_in_the_wrong_section() {
        let app = app_with(&["a.txt"], &[], &[]);
        assert!(matches!(
            app.targets(&[Section::Unstaged]),
            Targets::Inapplicable
        ));
    }

    #[test]
    fn targets_are_empty_without_a_selection() {
        let app = app_with(&[], &[], &[]);
        assert!(matches!(app.targets(&ALL_SECTIONS), Targets::Empty));
    }

    #[test]
    fn marks_win_over_the_selection_and_are_filtered_by_section() {
        let mut app = app_with(&["staged.txt"], &[], &["unstaged.txt"]);
        app.marked.insert("staged.txt".into());
        app.marked.insert("unstaged.txt".into());

        // Staging only applies to what is not staged yet, and unstaging only
        // to what is — even though both files are marked.
        assert_eq!(paths(app.targets(&[Section::Unstaged])), ["unstaged.txt"]);
        assert_eq!(paths(app.targets(&[Section::Staged])), ["staged.txt"]);
        assert_eq!(
            paths(app.targets(&ALL_SECTIONS)),
            ["staged.txt", "unstaged.txt"]
        );
    }

    #[test]
    fn marks_with_nothing_applicable_are_not_silently_empty() {
        let mut app = app_with(&["staged.txt"], &[], &[]);
        app.marked.insert("staged.txt".into());
        assert!(matches!(
            app.targets(&[Section::Unstaged]),
            Targets::Inapplicable
        ));
    }

    #[test]
    fn marks_do_not_survive_the_file_they_name() {
        let mut app = app_with(&[], &[], &["gone.txt", "stays.txt"]);
        app.marked.insert("gone.txt".into());
        app.marked.insert("stays.txt".into());

        app.status = Some(StatusSnapshot {
            unstaged: vec![file("stays.txt")],
            ..Default::default()
        });
        app.rebuild_rows();

        // Otherwise a file reappearing under the same path would arrive marked.
        assert_eq!(app.marked.len(), 1);
        assert!(app.marked.contains("stays.txt"));
    }

    #[test]
    fn a_moved_file_is_acted_on_by_its_destination() {
        let app = app_with(&[], &[], &["old.txt -> new.txt"]);
        assert_eq!(paths(app.targets(&[Section::Unstaged])), ["new.txt"]);
    }

    #[test]
    fn diff_lines_are_classified_by_their_prefix() {
        assert_eq!(classify_diff_line("+added"), DiffKind::Add);
        assert_eq!(classify_diff_line("-removed"), DiffKind::Delete);
        assert_eq!(classify_diff_line(" context"), DiffKind::Context);
        assert_eq!(classify_diff_line("@@ -1,2 +1,3 @@"), DiffKind::Hunk);
        assert_eq!(classify_diff_line("<<<<<<< ours"), DiffKind::Conflict);
        assert_eq!(classify_diff_line(">>>>>>> theirs"), DiffKind::Conflict);
        assert_eq!(classify_diff_line("======="), DiffKind::Conflict);

        // The patch's own file headers start with + and -, and must not read
        // as an added and a removed line.
        assert_eq!(classify_diff_line("+++ b/file"), DiffKind::Context);
        assert_eq!(classify_diff_line("--- a/file"), DiffKind::Context);
    }

    #[test]
    fn attribution_is_dropped_when_the_server_does_not_know_who_acted() {
        assert_eq!(by("alice"), " by alice");
        assert_eq!(by(""), "");
        assert_eq!(by("<unknown>"), "");
    }
}

/// How one line of a unified diff should read.
fn classify_diff_line(line: &str) -> DiffKind {
    if line.starts_with("<<<<<<<")
        || line.starts_with("|||||||")
        || line.starts_with("=======")
        || line.starts_with(">>>>>>>")
    {
        DiffKind::Conflict
    } else if line.starts_with("@@") {
        DiffKind::Hunk
    } else if line.starts_with("+++") || line.starts_with("---") {
        // File headers of the patch itself; the path is already in the heading
        // above them, so they read as context.
        DiffKind::Context
    } else if line.starts_with('+') {
        DiffKind::Add
    } else if line.starts_with('-') {
        DiffKind::Delete
    } else {
        DiffKind::Context
    }
}

/// Attribution suffix for a notification, omitted when the server does not
/// know who acted — which is every notification on a server without auth.
fn by(user: &str) -> String {
    if user.is_empty() || user == "<unknown>" {
        String::new()
    } else {
        format!(" by {user}")
    }
}

/// Actions that apply to a file whatever its staging state.
const ALL_SECTIONS: [Section; 3] = [Section::Staged, Section::Unmerged, Section::Unstaged];

/// What a file action should operate on.
enum Targets {
    Paths(Vec<String>),
    /// Nothing marked and nothing selected.
    Empty,
    /// Candidates exist, but none of them sit in a section the action applies
    /// to — staging something already staged, say.
    Inapplicable,
}

/// What a text prompt is collecting.
#[derive(Clone, PartialEq, Eq)]
pub enum InputKind {
    CommitMessage,
    BranchName,
    /// Message for the commit a clean merge makes on its own.
    MergeMessage { branch: String },
    /// Replacement message for the most recent revision.
    AmendMessage,
}

impl InputKind {
    pub fn title(&self) -> &'static str {
        match self {
            InputKind::CommitMessage => " Commit message ",
            InputKind::BranchName => " New branch name ",
            InputKind::MergeMessage { .. } => " Merge message ",
            InputKind::AmendMessage => " Amend the last commit message ",
        }
    }
}

pub enum Modal {
    None,
    Input { kind: InputKind, value: String },
    Confirm { request: Option<Request>, prompt: String },
    Help,
}

pub struct App {
    pub globals: LoreGlobalArgs,
    pub repo_path: String,
    pub view: View,
    pub status: Option<StatusSnapshot>,
    pub history: Vec<HistoryEntry>,
    pub rows: Vec<Row>,
    pub status_selected: usize,
    /// Files marked for the next stage or unstage, by the path Lore operates
    /// on. Empty means the action applies to the selection instead.
    pub marked: HashSet<String>,
    pub history_selected: usize,
    /// Server this working copy points at, and the one the browser lists.
    /// `None` until the config read lands; empty when there is no remote.
    /// Locks on the current branch, keyed by the path they apply to.
    pub locks: HashMap<String, LockEntry>,
    pub branches: Vec<BranchEntry>,
    pub branches_selected: usize,
    /// Push notifications waiting to be folded into the state.
    notices: NoticeSink,
    /// Whether the server is pushing changes to us.
    pub live: bool,
    pub tree: Vec<TreeEntry>,
    pub tree_selected: usize,
    /// Directory being listed, empty at the repository root.
    pub tree_path: String,
    /// Revision the tree belongs to, and how to name it.
    tree_revision: String,
    pub tree_label: String,
    pub file_history: Vec<FileHistoryEntry>,
    pub file_history_selected: usize,
    pub file_history_path: String,
    pub diff: Vec<DiffLine>,
    /// View a detour — the diff or a file's history — was opened from, to
    /// return to when it closes.
    detour_return: View,
    pub diff_title: String,
    pub diff_scroll: usize,
    /// Lines the diff pane can show, written by the renderer each frame so
    /// scrolling can stop at the last screenful instead of the last line.
    pub diff_viewport: usize,
    pub remote: Option<String>,
    pub repos: Vec<RepoEntry>,
    pub repos_selected: usize,
    /// Per-repository detail, keyed by name. Failures are cached too, so a
    /// repository that cannot be read is not retried on every frame.
    pub repo_info: HashMap<String, Result<RepoInfo, String>>,
    pub log: LogSink,
    pub busy: Arc<AtomicBool>,
    pub modal: Modal,
    /// Last operation result: the text and whether it was a failure.
    pub message: Option<(String, bool)>,
    pub spinner: usize,
    pub should_quit: bool,
    queue: VecDeque<Request>,
    tx: Sender<OpResult>,
    rx: Receiver<OpResult>,
}

impl App {
    pub fn new(globals: LoreGlobalArgs, repo_path: String) -> Self {
        let mut app = Self::bare(globals, repo_path);

        // The first load scans: the dirty flags on disk may predate this
        // session, and an empty file list that only means "nothing was
        // flagged" would read as "nothing changed".
        app.dispatch(Request::Status { scan: true });
        app.dispatch(Request::History);
        app.dispatch(Request::Locks);
        app.dispatch(Request::Branches);
        app.dispatch(Request::RemoteUrl);
        app.dispatch(Request::Subscribe {
            notices: app.notices.clone(),
        });
        app
    }

    /// State with nothing loaded and nothing asked of Lore. `new` builds on
    /// this, and tests use it to exercise the parts that are pure state.
    fn bare(globals: LoreGlobalArgs, repo_path: String) -> Self {
        let (tx, rx) = channel();

        Self {
            globals,
            repo_path,
            view: View::Status,
            status: None,
            history: Vec::new(),
            rows: Vec::new(),
            status_selected: 0,
            marked: HashSet::new(),
            history_selected: 0,
            locks: HashMap::new(),
            branches: Vec::new(),
            branches_selected: 0,
            notices: Arc::new(Mutex::new(Vec::new())),
            live: false,
            tree: Vec::new(),
            tree_selected: 0,
            tree_path: String::new(),
            tree_revision: String::new(),
            tree_label: String::new(),
            file_history: Vec::new(),
            file_history_selected: 0,
            file_history_path: String::new(),
            diff: Vec::new(),
            detour_return: View::Status,
            diff_title: String::new(),
            diff_scroll: 0,
            diff_viewport: 0,
            remote: None,
            repos: Vec::new(),
            repos_selected: 0,
            repo_info: HashMap::new(),
            log: Arc::new(Mutex::new(Vec::new())),
            busy: Arc::new(AtomicBool::new(false)),
            modal: Modal::None,
            message: None,
            spinner: 0,
            should_quit: false,
            queue: VecDeque::new(),
            tx,
            rx,
        }
    }

    pub fn busy(&self) -> bool {
        self.busy.load(Ordering::SeqCst)
    }

    /// Queue a Lore operation. Only one runs at a time: overlapping calls would
    /// race the snapshot the UI is drawing, and Lore serializes repository
    /// writes regardless. Reloads are collapsed rather than queued twice — a
    /// commit that triggers a refresh while one is already pending should not
    /// run two identical scans back to back.
    pub fn dispatch(&mut self, request: Request) {
        let reload = matches!(request, Request::Status { .. } | Request::History);
        if reload
            && self
                .queue
                .iter()
                .any(|queued| queued.name() == request.name())
        {
            return;
        }

        self.queue.push_back(request);
        self.pump();
    }

    /// Start the next queued operation if nothing is in flight.
    fn pump(&mut self) {
        if self.busy() {
            return;
        }
        let Some(request) = self.queue.pop_front() else {
            return;
        };

        ops::spawn(
            request,
            self.globals.clone(),
            self.log.clone(),
            self.tx.clone(),
            self.busy.clone(),
        );
    }

    /// Drain finished operations and start any queued work. Called once per
    /// frame.
    pub fn poll(&mut self) {
        while let Ok(result) = self.rx.try_recv() {
            match result {
                OpResult::Status(Ok(snapshot)) => {
                    self.status = Some(snapshot);
                    self.rebuild_rows();
                }
                OpResult::Status(Err(error)) => {
                    self.message = Some((format!("status: {error}"), true));
                }
                OpResult::History(Ok(entries)) => {
                    self.history = entries;
                    self.history_selected = self
                        .history_selected
                        .min(self.history.len().saturating_sub(1));
                }
                OpResult::History(Err(error)) => {
                    self.message = Some((format!("history: {error}"), true));
                }
                OpResult::Diff {
                    label,
                    result: Ok(entries),
                } => {
                    if entries.is_empty()
                        || entries.iter().all(|entry| entry.patch.trim().is_empty())
                    {
                        // Lore answers with an empty patch for a file whose
                        // content did not change textually — a binary asset, or
                        // a pure rename.
                        self.message = Some(("nothing to show in a diff".into(), true));
                    } else {
                        self.build_diff(&entries, label);
                        self.enter_detour(View::Diff);
                    }
                }
                OpResult::Diff {
                    result: Err(error), ..
                } => {
                    self.message = Some((format!("diff: {error}"), true));
                }
                OpResult::Locks(Ok(locks)) => {
                    self.locks = locks
                        .into_iter()
                        .map(|lock| (lock.path.clone(), lock))
                        .collect();
                }
                OpResult::Locks(Err(error)) => {
                    self.message = Some((format!("locks: {error}"), true));
                }
                OpResult::FileHistory {
                    path,
                    result: Ok(entries),
                } => {
                    if entries.is_empty() {
                        self.message = Some((format!("{path} has no history yet"), true));
                    } else {
                        self.file_history = entries;
                        self.file_history_selected = 0;
                        self.file_history_path = path;
                        self.enter_detour(View::FileHistory);
                    }
                }
                OpResult::Tree {
                    path,
                    result: Ok(entries),
                } => {
                    self.tree = entries;
                    self.tree_selected = 0;
                    self.tree_path = path;
                    self.enter_detour(View::Tree);
                }
                OpResult::Tree {
                    result: Err(error), ..
                } => {
                    self.message = Some((format!("tree: {error}"), true));
                }
                OpResult::FileHistory {
                    result: Err(error), ..
                } => {
                    // Lore's error already names the file — a file with no
                    // revisions yet reads as "file not found".
                    self.message = Some((format!("history: {error}"), true));
                }
                OpResult::Branches(Ok(branches)) => {
                    self.branches = branches;
                    self.branches_selected = self
                        .branches_selected
                        .min(self.branches.len().saturating_sub(1));
                }
                OpResult::Branches(Err(error)) => {
                    self.message = Some((format!("branches: {error}"), true));
                }
                OpResult::RemoteUrl(Ok(url)) => {
                    self.remote = Some(url);
                    // The browser is only reachable once the server is known,
                    // so the first listing is kicked off from here rather than
                    // from the key that opens the view.
                    self.load_repos();
                }
                OpResult::RemoteUrl(Err(error)) => {
                    self.remote = Some(String::new());
                    self.message = Some((format!("remote: {error}"), true));
                }
                OpResult::Repos(Ok(repos)) => {
                    self.repos = repos;
                    self.repos_selected =
                        self.repos_selected.min(self.repos.len().saturating_sub(1));
                }
                OpResult::Repos(Err(error)) => {
                    self.message = Some((format!("repositories: {error}"), true));
                }
                OpResult::RepoInfo { name, result } => {
                    if let Err(error) = &result {
                        ops::log_push(&self.log, format!("[error] {name}: {error}"));
                    }
                    self.repo_info.insert(name, result);
                }
                OpResult::Action {
                    name,
                    result,
                    reload_history,
                } => {
                    match result {
                        Ok(summary) => {
                            self.message = Some((summary, false));
                            ops::log_push(&self.log, format!("< {name} ok"));
                            // The marks have been acted on; keeping them would
                            // leave the files selected in their new section.
                            if matches!(name, "stage" | "unstage") {
                                self.marked.clear();
                            }
                        }
                        Err(error) => {
                            ops::log_push(&self.log, format!("< {name} failed: {error}"));
                            // Notifications are a bonus nobody asked for, so a
                            // server without them should not greet the user
                            // with a red banner. The log line is enough.
                            if name != "subscribe" {
                                self.message = Some((format!("{name}: {error}"), true));
                            }
                        }
                    }

                    // Every action changes the working state, so the status
                    // view is refreshed either way — including on failure,
                    // where a partial effect is exactly what needs showing.
                    self.dispatch(Request::Status { scan: true });
                    if reload_history {
                        self.dispatch(Request::History);
                    }
                    if matches!(name, "lock" | "unlock") {
                        self.dispatch(Request::Locks);
                    }
                    if matches!(
                        name,
                        "switch branch" | "create branch" | "merge" | "abort merge"
                    ) {
                        self.dispatch(Request::Branches);
                    }
                }
            }
        }

        self.drain_notices();
        self.pump();
        self.autoload_repo_info();
    }

    /// Fold push notifications into the state. The server tells us something
    /// moved; it does not tell us the new state, so each kind of notice turns
    /// into the read that does.
    fn drain_notices(&mut self) {
        let notices: Vec<Notice> = std::mem::take(&mut *self.notices.lock());
        if notices.is_empty() {
            return;
        }

        let mut refresh_status = false;
        let mut refresh_locks = false;
        let mut refresh_branches = false;

        for notice in notices {
            match notice {
                Notice::Subscribed => {
                    self.live = true;
                    ops::log_push(&self.log, "[live] subscribed to server notifications");
                }
                Notice::BranchPushed {
                    revision_number,
                    user,
                } => {
                    ops::log_push(
                        &self.log,
                        format!("[live] revision {revision_number} pushed{}", by(&user)),
                    );
                    refresh_status = true;
                }
                Notice::BranchCreated => {
                    ops::log_push(&self.log, "[live] branch created");
                    refresh_branches = true;
                }
                Notice::BranchDeleted => {
                    ops::log_push(&self.log, "[live] branch deleted");
                    refresh_branches = true;
                }
                Notice::Locked { paths, user } => {
                    ops::log_push(
                        &self.log,
                        format!("[live] locked {}{}", paths.join(", "), by(&user)),
                    );
                    refresh_locks = true;
                }
                Notice::Unlocked { paths, user } => {
                    ops::log_push(
                        &self.log,
                        format!("[live] unlocked {}{}", paths.join(", "), by(&user)),
                    );
                    refresh_locks = true;
                }
            }
        }

        if refresh_status {
            // Someone else pushed: the remote moved, the working copy did not,
            // so this reads the branch state without walking the filesystem.
            self.dispatch(Request::Status { scan: false });
        }
        if refresh_locks {
            self.dispatch(Request::Locks);
        }
        if refresh_branches {
            self.dispatch(Request::Branches);
        }
    }

    /// Queue the listing for the configured server, if there is one.
    fn load_repos(&mut self) {
        match self.remote.as_deref() {
            Some("") | None => {
                self.message = Some(("no remote configured for this repository".into(), true));
            }
            Some(url) => {
                let url = url.to_string();
                self.dispatch(Request::Repos { url });
            }
        }
    }

    /// Fetch detail for the highlighted repository, but only while nothing else
    /// is running. Scrolling the list therefore issues no requests until the
    /// selection settles, which keeps a long list from queueing a call per row.
    fn autoload_repo_info(&mut self) {
        if self.view != View::Repos || self.busy() || !self.queue.is_empty() {
            return;
        }

        let Some(remote) = self.remote.clone().filter(|url| !url.is_empty()) else {
            return;
        };
        let Some(entry) = self.repos.get(self.repos_selected) else {
            return;
        };
        if self.repo_info.contains_key(&entry.name) {
            return;
        }

        let name = entry.name.clone();
        let url = ops::repository_url(&remote, &name);
        self.dispatch(Request::RepoInfo { url, name });
    }

    /// Flatten the patches into styled lines once, ready to scroll.
    fn build_diff(&mut self, entries: &[DiffEntry], label: Option<String>) {
        let mut lines = Vec::new();

        for entry in entries {
            if !lines.is_empty() {
                lines.push(DiffLine {
                    kind: DiffKind::Context,
                    text: String::new(),
                });
            }
            lines.push(DiffLine {
                kind: DiffKind::File,
                text: format!("{}  {}", entry.action, entry.path),
            });

            if entry.patch.trim().is_empty() {
                lines.push(DiffLine {
                    kind: DiffKind::Context,
                    text: "  (no textual diff)".to_string(),
                });
                continue;
            }

            for line in entry.patch.lines() {
                lines.push(DiffLine {
                    kind: classify_diff_line(line),
                    text: line.to_string(),
                });
            }
        }

        self.diff_title = match (label, entries) {
            (Some(label), _) => format!(" Diff · {label} "),
            (None, [entry]) => format!(" Diff · {} ", entry.path),
            (None, entries) => format!(" Diff · {} files ", entries.len()),
        };
        self.diff = lines;
        self.diff_scroll = 0;
    }

    /// Repository-relative path of a child of the directory being listed.
    fn tree_child_path(&self, name: &str) -> String {
        if self.tree_path.is_empty() {
            name.to_string()
        } else {
            format!("{}/{name}", self.tree_path)
        }
    }

    fn load_tree(&mut self, path: String) {
        let revision = self.tree_revision.clone();
        self.dispatch(Request::Tree { revision, path });
    }

    /// Open a view that is a detour from a list, remembering where to go back
    /// to. Re-entering from a detour keeps the original destination.
    fn enter_detour(&mut self, view: View) {
        if !matches!(self.view, View::Diff | View::FileHistory | View::Tree) {
            self.detour_return = self.view;
        }
        self.view = view;
    }

    /// Largest scroll offset that still fills the pane.
    fn diff_max_scroll(&self) -> usize {
        self.diff.len().saturating_sub(self.diff_viewport.max(1))
    }

    pub fn selected_repo(&self) -> Option<&RepoEntry> {
        self.repos.get(self.repos_selected)
    }

    pub fn selected_branch(&self) -> Option<&BranchEntry> {
        self.branches.get(self.branches_selected)
    }

    /// Settle conflicted files. `verb` is only for the message when there is
    /// nothing to settle.
    fn resolve(&mut self, how: Resolution, verb: &str) {
        if self.view != View::Status {
            return;
        }

        match self.targets(&[Section::Unmerged]) {
            Targets::Paths(paths) => self.dispatch(Request::Resolve { paths, how }),
            Targets::Empty => self.message = Some(("no file selected".into(), true)),
            Targets::Inapplicable => {
                self.message = Some((format!("nothing to be {verb} — no conflict here"), true));
            }
        }
    }

    /// Switch views, loading the repository list the first time the browser is
    /// opened after the remote became known.
    fn show(&mut self, view: View) {
        self.view = view;
        if view == View::Repos && self.repos.is_empty() && self.remote.is_some() {
            self.load_repos();
        }
    }

    fn rebuild_rows(&mut self) {
        let mut rows = Vec::new();
        if let Some(status) = &self.status {
            let sections = [
                (Section::Staged, &status.staged),
                (Section::Unmerged, &status.unmerged),
                (Section::Unstaged, &status.unstaged),
            ];
            for (section, files) in sections {
                if files.is_empty() {
                    continue;
                }
                rows.push(Row::Header(section, files.len()));
                for index in 0..files.len() {
                    rows.push(Row::File(section, index));
                }
            }
        }

        // Marks name paths, not list positions, so a refresh that drops a file
        // has to drop its mark too — otherwise it would silently apply to a
        // file that reappears later under the same path.
        let live: HashSet<String> = self
            .files()
            .into_iter()
            .map(|(_, file)| file.target().to_string())
            .collect();
        self.marked.retain(|path| live.contains(path));

        self.rows = rows;
        if self.status_selected >= self.rows.len() {
            self.status_selected = self.rows.len().saturating_sub(1);
        }
        // A header may have landed under the cursor after the rebuild. This
        // steps over it directly rather than through `move_selection`, which
        // acts on whichever view is showing — and a status refresh can land
        // while the user is looking at another one.
        self.step_status_selection(1);
    }

    /// Move the status selection by `delta` until it rests on a file row.
    /// Stays put when there is no file row in that direction.
    fn step_status_selection(&mut self, delta: isize) {
        if !matches!(self.rows.get(self.status_selected), Some(Row::Header(..))) {
            return;
        }

        let mut index = self.status_selected as isize;
        let last = self.rows.len() as isize - 1;
        loop {
            index += delta;
            if index < 0 || index > last {
                return;
            }
            if matches!(self.rows[index as usize], Row::File(..)) {
                self.status_selected = index as usize;
                return;
            }
        }
    }

    pub fn selected_file(&self) -> Option<(Section, &str)> {
        let Row::File(section, index) = *self.rows.get(self.status_selected)? else {
            return None;
        };
        let status = self.status.as_ref()?;
        let files = match section {
            Section::Staged => &status.staged,
            Section::Unmerged => &status.unmerged,
            Section::Unstaged => &status.unstaged,
        };
        Some((section, files.get(index)?.target()))
    }

    /// Every file in the current snapshot, paired with its section.
    fn files(&self) -> Vec<(Section, &FileEntry)> {
        let Some(status) = self.status.as_ref() else {
            return Vec::new();
        };

        [
            (Section::Staged, &status.staged),
            (Section::Unmerged, &status.unmerged),
            (Section::Unstaged, &status.unstaged),
        ]
        .into_iter()
        .flat_map(|(section, files)| files.iter().map(move |file| (section, file)))
        .collect()
    }

    pub fn is_marked(&self, file: &FileEntry) -> bool {
        self.marked.contains(file.target())
    }

    fn toggle_mark(&mut self) {
        if self.view != View::Status {
            return;
        }
        let Some(path) = self.selected_file().map(|(_, path)| path.to_string()) else {
            return;
        };
        if !self.marked.remove(&path) {
            self.marked.insert(path);
        }
    }

    /// Resolve what the next file action runs on. Marks win when there are any;
    /// otherwise it is the selected file, exactly as before marks existed.
    fn targets(&self, allowed: &[Section]) -> Targets {
        if self.marked.is_empty() {
            let Some((section, path)) = self.selected_file() else {
                return Targets::Empty;
            };
            return if allowed.contains(&section) {
                Targets::Paths(vec![path.to_string()])
            } else {
                Targets::Inapplicable
            };
        }

        let mut paths: Vec<String> = self
            .files()
            .into_iter()
            .filter(|(section, file)| allowed.contains(section) && self.is_marked(file))
            .map(|(_, file)| file.target().to_string())
            .collect();
        paths.sort();
        paths.dedup();

        if paths.is_empty() {
            Targets::Inapplicable
        } else {
            Targets::Paths(paths)
        }
    }

    fn move_selection(&mut self, delta: isize) {
        match self.view {
            View::Status => {
                if self.rows.is_empty() {
                    return;
                }
                let mut index = self.status_selected as isize;
                let last = self.rows.len() as isize - 1;
                loop {
                    index += delta;
                    if index < 0 || index > last {
                        return;
                    }
                    if matches!(self.rows[index as usize], Row::File(..)) {
                        self.status_selected = index as usize;
                        return;
                    }
                }
            }
            View::History => {
                if self.history.is_empty() {
                    return;
                }
                let last = self.history.len() as isize - 1;
                let index = (self.history_selected as isize + delta).clamp(0, last);
                self.history_selected = index as usize;
            }
            View::Repos => {
                if self.repos.is_empty() {
                    return;
                }
                let last = self.repos.len() as isize - 1;
                let index = (self.repos_selected as isize + delta).clamp(0, last);
                self.repos_selected = index as usize;
            }
            View::Branches => {
                if self.branches.is_empty() {
                    return;
                }
                let last = self.branches.len() as isize - 1;
                let index = (self.branches_selected as isize + delta).clamp(0, last);
                self.branches_selected = index as usize;
            }
            View::FileHistory => {
                if self.file_history.is_empty() {
                    return;
                }
                let last = self.file_history.len() as isize - 1;
                let index = (self.file_history_selected as isize + delta).clamp(0, last);
                self.file_history_selected = index as usize;
            }
            View::Tree => {
                if self.tree.is_empty() {
                    return;
                }
                let last = self.tree.len() as isize - 1;
                let index = (self.tree_selected as isize + delta).clamp(0, last);
                self.tree_selected = index as usize;
            }
            View::Diff => {
                let last = self.diff_max_scroll() as isize;
                let offset = (self.diff_scroll as isize + delta).clamp(0, last);
                self.diff_scroll = offset as usize;
            }
        }
    }

    fn select_edge(&mut self, first: bool) {
        match self.view {
            View::Status => {
                self.status_selected = if first { 0 } else { self.rows.len().saturating_sub(1) };
                self.step_status_selection(if first { 1 } else { -1 });
            }
            View::History => {
                self.history_selected = if first {
                    0
                } else {
                    self.history.len().saturating_sub(1)
                };
            }
            View::Repos => {
                self.repos_selected = if first {
                    0
                } else {
                    self.repos.len().saturating_sub(1)
                };
            }
            View::Branches => {
                self.branches_selected = if first {
                    0
                } else {
                    self.branches.len().saturating_sub(1)
                };
            }
            View::FileHistory => {
                self.file_history_selected = if first {
                    0
                } else {
                    self.file_history.len().saturating_sub(1)
                };
            }
            View::Tree => {
                self.tree_selected = if first {
                    0
                } else {
                    self.tree.len().saturating_sub(1)
                };
            }
            View::Diff => {
                self.diff_scroll = if first { 0 } else { self.diff_max_scroll() };
            }
        }
    }

    pub fn handle_key(&mut self, key: KeyEvent) {
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
            self.should_quit = true;
            return;
        }

        match &mut self.modal {
            Modal::Input { kind, value } => match key.code {
                KeyCode::Esc => self.modal = Modal::None,
                KeyCode::Enter => {
                    let kind = kind.clone();
                    let value = value.trim().to_string();
                    self.modal = Modal::None;
                    match (kind, value.is_empty()) {
                        (InputKind::CommitMessage, true) => {
                            self.message = Some(("commit needs a message".into(), true));
                        }
                        (InputKind::CommitMessage, false) => {
                            self.dispatch(Request::Commit { message: value });
                        }
                        (InputKind::BranchName, true) => {
                            self.message = Some(("branch needs a name".into(), true));
                        }
                        (InputKind::BranchName, false) => {
                            self.dispatch(Request::CreateBranch { name: value });
                        }
                        (InputKind::MergeMessage { .. }, true) => {
                            self.message = Some(("merge needs a message".into(), true));
                        }
                        (InputKind::MergeMessage { branch }, false) => {
                            self.dispatch(Request::MergeStart {
                                branch,
                                message: value,
                            });
                        }
                        (InputKind::AmendMessage, true) => {
                            self.message = Some(("a commit needs a message".into(), true));
                        }
                        (InputKind::AmendMessage, false) => {
                            self.dispatch(Request::Amend { message: value });
                        }
                    }
                }
                KeyCode::Backspace => {
                    value.pop();
                }
                KeyCode::Char(c) => value.push(c),
                _ => {}
            },
            Modal::Confirm { request, .. } => match key.code {
                KeyCode::Char('y' | 'Y') | KeyCode::Enter => {
                    let request = request.take();
                    self.modal = Modal::None;
                    if let Some(request) = request {
                        self.dispatch(request);
                    }
                }
                _ => self.modal = Modal::None,
            },
            Modal::Help => self.modal = Modal::None,
            Modal::None => self.handle_main_key(key),
        }
    }

    fn handle_main_key(&mut self, key: KeyEvent) {
        // A detour is not a place to live, so everything that means "I am done
        // here" returns to the list it was opened from.
        let leave = match self.view {
            View::Diff => matches!(key.code, KeyCode::Esc | KeyCode::Char('d' | 'q')),
            View::FileHistory => matches!(key.code, KeyCode::Esc | KeyCode::Char('h' | 'q')),
            // Escape steps out of a subdirectory before it leaves the tree.
            View::Tree => {
                matches!(key.code, KeyCode::Char('t' | 'q'))
                    || (key.code == KeyCode::Esc && self.tree_path.is_empty())
            }
            _ => false,
        };
        if leave {
            self.view = self.detour_return;
            return;
        }

        if self.view == View::Tree {
            match key.code {
                KeyCode::Enter => {
                    if let Some(entry) = self.tree.get(self.tree_selected)
                        && entry.directory
                    {
                        let path = self.tree_child_path(&entry.name.clone());
                        self.load_tree(path);
                    }
                    return;
                }
                KeyCode::Esc | KeyCode::Backspace => {
                    let parent = match self.tree_path.rsplit_once('/') {
                        Some((parent, _)) => parent.to_string(),
                        None => String::new(),
                    };
                    self.load_tree(parent);
                    return;
                }
                _ => {}
            }
        }

        match key.code {
            KeyCode::Char('q') => self.should_quit = true,
            KeyCode::Char('?') => self.modal = Modal::Help,
            // Marks are the more disruptive state to be stuck with, so escape
            // clears them first and only then dismisses the message.
            KeyCode::Esc => {
                if self.marked.is_empty() {
                    self.message = None;
                } else {
                    self.marked.clear();
                }
            }

            KeyCode::Tab => {
                let next = match self.view {
                    View::Status => View::History,
                    View::History => View::Branches,
                    View::Branches => View::Repos,
                    // Detours are outside the cycle; tab leaves them.
                    View::Repos | View::Diff | View::FileHistory | View::Tree => View::Status,
                };
                self.show(next);
            }
            KeyCode::Char('1') => self.show(View::Status),
            KeyCode::Char('2') => self.show(View::History),
            KeyCode::Char('3') => self.show(View::Branches),
            KeyCode::Char('4') => self.show(View::Repos),

            KeyCode::Char('j') | KeyCode::Down => self.move_selection(1),
            KeyCode::Char('k') | KeyCode::Up => self.move_selection(-1),
            KeyCode::PageDown => self.move_selection(10),
            KeyCode::PageUp => self.move_selection(-10),
            KeyCode::Char('g') | KeyCode::Home => self.select_edge(true),
            KeyCode::Char('G') | KeyCode::End => self.select_edge(false),

            KeyCode::Char('l') => match self.targets(&ALL_SECTIONS) {
                Targets::Paths(paths) => self.dispatch(Request::Lock { paths }),
                Targets::Empty | Targets::Inapplicable => {
                    self.message = Some(("no file selected".into(), true));
                }
            },
            KeyCode::Char('L') => match self.targets(&ALL_SECTIONS) {
                Targets::Paths(paths) => self.dispatch(Request::Unlock { paths }),
                Targets::Empty | Targets::Inapplicable => {
                    self.message = Some(("no file selected".into(), true));
                }
            },

            KeyCode::Char('r') => {
                if self.view == View::Repos {
                    // A listing is only worth re-reading on request; the detail
                    // cache goes with it so stale entries do not survive.
                    self.repo_info.clear();
                    self.load_repos();
                } else if self.view == View::Branches {
                    self.dispatch(Request::Branches);
                } else {
                    self.dispatch(Request::Status { scan: true });
                    self.dispatch(Request::Locks);
                }
            }
            KeyCode::Char('R') => self.dispatch(Request::History),

            KeyCode::Char(' ') => self.toggle_mark(),

            KeyCode::Char('d') => match self.view {
                View::Status => match self.targets(&ALL_SECTIONS) {
                    Targets::Paths(paths) => self.dispatch(Request::Diff {
                        paths,
                        source: String::new(),
                        target: String::new(),
                        label: None,
                    }),
                    Targets::Empty | Targets::Inapplicable => {
                        self.message = Some(("no file selected".into(), true));
                    }
                },
                // A revision is shown against its first parent: what this
                // revision changed. A root revision has nothing to compare to.
                View::History => match self.history.get(self.history_selected) {
                    Some(entry) => match entry.parents.first() {
                        Some(parent) => self.dispatch(Request::Diff {
                            paths: Vec::new(),
                            source: parent.clone(),
                            target: entry.revision.clone(),
                            label: Some(format!("revision {}", entry.number)),
                        }),
                        None => {
                            self.message =
                                Some(("the first revision has no parent to diff".into(), true));
                        }
                    },
                    None => self.message = Some(("no revision selected".into(), true)),
                },
                _ => {}
            },

            KeyCode::Char('s') => match self.targets(&[Section::Unstaged, Section::Unmerged]) {
                Targets::Paths(paths) => self.dispatch(Request::Stage { paths, scan: false }),
                Targets::Empty => self.message = Some(("no file selected".into(), true)),
                Targets::Inapplicable => {
                    let text = if self.marked.is_empty() {
                        "already staged"
                    } else {
                        "no marked file to stage"
                    };
                    self.message = Some((text.into(), true));
                }
            },
            KeyCode::Char('S') => self.dispatch(Request::Stage {
                paths: vec![".".to_string()],
                scan: true,
            }),
            KeyCode::Char('u') => match self.targets(&[Section::Staged, Section::Unmerged]) {
                Targets::Paths(paths) => self.dispatch(Request::Unstage { paths }),
                Targets::Empty => self.message = Some(("no file selected".into(), true)),
                Targets::Inapplicable => {
                    let text = if self.marked.is_empty() {
                        "not staged"
                    } else {
                        "no marked file to unstage"
                    };
                    self.message = Some((text.into(), true));
                }
            },

            KeyCode::Char('c') => {
                self.modal = Modal::Input {
                    kind: InputKind::CommitMessage,
                    value: String::new(),
                }
            }
            KeyCode::Char('t') if self.view == View::History => {
                match self.history.get(self.history_selected) {
                    Some(entry) => {
                        self.tree_revision = entry.revision.clone();
                        self.tree_label = format!("revision {}", entry.number);
                        self.load_tree(String::new());
                    }
                    None => self.message = Some(("no revision selected".into(), true)),
                }
            }

            // One file's history, so this reads the selection rather than the
            // marks the file actions use.
            KeyCode::Char('h') => {
                if self.view == View::Status {
                    match self.selected_file() {
                        Some((_, path)) => {
                            let path = path.to_string();
                            self.dispatch(Request::FileHistory { path });
                        }
                        None => self.message = Some(("no file selected".into(), true)),
                    }
                }
            }

            KeyCode::Char('C') => {
                // Prefilled with the message being replaced: an amend is
                // usually a fix to it, not a rewrite from nothing.
                let current = self
                    .history
                    .first()
                    .map(|entry| entry.message.clone())
                    .unwrap_or_default();
                self.modal = Modal::Input {
                    kind: InputKind::AmendMessage,
                    value: current,
                };
            }
            KeyCode::Char('D') => {
                if self.view == View::Status {
                    match self.targets(&ALL_SECTIONS) {
                        Targets::Paths(paths) => {
                            let what = match paths.as_slice() {
                                [path] => path.clone(),
                                paths => format!("{} files", paths.len()),
                            };
                            self.modal = Modal::Confirm {
                                request: Some(Request::Reset { paths }),
                                prompt: format!("Throw away local changes to {what}?"),
                            };
                        }
                        Targets::Empty | Targets::Inapplicable => {
                            self.message = Some(("no file selected".into(), true));
                        }
                    }
                }
            }

            KeyCode::Char('m') => {
                if self.view == View::Branches {
                    match self.selected_branch() {
                        Some(branch) if branch.current => {
                            self.message = Some(("cannot merge a branch into itself".into(), true));
                        }
                        Some(branch) => {
                            let branch = branch.name.clone();
                            self.modal = Modal::Input {
                                value: format!("Merge branch {branch}"),
                                kind: InputKind::MergeMessage { branch },
                            };
                        }
                        None => self.message = Some(("no branch selected".into(), true)),
                    }
                }
            }

            // Resolutions apply to the unmerged section only; the other
            // sections have nothing to settle.
            KeyCode::Char('o') => self.resolve(Resolution::Mine, "kept"),
            KeyCode::Char('t') => self.resolve(Resolution::Theirs, "took"),
            KeyCode::Char('x') => self.resolve(Resolution::AsIs, "resolved"),
            KeyCode::Char('A') => {
                if self.status.as_ref().is_some_and(|status| status.merging) {
                    self.modal = Modal::Confirm {
                        request: Some(Request::MergeAbort),
                        prompt: "Abort the merge and drop what it staged?".into(),
                    };
                } else {
                    self.message = Some(("no merge in progress".into(), true));
                }
            }

            KeyCode::Char('n') => {
                if self.view == View::Branches {
                    self.modal = Modal::Input {
                        kind: InputKind::BranchName,
                        value: String::new(),
                    }
                }
            }
            KeyCode::Enter => {
                if self.view == View::Branches {
                    match self.selected_branch() {
                        Some(branch) if branch.current => {
                            self.message = Some(("already on this branch".into(), true));
                        }
                        Some(branch) => {
                            let name = branch.name.clone();
                            self.modal = Modal::Confirm {
                                request: Some(Request::SwitchBranch { name: name.clone() }),
                                prompt: format!("Switch the working copy to {name}?"),
                            };
                        }
                        None => self.message = Some(("no branch selected".into(), true)),
                    }
                }
            }
            KeyCode::Char('p') => {
                self.modal = Modal::Confirm {
                    request: Some(Request::Push),
                    prompt: "Push local revisions to the remote?".into(),
                }
            }
            KeyCode::Char('y') => {
                self.modal = Modal::Confirm {
                    request: Some(Request::Sync),
                    prompt: "Sync the working copy to the branch tip?".into(),
                }
            }
            _ => {}
        }
    }
}
