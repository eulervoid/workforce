use std::io;
use std::sync::{Arc, Mutex};

use wf_harness::{Direction, PermissionMode, Renderer, Run, StopReason, TranscriptEntry};

#[derive(Clone, Default)]
struct SharedBuffer(Arc<Mutex<Vec<u8>>>);

impl io::Write for SharedBuffer {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

async fn run_fake_agent(permissions: PermissionMode) -> Vec<TranscriptEntry> {
    let directory = tempfile::tempdir().unwrap();
    let buffer = SharedBuffer::default();
    let stop_reason = Run {
        agent: agent_client_protocol::AcpAgentConfig::new(env!("CARGO_BIN_EXE_fake-acp-agent")),
        directory: directory.path(),
        prompt: "do the thing",
        permissions,
        transcript: buffer.clone(),
    }
    .execute()
    .await
    .unwrap();
    assert!(matches!(stop_reason, StopReason::EndTurn));

    let bytes = buffer.0.lock().unwrap().clone();
    String::from_utf8(bytes)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect()
}

fn rendered(entries: &[TranscriptEntry]) -> String {
    let mut renderer = Renderer::default();
    entries.iter().map(|entry| renderer.render(entry)).collect()
}

#[tokio::test]
async fn records_both_directions_and_the_agent_log() {
    let entries = run_fake_agent(PermissionMode::Allow).await;
    let methods: Vec<_> = entries
        .iter()
        .filter(|entry| entry.direction == Direction::ToAgent)
        .filter_map(|entry| entry.message["method"].as_str())
        .collect();
    assert_eq!(methods, ["initialize", "session/new", "session/prompt"]);
    assert!(entries.iter().any(|entry| {
        entry.direction == Direction::AgentLog
            && entry.message.as_str() == Some("fake agent got a prompt")
    }));
}

#[tokio::test]
async fn allowed_permissions_are_granted_and_rendered() {
    let entries = run_fake_agent(PermissionMode::Allow).await;
    let protocol_only: Vec<_> = entries
        .into_iter()
        .filter(|entry| entry.direction != Direction::AgentLog)
        .collect();
    assert_eq!(
        rendered(&protocol_only),
        "prompt do the thing\n\
         Working on it. \n\
         tool  Edit file (pending)\n\
         ask   permission for Edit file\n\
         permission: yes\n\
         end   end_turn\n"
    );
}

#[tokio::test]
async fn denied_permissions_are_rejected() {
    let entries = run_fake_agent(PermissionMode::Deny).await;
    assert!(rendered(&entries).contains("permission: no"));
}
