use super::*;

pub(super) fn render(frame: &mut Frame, app: &mut App) {
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
        let active_group = app
            .sessions
            .get(app.active)
            .and_then(|session| session.group);
        for (row_index, index) in sidebar_order(app)
            .into_iter()
            .take(sidebar.height.saturating_sub(2) as usize)
            .enumerate()
        {
            let active = index == app.active;
            let in_active_group = active_group.is_some()
                && app.sessions.get(index).and_then(|session| session.group) == active_group;
            let style = if active {
                Style::default()
                    .fg(Color::Black)
                    .bg(Color::Rgb(131, 166, 229))
                    .add_modifier(Modifier::BOLD)
            } else if in_active_group {
                Style::default()
                    .fg(Color::Rgb(205, 214, 244))
                    .bg(Color::Rgb(49, 50, 68))
            } else {
                Style::default().fg(Color::DarkGray)
            };
            let line = Line::from(Span::styled(collapsed_sidebar_label(app, index), style));
            frame.render_widget(
                Paragraph::new(line),
                Rect::new(
                    sidebar.x,
                    sidebar.y + 2 + row_index as u16,
                    sidebar.width.saturating_sub(1),
                    1,
                ),
            );
            app.hits.tabs.push(TabHit {
                index,
                body: Rect::new(
                    sidebar.x,
                    sidebar.y + 2 + row_index as u16,
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
            if index == app.active && !parser.screen().hide_cursor() {
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
