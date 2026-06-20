use std::{
    env,
    io::{self, Read, Write},
    path::PathBuf,
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
use portable_pty::{Child, CommandBuilder, MasterPty, PtySize, native_pty_system};
use ratatui::{
    Frame, Terminal,
    backend::CrosstermBackend,
    buffer::Buffer,
    layout::{Constraint, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, Paragraph, Widget},
};

const SIDEBAR_WIDTH: u16 = 29;
const COLLAPSED_WIDTH: u16 = 4;
const MOBILE_THRESHOLD: u16 = 72;

struct Session {
    id: u64,
    custom_name: Option<String>,
    last_command: Option<String>,
    input_line: String,
    cwd: Arc<Mutex<PathBuf>>,
    pending: Arc<std::sync::atomic::AtomicBool>,
    group: Option<u64>,
    parser: Arc<Mutex<vt100::Parser>>,
    writer: Box<dyn Write + Send>,
    master: Box<dyn MasterPty + Send>,
    _child: Box<dyn Child + Send + Sync>,
    shell_pid: Option<u32>,
}

impl Session {
    fn spawn(id: u64, rows: u16, cols: u16) -> Result<Self> {
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
        let child = pair.slave.spawn_command(command).context("start shell")?;
        let shell_pid = child.process_id();
        drop(pair.slave);

        let mut reader = pair.master.try_clone_reader().context("clone PTY reader")?;
        let writer = pair.master.take_writer().context("open PTY writer")?;
        let parser = Arc::new(Mutex::new(vt100::Parser::new(rows, cols, 10_000)));
        let cwd = Arc::new(Mutex::new(env::current_dir()?));
        let pending = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let parser_for_reader = Arc::clone(&parser);
        let cwd_for_reader = Arc::clone(&cwd);
        thread::spawn(move || {
            let mut bytes = [0_u8; 16 * 1024];
            while let Ok(read) = reader.read(&mut bytes) {
                if read == 0 {
                    break;
                }
                if let Ok(mut parser) = parser_for_reader.lock() {
                    parser.process(&bytes[..read]);
                }
                update_cwd_from_osc7(&bytes[..read], &cwd_for_reader);
            }
        });

        Ok(Self {
            id,
            custom_name: None,
            last_command: None,
            input_line: String::new(),
            cwd,
            pending,
            group: None,
            parser,
            writer,
            master: pair.master,
            _child: child,
            shell_pid,
        })
    }

    fn display_name(&self) -> &str {
        display_name_for(self.custom_name.as_deref(), self.last_command.as_deref())
    }

    fn directory_label(&self) -> String {
        let cwd = self.cwd.lock().map(|cwd| cwd.clone()).unwrap_or_default();
        let home = env::var_os("HOME").map(PathBuf::from);
        if let Some(home) = home
            && let Ok(suffix) = cwd.strip_prefix(home)
        {
            return if suffix.as_os_str().is_empty() {
                "~".into()
            } else {
                format!("~/{}", suffix.display())
            };
        }
        cwd.display().to_string()
    }

    fn is_pending(&self) -> bool {
        if !self.pending.load(std::sync::atomic::Ordering::Relaxed) {
            return false;
        }
        #[cfg(unix)]
        if let (Some(fd), Some(shell_pid)) = (self.master.as_raw_fd(), self.shell_pid) {
            let foreground = unsafe { libc::tcgetpgrp(fd) };
            if foreground > 0 && foreground as u32 == shell_pid {
                self.pending
                    .store(false, std::sync::atomic::Ordering::Relaxed);
                return false;
            }
        }
        true
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

fn display_name_for<'a>(custom_name: Option<&'a str>, last_command: Option<&'a str>) -> &'a str {
    custom_name.or(last_command).unwrap_or("New Tab")
}

#[derive(Clone, Copy)]
struct TabHit {
    index: usize,
    body: Rect,
    close: Rect,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SplitAxis {
    Horizontal,
    Vertical,
}

#[derive(Clone)]
enum PaneLayout {
    Leaf(u64),
    Split {
        axis: SplitAxis,
        ratio: u16,
        first: Box<PaneLayout>,
        second: Box<PaneLayout>,
    },
}

#[derive(Clone)]
struct SplitHit {
    group: u64,
    path: Vec<bool>,
    axis: SplitAxis,
    divider: Rect,
    container: Rect,
}

#[derive(Default)]
struct HitAreas {
    sidebar: Rect,
    terminal: Rect,
    plus: Rect,
    toggle: Rect,
    tabs: Vec<TabHit>,
    panes: Vec<(usize, Rect)>,
    groups: Vec<(u64, Rect)>,
    split_dividers: Vec<SplitHit>,
    confirm_yes: Rect,
    confirm_no: Rect,
}

#[derive(Clone)]
enum MenuAction {
    Rename,
    ClearName,
    RenameGroup(u64),
    ClearGroupName(u64),
    LayoutHorizontal(u64),
    LayoutVertical(u64),
    PlaceRelative {
        target: u64,
        pane: u64,
        axis: SplitAxis,
    },
    EqualizeGroup(u64),
    CreateGroup,
    RemoveFromGroup,
    Ungroup(u64),
    Close,
}

struct ContextMenu {
    session_id: u64,
    area: Rect,
    entries: Vec<(String, MenuAction)>,
}

enum InputMode {
    Normal,
    Rename { session_id: u64, value: String },
    RenameGroup { group_id: u64, value: String },
    ConfirmClose { session_id: u64 },
}

struct App {
    host_terminal: String,
    sessions: Vec<Session>,
    active: usize,
    sidebar_collapsed: bool,
    manual_collapse: bool,
    hits: HitAreas,
    last_area: Rect,
    next_session_id: u64,
    next_group_id: u64,
    group_names: std::collections::HashMap<u64, String>,
    group_layouts: std::collections::HashMap<u64, PaneLayout>,
    context_menu: Option<ContextMenu>,
    input_mode: InputMode,
    spinner_tick: usize,
    exit_requested: bool,
    prefix_started: Option<Instant>,
    dragging_split: Option<SplitHit>,
}

impl App {
    fn group_name(&self, group: u64) -> String {
        group_display_name(&self.group_names, group)
    }

    fn new(area: Rect) -> Result<Self> {
        let terminal = terminal_area(area, false);
        Ok(Self {
            host_terminal: detect_host_terminal(),
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
            next_session_id: 2,
            next_group_id: 1,
            group_names: std::collections::HashMap::new(),
            group_layouts: std::collections::HashMap::new(),
            context_menu: None,
            input_mode: InputMode::Normal,
            spinner_tick: 0,
            exit_requested: false,
            prefix_started: None,
            dragging_split: None,
        })
    }

    fn add_session(&mut self) -> Result<()> {
        let area = self.hits.terminal;
        self.sessions.push(Session::spawn(
            self.next_session_id,
            area.height.max(2),
            area.width.max(2),
        )?);
        self.next_session_id += 1;
        self.active = self.sessions.len() - 1;
        Ok(())
    }

    fn resize(&mut self, area: Rect) {
        self.last_area = area;
        let responsive = area.width < MOBILE_THRESHOLD;
        self.sidebar_collapsed = self.manual_collapse || responsive;
        let terminal = terminal_area(area, self.sidebar_collapsed);
        let pane_areas = pane_areas_for(self, terminal);
        for (index, pane) in pane_areas {
            if let Some(session) = self.sessions.get_mut(index) {
                session.resize(pane);
            }
        }
    }

    fn write_active(&mut self, bytes: &[u8]) {
        if let Some(session) = self.sessions.get_mut(self.active) {
            let _ = session.writer.write_all(bytes);
            let _ = session.writer.flush();
        }
    }

    fn select_top_level(&mut self, position: usize) {
        if let Some(index) = top_level_units(self).get(position).copied() {
            self.active = index;
            self.resize(self.last_area);
        }
    }

    fn cycle_top_level(&mut self, delta: isize) {
        let units = top_level_units(self);
        if units.is_empty() {
            return;
        }
        let active_group = self
            .sessions
            .get(self.active)
            .and_then(|session| session.group);
        let current = units
            .iter()
            .position(|index| {
                *index == self.active
                    || active_group.is_some()
                        && self.sessions.get(*index).and_then(|session| session.group)
                            == active_group
            })
            .unwrap_or(0);
        let next = (current as isize + delta).rem_euclid(units.len() as isize) as usize;
        self.active = units[next];
        self.resize(self.last_area);
    }

    fn focus_pane(&mut self, direction: KeyCode) {
        let Some((_, current)) = self
            .hits
            .panes
            .iter()
            .find(|(index, _)| *index == self.active)
            .copied()
        else {
            return;
        };
        let center = (
            current.x + current.width / 2,
            current.y + current.height / 2,
        );
        let candidate = self
            .hits
            .panes
            .iter()
            .filter(|(index, _)| *index != self.active)
            .filter_map(|(index, area)| {
                let other = (area.x + area.width / 2, area.y + area.height / 2);
                let valid = match direction {
                    KeyCode::Left => other.0 < center.0,
                    KeyCode::Right => other.0 > center.0,
                    KeyCode::Up => other.1 < center.1,
                    KeyCode::Down => other.1 > center.1,
                    _ => false,
                };
                valid.then(|| {
                    let primary = match direction {
                        KeyCode::Left | KeyCode::Right => other.0.abs_diff(center.0),
                        _ => other.1.abs_diff(center.1),
                    };
                    let secondary = match direction {
                        KeyCode::Left | KeyCode::Right => other.1.abs_diff(center.1),
                        _ => other.0.abs_diff(center.0),
                    };
                    (*index, u32::from(primary) * 1_000 + u32::from(secondary))
                })
            })
            .min_by_key(|(_, score)| *score)
            .map(|(index, _)| index);
        if let Some(index) = candidate {
            self.active = index;
        }
    }

    fn handle_prefix_key(&mut self, key: KeyEvent) -> Result<()> {
        match key.code {
            KeyCode::Char('1'..='9') => {
                let position = match key.code {
                    KeyCode::Char(digit) => digit.to_digit(10).unwrap_or(1) as usize - 1,
                    _ => 0,
                };
                self.select_top_level(position);
            }
            KeyCode::Char('0') => self.select_top_level(9),
            KeyCode::Left => self.cycle_top_level(-1),
            KeyCode::Right => self.cycle_top_level(1),
            KeyCode::Char('h') => self.focus_pane(KeyCode::Left),
            KeyCode::Char('j') => self.focus_pane(KeyCode::Down),
            KeyCode::Char('k') => self.focus_pane(KeyCode::Up),
            KeyCode::Char('l') => self.focus_pane(KeyCode::Right),
            KeyCode::Char('T' | 't') => self.add_session()?,
            KeyCode::Char('W' | 'w') => self.request_close(self.active),
            KeyCode::Char('s') => {
                self.manual_collapse = !self.sidebar_collapsed;
                self.resize(self.last_area);
            }
            KeyCode::Char('b') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.write_active(&[0x02]);
            }
            _ => {}
        }
        Ok(())
    }

    fn close_session(&mut self, index: usize) {
        if index >= self.sessions.len() {
            return;
        }
        if self.sessions.len() == 1 {
            self.exit_requested = true;
            return;
        }
        let id = self.sessions[index].id;
        let group = self.sessions[index].group;
        self.sessions.remove(index);
        if let Some(group) = group
            && let Some(layout) = self.group_layouts.remove(&group)
            && let Some(layout) = remove_layout_leaf(layout, id)
        {
            self.group_layouts.insert(group, layout);
        }
        self.active = self.active.min(self.sessions.len() - 1);
        self.cleanup_groups();
        self.resize(self.last_area);
    }

    fn request_close(&mut self, index: usize) {
        let Some(session) = self.sessions.get(index) else {
            return;
        };
        if self.sessions.len() == 1 || session.is_pending() {
            self.input_mode = InputMode::ConfirmClose {
                session_id: session.id,
            };
        } else {
            self.close_session(index);
        }
    }

    fn cleanup_groups(&mut self) {
        let groups: std::collections::HashSet<u64> = self
            .sessions
            .iter()
            .filter_map(|session| session.group)
            .collect();
        self.group_names.retain(|group, _| groups.contains(group));
        self.group_layouts.retain(|group, _| groups.contains(group));
    }

    fn open_context_menu(&mut self, index: usize, x: u16, y: u16) {
        let Some(session) = self.sessions.get(index) else {
            return;
        };
        let mut entries = vec![("Rename".into(), MenuAction::Rename)];
        if session.custom_name.is_some() {
            entries.push(("Clear custom name".into(), MenuAction::ClearName));
        }
        if session.group.is_some() {
            entries.push(("Remove from group".into(), MenuAction::RemoveFromGroup));
        } else {
            entries.push(("Create pane group".into(), MenuAction::CreateGroup));
        }
        if index != self.active {
            let target = self.sessions[self.active].id;
            entries.push((
                "Place beside current pane".into(),
                MenuAction::PlaceRelative {
                    target,
                    pane: session.id,
                    axis: SplitAxis::Horizontal,
                },
            ));
            entries.push((
                "Place below current pane".into(),
                MenuAction::PlaceRelative {
                    target,
                    pane: session.id,
                    axis: SplitAxis::Vertical,
                },
            ));
        }
        entries.push(("Close tab".into(), MenuAction::Close));
        self.show_context_menu(session.id, entries, x, y);
    }

    fn open_group_context_menu(&mut self, group: u64, x: u16, y: u16) {
        let Some(session_id) = self
            .sessions
            .iter()
            .find(|session| session.group == Some(group))
            .map(|session| session.id)
        else {
            return;
        };
        let mut entries = vec![("Rename group".into(), MenuAction::RenameGroup(group))];
        if self.group_names.contains_key(&group) {
            entries.push(("Clear group name".into(), MenuAction::ClearGroupName(group)));
        }
        entries.push(("Ungroup".into(), MenuAction::Ungroup(group)));
        entries.push((
            "Tile side by side".into(),
            MenuAction::LayoutHorizontal(group),
        ));
        entries.push((
            "Tile top and bottom".into(),
            MenuAction::LayoutVertical(group),
        ));
        entries.push(("Equalize panes".into(), MenuAction::EqualizeGroup(group)));
        self.show_context_menu(session_id, entries, x, y);
    }

    fn show_context_menu(
        &mut self,
        session_id: u64,
        entries: Vec<(String, MenuAction)>,
        x: u16,
        y: u16,
    ) {
        let width = (entries
            .iter()
            .map(|(label, _)| label.len())
            .max()
            .unwrap_or(10) as u16
            + 3)
        .max(24)
        .min(self.last_area.width);
        let height = entries.len() as u16 + 2;
        let area = Rect::new(
            x.min(self.last_area.width.saturating_sub(width)),
            y.min(self.last_area.height.saturating_sub(height)),
            width,
            height,
        );
        self.context_menu = Some(ContextMenu {
            session_id,
            area,
            entries,
        });
    }

    fn place_relative_to_current(&mut self, target: u64, pane: u64, axis: SplitAxis) {
        let Some(target_index) = self
            .sessions
            .iter()
            .position(|session| session.id == target)
        else {
            return;
        };
        let group = if let Some(group) = self.sessions[target_index].group {
            group
        } else {
            let group = self.next_group_id;
            self.next_group_id += 1;
            self.sessions[target_index].group = Some(group);
            self.group_layouts.insert(group, PaneLayout::Leaf(target));
            group
        };
        self.place_pane_relative(group, target, pane, axis);
    }

    fn place_pane_relative(&mut self, group: u64, target: u64, pane: u64, axis: SplitAxis) {
        if target == pane {
            return;
        }
        let Some(pane_index) = self.sessions.iter().position(|session| session.id == pane) else {
            return;
        };
        let old_group = self.sessions[pane_index].group;
        if let Some(old_group) = old_group
            && let Some(layout) = self.group_layouts.remove(&old_group)
            && let Some(layout) = remove_layout_leaf(layout, pane)
        {
            self.group_layouts.insert(old_group, layout);
        }
        self.sessions[pane_index].group = None;

        let mut layout = self
            .group_layouts
            .remove(&group)
            .unwrap_or_else(|| layout_for_group(&self.sessions, group));
        if !insert_layout_leaf(&mut layout, target, pane, axis) {
            layout = PaneLayout::Split {
                axis,
                ratio: 50,
                first: Box::new(layout),
                second: Box::new(PaneLayout::Leaf(pane)),
            };
        }
        self.sessions[pane_index].group = Some(group);
        self.group_layouts.insert(group, layout);
        if old_group != Some(group) {
            self.cleanup_groups();
        }
        if let Some(target_index) = self
            .sessions
            .iter()
            .position(|session| session.id == target)
        {
            self.active = target_index;
        }
        self.resize(self.last_area);
    }

    fn apply_menu_action(&mut self, session_id: u64, action: MenuAction) {
        let Some(index) = self
            .sessions
            .iter()
            .position(|session| session.id == session_id)
        else {
            return;
        };
        match action {
            MenuAction::Rename => {
                let value = self.sessions[index].custom_name.clone().unwrap_or_default();
                self.input_mode = InputMode::Rename { session_id, value };
            }
            MenuAction::ClearName => self.sessions[index].custom_name = None,
            MenuAction::RenameGroup(group) => {
                let value = self.group_names.get(&group).cloned().unwrap_or_default();
                self.input_mode = InputMode::RenameGroup {
                    group_id: group,
                    value,
                };
            }
            MenuAction::ClearGroupName(group) => {
                self.group_names.remove(&group);
            }
            MenuAction::CreateGroup => {
                let group = self.next_group_id;
                let partner = if self.active != index && self.sessions[self.active].group.is_none()
                {
                    Some(self.active)
                } else {
                    None
                };
                self.next_group_id += 1;
                self.sessions[index].group = Some(group);
                if let Some(partner) = partner {
                    self.sessions[partner].group = Some(group);
                    self.group_layouts.insert(
                        group,
                        PaneLayout::Split {
                            axis: SplitAxis::Horizontal,
                            ratio: 50,
                            first: Box::new(PaneLayout::Leaf(self.sessions[index].id)),
                            second: Box::new(PaneLayout::Leaf(self.sessions[partner].id)),
                        },
                    );
                } else {
                    self.group_layouts
                        .insert(group, PaneLayout::Leaf(self.sessions[index].id));
                }
                self.active = index;
                self.resize(self.last_area);
            }
            MenuAction::RemoveFromGroup => {
                if let Some(group) = self.sessions[index].group
                    && let Some(layout) = self.group_layouts.remove(&group)
                    && let Some(layout) = remove_layout_leaf(layout, self.sessions[index].id)
                {
                    self.group_layouts.insert(group, layout);
                }
                self.sessions[index].group = None;
                self.cleanup_groups();
                self.resize(self.last_area);
            }
            MenuAction::Ungroup(group) => {
                for session in &mut self.sessions {
                    if session.group == Some(group) {
                        session.group = None;
                    }
                }
                self.group_names.remove(&group);
                self.group_layouts.remove(&group);
                self.resize(self.last_area);
            }
            MenuAction::LayoutHorizontal(group) => {
                self.group_layouts.insert(
                    group,
                    balanced_layout(&self.sessions, group, SplitAxis::Horizontal),
                );
                self.resize(self.last_area);
            }
            MenuAction::LayoutVertical(group) => {
                self.group_layouts.insert(
                    group,
                    balanced_layout(&self.sessions, group, SplitAxis::Vertical),
                );
                self.resize(self.last_area);
            }
            MenuAction::PlaceRelative { target, pane, axis } => {
                self.place_relative_to_current(target, pane, axis)
            }
            MenuAction::EqualizeGroup(group) => {
                if let Some(layout) = self.group_layouts.get_mut(&group) {
                    equalize_layout(layout);
                }
                self.resize(self.last_area);
            }
            MenuAction::Close => self.request_close(index),
        }
    }

    fn handle_key(&mut self, key: KeyEvent) -> Result<bool> {
        if key.kind != KeyEventKind::Press {
            return Ok(false);
        }
        if self
            .prefix_started
            .is_some_and(|started| started.elapsed() > Duration::from_secs(1))
        {
            self.prefix_started = None;
        }
        if let InputMode::Rename { session_id, value } = &mut self.input_mode {
            match key.code {
                KeyCode::Esc => self.input_mode = InputMode::Normal,
                KeyCode::Enter => {
                    let id = *session_id;
                    let name = value.trim().to_string();
                    if let Some(session) = self.sessions.iter_mut().find(|session| session.id == id)
                    {
                        session.custom_name = (!name.is_empty()).then_some(name);
                    }
                    self.input_mode = InputMode::Normal;
                }
                KeyCode::Backspace => {
                    value.pop();
                }
                KeyCode::Char(c) if !key.modifiers.contains(KeyModifiers::CONTROL) => value.push(c),
                _ => {}
            }
            return Ok(false);
        }
        if let InputMode::RenameGroup { group_id, value } = &mut self.input_mode {
            match key.code {
                KeyCode::Esc => self.input_mode = InputMode::Normal,
                KeyCode::Enter => {
                    let id = *group_id;
                    let name = value.trim().to_string();
                    if name.is_empty() {
                        self.group_names.remove(&id);
                    } else {
                        self.group_names.insert(id, name);
                    }
                    self.input_mode = InputMode::Normal;
                }
                KeyCode::Backspace => {
                    value.pop();
                }
                KeyCode::Char(c) if !key.modifiers.contains(KeyModifiers::CONTROL) => value.push(c),
                _ => {}
            }
            return Ok(false);
        }
        let confirm_session_id = match &self.input_mode {
            InputMode::ConfirmClose { session_id } => Some(*session_id),
            _ => None,
        };
        if let Some(session_id) = confirm_session_id {
            match key.code {
                KeyCode::Char('y') | KeyCode::Char('Y') | KeyCode::Enter => {
                    if let Some(index) = self
                        .sessions
                        .iter()
                        .position(|session| session.id == session_id)
                    {
                        self.close_session(index);
                    }
                    self.input_mode = InputMode::Normal;
                }
                KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => {
                    self.input_mode = InputMode::Normal;
                }
                _ => {}
            }
            return Ok(false);
        }
        if self.prefix_started.take().is_some() {
            self.handle_prefix_key(key)?;
            return Ok(false);
        }
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('b') {
            self.prefix_started = Some(Instant::now());
            return Ok(false);
        }
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('q') {
            return Ok(true);
        }
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('t') {
            self.add_session()?;
            return Ok(false);
        }
        if let Some(session) = self.sessions.get_mut(self.active) {
            match key.code {
                KeyCode::Char(c) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                    session.input_line.push(c)
                }
                KeyCode::Backspace => {
                    session.input_line.pop();
                }
                KeyCode::Enter => {
                    let command = session.input_line.trim().to_string();
                    if !command.is_empty() {
                        session.last_command = Some(command);
                        session
                            .pending
                            .store(true, std::sync::atomic::Ordering::Relaxed);
                    }
                    session.input_line.clear();
                }
                _ => {}
            }
        }
        if let Some(bytes) = encode_key(key) {
            self.write_active(&bytes);
        }
        Ok(false)
    }

    fn handle_mouse(&mut self, mouse: MouseEvent) -> Result<()> {
        if let InputMode::ConfirmClose { session_id } = &self.input_mode {
            if matches!(mouse.kind, MouseEventKind::Down(MouseButton::Left)) {
                let session_id = *session_id;
                if contains(self.hits.confirm_yes, mouse.column, mouse.row) {
                    if let Some(index) = self
                        .sessions
                        .iter()
                        .position(|session| session.id == session_id)
                    {
                        self.close_session(index);
                    }
                    self.input_mode = InputMode::Normal;
                } else if contains(self.hits.confirm_no, mouse.column, mouse.row) {
                    self.input_mode = InputMode::Normal;
                }
            }
            return Ok(());
        }
        if let Some(menu) = self.context_menu.as_ref() {
            if matches!(mouse.kind, MouseEventKind::Down(MouseButton::Left)) {
                let action = if contains(menu.area, mouse.column, mouse.row)
                    && mouse.row > menu.area.y
                    && mouse.row < menu.area.bottom().saturating_sub(1)
                {
                    menu.entries
                        .get((mouse.row - menu.area.y - 1) as usize)
                        .map(|(_, action)| (menu.session_id, action.clone()))
                } else {
                    None
                };
                self.context_menu = None;
                if let Some((session_id, action)) = action {
                    self.apply_menu_action(session_id, action);
                }
                return Ok(());
            }
            return Ok(());
        }
        if matches!(mouse.kind, MouseEventKind::Drag(MouseButton::Left)) {
            if let Some(hit) = self.dragging_split.clone()
                && let Some(layout) = self.group_layouts.get_mut(&hit.group)
                && let Some(PaneLayout::Split { ratio, .. }) = layout_at_path_mut(layout, &hit.path)
            {
                let value = match hit.axis {
                    SplitAxis::Horizontal => mouse.column.saturating_sub(hit.container.x),
                    SplitAxis::Vertical => mouse.row.saturating_sub(hit.container.y),
                };
                let total = match hit.axis {
                    SplitAxis::Horizontal => hit.container.width,
                    SplitAxis::Vertical => hit.container.height,
                }
                .max(1);
                *ratio = ((u32::from(value) * 100) / u32::from(total)).clamp(10, 90) as u16;
                self.resize(self.last_area);
            }
            return Ok(());
        }
        if matches!(mouse.kind, MouseEventKind::Up(MouseButton::Left)) {
            self.dragging_split = None;
        }
        if matches!(mouse.kind, MouseEventKind::Down(MouseButton::Left)) {
            if let Some(hit) = self
                .hits
                .split_dividers
                .iter()
                .find(|hit| contains(hit.divider, mouse.column, mouse.row))
                .cloned()
            {
                self.dragging_split = Some(hit);
            } else if contains(self.hits.plus, mouse.column, mouse.row) {
                self.add_session()?;
            } else if contains(self.hits.toggle, mouse.column, mouse.row) {
                self.manual_collapse = !self.sidebar_collapsed;
                self.resize(self.last_area);
            } else if let Some(hit) = self
                .hits
                .tabs
                .iter()
                .find(|hit| contains(hit.close, mouse.column, mouse.row))
                .copied()
            {
                self.request_close(hit.index);
            } else if let Some(hit) = self
                .hits
                .tabs
                .iter()
                .find(|hit| contains(hit.body, mouse.column, mouse.row))
                .copied()
            {
                self.active = hit.index;
                self.resize(self.last_area);
            } else if let Some((index, _)) = self
                .hits
                .panes
                .iter()
                .find(|(_, area)| contains(*area, mouse.column, mouse.row))
                .copied()
            {
                self.active = index;
            }
        } else if matches!(mouse.kind, MouseEventKind::Down(MouseButton::Right))
            && let Some(hit) = self
                .hits
                .tabs
                .iter()
                .find(|hit| contains(hit.body, mouse.column, mouse.row))
                .copied()
        {
            self.open_context_menu(hit.index, mouse.column, mouse.row);
        } else if matches!(mouse.kind, MouseEventKind::Down(MouseButton::Right))
            && let Some((group, _)) = self
                .hits
                .groups
                .iter()
                .find(|(_, area)| contains(*area, mouse.column, mouse.row))
                .copied()
        {
            self.open_group_context_menu(group, mouse.column, mouse.row);
        }
        match mouse.kind {
            MouseEventKind::ScrollUp => self.write_active(b"\x1b[A\x1b[A\x1b[A"),
            MouseEventKind::ScrollDown => self.write_active(b"\x1b[B\x1b[B\x1b[B"),
            _ => {}
        }
        Ok(())
    }

    fn handle_paste(&mut self, text: &str) {
        if let Some(session) = self.sessions.get_mut(self.active) {
            let normalized = text.replace("\r\n", "\n").replace('\r', "\n");
            if normalized.contains('\n') {
                if let Some(command) = normalized
                    .lines()
                    .rev()
                    .find(|line| !line.trim().is_empty())
                {
                    session.last_command = Some(command.trim().to_string());
                    session
                        .pending
                        .store(true, std::sync::atomic::Ordering::Relaxed);
                }
                session.input_line = normalized
                    .rsplit('\n')
                    .next()
                    .unwrap_or_default()
                    .to_string();
            } else {
                session.input_line.push_str(&normalized);
            }
        }
        self.write_active(text.as_bytes());
    }
}

fn group_display_name(names: &std::collections::HashMap<u64, String>, group: u64) -> String {
    names
        .get(&group)
        .cloned()
        .unwrap_or_else(|| format!("Group {group}"))
}

fn detect_host_terminal() -> String {
    detect_terminal_from_env(|name| env::var(name).ok())
        .or_else(detect_terminal_from_process_tree)
        .unwrap_or_else(|| "terminal".to_string())
}

fn detect_terminal_from_env(lookup: impl Fn(&str) -> Option<String>) -> Option<String> {
    if let Some(program) = lookup("TERM_PROGRAM") {
        return Some(terminal_display_name(&program));
    }
    for (variable, label) in [
        ("KITTY_WINDOW_ID", "kitty"),
        ("GHOSTTY_RESOURCES_DIR", "Ghostty"),
        ("WEZTERM_PANE", "WezTerm"),
        ("ALACRITTY_WINDOW_ID", "Alacritty"),
        ("ALACRITTY_SOCKET", "Alacritty"),
        ("KONSOLE_VERSION", "Konsole"),
        ("WT_SESSION", "Windows Terminal"),
    ] {
        if lookup(variable).is_some() {
            return Some(label.to_string());
        }
    }
    lookup("TERM").and_then(|term| {
        let lower = term.to_ascii_lowercase();
        if lower.contains("kitty") {
            Some("kitty".to_string())
        } else if lower.contains("foot") {
            Some("Foot".to_string())
        } else if lower.contains("wezterm") {
            Some("WezTerm".to_string())
        } else {
            None
        }
    })
}

fn terminal_display_name(program: &str) -> String {
    match program.to_ascii_lowercase().as_str() {
        "kitty" => "kitty".to_string(),
        "ghostty" => "Ghostty".to_string(),
        "wezterm" => "WezTerm".to_string(),
        "alacritty" => "Alacritty".to_string(),
        "foot" => "Foot".to_string(),
        "vscode" => "VS Code".to_string(),
        "apple_terminal" => "Terminal".to_string(),
        _ => program.to_string(),
    }
}

#[cfg(target_os = "linux")]
fn detect_terminal_from_process_tree() -> Option<String> {
    let mut pid = std::process::id();
    for _ in 0..8 {
        let status = std::fs::read_to_string(format!("/proc/{pid}/status")).ok()?;
        let parent = status
            .lines()
            .find_map(|line| line.strip_prefix("PPid:\t"))?
            .trim()
            .parse::<u32>()
            .ok()?;
        if parent == 0 || parent == pid {
            break;
        }
        let command = std::fs::read_to_string(format!("/proc/{parent}/comm"))
            .unwrap_or_default()
            .trim()
            .to_ascii_lowercase();
        for (needle, label) in [
            ("kitty", "kitty"),
            ("ghostty", "Ghostty"),
            ("wezterm", "WezTerm"),
            ("alacritty", "Alacritty"),
            ("foot", "Foot"),
            ("konsole", "Konsole"),
            ("gnome-terminal", "GNOME Terminal"),
            ("xfce4-terminal", "Xfce Terminal"),
            ("tilix", "Tilix"),
            ("rio", "Rio"),
        ] {
            if command.contains(needle) {
                return Some(label.to_string());
            }
        }
        pid = parent;
    }
    None
}

#[cfg(not(target_os = "linux"))]
fn detect_terminal_from_process_tree() -> Option<String> {
    None
}

fn update_cwd_from_osc7(bytes: &[u8], cwd: &Arc<Mutex<PathBuf>>) {
    let text = String::from_utf8_lossy(bytes);
    for marker in ["\x1b]7;file://", "\u{9d}7;file://"] {
        let Some(start) = text.rfind(marker) else {
            continue;
        };
        let value = &text[start + marker.len()..];
        let value = value
            .split(['\x07', '\x1b', '\u{9c}'])
            .next()
            .unwrap_or(value);
        let path = value
            .find('/')
            .map(|index| &value[index..])
            .unwrap_or(value);
        if !path.is_empty()
            && let Ok(mut current) = cwd.lock()
        {
            *current = PathBuf::from(path.replace("%20", " "));
        }
        break;
    }
}

fn pane_geometry(app: &App, terminal: Rect) -> (Vec<(usize, Rect)>, Vec<SplitHit>) {
    let Some(active) = app.sessions.get(app.active) else {
        return (Vec::new(), Vec::new());
    };
    if let Some(group) = active.group {
        let fallback = layout_for_group(&app.sessions, group);
        let layout = app.group_layouts.get(&group).unwrap_or(&fallback);
        let mut panes = Vec::new();
        let mut dividers = Vec::new();
        collect_layout_geometry(
            layout,
            terminal,
            group,
            &mut Vec::new(),
            &app.sessions,
            &mut panes,
            &mut dividers,
        );
        (panes, dividers)
    } else {
        (vec![(app.active, terminal)], Vec::new())
    }
}

fn pane_areas_for(app: &App, terminal: Rect) -> Vec<(usize, Rect)> {
    pane_geometry(app, terminal).0
}

fn collect_layout_geometry(
    layout: &PaneLayout,
    area: Rect,
    group: u64,
    path: &mut Vec<bool>,
    sessions: &[Session],
    panes: &mut Vec<(usize, Rect)>,
    dividers: &mut Vec<SplitHit>,
) {
    match layout {
        PaneLayout::Leaf(id) => {
            if let Some(index) = sessions.iter().position(|session| session.id == *id) {
                panes.push((index, area));
            }
        }
        PaneLayout::Split {
            axis,
            ratio,
            first,
            second,
        } => {
            let total = match axis {
                SplitAxis::Horizontal => area.width,
                SplitAxis::Vertical => area.height,
            };
            let usable = total.saturating_sub(1);
            let first_len = if usable >= 2 {
                (((u32::from(usable) * u32::from(*ratio)) / 100) as u16).clamp(1, usable - 1)
            } else {
                usable
            };
            let second_len = usable.saturating_sub(first_len);
            let (first_area, divider, second_area) = match axis {
                SplitAxis::Horizontal => (
                    Rect::new(area.x, area.y, first_len, area.height),
                    Rect::new(area.x + first_len, area.y, 1, area.height),
                    Rect::new(area.x + first_len + 1, area.y, second_len, area.height),
                ),
                SplitAxis::Vertical => (
                    Rect::new(area.x, area.y, area.width, first_len),
                    Rect::new(area.x, area.y + first_len, area.width, 1),
                    Rect::new(area.x, area.y + first_len + 1, area.width, second_len),
                ),
            };
            dividers.push(SplitHit {
                group,
                path: path.clone(),
                axis: *axis,
                divider,
                container: area,
            });
            path.push(false);
            collect_layout_geometry(first, first_area, group, path, sessions, panes, dividers);
            path.pop();
            path.push(true);
            collect_layout_geometry(second, second_area, group, path, sessions, panes, dividers);
            path.pop();
        }
    }
}

fn layout_for_group(sessions: &[Session], group: u64) -> PaneLayout {
    balanced_layout(sessions, group, SplitAxis::Horizontal)
}

fn balanced_layout(sessions: &[Session], group: u64, axis: SplitAxis) -> PaneLayout {
    let ids: Vec<u64> = sessions
        .iter()
        .filter(|session| session.group == Some(group))
        .map(|session| session.id)
        .collect();
    balanced_layout_ids(&ids, axis)
}

fn balanced_layout_ids(ids: &[u64], axis: SplitAxis) -> PaneLayout {
    if ids.len() <= 1 {
        return PaneLayout::Leaf(ids.first().copied().unwrap_or_default());
    }
    let midpoint = ids.len().div_ceil(2);
    PaneLayout::Split {
        axis,
        ratio: ((midpoint * 100) / ids.len()) as u16,
        first: Box::new(balanced_layout_ids(&ids[..midpoint], axis)),
        second: Box::new(balanced_layout_ids(&ids[midpoint..], axis)),
    }
}

fn remove_layout_leaf(layout: PaneLayout, id: u64) -> Option<PaneLayout> {
    match layout {
        PaneLayout::Leaf(leaf) => (leaf != id).then_some(PaneLayout::Leaf(leaf)),
        PaneLayout::Split {
            axis,
            ratio,
            first,
            second,
        } => match (
            remove_layout_leaf(*first, id),
            remove_layout_leaf(*second, id),
        ) {
            (Some(first), Some(second)) => Some(PaneLayout::Split {
                axis,
                ratio,
                first: Box::new(first),
                second: Box::new(second),
            }),
            (Some(layout), None) | (None, Some(layout)) => Some(layout),
            (None, None) => None,
        },
    }
}

fn equalize_layout(layout: &mut PaneLayout) {
    if let PaneLayout::Split {
        ratio,
        first,
        second,
        ..
    } = layout
    {
        *ratio = 50;
        equalize_layout(first);
        equalize_layout(second);
    }
}

fn insert_layout_leaf(layout: &mut PaneLayout, target: u64, pane: u64, axis: SplitAxis) -> bool {
    if matches!(layout, PaneLayout::Leaf(id) if *id == target) {
        *layout = PaneLayout::Split {
            axis,
            ratio: 50,
            first: Box::new(PaneLayout::Leaf(target)),
            second: Box::new(PaneLayout::Leaf(pane)),
        };
        return true;
    }
    match layout {
        PaneLayout::Split { first, second, .. } => {
            insert_layout_leaf(first, target, pane, axis)
                || insert_layout_leaf(second, target, pane, axis)
        }
        PaneLayout::Leaf(_) => false,
    }
}

fn layout_at_path_mut<'a>(layout: &'a mut PaneLayout, path: &[bool]) -> Option<&'a mut PaneLayout> {
    if path.is_empty() {
        return Some(layout);
    }
    match layout {
        PaneLayout::Split { first, second, .. } => {
            layout_at_path_mut(if path[0] { second } else { first }, &path[1..])
        }
        PaneLayout::Leaf(_) => None,
    }
}

fn sidebar_order(app: &App) -> Vec<usize> {
    let mut result = Vec::with_capacity(app.sessions.len());
    let mut seen_groups = Vec::new();
    for (index, session) in app.sessions.iter().enumerate() {
        match session.group {
            Some(group) if !seen_groups.contains(&group) => {
                seen_groups.push(group);
                result.extend(
                    app.sessions
                        .iter()
                        .enumerate()
                        .filter_map(|(index, member)| {
                            (member.group == Some(group)).then_some(index)
                        }),
                );
            }
            Some(_) => {}
            None => result.push(index),
        }
    }
    result
}

fn top_level_units(app: &App) -> Vec<usize> {
    let mut units = Vec::new();
    let mut groups = Vec::new();
    for (index, session) in app.sessions.iter().enumerate() {
        match session.group {
            Some(group) if !groups.contains(&group) => {
                groups.push(group);
                units.push(index);
            }
            Some(_) => {}
            None => units.push(index),
        }
    }
    units
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
    app.spinner_tick = app.spinner_tick.wrapping_add(1);
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
    app.hits.tabs.clear();
    app.hits.panes.clear();
    app.hits.groups.clear();
    app.hits.split_dividers.clear();
    app.hits.confirm_yes = Rect::default();
    app.hits.confirm_no = Rect::default();

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
            app.hits.tabs.push(TabHit {
                index,
                body: Rect::new(
                    sidebar.x,
                    sidebar.y + 2 + index as u16,
                    sidebar.width.saturating_sub(1),
                    1,
                ),
                close: Rect::default(),
            });
        }
    } else {
        let title = Line::from(Span::styled(
            format!(" {}", app.host_terminal),
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

        let mut row = sidebar.y + 2;
        let mut previous_group = None;
        let order = sidebar_order(app);
        for (order_position, index) in order.iter().copied().enumerate() {
            let session = &app.sessions[index];
            if row + 2 >= sidebar.bottom().saturating_sub(1) {
                break;
            }
            if session.group.is_some() && session.group != previous_group {
                let group = session.group.unwrap_or_default();
                frame.render_widget(
                    Paragraph::new(Line::from(vec![
                        Span::styled(" ▾ ", Style::default().fg(Color::Rgb(110, 115, 141))),
                        Span::styled(
                            app.group_name(group),
                            Style::default()
                                .fg(Color::Rgb(137, 180, 250))
                                .add_modifier(Modifier::BOLD),
                        ),
                    ])),
                    Rect::new(sidebar.x, row, sidebar.width.saturating_sub(1), 1),
                );
                app.hits.groups.push((
                    group,
                    Rect::new(sidebar.x, row, sidebar.width.saturating_sub(1), 1),
                ));
                row += 1;
            }
            previous_group = session.group;
            let grouped = session.group.is_some();
            let group_continues = session.group.is_some()
                && order
                    .get(order_position + 1)
                    .and_then(|next| app.sessions.get(*next))
                    .is_some_and(|next| next.group == session.group);
            let active = index == app.active;
            let style = if active {
                Style::default()
                    .fg(Color::Rgb(205, 211, 240))
                    .bg(Color::Rgb(35, 36, 54))
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(Color::Rgb(166, 173, 200))
            };
            let status = if session.is_pending() {
                ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"][(app.spinner_tick / 5) % 10]
            } else {
                " "
            };
            let tree_prefix = if grouped { " │ " } else { " " };
            let first_line = Line::from(vec![
                Span::styled(tree_prefix, Style::default().fg(Color::Rgb(110, 115, 141))),
                Span::styled(
                    format!("{status} "),
                    Style::default().fg(Color::Rgb(137, 180, 250)),
                ),
                Span::styled(
                    truncate(
                        session.display_name(),
                        sidebar.width.saturating_sub(if grouped { 8 } else { 6 }) as usize,
                    ),
                    style,
                ),
            ]);
            frame.render_widget(
                Paragraph::new(first_line).style(style),
                Rect::new(sidebar.x, row, sidebar.width.saturating_sub(1), 1),
            );
            let close = Rect::new(sidebar.right().saturating_sub(4), row, 2, 1);
            frame.render_widget(
                Paragraph::new(Span::styled(
                    "×",
                    Style::default().fg(Color::Rgb(137, 180, 250)),
                ))
                .alignment(ratatui::layout::Alignment::Right),
                close,
            );
            frame.render_widget(
                Paragraph::new(Line::from(vec![
                    Span::styled(
                        if grouped && group_continues {
                            " │   "
                        } else if grouped {
                            " └   "
                        } else {
                            "   "
                        },
                        Style::default().fg(Color::Rgb(110, 115, 141)),
                    ),
                    Span::styled(
                        compact_path(
                            &session.directory_label(),
                            sidebar.width.saturating_sub(if grouped { 6 } else { 4 }) as usize,
                        ),
                        Style::default().fg(Color::Rgb(110, 115, 141)),
                    ),
                ]))
                .style(style),
                Rect::new(sidebar.x, row + 1, sidebar.width.saturating_sub(1), 1),
            );
            app.hits.tabs.push(TabHit {
                index,
                body: Rect::new(sidebar.x, row, sidebar.width.saturating_sub(1), 2),
                close,
            });
            row += 2;
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

    let (panes, split_dividers) = pane_geometry(app, terminal);
    for hit in &split_dividers {
        let symbol = match hit.axis {
            SplitAxis::Horizontal => "│",
            SplitAxis::Vertical => "─",
        };
        for y in hit.divider.y..hit.divider.bottom() {
            for x in hit.divider.x..hit.divider.right() {
                frame.buffer_mut()[(x, y)]
                    .set_symbol(symbol)
                    .set_style(Style::default().fg(Color::Rgb(69, 71, 90)));
            }
        }
    }
    app.hits.split_dividers = split_dividers;
    for (index, area) in panes.iter().copied() {
        app.hits.panes.push((index, area));
        if let Some(session) = app.sessions.get(index)
            && let Ok(parser) = session.parser.lock()
        {
            frame.render_widget(TerminalView(parser.screen()), area);
            if index == app.active {
                let (row, col) = parser.screen().cursor_position();
                if col < area.width && row < area.height {
                    frame.set_cursor_position((area.x + col, area.y + row));
                }
            }
        }
    }

    if app
        .prefix_started
        .is_some_and(|started| started.elapsed() <= Duration::from_secs(1))
    {
        let hint = " prefix: 1-0 tab  ←/→ cycle  h/j/k/l pane  T new  W close  s sidebar ";
        let width = hint.chars().count().min(terminal.width as usize) as u16;
        let hint_area = Rect::new(
            terminal.right().saturating_sub(width),
            terminal.bottom().saturating_sub(1),
            width,
            1,
        );
        frame.render_widget(
            Paragraph::new(hint).style(
                Style::default()
                    .fg(Color::Rgb(24, 24, 37))
                    .bg(Color::Rgb(137, 180, 250))
                    .add_modifier(Modifier::BOLD),
            ),
            hint_area,
        );
    }

    if let Some(menu) = &app.context_menu {
        frame.render_widget(Clear, menu.area);
        frame.render_widget(
            Paragraph::new(
                menu.entries
                    .iter()
                    .map(|(label, _)| Line::from(format!(" {label}")))
                    .collect::<Vec<_>>(),
            )
            .block(Block::default().borders(Borders::ALL).border_style(divider))
            .style(
                Style::default()
                    .bg(Color::Rgb(24, 24, 37))
                    .fg(Color::Rgb(205, 214, 244)),
            ),
            menu.area,
        );
    }

    let rename = match &app.input_mode {
        InputMode::Rename { value, .. } => Some((value, " Rename tab ")),
        InputMode::RenameGroup { value, .. } => Some((value, " Rename group ")),
        _ => None,
    };
    if let Some((value, title)) = rename {
        let width = 42.min(area.width.saturating_sub(4));
        let popup = Rect::new(
            area.x + (area.width.saturating_sub(width)) / 2,
            area.y + area.height.saturating_sub(3) / 2,
            width,
            3,
        );
        frame.render_widget(Clear, popup);
        frame.render_widget(
            Paragraph::new(value.as_str())
                .block(
                    Block::default()
                        .title(title)
                        .borders(Borders::ALL)
                        .border_style(Style::default().fg(Color::Rgb(137, 180, 250))),
                )
                .style(
                    Style::default()
                        .bg(Color::Rgb(24, 24, 37))
                        .fg(Color::Rgb(205, 214, 244)),
                ),
            popup,
        );
        frame.set_cursor_position((
            popup.x
                + 1
                + value
                    .chars()
                    .count()
                    .min(popup.width.saturating_sub(3) as usize) as u16,
            popup.y + 1,
        ));
    }

    if let InputMode::ConfirmClose { session_id } = &app.input_mode {
        let exits_workspace = app.sessions.len() == 1;
        let label = app
            .sessions
            .iter()
            .find(|session| session.id == *session_id)
            .map(Session::display_name)
            .unwrap_or("tab");
        let width = 52.min(area.width.saturating_sub(4));
        let popup = Rect::new(
            area.x + (area.width.saturating_sub(width)) / 2,
            area.y + area.height.saturating_sub(7) / 2,
            width,
            7,
        );
        frame.render_widget(Clear, popup);
        frame.render_widget(
            Paragraph::new(vec![
                Line::from(""),
                Line::from(if exits_workspace {
                    " Exit this workspace?".to_string()
                } else {
                    format!(
                        " Close “{}”?",
                        truncate(label, width.saturating_sub(12) as usize)
                    )
                }),
                Line::from(if exits_workspace {
                    " This is the last tab. Are you sure?"
                } else {
                    " A command is still running. Are you sure?"
                }),
                Line::from(""),
                Line::from(vec![
                    Span::styled(
                        if exits_workspace {
                            "   Exit [Y]    "
                        } else {
                            "   Close [Y]   "
                        },
                        Style::default()
                            .fg(Color::Rgb(243, 139, 168))
                            .add_modifier(Modifier::BOLD),
                    ),
                    Span::styled(
                        " Cancel [N] ",
                        Style::default().fg(Color::Rgb(166, 173, 200)),
                    ),
                ]),
            ])
            .block(
                Block::default()
                    .title(" Confirm close ")
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(Color::Rgb(243, 139, 168))),
            )
            .style(
                Style::default()
                    .bg(Color::Rgb(24, 24, 37))
                    .fg(Color::Rgb(205, 214, 244)),
            ),
            popup,
        );
        app.hits.confirm_yes = Rect::new(popup.x + 3, popup.y + 5, 15, 1);
        app.hits.confirm_no = Rect::new(popup.x + 18, popup.y + 5, 12, 1);
    }
}

fn truncate(value: &str, width: usize) -> String {
    if value.chars().count() <= width {
        return value.to_string();
    }
    if width < 2 {
        return "…".chars().take(width).collect();
    }
    format!("{}…", value.chars().take(width - 1).collect::<String>())
}

fn compact_path(value: &str, width: usize) -> String {
    if value.chars().count() <= width {
        return value.to_string();
    }
    if width == 0 {
        return String::new();
    }

    let prefix = if value.starts_with("~/") { "~" } else { "" };
    let trimmed = value.trim_start_matches("~/").trim_start_matches('/');
    let components: Vec<&str> = trimmed.split('/').filter(|part| !part.is_empty()).collect();
    if components.is_empty() {
        return truncate(value, width);
    }

    let mut displayed: Vec<String> = components
        .iter()
        .enumerate()
        .map(|(index, component)| {
            if index + 1 < components.len() && index % 2 == 0 {
                "…".to_string()
            } else {
                (*component).to_string()
            }
        })
        .collect();

    let render = |parts: &[String]| {
        let body = parts
            .iter()
            .fold(Vec::<&str>::new(), |mut result, part| {
                if part != "…" || result.last().copied() != Some("…") {
                    result.push(part);
                }
                result
            })
            .join("/");
        if prefix == "~" {
            format!("~/{body}")
        } else if value.starts_with('/') {
            format!("/{body}")
        } else {
            body
        }
    };

    let mut result = render(&displayed);
    for index in 0..displayed.len().saturating_sub(1) {
        if result.chars().count() <= width {
            return result;
        }
        displayed[index] = "…".into();
        result = render(&displayed);
    }
    if result.chars().count() <= width {
        return result;
    }

    let last = components.last().copied().unwrap_or_default();
    if width <= 2 {
        return "…".chars().take(width).collect();
    }
    let available = width.saturating_sub(2);
    let tail: String = last
        .chars()
        .rev()
        .take(available)
        .collect::<String>()
        .chars()
        .rev()
        .collect();
    format!("…/{tail}")
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
        if app.exit_requested {
            break;
        }
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
            Ok(Event::Paste(text)) => app.handle_paste(&text),
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

    #[test]
    fn custom_name_overrides_command_until_cleared() {
        assert_eq!(display_name_for(None, None), "New Tab");
        assert_eq!(display_name_for(None, Some("cargo test")), "cargo test");
        assert_eq!(
            display_name_for(Some("server"), Some("cargo test")),
            "server"
        );
    }

    #[test]
    fn osc7_updates_the_displayed_directory() {
        let cwd = Arc::new(Mutex::new(PathBuf::from("/old")));
        update_cwd_from_osc7(b"\x1b]7;file://host/home/user/project\x07", &cwd);
        assert_eq!(*cwd.lock().unwrap(), PathBuf::from("/home/user/project"));
    }

    #[test]
    fn long_tab_labels_are_ellipsized() {
        assert_eq!(truncate("cargo test --workspace", 10), "cargo tes…");
    }

    #[test]
    fn compacted_paths_prioritize_the_final_directory() {
        let compacted = compact_path("~/Projects/client/packages/frontend/src", 22);
        assert!(compacted.chars().count() <= 22);
        assert!(compacted.ends_with("src"));
        assert!(compacted.contains('…'));
    }

    #[test]
    fn paths_that_fit_are_not_compacted() {
        assert_eq!(compact_path("~/Projects/app", 20), "~/Projects/app");
    }

    #[test]
    fn pane_groups_use_default_or_custom_names() {
        let mut names = std::collections::HashMap::new();
        assert_eq!(group_display_name(&names, 3), "Group 3");
        names.insert(3, "Backend".to_string());
        assert_eq!(group_display_name(&names, 3), "Backend");
    }

    #[test]
    fn balanced_layout_uses_requested_orientation_and_all_panes() {
        let layout = balanced_layout_ids(&[1, 2, 3, 4], SplitAxis::Vertical);
        let PaneLayout::Split { axis, .. } = &layout else {
            panic!("multiple panes should create a split");
        };
        assert_eq!(*axis, SplitAxis::Vertical);
        assert_eq!(layout_leaf_ids(&layout), vec![1, 2, 3, 4]);
    }

    #[test]
    fn a_group_layout_can_contain_one_pane() {
        let layout = balanced_layout_ids(&[42], SplitAxis::Horizontal);
        assert_eq!(layout_leaf_ids(&layout), vec![42]);
        assert!(matches!(layout, PaneLayout::Leaf(42)));
    }

    #[test]
    fn removing_a_layout_leaf_collapses_its_empty_split() {
        let layout = balanced_layout_ids(&[1, 2, 3], SplitAxis::Horizontal);
        let layout = remove_layout_leaf(layout, 2).expect("other panes remain");
        assert_eq!(layout_leaf_ids(&layout), vec![1, 3]);
    }

    #[test]
    fn pane_can_be_inserted_below_a_specific_target() {
        let mut layout = balanced_layout_ids(&[1, 2, 3, 4], SplitAxis::Horizontal);
        let layout_without_four = remove_layout_leaf(layout, 4).expect("other panes remain");
        layout = layout_without_four;
        assert!(insert_layout_leaf(&mut layout, 3, 4, SplitAxis::Vertical));
        let PaneLayout::Split {
            axis: root_axis,
            second,
            ..
        } = &layout
        else {
            panic!("expected root split");
        };
        assert_eq!(*root_axis, SplitAxis::Horizontal);
        let PaneLayout::Split {
            axis: branch_axis, ..
        } = second.as_ref()
        else {
            panic!("expected nested split");
        };
        assert_eq!(*branch_axis, SplitAxis::Vertical);
        assert_eq!(layout_leaf_ids(&layout), vec![1, 2, 3, 4]);
    }

    #[test]
    fn host_terminal_uses_explicit_environment_markers() {
        let variables =
            std::collections::HashMap::from([("WEZTERM_PANE".to_string(), "7".to_string())]);
        assert_eq!(
            detect_terminal_from_env(|name| variables.get(name).cloned()),
            Some("WezTerm".to_string())
        );
    }

    #[test]
    fn term_program_names_are_normalized_for_display() {
        let variables =
            std::collections::HashMap::from([("TERM_PROGRAM".to_string(), "vscode".to_string())]);
        assert_eq!(
            detect_terminal_from_env(|name| variables.get(name).cloned()),
            Some("VS Code".to_string())
        );
    }

    fn layout_leaf_ids(layout: &PaneLayout) -> Vec<u64> {
        match layout {
            PaneLayout::Leaf(id) => vec![*id],
            PaneLayout::Split { first, second, .. } => {
                let mut ids = layout_leaf_ids(first);
                ids.extend(layout_leaf_ids(second));
                ids
            }
        }
    }
}
