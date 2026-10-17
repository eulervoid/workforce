//! `wf agent add` and `edit` refuse a harness this machine cannot start.

mod common;

use common::Sandbox;

fn sandbox() -> Sandbox {
    let wf = Sandbox::new();
    wf.run(&["init", "--name", "demo"]);
    wf
}

#[test]
fn a_typo_in_the_harness_is_refused_with_the_closest_known_one() {
    let wf = sandbox();
    let error = wf.fail(&["agent", "add", "coder", "--harness", "claud-code"]);
    assert!(error.contains("did you mean `claude-code`?"), "{error}");
    assert!(error.contains("`opencode`"), "{error}");
    assert!(wf.json(&["agent", "list"]).as_array().unwrap().is_empty());
}

#[test]
fn an_unknown_harness_is_defined_with_a_warning_and_marked_in_the_list() {
    let wf = sandbox();
    let defined = wf.run(&[
        "agent",
        "add",
        "coder",
        "--harness",
        "elsewhere",
        "--harness-unknown-here",
    ]);
    assert!(defined.contains("warning:"), "{defined}");
    let agents = wf.json(&["agent", "list"]);
    assert_eq!(agents[0]["harness_startable_here"], false);
    assert!(wf.run(&["agent", "list"]).contains("not startable here"));
}

#[test]
fn edit_refuses_an_unknown_harness_but_keeps_a_known_one() {
    let wf = sandbox();
    wf.run(&["agent", "add", "coder", "--harness", "claude-code"]);
    let error = wf.fail(&["agent", "edit", "coder", "--harness", "claud-code"]);
    assert!(error.contains("did you mean `claude-code`?"), "{error}");
    wf.run(&["agent", "edit", "coder", "--model", "deep"]);
    wf.run(&["agent", "edit", "coder", "--harness", "opencode"]);
    let agents = wf.json(&["agent", "list"]);
    assert_eq!(agents[0]["harness"], "opencode");
    assert_eq!(agents[0]["harness_startable_here"], true);
}
