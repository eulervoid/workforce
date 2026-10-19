//! How the dashboard looks: a function of its [`App`].

mod chrome;
mod detail;
mod lists;
mod popups;

use crate::theme::Background;
use ansi_to_tui::IntoText;
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Style, Styled};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Clear, Paragraph};

use self::chrome::{
    counts_line, draw_header, draw_hints, draw_rule, draw_tip, draw_warning, latest_line,
};
use self::detail::{draw_detail, draw_log};
use self::lists::{
    ListLines, agents_lines, draw_indicator, draw_list, items_lines, now_lines, row,
};
use self::popups::{draw_choice, draw_keys, draw_prompt};
use crate::dashboard::app::{App, Clickable, Drawn, DrawnDetail, Hovered, Prompt, STRIP, Tab};
use crate::dashboard::links;
use crate::theme;

/// `text` in a style of the theme.
pub(super) fn themed<'a>(
    text: impl Into<std::borrow::Cow<'a, str>>,
    style: anstyle::Style,
) -> Span<'a> {
    Span::styled(text, Style::from(style))
}

/// A style of the theme on top of what is styled already, as `Stylize` puts its own.
pub(super) trait Themed: Styled<Item = Self> + Sized {
    fn themed(self, style: anstyle::Style) -> Self {
        let style = self.style().patch(Style::from(style));
        self.set_style(style)
    }
}

impl<T: Styled<Item = T>> Themed for T {}

/// What every cell has under what is drawn on it: `text`, on `background`. Without either,
/// nothing: the terminal's own colours show.
pub(super) fn base(text: anstyle::Style, background: Background) -> Style {
    let text = Style::from(text);
    match background {
        Background::Terminal => text,
        Background::Color(color) => text.bg(color.into()),
    }
}

/// Clears `area` for a popup, down to `base`.
pub(super) fn clear(frame: &mut Frame, area: Rect, base: Style) {
    frame.render_widget(Clear, area);
    frame.buffer_mut().set_style(area, base);
}

/// Text with escape codes, such as a rendered transcript, as ratatui text. A reset in it goes
/// back to the base rather than to the terminal's colours.
pub(super) fn ansi_text(text: &str) -> Text<'static> {
    let mut text = text.into_text().unwrap_or_default();
    for span in text.lines.iter_mut().flat_map(|line| line.spans.iter_mut()) {
        if span.style.fg == Some(Color::Reset) {
            span.style.fg = None;
        }
        if span.style.bg == Some(Color::Reset) {
            span.style.bg = None;
        }
    }
    text
}

/// The place on screen the keys go to, which the accent marks.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum KeysAt {
    /// The list of the tab: its selection. The log has none, so nothing marks it.
    List,
    /// A detail: the last part of its breadcrumb.
    Detail,
    /// What the footer asks for: its label.
    Footer,
    /// A list to pick from: the option picked.
    Choice,
    /// The keys: their title.
    Keys,
}

impl KeysAt {
    fn of(app: &App) -> Self {
        match &app.prompt {
            Some(
                Prompt::Filter { .. }
                | Prompt::Answer { .. }
                | Prompt::Verdict { .. }
                | Prompt::Comment { .. }
                | Prompt::LineComment { .. }
                | Prompt::Stop { .. },
            ) => KeysAt::Footer,
            Some(Prompt::Choose(_)) => KeysAt::Choice,
            Some(Prompt::Keys) => KeysAt::Keys,
            None => match (app.tab, app.list_has_keys()) {
                (Tab::Now | Tab::Items | Tab::Agents | Tab::Log, true) => KeysAt::List,
                (Tab::Now | Tab::Items | Tab::Agents | Tab::Log, false) => KeysAt::Detail,
            },
        }
    }
}

/// The accent of `place`: full while it has the keys and the terminal pane is focused, dimmed
/// otherwise.
pub(super) fn accent(app: &App, place: KeysAt) -> Color {
    if app.terminal_focused && KeysAt::of(app) == place {
        theme::accent().into()
    } else {
        theme::accent_dimmed().into()
    }
}

/// The bar left of a selection in `accent`, on the background of the selection: white while the
/// accent is full, in the dimmed accent itself otherwise, so only the dimmed background shows.
pub(super) fn selection_bar(accent: Color) -> Span<'static> {
    let style = if accent == Color::from(theme::accent()) {
        Style::from(theme::on_accent())
    } else {
        Style::new().fg(accent)
    };
    Span::styled("▎", style.bg(accent))
}

/// The ends of a highlight inside a line, on `background`: a right half block before it and a left
/// half block after it, in `background` on the background of the line, so the colour reaches half
/// a cell past the text on either side.
pub(super) fn highlight_ends(background: Color) -> (Span<'static>, Span<'static>) {
    let style = Style::new().fg(background);
    (Span::styled("▐", style), Span::styled("▌", style))
}

/// `content` highlighted inside a line: in `style` on `background`, between its ends. The ends
/// take a cell each, where the line would otherwise have a space around the text.
pub(super) fn highlight(
    content: Vec<Span<'static>>,
    style: Style,
    background: Color,
) -> Vec<Span<'static>> {
    let (start, end) = highlight_ends(background);
    let style = style.bg(background);
    let mut spans = vec![start];
    spans.extend(content.into_iter().map(|span| span.patch_style(style)));
    spans.push(end);
    spans
}

pub(super) const SPINNER: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

/// Draws the dashboard, and keeps the scroll of the text and of the list to what is shown of them.
pub fn draw(frame: &mut Frame, app: &mut App) {
    let area = frame.area();
    app.resize(area.width, area.height);
    app.lay_out_transcript();
    app.lay_out_log();
    let drawn = draw_all(frame, app);
    app.shown(drawn);
}

/// Draws the dashboard; where the text and the list on screen ended up.
pub(super) fn draw_all(frame: &mut Frame, app: &App) -> Drawn {
    let area = frame.area();
    frame.buffer_mut().set_style(area, app.base);
    let [header, counts, rule, body, rule_below, latest, keys] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Fill(1),
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Length(1),
    ])
    .areas(frame.area());
    let tabs = draw_header(frame, app, header);
    frame.render_widget(Paragraph::new(counts_line(app)), counts);
    draw_rule(frame, rule);
    let mut shown = draw_body(frame, app, body);
    shown.clickable.tabs = tabs;
    draw_rule(frame, rule_below);
    // A warning takes the right of the line, and the latest event what is left.
    let warning = app
        .moved_back
        .last()
        .map(|moved| draw_warning(frame, moved, app.moved_back.len() - 1, latest));
    let event_area = match &warning {
        // Two columns apart, as a tip keeps from the event.
        Some(warning) => Rect {
            width: warning.area.x.saturating_sub(latest.x + 2),
            ..latest
        },
        None => latest,
    };
    let latest_line = latest_line(app);
    let latest_width = latest_line.width().min(usize::from(event_area.width));
    frame.render_widget(Paragraph::new(latest_line), event_area);
    if app.status.is_none() {
        shown.clickable.spots.extend(links::find(
            frame.buffer_mut(),
            event_area,
            None,
            None,
            &app.snapshot.links,
        ));
    }
    shown.clickable.spots.extend(warning.clone());
    match &app.prompt {
        Some(Prompt::Choose(choice)) => {
            draw_choice(frame, choice, accent(app, KeysAt::Choice), body, app.base);
            frame.render_widget(
                Paragraph::new(
                    Line::from(" j/k move · enter pick · esc cancel").themed(theme::text_dimmed()),
                ),
                keys,
            );
        }
        Some(Prompt::Keys) => {
            draw_keys(frame, accent(app, KeysAt::Keys), body, app.base);
            frame.render_widget(
                Paragraph::new(Line::from(" any key closes").themed(theme::text_dimmed())),
                keys,
            );
        }
        Some(prompt) => draw_prompt(frame, app, prompt, keys),
        None => shown.clickable.spots.extend(draw_hints(frame, app, keys)),
    }
    shown.hovered = app
        .pointer
        .filter(|_| app.mouse_passes())
        .and_then(|pointer| shown.clickable.hovered(pointer));
    match &shown.hovered {
        // The warning under the pointer stays: it says more than its tip would.
        Some(Hovered::Spot(spot)) if Some(spot) == warning.as_ref() => {
            frame
                .buffer_mut()
                .set_style(spot.area, Style::from(theme::link()));
        }
        Some(Hovered::Spot(spot)) => {
            frame
                .buffer_mut()
                .set_style(spot.area, Style::from(theme::link()));
            // A tip takes the place of the warning while it shows.
            if warning.is_some() {
                let cleared = Rect {
                    x: event_area.right(),
                    width: latest.right() - event_area.right(),
                    ..latest
                };
                clear(frame, cleared, app.base);
            }
            draw_tip(frame, &app.tip(&spot.target), latest_width, latest);
        }
        // An inactive tab under the pointer is no longer dim: stronger than the other inactive
        // tabs, weaker than the active one.
        Some(Hovered::Tab(tab)) if *tab != app.tab => {
            let label = shown.clickable.tabs.iter().find(|(each, _)| each == tab);
            if let Some((_, label)) = label {
                let text = app.base.fg.unwrap_or(Color::Reset);
                frame
                    .buffer_mut()
                    .set_style(*label, Style::new().not_dim().fg(text));
            }
        }
        Some(Hovered::Tab(_)) | None => {}
    }
    shown
}

pub(super) fn draw_body(frame: &mut Frame, app: &App, area: Rect) -> Drawn {
    match app.tab {
        Tab::Now => draw_split(frame, app, area, now_lines),
        Tab::Items => draw_split(frame, app, area, items_lines),
        Tab::Log => {
            let mut spots = Vec::new();
            let shown = match app.on_screen() {
                Some(opened) => draw_detail(frame, app, opened, area, &mut spots),
                None => draw_log(frame, app, area, &mut spots).map(DrawnDetail::Text),
            };
            Drawn {
                clickable: Clickable {
                    spots,
                    ..Clickable::default()
                },
                detail: shown,
                ..Drawn::default()
            }
        }
        Tab::Agents => draw_split(frame, app, area, agents_lines),
    }
}

/// The list, and the detail on screen: right of it in a wide window, or in its place in a
/// narrow one or with the other pane folded away to a strip.
fn draw_split(frame: &mut Frame, app: &App, area: Rect, list_lines: ListLines) -> Drawn {
    let in_view = app.on_screen();
    if !app.shows_both() {
        let (area, strip) = if app.is_wide() {
            let (pane, strip) = draw_strip(frame, app, area);
            (pane, Some(strip))
        } else {
            (area, None)
        };
        let mut drawn = match in_view {
            Some(opened) => {
                let mut spots = Vec::new();
                let shown = draw_detail(frame, app, opened, area, &mut spots);
                Drawn {
                    clickable: Clickable {
                        spots,
                        ..Clickable::default()
                    },
                    detail: shown,
                    ..Drawn::default()
                }
            }
            None => {
                let mut spots = Vec::new();
                let (list_top, list) = draw_list(frame, app, list_lines, area, &mut spots);
                Drawn {
                    list_top: Some(list_top),
                    clickable: Clickable {
                        list: Some(list),
                        spots,
                        ..Clickable::default()
                    },
                    ..Drawn::default()
                }
            }
        };
        drawn.clickable.strip = strip;
        return drawn;
    }
    let [list, rule, detail] = Layout::horizontal([
        Constraint::Length(app.list_width()),
        Constraint::Length(1),
        Constraint::Fill(1),
    ])
    .areas(area);
    let mut spots = Vec::new();
    let (list_top, list) = draw_list(frame, app, list_lines, list, &mut spots);
    let rule_text: Vec<Line> = (0..rule.height).map(|_| Line::from("│")).collect();
    frame.render_widget(Paragraph::new(rule_text).themed(theme::divider()), rule);
    let shown = in_view.and_then(|opened| draw_detail(frame, app, opened, detail, &mut spots));
    Drawn {
        list_top: Some(list_top),
        clickable: Clickable {
            list: Some(list),
            detail: Some(detail),
            spots,
            ..Clickable::default()
        },
        detail: shown,
        ..Drawn::default()
    }
}

/// Draws the strip of the pane folded away at its edge, with a chevron halfway down: `‹` for the
/// list at the left, `›` for the detail at the right. Returns what is left for the pane on
/// screen, and the strip.
pub(super) fn draw_strip(frame: &mut Frame, app: &App, area: Rect) -> (Rect, Rect) {
    let (pane, strip, chevron) = if app.list_has_keys() {
        let [pane, strip] =
            Layout::horizontal([Constraint::Fill(1), Constraint::Length(STRIP)]).areas(area);
        (pane, strip, " ›")
    } else {
        // The detail keeps its own margin, which stands in for the strip's second column.
        let [strip, pane] =
            Layout::horizontal([Constraint::Length(STRIP - 1), Constraint::Fill(1)]).areas(area);
        (pane, strip, "‹")
    };
    let mut lines = vec![Line::default(); usize::from(strip.height / 2)];
    lines.push(Line::from(chevron));
    frame.render_widget(Paragraph::new(lines).themed(theme::text_dimmed()), strip);
    (pane, strip)
}

/// Draws where a scrolled text `area` high is, at the right end of its last row, as neovim's
/// ruler shows it: `Top`, `Bot`, or how far down as a percentage. Nothing when all of the text
/// is on screen, as for a list that fits.
pub(super) fn draw_position(
    frame: &mut Frame,
    area: Rect,
    top: usize,
    last_top: usize,
    base: Style,
) {
    if last_top == 0 || area.height == 0 {
        return;
    }
    let position = format!("{}%", (top * 100 / last_top).clamp(0, 100));
    draw_indicator(frame, row(area, area.height - 1), &position, base);
}

/// `area` without its last column, which keeps text off the right edge and out of the corner.
pub(super) fn padded(area: Rect) -> Rect {
    Rect {
        width: area.width.saturating_sub(1),
        ..area
    }
}

/// The first line of a list of `lines` drawn `height` high: `previous`, moved only as far as
/// keeps the selection on screen with the line before it (a heading, maybe) and the line after
/// it (the rest of its row, maybe).
pub fn list_top(
    previous: usize,
    selected_line: Option<usize>,
    lines: usize,
    height: usize,
) -> usize {
    let mut top = previous.min(lines.saturating_sub(height));
    if let Some(line) = selected_line {
        if line < top + 1 {
            top = line.saturating_sub(1);
        }
        if line + 2 > top + height {
            top = (line + 2).saturating_sub(height);
        }
    }
    top
}

/// `line` wrapped into rows of at most `width` columns, each character keeping its style.
pub(super) fn break_line(line: Line<'static>, width: usize) -> Vec<Line<'static>> {
    if line.width() <= width {
        return vec![line];
    }
    let styled: Vec<(char, Style)> = line
        .spans
        .iter()
        .flat_map(|span| {
            span.content
                .chars()
                .map(|character| (character, span.style))
        })
        .collect();
    let plain: String = styled.iter().map(|(character, _)| character).collect();
    let mut rest = styled.into_iter().peekable();
    textwrap::wrap(
        &plain,
        textwrap::Options::new(width)
            .wrap_algorithm(textwrap::WrapAlgorithm::FirstFit)
            .word_separator(textwrap::WordSeparator::AsciiSpace),
    )
    .iter()
    .map(|row| {
        let mut spans: Vec<Span<'static>> = Vec::new();
        for character in row.chars() {
            // The whitespace the wrap dropped between rows has no place in the next one.
            let (kept, style) = loop {
                match rest.next() {
                    Some((kept, style)) if kept == character => break (kept, style),
                    Some(_) => {}
                    None => break (character, Style::new()),
                }
            };
            match spans.last_mut() {
                Some(last) if last.style == style => last.content.to_mut().push(kept),
                _ => spans.push(Span::styled(kept.to_string(), style)),
            }
        }
        Line::from(spans).style(line.style)
    })
    .collect()
}

#[cfg(test)]
mod tests {
    use ratatui::style::{Color, Style};
    use ratatui::text::{Line, Span};
    use tui_input::Input;

    use super::break_line;

    use super::popups::visible_input;

    #[test]
    fn wide_characters_are_skipped_by_cells_and_the_cursor_follows() {
        // 13 cells; in 3 cells the scroll is 10 cells, which is 5 characters. Skipping 10
        // characters would drop everything.
        let input = Input::new("日本語日本語a".to_owned());
        assert_eq!(input.visual_scroll(3), 10);
        let (typed, cursor) = visible_input(&input, 3);
        assert_eq!(typed, "語a");
        assert_eq!(cursor, 3);
    }

    #[test]
    fn narrow_text_is_skipped_by_cells_as_characters() {
        let input = Input::new("abcdefgh".to_owned());
        let (typed, cursor) = visible_input(&input, 4);
        assert_eq!(typed, "efgh");
        assert_eq!(cursor, 4);
    }

    #[test]
    fn a_wide_styled_line_breaks_into_rows_that_fit_and_keep_their_styles() {
        let red = Style::new().fg(Color::Red);
        let blue = Style::new().fg(Color::Blue);
        let line = Line::from(vec![
            Span::styled("alpha beta ", red),
            Span::styled("gamma delta", blue),
        ]);
        let rows = break_line(line, 11);
        assert!(rows.len() > 1);
        assert!(rows.iter().all(|row| row.width() <= 11));
        let cells: Vec<(String, Style)> = rows
            .iter()
            .flat_map(|row| {
                row.spans
                    .iter()
                    .map(|span| (span.content.to_string(), span.style))
            })
            .collect();
        assert_eq!(
            cells,
            vec![
                ("alpha beta".to_owned(), red),
                ("gamma delta".to_owned(), blue),
            ]
        );
    }
}
