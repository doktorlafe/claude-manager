//! The interactive overview: sessions sorted by how much they need the user.

use std::collections::HashMap;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread;
use std::time::{Duration, Instant, SystemTime};

use ratatui::crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Block, List, ListState, Paragraph, Wrap};
use ratatui::{DefaultTerminal, Frame};

use crate::haiku::{self, Kind, Verdict};
use crate::sessions::{self, Session, Status, Turn};
use crate::tmux::{self, Pane};
use crate::{home, now_ms};

const REFRESH: Duration = Duration::from_secs(1);
/// How often the transcript of a working session is reread.
const BUSY_REREAD: Duration = Duration::from_secs(5);
/// How many chats Haiku reads at once.
const MAX_RUNNING: usize = 3;

/// What a session needs, most urgent first; the list is sorted by this.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum State {
    Confirm,
    Question,
    Failed,
    Done,
    /// Has a final answer that Haiku has not classified (yet).
    Unread,
    Answer,
    Empty,
    Busy,
}

impl State {
    fn badge(self) -> (&'static str, Color) {
        match self {
            State::Confirm => ("POTVRDIT", Color::Red),
            State::Question => ("OTÁZKA", Color::Yellow),
            State::Failed => ("PROBLÉM", Color::Magenta),
            State::Done => ("HOTOVO", Color::Green),
            State::Unread => ("DOPSÁNO", Color::Green),
            State::Answer => ("ODPOVĚĎ", Color::Cyan),
            State::Empty => ("NOVÁ", Color::DarkGray),
            State::Busy => ("PRACUJE", Color::Blue),
        }
    }

    fn needs_user(self) -> bool {
        self < State::Answer
    }
}

#[derive(Default)]
enum Analysis {
    #[default]
    None,
    Pending,
    Ready(Verdict),
    Failed(String),
}

/// What we know about a session beyond its status file.
#[derive(Default)]
struct Info {
    /// Transcript mtime and size the turn was read at.
    stamp: Option<(SystemTime, u64)>,
    read_at: Option<Instant>,
    turn: Turn,
    analysis: Analysis,
}

struct Row {
    s: Session,
    pane: Option<Pane>,
}

/// A finished Haiku run, sent back from its thread.
struct Finished {
    id: String,
    key: String,
    result: Result<Verdict, String>,
}

/// Directory picker for a new session. Row 0 stands for the current directory itself.
struct Browser {
    dir: PathBuf,
    entries: Vec<String>,
    sel: usize,
    hidden: bool,
    error: Option<String>,
}

impl Browser {
    fn new(dir: PathBuf) -> Self {
        let mut b = Browser {
            dir,
            entries: Vec::new(),
            sel: 0,
            hidden: false,
            error: None,
        };
        b.load();
        b
    }

    fn load(&mut self) {
        self.entries.clear();
        self.error = None;
        match fs::read_dir(&self.dir) {
            Ok(rd) => {
                for entry in rd.flatten() {
                    let name = entry.file_name().to_string_lossy().into_owned();
                    if (name.starts_with('.') && !self.hidden) || !entry.path().is_dir() {
                        continue;
                    }
                    self.entries.push(name);
                }
                self.entries.sort();
            }
            Err(e) => self.error = Some(e.to_string()),
        }
        self.sel = self.sel.min(self.entries.len());
    }

    fn enter(&mut self, dir: PathBuf) {
        self.dir = dir;
        self.sel = 0;
        self.load();
    }

    fn parent(&mut self) {
        let Some(parent) = self.dir.parent().map(Path::to_path_buf) else {
            return;
        };
        let came_from = self
            .dir
            .file_name()
            .map(|n| n.to_string_lossy().into_owned());
        self.enter(parent);
        if let Some(pos) = self
            .entries
            .iter()
            .position(|e| Some(e) == came_from.as_ref())
        {
            self.sel = pos + 1;
        }
    }

    /// The highlighted subdirectory, or `None` on the "this directory" row.
    fn current(&self) -> Option<PathBuf> {
        let name = self.sel.checked_sub(1).and_then(|i| self.entries.get(i))?;
        Some(self.dir.join(name))
    }
}

enum Mode {
    Main,
    Browse(Browser),
}

enum Action {
    None,
    Quit,
}

struct App {
    once: bool,
    has_tmux: bool,
    rows: Vec<Row>,
    infos: HashMap<String, Info>,
    sel: usize,
    mode: Mode,
    /// Message and whether it is an error.
    status: Option<(String, bool)>,
    running: usize,
    tx: Sender<Finished>,
    rx: Receiver<Finished>,
}

fn state_of(infos: &HashMap<String, Info>, s: &Session) -> State {
    match s.status {
        Status::Waiting(_) => return State::Confirm,
        Status::Busy => return State::Busy,
        Status::Idle => {}
    }
    match infos.get(&s.id) {
        Some(info) if !info.turn.key.is_empty() => match &info.analysis {
            Analysis::Ready(v) => match v.kind {
                Kind::Question => State::Question,
                Kind::Failed => State::Failed,
                Kind::Done => State::Done,
                Kind::Answer => State::Answer,
            },
            _ => State::Unread,
        },
        _ => State::Empty,
    }
}

/// Collapses whitespace and cuts to `max` characters.
fn one_line(text: &str, max: usize) -> String {
    let flat = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if flat.chars().count() <= max {
        return flat;
    }
    let cut: String = flat.chars().take(max.saturating_sub(1)).collect();
    format!("{cut}…")
}

fn age(since_ms: u64) -> String {
    let secs = now_ms().saturating_sub(since_ms) / 1000;
    match secs {
        0..60 => "teď".into(),
        60..3600 => format!("{} min", secs / 60),
        3600..86400 => format!("{} h", secs / 3600),
        _ => format!("{} d", secs / 86400),
    }
}

/// The last lines of `text` that fit `rows` rows of `width` columns when wrapped.
fn tail_lines(text: &str, width: usize, rows: usize) -> Vec<&str> {
    let mut used = 0;
    let mut lines: Vec<&str> = text
        .lines()
        .rev()
        .take_while(|line| {
            used += line.chars().count().div_ceil(width.max(1)).max(1);
            used <= rows
        })
        .collect();
    lines.reverse();
    lines
}

impl App {
    fn title<'a>(&'a self, s: &'a Session) -> &'a str {
        let title = self.infos.get(&s.id).and_then(|i| i.turn.title.as_deref());
        title.unwrap_or(if s.name.is_empty() {
            "(bez názvu)"
        } else {
            &s.name
        })
    }

    fn sort(&mut self) {
        let keep = self.rows.get(self.sel).map(|r| r.s.id.clone());
        let infos = &self.infos;
        // Among equally urgent sessions, the one waiting longest goes first.
        self.rows
            .sort_by_key(|r| (state_of(infos, &r.s), r.s.since_ms));
        // The cursor follows its session as the order changes.
        self.sel = keep
            .and_then(|id| self.rows.iter().position(|r| r.s.id == id))
            .unwrap_or(self.sel)
            .min(self.rows.len().saturating_sub(1));
    }

    fn refresh(&mut self) {
        let panes = if self.has_tmux {
            tmux::panes()
        } else {
            Vec::new()
        };
        let keep = self.rows.get(self.sel).map(|r| r.s.id.clone());
        self.rows = sessions::load()
            .into_iter()
            .map(|s| Row {
                pane: tmux::pane_of(s.pid, &panes).cloned(),
                s,
            })
            .collect();
        if let Some(pos) = keep.and_then(|id| self.rows.iter().position(|r| r.s.id == id)) {
            self.sel = pos;
        }
        let rows = &self.rows;
        self.infos
            .retain(|id, _| rows.iter().any(|r| r.s.id == *id));
        for i in 0..self.rows.len() {
            self.update_info(i);
        }
        self.sort();
    }

    /// Rereads the transcript when it changed and has Haiku classify a new final answer.
    fn update_info(&mut self, row: usize) {
        let s = &self.rows[row].s;
        let busy = matches!(s.status, Status::Busy);
        let info = self.infos.entry(s.id.clone()).or_default();
        let stamp = fs::metadata(&s.transcript)
            .ok()
            .and_then(|m| Some((m.modified().ok()?, m.len())));
        // A working session rewrites its transcript constantly; follow it only loosely.
        let due = info.read_at.is_none_or(|t| t.elapsed() >= BUSY_REREAD);
        if stamp != info.stamp && (!busy || due) {
            info.stamp = stamp;
            info.read_at = Some(Instant::now());
            let turn = sessions::read_turn(&s.transcript);
            if turn.key != info.turn.key {
                info.analysis = Analysis::None;
            }
            info.turn = turn;
        }
        if busy || info.turn.key.is_empty() || !matches!(info.analysis, Analysis::None) {
            return;
        }
        if let Some(verdict) = haiku::cached(&s.id, &info.turn.key) {
            info.analysis = Analysis::Ready(verdict);
            return;
        }
        if self.running >= MAX_RUNNING {
            return;
        }
        info.analysis = Analysis::Pending;
        self.running += 1;
        let (id, key) = (s.id.clone(), info.turn.key.clone());
        let (prompt, answer) = (info.turn.prompt.clone(), info.turn.answer.clone());
        let tx = self.tx.clone();
        thread::spawn(move || {
            let result = haiku::analyse(&prompt, &answer);
            let _ = tx.send(Finished { id, key, result });
        });
    }

    /// Takes over the verdicts Haiku finished since the last frame.
    fn collect(&mut self) {
        let mut changed = false;
        while let Ok(done) = self.rx.try_recv() {
            self.running -= 1;
            // The chat may have moved on while Haiku was reading.
            let Some(info) = self
                .infos
                .get_mut(&done.id)
                .filter(|i| i.turn.key == done.key)
            else {
                continue;
            };
            info.analysis = match done.result {
                Ok(verdict) => {
                    haiku::store(&done.id, &done.key, &verdict);
                    Analysis::Ready(verdict)
                }
                Err(e) => Analysis::Failed(e),
            };
            changed = true;
        }
        if changed {
            self.sort();
        }
    }

    fn err(&mut self, msg: impl Into<String>) {
        self.status = Some((msg.into(), true));
    }

    fn jump(&mut self, pane: Pane) -> Action {
        match tmux::switch(&pane) {
            Ok(()) if self.once => return Action::Quit,
            Ok(()) => self.status = Some((format!("přepnuto na {}", pane.label), false)),
            Err(e) => self.err(e),
        }
        Action::None
    }

    fn start_in(&mut self, dir: &Path) -> Action {
        match tmux::new_window(dir) {
            Ok(()) if self.once => return Action::Quit,
            Ok(()) => self.status = Some((format!("spuštěno v {}", dir.display()), false)),
            Err(e) => self.err(e),
        }
        Action::None
    }

    fn on_key(&mut self, key: KeyCode, ctrl: bool) -> Action {
        self.status = None;
        if ctrl {
            // Raw mode swallows SIGINT, so Ctrl+C has to be handled here.
            if key == KeyCode::Char('c') {
                return Action::Quit;
            }
            return Action::None;
        }
        match std::mem::replace(&mut self.mode, Mode::Main) {
            Mode::Main => self.key_main(key),
            Mode::Browse(browser) => self.key_browse(key, browser),
        }
    }

    fn key_main(&mut self, key: KeyCode) -> Action {
        let last = self.rows.len().saturating_sub(1);
        match key {
            KeyCode::Char('q') | KeyCode::Esc => return Action::Quit,
            KeyCode::Up | KeyCode::Char('k') => self.sel = self.sel.saturating_sub(1),
            KeyCode::Down | KeyCode::Char('j') => self.sel = (self.sel + 1).min(last),
            KeyCode::Home | KeyCode::Char('g') => self.sel = 0,
            KeyCode::End | KeyCode::Char('G') => self.sel = last,
            KeyCode::Enter | KeyCode::Char(' ') => {
                let Some(row) = self.rows.get(self.sel) else {
                    return Action::None;
                };
                match row.pane.clone() {
                    Some(pane) => return self.jump(pane),
                    None if !self.has_tmux => self.err("tmux není nainstalován"),
                    None => {
                        let msg =
                            format!("session (pid {}) neběží v tmuxu, přepnout nejde", row.s.pid);
                        self.err(msg);
                    }
                }
            }
            KeyCode::Char('n') if !self.has_tmux => self.err("tmux není nainstalován"),
            KeyCode::Char('n') => {
                // Start next to the selected session's project.
                let start = self
                    .rows
                    .get(self.sel)
                    .and_then(|r| r.s.cwd.parent())
                    .filter(|p| p.is_dir())
                    .map_or_else(home, Path::to_path_buf);
                self.mode = Mode::Browse(Browser::new(start));
            }
            KeyCode::Char('a') => {
                if let Some(row) = self.rows.get(self.sel) {
                    if let Some(info) = self.infos.get_mut(&row.s.id) {
                        if !matches!(info.analysis, Analysis::Pending) {
                            haiku::forget(&row.s.id);
                            info.analysis = Analysis::None;
                            self.status = Some(("Haiku přečte chat znovu".into(), false));
                        }
                    }
                }
            }
            _ => {}
        }
        Action::None
    }

    fn key_browse(&mut self, key: KeyCode, mut b: Browser) -> Action {
        match key {
            KeyCode::Esc | KeyCode::Char('q') => return Action::None,
            KeyCode::Up | KeyCode::Char('k') => b.sel = b.sel.saturating_sub(1),
            KeyCode::Down | KeyCode::Char('j') => b.sel = (b.sel + 1).min(b.entries.len()),
            KeyCode::Left | KeyCode::Backspace | KeyCode::Char('h') => b.parent(),
            KeyCode::Right | KeyCode::Char('l') => {
                if let Some(dir) = b.current() {
                    b.enter(dir);
                }
            }
            KeyCode::Enter => match b.current() {
                Some(dir) => b.enter(dir),
                None => return self.start_in(&b.dir),
            },
            KeyCode::Char(' ') => return self.start_in(&b.current().unwrap_or(b.dir)),
            KeyCode::Char('.') => {
                b.hidden = !b.hidden;
                b.load();
            }
            _ => {}
        }
        self.mode = Mode::Browse(b);
        Action::None
    }

    fn draw(&self, f: &mut Frame) {
        let dim = Style::new().fg(Color::DarkGray);
        let [body, status, help] = Layout::vertical([
            Constraint::Min(3),
            Constraint::Length(1),
            Constraint::Length(1),
        ])
        .areas(f.area());

        let keys = match &self.mode {
            Mode::Browse(b) => {
                self.draw_browser(f, body, b);
                "↑↓ pohyb · Enter/→ otevřít · ← zpět · mezerník spustit zde · . skryté · Esc zrušit"
            }
            Mode::Main => {
                let [list, detail] =
                    Layout::vertical([Constraint::Min(4), Constraint::Percentage(40)]).areas(body);
                self.draw_list(f, list);
                self.draw_detail(f, detail);
                "↑↓ pohyb · Enter přepnout · n nová session · a znovu analyzovat · q konec"
            }
        };

        let status_line = match &self.status {
            Some((msg, true)) => Line::styled(format!(" {msg}"), Style::new().fg(Color::Red)),
            Some((msg, false)) => Line::styled(format!(" {msg}"), Style::new().fg(Color::Green)),
            None if !self.has_tmux => Line::styled(
                " tmux není nainstalován (sudo apt install tmux), přepínání nefunguje",
                Style::new().fg(Color::Red),
            ),
            None if self.running > 0 => {
                Line::styled(format!(" Haiku čte chaty: {}", self.running), dim)
            }
            None => Line::raw(""),
        };
        f.render_widget(Paragraph::new(status_line), status);
        f.render_widget(Paragraph::new(Line::styled(format!(" {keys}"), dim)), help);
    }

    fn draw_list(&self, f: &mut Frame, area: Rect) {
        let dim = Style::new().fg(Color::DarkGray);
        let waiting = self
            .rows
            .iter()
            .filter(|r| state_of(&self.infos, &r.s).needs_user())
            .count();
        let title = format!(
            " claude-manager · sessions: {} · čeká na vás: {waiting} ",
            self.rows.len()
        );
        if self.rows.is_empty() {
            let text = "Neběží žádná Claude Code session. Klávesa n spustí novou v novém okně.";
            let block = Block::bordered().title(title);
            f.render_widget(Paragraph::new(Line::styled(text, dim)).block(block), area);
            return;
        }

        let width = usize::from(area.width).saturating_sub(14);
        let items = self.rows.iter().map(|row| {
            let s = &row.s;
            let state = state_of(&self.infos, s);
            let (badge, color) = state.badge();
            let dir = s
                .cwd
                .file_name()
                .map_or("/".into(), |n| n.to_string_lossy());
            let place = row.pane.as_ref().map_or("mimo tmux", |p| p.label.as_str());
            let head = Line::from(vec![
                Span::styled(
                    format!("{badge:<9}"),
                    Style::new().fg(color).add_modifier(Modifier::BOLD),
                ),
                Span::styled(
                    self.title(s).to_string(),
                    Style::new().add_modifier(Modifier::BOLD),
                ),
                Span::styled(format!("  {dir} · {place} · {}", age(s.since_ms)), dim),
            ]);

            let info = self.infos.get(&s.id);
            let note = match (&s.status, info.map(|i| &i.analysis)) {
                (Status::Waiting(what), _) if what.is_empty() => "čeká na vaše potvrzení".into(),
                (Status::Waiting(what), _) => format!("čeká na: {what}"),
                (Status::Busy, _) => {
                    format!("↳ {}", info.map_or("", |i| i.turn.prompt.as_str()))
                }
                (_, Some(Analysis::Ready(v))) => v.summary.clone(),
                (_, Some(Analysis::Pending)) => "Haiku čte chat…".into(),
                (_, Some(Analysis::Failed(e))) => format!("analýza selhala: {e}"),
                _ if state == State::Empty => "zatím bez odpovědi".into(),
                _ => "čeká na analýzu".into(),
            };
            let note_style = if state.needs_user() {
                Style::new()
            } else {
                dim
            };
            let note = Line::styled(format!("{:9}{}", "", one_line(&note, width)), note_style);
            Text::from(vec![head, note])
        });

        let list = List::new(items)
            .block(Block::bordered().title(title))
            .highlight_symbol("› ")
            .highlight_style(Style::new().add_modifier(Modifier::REVERSED));
        let mut state = ListState::default().with_selected(Some(self.sel));
        f.render_stateful_widget(list, area, &mut state);
    }

    /// The selected session's last prompt and the end of its final answer.
    fn draw_detail(&self, f: &mut Frame, area: Rect) {
        let dim = Style::new().fg(Color::DarkGray);
        let Some(row) = self.rows.get(self.sel) else {
            f.render_widget(Block::bordered(), area);
            return;
        };
        let block = Block::bordered().title(format!(" {} ", row.s.cwd.display()));
        let inner = block.inner(area);
        let empty = Turn::default();
        let turn = self.infos.get(&row.s.id).map_or(&empty, |i| &i.turn);

        let mut lines = vec![
            Line::from(vec![
                Span::styled("Zadání: ", Style::new().fg(Color::Cyan)),
                Span::raw(one_line(
                    &turn.prompt,
                    usize::from(inner.width).saturating_sub(9),
                )),
            ]),
            Line::raw(""),
        ];
        let rows = usize::from(inner.height).saturating_sub(lines.len());
        if matches!(row.s.status, Status::Busy) {
            lines.push(Line::styled("Claude ještě pracuje…", dim));
        } else {
            let tail = tail_lines(&turn.answer, usize::from(inner.width), rows);
            lines.extend(tail.into_iter().map(Line::raw));
        }
        let text = Paragraph::new(lines)
            .wrap(Wrap { trim: false })
            .block(block);
        f.render_widget(text, area);
    }

    fn draw_browser(&self, f: &mut Frame, area: Rect, b: &Browser) {
        let mut items = vec![Line::styled(
            "▶ spustit claude v této složce",
            Style::new().fg(Color::Green),
        )];
        items.extend(
            b.entries
                .iter()
                .map(|name| Line::styled(format!("{name}/"), Style::new().fg(Color::Blue))),
        );
        if let Some(err) = &b.error {
            items.push(Line::styled(
                format!("nelze číst: {err}"),
                Style::new().fg(Color::Red),
            ));
        }
        let title = format!(" Kde spustit novou session · {} ", b.dir.display());
        let list = List::new(items)
            .block(Block::bordered().title(title))
            .highlight_symbol("› ")
            .highlight_style(Style::new().add_modifier(Modifier::REVERSED));
        let mut state = ListState::default().with_selected(Some(b.sel));
        f.render_stateful_widget(list, area, &mut state);
    }

    fn event_loop(&mut self, term: &mut DefaultTerminal) -> io::Result<()> {
        let mut refreshed: Option<Instant> = None;
        loop {
            if refreshed.is_none_or(|t| t.elapsed() >= REFRESH) {
                self.refresh();
                refreshed = Some(Instant::now());
            }
            self.collect();
            term.draw(|f| self.draw(f))?;
            // Wake up regularly so the list keeps up with the sessions.
            if !event::poll(Duration::from_millis(250))? {
                continue;
            }
            let Event::Key(key) = event::read()? else {
                continue;
            };
            if key.kind != KeyEventKind::Press {
                continue;
            }
            match self.on_key(key.code, key.modifiers.contains(KeyModifiers::CONTROL)) {
                Action::None => {}
                Action::Quit => return Ok(()),
            }
        }
    }
}

pub fn run(once: bool) -> Result<(), String> {
    let (tx, rx) = mpsc::channel();
    let mut app = App {
        once,
        has_tmux: tmux::inside(),
        rows: Vec::new(),
        infos: HashMap::new(),
        sel: 0,
        mode: Mode::Main,
        status: None,
        running: 0,
        tx,
        rx,
    };
    let home = tmux::Home::find();
    if let Some(home) = &home {
        home.show_way_back();
    }
    let mut term = ratatui::init();
    let result = app.event_loop(&mut term);
    ratatui::restore();
    // Our window closes with us; without this the terminal would land in some session.
    if let Some(home) = &home {
        home.detach();
    }
    result.map_err(|e| format!("terminál: {e}"))
}
