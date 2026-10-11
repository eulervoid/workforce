//! Every `wf` command a brief tells an agent to run is one the CLI takes.

use clap::Parser;
use wf_cli::cli::Cli;
use wf_core::brief::{Dependency, Earlier, OpenComment, ReviewBrief, SuperviseBrief, WorkBrief};

fn open(comment: &str) -> Vec<OpenComment> {
    vec![OpenComment {
        comment: comment.into(),
        by: "reviewer".into(),
        text: "exit with 2".into(),
    }]
}

fn briefs() -> Vec<String> {
    let earlier = || {
        vec![Earlier {
            by: "coder".into(),
            kind: "comment".into(),
            text: "took node --test".into(),
        }]
    };
    let work = WorkBrief {
        agent: "coder".into(),
        item: "k3f9ab".into(),
        title: "Add the summary command".into(),
        body: "- [ ] 1. parse the arguments".into(),
        branch: "wf/k3f9ab".into(),
        target: "main".into(),
        dependencies: vec![Dependency {
            item: "q7x2cd".into(),
            title: "Parser".into(),
            state: "done".into(),
            note: None,
        }],
        change_requests: open("c4d2e1"),
        earlier: earlier(),
    };
    let review = |commit: Option<&str>| ReviewBrief {
        agent: "reviewer".into(),
        item: "k3f9ab".into(),
        title: "Add the summary command".into(),
        body: String::new(),
        commit: commit.map(Into::into),
        target: "main".into(),
        step: "a review".into(),
        earlier: earlier(),
        diff: "+added".into(),
    };
    let supervise = SuperviseBrief {
        agent: "supervisor".into(),
        item: "k3f9ab".into(),
        title: "Add the summary command".into(),
        body: String::new(),
        earlier: earlier(),
        blockers: open("b7e3f0"),
        person: "@joshua".into(),
    };
    vec![
        work.render(),
        review(Some("abc123")).render(),
        review(None).render(),
        supervise.render(),
    ]
}

/// The spans in backticks that start with `wf `.
fn commands(text: &str) -> Vec<&str> {
    text.split('`')
        .skip(1)
        .step_by(2)
        .filter(|span| span.starts_with("wf "))
        .collect()
}

/// The words of a command as a shell splits them, with the placeholders outside quotes filled.
fn words(command: &str) -> Vec<String> {
    let mut words = Vec::new();
    let mut word = String::new();
    let mut quoted = false;
    for character in command.chars() {
        match character {
            '"' => quoted = !quoted,
            ' ' if !quoted => words.push(std::mem::take(&mut word)),
            other => word.push(other),
        }
    }
    words.push(word);
    words
        .into_iter()
        .filter(|word| !word.is_empty())
        .map(|word| match word.as_str() {
            "<number>" => "1".to_string(),
            "<id>" => "c4d2e1".to_string(),
            _ => word,
        })
        .collect()
}

#[test]
fn every_command_in_a_brief_parses() {
    let mut checked = 0;
    for brief in briefs() {
        for command in commands(&brief) {
            if let Err(error) = Cli::try_parse_from(words(command)) {
                panic!("`{command}` does not parse: {error}");
            }
            checked += 1;
        }
    }
    assert!(checked >= 15, "only {checked} commands found");
    assert!(
        Cli::try_parse_from(words("wf work finish --note \"done\"")).is_err(),
        "a command the CLI does not have fails"
    );
}
