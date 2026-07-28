use crate::graph::{DependencyGraph, RowKind};
use anyhow::Result;
use crossterm::{
    event::{self, Event, KeyCode},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::{
    backend::CrosstermBackend,
    layout::{Constraint, Direction, Layout},
    style::{Color, Modifier, Style},
    widgets::{Block, Borders, List, ListItem, ListState, Paragraph},
    Terminal,
};
use std::io::stdout;

pub fn run_tui(graph: &mut DependencyGraph) -> Result<()> {
    enable_raw_mode()?;
    let mut stdout = stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let mut list_state = ListState::default();
    list_state.select(Some(0));

    let mut status_msg = String::from("Ready");

    loop {
        terminal.draw(|f| {
            let chunks = Layout::default()
                .direction(Direction::Vertical)
                .constraints([
                    Constraint::Length(3),
                    Constraint::Min(5),
                    Constraint::Length(3),
                ])
                .split(f.size());

            let header = Paragraph::new(format!(" Target: {}", graph.root_origin))
                .style(Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD))
                .block(
                    Block::default()
                        .borders(Borders::ALL)
                        .title(" bgone - FreeBSD Ports Configurator "),
                );
            f.render_widget(header, chunks[0]);

            let items: Vec<ListItem> = graph
                .visible_rows
                .iter()
                .map(|row| {
                    let indent = "  ".repeat(row.depth);
                    let line = match &row.kind {
                        RowKind::Port { origin } => {
                            let prefix = if row.has_children {
                                if row.is_expanded { "[-] " } else { "[+] " }
                            } else {
                                "    "
                            };
                            format!("{}{}{}", indent, prefix, origin)
                        }
                        RowKind::Option {
                            name,
                            description,
                            enabled,
                            group_type,
                            group_name,
                        } => {
                            let prefix = if row.has_children {
                                if row.is_expanded { "[-] " } else { "[+] " }
                            } else {
                                "    "
                            };

                            let is_radio = group_type == "SINGLE" || group_type == "RADIO";
                            let control = if is_radio {
                                if *enabled { "(*)" } else { "( )" }
                            } else {
                                if *enabled { "[X]" } else { "[ ]" }
                            };

                            let category_badge = if !group_name.is_empty() {
                                format!("<{}> ", group_name)
                            } else {
                                String::new()
                            };

                            if description.is_empty() {
                                format!("{}{}{} {}{}", indent, prefix, control, category_badge, name)
                            } else {
                                format!(
                                    "{}{}{} {}{} - {}",
                                    indent, prefix, control, category_badge, name, description
                                )
                            }
                        }
                        RowKind::Info { message } => {
                            format!("{}* {}", indent, message)
                        }
                    };
                    ListItem::new(line)
                })
                .collect();

            let tree_list = List::new(items)
                .block(
                    Block::default()
                        .borders(Borders::ALL)
                        .title(" Options & Dependencies "),
                )
                .highlight_style(
                    Style::default()
                        .bg(Color::Blue)
                        .fg(Color::White)
                        .add_modifier(Modifier::BOLD),
                )
                .highlight_symbol("> ");

            f.render_stateful_widget(tree_list, chunks[1], &mut list_state);

            let footer = Paragraph::new(format!(
                " [e/c] Subtree Expand/Collapse | [Shift+E/C] Global | [Enter] Single Toggle | [Space] Select | {}",
                status_msg
            ))
            .style(Style::default().fg(Color::DarkGray))
            .block(Block::default().borders(Borders::ALL));

            f.render_widget(footer, chunks[2]);
        })?;

        if event::poll(std::time::Duration::from_millis(50))? {
            if let Event::Key(key) = event::read()? {
                let total_rows = graph.visible_rows.len();

                match key.code {
                    KeyCode::Char('q') | KeyCode::Esc => break,

                    KeyCode::Up => {
                        let i = match list_state.selected() {
                            Some(i) => {
                                if i == 0 {
                                    0
                                } else {
                                    i - 1
                                }
                            }
                            None => 0,
                        };
                        list_state.select(Some(i));
                    }

                    KeyCode::Down => {
                        let i = match list_state.selected() {
                            Some(i) => {
                                if i >= total_rows.saturating_sub(1) {
                                    total_rows.saturating_sub(1)
                                } else {
                                    i + 1
                                }
                            }
                            None => 0,
                        };
                        list_state.select(Some(i));
                    }

                    KeyCode::Char('e') => {
                        if let Some(selected) = list_state.selected() {
                            graph.expand_subtree(selected);
                            status_msg = format!("Action: Expand Subtree at row {}", selected);
                        }
                    }

                    KeyCode::Char('c') => {
                        if let Some(selected) = list_state.selected() {
                            graph.collapse_subtree(selected);
                            status_msg = format!("Action: Collapse Subtree at row {}", selected);
                        }
                    }

                    KeyCode::Char('E') => {
                        graph.expand_all();
                        status_msg = String::from("Action: Expand All (Global)");
                    }

                    KeyCode::Char('C') => {
                        graph.collapse_all();
                        status_msg = String::from("Action: Collapse All (Global)");
                    }

                    KeyCode::Enter | KeyCode::Char('\n') | KeyCode::Char('\r') => {
                        if let Some(selected) = list_state.selected() {
                            graph.toggle_expand(selected);
                            status_msg = format!("Action: Toggled single node at row {}", selected);
                        }
                    }

                    KeyCode::Char(' ') => {
                        if let Some(selected) = list_state.selected() {
                            if let Some(row) = graph.visible_rows.get(selected) {
                                match &row.kind {
                                    RowKind::Option { name, .. } => {
                                        let opt_name = name.clone();
                                        graph.toggle_option(selected);
                                        status_msg =
                                            format!("Action: Selected option '{}'", opt_name);
                                    }
                                    _ => {
                                        status_msg =
                                            String::from("Status: Cannot toggle non-option row");
                                    }
                                }
                            }
                        }
                    }

                    _ => {}
                }
            }
        }
    }

    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
    terminal.show_cursor()?;

    Ok(())
}
