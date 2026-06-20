use super::*;

pub(super) fn pane_geometry(app: &App, terminal: Rect) -> (Vec<(usize, Rect)>, Vec<SplitHit>) {
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

pub(super) fn pane_areas_for(app: &App, terminal: Rect) -> Vec<(usize, Rect)> {
    pane_geometry(app, terminal).0
}

pub(super) fn collect_layout_geometry(
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

pub(super) fn layout_for_group(sessions: &[Session], group: u64) -> PaneLayout {
    balanced_layout(sessions, group, SplitAxis::Horizontal)
}

pub(super) fn balanced_layout(sessions: &[Session], group: u64, axis: SplitAxis) -> PaneLayout {
    let ids: Vec<u64> = sessions
        .iter()
        .filter(|session| session.group == Some(group))
        .map(|session| session.id)
        .collect();
    balanced_layout_ids(&ids, axis)
}

pub(super) fn balanced_layout_ids(ids: &[u64], axis: SplitAxis) -> PaneLayout {
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

pub(super) fn remove_layout_leaf(layout: PaneLayout, id: u64) -> Option<PaneLayout> {
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

pub(super) fn equalize_layout(layout: &mut PaneLayout) {
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

pub(super) fn insert_layout_leaf(
    layout: &mut PaneLayout,
    target: u64,
    pane: u64,
    axis: SplitAxis,
) -> bool {
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

pub(super) fn layout_at_path_mut<'a>(
    layout: &'a mut PaneLayout,
    path: &[bool],
) -> Option<&'a mut PaneLayout> {
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
