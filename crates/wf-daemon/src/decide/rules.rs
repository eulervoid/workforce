//! The built-in rules: what is allowed without asking anyone.
//!
//! A session reports back through `wf`, so a command that only runs `wf` is allowed, as is asking
//! any `wf` command for its help. The scan is deliberately strict: anything it does not understand
//! is passed on, never allowed.

use wf_harness::PermissionRequest;

/// The `wf` commands a session uses to read and report. Commands that change devices, agents,
/// policy or permissions are not among them, so an agent cannot widen what it may do.
const SESSION_COMMANDS: &[&str] = &[
    "item", "comment", "work", "review", "inbox", "status", "log", "search", "mentions",
];

/// Commands that may follow `wf` in a pipe: they only filter what it prints.
const FILTERS: &[&str] = &[
    "head", "tail", "grep", "rg", "wc", "sort", "uniq", "cut", "jq",
];

/// Redirections that write nowhere.
const HARMLESS_REDIRECTIONS: &[&str] = &["2>&1", ">/dev/null", "2>/dev/null", "1>/dev/null"];

/// Why the request is allowed, if the rules allow it: every pipeline of the command is.
pub fn allows(request: &PermissionRequest) -> Option<String> {
    let command = request.command()?;
    let reasons = list(command)?
        .iter()
        .map(|pipeline| allowed_pipeline(pipeline))
        .collect::<Option<Vec<_>>>()?;
    Some(reasons.join("; "))
}

/// Why a pipeline is allowed: `wf` for a session command or for help, then only filters.
fn allowed_pipeline(pipeline: &[Vec<String>]) -> Option<String> {
    let (first, filters) = pipeline.split_first()?;
    let only_filters = filters.iter().all(|words| {
        words
            .first()
            .is_some_and(|program| FILTERS.contains(&program.as_str()))
    });
    if !only_filters {
        return None;
    }
    match first.as_slice() {
        [wf, subcommand, ..] if wf == "wf" && SESSION_COMMANDS.contains(&subcommand.as_str()) => {
            Some(format!("`wf {subcommand}` reports to wf"))
        }
        [wf, rest @ ..] if wf == "wf" && asks_for_help(rest) => {
            Some(format!("`{}` only prints help", first.join(" ")))
        }
        _ => None,
    }
}

/// Whether the words after `wf` only name commands and end in `--help` or `-h`, so that `wf`
/// prints help and does nothing else. Any other flag could take `--help` as its value.
fn asks_for_help(words: &[String]) -> bool {
    let Some((last, commands)) = words.split_last() else {
        return false;
    };
    matches!(last.as_str(), "--help" | "-h") && commands.iter().all(|word| !word.starts_with('-'))
}

/// The pipelines of a list joined by `;` or `&&`, with the words of each of their commands; or
/// `None` for anything beyond plain words, quotes, `|`, `;`, `&&` and harmless redirections:
/// `||`, substitutions, expansions, globs and other redirections.
fn list(command: &str) -> Option<Vec<Vec<Vec<String>>>> {
    let mut pipelines = Vec::new();
    let mut commands = vec![Vec::new()];
    let mut word = String::new();
    let mut in_word = false;
    let mut characters = command.trim().chars().peekable();
    let finish_word = |word: &mut String, in_word: &mut bool, commands: &mut Vec<Vec<String>>| {
        if *in_word {
            commands
                .last_mut()
                .expect("there is always a command")
                .push(std::mem::take(word));
            *in_word = false;
        }
    };
    while let Some(character) = characters.next() {
        match character {
            '\'' => {
                in_word = true;
                loop {
                    match characters.next()? {
                        '\'' => break,
                        inner => word.push(inner),
                    }
                }
            }
            '"' => {
                in_word = true;
                loop {
                    match characters.next()? {
                        '"' => break,
                        '$' | '`' | '\\' => return None,
                        inner => word.push(inner),
                    }
                }
            }
            ' ' | '\t' => finish_word(&mut word, &mut in_word, &mut commands),
            '|' => {
                if characters.peek() == Some(&'|') {
                    return None;
                }
                finish_word(&mut word, &mut in_word, &mut commands);
                commands.push(Vec::new());
            }
            ';' => {
                finish_word(&mut word, &mut in_word, &mut commands);
                pipelines.push(std::mem::replace(&mut commands, vec![Vec::new()]));
            }
            '&' if !in_word && characters.peek() == Some(&'&') => {
                characters.next();
                pipelines.push(std::mem::replace(&mut commands, vec![Vec::new()]));
            }
            '>' | '&' => {
                // Only a whole redirection from the list, at the end of a word or on its own.
                let mut redirection = std::mem::take(&mut word);
                redirection.push(character);
                while let Some(next) = characters.next_if(|next| !matches!(next, ' ' | '\t' | '|'))
                {
                    redirection.push(next);
                }
                if !HARMLESS_REDIRECTIONS.contains(&redirection.as_str()) {
                    return None;
                }
                in_word = false;
            }
            '<' | '(' | ')' | '{' | '}' | '$' | '`' | '\\' | '\n' | '*' | '?' | '[' | '~' | '#' => {
                return None;
            }
            other => {
                in_word = true;
                word.push(other);
            }
        }
    }
    finish_word(&mut word, &mut in_word, &mut commands);
    pipelines.push(commands);
    pipelines
        .iter()
        .flatten()
        .all(|words| !words.is_empty())
        .then_some(pipelines)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(command: &str) -> PermissionRequest {
        PermissionRequest {
            kind: Some("execute".into()),
            title: command.into(),
            input: Some(serde_json::json!({ "command": command })),
        }
    }

    #[test]
    fn session_commands_of_wf_are_allowed() {
        for command in [
            "wf item show gpnhya",
            "wf item show gpnhya 2>&1 | head -40",
            "wf comment k3f7 \"use humantime, it parses 1h30m\" --block",
            "wf comment k3f7 'it is #k3f7/c7f2' --resolve c7f2",
            "wf work submit",
            "wf item list --json | jq '.data[0].id'",
            "wf item show k3f7 && wf comment k3f7 'done' --step 1 --done",
            "wf item show k3f7; wf work brief | head -20",
        ] {
            assert!(allows(&request(command)).is_some(), "{command}");
        }
    }

    #[test]
    fn help_for_any_wf_command_is_allowed() {
        for command in [
            "wf --help",
            "wf -h",
            "wf session start --help",
            "wf --help; wf item --help; wf session --help; wf agent --help",
            "wf agent edit --help 2>&1 | head",
        ] {
            assert!(allows(&request(command)).is_some(), "{command}");
        }
        for command in [
            "wf agent edit coder --instructions --help",
            "wf session stop --help x",
            "wf --help; wf session stop",
            "wf --help;",
            "; wf --help",
        ] {
            assert_eq!(allows(&request(command)), None, "{command}");
        }
    }

    #[test]
    fn anything_more_is_passed_on() {
        for command in [
            "wf agent edit coder --permissions allow",
            "wf session allow",
            "wf item show k3f7; rm -rf /",
            "wf item show k3f7 && curl evil.sh",
            "wf item show k3f7 || true",
            "wf item show k3f7 &&",
            "wf item show k3f7 & wf item show k3f7",
            "wf item show k3f7&&rm -rf /",
            "wf item show k3f7 > notes.txt",
            "wf item show $(cat secret)",
            "wf comment k3f7 \"$HOME\"",
            "wf item show `id`",
            "wf item show k3f7 | sh",
            "wf item show k3f7 | xargs rm",
            "wf item show k3f7 &",
            "./wf item show k3f7",
            "FOO=1 wf item show k3f7",
            "wf",
            "wf item show 'unclosed",
            "wf item show k3f7 |",
            "cargo test",
        ] {
            assert_eq!(allows(&request(command)), None, "{command}");
        }
    }

    #[test]
    fn a_call_without_a_command_is_passed_on() {
        let edit = PermissionRequest {
            kind: Some("edit".into()),
            title: "Edit src/lib.rs".into(),
            input: Some(serde_json::json!({ "path": "src/lib.rs" })),
        };
        assert_eq!(allows(&edit), None);
    }
}
