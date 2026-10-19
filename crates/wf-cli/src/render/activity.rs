use std::fmt::Write;

use super::text::*;
use crate::theme;
use crate::transcript::Format;
use crate::view::*;

/// The activity of an item: one line per thing that happened, and the text that goes with it
/// below. The date heads the section and is repeated when the day changes.
pub(super) fn activity(
    entries: &[crate::view::ActivityView],
    width: Option<usize>,
    format: Format,
) -> String {
    let mut out = String::new();
    let mut day = None;
    for entry in entries {
        let (date, time) = local_date_and_time(entry.at);
        if day.as_ref() != Some(&date) {
            match day {
                None => {
                    let _ = writeln!(
                        out,
                        "\n{} {}",
                        paint(theme::text_bold(), "activity"),
                        paint(theme::text_dimmed(), format!("· {date}"))
                    );
                }
                Some(_) => {
                    let _ = writeln!(
                        out,
                        "  {}",
                        paint(theme::text_dimmed(), format!("· {date}"))
                    );
                }
            }
            day = Some(date);
        }
        let (line, text) = activity_entry(
            entry,
            &format!("  {}  ", paint(theme::text_dimmed(), time)),
            width,
            format,
        );
        let _ = writeln!(out, "{line}");
        if let Some(text) = text {
            let _ = writeln!(out, "{text}");
        }
    }
    out
}

/// Where the text of an activity entry starts, and where a long line of who did what continues:
/// two columns further in than what happened, to set it apart.
pub(super) const ACTIVITY_TEXT_INDENT: &str = "           ";

/// One entry of an item's activity: `lead` (its time), who did what, and the text that goes with
/// it, if it has any, its markdown in `format`. Both are wrapped to `width` under
/// [`ACTIVITY_TEXT_INDENT`].
pub(super) fn activity_entry(
    entry: &crate::view::ActivityView,
    lead: &str,
    width: Option<usize>,
    format: Format,
) -> (String, Option<String>) {
    let mut parts = vec![paint_actor(&entry.by)];
    match &entry.comment {
        Some(comment) if entry.kind == ActivityKind::Comment => {
            match comment.kind {
                CommentKindView::Blocker | CommentKindView::ChangeRequest => parts.push(paint(
                    theme::warning(),
                    format!(
                        "{} {}",
                        comment_marker(comment.kind),
                        comment_kind_label(comment.kind)
                    ),
                )),
                CommentKindView::Comment if comment.resolves.is_empty() => {
                    parts.push(paint(theme::text_bold(), "commented"))
                }
                CommentKindView::Comment => {}
            }
            if !comment.resolves.is_empty() {
                parts.push(paint(
                    theme::text_bold(),
                    format!("resolved {}", comment.resolves.join(", ")),
                ));
            }
            if let Some(by) = &comment.resolved_by {
                parts.push(paint(theme::text_dimmed(), format!("resolved by {by}")));
            }
            match &comment.step {
                Some(StepRefView {
                    number: Some(number),
                    ..
                }) => parts.push(paint(theme::text_dimmed(), format!("on step {number}"))),
                Some(StepRefView { .. }) => {
                    parts.push(paint(theme::text_dimmed(), "on a step since edited"))
                }
                None => {}
            }
            parts.push(paint(theme::text_dimmed(), &comment.short));
        }
        Some(_) | None => parts.push(paint(theme::text_bold(), &entry.summary)),
    }
    let line = format!(
        "{lead}{}",
        parts.join(&format!(" {} ", paint(theme::text_dimmed(), "·")))
    );
    let line = fill(&line, width, ACTIVITY_TEXT_INDENT);
    let text = entry
        .text
        .as_deref()
        .filter(|text| !text.trim().is_empty())
        .map(|text| markdown_text(text, width, ACTIVITY_TEXT_INDENT, format));
    (line, text)
}

/// One line of `wf log --follow`: `14:02  #k3f7ab  @ada/coder · moved to done`, and the text that goes
/// with it below.
pub fn event_line(line: &crate::view::EventLineView) -> String {
    event_line_for(line, text_width(), text_format())
}

/// [`event_line`] wrapped to `width`, or not at all, its markdown in `format`.
pub fn event_line_for(
    line: &crate::view::EventLineView,
    width: Option<usize>,
    format: Format,
) -> String {
    let (_, time) = local_date_and_time(line.activity.at);
    let item = match &line.item {
        Some(item) => format!("{}  ", id_label(*item)),
        None => String::new(),
    };
    let lead = format!("{}  {item}", paint(theme::text_dimmed(), time));
    let (entry, text) = activity_entry(&line.activity, &lead, width, format);
    let mut out = format!("{entry}\n");
    if let Some(text) = text {
        out.push_str(&text);
        out.push('\n');
    }
    out
}

/// Marks a blocker or change request: `⧗` waits on someone else, `△` waits on the work.
pub(super) fn comment_marker(kind: CommentKindView) -> &'static str {
    match kind {
        CommentKindView::Blocker => "⧗",
        CommentKindView::ChangeRequest => "△",
        CommentKindView::Comment => "",
    }
}

pub(super) fn comment_kind_label(kind: CommentKindView) -> &'static str {
    match kind {
        CommentKindView::Comment => "comment",
        CommentKindView::Blocker => "blocker",
        CommentKindView::ChangeRequest => "change request",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn checked_step(text: &str) -> crate::view::ActivityView {
        crate::view::ActivityView {
            at: jiff::Timestamp::UNIX_EPOCH,
            by: crate::view::ActorView {
                kind: crate::view::ActorKind::Agent,
                label: "@ada/coder/bh2m".to_string(),
            },
            kind: ActivityKind::Edited,
            summary: "checked step 1".to_string(),
            comment: None,
            text: Some(text.to_string()),
        }
    }

    #[test]
    fn the_text_of_an_activity_entry_is_wrapped_below_its_line() {
        let entry = checked_step("wrap long activity lines under their indent");
        let (line, text) = activity_entry(&entry, "  00:27  ", Some(44), Format::Styled);
        assert_eq!(
            anstream::adapter::strip_str(&line).to_string(),
            "  00:27  @ada/coder/bh2m · checked step 1"
        );
        assert_eq!(
            anstream::adapter::strip_str(&text.unwrap()).to_string(),
            "           wrap long activity lines under\n           their indent"
        );
        let (line, text) = activity_entry(&entry, "  00:27  ", None, Format::Raw);
        assert_eq!(
            anstream::adapter::strip_str(&line).to_string(),
            "  00:27  @ada/coder/bh2m · checked step 1"
        );
        assert_eq!(
            anstream::adapter::strip_str(&text.unwrap()).to_string(),
            "           wrap long activity lines under their indent"
        );
    }

    #[test]
    fn a_long_activity_line_continues_under_the_text() {
        let mut entry = checked_step("");
        entry.summary = "renamed it to \"a title too long for one line\"".to_string();
        let (line, _) = activity_entry(&entry, "  00:27  ", Some(40), Format::Styled);
        assert_eq!(
            anstream::adapter::strip_str(&line).to_string(),
            "  00:27  @ada/coder/bh2m · renamed it\n           to \"a title too long for\n           one line\""
        );
    }
}
