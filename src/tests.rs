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
fn workspace_snapshot_round_trips_to_an_atomic_json_file() {
    let path = env::temp_dir().join(format!(
        "plexr-workspace-test-{}-{}.json",
        std::process::id(),
        std::thread::current().name().unwrap_or("snapshot")
    ));
    let snapshot = WorkspaceSnapshot {
        version: WORKSPACE_VERSION,
        active_session: 9,
        next_session_id: 10,
        next_group_id: 4,
        sessions: vec![PersistedSession {
            id: 9,
            custom_name: Some("server".to_string()),
            last_command: Some("cargo run".to_string()),
            cwd: PathBuf::from("/tmp/project"),
            group: Some(3),
            running_command: Some("cargo run".to_string()),
        }],
        group_names: std::collections::HashMap::from([(3, "Backend".to_string())]),
        group_layouts: std::collections::HashMap::from([(3, PaneLayout::Leaf(9))]),
    };
    write_workspace_snapshot_to(&snapshot, &path).expect("write snapshot");
    let restored = load_workspace_snapshot_from(&path)
        .expect("read snapshot")
        .expect("snapshot exists");
    assert_eq!(restored.active_session, 9);
    assert_eq!(restored.sessions[0].custom_name.as_deref(), Some("server"));
    assert_eq!(
        restored.sessions[0].running_command.as_deref(),
        Some("cargo run")
    );
    assert_eq!(restored.sessions[0].cwd, PathBuf::from("/tmp/project"));
    assert_eq!(
        restored.group_names.get(&3).map(String::as_str),
        Some("Backend")
    );
    assert_eq!(
        layout_leaf_ids(restored.group_layouts.get(&3).expect("group layout")),
        vec![9]
    );
    let _ = std::fs::remove_file(path);
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

#[test]
fn sgr_mouse_wheel_uses_one_based_pane_coordinates() {
    assert_eq!(
        encode_mouse_wheel(vt100::MouseProtocolEncoding::Sgr, true, 4, 2),
        b"\x1b[<64;5;3M"
    );
    assert_eq!(
        encode_mouse_wheel(vt100::MouseProtocolEncoding::Sgr, false, 0, 0),
        b"\x1b[<65;1;1M"
    );
}

#[test]
fn legacy_mouse_wheel_uses_x10_encoding() {
    assert_eq!(
        encode_mouse_wheel(vt100::MouseProtocolEncoding::Default, true, 0, 0),
        vec![0x1b, b'[', b'M', 96, 33, 33]
    );
}

#[test]
fn vt100_scrollback_stores_scrolled_content() {
    let mut parser = vt100::Parser::new(5, 40, 100);
    for i in 0..10 {
        let ch = (b'a' + i) as char;
        parser.process(format!("{ch} line {i}\r\n").as_bytes());
    }
    let before = parser.screen().cell(0, 0).map(|c| c.contents().to_string());
    parser.screen_mut().set_scrollback(6);
    let after = parser.screen().cell(0, 0).map(|c| c.contents().to_string());
    assert_ne!(
        before, after,
        "scrollback should change cell(0,0) content"
    );
    assert_eq!(after, Some("a".into()));
    parser.screen_mut().set_scrollback(0);
    let reset = parser.screen().cell(0, 0).map(|c| c.contents().to_string());
    assert_eq!(reset, before, "scrollback=0 should restore original view");
}

#[test]
fn scroll_up_populates_scrollback() {
    let mut parser = vt100::Parser::new(3, 40, 100);
    assert_eq!(parser.screen().scrollback(), 0);
    for i in 0..6 {
        parser.process(format!("row {}\r\n", i).as_bytes());
    }
    // 6 lines into a 3-row terminal = 3 lines should be in scrollback
    parser.screen_mut().set_scrollback(3);
    let cell = parser.screen().cell(0, 0).map(|c| c.contents());
    assert_eq!(cell, Some("r".into()), "should show 'row 0' from scrollback");
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

#[test]
fn background_color_is_preserved_through_el_and_text() {
    // Simulate a TUI app clearing a line with a specific background color, then writing text
    let mut parser = vt100::Parser::new(5, 40, 0);
    
    // Set background color to dark gray (indexed 236), then clear entire line
    parser.process(b"\x1b[48;5;236m\x1b[2K");
    // All cells in the row should now have bgcolor = Idx(236)
    let cell = parser.screen().cell(0, 0).unwrap();
    assert_eq!(cell.bgcolor(), vt100::Color::Idx(236), 
        "Cell after EL 2 with bg set should have background color");
    assert!(!cell.has_contents(), "Cleared cell should have no content");
    
    // Write text with reset attributes
    parser.process(b"\x1b[mhello");
    let cell = parser.screen().cell(0, 0).unwrap();
    assert_eq!(cell.contents(), "h");
    // Text written after reset should have default bg
    assert_eq!(cell.bgcolor(), vt100::Color::Default);
}

#[test]
fn background_color_is_preserved_through_sgr_and_text() {
    let mut parser = vt100::Parser::new(5, 40, 0);
    
    // Set bg and write text
    parser.process(b"\x1b[44mcolored text");
    let cell = parser.screen().cell(0, 0).unwrap();
    assert_eq!(cell.contents(), "c");
    assert_eq!(cell.bgcolor(), vt100::Color::Idx(4), 
        "Text with blue bg should have bg = Idx(4)");
}

#[test]
fn ed2_clears_screen_with_current_background() {
    let mut parser = vt100::Parser::new(5, 40, 0);
    
    // Write some text, set background, then clear entire screen
    parser.process(b"some text");
    parser.process(b"\x1b[43m\x1b[2J");
    
    // All cells should have bgcolor = Idx(3) (yellow)
    for row in 0..5 {
        let cell = parser.screen().cell(row, 0).unwrap();
        assert_eq!(cell.bgcolor(), vt100::Color::Idx(3),
            "Row {row} should have yellow bg after ED 2");
    }
}

#[test]
fn terminal_view_renders_background_colors() {
    use ratatui::buffer::Buffer;
    use ratatui::layout::Rect;
    
    let mut parser = vt100::Parser::new(3, 10, 0);
    parser.process(b"\x1b[41m\x1b[2Khello");
    
    let mut buf = Buffer::empty(Rect::new(0, 0, 10, 3));
    let view = TerminalView(parser.screen());
    view.render(Rect::new(0, 0, 10, 3), &mut buf);
    
    // Cell at (0,0) should have bg = Indexed(1) (red)
    // h from "hello" was written AFTER reset (\x1b[m), so bg should be Reset/default
    // Wait - there's no reset in the test. Let me check:
    // Actually we set bg=41, then EL, then wrote "hello". The "hello" is NOT preceded by \x1b[m
    // So the text cells inherit bg=Idx(1)
    let cell0 = buf[(0, 0)].style(); // cell for 'h'
    // Actually \x1b[41m\x1b[2Khello:
    // - \x1b[41m sets bg = red
    // - \x1b[2K erases line (all cells get current attrs with bg=red, content empty)
    // - "hello" writes text at cursor (row 0, col 0), cells get same attrs (bg=red)
    assert_eq!(cell0.bg, Some(Color::Indexed(1)), 
        "Cell 'h' should have red background");
}
