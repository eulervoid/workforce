//! A program that decides: the request as JSON on its stdin, its answer as JSON on its stdout.

use std::process::Stdio;
use std::time::Duration;

use tokio::io::AsyncWriteExt;
use tokio::process::Command;

use super::{DeciderRequest, Outcome, read_answer};

pub(super) async fn ask(
    command: &[String],
    timeout: Duration,
    request: &DeciderRequest,
) -> Outcome {
    let passed = |why: String| Outcome::Passed { why };
    let Some((program, arguments)) = command.split_first() else {
        return passed("it has no command".to_string());
    };
    let input = match serde_json::to_vec(request) {
        Ok(input) => input,
        Err(error) => return passed(format!("the request does not encode: {error}")),
    };
    let child = Command::new(program)
        .args(arguments)
        .current_dir(&request.directory)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn();
    let mut child = match child {
        Ok(child) => child,
        Err(error) => return passed(format!("`{program}` does not start: {error}")),
    };
    let mut stdin = child.stdin.take().expect("stdin is piped");
    let run = async {
        // A hook may answer without reading all of it; that is not a failure.
        let _ = stdin.write_all(&input).await;
        drop(stdin);
        child.wait_with_output().await
    };
    match tokio::time::timeout(timeout, run).await {
        Err(_) => passed(format!(
            "it did not answer within {}",
            humantime::format_duration(timeout)
        )),
        Ok(Err(error)) => passed(format!("it failed: {error}")),
        Ok(Ok(output)) if !output.status.success() => passed(format!(
            "it exited with {}: {}",
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        )),
        Ok(Ok(output)) => read_answer(&String::from_utf8_lossy(&output.stdout)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::decide::{AgentContext, ItemContext};
    use wf_harness::{PermissionMode, PermissionRequest};

    fn request() -> DeciderRequest {
        DeciderRequest {
            hook: "permission".into(),
            version: 1,
            workforce: "w".into(),
            item: ItemContext {
                id: "k3f7".into(),
                title: "t".into(),
                item_type: "task".into(),
                state: "in_progress".into(),
            },
            session: "s".into(),
            agent: AgentContext {
                name: "coder".into(),
                capabilities: vec![],
            },
            directory: std::env::temp_dir(),
            tool: PermissionRequest {
                kind: Some("execute".into()),
                title: "cargo test".into(),
                input: Some(serde_json::json!({"command": "cargo test"})),
            },
            guidance: None,
        }
    }

    fn sh(script: &str) -> Vec<String> {
        vec!["sh".into(), "-c".into(), script.into()]
    }

    #[tokio::test]
    async fn the_hook_reads_the_request_and_answers() {
        let script =
            r#"grep -q '"command":"cargo test"' && echo '{"permission":"allow","reason":"tests"}'"#;
        assert_eq!(
            ask(&sh(script), Duration::from_secs(5), &request()).await,
            Outcome::Decided {
                permission: PermissionMode::Allow,
                reason: Some("tests".into())
            }
        );
    }

    #[tokio::test]
    async fn a_failing_or_slow_hook_passes_on() {
        for script in ["exit 3", "sleep 5", "echo nonsense"] {
            let outcome = ask(&sh(script), Duration::from_millis(300), &request()).await;
            assert!(
                matches!(outcome, Outcome::Passed { .. }),
                "{script}: {outcome:?}"
            );
        }
        let missing = ask(
            &["wf-no-such-hook".to_string()],
            Duration::from_secs(1),
            &request(),
        )
        .await;
        assert!(matches!(missing, Outcome::Passed { .. }));
    }
}
