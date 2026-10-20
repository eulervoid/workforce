//! The frame around the dashboard: the header, the footer with its key hints, the breadcrumb and the latest event.

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use wf_core::id::HashId;

use super::lists::{age, cut_spans};
use super::{KeysAt, Themed, accent, ansi_text, highlight_ends, themed};
use crate::dashboard::app::{App, Scroll, Spot, Status, Tab, Target};
use crate::dashboard::hint::{
    ALLOW, ANSWER, ANSWER_IN_EDITOR, BACK, CHANGE, CHANGE_FILTER, CLEAR_FILTER, COMMENT, DENY,
    DIFF, DROP, EDIT, EXTEND, FILE, FILTER, FOLD, FOLLOW, FULL, GROUP, Hint, ITEM, JUMP_BACK, KEYS,
    MOVE, NEXT_SESSION, OPEN, OPEN_DIFF, OPEN_SESSION, PAGER, READINESS, REVIEW, REVIEW_IN_DIFF,
    SCROLL, SELECT, UNSELECT,
};
use crate::dashboard::snapshot::Detail;
use crate::dashboard::snapshot::{InView, InboxEntry, MovedBack, SessionInfo};
use crate::theme;
use crate::view::SessionStatus;

/// Draws `tip` at the right of the latest-event line, `taken` columns of which the latest event
/// fills. A line too narrow for both leaves the tip out: it never covers the event, nor is cut.
pub(super) fn draw_tip(frame: &mut Frame, tip: &str, taken: usize, latest: Rect) {
    // A column off the right edge, as the event keeps one off the left.
    let tip = Line::from(format!("{tip} "));
    // Two columns apart, so the tip does not read as part of the event.
    if taken + 2 + tip.width() > usize::from(latest.width) {
        return;
    }
    frame.render_widget(Paragraph::new(tip).right_aligned(), latest);
}

/// Draws the warning of `moved` at the right of the latest-event line, with how many `others` it
/// stands for, its reason cut to fit; returns it as a button that opens the item.
pub(super) fn draw_warning(
    frame: &mut Frame,
    moved: &MovedBack,
    others: usize,
    latest: Rect,
) -> Spot {
    let name = format!("#{}", moved.item.short());
    let opening = format!("⚠ {name} back to open: ");
    let count = if others > 0 {
        format!(" · {others} more")
    } else {
        String::new()
    };
    // A column off either edge.
    let room = usize::from(latest.width).saturating_sub(2);
    let width = textwrap::core::display_width;
    let reason_room = room.saturating_sub(width(&opening) + width(&count));
    let mut spans = vec![Span::raw(opening)];
    spans.extend(cut_spans(
        vec![Span::raw(moved.reason.clone())],
        reason_room,
    ));
    spans.push(Span::raw(count));
    let line = Line::from(spans).style(theme::attention());
    let line_width = line.width().min(room) as u16;
    let area = Rect {
        x: latest.right().saturating_sub(line_width + 1),
        width: line_width,
        ..latest
    };
    frame.render_widget(Paragraph::new(line), area);
    Spot {
        target: Target::Open {
            detail: Detail::Item(moved.item),
            name,
        },
        area,
    }
}

/// The label of the tab on screen, between ends in the colour behind it, as a highlight has; on a
/// dimmer background while the terminal pane is not focused. Without a colour behind it, the ends
/// are spaces.
pub(super) fn active_tab(label: String, app: &App) -> Vec<Span<'static>> {
    let style = Style::from(if app.terminal_focused {
        theme::tab_active()
    } else {
        theme::tab_active_dimmed()
    });
    let label = Span::styled(label, style);
    match style.bg {
        Some(behind) => {
            let (start, end) = highlight_ends(behind);
            vec![start, label, end]
        }
        None => vec![Span::raw(" "), label, Span::raw(" ")],
    }
}

/// Draws the name of the project and the tabs; returns where the label of each tab went.
pub(super) fn draw_header(frame: &mut Frame, app: &App, area: Rect) -> Vec<(Tab, Rect)> {
    let mut tabs = Vec::new();
    // Each tab with the column its label starts on and its width, from the left of the tabs.
    let mut labels = Vec::new();
    for tab in Tab::ALL {
        if !tabs.is_empty() {
            tabs.push(Span::raw(" "));
        }
        let label = format!("{} {}", tab.key(), tab.name());
        let label = if tab == app.tab {
            active_tab(label, app)
        } else {
            // A space where the active tab has its ends, so the tabs keep their places.
            vec![Span::raw(format!(" {label} ")).themed(theme::text_dimmed())]
        };
        let start: usize = tabs.iter().map(Span::width).sum();
        let width: usize = label.iter().map(Span::width).sum();
        labels.push((tab, start, width));
        tabs.extend(label);
    }
    let tabs = Line::from(tabs);
    let [name, tabs_area] = Layout::horizontal([
        Constraint::Fill(1),
        Constraint::Length(tabs.width() as u16 + 1),
    ])
    .areas(area);
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::raw(" "),
            Span::raw(app.snapshot.name.clone()).themed(theme::text_bold()),
        ])),
        name,
    );
    frame.render_widget(Paragraph::new(tabs), tabs_area);
    labels
        .into_iter()
        .map(|(tab, start, width)| {
            let label = Rect {
                x: tabs_area.x.saturating_add(start as u16),
                width: width as u16,
                ..tabs_area
            };
            // What a narrow window cuts off cannot be clicked.
            (tab, label.intersection(tabs_area))
        })
        .collect()
}

/// `3 live · 14 open · 2 waiting · 1 blocked · 2 devices`, leaving out what is none; `waiting`
/// counts every entry that waits on the principal.
pub(super) fn counts_line(app: &App) -> Line<'static> {
    let counts = app.snapshot.counts;
    let mut parts = vec![
        format!("{} live", counts.live_sessions),
        format!("{} open", counts.open_items),
    ];
    if !app.snapshot.waiting.is_empty() {
        parts.push(format!("{} waiting", app.snapshot.waiting.len()));
    }
    if counts.blocked_items > 0 {
        parts.push(format!("{} blocked", counts.blocked_items));
    }
    if counts.devices > 1 {
        parts.push(format!("{} devices", counts.devices));
    }
    Line::from(format!(" {}", parts.join(SEPARATOR))).themed(theme::text_dimmed())
}

/// A button `width` columns wide, `start` columns into the line `area`; what a narrow window cuts
/// off cannot be clicked.
pub(super) fn button(area: Rect, start: usize, width: usize, target: Target) -> Spot {
    let button = Rect {
        x: area.x.saturating_add(start as u16),
        width: width as u16,
        ..area
    };
    Spot {
        target,
        area: button.intersection(area),
    }
}

pub(super) fn draw_rule(frame: &mut Frame, area: Rect) {
    let rule = "─".repeat(usize::from(area.width));
    frame.render_widget(Paragraph::new(rule).themed(theme::divider()), area);
}

/// Draws where the detail on screen is: `Now › #k3j1 › coder · work · 12m ◆`. Its last part
/// has the accent, full while the detail has the keys. Returns the parts that go back to a detail opened before, or to the list.
pub(super) fn draw_breadcrumb(frame: &mut Frame, app: &App, area: Rect) -> Vec<Spot> {
    // Each part, with how many of the details opened it goes back to; `None` for an item named
    // only for its session, which was not opened.
    let mut crumbs: Vec<(Vec<Span<'static>>, Option<usize>)> =
        vec![(vec![Span::raw(app.tab.name())], Some(0))];
    let path = app.path();
    for (index, detail) in path.iter().enumerate() {
        let depth = Some(index + 1);
        match detail {
            Detail::Item(item) => {
                crumbs.push((vec![Span::raw(format!("#{}", item.short()))], depth));
            }
            Detail::Agent(agent) => crumbs.push((vec![Span::raw(agent.name.to_string())], depth)),
            Detail::Flag(flag) => crumbs.push((vec![Span::raw(flag.heading())], depth)),
            Detail::Diff(item) => {
                // A diff is of an item: named, unless the way here went through it.
                let name = format!("#{}", item.short());
                let through_item = path[..index].iter().any(|before| match before {
                    Detail::Item(before) | Detail::Diff(before) => before == item,
                    Detail::Session(session) => app
                        .session_info(*session)
                        .is_some_and(|session| session.item == *item),
                    Detail::Agent(_) | Detail::Flag(_) => false,
                });
                if !through_item {
                    crumbs.push((vec![Span::raw(name)], None));
                }
                crumbs.push((vec![Span::raw("diff")], depth));
            }
            Detail::Session(id) => {
                let Some(session) = app.session_info(*id) else {
                    crumbs.push((vec![Span::raw(id.short())], depth));
                    continue;
                };
                // A session is about an item: named, unless the way here went through it.
                let item = session.item;
                if crumbs
                    .last()
                    .and_then(|(crumb, _)| crumb.first())
                    .map(|span| span.content.as_ref())
                    != Some(format!("#{}", item.short()).as_str())
                {
                    let mut crumb = vec![Span::raw(format!("#{}", item.short()))];
                    // With the detail in its place, the list with its title is not on screen.
                    if !app.shows_both()
                        && let Some(title) = app.item_title(item)
                    {
                        crumb.push(Span::raw(format!(" {title}")));
                    }
                    crumbs.push((crumb, None));
                }
                crumbs.push((session_crumb(app, session), depth));
            }
        }
    }
    let accent = accent(app, KeysAt::Detail);
    let (start_of_last, end_of_last) = highlight_ends(accent);
    let last = crumbs.len() - 1;
    // Only what was opened, below the detail on top, is there to go back to; nothing while the
    // list has the keys, even with a detail kept beside it.
    let opened = if app.list_has_keys() {
        0
    } else {
        app.stack().len()
    };
    let mut spots = Vec::new();
    let mut spans = Vec::new();
    for (index, (crumb, depth)) in crumbs.into_iter().enumerate() {
        if index > 0 {
            spans.push(Span::raw(" ›").themed(theme::text_dimmed()));
        }
        // The last part has its start of the highlight where the others have a space.
        spans.push(if index == last {
            start_of_last.clone()
        } else {
            Span::raw(" ")
        });
        let start: usize = spans.iter().map(Span::width).sum();
        let width: usize = crumb.iter().map(Span::width).sum();
        if let Some(depth) = depth.filter(|depth| *depth < opened)
            && let Some(name) = crumb.first()
        {
            let button = Rect {
                x: area.x.saturating_add(start as u16),
                width: width as u16,
                ..area
            };
            spots.push(Spot {
                target: Target::Back {
                    depth,
                    name: name.content.trim().to_owned(),
                },
                // What a narrow window cuts off cannot be clicked.
                area: button.intersection(area),
            });
        }
        for span in crumb {
            spans.push(if index == last {
                span.patch_style(Style::from(theme::text_bold()).bg(accent))
            } else {
                span.themed(theme::text_dimmed())
            });
        }
    }
    // A breadcrumb cut short leaves out the end of the highlight, as it does the rest.
    let room = usize::from(area.width);
    let width: usize = spans.iter().map(Span::width).sum();
    let mut spans = cut_spans(spans, room);
    if width < room {
        spans.push(end_of_last);
    }
    let line = Line::from(spans);
    frame.render_widget(Paragraph::new(line), area);
    spots.retain(|spot| !spot.area.is_empty());
    spots
}

/// `coder · work · 12m ◆`
pub(super) fn session_crumb(app: &App, session: &SessionInfo) -> Vec<Span<'static>> {
    let status = match session.status {
        SessionStatus::Live => themed(" ◆", theme::live()),
        SessionStatus::Parked => themed(" ◇ parked", theme::live()),
        SessionStatus::Ended { reason, .. } => Span::raw(format!(" · {}", reason.name())),
    };
    vec![
        Span::raw(format!(
            "{} · {} · {}",
            session.agent_name,
            session.role.name(),
            age(session.started, app.snapshot.now_ms)
        )),
        status,
    ]
}

/// The latest event, or why the last refresh failed.
pub(super) fn latest_line(app: &App) -> Line<'static> {
    match &app.status {
        Some(Status::Done { what, .. }) => {
            return Line::from(format!(" ✓ {what}")).style(theme::success());
        }
        Some(
            Status::ActionFailed { error, .. }
            | Status::RefreshFailed(error)
            | Status::TranscriptFailed { error, .. },
        ) => {
            let first = error.lines().next().unwrap_or_default();
            return Line::from(format!(" error: {first}")).style(theme::error());
        }
        None => {}
    }
    match app.log.lines.last() {
        Some(line) => {
            let first = line.lines().next().unwrap_or_default();
            let text = ansi_text(first);
            let mut spans = vec![Span::raw(" ")];
            spans.extend(
                text.lines
                    .into_iter()
                    .next()
                    .into_iter()
                    .flat_map(|line| line.spans),
            );
            Line::from(spans)
        }
        None => Line::from(" no events yet").themed(theme::text_dimmed()),
    }
}

/// Draws the hints of the keys that work on what is on screen, which a click presses.
pub(super) fn draw_hints(frame: &mut Frame, app: &App, area: Rect) -> Vec<Spot> {
    let hints = hints(app);
    let request = app.subjects().into_iter().find_map(InboxEntry::request);
    let mut spots = Vec::new();
    let mut start = 1;
    for hint in &hints {
        let text = hint.text();
        let width = textwrap::core::display_width(&text);
        if let Some(target) = hint.target(request) {
            spots.push(button(area, start, width, target));
        }
        start += width + textwrap::core::display_width(SEPARATOR);
    }
    let texts: Vec<String> = hints.iter().map(Hint::text).collect();
    let line = Line::from(format!(" {}", texts.join(SEPARATOR))).themed(theme::text_dimmed());
    frame.render_widget(Paragraph::new(line), area);
    spots.retain(|spot| !spot.area.is_empty());
    spots
}

/// Between two hints.
pub(super) const SEPARATOR: &str = " · ";

/// The keys that work on what is on screen: `esc back`, `v review`.
pub(super) fn hints(app: &App) -> Vec<Hint> {
    let mut acts: Vec<Hint> = Vec::new();
    for entry in app.subjects() {
        let keys: &[Hint] = match entry {
            InboxEntry::Permission { .. } => &[ALLOW, DENY],
            InboxEntry::Blocker { .. } => &[ANSWER, ANSWER_IN_EDITOR],
            // In the diff, `v` selects lines.
            InboxEntry::Review { .. } if app.diff_has_keys() => &[REVIEW_IN_DIFF],
            InboxEntry::Review { .. } => &[REVIEW, DIFF],
            InboxEntry::Notice { .. } | InboxEntry::MachineFault { .. } => &[],
        };
        for key in keys {
            if !acts.contains(key) {
                acts.push(*key);
            }
        }
    }
    // A detail kept while the list has the keys takes none.
    let opened = app.stack().last().filter(|_| !app.list_has_keys());
    let item_session = matches!(
        opened.and_then(|opened| app.in_view_of(&opened.detail)),
        Some(InView::Item {
            session: Some(_),
            ..
        })
    );
    let successor = matches!(
        opened.and_then(|opened| app.in_view_of(&opened.detail)),
        Some(InView::Session {
            successor: Some(_),
            ..
        })
    );
    let navigation: &[Hint] = match (app.tab, opened.map(|opened| &opened.detail)) {
        (_, Some(Detail::Session(_))) if successor => {
            &[BACK, SCROLL, NEXT_SESSION, ITEM, FULL, OPEN_DIFF]
        }
        (_, Some(Detail::Session(_))) => &[BACK, SCROLL, ITEM, FULL, OPEN_DIFF],
        (_, Some(Detail::Diff(_))) => match app.diff_on_top() {
            Some(pane) if pane.selection.is_some() => &[UNSELECT, EXTEND, COMMENT, CHANGE],
            Some(pane) if pane.draft_at_cursor().is_some() => &[BACK, MOVE, EDIT, DROP],
            Some(_) | None => &[BACK, MOVE, SELECT, COMMENT, CHANGE, FILE, FOLD, ITEM, PAGER],
        },
        (_, Some(Detail::Item(_))) if item_session => &[BACK, SCROLL, OPEN_SESSION],
        (_, Some(Detail::Item(_) | Detail::Agent(_) | Detail::Flag(_))) => &[BACK, SCROLL],
        (Tab::Now, None) => &[OPEN, MOVE],
        (Tab::Items, None) if !app.items.filter.is_empty() => {
            &[OPEN, GROUP, CHANGE_FILTER, CLEAR_FILTER, READINESS]
        }
        (Tab::Items, None) => &[OPEN, GROUP, FILTER, READINESS],
        (Tab::Log, None) if !app.log.filter.is_empty() => &[CHANGE_FILTER, CLEAR_FILTER],
        (Tab::Log, None) => &[FILTER, SCROLL],
        (Tab::Agents, None) => &[OPEN, MOVE],
    };
    // What follows what is added, once scrolled away from its end.
    let followed = match (app.tab, opened) {
        (_, Some(opened)) => match opened.detail {
            Detail::Session(_) => Some(opened.scroll),
            Detail::Item(_) | Detail::Agent(_) | Detail::Flag(_) | Detail::Diff(_) => None,
        },
        (Tab::Log, None) => Some(app.log.scroll),
        (Tab::Now | Tab::Items | Tab::Agents, None) => None,
    };
    let mut hints = navigation.to_vec();
    if let Some(Scroll::At(_)) = followed {
        hints.push(FOLLOW);
    }
    if !app.jumped_from().is_empty() {
        hints.push(JUMP_BACK);
    }
    hints.extend(acts);
    hints.push(KEYS);
    hints
}
