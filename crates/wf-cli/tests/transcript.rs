//! How `wf session watch` draws a real transcript: a read-only task whose first `wf comment`
//! failed inside a chained command and whose retry found its worktree gone.

use jiff::tz::TimeZone;
use wf_cli::transcript::{Detail, Format, Renderer};
use wf_harness::TranscriptEntry;

const TRANSCRIPT: &str = include_str!("fixtures/transcript-ivnj5b.jsonl");

fn render(width: usize, format: Format, detail: Detail) -> String {
    let mut renderer = Renderer::new("coder", width, format, detail, TimeZone::UTC);
    let mut out = String::new();
    for line in TRANSCRIPT.lines() {
        let entry: TranscriptEntry = serde_json::from_str(line).unwrap();
        out.push_str(&renderer.render(&entry));
    }
    out.push_str(&renderer.finish());
    out
}

/// Styled output with its escape codes spelled out, so the snapshot shows where color goes.
fn visible(text: &str) -> String {
    text.replace('\x1b', "␛")
}

#[test]
fn abridged() {
    for width in [80, 120] {
        let styled = render(width, Format::Styled, Detail::Abridged);
        insta::assert_snapshot!(format!("abridged_{width}"), visible(&styled));
    }
}

#[test]
fn full() {
    for width in [80, 120] {
        let styled = render(width, Format::Styled, Detail::Full);
        insta::assert_snapshot!(format!("full_{width}"), visible(&styled));
    }
}

#[test]
fn raw() {
    for width in [80, 120] {
        let raw = render(width, Format::Raw, Detail::Abridged);
        assert!(!raw.contains('\x1b') && !raw.contains('│'));
        insta::assert_snapshot!(format!("raw_{width}"), raw);
    }
}

#[test]
fn every_line_fits_the_width() {
    for width in [80, 120] {
        for detail in [Detail::Abridged, Detail::Full] {
            let raw = render(width, Format::Raw, detail);
            for line in raw.lines() {
                assert!(
                    textwrap::core::display_width(line) <= width,
                    "wider than {width}: {line}"
                );
            }
        }
    }
}
