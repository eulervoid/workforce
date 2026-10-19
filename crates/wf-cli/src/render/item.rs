use std::fmt::Write;

use super::activity::*;
use super::table::table;
use super::text::*;
use crate::cli::ItemGrouping;
use crate::markdown;
use crate::theme;
use crate::transcript::Format;
use crate::view::*;
use wf_core::graph::Readiness;
use wf_core::id::HashId;
use wf_core::id::ItemId;
use wf_core::inbox::InboxKind;
use wf_core::item::ItemState;
use wf_core::stats::ItemStats;

pub(crate) fn state_icon(state: ItemState) -> &'static str {
    match state {
        ItemState::Draft => "◌",
        ItemState::Open => "○",
        ItemState::Active => "◐",
        ItemState::Review => "◎",
        ItemState::Done => "✓",
        ItemState::Cancelled => "✗",
    }
}

fn readiness_icon(readiness: Readiness) -> &'static str {
    match readiness {
        Readiness::Ready => "○",
        Readiness::Claimed => "◐",
        Readiness::Blocked => "⊘",
        Readiness::Stranded => "⚠",
    }
}

/// The icon of an item: its state's icon, or a warning icon when the item is stranded.
pub fn item_icon(state: ItemState, readiness: Option<Readiness>) -> &'static str {
    match readiness {
        Some(Readiness::Stranded) => "⚠",
        Some(Readiness::Ready | Readiness::Claimed | Readiness::Blocked) | None => {
            state_icon(state)
        }
    }
}

/// `○ #7ph6ds (task:open)  Title`. Pickers take plain text, so nothing is colored.
pub fn item_choice(summary: &ItemSummary) -> String {
    let state = if summary.archived {
        "archived"
    } else {
        summary.state.name()
    };
    format!(
        "{} #{} ({}:{state})  {}",
        item_icon(summary.state, summary.readiness),
        summary.id.short(),
        summary.item_type,
        summary.title,
    )
}

/// `○ #q7x2 (task:open) `, the icon and state colored by the state. A stranded item gets a warning
/// icon instead. `suffix` follows the state.
pub(super) fn item_label(
    id: ItemId,
    item_type: &str,
    state: ItemState,
    readiness: Option<Readiness>,
    suffix: &str,
) -> String {
    let state_style = theme::state(state);
    let icon_style = match readiness {
        Some(Readiness::Stranded) => theme::warning(),
        Some(Readiness::Ready | Readiness::Claimed | Readiness::Blocked) | None => state_style,
    };
    format!(
        "{} {} {}{}{}{}",
        paint(icon_style, item_icon(state, readiness)),
        id_label(id),
        paint(theme::text_dimmed(), format!("({item_type}:")),
        paint(state_style, state.name()),
        paint(theme::text_dimmed(), suffix),
        paint(theme::text_dimmed(), ")"),
    )
}

const ITEM_COLUMNS: [&str; 5] = [
    "ITEM (TYPE:STATE)",
    "PRIORITY",
    "TITLE",
    "STEPS",
    "BLOCKING",
];

fn item_row(item: &ItemSummary) -> [String; 5] {
    let suffix = if item.archived { " · archived" } else { "" };
    let priority = item.priority.map_or_else(
        || paint(theme::text_dimmed(), "-"),
        |priority| paint(theme::priority(priority), priority),
    );
    let blocking = match item.blocking.split_first() {
        None => String::new(),
        Some((first, [])) => id_label(first.id),
        Some((first, rest)) => format!(
            "{} {}",
            id_label(first.id),
            paint(theme::text_dimmed(), format!("+{}", rest.len()))
        ),
    };
    [
        item_label(item.id, &item.item_type, item.state, item.readiness, suffix),
        priority,
        item.title.clone(),
        steps_label(item.steps),
        blocking,
    ]
}

/// `2/5`, or nothing for an item without steps.
pub(super) fn steps_label(steps: Option<StepsView>) -> String {
    steps.map_or_else(String::new, |steps| {
        format!("{}/{}", steps.done, steps.total)
    })
}

pub fn item_list(items: &[ItemSummary]) -> String {
    if items.is_empty() {
        return "no items\n".to_string();
    }
    let mut table = table(&ITEM_COLUMNS);
    for item in items {
        table.add_row(item_row(item));
    }
    format!("{table}\n")
}

/// `◐ active (3)`: the group's icon and name, with the number of items.
fn group_heading(grouping: ItemGrouping, group: &ItemGroup) -> String {
    let Some(first) = group.items.first() else {
        return group.group.clone();
    };
    let (icon, heading_style) = match grouping {
        ItemGrouping::State => (state_icon(first.state), theme::state(first.state)),
        ItemGrouping::Readiness => match first.readiness {
            Some(readiness) => (readiness_icon(readiness), theme::readiness(readiness)),
            None => ("·", theme::text_dimmed()),
        },
        ItemGrouping::Label | ItemGrouping::Type | ItemGrouping::Lane | ItemGrouping::Priority => {
            ("·", theme::text_dimmed())
        }
    };
    format!(
        "{} {}",
        paint(heading_style, format!("{icon} {}", group.group)),
        paint(theme::text_dimmed(), format!("({})", group.items.len()))
    )
}

/// One table, so columns line up across groups; each group starts with a heading row.
pub fn item_groups(grouping: ItemGrouping, groups: &[ItemGroup]) -> String {
    if groups.is_empty() {
        return "no items\n".to_string();
    }
    let mut table = table(&ITEM_COLUMNS);
    for (index, group) in groups.iter().enumerate() {
        if index > 0 {
            table.add_row([""; 5]);
        }
        table.add_row([
            group_heading(grouping, group),
            String::new(),
            String::new(),
            String::new(),
            String::new(),
        ]);
        for item in &group.items {
            table.add_row(item_row(item));
        }
    }
    format!("{table}\n")
}

pub fn item_detail(detail: &ItemDetail) -> String {
    item_detail_for(detail, text_width(), text_format())
}

/// [`item_detail`] with prose wrapped to `width`, or not at all, and its markdown in `format`.
pub fn item_detail_for(detail: &ItemDetail, width: Option<usize>, format: Format) -> String {
    let summary = &detail.summary;
    let mut out = String::new();
    // A header line continues under its text: past the label for the title, past `↳ ` for a link.
    let mut line = |text: String, indent: &str| {
        let _ = writeln!(out, "{}", fill(&text, width, indent));
    };
    let archived = if summary.archived { "[archived]" } else { "" };
    let label = item_label(
        summary.id,
        &summary.item_type,
        summary.state,
        summary.readiness,
        archived,
    );
    let title_indent = " ".repeat(textwrap::core::display_width(&label) + 1);
    line(
        format!("{label} {}", paint(theme::text_bold(), &summary.title)),
        &title_indent,
    );

    if let Some(status) = status_line(detail) {
        line(format!("  {status}"), "  ");
    }
    line(format!("  {}", byline(detail)), "  ");
    if detail.stats.sessions > 0 {
        line(
            format!(
                "  {}",
                paint(theme::text_dimmed(), item_stats(&detail.stats))
            ),
            "  ",
        );
    }
    if let Some(open) = open_comments_line(detail) {
        line(format!("  {open}"), "  ");
    }
    for link in link_lines(detail) {
        line(link, LINK_INDENT);
    }
    out.push_str(&notices(detail, width, format));
    if !detail.body.is_empty() {
        out.push('\n');
        out.push_str(&body(&detail.body, width, format));
    }
    if !detail.activity.is_empty() {
        out.push_str(&activity(&detail.activity, width, format));
    }
    out
}

/// Priority, lane, review and the other facts about the item that fit on one line, or nothing when
/// there are none.
fn status_line(detail: &ItemDetail) -> Option<String> {
    let summary = &detail.summary;
    let mut status = Vec::new();
    if let Some(priority) = summary.priority {
        status.push(paint(theme::priority(priority), priority));
    }
    if let Some(lane) = &summary.lane {
        status.push(paint(theme::text_dimmed(), format!("lane={lane}")));
    }
    if let Some(review) = &detail.review {
        status.push(paint(theme::text_dimmed(), format!("review={review}")));
    }
    if let Some(recovery) = &detail.recovery {
        status.push(paint(theme::text_dimmed(), format!("recovery={recovery}")));
    }
    if !summary.labels.is_empty() {
        status.push(paint(
            theme::text_dimmed(),
            format!("labels={}", summary.labels.join(",")),
        ));
    }
    if !detail.requires.is_empty() {
        status.push(paint(
            theme::text_dimmed(),
            format!("requires={}", detail.requires.join(",")),
        ));
    }
    if let Some(steps) = summary.steps {
        status.push(format!("steps {}", steps_label(Some(steps))));
    }
    if let Some(review) = &detail.review_status {
        status.push(review_status_label(review));
    }
    if !summary.blocked_by.is_empty() {
        let ids: Vec<String> = summary
            .blocked_by
            .iter()
            .map(|blocker| id_label(blocker.id))
            .collect();
        status.push(format!("blocked by {}", ids.join(", ")));
    }
    if status.is_empty() {
        None
    } else {
        Some(status.join("  "))
    }
}

fn review_status_label(review: &ReviewStatusView) -> String {
    let short_commit = wf_core::session::short_commit;
    match review {
        ReviewStatusView::Awaiting { step } => format!("review: awaiting {step}"),
        ReviewStatusView::ChangesRequested => "review: changes requested".to_string(),
        ReviewStatusView::Approved {
            commit: Some(commit),
        } => format!("review: approved {}", short_commit(commit)),
        ReviewStatusView::Approved { commit: None } => "review: approved".to_string(),
        ReviewStatusView::Merged { commit } => {
            format!("review: merged {}", short_commit(commit))
        }
    }
}

/// Who created the item and when it was created and last updated.
fn byline(detail: &ItemDetail) -> String {
    [
        paint_actor(&detail.author),
        paint(
            theme::text_dimmed(),
            format!("created {}", short_time(detail.created)),
        ),
        paint(
            theme::text_dimmed(),
            format!("updated {}", short_time(detail.updated)),
        ),
    ]
    .join(&format!(" {} ", paint(theme::text_dimmed(), "·")))
}

/// `2 open: ⧗ c1 blocker by @ada · △ c2 change request by @bo`, or nothing when no comment is open.
fn open_comments_line(detail: &ItemDetail) -> Option<String> {
    if detail.open.is_empty() {
        return None;
    }
    let open: Vec<String> = detail
        .open
        .iter()
        .map(|comment| {
            format!(
                "{} {} {} by {}",
                comment_marker(comment.kind),
                comment.id,
                comment_kind_label(comment.kind),
                paint_actor(&comment.by)
            )
        })
        .collect();
    Some(format!(
        "{} {}",
        paint(theme::warning(), format!("{} open:", detail.open.len())),
        open.join(&format!(" {} ", paint(theme::text_dimmed(), "·")))
    ))
}

/// One `↳` line for each link of the item: what it depends on, requires and supersedes, what it
/// references and is referenced by, and who it mentions.
fn link_lines(detail: &ItemDetail) -> Vec<String> {
    let mut lines = Vec::new();
    for (label, links) in [
        ("depends on", detail.depends_on.as_slice()),
        ("required by", &detail.required_by),
        ("supersedes", detail.supersedes.as_slice()),
    ] {
        for link in links {
            let ignored = if link.ignored {
                " [ignored: cycle]"
            } else {
                ""
            };
            lines.push(format!(
                "  ↳ {label} {} \"{}\"{ignored}",
                item_label(link.id, &link.item_type, link.state, None, ""),
                link.title,
            ));
        }
    }
    for reference in &detail.references {
        let link = &reference.item;
        let what = match &reference.comment {
            Some(comment) => format!("{} on ", paint(theme::text_dimmed(), comment)),
            None => String::new(),
        };
        lines.push(format!(
            "  ↳ references {what}{} \"{}\"",
            item_label(link.id, &link.item_type, link.state, None, ""),
            link.title,
        ));
    }
    for backlink in &detail.referenced_by {
        let what = match &backlink.comment {
            Some(comment) => format!("{} ", paint(theme::text_dimmed(), comment)),
            None => String::new(),
        };
        lines.push(format!(
            "  ↳ {what}referenced by {} \"{}\"",
            id_label(backlink.item),
            backlink.title
        ));
    }
    if !detail.mentions.is_empty() {
        let mentions: Vec<String> = detail.mentions.iter().map(paint_actor).collect();
        lines.push(format!("  ↳ mentions {}", mentions.join(" ")));
    }
    lines
}

/// What about an item waits on the principal and what needs attention, in full. Styled, a block
/// under a bar: yellow for what waits, red for failed sessions and the item's flags, each entry
/// under a heading. Raw, one `!` line per entry with what it points to indented below.
pub(super) fn notices(detail: &ItemDetail, width: Option<usize>, format: Format) -> String {
    let mut out = String::new();
    match format {
        Format::Styled => {
            let mut entries: Vec<Notice> = detail
                .waiting
                .iter()
                .map(|waiting| {
                    let style = match waiting.kind {
                        InboxKind::Recovery | InboxKind::MachineFault => theme::error(),
                        InboxKind::Blocker
                        | InboxKind::Proposal
                        | InboxKind::PrincipalReview
                        | InboxKind::Merge
                        | InboxKind::Permission => theme::attention(),
                    };
                    let paragraphs = std::iter::once(&waiting.what)
                        .chain(&waiting.points_to)
                        .cloned()
                        .collect();
                    (style, waiting.heading.as_str(), paragraphs)
                })
                .collect();
            if !detail.flags.is_empty() {
                let flags: Vec<String> = detail
                    .flags
                    .iter()
                    .map(|flag| format!("- {flag}"))
                    .collect();
                entries.push((theme::error(), "needs attention", vec![flags.join("\n")]));
            }
            if entries.is_empty() {
                return out;
            }
            out.push('\n');
            out.push_str(&notice_box(entries, width));
        }
        Format::Raw => {
            for waiting in &detail.waiting {
                let line = format!("! {}: {}", waiting.heading, waiting.what);
                let _ = writeln!(
                    out,
                    "  {}",
                    paint(theme::warning(), fill(&line, width, "    "))
                );
                for paragraph in &waiting.points_to {
                    let _ = writeln!(out, "{}", indented(paragraph, "    "));
                }
            }
            for flag in &detail.flags {
                let _ = writeln!(
                    out,
                    "  {}",
                    paint(theme::warning(), fill(&format!("! {flag}"), width, "    "))
                );
            }
        }
    }
    out
}

/// An entry of a notice box: the style of its bar, its heading, and its paragraphs in markdown.
type Notice<'a> = (anstyle::Style, &'a str, Vec<String>);

/// Notices under a bar in their style, each under its heading, the paragraphs wrapped to `width`
/// and never cut.
fn notice_box(entries: Vec<Notice>, width: Option<usize>) -> String {
    let mut out = String::new();
    // The bar and the space after it.
    let inner = width.map(|width| width.saturating_sub(4));
    for (index, (style, heading, paragraphs)) in entries.into_iter().enumerate() {
        let bar = paint(style, "┃");
        if index > 0 {
            let _ = writeln!(out, "  {bar}");
        }
        let heading = paint(style.bold(), heading);
        let _ = writeln!(out, "  {bar} {heading}");
        for paragraph in paragraphs {
            for line in markdown::render(&paragraph, inner, "")
                .trim_end_matches('\n')
                .lines()
            {
                let _ = writeln!(out, "  {bar} {line}");
            }
        }
    }
    out
}

/// A flag of `wf status` alone, in the notice box of an item: its heading, what is wrong, and the
/// command that fixes it on a line of its own.
pub fn flag_detail_for(flag: &Flag, width: Option<usize>) -> String {
    let mut paragraphs = vec![flag.what()];
    paragraphs.extend(flag.command().map(|command| format!("`{command}`")));
    notice_box(vec![(theme::error(), flag.heading(), paragraphs)], width)
}

/// `3 sessions · 2 submissions · 4 change requests · 2d to done`.
pub fn item_stats(stats: &ItemStats) -> String {
    let count = |count: usize, one: &str, many: &str| {
        format!("{count} {}", if count == 1 { one } else { many })
    };
    let mut parts = vec![
        count(stats.sessions, "session", "sessions"),
        count(stats.submissions, "submission", "submissions"),
        count(stats.change_requests, "change request", "change requests"),
    ];
    if let Some(lead) = stats.lead_ms {
        parts.push(format!("{} to done", crate::text::duration(lead)));
    }
    parts.join(" · ")
}

/// The body, indented by two columns, with each step's number before its text:
/// `- [ ] 1. write parser`. Styled, the markdown is rendered and wrapped to `width` and a step's
/// text continues under its text.
pub(super) fn body(parts: &[BodyPartView], width: Option<usize>, format: Format) -> String {
    let mut out = String::new();
    let mut previous_was_step = false;
    for (index, part) in parts.iter().enumerate() {
        let is_step = matches!(part, BodyPartView::Step { .. });
        if index > 0 {
            out.push_str(if previous_was_step && is_step {
                "\n"
            } else {
                "\n\n"
            });
        }
        match part {
            BodyPartView::Markdown { text } => {
                out.push_str(&markdown_text(text, width, "  ", format));
            }
            BodyPartView::Step { number, done, text } => {
                let checkbox = if *done { "[x]" } else { "[ ]" };
                let number = paint(theme::text_dimmed(), format!("{number}."));
                let marker = format!("- {checkbox} {number} ");
                let step = match format {
                    Format::Styled => markdown::render_after(&marker, text, width, "  ")
                        .trim_end_matches('\n')
                        .to_string(),
                    Format::Raw => indented(&format!("{marker}{text}"), "  "),
                };
                out.push_str(&step);
            }
        }
        previous_was_step = is_step;
    }
    out.push('\n');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_item_that_references_this_one_several_times_is_listed_once() {
        use wf_core::id::{CommentId, HashId, ItemId};
        use wf_core::item::ItemEntry;
        use wf_core::testing::*;

        let fixture = Fixture::new(1);
        let mut state = fixture.state();
        let mut now = 2_000;
        let mut apply = |state: &mut wf_core::state::State, body| {
            now += 1_000;
            fixture.apply(state, 0, now, body)
        };
        let target: ItemId = apply(&mut state, create("task", "Target")).id().into();
        let note = apply(&mut state, comment(target, "a note to point at"));
        let note = CommentId::from(note.id()).short();
        let mention = format!("see #{}", target.short());
        let mut referencing = |title: &str, body: Option<&str>, comments: &[&str]| -> ItemId {
            let created = match body {
                Some(body) => create_with_body("task", title, body),
                None => create("task", title),
            };
            let item: ItemId = apply(&mut state, created).id().into();
            for text in comments {
                apply(&mut state, comment(item, text));
            }
            item
        };
        let body_only = referencing("Body only", Some(&mention), &[]);
        let everywhere = referencing("Everywhere", Some(&mention), &[&mention, &mention]);
        let one_comment = referencing("One comment", None, &[&mention]);
        let comments_only = referencing("Comments only", None, &[&mention, &mention]);
        let note_and_item = referencing(
            "Note and item",
            Some(&mention),
            &[&format!("see #{}/{note}", target.short())],
        );

        let ItemEntry::Live(item) = state.item(target).unwrap() else {
            panic!()
        };
        let detail = crate::view::item_detail(&state, &state.dependency_graph(), item, &[]);
        let shown =
            anstream::adapter::strip_str(&item_detail_for(&detail, None, Format::Raw)).to_string();
        let mut backlinks: Vec<&str> = shown
            .lines()
            .filter(|line| line.contains("referenced by"))
            .map(str::trim)
            .collect();
        backlinks.sort();
        let line = |item: ItemId, rest: &str| format!("↳ referenced by #{}{rest}", item.short());
        let mut expected = vec![
            line(body_only, " \"Body only\""),
            line(everywhere, " \"Everywhere\""),
            line(one_comment, " \"One comment\""),
            line(comments_only, " \"Comments only\""),
            line(note_and_item, " \"Note and item\""),
            format!(
                "↳ {note} referenced by #{} \"Note and item\"",
                note_and_item.short()
            ),
        ];
        expected.sort();
        assert_eq!(backlinks, expected);
    }

    /// An item id that starts with `prefix`.
    fn test_id(prefix: &str) -> ItemId {
        format!("{prefix:a<52}").parse().unwrap()
    }

    fn detail_with_a_reference(title: &str, referenced: &str) -> ItemDetail {
        let author = crate::view::ActorView {
            kind: crate::view::ActorKind::Agent,
            label: "@ada/coder/bh2m".to_string(),
        };
        ItemDetail {
            summary: crate::view::ItemSummary {
                id: test_id("dm3q4j"),
                item_type: "task".to_string(),
                title: title.to_string(),
                state: ItemState::Open,
                priority: None,
                readiness: None,
                blocked_by: Vec::new(),
                blocking: Vec::new(),
                labels: Vec::new(),
                lane: None,
                steps: None,
                archived: false,
            },
            body: Vec::new(),
            author,
            created: jiff::Timestamp::UNIX_EPOCH,
            updated: jiff::Timestamp::UNIX_EPOCH,
            review: None,
            review_status: None,
            recovery: None,
            stats: Default::default(),
            waiting: Vec::new(),
            requires: Vec::new(),
            depends_on: Vec::new(),
            required_by: Vec::new(),
            supersedes: None,
            references: vec![crate::view::ReferenceView {
                item: crate::view::LinkView {
                    id: test_id("kvb7vj"),
                    item_type: "task".to_string(),
                    title: referenced.to_string(),
                    state: ItemState::Open,
                    ignored: false,
                },
                comment: None,
            }],
            referenced_by: Vec::new(),
            mentions: Vec::new(),
            activity: Vec::new(),
            open: Vec::new(),
            flags: Vec::new(),
        }
    }

    #[test]
    fn header_lines_continue_under_their_text() {
        let detail = detail_with_a_reference(
            "Wrap the header of wf item show under its indent",
            "Wrap long activity lines under their indent",
        );
        let shown =
            anstream::adapter::strip_str(&item_detail_for(&detail, Some(40), Format::Styled))
                .to_string();
        let time = short_time(jiff::Timestamp::UNIX_EPOCH);
        let (date, clock) = time.split_once(' ').unwrap();
        assert_eq!(
            shown,
            format!(
                "○ #dm3q4j (task:open) Wrap the header of\n\
                 \x20                     wf item show under\n\
                 \x20                     its indent\n\
                 \x20 @ada/coder/bh2m · created {date}\n\
                 \x20 {clock} · updated {time}\n\
                 \x20 ↳ references ○ #kvb7vj (task:open)\n\
                 \x20   \"Wrap long activity lines under\n\
                 \x20   their indent\"\n"
            )
        );
        let shown =
            anstream::adapter::strip_str(&item_detail_for(&detail, None, Format::Raw)).to_string();
        assert_eq!(
            shown,
            format!(
                "○ #dm3q4j (task:open) Wrap the header of wf item show under its indent\n\
                 \x20 @ada/coder/bh2m · created {time} · updated {time}\n\
                 \x20 ↳ references ○ #kvb7vj (task:open) \"Wrap long activity lines under their indent\"\n"
            )
        );
    }

    const BODY: &str = "## Plan\n\nKeep **bold** and `#k3f9` as links.\n\n- one\n- two\n\n```\nlet value = 1;\n```\n\n| who | what it does to the item |\n|-----|--------------------------|\n| ada | renders it               |";

    fn detail_with_a_body() -> ItemDetail {
        let mut detail = detail_with_a_reference("Render", "Other");
        detail.references.clear();
        detail.body = vec![
            BodyPartView::Markdown {
                text: BODY.to_string(),
            },
            BodyPartView::Step {
                number: 1,
                done: true,
                text: "render the **body**".to_string(),
            },
            BodyPartView::Step {
                number: 2,
                done: false,
                text: "render the comments under their line".to_string(),
            },
        ];
        detail
    }

    fn body_of(shown: &str) -> String {
        let plain = anstream::adapter::strip_str(shown).to_string();
        let (_, body) = plain.split_once("\n\n").unwrap();
        body.to_string()
    }

    #[test]
    fn a_body_is_rendered_when_styled() {
        let shown = item_detail_for(&detail_with_a_body(), Some(30), Format::Styled);
        assert!(shown.contains(&paint(theme::text_bold(), "bold")));
        assert!(shown.contains(&paint(theme::text_dimmed(), "2.")));
        assert_eq!(
            body_of(&shown),
            "  Plan\n\
             \n\
             \x20 Keep bold and #k3f9 as\n\
             \x20 links.\n\
             \n\
             \x20 • one\n\
             \x20 • two\n\
             \n\
             \x20     let value = 1;\n\
             \n\
             \x20 | who | what it does to the item |\n\
             \x20 |-----|--------------------------|\n\
             \x20 | ada | renders it               |\n\
             \n\
             \x20 - [x] 1. render the body\n\
             \x20 - [ ] 2. render the comments\n\
             \x20          under their line\n"
        );
    }

    #[test]
    fn a_body_is_its_markdown_when_raw() {
        let shown = item_detail_for(&detail_with_a_body(), None, Format::Raw);
        let indented: String = BODY
            .lines()
            .map(|line| match line {
                "" => "\n".to_string(),
                line => format!("  {line}\n"),
            })
            .collect();
        assert_eq!(
            body_of(&shown),
            format!(
                "{indented}\n  - [x] 1. render the **body**\n  - [ ] 2. render the comments under their line\n"
            )
        );
    }

    #[test]
    fn a_comment_is_rendered_under_its_line_at_the_comment_indent() {
        let by = crate::view::ActorView {
            kind: crate::view::ActorKind::Agent,
            label: "@ada/coder/bh2m".to_string(),
        };
        let entry = crate::view::ActivityView {
            at: jiff::Timestamp::UNIX_EPOCH,
            by: by.clone(),
            kind: ActivityKind::Comment,
            summary: "commented".to_string(),
            comment: Some(crate::view::CommentView {
                id: "nn73nn73nn73".to_string(),
                short: "nn73".to_string(),
                kind: CommentKindView::Comment,
                by,
                at: jiff::Timestamp::UNIX_EPOCH,
                session: None,
                step: None,
                resolves: Vec::new(),
                resolved_by: None,
            }),
            text: Some("Chose **this**:\n\n- see `#k3f9`".to_string()),
        };
        let (line, text) = activity_entry(&entry, "  00:27  ", Some(60), Format::Styled);
        assert_eq!(
            anstream::adapter::strip_str(&line).to_string(),
            "  00:27  @ada/coder/bh2m · commented · nn73"
        );
        assert_eq!(
            anstream::adapter::strip_str(&text.unwrap()).to_string(),
            format!("{ACTIVITY_TEXT_INDENT}Chose this:\n\n{ACTIVITY_TEXT_INDENT}• see #k3f9")
        );
        let (_, text) = activity_entry(&entry, "  00:27  ", None, Format::Raw);
        assert_eq!(
            text.unwrap(),
            format!("{ACTIVITY_TEXT_INDENT}Chose **this**:\n\n{ACTIVITY_TEXT_INDENT}- see `#k3f9`")
        );
    }

    /// The detail of an approved item whose merge waits on a dirty checkout, which the daemon
    /// commented on twice.
    fn held_merge() -> ItemDetail {
        merge_pending(
            &[],
            &[
                "Waiting to merge: main kept moving.",
                "Waiting to merge: main is checked out in /src/app with changes to src/lib.rs, Cargo.toml. Commit or stash them.",
            ],
        )
    }

    /// The detail of an approved item whose merge is pending. With `earlier` comments, the daemon
    /// made them on an earlier submission, before the latest one; `after` it made on the latest.
    fn merge_pending(earlier: &[&str], after: &[&str]) -> ItemDetail {
        use wf_core::id::{ItemId, SessionId};
        use wf_core::identity::Actor;
        use wf_core::item::ItemEntry;
        use wf_core::session::{CommitId, EndReason, Verdict};
        use wf_core::testing::*;

        let fixture = Fixture::new(1);
        let mut state = fixture.state();
        let mut events = Vec::new();
        let mut now = 2_000;
        let mut apply = |state: &mut wf_core::state::State, actor: Actor, body| {
            now += 1_000;
            let event = fixture.author_as(state, 0, actor, now, body);
            state
                .receive(event.clone(), wf_core::state::Trust::Untrusted)
                .unwrap();
            events.push(event.clone());
            event
        };
        let principal = fixture.founder_actor();
        let daemon = Actor::Daemon(fixture.founder.key());
        let commit = CommitId::Git("75eba435".repeat(5).parse().unwrap());
        apply(&mut state, principal.clone(), define_agent("coder"));
        let item: ItemId = apply(
            &mut state,
            principal.clone(),
            create_reviewed("task", "Held", "standard"),
        )
        .id()
        .into();
        let submitted = EndReason::Submitted {
            commit: Some(commit),
        };
        if !earlier.is_empty() {
            let session: SessionId =
                apply(&mut state, principal.clone(), start_session("coder", item))
                    .id()
                    .into();
            apply(
                &mut state,
                principal.clone(),
                end_session(session, submitted),
            );
            for text in earlier {
                apply(&mut state, daemon.clone(), comment(item, text));
            }
        }
        let session: SessionId = apply(&mut state, principal.clone(), start_session("coder", item))
            .id()
            .into();
        apply(
            &mut state,
            principal.clone(),
            end_session(session, submitted),
        );
        apply(
            &mut state,
            principal.clone(),
            moved(item, ItemState::Review),
        );
        apply(
            &mut state,
            principal.clone(),
            verdict(item, commit, Verdict::Approve),
        );
        for text in after {
            apply(&mut state, daemon.clone(), comment(item, text));
        }
        let ItemEntry::Live(live) = state.item(item).unwrap() else {
            panic!()
        };
        crate::view::item_detail(&state, &state.dependency_graph(), live, &events)
    }

    /// The lines of the notice box, without the bar.
    fn boxed(shown: &str) -> Vec<String> {
        anstream::adapter::strip_str(shown)
            .to_string()
            .lines()
            .filter_map(|line| line.strip_prefix("  ┃"))
            .map(|line| line.trim().to_string())
            .collect()
    }

    #[test]
    fn a_held_merge_shows_in_full_with_its_files_in_the_box() {
        let detail = held_merge();
        let what = &detail.waiting[0].what;
        assert!(what.contains("is ready to merge"), "{what}");
        let shown = item_detail_for(&detail, Some(40), Format::Styled);
        let plain = anstream::adapter::strip_str(&shown).to_string();
        assert!(
            plain
                .lines()
                .all(|line| textwrap::core::display_width(line) <= 40),
            "{plain}"
        );
        let boxed = boxed(&shown);
        assert_eq!(boxed[0], "merge waits");
        let text = boxed[1..].join(" ");
        assert!(text.contains(what.as_str()), "{text}");
        assert!(
            text.contains("with changes to src/lib.rs, Cargo.toml. Commit or stash them."),
            "{text}"
        );
        assert!(!text.contains("kept moving"), "only the latest: {text}");
        let body = plain.find("activity").unwrap();
        assert!(plain.find("merge waits").unwrap() < body, "{plain}");
    }

    #[test]
    fn a_daemon_comment_from_an_earlier_round_is_not_in_the_box() {
        let detail = merge_pending(
            &[
                "Merging into main conflicts in src/lib.rs. The next session merges main into the branch first.",
            ],
            &[],
        );
        let shown = item_detail_for(&detail, Some(60), Format::Styled);
        let boxed = boxed(&shown);
        assert_eq!(boxed[0], "merge waits");
        assert!(detail.waiting[0].points_to.is_empty(), "{boxed:?}");
        assert!(!boxed.join(" ").contains("conflicts"), "{boxed:?}");
    }

    #[test]
    fn an_item_with_nothing_waiting_has_no_box() {
        let shown = item_detail_for(&detail_with_a_body(), Some(40), Format::Styled);
        assert!(boxed(&shown).is_empty(), "{shown}");
    }

    #[test]
    fn piped_what_waits_and_the_flags_are_plain_lines() {
        let mut detail = held_merge();
        detail.flags = vec!["a task needs a priority".to_string()];
        let shown =
            anstream::adapter::strip_str(&item_detail_for(&detail, None, Format::Raw)).to_string();
        assert!(!shown.contains('┃'), "{shown}");
        let expected = format!(
            "  ! merge waits: {}\n    Waiting to merge: main is checked out in /src/app with changes to src/lib.rs, Cargo.toml. Commit or stash them.\n  ! a task needs a priority\n",
            detail.waiting[0].what
        );
        assert!(shown.contains(&expected), "{shown}");
    }
}
