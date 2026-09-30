use std::collections::{HashMap, HashSet};

use ratatui::{
    buffer::Buffer,
    layout::Rect,
    style::{Modifier, Style},
    text::Line,
    widgets::{Paragraph, Widget},
};

use super::*;

pub(super) struct AgentRow {
    pub(super) pane_id: String,
    pub(super) status: crate::api::schema::AgentStatus,
    pub(super) focused: bool,
    pub(super) rows: Vec<Vec<crate::ui::ResolvedToken>>,
    /// Nesting depth in the dispatch hierarchy (0 = top level).
    pub(super) depth: usize,
    /// Whether this row is the last child of its parent, for tree connectors.
    pub(super) last_child: bool,
    /// Number of dispatch descendants anywhere below this row.
    pub(super) descendant_count: usize,
    /// Blocked agents below this row (excluding the row itself).
    pub(super) descendants_blocked: usize,
    /// Whether this row has children hidden behind a collapse.
    pub(super) collapsed: bool,
    /// Whether this row has any dispatch children at all.
    pub(super) has_children: bool,
}

pub(super) struct AgentTreeEntry {
    pub(super) pane_id: String,
    pub(super) depth: usize,
    pub(super) last_child: bool,
    pub(super) descendant_count: usize,
    pub(super) descendants_blocked: usize,
    pub(super) collapsed: bool,
    pub(super) has_children: bool,
}

/// Flat order used by the agents panel when no dispatch hierarchy applies.
fn agent_base_order(
    snapshot: &ClientShellSnapshot,
    sort: crate::config::AgentPanelSortConfig,
) -> Vec<String> {
    if snapshot.agent_view_label.is_some() {
        return snapshot
            .agent_order
            .iter()
            .filter(|pane_id| {
                snapshot
                    .agents
                    .iter()
                    .any(|agent| agent.pane_id == pane_id.as_str())
            })
            .cloned()
            .collect();
    }
    let mut agents = snapshot.agents.iter().collect::<Vec<_>>();
    if sort == crate::config::AgentPanelSortConfig::Priority {
        agents.sort_by_key(|agent| {
            (
                std::cmp::Reverse(status_priority(agent.agent_status)),
                std::cmp::Reverse(agent.state_change_seq),
            )
        });
    }
    agents
        .into_iter()
        .map(|agent| agent.pane_id.clone())
        .collect()
}

/// Walk the agents panel in dispatch order: every parent directly precedes its
/// children, and children of a collapsed parent are skipped.
pub(super) fn agent_tree_entries(
    snapshot: &ClientShellSnapshot,
    sort: crate::config::AgentPanelSortConfig,
    collapsed: &HashSet<String>,
) -> Vec<AgentTreeEntry> {
    let order = agent_base_order(snapshot, sort);
    if order.is_empty() {
        return Vec::new();
    }
    let index_of = order
        .iter()
        .enumerate()
        .map(|(index, pane_id)| (pane_id.as_str(), index))
        .collect::<HashMap<_, _>>();
    let statuses = order
        .iter()
        .map(|pane_id| {
            snapshot
                .agents
                .iter()
                .find(|agent| agent.pane_id == *pane_id)
                .map(|agent| agent.agent_status)
                .unwrap_or(crate::api::schema::AgentStatus::Unknown)
        })
        .collect::<Vec<_>>();
    let mut parent = vec![None; order.len()];
    for (index, pane_id) in order.iter().enumerate() {
        let Some(agent) = snapshot
            .agents
            .iter()
            .find(|agent| agent.pane_id == *pane_id)
        else {
            continue;
        };
        let Some(parent_id) = agent.parent_pane_id.as_deref() else {
            continue;
        };
        let Some(&candidate) = index_of.get(parent_id) else {
            // A parent that is gone or filtered out re-roots its children.
            continue;
        };
        if candidate == index || creates_agent_cycle(&parent, index, candidate) {
            continue;
        }
        parent[index] = Some(candidate);
    }
    let mut children = vec![Vec::new(); order.len()];
    let mut roots = Vec::new();
    for (index, parent_index) in parent.iter().enumerate() {
        match parent_index {
            Some(parent_index) => children[*parent_index].push(index),
            None => roots.push(index),
        }
    }
    if sort == crate::config::AgentPanelSortConfig::Priority {
        roots.sort_by_key(|index| {
            (
                std::cmp::Reverse(subtree_stats(*index, &children, &statuses).0),
                *index,
            )
        });
    }
    let mut entries = Vec::new();
    for root in roots {
        push_agent_tree_entry(
            &mut entries,
            root,
            0,
            false,
            &children,
            &statuses,
            collapsed,
            &order,
        );
    }
    entries
}

fn creates_agent_cycle(parent: &[Option<usize>], index: usize, candidate: usize) -> bool {
    let mut cursor = Some(candidate);
    let mut steps = 0;
    while let Some(current) = cursor {
        if current == index {
            return true;
        }
        steps += 1;
        if steps > parent.len() {
            return true;
        }
        cursor = parent[current];
    }
    false
}

/// Return `(highest status priority, descendant count, blocked descendants)`.
fn subtree_stats(
    index: usize,
    children: &[Vec<usize>],
    statuses: &[crate::api::schema::AgentStatus],
) -> (u8, usize, usize) {
    let mut priority = status_priority(statuses[index]);
    let mut count = 0usize;
    let mut blocked = 0usize;
    for &child in &children[index] {
        let (child_priority, child_count, child_blocked) = subtree_stats(child, children, statuses);
        priority = priority.max(child_priority);
        count += child_count + 1;
        blocked += child_blocked
            + usize::from(statuses[child] == crate::api::schema::AgentStatus::Blocked);
    }
    (priority, count, blocked)
}

#[allow(clippy::too_many_arguments)]
fn push_agent_tree_entry(
    entries: &mut Vec<AgentTreeEntry>,
    index: usize,
    depth: usize,
    last_child: bool,
    children: &[Vec<usize>],
    statuses: &[crate::api::schema::AgentStatus],
    collapsed: &HashSet<String>,
    order: &[String],
) {
    let pane_id = &order[index];
    let (_, descendant_count, descendants_blocked) = subtree_stats(index, children, statuses);
    let has_children = !children[index].is_empty();
    let is_collapsed = has_children && collapsed.contains(pane_id.as_str());
    entries.push(AgentTreeEntry {
        pane_id: pane_id.clone(),
        depth,
        last_child,
        descendant_count,
        descendants_blocked,
        collapsed: is_collapsed,
        has_children,
    });
    if is_collapsed {
        return;
    }
    let child_count = children[index].len();
    for (position, child) in children[index].iter().copied().enumerate() {
        push_agent_tree_entry(
            entries,
            child,
            depth + 1,
            position + 1 == child_count,
            children,
            statuses,
            collapsed,
            order,
        );
    }
}

/// Flat dispatch order used by cycling and compact surfaces. Children follow
/// their parent; a collapsed set is only used by the rendered panel.
pub(super) fn ordered_agent_pane_ids(
    snapshot: &ClientShellSnapshot,
    sort: crate::config::AgentPanelSortConfig,
) -> Vec<String> {
    agent_tree_entries(snapshot, sort, &HashSet::new())
        .into_iter()
        .map(|entry| entry.pane_id)
        .collect()
}

pub(super) fn render_agent_panel(
    buffer: &mut Buffer,
    area: Rect,
    snapshot: &ClientShellSnapshot,
    config: &ClientShellConfig,
    collapsed_agent_parents: &HashSet<String>,
    agent_scroll: &mut usize,
    hits: &mut ShellHitMap,
) {
    if !render_agent_panel_header(
        buffer,
        area,
        snapshot.agent_view_label.as_deref(),
        config,
        hits,
    ) {
        return;
    }

    let rows = agent_rows(snapshot, config, None, collapsed_agent_parents);
    render_agent_list(
        buffer,
        area,
        &rows,
        snapshot
            .agent_view_label
            .as_ref()
            .map(|_| " no matching agents"),
        config,
        agent_scroll,
        hits,
        |row| row.rows.len(),
        |buffer, rect, row, hits| {
            hits.agents.push((rect, row.pane_id.clone()));
            if let Some(toggle) = render_agent_row(buffer, rect, row, config) {
                hits.agent_toggles.push((toggle, row.pane_id.clone()));
            }
        },
    );
}

pub(super) fn render_agent_panel_header(
    buffer: &mut Buffer,
    area: Rect,
    agent_view_label: Option<&str>,
    config: &ClientShellConfig,
    hits: &mut ShellHitMap,
) -> bool {
    if area.height == 0 {
        return false;
    }
    put_text(
        buffer,
        area.x,
        area.y,
        area.width,
        &"─".repeat(area.width as usize),
        Style::default().fg(config.palette.surface_dim),
    );
    if area.height < 2 {
        return false;
    }
    put_text(
        buffer,
        area.x,
        area.y + 1,
        area.width,
        " agents",
        Style::default()
            .fg(config.palette.overlay0)
            .add_modifier(Modifier::BOLD),
    );
    let sort_label = agent_view_label.unwrap_or(match config.agent_panel_sort {
        crate::config::AgentPanelSortConfig::Spaces => "grouped",
        crate::config::AgentPanelSortConfig::Priority => "priority",
    });
    let sort_width = display_width(sort_label).min(area.width as usize) as u16;
    let sort_rect = Rect::new(
        area.right().saturating_sub(sort_width),
        area.y + 1,
        sort_width,
        1,
    );
    hits.agent_sort_toggle = if config.mouse_capture && agent_view_label.is_none() {
        sort_rect
    } else {
        Rect::default()
    };
    put_text(
        buffer,
        sort_rect.x,
        sort_rect.y,
        sort_rect.width,
        sort_label,
        Style::default()
            .fg(if agent_view_label.is_some() {
                config.palette.accent
            } else {
                config.palette.overlay0
            })
            .add_modifier(Modifier::BOLD),
    );
    true
}

pub(super) fn render_agent_list<T>(
    buffer: &mut Buffer,
    area: Rect,
    rows: &[T],
    empty_message: Option<&str>,
    config: &ClientShellConfig,
    agent_scroll: &mut usize,
    hits: &mut ShellHitMap,
    row_lines: impl Fn(&T) -> usize,
    mut render_row: impl FnMut(&mut Buffer, Rect, &T, &mut ShellHitMap),
) {
    let body = Rect::new(
        area.x,
        area.y.saturating_add(3),
        area.width,
        area.height.saturating_sub(3),
    );
    hits.agent_body = body;
    if body.is_empty() || rows.is_empty() {
        *agent_scroll = 0;
        if let Some(message) = empty_message.filter(|_| !body.is_empty()) {
            put_text(
                buffer,
                body.x,
                body.y,
                body.width,
                message,
                Style::default()
                    .fg(config.palette.overlay0)
                    .add_modifier(Modifier::DIM),
            );
        }
        return;
    }

    let row_heights = rows
        .iter()
        .map(|row| row_lines(row).max(1).min(u16::MAX as usize) as u16)
        .collect::<Vec<_>>();
    let gaps = rows
        .iter()
        .enumerate()
        .map(|(index, _)| {
            if index + 1 < rows.len() {
                config.agents.row_gap
            } else {
                0
            }
        })
        .collect::<Vec<_>>();
    let metrics =
        super::scroll::list_scroll_metrics(&row_heights, &gaps, body.height, *agent_scroll);
    hits.agent_max_scroll = metrics.max_offset_from_bottom;
    hits.agent_scroll_metrics = Some(metrics);
    *agent_scroll = metrics
        .max_offset_from_bottom
        .saturating_sub(metrics.offset_from_bottom);
    let show_scrollbar = metrics.max_offset_from_bottom > 0 && body.width > 1;
    let content_width = body.width.saturating_sub(u16::from(show_scrollbar));
    let mut y = body.y;
    for (index, row) in rows.iter().enumerate().skip(*agent_scroll) {
        let height = row_heights[index].min(body.height);
        if y.saturating_add(height) > body.bottom() {
            break;
        }
        let rect = Rect::new(body.x, y, content_width, height);
        render_row(buffer, rect, row, hits);
        y = y
            .saturating_add(height)
            .saturating_add(if index + 1 < rows.len() {
                config.agents.row_gap
            } else {
                0
            });
    }

    if show_scrollbar {
        let track = Rect::new(body.right().saturating_sub(1), body.y, 1, body.height);
        hits.agent_scrollbar = track;
        super::scroll::render_list_scrollbar(buffer, track, metrics, &config.palette);
    }
}

pub(super) fn agent_rows(
    snapshot: &ClientShellSnapshot,
    config: &ClientShellConfig,
    machine: Option<&str>,
    collapsed: &HashSet<String>,
) -> Vec<AgentRow> {
    agent_tree_entries(snapshot, config.agent_panel_sort, collapsed)
        .into_iter()
        .filter_map(|entry| {
            let mut row = agent_row(snapshot, &entry.pane_id, config, machine)?;
            row.depth = entry.depth;
            row.last_child = entry.last_child;
            row.descendant_count = entry.descendant_count;
            row.descendants_blocked = entry.descendants_blocked;
            row.collapsed = entry.collapsed;
            row.has_children = entry.has_children;
            Some(row)
        })
        .collect()
}

pub(super) fn agent_row(
    snapshot: &ClientShellSnapshot,
    pane_id: &str,
    config: &ClientShellConfig,
    machine: Option<&str>,
) -> Option<AgentRow> {
    let agent = snapshot
        .agents
        .iter()
        .find(|agent| agent.pane_id == pane_id)?;
    let workspace = snapshot
        .workspaces
        .iter()
        .find(|workspace| workspace.workspace_id == agent.workspace_id)?;
    let tab = snapshot.tabs.iter().find(|tab| tab.tab_id == agent.tab_id);
    let pane = snapshot
        .panes
        .iter()
        .find(|pane| pane.pane_id == agent.pane_id);
    let tab_count = snapshot
        .tabs
        .iter()
        .filter(|candidate| candidate.workspace_id == agent.workspace_id)
        .count();
    let tab_label = tab
        .filter(|tab| tab_count > 1 || tab.custom_label)
        .map(|tab| tab.label.as_str());
    let agent_label = agent
        .display_agent
        .as_deref()
        .or(agent.name.as_deref())
        .or(agent.agent.as_deref())
        .or(agent.title.as_deref());
    let labels = agent
        .state_labels
        .iter()
        .cloned()
        .collect::<HashMap<_, _>>();
    let tokens = agent.tokens.iter().cloned().collect::<HashMap<_, _>>();
    let state_text = labels
        .get(status_text(agent.agent_status))
        .map(String::as_str)
        .unwrap_or_else(|| sidebar_status_text(agent.agent_status));
    let canonical_agent = agent
        .agent
        .as_deref()
        .and_then(crate::detect::parse_agent_label);
    let rows = crate::ui::sidebar_agent_rows(
        &config.agents,
        crate::ui::AgentTokenContext {
            machine,
            workspace: &workspace.label,
            tab: tab_label,
            pane: agent
                .title
                .as_deref()
                .or_else(|| pane.and_then(|pane| pane.label.as_deref())),
            agent_label,
            terminal_title: agent.terminal_title.as_deref(),
            terminal_title_stripped: agent.terminal_title_stripped.as_deref(),
            canonical_agent,
            tokens: &tokens,
        },
        state_text,
    );
    Some(AgentRow {
        pane_id: agent.pane_id.clone(),
        status: agent.agent_status,
        focused: agent.focused,
        rows,
        depth: 0,
        last_child: false,
        descendant_count: 0,
        descendants_blocked: 0,
        collapsed: false,
        has_children: false,
    })
}

pub(super) fn render_agent_row(
    buffer: &mut Buffer,
    rect: Rect,
    row: &AgentRow,
    config: &ClientShellConfig,
) -> Option<Rect> {
    let palette = &config.palette;
    let row_style = if row.focused {
        Style::default().bg(palette.active_row_bg)
    } else {
        Style::default()
    };
    let name_style = if row.focused {
        Style::default()
            .fg(palette.text)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default()
            .fg(palette.subtext0)
            .add_modifier(Modifier::BOLD)
    };
    let status_style = Style::default().fg(status_color(row.status, palette));
    let secondary = Style::default().fg(palette.overlay0);
    let icon = (
        status_icon(row.status, config.status_indicators),
        Style::default().fg(status_color(row.status, palette)),
    );
    let rows = if row.rows.is_empty() {
        vec![vec![crate::ui::ResolvedToken {
            kind: crate::ui::ResolvedTokenKind::StateIcon,
            style: Default::default(),
        }]]
    } else {
        row.rows.clone()
    };
    let toggle = row
        .has_children
        .then(|| Rect::new(rect.right().saturating_sub(1), rect.y, 1, 1));
    let last_index = rows.len().saturating_sub(1);
    for (index, tokens) in rows.iter().take(rect.height as usize).enumerate() {
        let (first_prefix, continuation_prefix) = agent_tree_prefix(row);
        let prefix = if index == 0 {
            &first_prefix
        } else {
            &continuation_prefix
        };
        let indent = display_width(prefix);
        let collapsed_suffix = (index == last_index && row.collapsed && row.descendant_count > 0)
            .then(|| format!("+{}", row.descendant_count));
        let blocked_suffix = (index == last_index && row.collapsed && row.descendants_blocked > 0)
            .then(|| format!(" \u{d7}{}", row.descendants_blocked));
        let suffix_width = collapsed_suffix.as_deref().map_or(0, display_width)
            + blocked_suffix.as_deref().map_or(0, display_width);
        let toggle_reserve = usize::from(toggle.is_some() && index == 0);
        let content_width = (rect.width as usize)
            .saturating_sub(indent)
            .saturating_sub(suffix_width)
            .saturating_sub(toggle_reserve);
        let mut spans = vec![ratatui::text::Span::styled(
            prefix.clone(),
            Style::default().fg(palette.overlay0),
        )];
        spans.extend(crate::ui::resolved_token_spans(
            tokens,
            icon,
            status_style,
            name_style,
            secondary,
            secondary,
            palette,
            content_width,
        ));
        if let Some(suffix) = collapsed_suffix {
            spans.push(ratatui::text::Span::styled(
                suffix,
                Style::default().fg(palette.overlay0),
            ));
        }
        if let Some(suffix) = blocked_suffix {
            spans.push(ratatui::text::Span::styled(
                suffix,
                Style::default().fg(status_color(
                    crate::api::schema::AgentStatus::Blocked,
                    palette,
                )),
            ));
        }
        Paragraph::new(Line::from(spans)).style(row_style).render(
            Rect::new(rect.x, rect.y + index as u16, rect.width, 1),
            buffer,
        );
    }
    if let Some(toggle) = toggle {
        put_text(
            buffer,
            toggle.x,
            toggle.y,
            toggle.width,
            if row.collapsed {
                "\u{25b8}"
            } else {
                "\u{25be}"
            },
            Style::default().fg(palette.accent),
        );
    }
    toggle
}

fn agent_tree_prefix(row: &AgentRow) -> (String, String) {
    if row.depth == 0 {
        return (" ".to_string(), "   ".to_string());
    }
    let base = "   ".repeat(row.depth);
    let first = format!(
        "{base}{}",
        if row.last_child {
            "\u{2514}\u{2500} "
        } else {
            "\u{251c}\u{2500} "
        }
    );
    let continuation = format!(
        "{base}{}",
        if row.last_child { "   " } else { "\u{2502}  " }
    );
    (first, continuation)
}

fn put_text(buffer: &mut Buffer, x: u16, y: u16, width: u16, text: &str, style: Style) {
    for (offset, character) in text.chars().take(width as usize).enumerate() {
        if let Some(cell) = buffer.cell_mut((x + offset as u16, y)) {
            cell.set_char(character).set_style(style);
        }
    }
}

fn display_width(text: &str) -> usize {
    unicode_width::UnicodeWidthStr::width(text)
}

fn sidebar_status_text(status: crate::api::schema::AgentStatus) -> &'static str {
    use crate::api::schema::AgentStatus;
    match status {
        AgentStatus::Blocked => "blocked",
        AgentStatus::Done => "done",
        AgentStatus::Working => "working",
        AgentStatus::Idle | AgentStatus::Unknown => "idle",
    }
}
