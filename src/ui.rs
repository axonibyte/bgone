use crate::config::save_groups;
use crate::graph::{
    next_sibling_index, prev_sibling_index, DependencyGraph, NodeId, Provenance, ResettleOutcome,
    RowAnchor, RowKind, SectionKind,
};
use crate::oracle::{Options, Oracle, Question};
use crate::reader::SystemOptions;
use crate::resolve::PortFacts;
use anyhow::Result;
use crossterm::{
    cursor::{MoveTo, Show},
    event::{
        self, DisableBracketedPaste, EnableBracketedPaste, Event, KeyCode, KeyEvent, KeyEventKind,
        KeyModifiers,
    },
    execute,
    style::ResetColor,
    // Aliased: ratatui has a `Clear` widget of its own, and both are in use here
    terminal::{
        disable_raw_mode, enable_raw_mode, Clear as ClearScreen, ClearType, EnterAlternateScreen,
        LeaveAlternateScreen,
    },
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
use std::sync::mpsc::{self, Receiver, Sender};

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
    " Space toggle | Enter jump | Bksp back | ^G group | Shift+H hide\n",
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

/// Button focus inside the "leaving" confirmation box.
///
/// Three ways out rather than two. "Yes / No" only ever asked whether to throw
/// the session away, so saving on the way out meant answering no, finding the
/// OK button and pressing that instead — with the quit key still the obvious
/// thing to have reached for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConfirmChoice {
    Save,
    Discard,
    Cancel,
}

impl ConfirmChoice {
    pub fn next(self) -> Self {
        match self {
            ConfirmChoice::Save => ConfirmChoice::Discard,
            ConfirmChoice::Discard => ConfirmChoice::Cancel,
            ConfirmChoice::Cancel => ConfirmChoice::Save,
        }
    }

    pub fn prev(self) -> Self {
        match self {
            ConfirmChoice::Save => ConfirmChoice::Cancel,
            ConfirmChoice::Discard => ConfirmChoice::Save,
            ConfirmChoice::Cancel => ConfirmChoice::Discard,
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

/// Describes what a toggle did: the pressed option's new state, anything else
/// the press moved on its port — implications, conflicts, a displaced group
/// sibling — and, when nothing moved at all, which rule refused the press.
/// A cascade or a refusal that happens silently reads as corruption.
fn toggle_report(
    graph: &DependencyGraph,
    pressed_name: &str,
    pressed_id: usize,
    before: &[(usize, bool)],
) -> String {
    let state = |on: bool| if on { "on" } else { "off" };
    let now_on = graph.option_nodes[pressed_id].enabled;
    let was_on = before
        .iter()
        .find(|(id, _)| *id == pressed_id)
        .map(|&(_, on)| on)
        .unwrap_or(!now_on);

    let also: Vec<String> = before
        .iter()
        .filter(|&&(id, was)| id != pressed_id && graph.option_nodes[id].enabled != was)
        .map(|&(id, was)| format!("{} {}", graph.option_nodes[id].name, state(!was)))
        .collect();

    let lead = if now_on == was_on {
        format!("'{pressed_name}' stays {}", state(now_on))
    } else {
        format!("'{pressed_name}' {}", state(now_on))
    };

    if !also.is_empty() {
        return format!("{lead}; also: {}", also.join(", "));
    }
    if now_on != was_on {
        return lead;
    }

    // Nothing moved: name the rule that held the press.
    let opt = &graph.option_nodes[pressed_id];
    if opt.group_type == "SINGLE" && now_on {
        return format!("{lead}: a SINGLE keeps one member set");
    }
    let implier = graph.ports[opt.parent_port]
        .options
        .iter()
        .copied()
        .find(|&id| {
            let o = &graph.option_nodes[id];
            id != pressed_id && o.enabled && o.implies.iter().any(|n| n == pressed_name)
        });
    if let Some(id) = implier {
        return format!(
            "{lead}: {} implies it and cannot turn off",
            graph.option_nodes[id].name
        );
    }
    lead
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

/// First row of a `height`-tall window over `len` rows that contains `cursor`.
///
/// A dialog listing every group outgrew the screen as soon as there were more
/// groups than rows, and what fell off the bottom was unreachable rather than
/// merely unseen — the cursor could be moved onto it, but nothing showed where
/// it had gone. The window scrolls just far enough to keep the cursor inside it,
/// so it never moves while the cursor is already visible.
fn scroll_window(len: usize, cursor: usize, height: usize) -> usize {
    if height == 0 || len <= height {
        return 0;
    }
    let last_start = len - height;
    cursor.saturating_sub(height - 1).min(last_start)
}

/// The visible slice of a scrolled list, and a note saying where in the whole
/// it sits. The note is `None` when all of it fits and there is nothing to say.
///
/// The slice is exactly `height` rows: a marker drawn *over* an edge row would
/// hide the very entry the cursor had just been moved onto.
fn windowed_lines(
    lines: Vec<Line<'static>>,
    cursor: usize,
    height: usize,
) -> (Vec<Line<'static>>, Option<String>) {
    if lines.len() <= height || height == 0 {
        return (lines, None);
    }
    let start = scroll_window(lines.len(), cursor, height);
    let end = (start + height).min(lines.len());
    let note = format!("  showing {}-{} of {}", start + 1, end, lines.len());
    (lines[start..end].to_vec(), Some(note))
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
    // One row per group, plus <new group...>; the rest of the box is the
    // heading, the blank line, the hints and the two borders.
    let rows = groups.len() + 1;
    let chrome = 6;
    let height = (rows as u16 + chrome as u16).min(f.size().height.saturating_sub(2));
    let area = centered_rect(64, height.max(chrome as u16 + 1), f.size());
    f.render_widget(Clear, area);

    let room = (area.height as usize).saturating_sub(chrome).max(1);

    let mut items = Vec::new();
    for (i, (name, members)) in groups.iter().enumerate() {
        let already = members.iter().any(|m| m == target);
        let label = if already {
            format!("{name}  (already a member)")
        } else {
            format!("{name}  ({} ports)", members.len())
        };
        items.push(selectable_line(&label, i == cursor, already));
    }
    items.push(selectable_line(
        "<new group...>",
        cursor == groups.len(),
        false,
    ));
    let (items, note) = windowed_lines(items, cursor, room);

    let mut lines = vec![Line::from(vec![
        Span::raw("Add "),
        Span::styled(target.to_string(), Style::default().fg(Color::White)),
        Span::raw(" to:"),
    ])];
    lines.extend(items);
    lines.push(Line::from(""));
    if let Some(note) = note {
        lines.push(Line::styled(note, Style::default().fg(Color::DarkGray)));
    }
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
    // Blank line, the "saves to" line, the hints and the two borders.
    let chrome = 5;
    let height = (rows.len().max(1) as u16 + chrome as u16).min(f.size().height.saturating_sub(2));
    let area = centered_rect(70, height.max(chrome as u16 + 1), f.size());
    f.render_widget(Clear, area);

    let room = (area.height as usize).saturating_sub(chrome).max(1);

    let mut items = Vec::new();
    if rows.is_empty() {
        items.push(Line::styled(
            "  No groups yet. Ctrl+G on a port starts one.",
            Style::default().fg(Color::DarkGray),
        ));
    }
    for (i, row) in rows.iter().enumerate() {
        match row {
            ManageRow::Group(name) => {
                let count = groups.get(name).map(|m| m.len()).unwrap_or(0);
                items.push(selectable_line(
                    &format!("{name}  ({count} ports)"),
                    i == cursor,
                    false,
                ));
            }
            ManageRow::Member { origin, .. } => {
                items.push(selectable_line(
                    &format!("    {origin}"),
                    i == cursor,
                    false,
                ));
            }
        }
    }
    let (items, note) = windowed_lines(items, cursor, room);

    let mut lines = items;
    match note {
        Some(note) => lines.push(Line::styled(note, Style::default().fg(Color::DarkGray))),
        None => lines.push(Line::from("")),
    }
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

fn render_confirm_quit(f: &mut Frame, choice: ConfirmChoice, dirty: bool) {
    let area = centered_rect(56, 7, f.size());
    f.render_widget(Clear, area);

    let body = Text::from(vec![
        Line::from(""),
        Line::from(if dirty {
            "You have unsaved option changes."
        } else {
            "Nothing has changed since the last save."
        }),
        Line::from(""),
        button_row(&[
            ("Save and exit", choice == ConfirmChoice::Save),
            ("Discard", choice == ConfirmChoice::Discard),
            ("Cancel", choice == ConfirmChoice::Cancel),
        ]),
        Line::styled("Esc again discards", Style::default().fg(Color::DarkGray)),
    ]);

    let modal = Paragraph::new(body)
        .alignment(Alignment::Center)
        .style(Style::default().fg(Color::White))
        .block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(Color::Yellow))
                .title(" Leaving bgone "),
        );

    f.render_widget(modal, area);
}

/// Everything the interface remembers between keystrokes.
///
/// Held as one value rather than as locals inside the event loop so that key
/// handling is a plain function of (state, key) and can be exercised without a
/// terminal. The dialogs went in without that and shipped with an unreachable
/// group manager, which reading the code twice did not catch.
/// Answers to a batch, in the order they were asked for.
type Answers = Vec<(Question, std::result::Result<PortFacts, String>)>;

/// Runs evaluations away from the event loop.
///
/// A toggle changes a port's option set, which changes what it depends on — and
/// finding out means running `make`, which takes between 50 ms and 600 ms
/// depending on the port. Doing that on the keystroke would stutter visibly on
/// anything the size of `lang/php84`, so the request is posted and the answer
/// collected on a later frame.
///
/// A batch at a time rather than a request at a time, so one round of the walk
/// runs in parallel across cores through [`Oracle::facts_many`].
struct Resolver {
    tx: Sender<Vec<Question>>,
    rx: Receiver<Answers>,
}

impl Resolver {
    fn spawn(oracle: Oracle) -> Self {
        let (tx, requests) = mpsc::channel::<Vec<Question>>();
        let (answers, rx) = mpsc::channel::<Answers>();

        std::thread::spawn(move || {
            while let Ok(batch) = requests.recv() {
                let replies = oracle.facts_many(&batch);
                let out: Answers = batch
                    .into_iter()
                    .zip(replies)
                    // The error is flattened to a string because `anyhow::Error`
                    // is not `Send` in every shape it can take, and nothing on
                    // the far side does anything with it but show it.
                    .map(|(q, (_, r))| (q, r.map_err(|e| e.to_string())))
                    .collect();
                if answers.send(out).is_err() {
                    break;
                }
            }
        });

        Self { tx, rx }
    }
}

/// Writes the options out where they belong, reporting what it wrote.
///
/// Held as a function rather than done inline so that key handling stays a plain
/// function of (state, key): the tests drive `Ctrl + S` with a closure that
/// records the call, and neither they nor `on_key` need an options directory.
pub type Saver = Box<dyn Fn(&DependencyGraph) -> Result<String>>;

/// Brings the graph into agreement with the tree, blocking until it is.
///
/// Run before writing, because what gets written is exactly what is in the
/// build: a question raised by the last keystroke may still be out with the
/// worker, and a port pulled in by that toggle would otherwise be left out of
/// the files — which is the failure this whole design exists to fix.
///
/// Returns what the resettle did — the ports that arrived, and any it could
/// not re-ask.
pub type Settle = Box<dyn Fn(&mut DependencyGraph) -> ResettleOutcome>;

struct App {
    input_mode: InputMode,
    focus: Focus,
    list_state: ListState,
    status_msg: String,
    confirm_choice: ConfirmChoice,
    /// What `Ctrl + S` calls. Absent only in tests that never press it.
    saver: Option<Saver>,
    /// Run before a save, to settle anything the last keystroke raised.
    settle: Option<Settle>,
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
    /// Ports whose dependencies need asking about again, queued by key handling
    /// and posted by the event loop.
    ///
    /// Held rather than sent so that `on_key` stays a plain function of (state,
    /// key) with no channel of its own — the tests read this list instead of
    /// standing up a worker and a ports tree.
    pending: Vec<Question>,
    /// How many questions are out with the worker, for the "resolving" note.
    outstanding: usize,
    /// A message that must not be hidden behind the "resolving" note — a save
    /// result, the worker dying. Cleared by the next keystroke, which is the
    /// acknowledgement.
    important_msg: Option<String>,
    /// True once the worker's channel is gone. Nothing further is posted, no
    /// more `outstanding` accrues, and the header says so.
    resolver_dead: bool,
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
            confirm_choice: ConfirmChoice::Save,
            saver: None,
            settle: None,
            recenter_pos: None,
            last_tree_key: None,
            viewport_height: 0,
            jump_stack: Vec::new(),
            config_path,
            group_target: None,
            group_cursor: 0,
            text_buffer: String::new(),
            text_cursor: 0,
            pending: Vec::new(),
            outstanding: 0,
            important_msg: None,
            resolver_dead: false,
        }
    }

    fn with_saver(config_path: Option<PathBuf>, saver: Saver) -> Self {
        Self {
            saver: Some(saver),
            ..Self::new(config_path)
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
    /// Settle and write without leaving. Returned rather than performed so the
    /// event loop can draw a "saving" frame before `make` blocks the thread.
    SaveInPlace,
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
        // How far down the screen the cursor was sitting. Keeping the row still
        // is only half of it: a change *above* the cursor renumbers it, and the
        // list widget then scrolls by exactly enough to bring the new number
        // back into view — which parks the same row against the bottom edge.
        // Holding the offset the same distance behind it leaves the row where
        // the eye already is.
        let screen_row = self
            .list_state
            .selected()
            .unwrap_or(0)
            .saturating_sub(self.list_state.offset());

        let row = anchor
            .and_then(|a| graph.row_of_anchor(&a))
            .unwrap_or(fallback)
            .min(last);

        self.list_state.select(Some(row));
        *self.list_state.offset_mut() = row.saturating_sub(screen_row);
    }

    /// Queues a fresh question about every port whose options have just changed.
    ///
    /// What a port depends on is not derivable from what is already known about
    /// it: `MYSQL_USES=mysql` and `.if ${PORT_OPTIONS:MFOO}` blocks produce
    /// nothing at all until the option is set while make reads the Makefile. So
    /// the port is asked about again, under the set now in force.
    ///
    /// A port already queued is not queued twice — holding a key down would
    /// otherwise pile up questions whose answers are all superseded by the last.
    fn ask_about(&mut self, graph: &DependencyGraph, origins: &[String]) {
        for origin in origins {
            let Some(port_id) = graph.port_index(origin) else {
                continue;
            };
            let question = (origin.clone(), Options::Exactly(graph.option_set(port_id)));
            if !self.pending.contains(&question) {
                self.pending.push(question);
            }
        }
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

    /// What Ctrl+S does: settle, write, and report. Split from the key handler
    /// so the event loop can draw a "saving" frame first — `make` blocks the
    /// thread — and so tests can drive it without a terminal. The outcome
    /// lands in `important_msg`, which the header shows even while answers are
    /// outstanding; written to `status_msg`, the save message was invisible
    /// behind the "resolving" note whenever a toggle's question was still out.
    ///
    /// `pending` and `outstanding` are deliberately left alone: in-flight
    /// answers drain normally afterwards, and the staleness gate in [`merge`]
    /// drops any the settle superseded.
    fn perform_save(&mut self, graph: &mut DependencyGraph) {
        let selected_index = self.list_state.selected().unwrap_or(0);

        // Settled first: what is written is what is in the build, so the
        // build has to be up to date with the options as they now stand.
        let mut arrived = 0;
        let mut failed = 0;
        if let Some(settle) = &self.settle {
            let anchor = graph.anchor_at(selected_index);
            let outcome = settle(graph);
            arrived = outcome.arrived.len();
            failed = outcome.failed.len();
            self.restore_anchor(graph, anchor);
        }

        let mut message = match &self.saver {
            Some(save) => match save(graph) {
                Ok(what) => what,
                Err(e) => format!("Could not save: {e}"),
            },
            None => String::from("Nowhere to save to"),
        };
        if arrived > 0 {
            message = format!("{message}; {arrived} port(s) pulled in");
        }
        // A port that could not be re-asked may have been written stale —
        // said next to the save, not swallowed by it.
        if failed > 0 {
            message = format!("{message}; {failed} port(s) could not be re-evaluated");
        }
        self.important_msg = Some(message);
    }

    /// Called when the worker's channel reports closed. Its outstanding
    /// questions will never be answered, so the count is zeroed rather than
    /// left pinning the header to "resolving" forever — which also hid every
    /// status message behind it, the saved-files message included. Pending
    /// questions are dropped: there is nothing left to answer them.
    fn mark_resolver_dead(&mut self) {
        self.resolver_dead = true;
        self.outstanding = 0;
        self.pending.clear();
        self.important_msg = Some(String::from(
            "resolver died; new dependencies will not appear, but Ctrl+S still settles and saves",
        ));
    }

    /// Folds pasted text into whichever field is being typed into. Only the
    /// first line is taken — a pasted newline must not act as Enter — and
    /// control characters are dropped. Everywhere without a field (Normal,
    /// the dialogs) a paste is ignored outright: before bracketed paste, a
    /// paste there arrived as keystrokes, and any `s` in it saved and exited.
    fn on_paste(&mut self, pasted: &str, graph: &mut DependencyGraph) {
        let line = pasted.lines().next().unwrap_or("");
        let chars = line.chars().filter(|c| !c.is_control());

        match self.input_mode {
            InputMode::Search => {
                for c in chars {
                    edit_field(
                        &mut graph.search_query,
                        &mut self.text_cursor,
                        KeyCode::Char(c),
                        false,
                    );
                }
                // The same refresh a typed character gets: results narrow as
                // the query grows, anchored to the row being looked at.
                let selected = self.list_state.selected().unwrap_or(0);
                let anchor = graph.anchor_at(selected);
                graph.rebuild_visible_rows();
                self.restore_anchor_or(graph, anchor, 0);
            }
            InputMode::GroupNewName | InputMode::ConfigPath => {
                for c in chars {
                    edit_field(
                        &mut self.text_buffer,
                        &mut self.text_cursor,
                        KeyCode::Char(c),
                        false,
                    );
                }
            }
            InputMode::Normal
            | InputMode::ConfirmQuit
            | InputMode::GroupAssign
            | InputMode::GroupManage => {}
        }
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

        // Any keystroke acknowledges an important message, the way status
        // messages have always been overwritten rather than expired.
        self.important_msg = None;

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
                            let already = graph
                                .groups
                                .get(&name)
                                .map(|m| m.contains(&target))
                                .unwrap_or(false);

                            if already {
                                self.status_msg = format!("{target} is already in {name}");
                            } else {
                                // Read before the port is added, so it cannot
                                // find itself as the member to copy from
                                let adopted = graph.adopt_group_options(&name, &target);
                                let members = graph.groups.entry(name.clone()).or_default();
                                members.push(target.clone());
                                members.sort();

                                let did = match adopted {
                                    0 => format!("Added {target} to {name}"),
                                    1 => format!("Added {target} to {name}, matching 1 option"),
                                    n => format!("Added {target} to {name}, matching {n} options"),
                                };
                                self.autosave_groups(graph, &did);
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
                // Asking twice and getting the same answer means it: the second
                // Esc or Ctrl+C leaves without waiting to be pointed at a
                // button. Checked before the plain 'c' below, which is the
                // Cancel hotkey and means the opposite.
                KeyCode::Esc => return KeyOutcome::Finish(TuiAction::QuitWithoutSaving),
                KeyCode::Char('c') | KeyCode::Char('C') if has_ctrl => {
                    return KeyOutcome::Finish(TuiAction::QuitWithoutSaving);
                }

                KeyCode::Char('s') | KeyCode::Char('S') => {
                    return KeyOutcome::Finish(TuiAction::SaveAndQuit);
                }
                KeyCode::Char('d')
                | KeyCode::Char('D')
                | KeyCode::Char('y')
                | KeyCode::Char('Y') => {
                    return KeyOutcome::Finish(TuiAction::QuitWithoutSaving);
                }
                KeyCode::Char('c')
                | KeyCode::Char('C')
                | KeyCode::Char('n')
                | KeyCode::Char('N') => {
                    self.input_mode = InputMode::Normal;
                    self.status_msg = String::from("Returned to configuration");
                }

                KeyCode::Right | KeyCode::Tab | KeyCode::Down => {
                    self.confirm_choice = self.confirm_choice.next();
                }
                KeyCode::Left | KeyCode::BackTab | KeyCode::Up => {
                    self.confirm_choice = self.confirm_choice.prev();
                }

                KeyCode::Enter | KeyCode::Char(' ') => match self.confirm_choice {
                    ConfirmChoice::Save => return KeyOutcome::Finish(TuiAction::SaveAndQuit),
                    ConfirmChoice::Discard => {
                        return KeyOutcome::Finish(TuiAction::QuitWithoutSaving);
                    }
                    ConfirmChoice::Cancel => {
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

                    // Write the options out and carry on. Distinct from the
                    // < OK > button, which writes and leaves: keeping a long
                    // session's work safe should not cost the session.
                    // Returned to the loop rather than performed, so a frame
                    // of feedback can go up before `make` blocks it.
                    KeyCode::Char('s') | KeyCode::Char('S') if has_ctrl => {
                        return KeyOutcome::SaveInPlace;
                    }

                    // Letter hotkeys work from any focus. `s` and `o` go
                    // through the confirmation like `q`, with Save already
                    // highlighted — ending the session and writing files is
                    // one slip of a finger otherwise, and dialog(1) commits
                    // only from a focused button. Enter on < OK > stays
                    // immediate, because focusing the button is deliberate.
                    KeyCode::Char('o') | KeyCode::Char('O') => quit_requested = true,

                    KeyCode::Char('s') | KeyCode::Char('S') => quit_requested = true,

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

                    // Navigation: Top & Bottom. Only the real keys: the
                    // `Char('g')`/`Char('G')` alternatives that used to sit
                    // here were unreachable — their guard required Ctrl, and
                    // Ctrl+G is consumed by the group arm above.
                    KeyCode::Home => {
                        self.list_state.select(Some(0));
                        self.status_msg = String::from("Jumped to top");
                    }

                    KeyCode::End => {
                        let last = total_rows.saturating_sub(1);
                        self.list_state.select(Some(last));
                        self.status_msg = String::from("Jumped to bottom");
                    }

                    // Most of a build set is leaf libraries with nothing to
                    // decide; Shift + H takes them out of the way and puts
                    // them back.
                    KeyCode::Char('H') if !has_ctrl => {
                        let anchor = graph.anchor_at(selected_index);
                        graph.hide_optionless = !graph.hide_optionless;
                        graph.rebuild_visible_rows();
                        self.restore_anchor(graph, anchor);

                        let hidden = graph.optionless_count();
                        self.status_msg = if graph.hide_optionless {
                            format!("Hiding {hidden} port(s) with no options")
                        } else {
                            format!("Showing all {} ports", graph.live_count())
                        };
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
                            let pressed = graph
                                .visible_rows
                                .get(selected)
                                .map(|row| (row.kind.clone(), row.node_id.clone()));
                            match pressed {
                                Some((RowKind::Option { name, .. }, NodeId::Option(opt_id))) => {
                                    // Cascades — implications, conflicts, group
                                    // sync — move options the press never named,
                                    // and moving them silently reads as
                                    // corruption. The port's states are
                                    // snapshotted so the message can say what
                                    // else moved, or why nothing did.
                                    let port_id = graph.option_nodes[opt_id].parent_port;
                                    let before: Vec<(usize, bool)> = graph.ports[port_id]
                                        .options
                                        .iter()
                                        .map(|&id| (id, graph.option_nodes[id].enabled))
                                        .collect();

                                    // A toggle can strand whole ports out of
                                    // the list, so it renumbers rows just as
                                    // an expand does
                                    let anchor = graph.anchor_at(selected);
                                    let touched = graph.toggle_option(selected);
                                    self.restore_anchor(graph, anchor);
                                    self.ask_about(graph, &touched);

                                    self.status_msg = toggle_report(graph, &name, opt_id, &before);
                                }
                                Some(_) => {
                                    self.status_msg = String::from("Cannot toggle non-option row");
                                }
                                None => {}
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

        // Always asked, not only when something has changed. Esc and Ctrl+C are
        // reached for by reflex and by habit from other programs, and neither
        // should be able to end a session of picking through options on the
        // first press. Pressing the same key again answers it.
        if quit_requested {
            self.input_mode = InputMode::ConfirmQuit;
            self.confirm_choice = ConfirmChoice::Save;
            self.status_msg = if graph.is_dirty() {
                String::from("Unsaved changes")
            } else {
                String::from("Leaving")
            };
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
    let target = if graph.search_query.is_empty() && !graph.hide_optionless {
        format!(
            " Target: {}  ({} ports)",
            graph.root_origin,
            graph.live_count()
        )
    } else if graph.search_query.is_empty() {
        // Hiding narrows the list the same way a filter does, and leaving it
        // unsaid makes the missing ports look like a fault.
        format!(
            " Target: {}  ({} of {} ports, no-option ports hidden)",
            graph.root_origin,
            graph.listed_port_count(),
            graph.live_count()
        )
    } else {
        format!(
            " Target: {}  ({} of {} ports matching \"{}\"{})",
            graph.root_origin,
            graph.listed_port_count(),
            graph.live_count(),
            graph.search_query,
            if graph.hide_optionless {
                ", no-option ports hidden"
            } else {
                ""
            }
        )
    };
    // Said while make runs, because a toggle can take the better part of a
    // second on a large port and silence reads as nothing having happened.
    // An important message — a save result, the worker dying — outranks the
    // note instead of hiding behind it: Ctrl+S while a toggle's question was
    // still out used to report the save invisibly.
    let mut status = if let Some(msg) = &app.important_msg {
        if app.outstanding > 0 {
            format!("{msg} | resolving {} port(s)... ", app.outstanding)
        } else {
            format!("{msg} ")
        }
    } else if app.outstanding > 0 {
        format!("resolving {} port(s)... ", app.outstanding)
    } else {
        format!("{} ", app.status_msg)
    };
    if app.resolver_dead {
        status = format!("[resolver stopped] {status}");
    }
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

                // Membership has to be visible on the row, because it changes
                // what a keystroke does: Space here also moves every other
                // member. Its own colour, since provenance already owns the
                // name's.
                let joined: Vec<&str> = graph
                    .groups
                    .iter()
                    .filter(|(_, members)| members.iter().any(|m| m == origin))
                    .map(|(name, _)| name.as_str())
                    .collect();
                if !joined.is_empty() {
                    spans.push(Span::styled(
                        format!("  [{}]", joined.join(", ")),
                        Style::default().fg(Color::Cyan),
                    ));
                }

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
        InputMode::ConfirmQuit => render_confirm_quit(f, app.confirm_choice, graph.is_dirty()),
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

/// Folds one batch of answers into the graph, and reports what it learned.
///
/// A port the answer mentions that the graph has never seen is added and asked
/// about in turn, so a dependency that appears three levels down from a toggle
/// still arrives. That cascade is what makes the list match what poudriere will
/// compute, rather than what the port looked like at its maintainer's defaults.
///
/// An answer is folded in only if it still describes the port as it now
/// stands. Toggling an option twice while `make` is out produces two
/// questions; applying the first answer would show dependencies for an option
/// set no longer in force until the second landed. The superseded answer is
/// dropped instead — its successor is already in flight.
fn merge(
    graph: &mut DependencyGraph,
    app: &mut App,
    sys_opts: &SystemOptions,
    answers: Answers,
) -> Vec<String> {
    let mut arrived = Vec::new();

    for ((origin, asked), reply) in answers {
        let facts = match reply {
            Ok(facts) => facts,
            Err(e) => {
                app.status_msg = format!("Could not resolve {origin}: {e}");
                continue;
            }
        };

        match graph.port_index(&origin) {
            None => {
                let id = graph.add_port(&facts, sys_opts);
                arrived.push(origin.clone());

                // Seeded from the saved configuration but answered as it
                // ships — and a dependency behind `${opt}_USES` or an `.if`
                // block exists only under the options in force. Where the two
                // differ the port is asked again, exactly; `resettle` and
                // `load_ports` make the same second ask.
                let in_force = graph.option_set(id);
                let mut sorted_in_force = in_force.clone();
                sorted_in_force.sort();
                let mut shipped: Vec<String> = facts
                    .options
                    .iter()
                    .filter(|o| o.default_on)
                    .map(|o| o.name.clone())
                    .collect();
                shipped.sort();
                if sorted_in_force != shipped {
                    let question = (origin.clone(), Options::Exactly(in_force));
                    if !app.pending.contains(&question) {
                        app.pending.push(question);
                    }
                }
            }
            Some(port_id) => match &asked {
                // A duplicate as-shipped ask for a port that has since been
                // added, and possibly touched; the first answer did the work.
                Options::AsShipped => continue,
                // Superseded: the toggle that changed the set queued a fresh
                // question, so this answer describes options no longer set.
                Options::Exactly(set) if graph.option_set(port_id) != *set => continue,
                Options::Exactly(_) => {}
            },
        }

        for unknown in graph.apply_resolution(&origin, &facts) {
            // Asked as the port ships, because nothing is known about its
            // options yet — what they default to is part of the answer.
            let question = (unknown, Options::AsShipped);
            if !app.pending.contains(&question) {
                app.pending.push(question);
            }
        }
    }

    graph.settle_batch();
    graph.rebuild_visible_rows();
    arrived
}

pub fn run_tui(
    graph: &mut DependencyGraph,
    config_path: Option<PathBuf>,
    saver: Saver,
) -> Result<TuiAction> {
    run_tui_with(
        graph,
        config_path,
        saver,
        None,
        None,
        &SystemOptions::default(),
    )
}

/// Whether a key event is one the interface should act on.
///
/// Terminals speaking the kitty keyboard protocol, and the Windows console,
/// report key releases as events of their own; acting on those runs every
/// handler twice per press — Space toggling an option straight back off, or a
/// quit confirmation answered by the release of the key that opened it.
/// Repeats stay: a held arrow key has to keep moving.
fn key_acts(kind: KeyEventKind) -> bool {
    kind != KeyEventKind::Release
}

/// Puts the terminal into the interface's mode — raw, on the alternate screen —
/// and guarantees it comes back out: on return, on any `?`, and on panic.
///
/// Restoration has to be unconditional, because every failure between setup and
/// teardown otherwise leaves the shell in raw mode on the alternate screen,
/// where even the message explaining the failure is invisible.
struct TerminalGuard;

impl TerminalGuard {
    fn new() -> Result<Self> {
        enable_raw_mode()?;
        // Bracketed paste makes a paste arrive as one Event::Paste instead of
        // a burst of keystrokes — without it, pasted text containing a hotkey
        // letter acts on the list, and pasting `s` anywhere used to write the
        // options out.
        execute!(stdout(), EnterAlternateScreen, EnableBracketedPaste)?;

        // Restore before the default hook prints, so the panic message lands on
        // the main screen in cooked mode instead of vanishing with the
        // alternate screen. The guard's own Drop runs again while unwinding;
        // restore() is idempotent, so the double call is harmless.
        let prev = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            TerminalGuard::restore();
            prev(info);
        }));

        Ok(Self)
    }

    /// Every step is safe to repeat and safe to run when setup half-failed:
    /// leaving the main screen and disabling raw mode on a cooked terminal are
    /// both no-ops. Errors are dropped — there is no better terminal to report
    /// them on. Deliberately no once-flag: one would wrongly suppress the
    /// restore if the interface were ever entered twice in one process.
    fn restore() {
        let _ = execute!(stdout(), DisableBracketedPaste, LeaveAlternateScreen, Show);
        let _ = disable_raw_mode();
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        Self::restore();
    }
}

/// As [`run_tui`], with an oracle to answer questions raised while it runs.
///
/// Without one the interface still works and still writes; it simply cannot
/// discover a dependency that only exists once an option is turned on. That is
/// the shape a session with no readable ports tree takes.
pub fn run_tui_with(
    graph: &mut DependencyGraph,
    config_path: Option<PathBuf>,
    saver: Saver,
    settle: Option<Settle>,
    oracle: Option<Oracle>,
    sys_opts: &SystemOptions,
) -> Result<TuiAction> {
    let resolver = oracle.map(Resolver::spawn);

    let guard = TerminalGuard::new()?;
    let backend = CrosstermBackend::new(stdout());
    let mut terminal = Terminal::new(backend)?;

    // The alternate screen keeps whatever was last on it, and the first draw
    // only paints cells that differ from ratatui's own buffer — which starts
    // empty, so anything already on screen matches "empty" and is left alone.
    // Without this the previous contents show through until something forces a
    // full repaint, which is why starting up used to need a manual Ctrl + L.
    terminal.clear()?;

    let mut app = App::with_saver(config_path, saver);
    app.settle = settle;
    let action;

    loop {
        terminal.draw(|f| render(f, graph, &mut app))?;

        // Post whatever the last keystroke raised. Batched rather than sent one
        // at a time so a round runs in parallel across cores.
        if let Some(resolver) = &resolver {
            if !app.pending.is_empty() && !app.resolver_dead {
                let batch = std::mem::take(&mut app.pending);
                let posted = batch.len();
                // Counted only once the send succeeds: incrementing before a
                // send into a dead worker left the count stuck above zero
                // forever, pinning the header to "resolving" and hiding every
                // message after it.
                if resolver.tx.send(batch).is_ok() {
                    app.outstanding += posted;
                } else {
                    app.mark_resolver_dead();
                }
            }

            // Answers are collected here rather than waited for, so the poll
            // above keeps the interface responsive while make runs.
            loop {
                match resolver.rx.try_recv() {
                    Ok(answers) => {
                        app.outstanding = app.outstanding.saturating_sub(answers.len());
                        let anchor = app
                            .list_state
                            .selected()
                            .and_then(|row| graph.anchor_at(row));
                        let arrived = merge(graph, &mut app, sys_opts, answers);
                        app.restore_anchor(graph, anchor);

                        if !arrived.is_empty() {
                            app.status_msg = match arrived.len() {
                                1 => format!("{} pulled in", arrived[0]),
                                n => format!("{} and {} more pulled in", arrived[0], n - 1),
                            };
                        }
                    }
                    Err(mpsc::TryRecvError::Empty) => break,
                    Err(mpsc::TryRecvError::Disconnected) => {
                        if !app.resolver_dead {
                            app.mark_resolver_dead();
                        }
                        break;
                    }
                }
            }
        } else {
            app.pending.clear();
        }

        if event::poll(std::time::Duration::from_millis(50))? {
            match event::read()? {
                Event::Key(key) if key_acts(key.kind) => match app.on_key(key, graph) {
                    KeyOutcome::Continue => {}
                    KeyOutcome::Redraw => terminal.clear()?,
                    KeyOutcome::SaveInPlace => {
                        // One frame of feedback before make blocks the
                        // loop: settling can take seconds, and a frozen
                        // interface with no message reads as a hang.
                        app.important_msg = Some(String::from("Settling and saving..."));
                        terminal.draw(|f| render(f, graph, &mut app))?;
                        app.perform_save(graph);
                    }
                    KeyOutcome::Finish(finished) => {
                        action = finished;
                        break;
                    }
                },
                Event::Paste(text) => app.on_paste(&text, graph),
                _ => {}
            }
        }
    }

    // The guard leaves the alternate screen here, so the wipe below paints the
    // main screen and not the interface's.
    drop(guard);

    // Leaving the alternate screen puts back whatever the shell had on screen
    // before bgone started, with the cursor dropped wherever that left it —
    // part-way along a line if the last thing printed there did not end in one.
    // What comes next, this program's own output or the prompt, then starts
    // mid-line against a screenful of unrelated scrollback.
    //
    // So the visible screen is wiped to the terminal's own background and the
    // cursor put at the top of it, colours reset first so a style left over
    // from the interface cannot tint what is painted. Only the visible screen:
    // the scrollback is the user's history, not this program's to clear. The
    // newline after that is what separates the summary from the top edge.
    let mut out = std::io::stdout();
    execute!(out, ResetColor, ClearScreen(ClearType::All), MoveTo(0, 0))?;
    writeln!(out)?;
    out.flush()?;

    Ok(action)
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::{backend::TestBackend, Terminal};

    use crate::graph::NodeId;
    use crossterm::event::KeyEventState;

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

    /// A port as make would have described it.
    ///
    /// These tests are about key handling rather than resolution, so they build
    /// facts directly instead of standing up a ports tree and a `make` to read
    /// it with. Resolution itself is covered against a stub tree in
    /// `tests/integration_tests.rs`.
    fn fixture_port(origin: &str) -> PortFacts {
        let name = origin.split('/').nth(1).unwrap_or(origin);
        PortFacts {
            origin: origin.to_string(),
            pkgname: format!("{name}-1.0"),
            pkgbase: name.to_string(),
            flavours: Vec::new(),
            options: Vec::new(),
            deps: Vec::new(),
            unresolved: Vec::new(),
            source_mtime: 0,
        }
    }

    fn fixture_option(
        facts: &mut PortFacts,
        name: &str,
        default_on: bool,
        group_type: &str,
        group_name: &str,
    ) {
        facts.options.push(crate::resolve::OptionFacts {
            name: name.to_string(),
            description: String::new(),
            group_type: group_type.to_string(),
            group_name: group_name.to_string(),
            default_on,
            implies: Vec::new(),
            prevents: Vec::new(),
        });
    }

    fn fixture_edge(facts: &mut PortFacts, to: &str, via: Option<&str>) {
        facts.deps.push(crate::resolve::DepEntry {
            origin: to.to_string(),
            flavour: None,
            class: "RUN".to_string(),
            via_option: via.map(str::to_string),
            polarity: crate::resolve::Polarity::On,
        });
    }

    fn built(facts: Vec<PortFacts>, requested: &[&str]) -> DependencyGraph {
        let mut graph = DependencyGraph::from_facts(&facts, requested, &SystemOptions::default());
        graph.expand_all();
        graph
    }

    /// Two ports sharing an option, so the list has something to put a cursor on.
    fn test_graph() -> DependencyGraph {
        let ports: Vec<PortFacts> = ["lang/php83-extensions", "lang/php84-extensions"]
            .iter()
            .map(|origin| {
                let mut p = fixture_port(origin);
                fixture_option(&mut p, "SOAP", false, "DEFINE", "");
                p
            })
            .collect();
        built(ports, &["lang/php83-extensions", "lang/php84-extensions"])
    }

    /// A stale answer — one computed for an option set no longer in force —
    /// must be dropped, not folded in. Its successor is already in flight, and
    /// folding the stale one in would show its dependencies until then.
    #[test]
    fn merge_drops_an_answer_for_options_no_longer_set() {
        let mut p = fixture_port("www/app");
        fixture_option(&mut p, "X", false, "DEFINE", "");
        let mut graph = built(vec![p], &["www/app"]);
        let mut app = App::new(None);
        let sys = SystemOptions::default();

        // Computed while X was on; the user has since turned X back off.
        let mut facts = fixture_port("www/app");
        fixture_option(&mut facts, "X", false, "DEFINE", "");
        fixture_edge(&mut facts, "www/newdep", None);
        let answers = vec![(
            (
                "www/app".to_string(),
                Options::Exactly(vec!["X".to_string()]),
            ),
            Ok(facts),
        )];

        let arrived = merge(&mut graph, &mut app, &sys, answers);
        assert!(arrived.is_empty());
        assert!(
            app.pending.is_empty(),
            "a superseded answer must not queue work for its dependencies"
        );
    }

    /// The gate must not drop legitimate answers: one matching the current
    /// option set applies, and its dependencies get asked about.
    #[test]
    fn merge_applies_an_answer_for_the_options_in_force() {
        let mut p = fixture_port("www/app");
        fixture_option(&mut p, "X", true, "DEFINE", "");
        let mut graph = built(vec![p], &["www/app"]);
        let mut app = App::new(None);
        let sys = SystemOptions::default();

        let mut facts = fixture_port("www/app");
        fixture_option(&mut facts, "X", true, "DEFINE", "");
        fixture_edge(&mut facts, "www/newdep", None);
        let answers = vec![(
            (
                "www/app".to_string(),
                Options::Exactly(vec!["X".to_string()]),
            ),
            Ok(facts),
        )];

        merge(&mut graph, &mut app, &sys, answers);
        assert!(
            app.pending
                .contains(&("www/newdep".to_string(), Options::AsShipped)),
            "the dependency the answer names has to be asked about"
        );
    }

    /// A second as-shipped answer for a port that already arrived did its work
    /// the first time; re-applying it would overwrite state the user may have
    /// touched since.
    #[test]
    fn merge_skips_a_duplicate_as_shipped_answer_for_a_known_port() {
        let mut graph = built(vec![fixture_port("www/app")], &["www/app"]);
        let mut app = App::new(None);
        let sys = SystemOptions::default();

        let mut facts = fixture_port("www/app");
        fixture_edge(&mut facts, "www/newdep", None);
        let answers = vec![(("www/app".to_string(), Options::AsShipped), Ok(facts))];

        merge(&mut graph, &mut app, &sys, answers);
        assert!(app.pending.is_empty());
    }

    /// A port that arrives mid-session with saved options differing from its
    /// defaults is queued to be asked again under the saved set — the second
    /// evaluation, mirrored from load_ports and resettle.
    #[test]
    fn merge_asks_an_arrived_port_under_its_saved_options() {
        let mut graph = built(vec![fixture_port("www/app")], &["www/app"]);
        let mut app = App::new(None);
        let mut sys = SystemOptions::default();
        sys.port_overrides
            .entry("www/b".to_string())
            .or_default()
            .insert("X".to_string(), true);

        let mut facts = fixture_port("www/b");
        fixture_option(&mut facts, "X", false, "DEFINE", "");
        let answers = vec![(("www/b".to_string(), Options::AsShipped), Ok(facts))];

        let arrived = merge(&mut graph, &mut app, &sys, answers);
        assert_eq!(arrived, vec!["www/b".to_string()]);
        assert!(
            app.pending
                .contains(&("www/b".to_string(), Options::Exactly(vec!["X".to_string()]))),
            "saved configuration differs from shipped, so the port must be re-asked"
        );
    }

    /// A toggle that fires implications queues the port's question with the
    /// post-implication set. The staleness gate depends on every mutation's
    /// question describing the state it left behind — an implied option
    /// missing from the question would make the graph drop its own answer.
    #[test]
    fn a_toggled_ports_question_carries_its_implied_options() {
        let mut p = fixture_port("www/app");
        fixture_option(&mut p, "NJS", false, "DEFINE", "");
        fixture_option(&mut p, "STREAM", false, "DEFINE", "");
        p.options[0].implies = vec!["STREAM".to_string()];
        let mut graph = built(vec![p], &["www/app"]);
        let mut app = App::new(None);

        let row = row_of_option(&graph, "www/app", "NJS");
        let touched = graph.toggle_option(row);
        app.ask_about(&graph, &touched);

        let Some((_, Options::Exactly(set))) =
            app.pending.iter().find(|(origin, _)| origin == "www/app")
        else {
            panic!("the toggled port must have a question queued");
        };
        assert!(set.contains(&"NJS".to_string()));
        assert!(
            set.contains(&"STREAM".to_string()),
            "the implied option is part of the state the question describes"
        );
    }

    /// A cascade is said, not silent: a toggle that implications move reports
    /// what else changed on the port, in both directions.
    #[test]
    fn the_status_line_reports_toggle_cascades() {
        let mut p = fixture_port("www/app");
        fixture_option(&mut p, "NJS", false, "DEFINE", "");
        fixture_option(&mut p, "STREAM", false, "DEFINE", "");
        p.options[0].implies = vec!["STREAM".to_string()];
        let mut graph = built(vec![p], &["www/app"]);
        let mut app = App::new(None);

        app.list_state
            .select(Some(row_of_option(&graph, "www/app", "NJS")));
        app.on_key(key(KeyCode::Char(' ')), &mut graph);
        assert!(
            app.status_msg.contains("'NJS' on") && app.status_msg.contains("STREAM on"),
            "the implied option must be named: {:?}",
            app.status_msg
        );

        app.list_state
            .select(Some(row_of_option(&graph, "www/app", "STREAM")));
        app.on_key(key(KeyCode::Char(' ')), &mut graph);
        assert!(
            app.status_msg.contains("'STREAM' off") && app.status_msg.contains("NJS off"),
            "the retired implier must be named: {:?}",
            app.status_msg
        );
    }

    /// A refused press says which rule held it, instead of claiming a toggle
    /// that never happened.
    #[test]
    fn the_status_line_explains_a_refused_toggle() {
        let mut p = fixture_port("www/app");
        fixture_option(&mut p, "MYSQL", true, "SINGLE", "BACKEND");
        fixture_option(&mut p, "PGSQL", false, "SINGLE", "BACKEND");
        fixture_option(&mut p, "SSL", true, "DEFINE", "");
        p.options[0].implies = vec!["SSL".to_string()];
        let mut graph = built(vec![p], &["www/app"]);
        let mut app = App::new(None);

        // The SINGLE's set member refuses directly...
        app.list_state
            .select(Some(row_of_option(&graph, "www/app", "MYSQL")));
        app.on_key(key(KeyCode::Char(' ')), &mut graph);
        assert!(
            app.status_msg.contains("stays on")
                && app.status_msg.contains("SINGLE keeps one member"),
            "got: {:?}",
            app.status_msg
        );

        // ...and so does an option that member implies.
        app.list_state
            .select(Some(row_of_option(&graph, "www/app", "SSL")));
        app.on_key(key(KeyCode::Char(' ')), &mut graph);
        assert!(
            app.status_msg.contains("'SSL' stays on") && app.status_msg.contains("MYSQL"),
            "got: {:?}",
            app.status_msg
        );
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
        let ports: Vec<PortFacts> = ["www/aaa", "www/zzz"]
            .iter()
            .map(|origin| {
                let mut p = fixture_port(origin);
                for opt in ["ALPHA", "BETA", "GAMMA"] {
                    fixture_option(&mut p, opt, false, "DEFINE", "");
                }
                p
            })
            .collect();
        built(ports, &["www/aaa", "www/zzz"])
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
        // EXTRA is on by default and pulls in aaa/pulled, which sorts *above*
        // www/app — so turning it off shifts every row under www/app upwards.
        let mut app_port = fixture_port("www/app");
        fixture_option(&mut app_port, "EXTRA", true, "DEFINE", "");
        fixture_edge(&mut app_port, "aaa/pulled", Some("EXTRA"));
        let mut pulled = fixture_port("aaa/pulled");
        fixture_option(&mut pulled, "DOCS", true, "DEFINE", "");

        let mut graph = built(vec![app_port, pulled], &["www/app"]);
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

    /// Quitting always asks, whether or not anything has changed — Esc and
    /// Ctrl+C are pressed by reflex, and one press must not end the session.
    #[test]
    fn quitting_always_asks_first() {
        let mut graph = test_graph();
        let mut app = App::new(None);

        assert!(!graph.is_dirty());
        assert_eq!(
            app.on_key(key(KeyCode::Char('q')), &mut graph),
            KeyOutcome::Continue
        );
        assert_eq!(app.input_mode, InputMode::ConfirmQuit);

        // Cancel, change something, and ask again
        app.on_key(key(KeyCode::Char('c')), &mut graph);
        assert_eq!(app.input_mode, InputMode::Normal);

        app.list_state.select(Some(first_option_row(&graph)));
        app.on_key(key(KeyCode::Char(' ')), &mut graph);
        assert!(graph.is_dirty());
        assert_eq!(
            app.on_key(key(KeyCode::Char('q')), &mut graph),
            KeyOutcome::Continue
        );
        assert_eq!(app.input_mode, InputMode::ConfirmQuit);

        assert_eq!(
            app.on_key(key(KeyCode::Char('d')), &mut graph),
            KeyOutcome::Finish(TuiAction::QuitWithoutSaving)
        );
    }

    /// The three ways out, each reachable both by its letter and by walking the
    /// buttons to it.
    #[test]
    fn the_leaving_box_offers_save_discard_and_cancel() {
        let mut graph = test_graph();

        let mut ask = |k: KeyEvent| {
            let mut app = App::new(None);
            app.on_key(key(KeyCode::Esc), &mut graph);
            assert_eq!(app.input_mode, InputMode::ConfirmQuit);
            let outcome = app.on_key(k, &mut graph);
            (app.input_mode, outcome)
        };

        assert_eq!(
            ask(key(KeyCode::Char('s'))).1,
            KeyOutcome::Finish(TuiAction::SaveAndQuit)
        );
        assert_eq!(
            ask(key(KeyCode::Char('d'))).1,
            KeyOutcome::Finish(TuiAction::QuitWithoutSaving)
        );
        assert_eq!(ask(key(KeyCode::Char('c'))).0, InputMode::Normal);

        // The box opens on "Save and exit", so Enter saves...
        assert_eq!(
            ask(key(KeyCode::Enter)).1,
            KeyOutcome::Finish(TuiAction::SaveAndQuit)
        );

        // ...and one step right of it discards
        let mut app = App::new(None);
        app.on_key(key(KeyCode::Esc), &mut graph);
        app.on_key(key(KeyCode::Right), &mut graph);
        assert_eq!(app.confirm_choice, ConfirmChoice::Discard);
        assert_eq!(
            app.on_key(key(KeyCode::Enter), &mut graph),
            KeyOutcome::Finish(TuiAction::QuitWithoutSaving)
        );
    }

    /// Asked twice and answered the same way: the second Esc — or Ctrl+C — is
    /// taken as the answer rather than waiting to be pointed at a button.
    #[test]
    fn a_second_escape_discards() {
        for quit in [key(KeyCode::Esc), ctrl('c')] {
            let mut graph = test_graph();
            let mut app = App::new(None);
            app.list_state.select(Some(first_option_row(&graph)));
            app.on_key(key(KeyCode::Char(' ')), &mut graph);

            assert_eq!(app.on_key(quit, &mut graph), KeyOutcome::Continue);
            assert_eq!(app.input_mode, InputMode::ConfirmQuit);
            assert_eq!(
                app.on_key(quit, &mut graph),
                KeyOutcome::Finish(TuiAction::QuitWithoutSaving),
                "a second {quit:?} must not need a button pressed"
            );
        }
    }

    /// Ctrl+S writes without leaving, so a long session is not all riding on
    /// getting out cleanly at the end.
    #[test]
    fn ctrl_s_saves_in_place_and_stays() {
        use std::cell::RefCell;
        use std::rc::Rc;

        let mut graph = test_graph();
        let calls = Rc::new(RefCell::new(0));
        let counted = Rc::clone(&calls);

        let mut app = App::with_saver(
            None,
            Box::new(move |_| {
                *counted.borrow_mut() += 1;
                Ok(String::from("Saved 3 options across 2 files"))
            }),
        );

        assert_eq!(app.on_key(ctrl('s'), &mut graph), KeyOutcome::SaveInPlace);
        app.perform_save(&mut graph);
        assert_eq!(*calls.borrow(), 1, "Ctrl+S must write the options out");
        assert_eq!(app.input_mode, InputMode::Normal);
        assert_eq!(
            app.important_msg.as_deref(),
            Some("Saved 3 options across 2 files"),
            "the result must go where the header cannot hide it"
        );

        // Plain `s` asks first now, with < Save and exit > preselected, so
        // ending the session is never one unguarded letter; Enter confirms.
        assert_eq!(
            app.on_key(key(KeyCode::Char('s')), &mut graph),
            KeyOutcome::Continue
        );
        assert_eq!(app.input_mode, InputMode::ConfirmQuit);
        assert_eq!(app.confirm_choice, ConfirmChoice::Save);
        assert_eq!(
            app.on_key(key(KeyCode::Enter), &mut graph),
            KeyOutcome::Finish(TuiAction::SaveAndQuit)
        );
        assert_eq!(*calls.borrow(), 1, "leaving must not save twice");
    }

    /// `o` matches `s`: the confirmation opens on < Save and exit >.
    #[test]
    fn bare_o_asks_before_saving_and_exiting() {
        let mut graph = test_graph();
        let mut app = App::new(None);

        assert_eq!(
            app.on_key(key(KeyCode::Char('o')), &mut graph),
            KeyOutcome::Continue
        );
        assert_eq!(app.input_mode, InputMode::ConfirmQuit);
        assert_eq!(app.confirm_choice, ConfirmChoice::Save);
    }

    // ------------------------------------------------------------------ paste

    /// Pasting where there is no field must do nothing. Before bracketed
    /// paste, pasted text arrived as keystrokes, and any `s` in it ended the
    /// session and wrote the options files.
    #[test]
    fn pasting_outside_a_field_is_ignored() {
        let mut graph = test_graph();
        let mut app = App::new(None);

        app.on_paste("notes with an s and a q in them", &mut graph);
        assert_eq!(app.input_mode, InputMode::Normal);
    }

    /// A paste into the search bar types its first line — a pasted newline
    /// must not act as Enter — with control characters dropped, and the
    /// filter narrows exactly as if it had been typed.
    #[test]
    fn pasting_into_search_types_the_first_line() {
        let mut graph = test_graph();
        let mut app = App::new(None);
        app.on_key(key(KeyCode::Char('/')), &mut graph);
        assert_eq!(app.input_mode, InputMode::Search);

        app.on_paste("php83\nsecond line", &mut graph);
        assert_eq!(graph.search_query, "php83");
        assert_eq!(app.input_mode, InputMode::Search, "no Enter smuggled in");
        assert!(
            !listed_test_rows(&graph).iter().any(|o| o.contains("php84")),
            "the filter must have narrowed"
        );
    }

    /// The prompt fields take a paste through the same editing seam as typing.
    #[test]
    fn pasting_into_a_prompt_fills_the_buffer() {
        let mut graph = test_graph();
        let mut app = App::new(None);
        app.input_mode = InputMode::GroupNewName;

        app.on_paste("php\tset", &mut graph);
        assert_eq!(app.text_buffer, "phpset", "control characters are dropped");
    }

    /// Origins of the ports visible in the current row list.
    fn listed_test_rows(graph: &DependencyGraph) -> Vec<String> {
        graph
            .visible_rows
            .iter()
            .filter_map(|r| match &r.kind {
                RowKind::Port { origin, .. } => Some(origin.clone()),
                _ => None,
            })
            .collect()
    }

    /// A failing write is reported rather than swallowed or fatal.
    #[test]
    fn a_failed_save_says_so_and_carries_on() {
        let mut graph = test_graph();
        let mut app = App::with_saver(
            None,
            Box::new(|_| Err(anyhow::anyhow!("read-only file system"))),
        );

        assert_eq!(app.on_key(ctrl('s'), &mut graph), KeyOutcome::SaveInPlace);
        app.perform_save(&mut graph);
        assert!(
            app.important_msg
                .as_deref()
                .unwrap_or("")
                .contains("read-only file system"),
            "message said {:?}",
            app.important_msg
        );
    }

    /// The save result has to be readable even while questions are still out.
    /// This was the field report: Ctrl+S during a resolve showed only
    /// "resolving N port(s)...", and the saved message never appeared.
    #[test]
    fn the_save_message_shows_even_while_resolving() {
        // A short origin keeps the header's target brief: the 90-column frame
        // truncates from the right, and what matters here is precedence, not
        // packing.
        let graph = built(vec![fixture_port("www/a")], &["www/a"]);
        let mut app = App::new(None);
        app.outstanding = 2;
        app.important_msg = Some(String::from("Saved 3 options across 2 files"));

        let out = draw(|f| render(f, &graph, &mut app));
        assert!(
            out.contains("Saved 3 options across 2 files"),
            "got:\n{out}"
        );
        assert!(out.contains("resolving 2 port(s)"), "got:\n{out}");
    }

    /// A dead worker zeroes the outstanding count — left non-zero it pinned
    /// the header to "resolving" forever, hiding every message behind it —
    /// and the header says the resolver stopped.
    #[test]
    fn a_dead_resolver_unpins_the_header() {
        let graph = test_graph();
        let mut app = App::new(None);
        app.outstanding = 3;
        app.pending
            .push(("www/gone".to_string(), Options::AsShipped));

        app.mark_resolver_dead();
        assert_eq!(app.outstanding, 0, "nothing will ever answer these");
        assert!(app.pending.is_empty(), "there is nothing left to ask");

        let out = draw(|f| render(f, &graph, &mut app));
        assert!(out.contains("[resolver stopped]"), "got:\n{out}");
    }

    /// Being in a group changes what a keystroke does — Space here also moves
    /// every other member — so it has to be visible on the row itself.
    #[test]
    fn a_grouped_port_is_labelled_on_its_row() {
        let mut graph = test_graph();
        graph.groups = groups_of(&[("php", &["lang/php83-extensions"])]);

        let mut app = App::new(None);
        let screen = draw(|f| render(f, &graph, &mut app));

        assert!(
            screen.contains("lang/php83-extensions  [php]"),
            "a member must say which group it is in:\n{screen}"
        );
        assert!(
            !screen.contains("lang/php84-extensions  [php]"),
            "a port that is not in the group must not be labelled:\n{screen}"
        );
    }

    /// Shift + H takes the ports with nothing to decide out of the way, and
    /// leaves the cursor on what it was reading.
    #[test]
    fn shift_h_hides_the_ports_with_no_options() {
        let mut app_port = fixture_port("www/app");
        fixture_option(&mut app_port, "SSL", false, "DEFINE", "");
        fixture_edge(&mut app_port, "devel/leaf", None);

        let mut graph = built(vec![app_port, fixture_port("devel/leaf")], &["www/app"]);

        let mut app = App::new(None);
        app.list_state.select(Some(row_of_port(&graph, "www/app")));

        let shift_h = KeyEvent {
            code: KeyCode::Char('H'),
            modifiers: KeyModifiers::SHIFT,
            kind: KeyEventKind::Press,
            state: KeyEventState::NONE,
        };

        app.on_key(shift_h, &mut graph);
        assert!(graph.hide_optionless);
        assert!(
            port_row_index(&graph, "devel/leaf").is_none(),
            "the leaf has nothing to decide and should be out of the way"
        );
        assert!(port_row_index(&graph, "www/app").is_some());
        assert_eq!(
            under_cursor(&graph, &app),
            "port www/app",
            "hiding must not lose the row being read"
        );

        app.on_key(shift_h, &mut graph);
        assert!(!graph.hide_optionless);
        assert!(port_row_index(&graph, "devel/leaf").is_some());
    }

    /// A dialog listing more groups than fit on the screen scrolled nowhere:
    /// the cursor could be moved onto a row that was never drawn.
    #[test]
    fn the_window_follows_the_cursor_off_the_bottom() {
        // Everything fits: no scrolling, ever
        assert_eq!(scroll_window(3, 0, 10), 0);
        assert_eq!(scroll_window(3, 2, 10), 0);

        // It only moves once the cursor would leave the window...
        assert_eq!(scroll_window(20, 0, 5), 0);
        assert_eq!(scroll_window(20, 4, 5), 0);
        assert_eq!(scroll_window(20, 5, 5), 1);

        // ...and stops at the end rather than scrolling past it
        assert_eq!(scroll_window(20, 19, 5), 15);
        assert_eq!(scroll_window(20, 100, 5), 15);
    }

    /// The window is exactly as tall as the room it was given: a marker drawn
    /// over an edge row would hide the entry just moved onto.
    #[test]
    fn a_scrolled_dialog_says_where_in_the_list_it_is() {
        let lines: Vec<Line<'static>> = (0..20).map(|i| Line::from(format!("row {i}"))).collect();

        let (shown, note) = windowed_lines(lines.clone(), 12, 5);
        assert_eq!(shown.len(), 5);
        assert_eq!(note.as_deref(), Some("  showing 9-13 of 20"));

        let (shown, note) = windowed_lines(lines[..3].to_vec(), 0, 5);
        assert_eq!(shown.len(), 3);
        assert_eq!(note, None, "nothing to say when it all fits");
    }

    /// A dialog with more groups than rows still draws the one under the
    /// cursor, wherever in the list it is.
    #[test]
    fn the_group_dialog_draws_the_row_the_cursor_is_on() {
        let names: Vec<String> = (0..40).map(|i| format!("group{i:02}")).collect();
        let groups: BTreeMap<String, Vec<String>> = names
            .iter()
            .map(|n| (n.clone(), vec!["www/app".to_string()]))
            .collect();

        let screen = draw(|f| render_group_assign(f, "www/app", &groups, 39));
        assert!(
            screen.contains("group39"),
            "the row under the cursor was never drawn:\n{screen}"
        );
        assert!(screen.contains("showing "), "and it must say where it is");
    }

    /// Keeping the row still is only half of it. A change *above* the cursor
    /// renumbers it, and the list widget then scrolls by exactly enough to
    /// bring the new number into view — parking the row against the bottom
    /// edge, several screens from where the eye already was.
    #[test]
    fn the_row_stays_where_it_is_on_the_screen() {
        // Every one of these sorts before www/zzz, so opening them renumbers it
        let mut ports = Vec::new();
        let origins: Vec<String> = (0..6).map(|i| format!("www/a{i}")).collect();
        for origin in &origins {
            let mut p = fixture_port(origin);
            for opt in ["ONE", "TWO", "THREE"] {
                fixture_option(&mut p, opt, false, "DEFINE", "");
            }
            ports.push(p);
        }
        let mut zzz = fixture_port("www/zzz");
        fixture_option(&mut zzz, "ZED", false, "DEFINE", "");
        ports.push(zzz);

        let mut targets: Vec<&str> = origins.iter().map(|s| s.as_str()).collect();
        targets.push("www/zzz");
        let mut graph = built(ports, &targets);
        graph.collapse_all();

        let mut app = App::new(None);
        app.viewport_height = 10;
        // Reading www/zzz, five rows down the screen
        let zzz = row_of_port(&graph, "www/zzz");
        app.list_state.select(Some(zzz));
        *app.list_state.offset_mut() = zzz.saturating_sub(5);
        assert_eq!(
            app.list_state.selected().unwrap() - app.list_state.offset(),
            5
        );

        // Open everything: www/aaa gains six option rows above the cursor
        app.on_key(key(KeyCode::Char('+')), &mut graph);
        app.on_key(key(KeyCode::Char('+')), &mut graph);

        assert_eq!(
            under_cursor(&graph, &app),
            "port www/zzz",
            "the cursor must still be on the port it was reading"
        );
        let moved = row_of_port(&graph, "www/zzz");
        assert!(moved > zzz, "the row number must have moved");
        assert_eq!(
            app.list_state.selected().unwrap() - app.list_state.offset(),
            5,
            "and it must still be five rows down the screen"
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
        let mut app_port = fixture_port("www/app");
        fixture_option(&mut app_port, "DOCS", true, "DEFINE", "");
        fixture_edge(&mut app_port, "devel/lib", None);
        let mut lib = fixture_port("devel/lib");
        fixture_option(&mut lib, "DOCS", true, "DEFINE", "");
        built(vec![app_port, lib], &["www/app"])
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

    /// The guard restores from Drop and again from the panic hook, so restoring
    /// twice — and outside the interface's mode altogether — has to be safe.
    #[test]
    fn restoring_the_terminal_twice_is_harmless() {
        TerminalGuard::restore();
        TerminalGuard::restore();
    }

    // ------------------------------------------------------- sequence fuzzing
    //
    // Tier B of the simulated-user stage (tests/sim/ is tier A): seeded random
    // key and paste sequences over the whole vocabulary, interleaved with
    // synthesized resolver answers — stale ones included — and the
    // dead-resolver nemesis. It lives in this module because on_key and merge
    // are deliberately private; the render smoke runs on a TestBackend. The
    // seed prints in every failure; replay one with BGONE_UI_SIM_SEED.

    /// The same xorshift32 the tier A engine uses; duplicated because tests/
    /// cannot be imported from here, and eight lines are cheaper than a
    /// visibility hole.
    struct FuzzRng(u32);

    impl FuzzRng {
        fn next(&mut self) -> u32 {
            let mut x = self.0;
            x ^= x << 13;
            x ^= x >> 17;
            x ^= x << 5;
            self.0 = x;
            x
        }

        fn below(&mut self, n: usize) -> usize {
            (self.next() as usize) % n.max(1)
        }
    }

    /// Ports with a SINGLE group, an implication, a shared dependency and an
    /// option-conditional edge — enough structure for cascades, jumps and
    /// arrivals to mean something.
    fn fuzz_graph() -> DependencyGraph {
        let mut a = fixture_port("www/alpha");
        fixture_option(&mut a, "MYSQL", true, "SINGLE", "BACKEND");
        fixture_option(&mut a, "PGSQL", false, "SINGLE", "BACKEND");
        fixture_option(&mut a, "NJS", false, "DEFINE", "");
        fixture_option(&mut a, "STREAM", false, "DEFINE", "");
        a.options[2].implies = vec!["STREAM".to_string()];
        fixture_edge(&mut a, "devel/shared", Some("NJS"));
        let mut b = fixture_port("databases/beta");
        fixture_option(&mut b, "DOCS", true, "DEFINE", "");
        fixture_edge(&mut b, "devel/shared", None);
        let s = fixture_port("devel/shared");
        built(vec![a, b, s], &["www/alpha", "databases/beta"])
    }

    /// The fuzz App carries a config path in a temp directory: with none, the
    /// group manager's save opens the path prompt, and the fuzzer's random
    /// typing plus Enter wrote group files to relative paths — straight into
    /// the working tree on the first landing of this test.
    fn fuzz_app(seed: u32) -> App {
        let dir = std::env::temp_dir().join(format!("bgone_ui_fuzz_{}_{seed}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        App::with_saver(
            Some(dir.join("bgone.toml")),
            Box::new(|_| Ok(String::from("Saved"))),
        )
    }

    fn fuzz_event(r: &mut FuzzRng) -> KeyEvent {
        let plain = [
            KeyCode::Char(' '),
            KeyCode::Enter,
            KeyCode::Tab,
            KeyCode::BackTab,
            KeyCode::Esc,
            KeyCode::Up,
            KeyCode::Down,
            KeyCode::PageUp,
            KeyCode::PageDown,
            KeyCode::Home,
            KeyCode::End,
            KeyCode::Backspace,
            KeyCode::Delete,
            KeyCode::Char('q'),
            KeyCode::Char('c'),
            KeyCode::Char('o'),
            KeyCode::Char('s'),
            KeyCode::Char('d'),
            KeyCode::Char('/'),
            KeyCode::Char('='),
            KeyCode::Char('-'),
            KeyCode::Char('+'),
            KeyCode::Char('_'),
            KeyCode::Char('H'),
            KeyCode::Char('x'),
            KeyCode::Char('ü'),
            KeyCode::F(5),
            KeyCode::Null,
            KeyCode::Insert,
        ];
        match r.below(10) {
            0 | 1 => ctrl(['s', 'g', 'l', 'r', 'c', 'f', 'h', 'a'][r.below(8)]),
            2 => KeyEvent {
                code: KeyCode::Char('H'),
                modifiers: KeyModifiers::SHIFT,
                kind: KeyEventKind::Press,
                state: KeyEventState::NONE,
            },
            _ => key(plain[r.below(plain.len())]),
        }
    }

    /// SINGLE holds exactly one member, RADIO at most one — checked directly
    /// on the graph, since tier B has no model.
    fn single_groups_hold(graph: &DependencyGraph) -> Result<(), String> {
        let mut counts: BTreeMap<(String, String, String), usize> = BTreeMap::new();
        for o in graph.real_options() {
            if o.group_type == "SINGLE" || o.group_type == "RADIO" {
                let key = (
                    o.port_origin.clone(),
                    o.group_type.clone(),
                    o.group_name.clone(),
                );
                *counts.entry(key).or_default() += usize::from(o.enabled);
            }
        }
        for ((port, ty, group), set) in counts {
            if (ty == "SINGLE" && set != 1) || (ty == "RADIO" && set > 1) {
                return Err(format!("{ty} {group} on {port} holds {set} set members"));
            }
        }
        Ok(())
    }

    fn states_snapshot(graph: &DependencyGraph) -> Vec<(String, String, bool)> {
        graph
            .real_options()
            .map(|o| (o.port_origin.clone(), o.name.clone(), o.enabled))
            .collect()
    }

    fn ui_fuzz(seed: u32, steps: usize) {
        let mut rng = FuzzRng(if seed == 0 { 0x9e37_79b9 } else { seed });
        let mut graph = fuzz_graph();
        let mut app = fuzz_app(seed);
        let sys = SystemOptions::default();
        let mut resolver_dead = false;
        let mut arrivals = 0usize;

        for step in 0..steps {
            let check = |what: &str, result: Result<(), String>| {
                if let Err(e) = result {
                    panic!("ui fuzz seed {seed} step {step} ({what}): {e}");
                }
            };

            match rng.below(12) {
                // Synthesized resolver answers, the way the event loop folds
                // them in.
                9 if !resolver_dead => {
                    match rng.below(3) {
                        // A stale answer: computed for an option set that is
                        // not the port's current one. Folding it in must
                        // change nothing (the e6ea64f property).
                        0 => {
                            let mut facts = fixture_port("www/alpha");
                            fixture_option(&mut facts, "GHOST", true, "DEFINE", "");
                            fixture_edge(&mut facts, "devel/ghost-dep", None);
                            let before = states_snapshot(&graph);
                            let answers = vec![(
                                (
                                    "www/alpha".to_string(),
                                    Options::Exactly(vec!["GHOST".to_string()]),
                                ),
                                Ok(facts),
                            )];
                            merge(&mut graph, &mut app, &sys, answers);
                            if states_snapshot(&graph) != before {
                                panic!(
                                    "ui fuzz seed {seed} step {step}: a stale answer moved state"
                                );
                            }
                        }
                        // A fresh arrival, cascading like a real discovery.
                        1 => {
                            arrivals += 1;
                            let origin = format!("devel/arrival{arrivals}");
                            let mut facts = fixture_port(&origin);
                            fixture_option(&mut facts, "DOCS", true, "DEFINE", "");
                            let answers = vec![((origin, Options::AsShipped), Ok(facts))];
                            merge(&mut graph, &mut app, &sys, answers);
                        }
                        // A failure answer: reported, never fatal.
                        _ => {
                            let answers = vec![(
                                ("www/alpha".to_string(), Options::AsShipped),
                                Err("make exploded".to_string()),
                            )];
                            merge(&mut graph, &mut app, &sys, answers);
                        }
                    }
                }
                // The dead-resolver nemesis: the counter zeroes and nothing
                // stays pinned (the 93acc9e property).
                10 if !resolver_dead && rng.below(8) == 0 => {
                    app.outstanding += 3; // as if questions were out
                    app.mark_resolver_dead();
                    resolver_dead = true;
                    assert_eq!(app.outstanding, 0, "a dead resolver must unpin");
                    assert!(app.pending.is_empty());
                }
                // Paste, hostile shapes included (the 057940c/9109fe8 class).
                11 => {
                    let payloads = [
                        "grüße",
                        "s q o c",
                        "php83\nrm -rf /\n",
                        "tab\there",
                        "",
                        "ß",
                    ];
                    app.on_paste(payloads[rng.below(payloads.len())], &mut graph);
                }
                _ => {
                    let event = fuzz_event(&mut rng);
                    match app.on_key(event, &mut graph) {
                        KeyOutcome::SaveInPlace => app.perform_save(&mut graph),
                        KeyOutcome::Finish(_) => {
                            // The session ended; a new one starts, as the
                            // binary would.
                            app = fuzz_app(seed);
                        }
                        KeyOutcome::Continue | KeyOutcome::Redraw => {}
                    }
                }
            }

            check("groups", single_groups_hold(&graph));
            for (i, q) in app.pending.iter().enumerate() {
                if app.pending[i + 1..].contains(q) {
                    panic!("ui fuzz seed {seed} step {step}: duplicate pending question {q:?}");
                }
            }
            if app.pending.len() > 64 {
                panic!(
                    "ui fuzz seed {seed} step {step}: pending grew to {} — a queue that only grows is the loop signature",
                    app.pending.len()
                );
            }

            if step % 7 == 0 {
                // The render smoke: every frame must draw without panicking,
                // whatever the state machine got itself into.
                let _ = draw(|f| render(f, &graph, &mut app));
            }
        }
    }

    /// The always-on tier B runs. Two seeds, a few hundred steps each; no
    /// processes are spawned, so this stays well under a second.
    #[test]
    fn ui_fuzz_fixed_seeds_stay_green() {
        ui_fuzz(5, 400);
        ui_fuzz(6, 400);
    }

    /// Hunting mode for this tier: BGONE_UI_SIM_SEED replays exactly one
    /// seed, BGONE_SIM_ACTIONS scales it.
    #[test]
    fn ui_fuzz_hunting_mode_when_requested() {
        let Some(seed) = std::env::var("BGONE_UI_SIM_SEED")
            .ok()
            .and_then(|s| s.parse::<u32>().ok())
        else {
            return;
        };
        let steps = std::env::var("BGONE_SIM_ACTIONS")
            .ok()
            .and_then(|s| s.parse::<usize>().ok())
            .unwrap_or(5000);
        println!("ui fuzz hunting: seed {seed}, {steps} steps");
        ui_fuzz(seed, steps);
    }

    /// Presses and repeats act; releases do not. A terminal reporting releases
    /// would otherwise run every handler twice per keystroke.
    #[test]
    fn only_key_releases_are_ignored() {
        assert!(key_acts(KeyEventKind::Press));
        assert!(key_acts(KeyEventKind::Repeat));
        assert!(!key_acts(KeyEventKind::Release));
    }
}
