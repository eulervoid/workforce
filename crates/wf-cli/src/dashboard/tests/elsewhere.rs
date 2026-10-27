//! Dashboard tests: the transcript of a session of another device, read from the copy the daemon
//! keeps of it.

use super::support::*;
use crate::dashboard::app::App;
use crate::dashboard::snapshot::{Detail, Place};
use jiff::tz::TimeZone;
use ratatui::crossterm::event::KeyCode;
use wf_core::identity::DeviceSigner;
use wf_daemon::proto::TranscriptCopy;

/// The app with the coder's session on the vm open.
fn coder_on_vm() -> (App, crate::dashboard::snapshot::SessionInfo) {
    let mut on_vm = snapshot();
    on_vm.working[0].place = Place::Elsewhere("vm".to_owned());
    let coder = on_vm.working[0].clone();
    let mut app = App::new(on_vm.clone(), TimeZone::UTC);
    app.resize(100, 20);
    press(&mut app, KeyCode::Enter);
    on_vm.in_view = Some(session_in_view(coder.clone()));
    app.refresh(on_vm);
    assert_eq!(app.wanted().detail, Some(Detail::Session(coder.id)));
    (app, coder)
}

fn unreachable() -> TranscriptCopy {
    TranscriptCopy::Unreachable {
        device: DeviceSigner::from_bytes(&[2; 32]).unwrap().key(),
        reason: "timed out".to_owned(),
    }
}

#[test]
fn the_transcript_of_another_device_is_fetched_and_shown() {
    let (mut app, coder) = coder_on_vm();
    app.copy_followed(coder.id, TranscriptCopy::Fetching);
    let fetching = screen(&mut app, 100, 20);
    assert!(
        fetching.contains("fetching the transcript from vm"),
        "{fetching}"
    );
    app.copy_followed(coder.id, TranscriptCopy::CaughtUp);
    app.transcribed(coder.id, &coder_transcript());
    let shown = screen(&mut app, 100, 20);
    assert!(
        shown.contains("The loader now defers parsing until the first access."),
        "{shown}"
    );
    assert!(!shown.contains("vm"), "{shown}");
}

#[test]
fn a_device_that_cannot_be_reached_is_said_under_what_was_fetched_before() {
    let (mut app, coder) = coder_on_vm();
    app.copy_followed(coder.id, unreachable());
    let nothing = screen(&mut app, 100, 20);
    assert!(
        nothing.contains(
            "vm cannot be reached (timed out), and nothing of the transcript was fetched before"
        ),
        "{nothing}"
    );
    app.transcribed(coder.id, &coder_transcript());
    let fetched = screen(&mut app, 100, 20);
    assert!(
        fetched.contains("The loader now defers parsing until the first access."),
        "{fetched}"
    );
    assert!(
        fetched.contains("vm cannot be reached (timed out); this is what was fetched before"),
        "{fetched}"
    );
    // Reached again: the transcript goes on without the warning.
    app.copy_followed(coder.id, TranscriptCopy::CaughtUp);
    assert!(!screen(&mut app, 100, 20).contains("cannot be reached"));
}
