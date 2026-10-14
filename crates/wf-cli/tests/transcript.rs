//! How `wf session watch` draws a real transcript: a read-only task whose first `wf comment`
//! failed inside a chained command and whose retry found its worktree gone.

use jiff::tz::TimeZone;
use wf_cli::transcript::{Format, Printer, Session, Verbosity};
use wf_harness::TranscriptEntry;

const TRANSCRIPT: &str = include_str!("fixtures/transcript-ivnj5b.jsonl");

fn render(width: usize, format: Format, verbosity: Verbosity) -> String {
    let mut printer = Printer::new("coder", width, format, verbosity, TimeZone::UTC);
    let mut out = String::new();
    for line in TRANSCRIPT.lines() {
        let entry: TranscriptEntry = serde_json::from_str(line).unwrap();
        out.push_str(&printer.render(&entry));
    }
    out.push_str(&printer.finish(Session::Ended));
    out
}

/// Styled output with its escape codes spelled out, so the snapshot shows where color goes.
fn visible(text: &str) -> String {
    text.replace('\x1b', "␛")
}

#[test]
fn abridged() {
    for width in [80, 120] {
        let styled = render(width, Format::Styled, Verbosity::Abridged);
        insta::assert_snapshot!(format!("abridged_{width}"), visible(&styled));
    }
}

#[test]
fn full() {
    for width in [80, 120] {
        let styled = render(width, Format::Styled, Verbosity::Full);
        insta::assert_snapshot!(format!("full_{width}"), visible(&styled));
    }
}

#[test]
fn raw() {
    for width in [80, 120] {
        let raw = render(width, Format::Raw, Verbosity::Abridged);
        assert!(!raw.contains('\x1b') && !raw.contains('│'));
        insta::assert_snapshot!(format!("raw_{width}"), raw);
    }
}

#[test]
fn every_line_fits_the_width() {
    for width in [80, 120] {
        for verbosity in [Verbosity::Abridged, Verbosity::Full] {
            let raw = render(width, Format::Raw, verbosity);
            for line in raw.lines() {
                assert!(
                    textwrap::core::display_width(line) <= width,
                    "wider than {width}: {line}"
                );
            }
        }
    }
}
