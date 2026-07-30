use crate::graph::{next_sibling_index, prev_sibling_index, DependencyGraph, RowKind};
use anyhow::Result;
use crossterm::{
    event::{self, Event, KeyCode, KeyModifiers},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::{
    backend::CrosstermBackend,
    layout::{Alignment, Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span, Text},
    widgets::{
        Block, Borders, Clear, List, ListItem, ListState, Paragraph, Scrollbar,
        ScrollbarOrientation, ScrollbarState,
    },
    Frame, Terminal,
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
    ConfirmQuit,
}

/// Focus stops, cycled by Tab / Shift + Tab the way `bsddialog(1)` does: the
/// item list, then each button in the row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Focus {
    List,
    Ok,
    Cancel,
}

pub fn next_focus(focus: Focus) -> Focus {
    match focus {
        Focus::List => Focus::Ok,
        Focus::Ok => Focus::Cancel,
        Focus::Cancel => Focus::List,
    }
}

pub fn prev_focus(focus: Focus) -> Focus {
    match focus {
        Focus::List => Focus::Cancel,
        Focus::Cancel => Focus::Ok,
        Focus::Ok => Focus::List,
    }
}

/// Button focus inside the "unsaved changes" confirmation box.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConfirmChoice {
    Yes,
    No,
}

impl ConfirmChoice {
    pub fn toggle(self) -> Self {
        match self {
            ConfirmChoice::Yes => ConfirmChoice::No,
            ConfirmChoice::No => ConfirmChoice::Yes,
        }
    }
}

/// Tree visibility commands, scoped by *repetition* rather than by modifiers.
///
/// No terminal can distinguish `Ctrl + =` or `Ctrl + Shift + -` from their plain
/// forms — `=` and `+` have no C0 control code at all — so scope escalates by
/// pressing the same key twice in a row instead.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TreeOp {
    ExpandNode,
    CollapseNode,
    ExpandSection,
    CollapseSection,
    ExpandAll,
    CollapseAll,
}

/// Maps a tree key to its operation. `repeat` means this same key was the
/// *immediately* preceding keystroke; any other key in between breaks the run.
///
/// `=` and `-` always act on the single row under the cursor, so pressing them
/// repeatedly is idempotent. `+` and `_` act on the section under the cursor,
/// and a second consecutive press escalates to the whole tree.
pub fn tree_op(key: char, repeat: bool) -> Option<TreeOp> {
    match key {
        '=' => Some(TreeOp::ExpandNode),
        '-' => Some(TreeOp::CollapseNode),
        '+' => Some(if repeat {
            TreeOp::ExpandAll
        } else {
            TreeOp::ExpandSection
        }),
        '_' => Some(if repeat {
            TreeOp::CollapseAll
        } else {
            TreeOp::CollapseSection
        }),
        _ => None,
    }
}

/// Where `Ctrl + L` parks the cursor row within the viewport.
///
/// Mirrors Emacs' `recenter-top-bottom`: the first press centers the current
/// line, and each immediately following press advances through the default
/// `recenter-positions` cycle of middle -> top -> bottom -> middle. Any other
/// keystroke breaks the run, so the next `Ctrl + L` centers again.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecenterPosition {
    Middle,
    Top,
    Bottom,
}

impl RecenterPosition {
    pub fn next(self) -> Self {
        match self {
            RecenterPosition::Middle => RecenterPosition::Top,
            RecenterPosition::Top => RecenterPosition::Bottom,
            RecenterPosition::Bottom => RecenterPosition::Middle,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            RecenterPosition::Middle => "middle",
            RecenterPosition::Top => "top",
            RecenterPosition::Bottom => "bottom",
        }
    }
}

/// Scroll offset that places `selected` at `position` within a viewport of
/// `viewport_height` rows. The list can never scroll past its first row, so the
/// requested position is only reached when enough rows precede the cursor.
pub fn recenter_offset(
    selected: usize,
    viewport_height: usize,
    total_rows: usize,
    position: RecenterPosition,
) -> usize {
    if viewport_height == 0 {
        return selected;
    }

    let offset = match position {
        RecenterPosition::Top => selected,
        RecenterPosition::Middle => selected.saturating_sub(viewport_height / 2),
        RecenterPosition::Bottom => selected.saturating_sub(viewport_height - 1),
    };

    offset.min(total_rows.saturating_sub(1))
}

/// Renders one button as `< Label >` with its first character highlighted, which
/// is how dialog advertises the letter hotkey that presses it.
fn button_spans(label: &str, focused: bool) -> Vec<Span<'static>> {
    let base = if focused {
        Style::default()
            .bg(Color::Blue)
            .fg(Color::White)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(Color::Gray)
    };

    let hotkey_style = if focused {
        base.add_modifier(Modifier::UNDERLINED)
    } else {
        base.fg(Color::Yellow).add_modifier(Modifier::UNDERLINED)
    };

    let mut chars = label.chars();
    let hotkey = chars.next().unwrap_or(' ');
    let rest: String = chars.collect();

    vec![
        Span::styled("< ", base),
        Span::styled(hotkey.to_string(), hotkey_style),
        Span::styled(rest, base),
        Span::styled(" >", base),
    ]
}

fn button_row(buttons: &[(&str, bool)]) -> Line<'static> {
    let mut spans = Vec::new();
    for (i, (label, focused)) in buttons.iter().enumerate() {
        if i > 0 {
            spans.push(Span::raw("   "));
        }
        spans.extend(button_spans(label, *focused));
    }
    Line::from(spans)
}

fn centered_rect(width: u16, height: u16, area: Rect) -> Rect {
    let w = width.min(area.width);
    let h = height.min(area.height);
    Rect {
        x: area.x + area.width.saturating_sub(w) / 2,
        y: area.y + area.height.saturating_sub(h) / 2,
        width: w,
        height: h,
    }
}

fn render_confirm_quit(f: &mut Frame, choice: ConfirmChoice) {
    let area = centered_rect(44, 6, f.size());
    f.render_widget(Clear, area);

    let body = Text::from(vec![
        Line::from(""),
        Line::from("You have unsaved option changes."),
        Line::from(""),
        button_row(&[
            ("Yes", choice == ConfirmChoice::Yes),
            ("No", choice == ConfirmChoice::No),
        ]),
    ]);

    let modal = Paragraph::new(body)
        .alignment(Alignment::Center)
        .style(Style::default().fg(Color::White))
        .block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(Color::Yellow))
                .title(" Discard changes and quit? "),
        );

    f.render_widget(modal, area);
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
    let mut focus = Focus::List;
    let mut confirm_choice = ConfirmChoice::Yes;
    let mut recenter_pos: Option<RecenterPosition> = None;
    let mut last_tree_key: Option<char> = None;
    let mut viewport_height: usize = 0;

    loop {
        let total_rows = graph.visible_rows.len();
        let selected_index = list_state.selected().unwrap_or(0);

        terminal.draw(|f| {
            let chunks = Layout::default()
                .direction(Direction::Vertical)
                .constraints([
                    Constraint::Length(3),
                    Constraint::Min(5),
                    Constraint::Length(4),
                    Constraint::Length(3),
                ])
                .split(f.size());

            // Rows the tree list can actually show, minus its top/bottom border
            viewport_height = chunks[1].height.saturating_sub(2) as usize;

            // Target on the left, most recent action right-aligned, which keeps
            // the footer free for keybindings alone
            let target = format!(" Target: {}", graph.root_origin);
            let status = format!("{} ", status_msg);
            let gap = (chunks[0].width.saturating_sub(2) as usize)
                .saturating_sub(target.chars().count() + status.chars().count())
                .max(1);

            let header = Paragraph::new(Line::from(vec![
                Span::styled(
                    target,
                    Style::default()
                        .fg(Color::Cyan)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::raw(" ".repeat(gap)),
                Span::styled(status, Style::default().fg(Color::Gray)),
            ]))
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

            // The cursor row is only highlighted while the list holds focus, so
            // the focused button is never ambiguous
            let highlight_style = if focus == Focus::List {
                Style::default()
                    .bg(Color::Blue)
                    .fg(Color::White)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().add_modifier(Modifier::REVERSED)
            };

            let tree_list = List::new(items)
                .block(
                    Block::default()
                        .borders(Borders::ALL)
                        .title(" Options & Dependencies "),
                )
                .highlight_style(highlight_style)
                .highlight_symbol("> ");

            f.render_stateful_widget(tree_list, chunks[1], &mut list_state);

            // Render Right-Hand Scrollbar
            let mut scrollbar_state =
                ScrollbarState::new(total_rows.saturating_sub(1)).position(selected_index);

            let scrollbar = Scrollbar::default()
                .orientation(ScrollbarOrientation::VerticalRight)
                .begin_symbol(Some("?"))
                .end_symbol(Some("?"));

            f.render_stateful_widget(scrollbar, chunks[1], &mut scrollbar_state);

            // Footer / Status Bar Rendering
            let (primary_line, primary_style) = match input_mode {
                InputMode::Search => (
                    format!(
                        " SEARCH: {}_ (Press [Enter] to lock, [Esc] to clear)",
                        graph.search_query
                    ),
                    Style::default()
                        .fg(Color::Yellow)
                        .add_modifier(Modifier::BOLD),
                ),
                _ => (
                    " =/- open/close row | +/_ open/close branch | ++/__ open/close tree | / search"
                        .to_string(),
                    Style::default().fg(Color::DarkGray),
                ),
            };

            let secondary_line =
                " Space toggle | Tab OK/Cancel | ^S save | q quit | ^L recenter | ^R redraw"
                    .to_string();

            let footer = Paragraph::new(Text::from(vec![
                Line::styled(primary_line, primary_style),
                Line::styled(secondary_line, Style::default().fg(Color::DarkGray)),
            ]))
            .block(Block::default().borders(Borders::ALL));

            f.render_widget(footer, chunks[2]);

            let buttons = Paragraph::new(button_row(&[
                ("OK", focus == Focus::Ok),
                ("Cancel", focus == Focus::Cancel),
            ]))
            .alignment(Alignment::Center)
            .block(Block::default().borders(Borders::ALL));

            f.render_widget(buttons, chunks[3]);

            if input_mode == InputMode::ConfirmQuit {
                render_confirm_quit(f, confirm_choice);
            }
        })?;

        if event::poll(std::time::Duration::from_millis(50))? {
            if let Event::Key(key) = event::read()? {
                let code = key.code;
                let has_ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
                let has_shift = key.modifiers.contains(KeyModifiers::SHIFT);
                let page_size = viewport_height.max(1);

                // Runs of the same key drive the recenter cycle and the tree
                // scope escalation; anything else breaks the run
                let is_recenter =
                    has_ctrl && matches!(code, KeyCode::Char('l') | KeyCode::Char('L'));
                if !is_recenter {
                    recenter_pos = None;
                }
                let tree_key = match code {
                    KeyCode::Char(c @ ('=' | '-' | '+' | '_')) if !has_ctrl => Some(c),
                    _ => None,
                };
                let repeat = tree_key.is_some() && tree_key == last_tree_key;
                last_tree_key = tree_key;

                // Ctrl+R repaints from any mode without disturbing the view
                if has_ctrl && matches!(code, KeyCode::Char('r') | KeyCode::Char('R')) {
                    terminal.clear()?;
                    status_msg = String::from("Redrew screen");
                    continue;
                }

                let mut quit_requested = false;
                let mut save_requested = false;

                match input_mode {
                    InputMode::ConfirmQuit => match code {
                        KeyCode::Char('y') | KeyCode::Char('Y') => {
                            action = TuiAction::QuitWithoutSaving;
                            break;
                        }
                        KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => {
                            input_mode = InputMode::Normal;
                            status_msg = String::from("Returned to configuration");
                        }
                        KeyCode::Left
                        | KeyCode::Right
                        | KeyCode::Tab
                        | KeyCode::BackTab
                        | KeyCode::Up
                        | KeyCode::Down => {
                            confirm_choice = confirm_choice.toggle();
                        }
                        KeyCode::Enter | KeyCode::Char(' ') => match confirm_choice {
                            ConfirmChoice::Yes => {
                                action = TuiAction::QuitWithoutSaving;
                                break;
                            }
                            ConfirmChoice::No => {
                                input_mode = InputMode::Normal;
                                status_msg = String::from("Returned to configuration");
                            }
                        },
                        _ => {}
                    },

                    // Control chords must not type themselves into the query
                    InputMode::Search if has_ctrl => {}
                    InputMode::Search => match code {
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
                        match code {
                            // Focus: Tab cycles List -> OK -> Cancel -> List
                            KeyCode::Tab => {
                                focus = next_focus(focus);
                            }

                            KeyCode::BackTab => {
                                focus = prev_focus(focus);
                            }

                            // Buttons: Enter presses the focused one, defaulting
                            // to OK while the list has focus, as dialog does
                            KeyCode::Enter => match focus {
                                Focus::Cancel => quit_requested = true,
                                _ => save_requested = true,
                            },

                            // Letter hotkeys work from any focus
                            KeyCode::Char('o') | KeyCode::Char('O') => save_requested = true,

                            KeyCode::Char('s') | KeyCode::Char('S') => save_requested = true,

                            KeyCode::Char('c') | KeyCode::Char('C') => quit_requested = true,

                            KeyCode::Char('q') | KeyCode::Esc => quit_requested = true,

                            KeyCode::Char('/') => {
                                input_mode = InputMode::Search;
                            }

                            KeyCode::Char('f') | KeyCode::Char('F') if has_ctrl => {
                                input_mode = InputMode::Search;
                            }

                            // View: cycle the cursor row middle -> top -> bottom
                            // and repaint the screen
                            KeyCode::Char('l') | KeyCode::Char('L') if has_ctrl => {
                                let pos = match recenter_pos {
                                    Some(prev) => prev.next(),
                                    None => RecenterPosition::Middle,
                                };
                                recenter_pos = Some(pos);
                                *list_state.offset_mut() = recenter_offset(
                                    selected_index,
                                    viewport_height,
                                    total_rows,
                                    pos,
                                );
                                terminal.clear()?;
                                status_msg = format!("Recentered row to {}", pos.label());
                            }

                            // Button row: Left/Right move between buttons
                            KeyCode::Left if focus != Focus::List => {
                                focus = Focus::Ok;
                            }

                            KeyCode::Right if focus != Focus::List => {
                                focus = Focus::Cancel;
                            }

                            KeyCode::Char(' ') if focus != Focus::List => match focus {
                                Focus::Cancel => quit_requested = true,
                                _ => save_requested = true,
                            },

                            // Everything below acts on the tree, so it only
                            // applies while the list holds focus
                            _ if focus != Focus::List => {}

                            // Navigation: Siblings (Shift + Up/Down)
                            KeyCode::Down if has_shift => {
                                let depths: Vec<usize> =
                                    graph.visible_rows.iter().map(|r| r.depth).collect();
                                let i = next_sibling_index(&depths, selected_index);
                                list_state.select(Some(i));
                                status_msg = String::from("Jumped to next sibling");
                            }

                            KeyCode::Up if has_shift => {
                                let depths: Vec<usize> =
                                    graph.visible_rows.iter().map(|r| r.depth).collect();
                                let i = prev_sibling_index(&depths, selected_index);
                                list_state.select(Some(i));
                                status_msg = String::from("Jumped to previous sibling");
                            }

                            // Navigation: Top & Bottom
                            KeyCode::Home | KeyCode::Char('g')
                                if has_ctrl || code == KeyCode::Home =>
                            {
                                list_state.select(Some(0));
                                status_msg = String::from("Jumped to top");
                            }

                            KeyCode::End | KeyCode::Char('G')
                                if has_ctrl || code == KeyCode::End =>
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

                            // Tree Operations: = / - act on the row under the
                            // cursor, + / _ on its section, and pressing + or _
                            // twice in a row escalates to the whole tree
                            KeyCode::Char(c) if tree_key == Some(c) => {
                                if let Some(op) = tree_op(c, repeat) {
                                    let selected = list_state.selected().unwrap_or(0);
                                    status_msg = match op {
                                        TreeOp::ExpandNode => {
                                            graph.expand_node(selected);
                                            format!("Expanded row {}", selected)
                                        }
                                        TreeOp::CollapseNode => {
                                            graph.collapse_node(selected);
                                            format!("Collapsed row {}", selected)
                                        }
                                        TreeOp::ExpandSection => {
                                            graph.expand_subtree(selected);
                                            format!("Expanded section at row {}", selected)
                                        }
                                        TreeOp::CollapseSection => {
                                            graph.collapse_subtree(selected);
                                            format!("Collapsed section at row {}", selected)
                                        }
                                        TreeOp::ExpandAll => {
                                            graph.expand_all();
                                            String::from("Expanded all subtrees")
                                        }
                                        TreeOp::CollapseAll => {
                                            graph.collapse_all();
                                            String::from("Collapsed all subtrees")
                                        }
                                    };
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

                if save_requested {
                    action = TuiAction::SaveAndQuit;
                    break;
                }

                if quit_requested {
                    if graph.is_dirty() {
                        input_mode = InputMode::ConfirmQuit;
                        confirm_choice = ConfirmChoice::Yes;
                        status_msg = String::from("Unsaved changes");
                    } else {
                        action = TuiAction::QuitWithoutSaving;
                        break;
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
