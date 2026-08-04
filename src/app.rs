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
use crate::ops::DiffEntry;
use crate::ops::FileEntry;
use crate::ops::HistoryEntry;
use crate::ops::LockEntry;
use crate::ops::LogSink;
use crate::ops::Notice;
use crate::ops::NoticeSink;
use crate::ops::OpResult;
use crate::ops::RepoEntry;
use crate::ops::RepoInfo;
use crate::ops::Request;
use crate::ops::Section;
use crate::ops::StatusSnapshot;

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum View {
    Status,
    History,
    Repos,
    /// Reached with `d` from the status view rather than the view cycle, since
    /// it shows one specific diff rather than a standing list.
    Diff,
}

/// How a diff line should read. Classified once when the diff lands, so
/// scrolling does not re-parse the patch on every frame.
#[derive(Clone, Copy, PartialEq, Eq)]
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

pub enum Modal {
    None,
    Commit { message: String },
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
    /// Push notifications waiting to be folded into the state.
    notices: NoticeSink,
    /// Whether the server is pushing changes to us.
    pub live: bool,
    pub diff: Vec<DiffLine>,
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
        let (tx, rx) = channel();

        let mut app = Self {
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
            notices: Arc::new(Mutex::new(Vec::new())),
            live: false,
            diff: Vec::new(),
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
        };

        // The first load scans: the dirty flags on disk may predate this
        // session, and an empty file list that only means "nothing was
        // flagged" would read as "nothing changed".
        app.dispatch(Request::Status { scan: true });
        app.dispatch(Request::History);
        app.dispatch(Request::Locks);
        app.dispatch(Request::RemoteUrl);
        app.dispatch(Request::Subscribe {
            notices: app.notices.clone(),
        });
        app
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
                OpResult::Diff(Ok(entries)) => {
                    if entries.iter().all(|entry| entry.patch.trim().is_empty()) {
                        // Lore answers with an empty patch for a file whose
                        // content did not change textually — a binary asset, or
                        // a pure rename.
                        self.message = Some(("nothing to show in a diff".into(), true));
                    } else {
                        self.build_diff(&entries);
                        self.view = View::Diff;
                    }
                }
                OpResult::Diff(Err(error)) => {
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
                Notice::BranchCreated => ops::log_push(&self.log, "[live] branch created"),
                Notice::BranchDeleted => ops::log_push(&self.log, "[live] branch deleted"),
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
    fn build_diff(&mut self, entries: &[DiffEntry]) {
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
                let kind = if line.starts_with("<<<<<<<")
                    || line.starts_with("|||||||")
                    || line.starts_with("=======")
                    || line.starts_with(">>>>>>>")
                {
                    DiffKind::Conflict
                } else if line.starts_with("@@") {
                    DiffKind::Hunk
                } else if line.starts_with("+++") || line.starts_with("---") {
                    // File headers of the patch itself; the path is already in
                    // the heading above, so they read as context.
                    DiffKind::Context
                } else if line.starts_with('+') {
                    DiffKind::Add
                } else if line.starts_with('-') {
                    DiffKind::Delete
                } else {
                    DiffKind::Context
                };

                lines.push(DiffLine {
                    kind,
                    text: line.to_string(),
                });
            }
        }

        self.diff_title = match entries {
            [entry] => format!(" Diff · {} ", entry.path),
            entries => format!(" Diff · {} files ", entries.len()),
        };
        self.diff = lines;
        self.diff_scroll = 0;
    }

    /// Largest scroll offset that still fills the pane.
    fn diff_max_scroll(&self) -> usize {
        self.diff.len().saturating_sub(self.diff_viewport.max(1))
    }

    pub fn selected_repo(&self) -> Option<&RepoEntry> {
        self.repos.get(self.repos_selected)
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
        // A header may have landed under the cursor after the rebuild.
        if matches!(self.rows.get(self.status_selected), Some(Row::Header(..))) {
            self.move_selection(1);
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
                if matches!(self.rows.get(self.status_selected), Some(Row::Header(..))) {
                    self.move_selection(if first { 1 } else { -1 });
                }
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
            Modal::Commit { message } => match key.code {
                KeyCode::Esc => self.modal = Modal::None,
                KeyCode::Enter => {
                    let message = message.trim().to_string();
                    self.modal = Modal::None;
                    if message.is_empty() {
                        self.message = Some(("commit needs a message".into(), true));
                    } else {
                        self.dispatch(Request::Commit { message });
                    }
                }
                KeyCode::Backspace => {
                    message.pop();
                }
                KeyCode::Char(c) => message.push(c),
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
        // The diff is a detour rather than a place to live, so everything that
        // means "I am done here" takes you back to the status list.
        if self.view == View::Diff
            && matches!(
                key.code,
                KeyCode::Esc | KeyCode::Char('d' | 'q')
            )
        {
            self.view = View::Status;
            return;
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
                    View::History => View::Repos,
                    // The diff is outside the cycle; tab leaves it.
                    View::Repos | View::Diff => View::Status,
                };
                self.show(next);
            }
            KeyCode::Char('1') => self.show(View::Status),
            KeyCode::Char('2') => self.show(View::History),
            KeyCode::Char('3') => self.show(View::Repos),

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
                } else {
                    self.dispatch(Request::Status { scan: true });
                    self.dispatch(Request::Locks);
                }
            }
            KeyCode::Char('R') => self.dispatch(Request::History),

            KeyCode::Char(' ') => self.toggle_mark(),

            KeyCode::Char('d') => {
                if self.view == View::Status {
                    match self.targets(&ALL_SECTIONS) {
                        Targets::Paths(paths) => self.dispatch(Request::Diff { paths }),
                        Targets::Empty | Targets::Inapplicable => {
                            self.message = Some(("no file selected".into(), true));
                        }
                    }
                }
            }

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
                self.modal = Modal::Commit {
                    message: String::new(),
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
