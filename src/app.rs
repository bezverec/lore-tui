// SPDX-License-Identifier: MIT
//! TUI state and the key handling that drives it.

use std::collections::HashMap;
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
use crate::ops::HistoryEntry;
use crate::ops::LogSink;
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
}

/// A rendered line in the status view. Headers are selectable targets for
/// nothing, so navigation steps over them.
#[derive(Clone, Copy)]
pub enum Row {
    Header(Section, usize),
    File(Section, usize),
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
    pub history_selected: usize,
    /// Server this working copy points at, and the one the browser lists.
    /// `None` until the config read lands; empty when there is no remote.
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
            history_selected: 0,
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
        app.dispatch(Request::RemoteUrl);
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
                        }
                        Err(error) => {
                            self.message = Some((format!("{name}: {error}"), true));
                            ops::log_push(&self.log, format!("< {name} failed"));
                        }
                    }

                    // Every action changes the working state, so the status
                    // view is refreshed either way — including on failure,
                    // where a partial effect is exactly what needs showing.
                    self.dispatch(Request::Status { scan: true });
                    if reload_history {
                        self.dispatch(Request::History);
                    }
                }
            }
        }

        self.pump();
        self.autoload_repo_info();
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
        // Moved files are shown as "from -> to"; Lore takes the current path.
        let path = files.get(index)?.path.as_str();
        let path = path.rsplit(" -> ").next().unwrap_or(path);
        Some((section, path))
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
        match key.code {
            KeyCode::Char('q') => self.should_quit = true,
            KeyCode::Char('?') => self.modal = Modal::Help,
            KeyCode::Esc => self.message = None,

            KeyCode::Tab => {
                let next = match self.view {
                    View::Status => View::History,
                    View::History => View::Repos,
                    View::Repos => View::Status,
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

            KeyCode::Char('r') => match self.view {
                // A listing is only worth re-reading on request; the detail
                // cache is cleared with it so stale entries do not survive.
                View::Repos => {
                    self.repo_info.clear();
                    self.load_repos();
                }
                _ => self.dispatch(Request::Status { scan: true }),
            },
            KeyCode::Char('R') => self.dispatch(Request::History),

            KeyCode::Char('s') => match self.selected_file() {
                Some((Section::Staged, _)) => {
                    self.message = Some(("already staged".into(), true));
                }
                Some((_, path)) => {
                    let paths = vec![path.to_string()];
                    self.dispatch(Request::Stage { paths, scan: false });
                }
                None => self.message = Some(("no file selected".into(), true)),
            },
            KeyCode::Char('S') => self.dispatch(Request::Stage {
                paths: vec![".".to_string()],
                scan: true,
            }),
            KeyCode::Char('u') => match self.selected_file() {
                Some((Section::Unstaged, _)) => {
                    self.message = Some(("not staged".into(), true));
                }
                Some((_, path)) => {
                    let paths = vec![path.to_string()];
                    self.dispatch(Request::Unstage { paths });
                }
                None => self.message = Some(("no file selected".into(), true)),
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
