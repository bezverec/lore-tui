// SPDX-License-Identifier: MIT
//! Rendering. Reads `App`, writes nothing back except list scroll state.

use ratatui::Frame;
use ratatui::layout::Alignment;
use ratatui::layout::Constraint;
use ratatui::layout::Layout;
use ratatui::layout::Rect;
use ratatui::style::Color;
use ratatui::style::Modifier;
use ratatui::style::Style;
use ratatui::text::Line;
use ratatui::text::Span;
use ratatui::widgets::Block;
use ratatui::widgets::Clear;
use ratatui::widgets::List;
use ratatui::widgets::ListItem;
use ratatui::widgets::ListState;
use ratatui::widgets::Paragraph;
use ratatui::widgets::Wrap;

use crate::app::App;
use crate::app::DiffKind;
use crate::app::Modal;
use crate::app::Row;
use crate::app::View;
use crate::ops::Section;

const SPINNER: [&str; 4] = ["|", "/", "-", "\\"];

fn section_label(section: Section) -> &'static str {
    match section {
        Section::Staged => "Staged",
        Section::Unmerged => "Unmerged",
        Section::Unstaged => "Unstaged",
    }
}

fn section_color(section: Section) -> Color {
    match section {
        Section::Staged => Color::Green,
        Section::Unmerged => Color::Red,
        Section::Unstaged => Color::Yellow,
    }
}

fn format_size(bytes: u64) -> String {
    if bytes >= 1024 * 1024 * 1024 {
        format!("{:.1} GiB", bytes as f64 / (1024.0 * 1024.0 * 1024.0))
    } else if bytes >= 1024 * 1024 {
        format!("{:.1} MiB", bytes as f64 / (1024.0 * 1024.0))
    } else if bytes >= 1024 {
        format!("{:.1} KiB", bytes as f64 / 1024.0)
    } else {
        format!("{bytes} B")
    }
}

fn format_time(millis: u64) -> String {
    match chrono::DateTime::from_timestamp_millis(millis as i64) {
        Some(time) => time
            .with_timezone(&chrono::Local)
            .format("%Y-%m-%d %H:%M")
            .to_string(),
        None => String::new(),
    }
}

pub fn draw(frame: &mut Frame<'_>, app: &mut App) {
    let [header, body, log, footer] = Layout::vertical([
        Constraint::Length(5),
        Constraint::Min(6),
        Constraint::Length(8),
        Constraint::Length(1),
    ])
    .areas(frame.area());

    draw_header(frame, app, header);

    let [left, right] =
        Layout::horizontal([Constraint::Percentage(55), Constraint::Percentage(45)]).areas(body);

    match app.view {
        View::Status => {
            draw_status_list(frame, app, left);
            draw_status_detail(frame, app, right);
        }
        View::History => {
            draw_history_list(frame, app, left);
            draw_history_detail(frame, app, right);
        }
        View::Branches => {
            draw_branches_list(frame, app, left);
            draw_branches_detail(frame, app, right);
        }
        View::Repos => {
            draw_repos_list(frame, app, left);
            draw_repos_detail(frame, app, right);
        }
        View::FileHistory => {
            draw_file_history_list(frame, app, left);
            draw_file_history_detail(frame, app, right);
        }
        View::Tree => draw_tree(frame, app, body),
        // A diff needs the width; it takes the whole body rather than a column.
        View::Diff => draw_diff(frame, app, body),
    }

    draw_log(frame, app, log);
    draw_footer(frame, app, footer);

    match &app.modal {
        Modal::None => {}
        Modal::Input { kind, value } => draw_input(frame, kind.title(), value),
        Modal::Confirm { prompt, .. } => draw_confirm(frame, prompt),
        Modal::Help => draw_help(frame),
    }
}

fn draw_header(frame: &mut Frame<'_>, app: &App, area: Rect) {
    let title = match (app.busy(), app.live) {
        (true, true) => format!(" lore ● {} ", SPINNER[app.spinner % SPINNER.len()]),
        (true, false) => format!(" lore {} ", SPINNER[app.spinner % SPINNER.len()]),
        (false, true) => " lore ● ".to_string(),
        (false, false) => " lore ".to_string(),
    };

    let mut lines = vec![Line::from(vec![
        Span::styled("path  ", Style::new().fg(Color::DarkGray)),
        Span::raw(app.repo_path.clone()),
    ])];

    match &app.status {
        Some(status) => {
            lines.push(Line::from(vec![
                Span::styled("branch ", Style::new().fg(Color::DarkGray)),
                Span::styled(
                    status.branch_name.clone(),
                    Style::new().fg(Color::Cyan).add_modifier(Modifier::BOLD),
                ),
                Span::raw("  "),
                Span::styled("revision ", Style::new().fg(Color::DarkGray)),
                Span::raw(format!("{}", status.revision_number)),
                Span::raw("  "),
                Span::styled(
                    status.remote_state().to_string(),
                    Style::new().fg(if status.remote_state() == "in sync with remote" {
                        Color::Green
                    } else {
                        Color::Yellow
                    }),
                ),
                Span::raw(if status.remote_branch_exist {
                    format!(" ({})", status.remote_revision_number)
                } else {
                    String::new()
                }),
            ]));
            let mut third = vec![
                Span::styled("signature ", Style::new().fg(Color::DarkGray)),
                Span::styled(status.revision.clone(), Style::new().fg(Color::DarkGray)),
            ];
            if status.merging {
                third.push(Span::raw("  "));
                third.push(Span::styled(
                    "merge in progress",
                    Style::new().fg(Color::Red).add_modifier(Modifier::BOLD),
                ));
            }
            lines.push(Line::from(third));
        }
        None => lines.push(Line::from(Span::styled(
            "loading…",
            Style::new().fg(Color::DarkGray),
        ))),
    }

    frame.render_widget(Paragraph::new(lines).block(Block::bordered().title(title)), area);
}

fn draw_status_list(frame: &mut Frame<'_>, app: &mut App, area: Rect) {
    let mut items: Vec<ListItem<'_>> = Vec::new();

    if let Some(status) = &app.status {
        for row in &app.rows {
            match *row {
                Row::Header(section, count) => items.push(ListItem::new(Line::from(Span::styled(
                    format!("{} ({count})", section_label(section)),
                    Style::new()
                        .fg(section_color(section))
                        .add_modifier(Modifier::BOLD),
                )))),
                Row::File(section, index) => {
                    let files = match section {
                        Section::Staged => &status.staged,
                        Section::Unmerged => &status.unmerged,
                        Section::Unstaged => &status.unstaged,
                    };
                    let Some(file) = files.get(index) else {
                        continue;
                    };
                    let marked = app.is_marked(file);
                    let locked = app.locks.contains_key(file.target());
                    items.push(ListItem::new(Line::from(vec![
                        Span::styled(
                            if marked { " ✓ " } else { "   " },
                            Style::new().fg(Color::Magenta),
                        ),
                        Span::styled(
                            file.action.to_string(),
                            Style::new().fg(section_color(section)),
                        ),
                        Span::styled(
                            if locked { " L " } else { "   " },
                            Style::new().fg(Color::Yellow).add_modifier(Modifier::BOLD),
                        ),
                        Span::styled(
                            file.path.clone(),
                            if marked {
                                Style::new().add_modifier(Modifier::BOLD)
                            } else {
                                Style::new()
                            },
                        ),
                    ])));
                }
            }
        }
    }

    if items.is_empty() {
        items.push(ListItem::new(Line::from(Span::styled(
            "  working copy clean",
            Style::new().fg(Color::DarkGray),
        ))));
    }

    let title = if app.marked.is_empty() {
        " Status ".to_string()
    } else {
        format!(" Status · {} marked ", app.marked.len())
    };

    let list = List::new(items)
        .block(Block::bordered().title(title))
        .highlight_style(Style::new().bg(Color::DarkGray).add_modifier(Modifier::BOLD));

    let mut state = ListState::default();
    if !app.rows.is_empty() {
        state.select(Some(app.status_selected));
    }
    frame.render_stateful_widget(list, area, &mut state);
}

fn draw_status_detail(frame: &mut Frame<'_>, app: &App, area: Rect) {
    let mut lines: Vec<Line<'_>> = Vec::new();

    if let Some(status) = &app.status {
        lines.push(Line::from(vec![
            Span::styled("repository ", Style::new().fg(Color::DarkGray)),
            Span::raw(status.repository.clone()),
        ]));
        lines.push(Line::from(format!(
            "{} staged · {} unmerged · {} unstaged",
            status.staged.len(),
            status.unmerged.len(),
            status.unstaged.len()
        )));
        lines.push(Line::raw(""));
    }

    match app.selected_file() {
        Some((section, path)) => {
            lines.push(Line::from(Span::styled(
                section_label(section),
                Style::new()
                    .fg(section_color(section))
                    .add_modifier(Modifier::BOLD),
            )));
            lines.push(Line::raw(path.to_string()));

            if let Some(status) = &app.status
                && let Some(Row::File(_, index)) = app.rows.get(app.status_selected).copied()
            {
                let files = match section {
                    Section::Staged => &status.staged,
                    Section::Unmerged => &status.unmerged,
                    Section::Unstaged => &status.unstaged,
                };
                if let Some(file) = files.get(index) {
                    lines.push(Line::raw(format!("size {}", format_size(file.size))));
                    if file.conflict {
                        lines.push(Line::from(Span::styled(
                            "unresolved conflict",
                            Style::new().fg(Color::Red),
                        )));
                    }
                }
            }

            if let Some(lock) = app.locks.get(path) {
                lines.push(Line::raw(""));
                lines.push(Line::from(Span::styled(
                    "locked",
                    Style::new().fg(Color::Yellow).add_modifier(Modifier::BOLD),
                )));
                if !lock.owner.is_empty() {
                    lines.push(Line::raw(format!("by {}", lock.owner)));
                }
                if lock.locked_at > 0 {
                    lines.push(Line::raw(format_time(lock.locked_at)));
                }
            }
        }
        None => lines.push(Line::from(Span::styled(
            "no file selected",
            Style::new().fg(Color::DarkGray),
        ))),
    }

    frame.render_widget(
        Paragraph::new(lines)
            .block(Block::bordered().title(" Detail "))
            .wrap(Wrap { trim: false }),
        area,
    );
}

fn draw_history_list(frame: &mut Frame<'_>, app: &mut App, area: Rect) {
    let items: Vec<ListItem<'_>> = app
        .history
        .iter()
        .map(|entry| {
            ListItem::new(Line::from(vec![
                Span::styled(
                    format!("{:>5} ", entry.number),
                    Style::new().fg(Color::Cyan),
                ),
                Span::raw(entry.subject().to_string()),
            ]))
        })
        .collect();

    let items = if items.is_empty() {
        vec![ListItem::new(Line::from(Span::styled(
            "  no revisions",
            Style::new().fg(Color::DarkGray),
        )))]
    } else {
        items
    };

    let list = List::new(items)
        .block(Block::bordered().title(" History "))
        .highlight_style(Style::new().bg(Color::DarkGray).add_modifier(Modifier::BOLD));

    let mut state = ListState::default();
    if !app.history.is_empty() {
        state.select(Some(app.history_selected));
    }
    frame.render_stateful_widget(list, area, &mut state);
}

fn draw_history_detail(frame: &mut Frame<'_>, app: &App, area: Rect) {
    let mut lines: Vec<Line<'_>> = Vec::new();

    match app.history.get(app.history_selected) {
        Some(entry) => {
            lines.push(Line::from(vec![
                Span::styled("revision  ", Style::new().fg(Color::DarkGray)),
                Span::styled(
                    entry.number.to_string(),
                    Style::new().add_modifier(Modifier::BOLD),
                ),
            ]));
            lines.push(Line::from(vec![
                Span::styled("signature ", Style::new().fg(Color::DarkGray)),
                Span::raw(entry.revision.clone()),
            ]));
            if !entry.author.is_empty() {
                lines.push(Line::from(vec![
                    Span::styled("author    ", Style::new().fg(Color::DarkGray)),
                    Span::raw(entry.author.clone()),
                ]));
            }
            if entry.timestamp > 0 {
                lines.push(Line::from(vec![
                    Span::styled("date      ", Style::new().fg(Color::DarkGray)),
                    Span::raw(format_time(entry.timestamp)),
                ]));
            }
            for parent in &entry.parents {
                lines.push(Line::from(vec![
                    Span::styled("parent    ", Style::new().fg(Color::DarkGray)),
                    Span::raw(parent.clone()),
                ]));
            }
            lines.push(Line::raw(""));
            for line in entry.message.lines() {
                lines.push(Line::raw(line.to_string()));
            }
        }
        None => lines.push(Line::from(Span::styled(
            "no revision selected",
            Style::new().fg(Color::DarkGray),
        ))),
    }

    frame.render_widget(
        Paragraph::new(lines)
            .block(Block::bordered().title(" Revision "))
            .wrap(Wrap { trim: false }),
        area,
    );
}

fn draw_branches_list(frame: &mut Frame<'_>, app: &mut App, area: Rect) {
    let items: Vec<ListItem<'_>> = app
        .branches
        .iter()
        .map(|branch| {
            ListItem::new(Line::from(vec![
                Span::styled(
                    if branch.current { " ● " } else { "   " },
                    Style::new().fg(Color::Green),
                ),
                Span::styled(
                    branch.name.clone(),
                    if branch.current {
                        Style::new().fg(Color::Cyan).add_modifier(Modifier::BOLD)
                    } else {
                        Style::new()
                    },
                ),
                Span::styled(
                    if branch.remote { "  remote" } else { "" },
                    Style::new().fg(Color::DarkGray),
                ),
            ]))
        })
        .collect();

    let items = if items.is_empty() {
        vec![ListItem::new(Line::from(Span::styled(
            "  no branches",
            Style::new().fg(Color::DarkGray),
        )))]
    } else {
        items
    };

    let list = List::new(items)
        .block(Block::bordered().title(" Branches "))
        .highlight_style(Style::new().bg(Color::DarkGray).add_modifier(Modifier::BOLD));

    let mut state = ListState::default();
    if !app.branches.is_empty() {
        state.select(Some(app.branches_selected));
    }
    frame.render_stateful_widget(list, area, &mut state);
}

fn draw_branches_detail(frame: &mut Frame<'_>, app: &App, area: Rect) {
    let mut lines: Vec<Line<'_>> = Vec::new();

    match app.selected_branch() {
        Some(branch) => {
            lines.push(Line::from(vec![
                Span::styled("name    ", Style::new().fg(Color::DarkGray)),
                Span::styled(
                    branch.name.clone(),
                    Style::new().add_modifier(Modifier::BOLD),
                ),
            ]));
            lines.push(Line::from(vec![
                Span::styled("where   ", Style::new().fg(Color::DarkGray)),
                Span::raw(if branch.remote { "remote" } else { "local" }),
            ]));
            if !branch.category.is_empty() {
                lines.push(Line::from(vec![
                    Span::styled("category", Style::new().fg(Color::DarkGray)),
                    Span::raw(format!(" {}", branch.category)),
                ]));
            }
            if !branch.creator.is_empty() {
                lines.push(Line::from(vec![
                    Span::styled("creator ", Style::new().fg(Color::DarkGray)),
                    Span::raw(branch.creator.clone()),
                ]));
            }
            if branch.created > 0 {
                lines.push(Line::from(vec![
                    Span::styled("created ", Style::new().fg(Color::DarkGray)),
                    Span::raw(format_time(branch.created)),
                ]));
            }
            lines.push(Line::from(vec![
                Span::styled("latest  ", Style::new().fg(Color::DarkGray)),
                Span::raw(branch.latest.clone()),
            ]));
            if branch.archived {
                lines.push(Line::from(Span::styled(
                    "archived",
                    Style::new().fg(Color::Yellow),
                )));
            }

            lines.push(Line::raw(""));
            lines.push(Line::from(Span::styled(
                if branch.current {
                    "on this branch"
                } else {
                    "enter to switch · n for a new branch"
                },
                Style::new().fg(Color::DarkGray),
            )));
        }
        None => lines.push(Line::from(Span::styled(
            "no branch selected",
            Style::new().fg(Color::DarkGray),
        ))),
    }

    frame.render_widget(
        Paragraph::new(lines)
            .block(Block::bordered().title(" Branch "))
            .wrap(Wrap { trim: false }),
        area,
    );
}

fn draw_repos_list(frame: &mut Frame<'_>, app: &mut App, area: Rect) {
    // The repository this working copy belongs to gets a marker, so the
    // browser doubles as "where am I on this server".
    let current = app
        .status
        .as_ref()
        .map(|status| status.repository.clone())
        .unwrap_or_default();

    let items: Vec<ListItem<'_>> = app
        .repos
        .iter()
        .map(|entry| {
            let here = !current.is_empty() && entry.id == current;
            ListItem::new(Line::from(vec![
                Span::styled(
                    if here { " ● " } else { "   " },
                    Style::new().fg(Color::Green),
                ),
                Span::styled(
                    entry.name.clone(),
                    if here {
                        Style::new().add_modifier(Modifier::BOLD)
                    } else {
                        Style::new()
                    },
                ),
            ]))
        })
        .collect();

    let items = if items.is_empty() {
        let text = match app.remote.as_deref() {
            Some("") => "  no remote configured",
            None => "  reading the remote…",
            Some(_) => "  no repositories on this server",
        };
        vec![ListItem::new(Line::from(Span::styled(
            text,
            Style::new().fg(Color::DarkGray),
        )))]
    } else {
        items
    };

    let title = match app.remote.as_deref() {
        Some(url) if !url.is_empty() => format!(" Repositories · {url} "),
        _ => " Repositories ".to_string(),
    };

    let list = List::new(items)
        .block(Block::bordered().title(title))
        .highlight_style(Style::new().bg(Color::DarkGray).add_modifier(Modifier::BOLD));

    let mut state = ListState::default();
    if !app.repos.is_empty() {
        state.select(Some(app.repos_selected));
    }
    frame.render_stateful_widget(list, area, &mut state);
}

fn draw_repos_detail(frame: &mut Frame<'_>, app: &App, area: Rect) {
    let mut lines: Vec<Line<'_>> = Vec::new();

    match app.selected_repo() {
        Some(entry) => {
            lines.push(Line::from(vec![
                Span::styled("name       ", Style::new().fg(Color::DarkGray)),
                Span::styled(
                    entry.name.clone(),
                    Style::new().add_modifier(Modifier::BOLD),
                ),
            ]));
            lines.push(Line::from(vec![
                Span::styled("id         ", Style::new().fg(Color::DarkGray)),
                Span::raw(entry.id.clone()),
            ]));

            match app.repo_info.get(&entry.name) {
                Some(Ok(info)) => {
                    if !info.description.is_empty() {
                        lines.push(Line::from(vec![
                            Span::styled("description", Style::new().fg(Color::DarkGray)),
                            Span::raw(format!(" {}", info.description)),
                        ]));
                    }
                    if !info.default_branch.is_empty() {
                        lines.push(Line::from(vec![
                            Span::styled("branch     ", Style::new().fg(Color::DarkGray)),
                            Span::styled(info.default_branch.clone(), Style::new().fg(Color::Cyan)),
                        ]));
                    }
                    if !info.creator.is_empty() {
                        lines.push(Line::from(vec![
                            Span::styled("creator    ", Style::new().fg(Color::DarkGray)),
                            Span::raw(info.creator.clone()),
                        ]));
                    }
                    if info.created > 0 {
                        lines.push(Line::from(vec![
                            Span::styled("created    ", Style::new().fg(Color::DarkGray)),
                            Span::raw(format_time(info.created)),
                        ]));
                    }
                    if !info.remote_url.is_empty() {
                        lines.push(Line::from(vec![
                            Span::styled("url        ", Style::new().fg(Color::DarkGray)),
                            Span::raw(info.remote_url.clone()),
                        ]));
                    }
                }
                Some(Err(error)) => lines.push(Line::from(Span::styled(
                    error.clone(),
                    Style::new().fg(Color::Red),
                ))),
                None => lines.push(Line::from(Span::styled(
                    "loading…",
                    Style::new().fg(Color::DarkGray),
                ))),
            }
        }
        None => lines.push(Line::from(Span::styled(
            "no repository selected",
            Style::new().fg(Color::DarkGray),
        ))),
    }

    frame.render_widget(
        Paragraph::new(lines)
            .block(Block::bordered().title(" Repository "))
            .wrap(Wrap { trim: false }),
        area,
    );
}

fn draw_tree(frame: &mut Frame<'_>, app: &mut App, area: Rect) {
    let items: Vec<ListItem<'_>> = app
        .tree
        .iter()
        .map(|entry| {
            ListItem::new(Line::from(vec![
                Span::styled(
                    if entry.directory { "  d " } else { "  · " },
                    Style::new().fg(Color::DarkGray),
                ),
                Span::styled(
                    entry.name.clone(),
                    if entry.directory {
                        Style::new().fg(Color::Cyan).add_modifier(Modifier::BOLD)
                    } else {
                        Style::new()
                    },
                ),
                Span::styled(
                    if entry.directory {
                        String::new()
                    } else {
                        format!("   {}", format_size(entry.size))
                    },
                    Style::new().fg(Color::DarkGray),
                ),
            ]))
        })
        .collect();

    let items = if items.is_empty() {
        vec![ListItem::new(Line::from(Span::styled(
            "  empty",
            Style::new().fg(Color::DarkGray),
        )))]
    } else {
        items
    };

    let where_ = if app.tree_path.is_empty() {
        "/".to_string()
    } else {
        format!("/{}", app.tree_path)
    };

    let list = List::new(items)
        .block(
            Block::bordered().title(format!(" Tree · {} · {where_} ", app.tree_label)),
        )
        .highlight_style(Style::new().bg(Color::DarkGray).add_modifier(Modifier::BOLD));

    let mut state = ListState::default();
    if !app.tree.is_empty() {
        state.select(Some(app.tree_selected));
    }
    frame.render_stateful_widget(list, area, &mut state);
}

fn draw_file_history_list(frame: &mut Frame<'_>, app: &mut App, area: Rect) {
    let items: Vec<ListItem<'_>> = app
        .file_history
        .iter()
        .map(|entry| {
            ListItem::new(Line::from(vec![
                Span::styled(
                    format!("{:>5} ", entry.number),
                    Style::new().fg(Color::Cyan),
                ),
                Span::styled(entry.action.to_string(), Style::new().fg(Color::Yellow)),
                Span::raw("  "),
                Span::raw(entry.subject().to_string()),
            ]))
        })
        .collect();

    let list = List::new(items)
        .block(
            Block::bordered().title(format!(" History · {} ", app.file_history_path)),
        )
        .highlight_style(Style::new().bg(Color::DarkGray).add_modifier(Modifier::BOLD));

    let mut state = ListState::default();
    if !app.file_history.is_empty() {
        state.select(Some(app.file_history_selected));
    }
    frame.render_stateful_widget(list, area, &mut state);
}

fn draw_file_history_detail(frame: &mut Frame<'_>, app: &App, area: Rect) {
    let mut lines: Vec<Line<'_>> = Vec::new();

    match app.file_history.get(app.file_history_selected) {
        Some(entry) => {
            lines.push(Line::from(vec![
                Span::styled("revision  ", Style::new().fg(Color::DarkGray)),
                Span::styled(
                    entry.number.to_string(),
                    Style::new().add_modifier(Modifier::BOLD),
                ),
                Span::styled(
                    if entry.merge { "  merge" } else { "" },
                    Style::new().fg(Color::Magenta),
                ),
            ]));
            lines.push(Line::from(vec![
                Span::styled("signature ", Style::new().fg(Color::DarkGray)),
                Span::raw(entry.revision.clone()),
            ]));
            lines.push(Line::from(vec![
                Span::styled("size      ", Style::new().fg(Color::DarkGray)),
                Span::raw(format_size(entry.size)),
            ]));
            if !entry.author.is_empty() {
                lines.push(Line::from(vec![
                    Span::styled("author    ", Style::new().fg(Color::DarkGray)),
                    Span::raw(entry.author.clone()),
                ]));
            }
            if entry.timestamp > 0 {
                lines.push(Line::from(vec![
                    Span::styled("date      ", Style::new().fg(Color::DarkGray)),
                    Span::raw(format_time(entry.timestamp)),
                ]));
            }
            lines.push(Line::raw(""));
            for line in entry.message.lines() {
                lines.push(Line::raw(line.to_string()));
            }
        }
        None => lines.push(Line::from(Span::styled(
            "no revision selected",
            Style::new().fg(Color::DarkGray),
        ))),
    }

    frame.render_widget(
        Paragraph::new(lines)
            .block(Block::bordered().title(" Revision "))
            .wrap(Wrap { trim: false }),
        area,
    );
}

fn draw_diff(frame: &mut Frame<'_>, app: &mut App, area: Rect) {
    let viewport = area.height.saturating_sub(2) as usize;
    // Recorded for the scroll clamp, which cannot know the pane height.
    app.diff_viewport = viewport;

    let start = app.diff_scroll.min(app.diff.len());
    let end = (start + viewport).min(app.diff.len());

    let lines: Vec<Line<'_>> = app.diff[start..end]
        .iter()
        .map(|line| {
            let style = match line.kind {
                DiffKind::File => Style::new().fg(Color::Cyan).add_modifier(Modifier::BOLD),
                DiffKind::Hunk => Style::new().fg(Color::Blue),
                DiffKind::Add => Style::new().fg(Color::Green),
                DiffKind::Delete => Style::new().fg(Color::Red),
                DiffKind::Conflict => Style::new().fg(Color::Magenta).add_modifier(Modifier::BOLD),
                DiffKind::Context => Style::new().fg(Color::Gray),
            };
            Line::from(Span::styled(line.text.clone(), style))
        })
        .collect();

    let title = if app.diff.len() > viewport {
        format!(
            "{}· {}-{} of {} ",
            app.diff_title,
            start + 1,
            end,
            app.diff.len()
        )
    } else {
        app.diff_title.clone()
    };

    // Deliberately unwrapped: a wrapped diff loses the column alignment that
    // makes a patch readable. Long lines are cut at the pane edge.
    frame.render_widget(
        Paragraph::new(lines).block(Block::bordered().title(title)),
        area,
    );
}

fn draw_log(frame: &mut Frame<'_>, app: &App, area: Rect) {
    let visible = area.height.saturating_sub(2) as usize;
    let log = app.log.lock();
    let start = log.len().saturating_sub(visible);

    let lines: Vec<Line<'_>> = log[start..]
        .iter()
        .map(|line| {
            let style = if line.starts_with("[error]") {
                Style::new().fg(Color::Red)
            } else if line.starts_with("[warn]") {
                Style::new().fg(Color::Yellow)
            } else if line.starts_with("[live]") {
                Style::new().fg(Color::Magenta)
            } else if line.starts_with('>') || line.starts_with('<') {
                Style::new().fg(Color::Cyan)
            } else {
                Style::new().fg(Color::Gray)
            };
            Line::from(Span::styled(line.clone(), style))
        })
        .collect();

    frame.render_widget(
        Paragraph::new(lines).block(Block::bordered().title(" Log ")),
        area,
    );
}

fn draw_footer(frame: &mut Frame<'_>, app: &App, area: Rect) {
    let line = match &app.message {
        Some((text, true)) => Line::from(Span::styled(
            format!(" {text} "),
            Style::new().fg(Color::White).bg(Color::Red),
        )),
        Some((text, false)) => Line::from(Span::styled(
            format!(" {text} "),
            Style::new().fg(Color::Black).bg(Color::Green),
        )),
        None => Line::from(Span::styled(
            " tab view · j/k move · space mark · d diff · s stage · u unstage · l/L lock · c commit · p push · y sync · r refresh · ? keys · q quit",
            Style::new().fg(Color::DarkGray),
        )),
    };

    frame.render_widget(Paragraph::new(line), area);
}

/// Centered rect `width` x `height`, clamped to the frame.
fn centered(area: Rect, width: u16, height: u16) -> Rect {
    let width = width.min(area.width);
    let height = height.min(area.height);
    Rect {
        x: area.x + (area.width - width) / 2,
        y: area.y + (area.height - height) / 2,
        width,
        height,
    }
}

fn draw_input(frame: &mut Frame<'_>, title: &str, value: &str) {
    let area = centered(frame.area(), 70, 5);
    frame.render_widget(Clear, area);
    frame.render_widget(
        Paragraph::new(vec![
            Line::from(format!("{value}_")),
            Line::raw(""),
            Line::from(Span::styled(
                "enter confirm · esc cancel",
                Style::new().fg(Color::DarkGray),
            )),
        ])
        .block(Block::bordered().title(title.to_string()))
        .wrap(Wrap { trim: false }),
        area,
    );
}

fn draw_confirm(frame: &mut Frame<'_>, prompt: &str) {
    let area = centered(frame.area(), 60, 5);
    frame.render_widget(Clear, area);
    frame.render_widget(
        Paragraph::new(vec![
            Line::raw(prompt.to_string()),
            Line::raw(""),
            Line::from(Span::styled(
                "y confirm · any other key cancel",
                Style::new().fg(Color::DarkGray),
            )),
        ])
        .block(Block::bordered().title(" Confirm "))
        .wrap(Wrap { trim: false }),
        area,
    );
}

fn draw_help(frame: &mut Frame<'_>) {
    let keys = [
        ("tab / 1..4", "status, history, branches, repositories"),
        ("j k ↑ ↓", "move selection"),
        ("g G", "jump to first / last"),
        ("space", "mark the file for the next stage or unstage"),
        ("d", "diff: the marked files, or the selected revision"),
        ("h", "revisions that touched the selected file"),
        ("t", "browse the files of the selected revision"),
        ("l / L", "lock / unlock the marked files, or the selected one"),
        ("s", "stage the marked files, or the selected one"),
        ("S", "scan the working copy and stage everything"),
        ("u", "unstage the marked files, or the selected one"),
        ("c / C", "commit the staged revision / amend the last message"),
        ("D", "throw away local changes to the marked or selected files"),
        ("p", "push to the remote"),
        ("y", "sync to the branch tip"),
        ("enter / n / m", "branches: switch to / create / merge a branch"),
        ("o / t / x", "conflict: keep mine / take theirs / accept as is"),
        ("A", "abort the merge in progress"),
        ("r / R", "refresh the current list / the history"),
        ("esc", "clear the marks, or dismiss the status message"),
        ("q / ctrl-c", "quit"),
    ];

    let mut lines = vec![Line::raw("")];
    for (key, description) in keys {
        lines.push(Line::from(vec![
            Span::styled(format!("  {key:<12}"), Style::new().fg(Color::Cyan)),
            Span::raw(description),
        ]));
    }

    let area = centered(frame.area(), 64, lines.len() as u16 + 2);
    frame.render_widget(Clear, area);
    frame.render_widget(
        Paragraph::new(lines)
            .block(Block::bordered().title(" Keys ").title_alignment(Alignment::Center)),
        area,
    );
}
