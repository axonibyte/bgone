use crate::graph::{DependencyGraph, RowKind};
use anyhow::Result;
use crossterm::{
    event::{self, Event, KeyCode, KeyModifiers},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::{
    backend::CrosstermBackend,
    layout::{Constraint, Direction, Layout},
    style::{Color, Modifier, Style},
    widgets::{
        Block, Borders, List, ListItem, ListState, Paragraph, Scrollbar, ScrollbarOrientation,
        ScrollbarState,
    },
    Terminal,
};
use std::io::stdout;

pub enum TuiAction {
    SaveAndQuit,
    QuitWithoutSaving,
}

#[derive(PartialEq)]
enum InputMode {
    Normal,
    Search,
}

pub fn run_tui(graph: &mut DependencyGraph) -> Result<TuiAction> {
    enable_raw_mode()?;
    let mut stdout = stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let mut list_state = ListState::default();
    list_state.select(Some(0));

    let mut status_msg = String::from("Ready");
    let action;
    let mut input_mode = InputMode::Normal;

    loop {
        let total_rows = graph.visible_rows.len();
        let selected_index = list_state.selected().unwrap_or(0);

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
                .style(
                    Style::default()
                        .fg(Color::Cyan)
                        .add_modifier(Modifier::BOLD),
                )
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
                                if row.is_expanded {
                                    "[-] "
                                } else {
                                    "[+] "
                                }
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
                                if row.is_expanded {
                                    "[-] "
                                } else {
                                    "[+] "
                                }
                            } else {
                                "    "
                            };

                            let is_radio = group_type == "SINGLE" || group_type == "RADIO";
                            let control = if is_radio {
                                if *enabled {
                                    "(*)"
                                } else {
                                    "( )"
                                }
                            } else {
                                if *enabled {
                                    "[X]"
                                } else {
                                    "[ ]"
                                }
                            };

                            let category_badge = if !group_name.is_empty() {
                                format!("<{}> ", group_name)
                            } else {
                                String::new()
                            };

                            if description.is_empty() {
                                format!(
                                    "{}{}{} {}{}",
                                    indent, prefix, control, category_badge, name
                                )
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

            // Render Right-Hand Scrollbar
            let mut scrollbar_state = ScrollbarState::new(total_rows.saturating_sub(1))
                .position(selected_index);

            let scrollbar = Scrollbar::default()
                .orientation(ScrollbarOrientation::VerticalRight)
                .begin_symbol(Some("?"))
                .end_symbol(Some("?"));

            f.render_stateful_widget(scrollbar, chunks[1], &mut scrollbar_state);

            // Footer / Status Bar Rendering
            let footer_content = match input_mode {
                InputMode::Normal => format!(
                    " [Enter] Expand | [e/c] Subtree | [E/C] All | [/] Search | [s] Save | [q] Discard | {}",
                    status_msg
                ),
                InputMode::Search => format!(
                    " SEARCH: {}_ (Press [Enter] to lock, [Esc] to clear)",
                    graph.search_query
                ),
            };

            let footer_style = if input_mode == InputMode::Search {
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(Color::DarkGray)
            };

            let footer = Paragraph::new(footer_content)
                .style(footer_style)
                .block(Block::default().borders(Borders::ALL));

            f.render_widget(footer, chunks[2]);
        })?;

        if event::poll(std::time::Duration::from_millis(50))? {
            if let Event::Key(key) = event::read()? {
                let page_size = (terminal.size()?.height.saturating_sub(8)) as usize;

                match input_mode {
                    InputMode::Search => match key.code {
                        KeyCode::Esc => {
                            graph.search_query.clear();
                            graph.rebuild_visible_rows();
                            input_mode = InputMode::Normal;
                            status_msg = String::from("Cleared search filter");
                        }
                        KeyCode::Enter => {
                            input_mode = InputMode::Normal;
                            status_msg = format!("Filter locked: '{}'", graph.search_query);
                        }
                        KeyCode::Backspace => {
                            graph.search_query.pop();
                            graph.rebuild_visible_rows();
                        }
                        KeyCode::Char(c) => {
                            graph.search_query.push(c);
                            graph.rebuild_visible_rows();
                        }
                        _ => {}
                    },
                    InputMode::Normal => {
                        let has_ctrl = key.modifiers.contains(KeyModifiers::CONTROL);

                        match key.code {
                            KeyCode::Char('/') => {
                                input_mode = InputMode::Search;
                            }

                            KeyCode::Char('s') | KeyCode::Char('S') => {
                                action = TuiAction::SaveAndQuit;
                                break;
                            }

                            KeyCode::Char('q') | KeyCode::Esc => {
                                action = TuiAction::QuitWithoutSaving;
                                break;
                            }

                            // Navigation: Top & Bottom
                            KeyCode::Home | KeyCode::Char('g')
                                if has_ctrl || key.code == KeyCode::Home =>
                            {
                                list_state.select(Some(0));
                                status_msg = String::from("Jumped to top");
                            }

                            KeyCode::End | KeyCode::Char('G')
                                if has_ctrl || key.code == KeyCode::End =>
                            {
                                let last = total_rows.saturating_sub(1);
                                list_state.select(Some(last));
                                status_msg = String::from("Jumped to bottom");
                            }

                            // Navigation: Fast Scroll (Ctrl + Up/Down)
                            KeyCode::Up if has_ctrl => {
                                let i = selected_index.saturating_sub(5);
                                list_state.select(Some(i));
                            }

                            KeyCode::Down if has_ctrl => {
                                let i = (selected_index + 5).min(total_rows.saturating_sub(1));
                                list_state.select(Some(i));
                            }

                            // Navigation: Single Up / Down
                            KeyCode::Up => {
                                let i = selected_index.saturating_sub(1);
                                list_state.select(Some(i));
                            }

                            KeyCode::Down => {
                                let i = (selected_index + 1).min(total_rows.saturating_sub(1));
                                list_state.select(Some(i));
                            }

                            // Navigation: Page Up / Page Down
                            KeyCode::PageUp => {
                                let i = selected_index.saturating_sub(page_size);
                                list_state.select(Some(i));
                                status_msg = format!("Page Up (-{} rows)", page_size);
                            }

                            KeyCode::PageDown => {
                                let i =
                                    (selected_index + page_size).min(total_rows.saturating_sub(1));
                                list_state.select(Some(i));
                                status_msg = format!("Page Down (+{} rows)", page_size);
                            }

                            // Tree Operations
                            KeyCode::Char('e') => {
                                if let Some(selected) = list_state.selected() {
                                    graph.expand_subtree(selected);
                                    status_msg = format!("Expanded subtree at row {}", selected);
                                }
                            }

                            KeyCode::Char('c') => {
                                if let Some(selected) = list_state.selected() {
                                    graph.collapse_subtree(selected);
                                    status_msg = format!("Collapsed subtree at row {}", selected);
                                }
                            }

                            KeyCode::Char('E') => {
                                graph.expand_all();
                                status_msg = String::from("Expanded all subtrees");
                            }

                            KeyCode::Char('C') => {
                                graph.collapse_all();
                                status_msg = String::from("Collapsed all subtrees");
                            }

                            KeyCode::Enter | KeyCode::Char('\n') | KeyCode::Char('\r') => {
                                if let Some(selected) = list_state.selected() {
                                    graph.toggle_expand(selected);
                                    status_msg = format!("Toggled node at row {}", selected);
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
                                                    format!("Toggled option '{}'", opt_name);
                                            }
                                            _ => {
                                                status_msg =
                                                    String::from("Cannot toggle non-option row");
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
        }
    }

    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
    terminal.show_cursor()?;

    Ok(action)
}
