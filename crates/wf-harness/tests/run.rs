use std::io;
use std::sync::{Arc, Mutex};

use wf_harness::{
    Conversation, Direction, Launch, Model, PermissionMode, StopReason, TranscriptEntry,
};

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
    let mut conversation = Conversation::start(Launch {
        agent: agent_client_protocol::AcpAgentConfig::new(env!("CARGO_BIN_EXE_fake-acp-agent")),
        model: None,
        directory: directory.path().to_path_buf(),
        permissions: Arc::new(permissions),
        transcript: buffer.clone(),
    })
    .await
    .unwrap();
    let stop_reason = conversation.prompt("do the thing").await.unwrap();
    assert!(matches!(stop_reason, StopReason::EndTurn));
    conversation.close().await.unwrap();

    let bytes = buffer.0.lock().unwrap().clone();
    String::from_utf8(bytes)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect()
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
async fn allowed_permissions_are_granted() {
    let entries = run_fake_agent(PermissionMode::Allow).await;
    let protocol_only: Vec<_> = entries
        .into_iter()
        .filter(|entry| entry.direction != Direction::AgentLog)
        .collect();
    assert!(protocol_only.iter().any(|entry| {
        entry.message["params"]["update"]["content"]["text"]
            .as_str()
            .is_some_and(|text| text.contains("permission: yes"))
    }));
}

#[tokio::test]
async fn denied_permissions_are_rejected() {
    let entries = run_fake_agent(PermissionMode::Deny).await;
    assert!(entries.iter().any(|entry| {
        entry.message["params"]["update"]["content"]["text"]
            .as_str()
            .is_some_and(|text| text.contains("permission: no"))
    }));
}

#[tokio::test]
async fn one_session_takes_several_prompts() {
    let directory = tempfile::tempdir().unwrap();
    let buffer = SharedBuffer::default();
    let mut conversation = Conversation::start(Launch {
        agent: agent_client_protocol::AcpAgentConfig::new(env!("CARGO_BIN_EXE_fake-acp-agent")),
        model: None,
        directory: directory.path().to_path_buf(),
        permissions: Arc::new(PermissionMode::Allow),
        transcript: buffer.clone(),
    })
    .await
    .unwrap();
    for prompt in ["first", "second"] {
        let stop_reason = conversation.prompt(prompt).await.unwrap();
        assert!(matches!(stop_reason, StopReason::EndTurn));
    }
    conversation.close().await.unwrap();

    let text = String::from_utf8(buffer.0.lock().unwrap().clone()).unwrap();
    let entries: Vec<TranscriptEntry> = text
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    let methods: Vec<_> = entries
        .iter()
        .filter(|entry| entry.direction == Direction::ToAgent)
        .filter_map(|entry| entry.message["method"].as_str())
        .collect();
    assert_eq!(
        methods,
        [
            "initialize",
            "session/new",
            "session/prompt",
            "session/prompt"
        ]
    );
}

#[tokio::test]
async fn a_reply_holds_what_the_agent_said_in_its_turn() {
    let directory = tempfile::tempdir().unwrap();
    let mut conversation = Conversation::start(Launch {
        agent: agent_client_protocol::AcpAgentConfig::new(env!("CARGO_BIN_EXE_fake-acp-agent")),
        model: None,
        directory: directory.path().to_path_buf(),
        permissions: Arc::new(PermissionMode::Deny),
        transcript: SharedBuffer::default(),
    })
    .await
    .unwrap();
    let (stop_reason, said) = conversation.reply("do the thing").await.unwrap();
    assert!(matches!(stop_reason, StopReason::EndTurn));
    assert!(said.contains("permission: no"), "{said}");
    conversation.close().await.unwrap();
}

#[tokio::test]
async fn an_agent_that_cannot_start_is_an_error() {
    let result = Conversation::start(Launch {
        agent: agent_client_protocol::AcpAgentConfig::new("/nonexistent/agent"),
        model: None,
        directory: std::env::temp_dir(),
        permissions: Arc::new(PermissionMode::Allow),
        transcript: SharedBuffer::default(),
    })
    .await;
    assert!(result.is_err());
}

#[tokio::test]
async fn the_models_an_agent_offers_are_read_from_a_new_session() {
    let directory = tempfile::tempdir().unwrap();
    let models = wf_harness::models(
        agent_client_protocol::AcpAgentConfig::new(env!("CARGO_BIN_EXE_fake-acp-agent")),
        directory.path().to_path_buf(),
    )
    .await
    .unwrap();
    let model = |value: &str, name: &str, is_default| Model {
        value: value.into(),
        name: name.into(),
        is_default,
    };
    assert_eq!(
        models,
        [model("fast", "Fast", true), model("deep", "Deep", false)]
    );
}
