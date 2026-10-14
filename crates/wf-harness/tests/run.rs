use std::io;
use std::sync::{Arc, Mutex};

use wf_harness::{
    AgentSessionId, Conversation, Direction, Error, Launch, Model, Opening, Permission, StopReason,
    TranscriptEntry,
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

async fn run_fake_agent(permissions: Permission) -> Vec<TranscriptEntry> {
    let directory = tempfile::tempdir().unwrap();
    let buffer = SharedBuffer::default();
    let mut conversation = Conversation::start(Launch {
        agent: agent_client_protocol::AcpAgentConfig::new(env!("CARGO_BIN_EXE_fake-acp-agent")),
        opening: Opening::New,
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
    let entries = run_fake_agent(Permission::Allow).await;
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
    let entries = run_fake_agent(Permission::Allow).await;
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
    let entries = run_fake_agent(Permission::Deny).await;
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
        opening: Opening::New,
        model: None,
        directory: directory.path().to_path_buf(),
        permissions: Arc::new(Permission::Allow),
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
        opening: Opening::New,
        model: None,
        directory: directory.path().to_path_buf(),
        permissions: Arc::new(Permission::Deny),
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
        opening: Opening::New,
        model: None,
        directory: std::env::temp_dir(),
        permissions: Arc::new(Permission::Allow),
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

async fn model_of_a_new_session(model: Option<&str>) -> Option<String> {
    let directory = tempfile::tempdir().unwrap();
    let conversation = Conversation::start(Launch {
        agent: agent_client_protocol::AcpAgentConfig::new(env!("CARGO_BIN_EXE_fake-acp-agent")),
        opening: Opening::New,
        model: model.map(str::to_string),
        directory: directory.path().to_path_buf(),
        permissions: Arc::new(Permission::Allow),
        transcript: SharedBuffer::default(),
    })
    .await
    .unwrap();
    let current = conversation.model().map(str::to_string);
    conversation.close().await.unwrap();
    current
}

#[tokio::test]
async fn a_conversation_knows_the_model_its_session_runs_with() {
    assert_eq!(model_of_a_new_session(None).await.as_deref(), Some("fast"));
    assert_eq!(
        model_of_a_new_session(Some("Deep")).await.as_deref(),
        Some("deep")
    );
}

/// Reopens the session `fake` with an agent that offers `reopens`, prompts it once and returns
/// the transcript, or why it could not be opened.
async fn reopen(reopens: &str) -> Result<(String, Vec<TranscriptEntry>), Error> {
    let directory = tempfile::tempdir().unwrap();
    let buffer = SharedBuffer::default();
    let mut conversation = Conversation::start(Launch {
        agent: agent_client_protocol::AcpAgentConfig::new(env!("CARGO_BIN_EXE_fake-acp-agent"))
            .env("FAKE_AGENT_REOPENS", reopens),
        opening: Opening::Resume(AgentSessionId("fake".to_string())),
        model: None,
        directory: directory.path().to_path_buf(),
        permissions: Arc::new(Permission::Allow),
        transcript: buffer.clone(),
    })
    .await?;
    assert_eq!(conversation.session(), &AgentSessionId("fake".to_string()));
    let (_, said) = conversation.reply("go on").await.unwrap();
    conversation.close().await.unwrap();
    let bytes = buffer.0.lock().unwrap().clone();
    let entries = String::from_utf8(bytes)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    Ok((said, entries))
}

fn sent_methods(entries: &[TranscriptEntry]) -> Vec<&str> {
    entries
        .iter()
        .filter(|entry| entry.direction == Direction::ToAgent)
        .filter_map(|entry| entry.message["method"].as_str())
        .collect()
}

#[tokio::test]
async fn a_session_is_resumed_when_the_agent_can() {
    let (_, entries) = reopen("resume").await.unwrap();
    assert_eq!(
        sent_methods(&entries),
        ["initialize", "session/resume", "session/prompt"]
    );
}

#[tokio::test]
async fn a_loaded_session_does_not_record_or_hear_its_history_again() {
    let (said, entries) = reopen("load").await.unwrap();
    assert_eq!(
        sent_methods(&entries),
        ["initialize", "session/load", "session/prompt"]
    );
    assert!(!said.contains("said before"), "{said}");
    assert!(
        !entries
            .iter()
            .any(|entry| entry.message.to_string().contains("said before")),
        "the replay is not recorded"
    );
}

#[tokio::test]
async fn an_agent_that_cannot_reopen_sessions_says_so() {
    assert!(matches!(reopen("").await, Err(Error::CannotResume)));
}

/// While other threads run short commands with piped output, which on macOS create pipes that
/// are not close-on-exec for a moment, every agent started holds only stdin, stdout and stderr.
#[tokio::test(flavor = "multi_thread")]
async fn an_agent_inherits_no_descriptor_while_other_threads_create_pipes() {
    let directory = tempfile::tempdir().unwrap();
    let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let commands: Vec<_> = (0..4)
        .map(|_| {
            let stop = Arc::clone(&stop);
            std::thread::spawn(move || {
                while !stop.load(std::sync::atomic::Ordering::Relaxed) {
                    std::process::Command::new("true").output().unwrap();
                }
            })
        })
        .collect();
    let mut agents = tokio::task::JoinSet::new();
    for number in 0..40 {
        let held = directory.path().join(format!("agent-{number}"));
        let agent =
            agent_client_protocol::AcpAgentConfig::new(env!("CARGO_BIN_EXE_fake-acp-agent"))
                .env("FAKE_AGENT_DESCRIPTORS", held.to_string_lossy());
        let directory = directory.path().to_path_buf();
        agents.spawn(async move {
            wf_harness::models(agent, directory).await.unwrap();
            std::fs::read_to_string(held).unwrap()
        });
    }
    let held = agents.join_all().await;
    stop.store(true, std::sync::atomic::Ordering::Relaxed);
    for command in commands {
        command.join().unwrap();
    }
    for descriptors in held {
        assert_eq!(descriptors, "0\n1\n2");
    }
}
