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
        self, DisableBracketedPaste, EnableBracketedPaste,
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
use serde::{Deserialize, Serialize};

mod layout;
mod platform;
mod terminal;
mod ui;
use layout::*;
use platform::*;
use terminal::*;
use ui::render;

const SIDEBAR_WIDTH: u16 = 29;
const COLLAPSED_WIDTH: u16 = 4;
const MOBILE_THRESHOLD: u16 = 72;
const WORKSPACE_VERSION: u32 = 1;

struct Session {
    id: u64,
    custom_name: Option<String>,
    last_command: Option<String>,
    input_line: String,
    cwd: Arc<Mutex<PathBuf>>,
    pending: Arc<std::sync::atomic::AtomicBool>,
    group: Option<u64>,
    parser: Arc<Mutex<vt100::Parser>>,
    writer: Arc<Mutex<Box<dyn Write + Send>>>,
    master: Box<dyn MasterPty + Send>,
    _child: Box<dyn Child + Send + Sync>,
    shell_pid: Option<u32>,
    restore_grace_until: Option<Instant>,
}

impl Session {
    fn spawn(id: u64, rows: u16, cols: u16, cwd: PathBuf) -> Result<Self> {
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
        let cwd = if cwd.is_dir() {
            cwd
        } else {
            env::current_dir()?
        };
        command.cwd(&cwd);
        command.env("TERM", "xterm-256color");
        command.env("COLORTERM", "truecolor");
        let child = pair.slave.spawn_command(command).context("start shell")?;
        let shell_pid = child.process_id();
        drop(pair.slave);

        let mut reader = pair.master.try_clone_reader().context("clone PTY reader")?;
        let writer = Arc::new(Mutex::new(pair.master.take_writer().context("open PTY writer")?));
        let parser = Arc::new(Mutex::new(vt100::Parser::new(rows, cols, 10_000)));
        let cwd = Arc::new(Mutex::new(cwd));
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
            restore_grace_until: None,
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
        if self
            .restore_grace_until
            .is_some_and(|until| Instant::now() < until)
        {
            return true;
        }
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

    fn scroll_viewport(&self, delta: isize) {
        if let Ok(mut parser) = self.parser.lock() {
            let screen = parser.screen_mut();
            let current = screen.scrollback();
            let next = if delta.is_negative() {
                current.saturating_sub(delta.unsigned_abs())
            } else {
                current.saturating_add(delta as usize)
            };
            screen.set_scrollback(next);
        }
    }

    fn wheel_input(&self, up: bool, column: u16, row: u16) -> Option<Vec<u8>> {
        let parser = self.parser.lock().ok()?;
        let screen = parser.screen();
        if screen.mouse_protocol_mode() == vt100::MouseProtocolMode::None {
            return None;
        }
        Some(encode_mouse_wheel(
            screen.mouse_protocol_encoding(),
            up,
            column,
            row,
        ))
    }
}

fn display_name_for<'a>(custom_name: Option<&'a str>, last_command: Option<&'a str>) -> &'a str {
    custom_name.or(last_command).unwrap_or("New Tab")
}

fn group_display_name(names: &std::collections::HashMap<u64, String>, group: u64) -> String {
    names
        .get(&group)
        .cloned()
        .unwrap_or_else(|| format!("Group {group}"))
}

#[derive(Clone, Copy)]
struct TabHit {
    index: usize,
    body: Rect,
    close: Rect,
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq, Serialize)]
enum SplitAxis {
    Horizontal,
    Vertical,
}

#[derive(Clone, Deserialize, Serialize)]
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

#[derive(Deserialize, Serialize)]
struct WorkspaceSnapshot {
    version: u32,
    active_session: u64,
    next_session_id: u64,
    next_group_id: u64,
    sessions: Vec<PersistedSession>,
    group_names: std::collections::HashMap<u64, String>,
    group_layouts: std::collections::HashMap<u64, PaneLayout>,
}

#[derive(Deserialize, Serialize)]
struct PersistedSession {
    id: u64,
    custom_name: Option<String>,
    last_command: Option<String>,
    cwd: PathBuf,
    group: Option<u64>,
    running_command: Option<String>,
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
    clear_workspace_on_exit: bool,
    prefix_started: Option<Instant>,
    dragging_split: Option<SplitHit>,
}

impl App {
    fn group_name(&self, group: u64) -> String {
        group_display_name(&self.group_names, group)
    }

    fn new(area: Rect) -> Result<Self> {
        let terminal = terminal_area(area, false);
        let snapshot = load_workspace_snapshot()
            .ok()
            .flatten()
            .filter(|snapshot| snapshot.version == WORKSPACE_VERSION);
        let mut sessions = Vec::new();
        let mut commands_to_restart = Vec::new();
        let mut active_session = None;
        let mut next_session_id = 2;
        let mut next_group_id = 1;
        let mut group_names = std::collections::HashMap::new();
        let mut group_layouts = std::collections::HashMap::new();

        if let Some(snapshot) = snapshot {
            active_session = Some(snapshot.active_session);
            next_session_id = snapshot.next_session_id;
            next_group_id = snapshot.next_group_id;
            group_names = snapshot.group_names;
            group_layouts = snapshot.group_layouts;
            for persisted in snapshot.sessions {
                let mut session = Session::spawn(
                    persisted.id,
                    terminal.height.max(2),
                    terminal.width.max(2),
                    persisted.cwd,
            )?;
                session.custom_name = persisted.custom_name;
                session.group = persisted.group;
                if persisted.running_command.is_some() {
                    session.last_command = persisted.last_command;
                }
                if let Some(command) = persisted.running_command {
                    commands_to_restart.push((persisted.id, command));
                }
                sessions.push(session);
            }
        }
        if sessions.is_empty() {
            sessions.push(Session::spawn(
                1,
                terminal.height.max(2),
                terminal.width.max(2),
                env::current_dir()?,
            )?);
        }
        next_session_id = next_session_id.max(
            sessions
                .iter()
                .map(|session| session.id + 1)
                .max()
                .unwrap_or(2),
        );
        let active = active_session
            .and_then(|id| sessions.iter().position(|session| session.id == id))
            .unwrap_or(0);
        let mut app = Self {
            host_terminal: detect_host_terminal(),
            sessions,
            active,
            sidebar_collapsed: false,
            manual_collapse: false,
            hits: HitAreas::default(),
            last_area: area,
            next_session_id,
            next_group_id,
            group_names,
            group_layouts,
            context_menu: None,
            input_mode: InputMode::Normal,
            spinner_tick: 0,
            exit_requested: false,
            clear_workspace_on_exit: false,
            prefix_started: None,
            dragging_split: None,
        };
        app.cleanup_groups();
        for (session_id, command) in commands_to_restart {
            if let Some(index) = app
                .sessions
                .iter()
                .position(|session| session.id == session_id)
            {
                app.sessions[index]
                    .pending
                    .store(true, std::sync::atomic::Ordering::Relaxed);
                app.sessions[index].restore_grace_until =
                    Some(Instant::now() + Duration::from_secs(3));
                let command = format!("{command}\r");
                if let Ok(mut w) = app.sessions[index].writer.lock() {
                    let _ = w.write_all(command.as_bytes());
                    let _ = w.flush();
                }
            }
        }
        Ok(app)
    }

    fn add_session(&mut self) -> Result<()> {
        let area = self.hits.terminal;
        let cwd = self
            .sessions
            .get(self.active)
            .and_then(|session| session.cwd.lock().ok().map(|cwd| cwd.clone()))
            .unwrap_or(env::current_dir()?);
        self.sessions.push(Session::spawn(
            self.next_session_id,
            area.height.max(2),
            area.width.max(2),
            cwd,
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
            if let Ok(mut w) = session.writer.lock() {
                let _ = w.write_all(bytes);
                let _ = w.flush();
            }
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
            self.clear_workspace_on_exit = true;
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
                        if !session.is_pending() {
                            session.last_command = Some(command);
                        }
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
        if let MouseEventKind::ScrollUp | MouseEventKind::ScrollDown = mouse.kind
            && let Some((index, area)) = self
                .hits
                .panes
                .iter()
                .find(|(_, area)| contains(*area, mouse.column, mouse.row))
                .copied()
        {
            let up = matches!(mouse.kind, MouseEventKind::ScrollUp);
            let column = mouse.column.saturating_sub(area.x);
            let row = mouse.row.saturating_sub(area.y);
            self.active = index;
            if let Some(bytes) = self.sessions[index].wheel_input(up, column, row) {
                if let Ok(mut w) = self.sessions[index].writer.lock() {
                    let _ = w.write_all(&bytes);
                    let _ = w.flush();
                }
            }
            self.sessions[index].scroll_viewport(if up { 3 } else { -3 });
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
                    if !session.is_pending() {
                        session.last_command = Some(command.trim().to_string());
                    }
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

    fn save_workspace(&self) -> Result<()> {
        let snapshot = if self.clear_workspace_on_exit {
            WorkspaceSnapshot {
                version: WORKSPACE_VERSION,
                active_session: 0,
                next_session_id: 1,
                next_group_id: 1,
                sessions: Vec::new(),
                group_names: std::collections::HashMap::new(),
                group_layouts: std::collections::HashMap::new(),
            }
        } else {
            WorkspaceSnapshot {
                version: WORKSPACE_VERSION,
                active_session: self
                    .sessions
                    .get(self.active)
                    .map_or(0, |session| session.id),
                next_session_id: self.next_session_id,
                next_group_id: self.next_group_id,
                sessions: self
                    .sessions
                    .iter()
                    .map(|session| PersistedSession {
                        id: session.id,
                        custom_name: session.custom_name.clone(),
                        last_command: session.last_command.clone(),
                        cwd: session
                            .cwd
                            .lock()
                            .map(|cwd| cwd.clone())
                            .unwrap_or_default(),
                        group: session.group,
                        running_command: session
                            .is_pending()
                            .then(|| session.last_command.clone())
                            .flatten(),
                    })
                    .collect(),
                group_names: self.group_names.clone(),
                group_layouts: self.group_layouts.clone(),
            }
        };
        write_workspace_snapshot(&snapshot)
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

fn collapsed_sidebar_label(app: &App, index: usize) -> String {
    let units = top_level_units(app);
    let session = &app.sessions[index];
    let unit_position = units.iter().position(|unit_index| {
        *unit_index == index
            || session.group.is_some()
                && app.sessions.get(*unit_index).and_then(|unit| unit.group) == session.group
    });
    let is_group_child = session
        .group
        .is_some_and(|_| unit_position.is_some_and(|position| units[position] != index));
    if is_group_child {
        return " · ".to_string();
    }
    match unit_position {
        Some(position @ 0..=8) => format!(" {} ", position + 1),
        Some(9) => " 0 ".to_string(),
        Some(_) => " · ".to_string(),
        None => "   ".to_string(),
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
        // Disable mouse tracking (1000) and SGR encoding (1006)
        let _ = io::stdout().write_all(b"\x1b[?1000l\x1b[?1006l");
        let _ = execute!(
            io::stdout(),
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
        EnableBracketedPaste
    )?;
    // Enable mouse tracking (1000) and SGR encoding (1006), but NOT button-event tracking (1002)
    // which would prevent text selection via click-and-drag.
    io::stdout().write_all(b"\x1b[?1000h\x1b[?1006h")?;
    io::stdout().flush()?;
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
    let mut last_save = Instant::now();
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
        if last_save.elapsed() >= Duration::from_millis(500) {
            let _ = app.save_workspace();
            last_save = Instant::now();
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
    app.save_workspace()?;
    Ok(())
}

#[cfg(test)]
mod tests;
