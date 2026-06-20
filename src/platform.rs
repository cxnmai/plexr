use super::*;

pub(super) fn workspace_state_path() -> Result<PathBuf> {
    let state_root = env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .or_else(|| env::var_os("HOME").map(|home| PathBuf::from(home).join(".local/state")))
        .context("neither XDG_STATE_HOME nor HOME is set")?;
    Ok(state_root.join("plexr/workspace.json"))
}

pub(super) fn load_workspace_snapshot() -> Result<Option<WorkspaceSnapshot>> {
    load_workspace_snapshot_from(&workspace_state_path()?)
}

pub(super) fn load_workspace_snapshot_from(
    path: &std::path::Path,
) -> Result<Option<WorkspaceSnapshot>> {
    let contents = match std::fs::read_to_string(path) {
        Ok(contents) => contents,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    Ok(Some(
        serde_json::from_str(&contents).context("parse Plexr workspace snapshot")?,
    ))
}

pub(super) fn write_workspace_snapshot(snapshot: &WorkspaceSnapshot) -> Result<()> {
    write_workspace_snapshot_to(snapshot, &workspace_state_path()?)
}

pub(super) fn write_workspace_snapshot_to(
    snapshot: &WorkspaceSnapshot,
    path: &std::path::Path,
) -> Result<()> {
    let directory = path.parent().context("workspace path has no parent")?;
    std::fs::create_dir_all(directory)?;
    let temporary = path.with_extension("json.tmp");
    let contents = serde_json::to_vec_pretty(snapshot)?;
    let mut file = std::fs::File::create(&temporary)?;
    file.write_all(&contents)?;
    file.sync_all()?;
    std::fs::rename(temporary, path)?;
    Ok(())
}

pub(super) fn detect_host_terminal() -> String {
    detect_terminal_from_env(|name| env::var(name).ok())
        .or_else(detect_terminal_from_process_tree)
        .unwrap_or_else(|| "terminal".to_string())
}

pub(super) fn detect_terminal_from_env(lookup: impl Fn(&str) -> Option<String>) -> Option<String> {
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

pub(super) fn update_cwd_from_osc7(bytes: &[u8], cwd: &Arc<Mutex<PathBuf>>) {
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
