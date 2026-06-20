use std::{
    env,
    io::{self, Read, Write},
    sync::{Arc, Mutex, mpsc},
    thread,
    time::{Duration, Instant},
};

use anyhow::{Context, Result};
use crossterm::{
    event::{
        self, DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture,
        Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEvent,
        MouseEventKind,
    },
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use portable_pty::{CommandBuilder, MasterPty, PtySize, native_pty_system};
use ratatui::{
    Frame, Terminal,
    backend::CrosstermBackend,
    buffer::Buffer,
    layout::{Constraint, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Paragraph, Widget},
};

const SIDEBAR_WIDTH: u16 = 29;
const COLLAPSED_WIDTH: u16 = 4;
const MOBILE_THRESHOLD: u16 = 72;

struct Session {
    name: String,
    parser: Arc<Mutex<vt100::Parser>>,
    writer: Box<dyn Write + Send>,
    master: Box<dyn MasterPty + Send>,
}

impl Session {
    fn spawn(number: usize, rows: u16, cols: u16) -> Result<Self> {
        let pair = native_pty_system()
            .openpty(PtySize {
                rows,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            })
            .context("open PTY")?;
        let shell = env::var("SHELL").unwrap_or_else(|_| "/bin/sh".into());
        let mut command = CommandBuilder::new(&shell);
        command.cwd(env::current_dir()?);
        command.env("TERM", "xterm-256color");
        command.env("COLORTERM", "truecolor");
        pair.slave.spawn_command(command).context("start shell")?;
        drop(pair.slave);

        let mut reader = pair.master.try_clone_reader().context("clone PTY reader")?;
        let writer = pair.master.take_writer().context("open PTY writer")?;
        let parser = Arc::new(Mutex::new(vt100::Parser::new(rows, cols, 10_000)));
        let parser_for_reader = Arc::clone(&parser);
        thread::spawn(move || {
            let mut bytes = [0_u8; 16 * 1024];
            while let Ok(read) = reader.read(&mut bytes) {
                if read == 0 {
                    break;
                }
                if let Ok(mut parser) = parser_for_reader.lock() {
                    parser.process(&bytes[..read]);
                }
            }
        });

        Ok(Self {
            name: format!("terminal {number}"),
            parser,
            writer,
            master: pair.master,
        })
    }

    fn resize(&mut self, area: Rect) {
        if area.width == 0 || area.height == 0 {
            return;
        }
        let _ = self.master.resize(PtySize {
            rows: area.height,
            cols: area.width,
            pixel_width: 0,
            pixel_height: 0,
        });
        if let Ok(mut parser) = self.parser.lock() {
            parser.screen_mut().set_size(area.height, area.width);
        }
    }
}

#[derive(Default, Clone, Copy)]
struct HitAreas {
    sidebar: Rect,
    terminal: Rect,
    plus: Rect,
    toggle: Rect,
}

struct App {
    sessions: Vec<Session>,
    active: usize,
    sidebar_collapsed: bool,
    manual_collapse: bool,
    hits: HitAreas,
    last_area: Rect,
}

impl App {
    fn new(area: Rect) -> Result<Self> {
        let terminal = terminal_area(area, false);
        Ok(Self {
            sessions: vec![Session::spawn(
                1,
                terminal.height.max(2),
                terminal.width.max(2),
            )?],
            active: 0,
            sidebar_collapsed: false,
            manual_collapse: false,
            hits: HitAreas::default(),
            last_area: area,
        })
    }

    fn add_session(&mut self) -> Result<()> {
        let area = self.hits.terminal;
        self.sessions.push(Session::spawn(
            self.sessions.len() + 1,
            area.height.max(2),
            area.width.max(2),
        )?);
        self.active = self.sessions.len() - 1;
        Ok(())
    }

    fn resize(&mut self, area: Rect) {
        self.last_area = area;
        let responsive = area.width < MOBILE_THRESHOLD;
        self.sidebar_collapsed = self.manual_collapse || responsive;
        let terminal = terminal_area(area, self.sidebar_collapsed);
        for session in &mut self.sessions {
            session.resize(terminal);
        }
    }

    fn write_active(&mut self, bytes: &[u8]) {
        if let Some(session) = self.sessions.get_mut(self.active) {
            let _ = session.writer.write_all(bytes);
            let _ = session.writer.flush();
        }
    }

    fn handle_key(&mut self, key: KeyEvent) -> Result<bool> {
        if key.kind != KeyEventKind::Press {
            return Ok(false);
        }
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('q') {
            return Ok(true);
        }
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('t') {
            self.add_session()?;
            return Ok(false);
        }
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('b') {
            self.manual_collapse = !self.sidebar_collapsed;
            self.resize(self.last_area);
            return Ok(false);
        }
        if let Some(bytes) = encode_key(key) {
            self.write_active(&bytes);
        }
        Ok(false)
    }

    fn handle_mouse(&mut self, mouse: MouseEvent) -> Result<()> {
        if matches!(mouse.kind, MouseEventKind::Down(MouseButton::Left)) {
            if contains(self.hits.plus, mouse.column, mouse.row) {
                self.add_session()?;
            } else if contains(self.hits.toggle, mouse.column, mouse.row) {
                self.manual_collapse = !self.sidebar_collapsed;
                self.resize(self.last_area);
            } else if contains(self.hits.sidebar, mouse.column, mouse.row) {
                let first_row = self.hits.sidebar.y + 2;
                if mouse.row >= first_row {
                    let index = (mouse.row - first_row) as usize;
                    if index < self.sessions.len() {
                        self.active = index;
                    }
                }
            }
        }
        match mouse.kind {
            MouseEventKind::ScrollUp => self.write_active(b"\x1b[A\x1b[A\x1b[A"),
            MouseEventKind::ScrollDown => self.write_active(b"\x1b[B\x1b[B\x1b[B"),
            _ => {}
        }
        Ok(())
    }
}

fn terminal_area(area: Rect, collapsed: bool) -> Rect {
    let width = if collapsed {
        COLLAPSED_WIDTH
    } else {
        SIDEBAR_WIDTH
    };
    let [_, terminal] =
        Layout::horizontal([Constraint::Length(width), Constraint::Min(1)]).areas(area);
    terminal
}

fn contains(area: Rect, x: u16, y: u16) -> bool {
    area.width > 0
        && area.height > 0
        && x >= area.x
        && x < area.right()
        && y >= area.y
        && y < area.bottom()
}

fn encode_key(key: KeyEvent) -> Option<Vec<u8>> {
    let bytes = match key.code {
        KeyCode::Char(c) if key.modifiers.contains(KeyModifiers::CONTROL) => {
            vec![(c.to_ascii_lowercase() as u8) & 0x1f]
        }
        KeyCode::Char(c) => c.to_string().into_bytes(),
        KeyCode::Enter => b"\r".to_vec(),
        KeyCode::Tab => b"\t".to_vec(),
        KeyCode::BackTab => b"\x1b[Z".to_vec(),
        KeyCode::Backspace => vec![0x7f],
        KeyCode::Esc => vec![0x1b],
        KeyCode::Up => b"\x1b[A".to_vec(),
        KeyCode::Down => b"\x1b[B".to_vec(),
        KeyCode::Right => b"\x1b[C".to_vec(),
        KeyCode::Left => b"\x1b[D".to_vec(),
        KeyCode::Home => b"\x1b[H".to_vec(),
        KeyCode::End => b"\x1b[F".to_vec(),
        KeyCode::PageUp => b"\x1b[5~".to_vec(),
        KeyCode::PageDown => b"\x1b[6~".to_vec(),
        KeyCode::Delete => b"\x1b[3~".to_vec(),
        KeyCode::Insert => b"\x1b[2~".to_vec(),
        KeyCode::F(n) => format!("\x1b[{}~", 10 + n).into_bytes(),
        _ => return None,
    };
    Some(if key.modifiers.contains(KeyModifiers::ALT) {
        [vec![0x1b], bytes].concat()
    } else {
        bytes
    })
}

struct TerminalView<'a>(&'a vt100::Screen);

impl Widget for TerminalView<'_> {
    fn render(self, area: Rect, buf: &mut Buffer) {
        let rows = area.height.min(self.0.size().0);
        let cols = area.width.min(self.0.size().1);
        for row in 0..rows {
            for col in 0..cols {
                let Some(cell) = self.0.cell(row, col) else {
                    continue;
                };
                let mut style = Style::default()
                    .fg(vt_color(cell.fgcolor()))
                    .bg(vt_color(cell.bgcolor()));
                if cell.bold() {
                    style = style.add_modifier(Modifier::BOLD);
                }
                if cell.italic() {
                    style = style.add_modifier(Modifier::ITALIC);
                }
                if cell.underline() {
                    style = style.add_modifier(Modifier::UNDERLINED);
                }
                if cell.inverse() {
                    style = style.add_modifier(Modifier::REVERSED);
                }
                let symbol = if cell.contents().is_empty() {
                    " "
                } else {
                    cell.contents()
                };
                buf[(area.x + col, area.y + row)]
                    .set_symbol(symbol)
                    .set_style(style);
            }
        }
    }
}

fn vt_color(color: vt100::Color) -> Color {
    match color {
        vt100::Color::Default => Color::Reset,
        vt100::Color::Idx(i) => Color::Indexed(i),
        vt100::Color::Rgb(r, g, b) => Color::Rgb(r, g, b),
    }
}

fn render(frame: &mut Frame, app: &mut App) {
    let area = frame.area();
    let sidebar_w = if app.sidebar_collapsed {
        COLLAPSED_WIDTH
    } else {
        SIDEBAR_WIDTH
    };
    let [sidebar, terminal] =
        Layout::horizontal([Constraint::Length(sidebar_w), Constraint::Min(1)]).areas(area);
    app.hits.sidebar = sidebar;
    app.hits.terminal = terminal;

    let divider = Style::default().fg(Color::Rgb(55, 58, 74));
    for y in sidebar.y..sidebar.bottom() {
        if sidebar.width > 0 {
            frame.buffer_mut()[(sidebar.right() - 1, y)]
                .set_symbol("│")
                .set_style(divider);
        }
    }

    if app.sidebar_collapsed {
        for (index, _) in app
            .sessions
            .iter()
            .enumerate()
            .take(sidebar.height.saturating_sub(2) as usize)
        {
            let active = index == app.active;
            let line = Line::from(Span::styled(
                format!(" {} ", index + 1),
                if active {
                    Style::default()
                        .fg(Color::Black)
                        .bg(Color::Rgb(131, 166, 229))
                        .add_modifier(Modifier::BOLD)
                } else {
                    Style::default().fg(Color::DarkGray)
                },
            ));
            frame.render_widget(
                Paragraph::new(line),
                Rect::new(
                    sidebar.x,
                    sidebar.y + 2 + index as u16,
                    sidebar.width.saturating_sub(1),
                    1,
                ),
            );
        }
    } else {
        let title = Line::from(Span::styled(
            " kitty",
            Style::default()
                .fg(Color::Rgb(131, 138, 166))
                .add_modifier(Modifier::BOLD),
        ));
        frame.render_widget(
            Paragraph::new(title),
            Rect::new(sidebar.x, sidebar.y, sidebar.width.saturating_sub(1), 1),
        );
        app.hits.plus = Rect::new(sidebar.right().saturating_sub(3), sidebar.y, 2, 1);
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                "+",
                Style::default()
                    .fg(Color::Rgb(131, 166, 229))
                    .add_modifier(Modifier::BOLD),
            )))
            .alignment(ratatui::layout::Alignment::Right),
            Rect::new(sidebar.x, sidebar.y, sidebar.width.saturating_sub(1), 1),
        );

        for (index, session) in app
            .sessions
            .iter()
            .enumerate()
            .take(sidebar.height.saturating_sub(3) as usize)
        {
            let active = index == app.active;
            let style = if active {
                Style::default()
                    .fg(Color::Rgb(205, 211, 240))
                    .bg(Color::Rgb(35, 36, 54))
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(Color::Rgb(166, 173, 200))
            };
            let dot = if active { "●" } else { "○" };
            let line = Line::from(vec![
                Span::styled(
                    format!(" {dot} "),
                    Style::default().fg(Color::Rgb(166, 227, 161)),
                ),
                Span::styled(&session.name, style),
            ]);
            frame.render_widget(
                Paragraph::new(line).style(style),
                Rect::new(
                    sidebar.x,
                    sidebar.y + 2 + index as u16,
                    sidebar.width.saturating_sub(1),
                    1,
                ),
            );
        }
    }

    let toggle_text = if app.sidebar_collapsed { "»" } else { "«" };
    app.hits.toggle = Rect::new(
        sidebar.right().saturating_sub(3),
        sidebar.bottom().saturating_sub(1),
        2,
        1,
    );
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(
            toggle_text,
            Style::default().fg(Color::Rgb(131, 138, 166)),
        )))
        .alignment(ratatui::layout::Alignment::Right),
        Rect::new(
            sidebar.x,
            sidebar.bottom().saturating_sub(1),
            sidebar.width.saturating_sub(1),
            1,
        ),
    );

    if let Some(session) = app.sessions.get(app.active)
        && let Ok(parser) = session.parser.lock()
    {
        frame.render_widget(TerminalView(parser.screen()), terminal);
        let (row, col) = parser.screen().cursor_position();
        if col < terminal.width && row < terminal.height {
            frame.set_cursor_position((terminal.x + col, terminal.y + row));
        }
    }
}

struct TerminalGuard;
impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let _ = disable_raw_mode();
        let _ = execute!(
            io::stdout(),
            DisableMouseCapture,
            DisableBracketedPaste,
            LeaveAlternateScreen
        );
    }
}

fn main() -> Result<()> {
    enable_raw_mode()?;
    execute!(
        io::stdout(),
        EnterAlternateScreen,
        EnableMouseCapture,
        EnableBracketedPaste
    )?;
    let _guard = TerminalGuard;
    let backend = CrosstermBackend::new(io::stdout());
    let mut terminal = Terminal::new(backend)?;
    let mut app = App::new(terminal.size()?.into())?;
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        loop {
            if event::poll(Duration::from_millis(25)).unwrap_or(false)
                && let Ok(event) = event::read()
            {
                let _ = tx.send(event);
            }
        }
    });

    let mut last_draw = Instant::now();
    loop {
        let area: Rect = terminal.size()?.into();
        if area != app.last_area {
            app.resize(area);
        }
        if last_draw.elapsed() >= Duration::from_millis(16) {
            terminal.draw(|frame| render(frame, &mut app))?;
            last_draw = Instant::now();
        }
        match rx.recv_timeout(Duration::from_millis(8)) {
            Ok(Event::Key(key)) if app.handle_key(key)? => break,
            Ok(Event::Mouse(mouse)) => app.handle_mouse(mouse)?,
            Ok(Event::Paste(text)) => app.write_active(text.as_bytes()),
            Ok(Event::Resize(_, _)) | Err(mpsc::RecvTimeoutError::Timeout) => {}
            Ok(_) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn expanded_and_collapsed_layout_reserve_expected_width() {
        let area = Rect::new(0, 0, 120, 40);
        assert_eq!(
            terminal_area(area, false),
            Rect::new(SIDEBAR_WIDTH, 0, 91, 40)
        );
        assert_eq!(
            terminal_area(area, true),
            Rect::new(COLLAPSED_WIDTH, 0, 116, 40)
        );
    }

    #[test]
    fn hit_testing_excludes_right_and_bottom_edges() {
        let area = Rect::new(5, 7, 10, 4);
        assert!(contains(area, 5, 7));
        assert!(contains(area, 14, 10));
        assert!(!contains(area, 15, 10));
        assert!(!contains(area, 14, 11));
    }

    #[test]
    fn control_keys_are_forwarded_as_terminal_bytes() {
        let key = KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL);
        assert_eq!(encode_key(key), Some(vec![3]));
    }
}
