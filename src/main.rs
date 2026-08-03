// SPDX-License-Identifier: MIT
//! A terminal UI for Lore, built directly on the `lore` client library.

mod app;
mod ops;
mod ui;

use std::path::Path;
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Duration;

use lore::interface::LoreGlobalArgs;
use lore::interface::LoreString;
use ratatui::DefaultTerminal;
use ratatui::crossterm::event;
use ratatui::crossterm::event::Event;
use ratatui::crossterm::event::KeyEventKind;

use crate::app::App;

/// Walk up from `start` looking for a repository marker, mirroring how the CLI
/// resolves "the repository I am standing in". `.urc` is the pre-rename marker
/// and still exists in older working copies.
fn find_repository(start: &Path) -> Option<PathBuf> {
    let mut current = start;
    loop {
        if current.join(".lore").is_dir() || current.join(".urc").is_dir() {
            return Some(current.to_path_buf());
        }
        current = current.parent()?;
    }
}

fn run(terminal: &mut DefaultTerminal, app: &mut App) -> std::io::Result<()> {
    while !app.should_quit {
        app.poll();
        terminal.draw(|frame| ui::draw(frame, app))?;

        // Bounded wait so the spinner keeps turning and results that arrive
        // while the user is idle still get picked up.
        if event::poll(Duration::from_millis(120))?
            && let Event::Key(key) = event::read()?
            && key.kind == KeyEventKind::Press
        {
            app.handle_key(key);
        }

        app.spinner = app.spinner.wrapping_add(1);
    }

    Ok(())
}

/// Reading the process working directory is disallowed inside Lore, where a
/// call may be served by a background service process whose directory is not
/// the user's. This runs in the terminal process before any Lore call, where it
/// is the only sensible meaning of "the repository I am standing in" — the same
/// exception the CLI makes when it builds its global arguments.
#[allow(clippy::disallowed_methods)]
fn main() -> ExitCode {
    let requested = std::env::args().nth(1);
    if matches!(requested.as_deref(), Some("-h" | "--help")) {
        println!("usage: lore-tui [repository-path]");
        return ExitCode::SUCCESS;
    }

    let start = match requested {
        Some(path) => PathBuf::from(path),
        None => match std::env::current_dir() {
            Ok(path) => path,
            Err(error) => {
                eprintln!("lore-tui: cannot read the working directory: {error}");
                return ExitCode::FAILURE;
            }
        },
    };

    let Some(repository) = find_repository(&start) else {
        eprintln!(
            "lore-tui: no Lore repository at or above {}",
            start.display()
        );
        return ExitCode::FAILURE;
    };

    // Stage and unstage take paths as the user's shell would give them, so the
    // process has to stand in the repository root for the repo-relative paths
    // coming out of `status` to resolve.
    if let Err(error) = std::env::set_current_dir(&repository) {
        eprintln!("lore-tui: cannot enter {}: {error}", repository.display());
        return ExitCode::FAILURE;
    }

    lore::log::initialize();

    let globals = LoreGlobalArgs {
        repository_path: LoreString::from(&repository),
        ..Default::default()
    };

    let mut app = App::new(globals, repository.display().to_string());

    let mut terminal = ratatui::init();
    let result = run(&mut terminal, &mut app);
    ratatui::restore();

    lore::shutdown();

    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("lore-tui: {error}");
            ExitCode::FAILURE
        }
    }
}
