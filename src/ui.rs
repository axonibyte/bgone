use crate::config::save_groups;
use crate::graph::{
    next_sibling_index, prev_sibling_index, DependencyGraph, Provenance, RowAnchor, RowKind,
    SectionKind,
};
use anyhow::Result;
use crossterm::{
    event::{self, Event, KeyCode, KeyEvent, KeyModifiers},
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
use std::collections::BTreeMap;
use std::io::{stdout, Write};
use std::path::{Path, PathBuf};

/// The action keys shown along the bottom of the screen, one row per line.
///
/// A constant rather than an inline literal so `--help` can be checked against
/// it. Every key advertised here has to be explained there, and drift between
/// the two is invisible otherwise — `Ctrl + R` was dropped from this row once
/// without anything noticing.
///
/// Split across two rows because one would run to 110 columns and be truncated
/// on an 80-column terminal, silently taking the last few keys with it. Editing
/// keys first, then the ones that end or reposition the session.
pub const FOOTER_ACTION_KEYS: &str = concat!(
    " Space toggle | Enter jump | Bksp back | ^G group\n",
    " Tab OK/Cancel | ^S save | q quit | ^L recenter | ^R redraw",
);

#[derive(Debug, PartialEq, Eq)]
pub enum TuiAction {
    SaveAndQuit,
    QuitWithoutSaving,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum InputMode {
    Normal,
    Search,
    ConfirmQuit,
    /// Choosing which group the port under the cursor joins.
    GroupAssign,
    /// Naming a group that does not exist yet.
    GroupNewName,
    /// Browsing groups and their members.
    GroupManage,
    /// Naming a config file to save groups into, when none was given.
    ConfigPath,
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

/// Row index of the port `origin` is listed at, if it is currently listed. A
/// search filter can hide it, which is why this is fallible.
fn port_row_index(graph: &DependencyGraph, origin: &str) -> Option<usize> {
    graph
        .visible_rows
        .iter()
        .position(|r| matches!(&r.kind, RowKind::Port { origin: o, .. } if o == origin))
}

/// The port whose block `row` sits inside — the nearest port row at or above it.
///
/// Jump history is kept as origins rather than row indices because expanding a
/// port rebuilds the row list and shifts every index after it.
fn enclosing_port_origin(graph: &DependencyGraph, row: usize) -> Option<String> {
    graph
        .visible_rows
        .get(..=row)?
        .iter()
        .rev()
        .find_map(|r| match &r.kind {
            RowKind::Port { origin, .. } => Some(origin.clone()),
            _ => None,
        })
}

/// Moves to `origin`'s entry, opening it on arrival. `None` when that port is
/// not currently listed.
fn jump_to_port(graph: &mut DependencyGraph, origin: &str) -> Option<usize> {
    let port_id = graph.port_index(origin)?;
    if !graph.ports[port_id].is_expanded {
        graph.open_port(port_id);
    }
    port_row_index(graph, origin)
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

/// True when a key means "rub out the character behind the cursor".
///
/// Terminals disagree about backspace. Most send DEL (`0x7F`), which crossterm
/// reports as [`KeyCode::Backspace`]; some send BS (`0x08`), which its parser
/// folds into the `0x01..=0x1A` control range and reports as `Ctrl+H` instead
/// (`crossterm/src/event/sys/unix/parse.rs`). Text entry has to accept both or
/// it silently ignores the key on half the terminals in use — and `Ctrl+H` has
/// meant backspace since long before either of them.
fn is_backspace(code: KeyCode, has_ctrl: bool) -> bool {
    matches!(code, KeyCode::Backspace) || (has_ctrl && matches!(code, KeyCode::Char('h')))
}

/// Names the row at `index` for a status message — what was acted on, rather
/// than the row number it happened to have.
fn row_label(graph: &DependencyGraph, index: usize) -> String {
    match graph.visible_rows.get(index).map(|r| &r.kind) {
        Some(RowKind::Port { origin, .. }) => origin.clone(),
        Some(RowKind::Option { name, .. }) => name.clone(),
        Some(RowKind::SectionHeader { kind, .. }) => match kind {
            SectionKind::Requires => String::from("requires"),
            SectionKind::RequiredBy => String::from("required by"),
        },
        _ => String::from("this row"),
    }
}

/// Applies an editing key to a text field, reporting whether it was one.
///
/// Shared by the two prompts and the search bar so they all edit alike: typing
/// and rubbing out happen *at* the caret rather than always at the end, which is
/// what makes moving it worth anything.
///
/// The caret is counted in characters, not bytes. Port origins and file paths
/// can carry multibyte characters, and `String::insert`/`remove` at a byte
/// offset landing inside one would panic.
fn edit_field(text: &mut String, cursor: &mut usize, code: KeyCode, has_ctrl: bool) -> bool {
    let chars = text.chars().count();
    *cursor = (*cursor).min(chars);
    let byte_at = |t: &str, i: usize| t.char_indices().nth(i).map(|(b, _)| b).unwrap_or(t.len());

    if is_backspace(code, has_ctrl) {
        if *cursor > 0 {
            let at = byte_at(text, *cursor - 1);
            text.remove(at);
            *cursor -= 1;
        }
        return true;
    }

    match code {
        KeyCode::Left => *cursor = cursor.saturating_sub(1),
        KeyCode::Right => *cursor = (*cursor + 1).min(chars),
        KeyCode::Home => *cursor = 0,
        KeyCode::End => *cursor = chars,
        KeyCode::Delete => {
            if *cursor < chars {
                let at = byte_at(text, *cursor);
                text.remove(at);
            }
        }
        // A control chord must not type itself into the field
        KeyCode::Char(c) if !has_ctrl => {
            let at = byte_at(text, *cursor);
            text.insert(at, c);
            *cursor += 1;
        }
        _ => return false,
    }
    true
}

/// Renders a field with a block caret sitting on the character it is in front
/// of, so its position is visible rather than implied.
fn field_spans(text: &str, cursor: usize, style: Style) -> Vec<Span<'static>> {
    let at = text
        .char_indices()
        .nth(cursor)
        .map(|(b, _)| b)
        .unwrap_or(text.len());
    let (before, rest) = text.split_at(at);
    let mut rest_chars = rest.chars();
    let under = rest_chars.next();
    let after: String = rest_chars.collect();

    vec![
        Span::styled(before.to_string(), style),
        Span::styled(
            under.map(String::from).unwrap_or_else(|| " ".to_string()),
            style.add_modifier(Modifier::REVERSED),
        ),
        Span::styled(after, style),
    ]
}

/// One line of the group manager: either a group's name or one of its members.
///
/// Flattened per frame rather than kept as state, so the view cannot drift out
/// of step with the groups it is showing.
enum ManageRow {
    Group(String),
    Member { group: String, origin: String },
}

fn manage_rows(groups: &BTreeMap<String, Vec<String>>) -> Vec<ManageRow> {
    let mut rows = Vec::new();
    for (name, members) in groups {
        rows.push(ManageRow::Group(name.clone()));
        for origin in members {
            rows.push(ManageRow::Member {
                group: name.clone(),
                origin: origin.clone(),
            });
        }
    }
    rows
}

/// A bordered box with a title and a hint line along the bottom.
fn modal_block(title: &str) -> Block<'static> {
    Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Yellow))
        .title(format!(" {title} "))
}

/// Picks the group to add a port to, or offers to start a new one.
fn render_group_assign(
    f: &mut Frame,
    target: &str,
    groups: &BTreeMap<String, Vec<String>>,
    cursor: usize,
) {
    let height = (groups.len() as u16 + 6).min(f.size().height.saturating_sub(2));
    let area = centered_rect(64, height.max(6), f.size());
    f.render_widget(Clear, area);

    let mut lines = vec![Line::from(vec![
        Span::raw("Add "),
        Span::styled(target.to_string(), Style::default().fg(Color::White)),
        Span::raw(" to:"),
    ])];

    for (i, (name, members)) in groups.iter().enumerate() {
        let already = members.iter().any(|m| m == target);
        let label = if already {
            format!("{name}  (already a member)")
        } else {
            format!("{name}  ({} ports)", members.len())
        };
        lines.push(selectable_line(&label, i == cursor, already));
    }
    lines.push(selectable_line(
        "<new group...>",
        cursor == groups.len(),
        false,
    ));
    lines.push(Line::from(""));
    lines.push(Line::styled(
        "Up/Down choose | Enter confirm | Esc cancel",
        Style::default().fg(Color::DarkGray),
    ));

    f.render_widget(
        Paragraph::new(Text::from(lines)).block(modal_block("Add to group")),
        area,
    );
}

fn selectable_line(label: &str, selected: bool, dimmed: bool) -> Line<'static> {
    let marker = if selected { "> " } else { "  " };
    let style = if selected {
        Style::default()
            .bg(Color::Blue)
            .fg(Color::White)
            .add_modifier(Modifier::BOLD)
    } else if dimmed {
        Style::default().fg(Color::DarkGray)
    } else {
        Style::default()
    };
    Line::styled(format!("{marker}{label}"), style)
}

/// Browses groups and their members, and is where they get saved.
fn render_group_manage(
    f: &mut Frame,
    groups: &BTreeMap<String, Vec<String>>,
    cursor: usize,
    config_path: Option<&Path>,
) {
    let rows = manage_rows(groups);
    let height = (rows.len() as u16 + 6).min(f.size().height.saturating_sub(2));
    let area = centered_rect(70, height.max(7), f.size());
    f.render_widget(Clear, area);

    let mut lines = Vec::new();
    if rows.is_empty() {
        lines.push(Line::styled(
            "  No groups yet. Ctrl+G on a port starts one.",
            Style::default().fg(Color::DarkGray),
        ));
    }
    for (i, row) in rows.iter().enumerate() {
        match row {
            ManageRow::Group(name) => {
                let count = groups.get(name).map(|m| m.len()).unwrap_or(0);
                lines.push(selectable_line(
                    &format!("{name}  ({count} ports)"),
                    i == cursor,
                    false,
                ));
            }
            ManageRow::Member { origin, .. } => {
                lines.push(selectable_line(
                    &format!("    {origin}"),
                    i == cursor,
                    false,
                ));
            }
        }
    }

    lines.push(Line::from(""));
    lines.push(Line::styled(
        match config_path {
            Some(path) => format!("  saves to {}", path.display()),
            None => "  no config file yet; saving will ask for a path".to_string(),
        },
        Style::default().fg(Color::DarkGray),
    ));
    lines.push(Line::styled(
        "Up/Down choose | d remove | s save to config | Esc close",
        Style::default().fg(Color::DarkGray),
    ));

    f.render_widget(
        Paragraph::new(Text::from(lines)).block(modal_block("Groups")),
        area,
    );
}

/// Shared single-line text prompt, for a new group name or a config path.
fn render_prompt(f: &mut Frame, title: &str, question: &str, buffer: &str, cursor: usize) {
    let area = centered_rect(70, 7, f.size());
    f.render_widget(Clear, area);

    let body = Text::from(vec![
        Line::from(""),
        Line::from(question.to_string()),
        {
            let style = Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD);
            let mut spans = vec![Span::styled("  ", style)];
            spans.extend(field_spans(buffer, cursor, style));
            Line::from(spans)
        },
        Line::from(""),
        Line::styled(
            "Enter confirm | Esc cancel",
            Style::default().fg(Color::DarkGray),
        ),
    ]);

    f.render_widget(Paragraph::new(body).block(modal_block(title)), area);
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

/// Everything the interface remembers between keystrokes.
///
/// Held as one value rather than as locals inside the event loop so that key
/// handling is a plain function of (state, key) and can be exercised without a
/// terminal. The dialogs went in without that and shipped with an unreachable
/// group manager, which reading the code twice did not catch.
struct App {
    input_mode: InputMode,
    focus: Focus,
    list_state: ListState,
    status_msg: String,
    confirm_choice: ConfirmChoice,
    recenter_pos: Option<RecenterPosition>,
    last_tree_key: Option<char>,
    /// Rows the list can show, learned from the last frame drawn.
    viewport_height: usize,
    /// Ports each jump came from, so Backspace can retrace it. Origins rather
    /// than row indices, which shift whenever the row list is rebuilt.
    jump_stack: Vec<String>,
    config_path: Option<PathBuf>,
    group_target: Option<String>,
    group_cursor: usize,
    text_buffer: String,
    /// Caret position within whichever field is being typed into, in characters.
    text_cursor: usize,
}

impl App {
    fn new(config_path: Option<PathBuf>) -> Self {
        let mut list_state = ListState::default();
        list_state.select(Some(0));
        Self {
            input_mode: InputMode::Normal,
            focus: Focus::List,
            list_state,
            status_msg: String::from("Ready"),
            confirm_choice: ConfirmChoice::Yes,
            recenter_pos: None,
            last_tree_key: None,
            viewport_height: 0,
            jump_stack: Vec::new(),
            config_path,
            group_target: None,
            group_cursor: 0,
            text_buffer: String::new(),
            text_cursor: 0,
        }
    }
}

/// What the event loop should do after a keystroke.
#[derive(Debug, PartialEq, Eq)]
enum KeyOutcome {
    Continue,
    /// Repaint from scratch. Returned rather than performed so that key handling
    /// never touches the terminal.
    Redraw,
    Finish(TuiAction),
}

impl App {
    /// Writes the groups out if a config file is already known.
    ///
    /// Applied to every change to a group, not only additions: if adding saved
    /// but removing did not, deleting something and quitting would leave it in
    /// the file, and the config would quietly disagree with what was on screen.
    ///
    /// With no config file chosen there is nowhere to write, and interrupting an
    /// edit to demand a path would be worse than waiting — the manager's `s` is
    /// there for that.
    fn autosave_groups(&mut self, graph: &DependencyGraph, did: &str) {
        let Some(path) = self.config_path.clone() else {
            self.status_msg = format!("{did} (unsaved; no config file)");
            return;
        };
        match save_groups(&path, &graph.groups) {
            Ok(()) => self.status_msg = format!("{did}, saved"),
            Err(e) => self.status_msg = format!("{did}, but could not save: {e}"),
        }
    }

    /// Puts the cursor back on the row it was on before the row list changed.
    ///
    /// Expanding or collapsing renumbers everything below the change, so keeping
    /// the row *number* still slides the highlight onto some unrelated line.
    /// Keeping the row itself still is what a reader expects. When that row is
    /// no longer shown — collapsing a port hides the option the cursor was on —
    /// the anchor falls outwards to the port it belonged to.
    fn restore_anchor(&mut self, graph: &DependencyGraph, anchor: Option<RowAnchor>) {
        let here = self.list_state.selected().unwrap_or(0);
        self.restore_anchor_or(graph, anchor, here);
    }

    /// As [`restore_anchor`](Self::restore_anchor), but says where to land when
    /// the anchored row is gone entirely.
    fn restore_anchor_or(
        &mut self,
        graph: &DependencyGraph,
        anchor: Option<RowAnchor>,
        fallback: usize,
    ) {
        let last = graph.visible_rows.len().saturating_sub(1);
        let row = anchor
            .and_then(|a| graph.row_of_anchor(&a))
            .unwrap_or(fallback);
        self.list_state.select(Some(row.min(last)));
    }

    /// Moves the list cursor `delta` rows from `from`, clamped to the list.
    ///
    /// Shared by Normal mode and the search bar so that a row moved to while
    /// the query is still being typed lands exactly where the same keystroke
    /// would land afterwards — in particular, clamping to the *filtered* list,
    /// so holding Down cannot walk off the last match into hidden rows.
    fn move_selection(&mut self, from: usize, delta: isize, total_rows: usize) {
        let last = total_rows.saturating_sub(1);
        let row = if delta < 0 {
            from.saturating_sub(delta.unsigned_abs())
        } else {
            from.saturating_add(delta as usize).min(last)
        };
        self.list_state.select(Some(row));
    }

    /// Folds one keystroke into the state, reporting anything the event loop
    /// has to act on. Pure with respect to the terminal, so it can be driven
    /// directly by tests.
    fn on_key(&mut self, key: KeyEvent, graph: &mut DependencyGraph) -> KeyOutcome {
        let total_rows = graph.visible_rows.len();
        let selected_index = self.list_state.selected().unwrap_or(0);
        // Set where the old code called terminal.clear() directly; reported back
        // instead so this stays free of the terminal.
        let mut redraw = false;
        let code = key.code;
        let has_ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let has_shift = key.modifiers.contains(KeyModifiers::SHIFT);
        let page_size = self.viewport_height.max(1);

        // Runs of the same key drive the recenter cycle and the tree
        // scope escalation; anything else breaks the run
        let is_recenter = has_ctrl && matches!(code, KeyCode::Char('l') | KeyCode::Char('L'));
        if !is_recenter {
            self.recenter_pos = None;
        }
        let tree_key = match code {
            KeyCode::Char(c @ ('=' | '-' | '+' | '_')) if !has_ctrl => Some(c),
            _ => None,
        };
        let repeat = tree_key.is_some() && tree_key == self.last_tree_key;
        self.last_tree_key = tree_key;

        // Ctrl+R repaints from any mode without disturbing the view
        if has_ctrl && matches!(code, KeyCode::Char('r') | KeyCode::Char('R')) {
            self.status_msg = String::from("Redrew screen");
            return KeyOutcome::Redraw;
        }

        let mut quit_requested = false;
        let mut save_requested = false;

        match self.input_mode {
            // ------------------------------------------------ groups
            InputMode::GroupAssign => {
                let count = graph.groups.len();
                match code {
                    // The second of the two presses --help describes.
                    // Escalating by mode rather than by tracking a run is
                    // what makes this reachable at all: the first press has
                    // already left Normal, so a run counter there never sees
                    // the second one.
                    KeyCode::Char('g') | KeyCode::Char('G') if has_ctrl => {
                        self.group_target = None;
                        self.group_cursor = 0;
                        self.input_mode = InputMode::GroupManage;
                    }
                    KeyCode::Esc => {
                        self.input_mode = InputMode::Normal;
                        self.group_target = None;
                    }
                    KeyCode::Up => self.group_cursor = self.group_cursor.saturating_sub(1),
                    KeyCode::Down => self.group_cursor = (self.group_cursor + 1).min(count),
                    KeyCode::Enter => {
                        let target = self.group_target.clone().unwrap_or_default();
                        if self.group_cursor == count {
                            // "<new group...>"
                            self.text_buffer.clear();
                            self.text_cursor = 0;
                            self.input_mode = InputMode::GroupNewName;
                        } else {
                            let name = graph
                                .groups
                                .keys()
                                .nth(self.group_cursor)
                                .cloned()
                                .unwrap_or_default();
                            let members = graph.groups.entry(name.clone()).or_default();
                            if members.contains(&target) {
                                self.status_msg = format!("{target} is already in {name}");
                            } else {
                                members.push(target.clone());
                                members.sort();
                                self.status_msg = format!("Added {target} to {name}");
                            }
                            self.input_mode = InputMode::Normal;
                            self.group_target = None;
                        }
                    }
                    _ => {}
                }
            }

            InputMode::GroupNewName
                if edit_field(&mut self.text_buffer, &mut self.text_cursor, code, has_ctrl) => {}
            InputMode::GroupNewName => match code {
                KeyCode::Esc => {
                    self.input_mode = InputMode::Normal;
                    self.group_target = None;
                }
                KeyCode::Enter => {
                    let name = self.text_buffer.trim().to_string();
                    let target = self.group_target.clone().unwrap_or_default();
                    if name.is_empty() {
                        self.status_msg = String::from("A group needs a name");
                    } else {
                        let members = graph.groups.entry(name.clone()).or_default();
                        if !members.contains(&target) {
                            members.push(target.clone());
                            members.sort();
                        }
                        self.autosave_groups(graph, &format!("Added {target} to {name}"));
                        self.input_mode = InputMode::Normal;
                        self.group_target = None;
                    }
                }
                _ => {}
            },

            InputMode::GroupManage => {
                let rows = manage_rows(&graph.groups);
                match code {
                    KeyCode::Esc | KeyCode::Char('q') => self.input_mode = InputMode::Normal,
                    KeyCode::Up => self.group_cursor = self.group_cursor.saturating_sub(1),
                    KeyCode::Down => {
                        self.group_cursor =
                            (self.group_cursor + 1).min(rows.len().saturating_sub(1))
                    }
                    KeyCode::Char('d') | KeyCode::Delete => {
                        match rows.get(self.group_cursor) {
                            Some(ManageRow::Group(name)) => {
                                let name = name.clone();
                                graph.groups.remove(&name);
                                self.autosave_groups(graph, &format!("Deleted group {name}"));
                            }
                            Some(ManageRow::Member { group, origin }) => {
                                if let Some(members) = graph.groups.get_mut(group) {
                                    members.retain(|m| m != origin);
                                }
                                // A group with nothing in it is noise
                                if graph
                                    .groups
                                    .get(group)
                                    .map(|m| m.is_empty())
                                    .unwrap_or(false)
                                {
                                    graph.groups.remove(group);
                                }
                                self.autosave_groups(
                                    graph,
                                    &format!("Removed {origin} from {group}"),
                                );
                            }
                            None => {}
                        }
                        let rows = manage_rows(&graph.groups);
                        self.group_cursor = self.group_cursor.min(rows.len().saturating_sub(1));
                    }
                    KeyCode::Char('s') | KeyCode::Char('S') => match &self.config_path {
                        Some(path) => match save_groups(path, &graph.groups) {
                            Ok(()) => {
                                self.status_msg = format!("Saved groups to {}", path.display())
                            }
                            Err(e) => self.status_msg = format!("Could not save: {e}"),
                        },
                        None => {
                            self.text_buffer.clear();
                            self.text_cursor = 0;
                            self.input_mode = InputMode::ConfigPath;
                        }
                    },
                    _ => {}
                }
            }

            InputMode::ConfigPath
                if edit_field(&mut self.text_buffer, &mut self.text_cursor, code, has_ctrl) => {}
            InputMode::ConfigPath => match code {
                KeyCode::Esc => self.input_mode = InputMode::GroupManage,
                KeyCode::Enter => {
                    let path = PathBuf::from(self.text_buffer.trim());
                    if self.text_buffer.trim().is_empty() {
                        self.status_msg = String::from("Give a path, or Esc to cancel");
                    } else {
                        match save_groups(&path, &graph.groups) {
                            Ok(()) => {
                                self.status_msg = format!("Saved groups to {}", path.display());
                                // Remembered so a later save goes to the
                                // same place without asking again
                                self.config_path = Some(path);
                                self.input_mode = InputMode::GroupManage;
                            }
                            Err(e) => self.status_msg = format!("Could not save: {e}"),
                        }
                    }
                }
                _ => {}
            },

            InputMode::ConfirmQuit => match code {
                KeyCode::Char('y') | KeyCode::Char('Y') => {
                    return KeyOutcome::Finish(TuiAction::QuitWithoutSaving);
                }
                KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => {
                    self.input_mode = InputMode::Normal;
                    self.status_msg = String::from("Returned to configuration");
                }
                KeyCode::Left
                | KeyCode::Right
                | KeyCode::Tab
                | KeyCode::BackTab
                | KeyCode::Up
                | KeyCode::Down => {
                    self.confirm_choice = self.confirm_choice.toggle();
                }
                KeyCode::Enter | KeyCode::Char(' ') => match self.confirm_choice {
                    ConfirmChoice::Yes => {
                        return KeyOutcome::Finish(TuiAction::QuitWithoutSaving);
                    }
                    ConfirmChoice::No => {
                        self.input_mode = InputMode::Normal;
                        self.status_msg = String::from("Returned to configuration");
                    }
                },
                _ => {}
            },

            // Control chords must not type themselves into the query
            InputMode::Search
                if edit_field(
                    &mut graph.search_query,
                    &mut self.text_cursor,
                    code,
                    has_ctrl,
                ) =>
            {
                let anchor = graph.anchor_at(selected_index);
                graph.rebuild_visible_rows();
                // Stay on the port being looked at while it still matches;
                // once it stops, start again from the top of the results
                // rather than from wherever the old row number now points.
                self.restore_anchor_or(graph, anchor, 0);
            }
            InputMode::Search => match code {
                // The list stays live while the bar is open, so results can be
                // scanned as they narrow instead of only after Enter. The
                // vertical keys are free to do it: `edit_field` claims
                // Left/Right/Home/End for the caret and passes these through.
                KeyCode::Up => {
                    self.move_selection(selected_index, if has_ctrl { -5 } else { -1 }, total_rows)
                }
                KeyCode::Down => {
                    self.move_selection(selected_index, if has_ctrl { 5 } else { 1 }, total_rows)
                }
                KeyCode::PageUp => {
                    self.move_selection(selected_index, -(page_size as isize), total_rows)
                }
                KeyCode::PageDown => {
                    self.move_selection(selected_index, page_size as isize, total_rows)
                }
                KeyCode::Esc => {
                    let anchor = graph.anchor_at(selected_index);
                    graph.search_query.clear();
                    self.text_cursor = 0;
                    graph.rebuild_visible_rows();
                    self.restore_anchor(graph, anchor);
                    self.input_mode = InputMode::Normal;
                    self.status_msg = String::from("Cleared search filter");
                }
                KeyCode::Enter => {
                    self.input_mode = InputMode::Normal;
                    self.status_msg = format!("Filter locked: '{}'", graph.search_query);
                }
                _ => {}
            },

            InputMode::Normal => {
                match code {
                    // Focus: Tab cycles List -> OK -> Cancel -> List
                    KeyCode::Tab => {
                        self.focus = next_focus(self.focus);
                    }

                    KeyCode::BackTab => {
                        self.focus = prev_focus(self.focus);
                    }

                    // The list is flat, so a relationship is a reference
                    // rather than a nested subtree: Enter follows it to
                    // that port's own entry.
                    KeyCode::Enter
                        if self.focus == Focus::List
                            && graph
                                .visible_rows
                                .get(selected_index)
                                .and_then(|r| r.kind.jump_target())
                                .is_some() =>
                    {
                        let target = graph.visible_rows[selected_index]
                            .kind
                            .jump_target()
                            .unwrap_or_default()
                            .to_string();
                        let from = enclosing_port_origin(graph, selected_index);

                        match jump_to_port(graph, &target) {
                            Some(row) => {
                                if let Some(from) = from {
                                    self.jump_stack.push(from);
                                }
                                self.list_state.select(Some(row));
                                self.status_msg = format!("Jumped to {}", target);
                            }
                            None => {
                                self.status_msg = format!("{} is not in the list", target);
                            }
                        }
                    }

                    _ if is_backspace(code, has_ctrl) && self.focus == Focus::List => {
                        match self.jump_stack.pop() {
                            Some(origin) => match jump_to_port(graph, &origin) {
                                Some(row) => {
                                    self.list_state.select(Some(row));
                                    self.status_msg = format!("Back to {}", origin);
                                }
                                None => {
                                    self.status_msg = format!("{} is no longer listed", origin);
                                }
                            },
                            None => {
                                self.status_msg = String::from("No jump to go back from");
                            }
                        }
                    }

                    // Buttons: Enter presses the focused one, defaulting
                    // to OK while the list has focus, as dialog does
                    KeyCode::Enter => match self.focus {
                        Focus::Cancel => quit_requested = true,
                        _ => save_requested = true,
                    },

                    // Letter hotkeys work from any focus
                    KeyCode::Char('o') | KeyCode::Char('O') => save_requested = true,

                    KeyCode::Char('s') | KeyCode::Char('S') => save_requested = true,

                    KeyCode::Char('c') | KeyCode::Char('C') => quit_requested = true,

                    KeyCode::Char('q') | KeyCode::Esc => quit_requested = true,

                    KeyCode::Char('/') => {
                        // The query survives between visits, so pick up after it
                        self.text_cursor = graph.search_query.chars().count();
                        self.input_mode = InputMode::Search;
                    }

                    KeyCode::Char('f') | KeyCode::Char('F') if has_ctrl => {
                        self.text_cursor = graph.search_query.chars().count();
                        self.input_mode = InputMode::Search;
                    }

                    // Ctrl+G adds the port under the cursor to a group;
                    // a second consecutive press opens the manager. A
                    // terminal cannot tell Ctrl+Shift+G apart from
                    // Ctrl+G, so scope escalates by repetition, exactly
                    // as +/_ do.
                    KeyCode::Char('g') | KeyCode::Char('G') if has_ctrl => {
                        match enclosing_port_origin(graph, selected_index) {
                            Some(origin) => {
                                self.group_target = Some(origin);
                                self.group_cursor = 0;
                                self.input_mode = InputMode::GroupAssign;
                            }
                            None => {
                                self.status_msg = String::from("Put the cursor on a port first")
                            }
                        }
                    }

                    // View: cycle the cursor row middle -> top -> bottom
                    // and repaint the screen
                    KeyCode::Char('l') | KeyCode::Char('L') if has_ctrl => {
                        let pos = match self.recenter_pos {
                            Some(prev) => prev.next(),
                            None => RecenterPosition::Middle,
                        };
                        self.recenter_pos = Some(pos);
                        *self.list_state.offset_mut() =
                            recenter_offset(selected_index, self.viewport_height, total_rows, pos);
                        redraw = true;
                        self.status_msg = format!("Recentered row to {}", pos.label());
                    }

                    // Button row: Left/Right move between buttons
                    KeyCode::Left if self.focus != Focus::List => {
                        self.focus = Focus::Ok;
                    }

                    KeyCode::Right if self.focus != Focus::List => {
                        self.focus = Focus::Cancel;
                    }

                    KeyCode::Char(' ') if self.focus != Focus::List => match self.focus {
                        Focus::Cancel => quit_requested = true,
                        _ => save_requested = true,
                    },

                    // Everything below acts on the tree, so it only
                    // applies while the list holds focus
                    _ if self.focus != Focus::List => {}

                    // Navigation: Siblings (Shift + Up/Down)
                    KeyCode::Down if has_shift => {
                        let depths: Vec<usize> =
                            graph.visible_rows.iter().map(|r| r.depth).collect();
                        let i = next_sibling_index(&depths, selected_index);
                        self.list_state.select(Some(i));
                        self.status_msg = String::from("Jumped to next sibling");
                    }

                    KeyCode::Up if has_shift => {
                        let depths: Vec<usize> =
                            graph.visible_rows.iter().map(|r| r.depth).collect();
                        let i = prev_sibling_index(&depths, selected_index);
                        self.list_state.select(Some(i));
                        self.status_msg = String::from("Jumped to previous sibling");
                    }

                    // Navigation: Top & Bottom
                    KeyCode::Home | KeyCode::Char('g') if has_ctrl || code == KeyCode::Home => {
                        self.list_state.select(Some(0));
                        self.status_msg = String::from("Jumped to top");
                    }

                    KeyCode::End | KeyCode::Char('G') if has_ctrl || code == KeyCode::End => {
                        let last = total_rows.saturating_sub(1);
                        self.list_state.select(Some(last));
                        self.status_msg = String::from("Jumped to bottom");
                    }

                    // Navigation: Single Up / Down, five at a time with Ctrl
                    KeyCode::Up => {
                        self.move_selection(
                            selected_index,
                            if has_ctrl { -5 } else { -1 },
                            total_rows,
                        );
                    }

                    KeyCode::Down => {
                        self.move_selection(
                            selected_index,
                            if has_ctrl { 5 } else { 1 },
                            total_rows,
                        );
                    }

                    // Navigation: Page Up / Page Down
                    KeyCode::PageUp => {
                        self.move_selection(selected_index, -(page_size as isize), total_rows);
                        self.status_msg = format!("Page Up (-{} rows)", page_size);
                    }

                    KeyCode::PageDown => {
                        self.move_selection(selected_index, page_size as isize, total_rows);
                        self.status_msg = format!("Page Down (+{} rows)", page_size);
                    }

                    // Tree Operations: = / - act on the row under the
                    // cursor, + / _ on its section, and pressing + or _
                    // twice in a row escalates to the whole tree
                    KeyCode::Char(c) if tree_key == Some(c) => {
                        if let Some(op) = tree_op(c, repeat) {
                            let selected = self.list_state.selected().unwrap_or(0);
                            // Taken before the rebuild, restored after it
                            let anchor = graph.anchor_at(selected);
                            let what = row_label(graph, selected);

                            self.status_msg = match op {
                                TreeOp::ExpandNode => {
                                    graph.expand_node(selected);
                                    format!("Expanded {what}")
                                }
                                TreeOp::CollapseNode => {
                                    graph.collapse_node(selected);
                                    format!("Collapsed {what}")
                                }
                                TreeOp::ExpandSection => {
                                    graph.expand_subtree(selected);
                                    format!("Expanded everything under {what}")
                                }
                                TreeOp::CollapseSection => {
                                    graph.collapse_subtree(selected);
                                    format!("Collapsed everything under {what}")
                                }
                                TreeOp::ExpandAll => {
                                    graph.expand_all();
                                    String::from("Expanded the whole list")
                                }
                                TreeOp::CollapseAll => {
                                    graph.collapse_all();
                                    String::from("Collapsed the whole list")
                                }
                            };
                            self.restore_anchor(graph, anchor);
                        }
                    }

                    KeyCode::Char(' ') => {
                        if let Some(selected) = self.list_state.selected() {
                            if let Some(row) = graph.visible_rows.get(selected) {
                                match &row.kind {
                                    RowKind::Option { name, .. } => {
                                        let opt_name = name.clone();
                                        // A toggle can strand whole ports out of
                                        // the list, so it renumbers rows just as
                                        // an expand does
                                        let anchor = graph.anchor_at(selected);
                                        graph.toggle_option(selected);
                                        self.restore_anchor(graph, anchor);
                                        self.status_msg = format!("Toggled option '{}'", opt_name);
                                    }
                                    _ => {
                                        self.status_msg =
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
            return KeyOutcome::Finish(TuiAction::SaveAndQuit);
        }

        if quit_requested {
            if graph.is_dirty() {
                self.input_mode = InputMode::ConfirmQuit;
                self.confirm_choice = ConfirmChoice::Yes;
                self.status_msg = String::from("Unsaved changes");
            } else {
                return KeyOutcome::Finish(TuiAction::QuitWithoutSaving);
            }
        }

        if redraw {
            KeyOutcome::Redraw
        } else {
            KeyOutcome::Continue
        }
    }
}

/// Draws one frame.
///
/// Takes `&mut App` because the list widget owns the scroll offset and updates
/// it as it draws, and because the number of rows the list can show is only
/// known once the layout has been split.
fn render(f: &mut Frame, graph: &DependencyGraph, app: &mut App) {
    let total_rows = graph.visible_rows.len();
    let selected_index = app.list_state.selected().unwrap_or(0);

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),
            Constraint::Min(5),
            // Three key rows plus the border above and below
            Constraint::Length(5),
            Constraint::Length(3),
        ])
        .split(f.size());

    // Rows the tree list can actually show, minus its top/bottom border.
    // Recorded for the paging and recentre keys, which run before the next
    // frame and cannot work it out for themselves.
    app.viewport_height = chunks[1].height.saturating_sub(2) as usize;

    // Target on the left, most recent action right-aligned, which keeps
    // the footer free for keybindings alone
    // A locked filter leaves the list narrowed with the search bar gone, so the
    // header has to say so; otherwise the missing ports look like a fault.
    let target = if graph.search_query.is_empty() {
        format!(
            " Target: {}  ({} ports)",
            graph.root_origin,
            graph.live_count()
        )
    } else {
        format!(
            " Target: {}  ({} of {} ports matching \"{}\")",
            graph.root_origin,
            graph.listed_port_count(),
            graph.live_count(),
            graph.search_query
        )
    };
    let status = format!("{} ", app.status_msg);
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

            // Colour carries provenance: whether you asked for this port
            // or something else dragged it in.
            if let RowKind::Port {
                origin,
                provenance,
                resolved,
            } = &row.kind
            {
                let prefix = if row.has_children {
                    if row.is_expanded {
                        "[-] "
                    } else {
                        "[+] "
                    }
                } else {
                    "    "
                };

                let name_style = match provenance {
                    Provenance::Requested => Style::default().fg(Color::White),
                    Provenance::Dependency => Style::default().fg(Color::Yellow),
                };

                let mut spans = vec![
                    Span::raw(format!("{}{}", indent, prefix)),
                    Span::styled(origin.clone(), name_style),
                ];
                // Said on the port row as well as inside it, because a port
                // whose options are unknown looks exactly like one with none
                // until it is opened — and the row is what you scroll past.
                if !resolved {
                    spans.push(Span::styled(
                        "  [unevaluated]",
                        Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
                    ));
                }
                return ListItem::new(Line::from(spans));
            }

            // `required by` marks conditional parents so it is clear at
            // a glance which of them would stop needing this port if an
            // option were turned off.
            if let RowKind::RequiredByEntry { origin, via_option } = &row.kind {
                let (text, style) = match via_option {
                    Some(opt) => (
                        format!("{}    {}  (via {})", indent, origin, opt),
                        Style::default().fg(Color::Yellow),
                    ),
                    None => (
                        format!("{}    {}", indent, origin),
                        Style::default().fg(Color::Gray),
                    ),
                };
                return ListItem::new(Line::from(Span::styled(text, style)));
            }

            let line = match &row.kind {
                RowKind::Port { .. } | RowKind::RequiredByEntry { .. } => unreachable!(),
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
                        format!("{}{}{} {}{}", indent, prefix, control, category_badge, name)
                    } else {
                        format!(
                            "{}{}{} {}{} - {}",
                            indent, prefix, control, category_badge, name, description
                        )
                    }
                }
                RowKind::DependsOn {
                    origin,
                    active: false,
                } => {
                    // Nothing pulls it in right now, so it is not in the
                    // list; turning this option on is what would add it
                    format!("{}    {}  (not pulled in)", indent, origin)
                }
                RowKind::DependsOn { origin, .. } | RowKind::RequiresEntry { origin } => {
                    format!("{}    {}", indent, origin)
                }
                RowKind::SectionHeader { kind, count } => {
                    let prefix = if row.is_expanded { "[-] " } else { "[+] " };
                    let label = match kind {
                        SectionKind::Requires => "requires",
                        SectionKind::RequiredBy => "required by",
                    };
                    format!(
                        "{}{}--- {} {} port{} ---",
                        indent,
                        prefix,
                        label,
                        count,
                        if *count == 1 { "" } else { "s" }
                    )
                }
                RowKind::Info { message } => {
                    format!("{}* {}", indent, message)
                }
            };
            ListItem::new(line)
        })
        .collect();

    // The cursor row is only highlighted while the list holds app.focus, so
    // the focused button is never ambiguous
    let highlight_style = if app.focus == Focus::List {
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

    f.render_stateful_widget(tree_list, chunks[1], &mut app.list_state);

    // Render Right-Hand Scrollbar
    let mut scrollbar_state =
        ScrollbarState::new(total_rows.saturating_sub(1)).position(selected_index);

    let scrollbar = Scrollbar::default()
        .orientation(ScrollbarOrientation::VerticalRight)
        .begin_symbol(Some("?"))
        .end_symbol(Some("?"));

    f.render_stateful_widget(scrollbar, chunks[1], &mut scrollbar_state);

    // Footer / Status Bar Rendering
    let primary = match app.input_mode {
        InputMode::Search => {
            let style = Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD);
            // Named for what it matches. The bare word "SEARCH" invited the
            // assumption that option names and descriptions were in scope too.
            let mut spans = vec![Span::styled(" PORT SEARCH: ", style)];
            spans.extend(field_spans(&graph.search_query, app.text_cursor, style));
            spans.push(Span::styled(
                " ([Up]/[Down] browse, [Enter] locks, [Esc] clears)".to_string(),
                style,
            ));
            Line::from(spans)
        }
        _ => Line::styled(
            " =/- open/close row | +/_ open/close branch | ++/__ open/close list | / search",
            Style::default().fg(Color::DarkGray),
        ),
    };

    let action_rows = FOOTER_ACTION_KEYS
        .lines()
        .map(|row| Line::styled(row.to_string(), Style::default().fg(Color::DarkGray)));

    let mut footer_lines = vec![primary];
    footer_lines.extend(action_rows);
    let footer =
        Paragraph::new(Text::from(footer_lines)).block(Block::default().borders(Borders::ALL));

    f.render_widget(footer, chunks[2]);

    let buttons = Paragraph::new(button_row(&[
        ("OK", app.focus == Focus::Ok),
        ("Cancel", app.focus == Focus::Cancel),
    ]))
    .alignment(Alignment::Center)
    .block(Block::default().borders(Borders::ALL));

    f.render_widget(buttons, chunks[3]);

    match app.input_mode {
        InputMode::ConfirmQuit => render_confirm_quit(f, app.confirm_choice),
        InputMode::GroupAssign => render_group_assign(
            f,
            app.group_target.as_deref().unwrap_or(""),
            &graph.groups,
            app.group_cursor,
        ),
        InputMode::GroupNewName => render_prompt(
            f,
            "New group",
            &format!(
                "Name a group for {}:",
                app.group_target.as_deref().unwrap_or("")
            ),
            &app.text_buffer,
            app.text_cursor,
        ),
        InputMode::GroupManage => render_group_manage(
            f,
            &graph.groups,
            app.group_cursor,
            app.config_path.as_deref(),
        ),
        InputMode::ConfigPath => render_prompt(
            f,
            "Save groups",
            "No config file yet. Where should groups be saved?",
            &app.text_buffer,
            app.text_cursor,
        ),
        InputMode::Normal | InputMode::Search => {}
    }
}

pub fn run_tui(graph: &mut DependencyGraph, config_path: Option<PathBuf>) -> Result<TuiAction> {
    enable_raw_mode()?;
    let mut stdout = stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    // The alternate screen keeps whatever was last on it, and the first draw
    // only paints cells that differ from ratatui's own buffer — which starts
    // empty, so anything already on screen matches "empty" and is left alone.
    // Without this the previous contents show through until something forces a
    // full repaint, which is why starting up used to need a manual Ctrl + L.
    terminal.clear()?;

    let mut app = App::new(config_path);
    let action;

    loop {
        terminal.draw(|f| render(f, graph, &mut app))?;

        if event::poll(std::time::Duration::from_millis(50))? {
            if let Event::Key(key) = event::read()? {
                match app.on_key(key, graph) {
                    KeyOutcome::Continue => {}
                    KeyOutcome::Redraw => terminal.clear()?,
                    KeyOutcome::Finish(finished) => {
                        action = finished;
                        break;
                    }
                }
            }
        }
    }

    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
    terminal.show_cursor()?;

    // Leaving the alternate screen drops the cursor back where the shell left
    // it, which is part-way along a line if the last thing printed there did
    // not end in one. Whatever comes next — this program's own output, or the
    // prompt — then starts mid-line and reads as though the terminal is still
    // waiting for input. One newline, flushed before anything else is written,
    // is what makes exiting land cleanly.
    let mut out = std::io::stdout();
    writeln!(out)?;
    out.flush()?;

    Ok(action)
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::{backend::TestBackend, Terminal};

    use crate::graph::NodeId;
    use crossterm::event::{KeyEventKind, KeyEventState};
    use rusqlite::Connection;

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent {
            code,
            modifiers: KeyModifiers::NONE,
            kind: KeyEventKind::Press,
            state: KeyEventState::NONE,
        }
    }

    fn ctrl(c: char) -> KeyEvent {
        KeyEvent {
            code: KeyCode::Char(c),
            modifiers: KeyModifiers::CONTROL,
            kind: KeyEventKind::Press,
            state: KeyEventState::NONE,
        }
    }

    /// Writes a port with one option, in the shape `bgone index` would.
    ///
    /// The cache is normally filled by evaluating ports with make; these tests
    /// care about key handling, not resolution, so they write the rows directly.
    fn fixture_option(
        conn: &Connection,
        origin: &str,
        name: &str,
        default_on: bool,
        group_type: &str,
        group_name: &str,
    ) -> i64 {
        let pkgname = format!("{}-1.0", origin.split('/').nth(1).unwrap_or(origin));
        conn.execute(
            "INSERT OR IGNORE INTO ports (origin, pkgbase, pkgname, resolved) VALUES (?1, ?2, ?3, 1)",
            rusqlite::params![origin, origin.split('/').nth(1).unwrap_or(origin), pkgname],
        )
        .unwrap();
        let port_id: i64 = conn
            .query_row(
                "SELECT id FROM ports WHERE origin = ?1",
                rusqlite::params![origin],
                |r| r.get(0),
            )
            .unwrap();
        conn.execute(
            "INSERT OR REPLACE INTO options (port_id, name, description, group_type, group_name, default_on)
             VALUES (?1, ?2, '', ?3, ?4, ?5)",
            rusqlite::params![port_id, name, group_type, group_name, default_on as i32],
        )
        .unwrap();
        conn.last_insert_rowid()
    }

    fn fixture_edge(conn: &Connection, from: &str, to: &str, via: Option<i64>) {
        for origin in [from, to] {
            let pkgname = format!("{}-1.0", origin.split('/').nth(1).unwrap_or(origin));
            conn.execute(
                "INSERT OR IGNORE INTO ports (origin, pkgbase, pkgname, resolved) VALUES (?1, ?2, ?3, 1)",
                rusqlite::params![origin, origin.split('/').nth(1).unwrap_or(origin), pkgname],
            )
            .unwrap();
        }
        conn.execute(
            "INSERT INTO dep_edge (from_port_id, to_port_id, class, via_option_id, polarity)
             SELECT f.id, t.id, 'RUN', ?3, 'ON' FROM ports f, ports t
             WHERE f.origin = ?1 AND t.origin = ?2",
            rusqlite::params![from, to, via],
        )
        .unwrap();
    }

    /// Two ports sharing an option, so the list has something to put a cursor on.
    fn test_graph() -> DependencyGraph {
        let conn = Connection::open_in_memory().unwrap();
        crate::db::init_db(&conn, true).unwrap();
        for origin in ["lang/php83-extensions", "lang/php84-extensions"] {
            fixture_option(&conn, origin, "SOAP", false, "DEFINE", "");
        }
        let targets = vec![
            "lang/php83-extensions".to_string(),
            "lang/php84-extensions".to_string(),
        ];
        let mut graph = DependencyGraph::load_from_db(
            &conn,
            &targets,
            &crate::reader::SystemOptions::default(),
            false,
        )
        .unwrap();
        graph.expand_all();
        graph
    }

    /// Documented in --help, the README and the footer: Ctrl+G once to add,
    /// twice to manage.
    #[test]
    fn ctrl_g_twice_opens_the_group_manager() {
        let mut graph = test_graph();
        let mut app = App::new(None);

        assert_eq!(app.on_key(ctrl('g'), &mut graph), KeyOutcome::Continue);
        assert_eq!(app.input_mode, InputMode::GroupAssign);

        app.on_key(ctrl('g'), &mut graph);
        assert_eq!(
            app.input_mode,
            InputMode::GroupManage,
            "a second Ctrl+G must escalate to the manager"
        );
    }

    /// Row index of the first option row, for putting the cursor inside a
    /// port's block rather than on its header.
    fn first_option_row(graph: &DependencyGraph) -> usize {
        graph
            .visible_rows
            .iter()
            .position(|r| matches!(r.node_id, NodeId::Option(_)))
            .expect("no option row")
    }

    /// Ctrl+G works from anywhere inside a port's block, not just its header.
    #[test]
    fn ctrl_g_targets_the_port_the_cursor_is_inside() {
        let mut graph = test_graph();
        let mut app = App::new(None);
        app.list_state.select(Some(first_option_row(&graph)));

        app.on_key(ctrl('g'), &mut graph);

        assert_eq!(app.input_mode, InputMode::GroupAssign);
        assert_eq!(
            app.group_target.as_deref(),
            Some("lang/php83-extensions"),
            "an option row should target the port it belongs to"
        );
    }

    /// Escaping out breaks the run, so the next Ctrl+G starts over rather than
    /// jumping straight to the manager.
    #[test]
    fn escaping_assign_starts_the_next_ctrl_g_over() {
        let mut graph = test_graph();
        let mut app = App::new(None);

        app.on_key(ctrl('g'), &mut graph);
        app.on_key(key(KeyCode::Esc), &mut graph);
        assert_eq!(app.input_mode, InputMode::Normal);
        assert!(app.group_target.is_none(), "the target must be dropped");

        app.on_key(ctrl('g'), &mut graph);
        assert_eq!(app.input_mode, InputMode::GroupAssign);
    }

    // ---------------------------------------------------------------- assign

    #[test]
    fn assign_adds_the_port_to_the_chosen_group_without_duplicating() {
        let mut graph = test_graph();
        graph
            .groups
            .insert("ssl".into(), vec!["security/openssl".into()]);
        let mut app = App::new(None);

        app.on_key(ctrl('g'), &mut graph);
        app.on_key(key(KeyCode::Enter), &mut graph);

        assert_eq!(app.input_mode, InputMode::Normal);
        assert_eq!(
            graph.groups["ssl"],
            vec!["lang/php83-extensions", "security/openssl"],
            "members are kept sorted"
        );

        // Adding it again changes nothing
        app.on_key(ctrl('g'), &mut graph);
        app.on_key(key(KeyCode::Enter), &mut graph);
        assert_eq!(graph.groups["ssl"].len(), 2);
        assert!(app.status_msg.contains("already"), "{}", app.status_msg);
    }

    #[test]
    fn assign_cursor_is_clamped_at_both_ends() {
        let mut graph = test_graph();
        graph.groups.insert("a".into(), vec!["www/x".into()]);
        let mut app = App::new(None);
        app.on_key(ctrl('g'), &mut graph);

        app.on_key(key(KeyCode::Up), &mut graph);
        assert_eq!(app.group_cursor, 0, "cannot go above the first group");

        // one group plus the "<new group...>" entry
        for _ in 0..5 {
            app.on_key(key(KeyCode::Down), &mut graph);
        }
        assert_eq!(app.group_cursor, 1, "cannot go past <new group...>");
    }

    #[test]
    fn choosing_new_group_moves_to_naming() {
        let mut graph = test_graph();
        let mut app = App::new(None);

        app.on_key(ctrl('g'), &mut graph);
        // With no groups yet, the only entry is <new group...>
        app.on_key(key(KeyCode::Enter), &mut graph);

        assert_eq!(app.input_mode, InputMode::GroupNewName);
        assert!(app.text_buffer.is_empty());
    }

    // ---------------------------------------------------------------- naming

    #[test]
    fn naming_a_group_creates_it_with_the_port_in_it() {
        let mut graph = test_graph();
        let mut app = App::new(None);
        app.on_key(ctrl('g'), &mut graph);
        app.on_key(key(KeyCode::Enter), &mut graph);

        for c in "php-ext".chars() {
            app.on_key(key(KeyCode::Char(c)), &mut graph);
        }
        app.on_key(key(KeyCode::Backspace), &mut graph);
        assert_eq!(app.text_buffer, "php-ex");

        app.on_key(key(KeyCode::Enter), &mut graph);

        assert_eq!(app.input_mode, InputMode::Normal);
        assert_eq!(graph.groups["php-ex"], vec!["lang/php83-extensions"]);
    }

    /// Terminals that send BS (0x08) rather than DEL (0x7F) arrive as Ctrl+H,
    /// which the text prompts used to swallow along with every other control
    /// chord — backspace appeared to do nothing at all on those terminals.
    #[test]
    fn ctrl_h_rubs_out_like_backspace_does() {
        for erase in [key(KeyCode::Backspace), ctrl('h')] {
            let mut graph = test_graph();
            let mut app = App::new(None);
            app.on_key(ctrl('g'), &mut graph);
            app.on_key(key(KeyCode::Enter), &mut graph);

            for c in "phpp".chars() {
                app.on_key(key(KeyCode::Char(c)), &mut graph);
            }
            app.on_key(erase, &mut graph);

            assert_eq!(app.text_buffer, "php", "{erase:?} did not rub out");
        }
    }

    /// The same terminals, the same problem, in the other two places text is
    /// typed.
    #[test]
    fn ctrl_h_rubs_out_in_search_and_in_the_path_prompt() {
        let mut graph = test_graph();
        let mut app = App::new(None);

        app.on_key(key(KeyCode::Char('/')), &mut graph);
        for c in "soapp".chars() {
            app.on_key(key(KeyCode::Char(c)), &mut graph);
        }
        app.on_key(ctrl('h'), &mut graph);
        assert_eq!(graph.search_query, "soap");

        let mut graph = test_graph();
        graph
            .groups
            .insert("php".into(), vec!["lang/php83-extensions".into()]);
        let mut app = app_in_manager(&mut graph);
        app.on_key(key(KeyCode::Char('s')), &mut graph);
        for c in "/tmp/xx".chars() {
            app.on_key(key(KeyCode::Char(c)), &mut graph);
        }
        app.on_key(ctrl('h'), &mut graph);
        assert_eq!(app.text_buffer, "/tmp/x");
    }

    // ------------------------------------------------------- keeping my spot

    /// A graph with two ports, so collapsing the first renumbers everything
    /// under the second.
    fn spot_graph() -> DependencyGraph {
        let conn = Connection::open_in_memory().unwrap();
        crate::db::init_db(&conn, true).unwrap();
        for origin in ["www/aaa", "www/zzz"] {
            for opt in ["ALPHA", "BETA", "GAMMA"] {
                fixture_option(&conn, origin, opt, false, "DEFINE", "");
            }
        }
        let targets = vec!["www/aaa".to_string(), "www/zzz".to_string()];
        let mut graph = DependencyGraph::load_from_db(
            &conn,
            &targets,
            &crate::reader::SystemOptions::default(),
            false,
        )
        .unwrap();
        graph.expand_all();
        graph
    }

    /// What the cursor is sitting on, for asserting it did not wander.
    fn under_cursor(graph: &DependencyGraph, app: &App) -> String {
        let i = app.list_state.selected().unwrap();
        match &graph.visible_rows[i].kind {
            RowKind::Port { origin, .. } => format!("port {origin}"),
            RowKind::Option { name, .. } => format!("option {name}"),
            other => format!("{other:?}"),
        }
    }

    fn row_of_option(graph: &DependencyGraph, origin: &str, opt: &str) -> usize {
        graph
            .visible_rows
            .iter()
            .position(|r| match r.node_id {
                NodeId::Option(id) => {
                    let o = &graph.option_nodes[id];
                    o.port_origin == origin && o.name == opt
                }
                _ => false,
            })
            .unwrap_or_else(|| panic!("no row for {origin}/{opt}"))
    }

    fn row_of_port(graph: &DependencyGraph, origin: &str) -> usize {
        graph
            .visible_rows
            .iter()
            .position(|r| matches!(&r.kind, RowKind::Port { origin: o, .. } if o == origin))
            .unwrap_or_else(|| panic!("no row for {origin}"))
    }

    /// Expanding the whole list inserts rows *above* the cursor as well as
    /// below, so its row number changes even though nothing about the row it is
    /// on did. It has to follow the row.
    #[test]
    fn expanding_everything_keeps_the_cursor_on_the_same_port() {
        let mut graph = spot_graph();
        let mut app = App::new(None);

        // Collapse everything, then sit on the second port
        app.on_key(key(KeyCode::Char('_')), &mut graph);
        app.on_key(key(KeyCode::Char('_')), &mut graph);
        app.list_state.select(Some(row_of_port(&graph, "www/zzz")));
        let before = app.list_state.selected().unwrap();

        // Expanding puts www/aaa's three options in ahead of it
        app.on_key(key(KeyCode::Char('+')), &mut graph);
        app.on_key(key(KeyCode::Char('+')), &mut graph);

        let after = app.list_state.selected().unwrap();
        assert_ne!(before, after, "the row number must have moved");
        assert_eq!(
            under_cursor(&graph, &app),
            "port www/zzz",
            "holding the row number would have landed on one of www/aaa's options"
        );
    }

    /// The case that matters most: collapsing the port the cursor is inside
    /// hides the option, so the cursor falls out to the port itself.
    #[test]
    fn collapsing_the_port_im_inside_takes_me_to_the_port() {
        let mut graph = spot_graph();
        let mut app = App::new(None);

        // Deliberately the *first* port: after the collapse only two rows are
        // left, so simply clamping the old row number would land on www/zzz and
        // look correct by accident.
        app.list_state
            .select(Some(row_of_option(&graph, "www/aaa", "BETA")));
        assert_eq!(under_cursor(&graph, &app), "option BETA");

        // Collapse the whole list; every option row goes
        app.on_key(key(KeyCode::Char('_')), &mut graph);
        app.on_key(key(KeyCode::Char('_')), &mut graph);

        assert_eq!(
            under_cursor(&graph, &app),
            "port www/aaa",
            "should have fallen out to the port the option belonged to"
        );
    }

    /// Expanding again should leave the cursor where it is rather than jumping.
    #[test]
    fn expanding_keeps_the_cursor_on_the_port() {
        let mut graph = spot_graph();
        let mut app = App::new(None);

        app.on_key(key(KeyCode::Char('_')), &mut graph);
        app.on_key(key(KeyCode::Char('_')), &mut graph);

        let zzz = graph
            .visible_rows
            .iter()
            .position(|r| matches!(&r.kind, RowKind::Port { origin, .. } if origin == "www/zzz"))
            .unwrap();
        app.list_state.select(Some(zzz));

        app.on_key(key(KeyCode::Char('=')), &mut graph);
        assert_eq!(under_cursor(&graph, &app), "port www/zzz");

        app.on_key(key(KeyCode::Char('+')), &mut graph);
        assert_eq!(under_cursor(&graph, &app), "port www/zzz");
    }

    /// Toggling rebuilds the list too, and can strand whole ports out of it, so
    /// it renumbers rows exactly as an expand does.
    #[test]
    fn toggling_leaves_the_cursor_on_the_option_it_toggled() {
        let conn = Connection::open_in_memory().unwrap();
        crate::db::init_db(&conn, true).unwrap();
        // EXTRA is on by default and pulls in aaa/pulled, which sorts *above*
        // www/app — so turning it off shifts every row under www/app upwards.
        let extra = fixture_option(&conn, "www/app", "EXTRA", true, "DEFINE", "");
        fixture_option(&conn, "aaa/pulled", "DOCS", true, "DEFINE", "");
        fixture_edge(&conn, "www/app", "aaa/pulled", Some(extra));

        let mut graph = DependencyGraph::load_from_db(
            &conn,
            &["www/app".to_string()],
            &crate::reader::SystemOptions::default(),
            false,
        )
        .unwrap();
        graph.expand_all();
        let mut app = App::new(None);

        let before = row_of_option(&graph, "www/app", "EXTRA");
        app.list_state.select(Some(before));
        assert!(graph.is_live("aaa/pulled"));

        app.on_key(key(KeyCode::Char(' ')), &mut graph);

        assert!(!graph.is_live("aaa/pulled"), "the toggle stranded it");
        assert_ne!(
            app.list_state.selected().unwrap(),
            before,
            "the row number must have moved"
        );
        assert_eq!(
            under_cursor(&graph, &app),
            "option EXTRA",
            "the cursor should still be on the option that was toggled"
        );
    }

    /// The status line names what was acted on. A row number would be doubly
    /// useless here, since the operation changes it.
    #[test]
    fn the_status_line_names_what_was_expanded() {
        let mut graph = spot_graph();
        let mut app = App::new(None);

        let zzz = graph
            .visible_rows
            .iter()
            .position(|r| matches!(&r.kind, RowKind::Port { origin, .. } if origin == "www/zzz"))
            .unwrap();
        app.list_state.select(Some(zzz));
        app.on_key(key(KeyCode::Char('-')), &mut graph);

        assert!(
            app.status_msg.contains("www/zzz"),
            "got: {}",
            app.status_msg
        );
    }

    // -------------------------------------------------------- field editing

    /// Left and Right move the caret, and typing lands where it is rather than
    /// always at the end.
    #[test]
    fn arrows_move_the_caret_and_typing_lands_there() {
        let mut graph = test_graph();
        let mut app = App::new(None);
        app.on_key(ctrl('g'), &mut graph);
        app.on_key(key(KeyCode::Enter), &mut graph);

        for c in "php-ext".chars() {
            app.on_key(key(KeyCode::Char(c)), &mut graph);
        }
        assert_eq!(app.text_cursor, 7, "caret follows what was typed");

        for _ in 0..3 {
            app.on_key(key(KeyCode::Left), &mut graph);
        }
        assert_eq!(app.text_cursor, 4);

        app.on_key(key(KeyCode::Char('X')), &mut graph);
        assert_eq!(app.text_buffer, "php-Xext");
        assert_eq!(app.text_cursor, 5, "caret sits after what was inserted");

        app.on_key(key(KeyCode::Right), &mut graph);
        app.on_key(key(KeyCode::Char('Y')), &mut graph);
        assert_eq!(app.text_buffer, "php-XeYxt");
    }

    /// Backspace takes the character behind the caret, not the last one typed.
    #[test]
    fn backspace_rubs_out_behind_the_caret() {
        let mut graph = test_graph();
        let mut app = App::new(None);
        app.on_key(ctrl('g'), &mut graph);
        app.on_key(key(KeyCode::Enter), &mut graph);

        for c in "phpx-ext".chars() {
            app.on_key(key(KeyCode::Char(c)), &mut graph);
        }
        for _ in 0..4 {
            app.on_key(key(KeyCode::Left), &mut graph);
        }
        app.on_key(key(KeyCode::Backspace), &mut graph);

        assert_eq!(app.text_buffer, "php-ext");
        assert_eq!(app.text_cursor, 3);

        // Delete takes the one in front instead
        app.on_key(key(KeyCode::Delete), &mut graph);
        assert_eq!(app.text_buffer, "phpext");
        assert_eq!(app.text_cursor, 3, "the caret does not move");
    }

    #[test]
    fn the_caret_is_clamped_and_home_end_reach_the_edges() {
        let mut graph = test_graph();
        let mut app = App::new(None);
        app.on_key(ctrl('g'), &mut graph);
        app.on_key(key(KeyCode::Enter), &mut graph);
        for c in "abc".chars() {
            app.on_key(key(KeyCode::Char(c)), &mut graph);
        }

        for _ in 0..9 {
            app.on_key(key(KeyCode::Left), &mut graph);
        }
        assert_eq!(app.text_cursor, 0);
        app.on_key(key(KeyCode::Backspace), &mut graph);
        assert_eq!(app.text_buffer, "abc", "nothing behind the caret to take");

        for _ in 0..9 {
            app.on_key(key(KeyCode::Right), &mut graph);
        }
        assert_eq!(app.text_cursor, 3);
        app.on_key(key(KeyCode::Delete), &mut graph);
        assert_eq!(app.text_buffer, "abc", "nothing in front of it either");

        app.on_key(key(KeyCode::Home), &mut graph);
        assert_eq!(app.text_cursor, 0);
        app.on_key(key(KeyCode::End), &mut graph);
        assert_eq!(app.text_cursor, 3);
    }

    /// Paths and origins can carry multibyte characters, and editing by byte
    /// offset would split one and panic.
    #[test]
    fn editing_is_safe_across_multibyte_characters() {
        let mut graph = test_graph();
        let mut app = App::new(None);
        app.on_key(ctrl('g'), &mut graph);
        app.on_key(key(KeyCode::Enter), &mut graph);

        for c in "grüße".chars() {
            app.on_key(key(KeyCode::Char(c)), &mut graph);
        }
        assert_eq!(app.text_cursor, 5);

        app.on_key(key(KeyCode::Left), &mut graph);
        app.on_key(key(KeyCode::Backspace), &mut graph);
        assert_eq!(app.text_buffer, "grüe");

        app.on_key(key(KeyCode::Home), &mut graph);
        app.on_key(key(KeyCode::Delete), &mut graph);
        assert_eq!(app.text_buffer, "rüe");
    }

    /// The search bar is a field too, and keeps its query between visits, so the
    /// caret has to pick up after it rather than at zero.
    #[test]
    fn the_search_bar_edits_like_the_prompts_do() {
        let mut graph = test_graph();
        let mut app = App::new(None);

        app.on_key(key(KeyCode::Char('/')), &mut graph);
        for c in "soap".chars() {
            app.on_key(key(KeyCode::Char(c)), &mut graph);
        }
        app.on_key(key(KeyCode::Left), &mut graph);
        app.on_key(key(KeyCode::Char('X')), &mut graph);
        assert_eq!(graph.search_query, "soaXp");

        app.on_key(key(KeyCode::Enter), &mut graph);
        assert_eq!(app.input_mode, InputMode::Normal);

        app.on_key(key(KeyCode::Char('/')), &mut graph);
        assert_eq!(
            app.text_cursor, 5,
            "re-entering search should put the caret after the existing query"
        );
    }

    // ---------------------------------------------------------------- search

    /// Narrowing the results should leave the cursor at the top of them, not on
    /// whatever the old row number now points at.
    #[test]
    fn narrowing_the_search_moves_the_cursor_into_the_results() {
        let mut graph = spot_graph();
        let mut app = App::new(None);

        // Sit well down the list, inside www/zzz
        app.list_state
            .select(Some(row_of_option(&graph, "www/zzz", "GAMMA")));

        app.on_key(key(KeyCode::Char('/')), &mut graph);
        for c in "aaa".chars() {
            app.on_key(key(KeyCode::Char(c)), &mut graph);
        }

        assert_eq!(graph.listed_port_count(), 1, "only www/aaa matches");
        let selected = app.list_state.selected().unwrap();
        assert!(
            selected < graph.visible_rows.len(),
            "cursor left outside the list"
        );
        assert_eq!(
            under_cursor(&graph, &app),
            "port www/aaa",
            "should be at the top of the results"
        );
    }

    /// While the port under the cursor still matches, the cursor stays on it.
    #[test]
    fn the_cursor_stays_put_while_its_port_still_matches() {
        let mut graph = spot_graph();
        let mut app = App::new(None);

        app.list_state
            .select(Some(row_of_option(&graph, "www/zzz", "BETA")));
        app.on_key(key(KeyCode::Char('/')), &mut graph);
        for c in "zzz".chars() {
            app.on_key(key(KeyCode::Char(c)), &mut graph);
        }

        assert_eq!(
            under_cursor(&graph, &app),
            "option BETA",
            "www/zzz still matches, so nothing should have moved"
        );
    }

    /// A query matching nothing must not leave the cursor pointing past the end.
    #[test]
    fn a_search_matching_nothing_is_survivable() {
        let mut graph = spot_graph();
        let mut app = App::new(None);
        app.list_state
            .select(Some(row_of_option(&graph, "www/zzz", "GAMMA")));

        app.on_key(key(KeyCode::Char('/')), &mut graph);
        for c in "no-such-port".chars() {
            app.on_key(key(KeyCode::Char(c)), &mut graph);
        }
        assert!(graph.visible_rows.is_empty());

        // Rubbing the query out brings the list back without a panic
        for _ in 0..12 {
            app.on_key(key(KeyCode::Backspace), &mut graph);
        }
        assert_eq!(graph.listed_port_count(), 2);
        assert!(app.list_state.selected().unwrap() < graph.visible_rows.len());
    }

    /// Esc clears the filter and puts the cursor back on the port it was in.
    #[test]
    fn escaping_search_restores_the_list_and_the_cursor() {
        let mut graph = spot_graph();
        let mut app = App::new(None);

        app.list_state
            .select(Some(row_of_option(&graph, "www/zzz", "BETA")));
        app.on_key(key(KeyCode::Char('/')), &mut graph);
        for c in "zzz".chars() {
            app.on_key(key(KeyCode::Char(c)), &mut graph);
        }
        app.on_key(key(KeyCode::Esc), &mut graph);

        assert_eq!(app.input_mode, InputMode::Normal);
        assert!(graph.search_query.is_empty());
        assert_eq!(graph.listed_port_count(), 2);
        assert_eq!(under_cursor(&graph, &app), "option BETA");
    }

    /// The results are browsable while the bar is still open — the point being
    /// that a query need not be committed to before looking at what it found.
    #[test]
    fn the_list_can_be_walked_while_the_query_is_still_being_typed() {
        let mut graph = spot_graph();
        let mut app = App::new(None);

        app.on_key(key(KeyCode::Char('/')), &mut graph);
        for c in "zzz".chars() {
            app.on_key(key(KeyCode::Char(c)), &mut graph);
        }
        assert_eq!(under_cursor(&graph, &app), "port www/zzz");
        assert_eq!(app.input_mode, InputMode::Search, "still typing");

        app.on_key(key(KeyCode::Down), &mut graph);
        assert_eq!(under_cursor(&graph, &app), "option ALPHA");
        app.on_key(key(KeyCode::Down), &mut graph);
        assert_eq!(under_cursor(&graph, &app), "option BETA");
        app.on_key(key(KeyCode::Up), &mut graph);
        assert_eq!(under_cursor(&graph, &app), "option ALPHA");
    }

    /// Moving is not typing: the arrows must not reach the field, or browsing
    /// the results would rewrite the query that produced them.
    #[test]
    fn walking_the_results_leaves_the_query_and_the_caret_alone() {
        let mut graph = spot_graph();
        let mut app = App::new(None);

        app.on_key(key(KeyCode::Char('/')), &mut graph);
        for c in "zzz".chars() {
            app.on_key(key(KeyCode::Char(c)), &mut graph);
        }
        let caret = app.text_cursor;

        for code in [
            KeyCode::Down,
            KeyCode::Up,
            KeyCode::PageDown,
            KeyCode::PageUp,
        ] {
            app.on_key(key(code), &mut graph);
        }

        assert_eq!(graph.search_query, "zzz");
        assert_eq!(app.text_cursor, caret);
        assert_eq!(app.input_mode, InputMode::Search);
    }

    /// Clamping follows the *filtered* list, so holding Down cannot walk off the
    /// last match into rows the filter is hiding.
    #[test]
    fn walking_off_the_end_of_the_results_stops_at_the_last_one() {
        let mut graph = spot_graph();
        let mut app = App::new(None);

        app.on_key(key(KeyCode::Char('/')), &mut graph);
        for c in "aaa".chars() {
            app.on_key(key(KeyCode::Char(c)), &mut graph);
        }
        let last = graph.visible_rows.len() - 1;

        for _ in 0..40 {
            app.on_key(key(KeyCode::Down), &mut graph);
        }
        assert_eq!(app.list_state.selected(), Some(last));
        assert_eq!(
            graph.listed_port_count(),
            1,
            "www/zzz is still hidden, so there was nowhere further to go"
        );

        for _ in 0..40 {
            app.on_key(key(KeyCode::Up), &mut graph);
        }
        assert_eq!(app.list_state.selected(), Some(0));
    }

    /// Where you browsed to is where you are left when the filter locks.
    #[test]
    fn a_row_walked_to_while_typing_survives_enter() {
        let mut graph = spot_graph();
        let mut app = App::new(None);

        app.on_key(key(KeyCode::Char('/')), &mut graph);
        for c in "zzz".chars() {
            app.on_key(key(KeyCode::Char(c)), &mut graph);
        }
        app.on_key(key(KeyCode::Down), &mut graph);
        app.on_key(key(KeyCode::Down), &mut graph);
        assert_eq!(under_cursor(&graph, &app), "option BETA");

        app.on_key(key(KeyCode::Enter), &mut graph);
        assert_eq!(app.input_mode, InputMode::Normal);
        assert_eq!(under_cursor(&graph, &app), "option BETA");
    }

    /// Typing after browsing re-anchors on the row browsed to, rather than
    /// throwing the cursor back to the top of a still-matching port.
    #[test]
    fn typing_on_after_browsing_keeps_the_row_it_landed_on() {
        let mut graph = spot_graph();
        let mut app = App::new(None);

        app.on_key(key(KeyCode::Char('/')), &mut graph);
        for c in "zz".chars() {
            app.on_key(key(KeyCode::Char(c)), &mut graph);
        }
        app.on_key(key(KeyCode::Down), &mut graph);
        app.on_key(key(KeyCode::Down), &mut graph);
        assert_eq!(under_cursor(&graph, &app), "option BETA");

        // www/zzz still matches the longer query
        app.on_key(key(KeyCode::Char('z')), &mut graph);
        assert_eq!(graph.search_query, "zzz");
        assert_eq!(under_cursor(&graph, &app), "option BETA");
    }

    /// A locked filter leaves the list narrowed with the search bar gone, so the
    /// header has to keep saying why.
    #[test]
    fn the_header_reports_an_active_filter() {
        let mut graph = spot_graph();
        let mut app = App::new(None);

        app.on_key(key(KeyCode::Char('/')), &mut graph);
        for c in "aaa".chars() {
            app.on_key(key(KeyCode::Char(c)), &mut graph);
        }
        app.on_key(key(KeyCode::Enter), &mut graph);
        assert_eq!(app.input_mode, InputMode::Normal, "filter is locked");

        let out = draw(|f| render(f, &graph, &mut app));
        assert!(
            out.contains("1 of 2 ports matching"),
            "the header must show the filter is on:\n{out}"
        );
    }

    /// A control chord still must not type itself into a buffer.
    #[test]
    fn other_control_chords_do_not_reach_the_buffer() {
        let mut graph = test_graph();
        let mut app = App::new(None);
        app.on_key(ctrl('g'), &mut graph);
        app.on_key(key(KeyCode::Enter), &mut graph);

        for c in "php".chars() {
            app.on_key(key(KeyCode::Char(c)), &mut graph);
        }
        app.on_key(ctrl('a'), &mut graph);
        app.on_key(ctrl('s'), &mut graph);

        assert_eq!(app.text_buffer, "php");
    }

    // -------------------------------------------------------------- autosave

    fn temp_config(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("bgone_auto_{}_{tag}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir.join("bgone.toml")
    }

    /// With a config file known, joining a group writes it out there and then;
    /// no separate trip through the manager.
    #[test]
    fn adding_to_a_group_saves_when_a_config_is_known() {
        let path = temp_config("add");
        let _ = std::fs::remove_file(&path);

        let mut graph = test_graph();
        let mut app = App::new(Some(path.clone()));

        app.on_key(ctrl('g'), &mut graph);
        app.on_key(key(KeyCode::Enter), &mut graph); // <new group...>
        for c in "php".chars() {
            app.on_key(key(KeyCode::Char(c)), &mut graph);
        }
        app.on_key(key(KeyCode::Enter), &mut graph);

        let written = std::fs::read_to_string(&path).expect("should have been saved");
        assert!(written.contains("lang/php83-extensions"), "got: {written}");
        assert!(app.status_msg.contains("saved"), "{}", app.status_msg);
        std::fs::remove_file(&path).ok();
    }

    /// Removing has to save too. If adding persisted and removing did not, a
    /// delete followed by a quit would leave the member in the file.
    #[test]
    fn removing_from_a_group_saves_as_well() {
        let path = temp_config("remove");
        let _ = std::fs::remove_file(&path);

        let mut graph = test_graph();
        graph.groups.insert(
            "php".into(),
            vec![
                "lang/php83-extensions".into(),
                "lang/php84-extensions".into(),
            ],
        );
        let mut app = App::new(Some(path.clone()));
        app.on_key(ctrl('g'), &mut graph);
        app.on_key(ctrl('g'), &mut graph);
        assert_eq!(app.input_mode, InputMode::GroupManage);

        app.on_key(key(KeyCode::Down), &mut graph);
        app.on_key(key(KeyCode::Char('d')), &mut graph);

        let written = std::fs::read_to_string(&path).expect("should have been saved");
        assert!(
            !written.contains("php83"),
            "the removal must reach the file: {written}"
        );
        assert!(written.contains("php84"));
        std::fs::remove_file(&path).ok();
    }

    /// Without a config file there is nowhere to write, and stopping to demand
    /// a path mid-edit would be worse than saying so.
    #[test]
    fn adding_without_a_config_says_it_is_unsaved_rather_than_prompting() {
        let mut graph = test_graph();
        let mut app = App::new(None);

        app.on_key(ctrl('g'), &mut graph);
        app.on_key(key(KeyCode::Enter), &mut graph);
        for c in "php".chars() {
            app.on_key(key(KeyCode::Char(c)), &mut graph);
        }
        app.on_key(key(KeyCode::Enter), &mut graph);

        assert_eq!(app.input_mode, InputMode::Normal, "must not interrupt");
        assert_eq!(graph.groups["php"], vec!["lang/php83-extensions"]);
        assert!(app.status_msg.contains("unsaved"), "{}", app.status_msg);
    }

    /// A group with no name would be unusable and unremovable from the manager.
    #[test]
    fn a_blank_group_name_is_refused() {
        let mut graph = test_graph();
        let mut app = App::new(None);
        app.on_key(ctrl('g'), &mut graph);
        app.on_key(key(KeyCode::Enter), &mut graph);

        for c in "   ".chars() {
            app.on_key(key(KeyCode::Char(c)), &mut graph);
        }
        app.on_key(key(KeyCode::Enter), &mut graph);

        assert_eq!(app.input_mode, InputMode::GroupNewName, "still asking");
        assert!(graph.groups.is_empty(), "no group was created");
    }

    // ---------------------------------------------------------------- manage

    fn app_in_manager(graph: &mut DependencyGraph) -> App {
        let mut app = App::new(None);
        app.on_key(ctrl('g'), graph);
        app.on_key(ctrl('g'), graph);
        assert_eq!(app.input_mode, InputMode::GroupManage);
        app
    }

    #[test]
    fn manage_deletes_a_member_and_then_the_empty_group() {
        let mut graph = test_graph();
        graph.groups.insert(
            "php".into(),
            vec![
                "lang/php83-extensions".into(),
                "lang/php84-extensions".into(),
            ],
        );
        let mut app = app_in_manager(&mut graph);

        // rows: [group php] [php83] [php84]
        app.on_key(key(KeyCode::Down), &mut graph);
        app.on_key(key(KeyCode::Char('d')), &mut graph);
        assert_eq!(graph.groups["php"], vec!["lang/php84-extensions"]);

        // Removing the last member takes the group with it
        app.on_key(key(KeyCode::Down), &mut graph);
        app.on_key(key(KeyCode::Char('d')), &mut graph);
        assert!(
            graph.groups.is_empty(),
            "an empty group should not linger: {:?}",
            graph.groups
        );
    }

    #[test]
    fn manage_deletes_a_whole_group_from_its_header() {
        let mut graph = test_graph();
        graph
            .groups
            .insert("php".into(), vec!["lang/php83-extensions".into()]);
        graph.groups.insert("ssl".into(), vec!["security/x".into()]);
        let mut app = app_in_manager(&mut graph);

        app.on_key(key(KeyCode::Char('d')), &mut graph);

        assert!(!graph.groups.contains_key("php"));
        assert!(graph.groups.contains_key("ssl"), "only one group goes");
    }

    #[test]
    fn manage_cursor_is_clamped_and_closes_on_esc() {
        let mut graph = test_graph();
        graph
            .groups
            .insert("php".into(), vec!["lang/php83-extensions".into()]);
        let mut app = app_in_manager(&mut graph);

        for _ in 0..9 {
            app.on_key(key(KeyCode::Down), &mut graph);
        }
        assert_eq!(app.group_cursor, 1, "one header plus one member");

        app.on_key(key(KeyCode::Esc), &mut graph);
        assert_eq!(app.input_mode, InputMode::Normal);
    }

    // ----------------------------------------------------------- config path

    #[test]
    fn saving_without_a_config_asks_for_one_then_remembers_it() {
        let dir = std::env::temp_dir().join(format!("bgone_ui_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("groups.toml");
        let _ = std::fs::remove_file(&path);

        let mut graph = test_graph();
        graph
            .groups
            .insert("php".into(), vec!["lang/php83-extensions".into()]);
        let mut app = app_in_manager(&mut graph);

        app.on_key(key(KeyCode::Char('s')), &mut graph);
        assert_eq!(app.input_mode, InputMode::ConfigPath, "must ask where");

        for c in path.to_string_lossy().chars() {
            app.on_key(key(KeyCode::Char(c)), &mut graph);
        }
        app.on_key(key(KeyCode::Enter), &mut graph);

        assert_eq!(app.input_mode, InputMode::GroupManage);
        let written = std::fs::read_to_string(&path).unwrap();
        assert!(written.contains("php"), "got: {written}");

        // A second save goes straight there without asking again
        app.on_key(key(KeyCode::Char('s')), &mut graph);
        assert_eq!(app.input_mode, InputMode::GroupManage);
        assert!(app.status_msg.contains("Saved"), "{}", app.status_msg);

        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn escaping_the_path_prompt_writes_nothing() {
        let mut graph = test_graph();
        graph
            .groups
            .insert("php".into(), vec!["lang/php83-extensions".into()]);
        let mut app = app_in_manager(&mut graph);

        app.on_key(key(KeyCode::Char('s')), &mut graph);
        app.on_key(key(KeyCode::Char('x')), &mut graph);
        app.on_key(key(KeyCode::Esc), &mut graph);

        assert_eq!(app.input_mode, InputMode::GroupManage);
        assert!(app.config_path.is_none(), "nothing was chosen");
    }

    // ------------------------------------------------------------ normal mode

    /// A regression net over the extract itself: the ordinary keys still work.
    #[test]
    fn space_toggles_the_option_under_the_cursor() {
        let mut graph = test_graph();
        let mut app = App::new(None);
        app.list_state.select(Some(first_option_row(&graph)));

        assert!(!graph.is_dirty());
        app.on_key(key(KeyCode::Char(' ')), &mut graph);
        assert!(graph.is_dirty(), "Space should have toggled an option");
    }

    #[test]
    fn quitting_clean_finishes_but_quitting_dirty_asks_first() {
        let mut graph = test_graph();
        let mut app = App::new(None);

        assert_eq!(
            app.on_key(key(KeyCode::Char('q')), &mut graph),
            KeyOutcome::Finish(TuiAction::QuitWithoutSaving)
        );

        app.list_state.select(Some(first_option_row(&graph)));
        app.on_key(key(KeyCode::Char(' ')), &mut graph);
        assert_eq!(
            app.on_key(key(KeyCode::Char('q')), &mut graph),
            KeyOutcome::Continue
        );
        assert_eq!(app.input_mode, InputMode::ConfirmQuit);

        // ...and confirming goes through
        assert_eq!(
            app.on_key(key(KeyCode::Char('y')), &mut graph),
            KeyOutcome::Finish(TuiAction::QuitWithoutSaving)
        );
    }

    /// Ctrl+R is reported rather than performed, so the dispatch stays free of
    /// the terminal.
    #[test]
    fn ctrl_r_asks_for_a_repaint_instead_of_doing_one() {
        let mut graph = test_graph();
        let mut app = App::new(None);
        assert_eq!(app.on_key(ctrl('r'), &mut graph), KeyOutcome::Redraw);
    }

    /// A graph with a real dependency edge, so there is a relationship row to
    /// follow.
    fn linked_graph() -> DependencyGraph {
        let conn = Connection::open_in_memory().unwrap();
        crate::db::init_db(&conn, true).unwrap();
        for origin in ["www/app", "devel/lib"] {
            fixture_option(&conn, origin, "DOCS", true, "DEFINE", "");
        }
        fixture_edge(&conn, "www/app", "devel/lib", None);
        let mut graph = DependencyGraph::load_from_db(
            &conn,
            &["www/app".to_string()],
            &crate::reader::SystemOptions::default(),
            false,
        )
        .unwrap();
        graph.expand_all();
        graph
    }

    #[test]
    fn enter_follows_a_relationship_and_backspace_retraces() {
        let mut graph = linked_graph();
        let mut app = App::new(None);

        let requires_entry = graph
            .visible_rows
            .iter()
            .position(|r| matches!(&r.kind, RowKind::RequiresEntry { .. }))
            .expect("www/app requires devel/lib");
        app.list_state.select(Some(requires_entry));

        assert_eq!(
            app.on_key(key(KeyCode::Enter), &mut graph),
            KeyOutcome::Continue,
            "Enter on a relationship row follows it rather than pressing OK"
        );
        let landed = app.list_state.selected().unwrap();
        assert!(
            matches!(&graph.visible_rows[landed].kind,
                     RowKind::Port { origin, .. } if origin == "devel/lib"),
            "should have jumped to devel/lib"
        );

        app.on_key(key(KeyCode::Backspace), &mut graph);
        let back = app.list_state.selected().unwrap();
        assert!(
            matches!(&graph.visible_rows[back].kind,
                     RowKind::Port { origin, .. } if origin == "www/app"),
            "Backspace should retrace to where the jump started"
        );
    }

    /// Enter anywhere else still presses OK, as it always has.
    #[test]
    fn enter_off_a_relationship_row_still_presses_ok() {
        let mut graph = test_graph();
        let mut app = App::new(None);
        assert_eq!(
            app.on_key(key(KeyCode::Enter), &mut graph),
            KeyOutcome::Finish(TuiAction::SaveAndQuit)
        );
    }

    fn groups_of(pairs: &[(&str, &[&str])]) -> BTreeMap<String, Vec<String>> {
        pairs
            .iter()
            .map(|(n, m)| (n.to_string(), m.iter().map(|s| s.to_string()).collect()))
            .collect()
    }

    /// Renders one frame and returns it as text, so a dialog can be checked
    /// without a terminal to drive.
    fn draw(render: impl FnOnce(&mut Frame)) -> String {
        let mut terminal = Terminal::new(TestBackend::new(90, 24)).unwrap();
        terminal.draw(|f| render(f)).unwrap();
        let buffer = terminal.backend().buffer().clone();
        buffer
            .content()
            .chunks(buffer.area().width as usize)
            .map(|row| row.iter().map(|cell| cell.symbol()).collect::<String>())
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn manage_rows_lists_each_group_then_its_members() {
        let groups = groups_of(&[("a", &["www/one", "www/two"]), ("b", &["www/three"])]);
        let rows = manage_rows(&groups);

        let described: Vec<String> = rows
            .iter()
            .map(|r| match r {
                ManageRow::Group(name) => format!("group {name}"),
                ManageRow::Member { group, origin } => format!("  {group}/{origin}"),
            })
            .collect();

        assert_eq!(
            described,
            vec![
                "group a",
                "  a/www/one",
                "  a/www/two",
                "group b",
                "  b/www/three",
            ]
        );
    }

    #[test]
    fn assign_dialog_offers_every_group_and_a_new_one() {
        let groups = groups_of(&[("php-extensions", &["lang/php83-extensions"])]);
        let out = draw(|f| render_group_assign(f, "lang/php84-extensions", &groups, 0));

        assert!(out.contains("Add to group"));
        assert!(out.contains("lang/php84-extensions"));
        assert!(out.contains("php-extensions"));
        assert!(out.contains("<new group...>"));
    }

    /// A port already in the group is still shown, marked, rather than silently
    /// missing from the list.
    #[test]
    fn assign_dialog_marks_a_group_the_port_already_belongs_to() {
        let groups = groups_of(&[("php-extensions", &["lang/php83-extensions"])]);
        let out = draw(|f| render_group_assign(f, "lang/php83-extensions", &groups, 0));
        assert!(out.contains("already a member"), "got:\n{out}");
    }

    #[test]
    fn manage_dialog_shows_members_and_where_a_save_would_go() {
        let groups = groups_of(&[("php-extensions", &["lang/php83-extensions"])]);

        let unsaved = draw(|f| render_group_manage(f, &groups, 0, None));
        assert!(unsaved.contains("lang/php83-extensions"));
        assert!(
            unsaved.contains("no config file yet"),
            "the manager must say a save will ask for a path:\n{unsaved}"
        );

        let saved =
            draw(|f| render_group_manage(f, &groups, 0, Some(Path::new("/etc/bgone.toml"))));
        assert!(saved.contains("/etc/bgone.toml"), "got:\n{saved}");
    }

    #[test]
    fn manage_dialog_says_so_when_there_are_no_groups() {
        let out = draw(|f| render_group_manage(f, &BTreeMap::new(), 0, None));
        assert!(out.contains("No groups yet"), "got:\n{out}");
    }

    /// The caret has to be visible, or moving it is guesswork.
    #[test]
    fn the_prompt_marks_where_the_caret_is() {
        // Caret in the middle: the character it sits on is picked out
        let out = draw(|f| render_prompt(f, "New group", "Name:", "abcdef", 3));
        assert!(out.contains("abcdef"), "got:\n{out}");

        // Caret past the end still renders, on a blank cell
        let out = draw(|f| render_prompt(f, "New group", "Name:", "abc", 3));
        assert!(out.contains("abc"), "got:\n{out}");

        // ...and an empty field is fine
        let out = draw(|f| render_prompt(f, "New group", "Name:", "", 0));
        assert!(out.contains("Name:"), "got:\n{out}");
    }

    #[test]
    fn prompt_shows_the_question_and_what_has_been_typed() {
        let out = draw(|f| render_prompt(f, "New group", "Name a group:", "php-ext", 7));
        assert!(out.contains("New group"));
        assert!(out.contains("Name a group:"));
        assert!(out.contains("php-ext"));
    }
}
