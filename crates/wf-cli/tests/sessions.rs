//! Runs sessions end to end: a real daemon in this process, a real git repository, the real `wf`
//! binary for the commands, and a scripted agent in place of a harness.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::time::Duration;

use serde_json::Value;
use wf_core::name::HarnessName;
use wf_daemon::{Config, Daemon};
use wf_harness::AcpAgentConfig;
use wf_net::Network;

mod common;

use common::{Sandbox, short};

fn fake_agent() -> PathBuf {
    let test = std::env::current_exe().unwrap();
    let examples = test.parent().unwrap().parent().unwrap().join("examples");
    let path = examples.join("fake-acp-agent");
    assert!(path.is_file(), "{} is not built", path.display());
    path
}

fn git(directory: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .args(args)
        .current_dir(directory)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap().trim().to_string()
}

/// A workforce in a git repository, a daemon, and an agent `coder` that follows `script`.
struct World {
    wf: Sandbox,
    daemon: Arc<Daemon>,
    shutdown: Option<tokio::sync::oneshot::Sender<()>>,
}

impl World {
    async fn new(script: &str) -> Self {
        let world = Self::without_daemon(script);
        world.start_daemon().await
    }

    fn without_daemon(script: &str) -> WorldSetup {
        let wf = Sandbox::new();
        wf.run(&["init", "--name", "demo", "--no-agents"]);
        git(
            &wf.repository,
            &["init", "--quiet", "--initial-branch=main"],
        );
        git(&wf.repository, &["config", "user.name", "Test"]);
        git(
            &wf.repository,
            &["config", "user.email", "test@example.com"],
        );
        git(&wf.repository, &["add", "--all"]);
        git(&wf.repository, &["commit", "--quiet", "-m", "start"]);
        let script_path = wf.root.path().join("script.txt");
        std::fs::write(&script_path, script).unwrap();
        wf.run(&["agent", "add", "coder", "--harness", "fake"]);
        WorldSetup { wf, script_path }
    }

    fn wf(&self, args: &[&str]) -> Value {
        self.wf.json(args)
    }

    fn item(&self, id: &str) -> Value {
        self.wf(&["item", "show", short(id)])
    }

    fn sessions(&self) -> Vec<Value> {
        self.wf(&["session", "list"]).as_array().unwrap().clone()
    }

    /// Waits until `condition` holds.
    async fn wait(&self, what: &str, condition: impl Fn(&World) -> bool) {
        for _ in 0..400 {
            if condition(self) {
                return;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        panic!("timed out waiting for {what}");
    }

    fn questions(&self) -> Vec<Value> {
        self.wf(&["item", "list", "--type", "question"])
            .as_array()
            .unwrap()
            .clone()
    }

    /// Waits until the only session is not live any more and returns it.
    async fn ended_session(&self) -> Value {
        for _ in 0..400 {
            let sessions = self.sessions();
            if let Some(session) = sessions.first()
                && session["status"] != "live"
            {
                // The item moves after the end, so give that a moment.
                tokio::time::sleep(Duration::from_millis(300)).await;
                return self.sessions()[0].clone();
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        panic!("the session did not end: {:?}", self.sessions());
    }
}

struct WorldSetup {
    wf: Sandbox,
    script_path: PathBuf,
}

impl WorldSetup {
    async fn start_daemon(self) -> World {
        let data = wf_cli::keys::DataDir::new(self.wf.data.join("workforce"));
        let device = data.device(false).unwrap().unwrap();
        let mut harnesses = BTreeMap::new();
        harnesses.insert(
            HarnessName::new("fake").unwrap(),
            AcpAgentConfig::new(fake_agent())
                .env("FAKE_AGENT_SCRIPT", self.script_path.to_string_lossy())
                .env("WF_BIN", env!("CARGO_BIN_EXE_wf"))
                .env("XDG_DATA_HOME", self.wf.data.to_string_lossy())
                .env(
                    "XDG_RUNTIME_DIR",
                    self.wf.root.path().join("run").to_string_lossy(),
                )
                .env("USER", "joshua"),
        );
        let socket = self.wf.root.path().join("run/workforce/daemon.sock");
        let daemon = Daemon::start(
            Config {
                data_dir: data.root().to_path_buf(),
                socket: socket.clone(),
                network: Network::Direct,
                harnesses,
            },
            device,
        )
        .await
        .unwrap();
        let (shutdown, signal) = tokio::sync::oneshot::channel::<()>();
        tokio::spawn(Arc::clone(&daemon).serve(async {
            let _ = signal.await;
        }));
        for _ in 0..200 {
            if std::os::unix::net::UnixStream::connect(&socket).is_ok() {
                return World {
                    wf: self.wf,
                    daemon,
                    shutdown: Some(shutdown),
                };
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        panic!("the daemon did not start");
    }
}

impl Drop for World {
    fn drop(&mut self) {
        if let Some(shutdown) = self.shutdown.take() {
            let _ = shutdown.send(());
        }
        let _ = &self.daemon;
    }
}

fn start_session(world: &World, item: &str) -> String {
    let started = world.wf(&["session", "start", short(item), "--agent", "coder"]);
    started["event"].as_str().unwrap().to_string()
}

#[tokio::test(flavor = "multi_thread")]
async fn submitted_work_without_review_is_merged_and_the_item_is_done() {
    let script = "\
say hello
sh echo hi > hello.txt && git add hello.txt && git commit -q -m \"add hello\"
wf work report \"wrote the file\" --step 1 --done
wf work submit --note \"all done\"
";
    let world = World::new(script).await;
    let item = world
        .wf
        .add(&["--title", "Add hello", "--body", "- [ ] write the file"]);
    start_session(&world, &item);
    let session = world.ended_session().await;
    assert_eq!(session["status"], "submitted", "{session}");
    assert!(session["commit"].is_string());

    let shown = world.item(&item);
    assert_eq!(shown["state"], "done", "{shown}");
    let notes = shown["notes"].as_array().unwrap();
    assert!(
        notes
            .iter()
            .any(|note| note["kind"] == "report" && note["text"] == "wrote the file")
    );
    assert!(
        notes
            .iter()
            .any(|note| note["kind"] == "transition" && note["text"] == "all done")
    );
    let steps: Vec<bool> = shown["body"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|part| part["kind"] == "step")
        .map(|part| part["done"].as_bool().unwrap())
        .collect();
    assert_eq!(steps, [true]);

    let transcript = world
        .wf
        .run(&["session", "watch", short(&item), "--no-follow"]);
    assert!(
        transcript.contains("prompt You are working on item"),
        "{transcript}"
    );
    assert!(transcript.contains("hello"), "{transcript}");

    let repository = &world.wf.repository;
    assert_eq!(
        std::fs::read_to_string(repository.join("hello.txt")).unwrap(),
        "hi\n"
    );
    let subject = git(repository, &["log", "-1", "--format=%s", "main"]);
    assert!(subject.starts_with("Merge #"), "{subject}");
    assert_eq!(git(repository, &["status", "--porcelain"]), "");
    assert!(
        git(repository, &["branch", "--list", "wf/*"]).is_empty(),
        "the branch of a merged item is removed"
    );
    assert_eq!(git(repository, &["worktree", "list"]).lines().count(), 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn releasing_reopens_the_item_and_keeps_the_branch() {
    let script = "\
sh echo wip > wip.txt && git add wip.txt && git commit -q -m wip
wf work release --note \"I cannot do this\"
";
    let world = World::new(script).await;
    let item = world.wf.add(&["--title", "Hard one"]);
    start_session(&world, &item);
    let session = world.ended_session().await;
    assert_eq!(session["status"], "released");

    let shown = world.item(&item);
    assert_eq!(shown["state"], "open");
    assert!(
        shown["notes"]
            .as_array()
            .unwrap()
            .iter()
            .any(|note| note["kind"] == "comment" && note["text"] == "I cannot do this")
    );
    assert!(!git(&world.wf.repository, &["branch", "--list", "wf/*"]).is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn an_agent_that_never_commits_or_submits_is_released_after_its_nudges() {
    let world = World::new("").await;
    let item = world.wf.add(&["--title", "Idle"]);
    start_session(&world, &item);
    let session = world.ended_session().await;
    assert_eq!(session["status"], "released");
    let shown = world.item(&item);
    assert_eq!(shown["state"], "open");
    let gave_up = shown["notes"].as_array().unwrap().iter().any(|note| {
        note["text"]
            .as_str()
            .is_some_and(|text| text.starts_with("Gave up"))
    });
    assert!(gave_up, "{shown}");
}

#[tokio::test(flavor = "multi_thread")]
async fn stopping_a_session_cancels_the_turn_and_reopens_the_item() {
    let world = World::new("hang\n").await;
    let item = world.wf.add(&["--title", "Stuck"]);
    start_session(&world, &item);
    assert_eq!(world.sessions()[0]["status"], "live");
    assert_eq!(world.item(&item)["state"], "in_progress");

    world.wf.run(&[
        "session",
        "stop",
        short(world.sessions()[0]["id"].as_str().unwrap()),
    ]);
    let session = world.ended_session().await;
    assert_eq!(session["status"], "cancelled");
    assert_eq!(world.item(&item)["state"], "open");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_harness_that_crashes_fails_the_session() {
    let world = World::new("crash\n").await;
    let item = world.wf.add(&["--title", "Crashy"]);
    start_session(&world, &item);
    let session = world.ended_session().await;
    assert_eq!(session["status"], "failed");
    assert_eq!(world.item(&item)["state"], "open");
}

#[tokio::test(flavor = "multi_thread")]
async fn work_waits_while_the_target_is_checked_out_with_changes() {
    let script = "\
sh echo hi > hello.txt && git add hello.txt && git commit -q -m hello
wf work submit
";
    let world = World::new(script).await;
    let item = world.wf.add(&["--title", "Add hello"]);
    let file = world.wf.repository.join(".workforce.toml");
    let original = std::fs::read_to_string(&file).unwrap();
    std::fs::write(&file, format!("{original}# edited by a person\n")).unwrap();
    start_session(&world, &item);
    let session = world.ended_session().await;
    assert_eq!(session["status"], "submitted");
    let shown = world.item(&item);
    assert_eq!(shown["state"], "in_review", "{shown}");
    assert!(shown["notes"].as_array().unwrap().iter().any(|note| {
        note["text"]
            .as_str()
            .is_some_and(|text| text.starts_with("Waiting to merge"))
    }));
    assert!(
        std::fs::read_to_string(&file)
            .unwrap()
            .ends_with("# edited by a person\n")
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_session_left_live_by_an_earlier_daemon_is_failed_when_the_next_one_starts() {
    let setup = World::without_daemon("");
    let item = setup.wf.add(&["--title", "Orphaned"]);
    {
        let data = wf_cli::keys::DataDir::new(setup.wf.data.join("workforce"));
        let mut workspace =
            wf_cli::workspace::Workspace::open(data, &setup.wf.repository, false).unwrap();
        let id = workspace.resolve_item(short(&item)).unwrap();
        workspace
            .author(wf_core::event::EventBody::SessionStarted(
                wf_core::event::SessionStarted {
                    agent: wf_core::name::AgentName::new("coder").unwrap(),
                    item: id,
                    role: wf_core::session::SessionRole::Work,
                },
            ))
            .unwrap();
        workspace
            .author(wf_core::event::EventBody::ItemTransitioned(
                wf_core::event::ItemTransitioned {
                    item: id,
                    to: wf_core::name::StateName::new("in_progress").unwrap(),
                    note: None,
                },
            ))
            .unwrap();
    }
    let world = setup.start_daemon().await;
    let session = world.ended_session().await;
    assert_eq!(session["status"], "failed");
    assert_eq!(world.item(&item)["state"], "open");
}

#[tokio::test(flavor = "multi_thread")]
async fn sessions_need_a_known_agent_and_harness() {
    let world = World::new("").await;
    let item = world.wf.add(&["--title", "Anything"]);
    let message = world
        .wf
        .fail(&["session", "start", short(&item), "--agent", "ghost"]);
    assert!(message.contains("not an agent"), "{message}");
    world
        .wf
        .run(&["agent", "add", "odd", "--harness", "unheard-of"]);
    let message = world
        .wf
        .fail(&["session", "start", short(&item), "--agent", "odd"]);
    assert!(
        message.contains("not one wf knows how to start"),
        "{message}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_question_parks_the_session_until_it_is_answered() {
    let script = "\
wf work ask \"which crate?\" --body \"for durations\"
turn
sh echo done > a.txt && git add a.txt && git commit -q -m a
wf work submit
";
    let world = World::new(script).await;
    let item = world.wf.add(&["--title", "Parse durations"]);
    start_session(&world, &item);
    world
        .wait("the question", |world| !world.questions().is_empty())
        .await;
    let question = world.questions()[0]["id"].as_str().unwrap().to_string();
    assert_eq!(world.item(&question)["state"], "open");
    let waiting = world.item(&item);
    assert_eq!(waiting["state"], "in_progress");
    assert_eq!(waiting["depends_on"].as_array().unwrap().len(), 1);

    tokio::time::sleep(Duration::from_millis(1_000)).await;
    assert_eq!(world.sessions()[0]["status"], "live", "parked, not ended");

    world.wf.run(&[
        "item",
        "move",
        short(&question),
        "answered",
        "--note",
        "Use humantime",
    ]);
    let session = world.ended_session().await;
    assert_eq!(world.sessions().len(), 1, "the same session continued");
    assert_eq!(session["status"], "submitted", "{session}");
    assert_eq!(world.item(&item)["state"], "done");
    let transcript = world
        .wf
        .run(&["session", "watch", short(&item), "--no-follow"]);
    assert!(transcript.contains("Use humantime"), "{transcript}");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_session_that_waits_too_long_for_an_answer_times_out() {
    let script = "wf work ask \"anyone there?\"\n";
    let world = World::new(script).await;
    world
        .wf
        .run(&["agent", "edit", "coder", "--park-timeout", "1s"]);
    let item = world.wf.add(&["--title", "Waits"]);
    start_session(&world, &item);
    let session = world.ended_session().await;
    assert_eq!(session["status"], "timed_out", "{session}");
    assert_eq!(world.item(&item)["state"], "open");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_session_that_runs_too_long_is_cancelled_and_times_out() {
    let world = World::new("hang\n").await;
    world
        .wf
        .run(&["agent", "edit", "coder", "--max-duration", "1s"]);
    let item = world.wf.add(&["--title", "Slow"]);
    start_session(&world, &item);
    let session = world.ended_session().await;
    assert_eq!(session["status"], "timed_out", "{session}");
    assert_eq!(world.item(&item)["state"], "open");
}

#[tokio::test(flavor = "multi_thread")]
async fn submitting_without_commits_answers_a_question_and_leaves_no_branch() {
    let world = World::new("wf work submit --note \"Use humantime\"\n").await;
    let question = world
        .wf
        .add(&["--type", "question", "--title", "Which crate?"]);
    start_session(&world, &question);
    let session = world.ended_session().await;
    assert_eq!(session["status"], "submitted");
    assert_eq!(world.item(&question)["state"], "answered");
    assert!(git(&world.wf.repository, &["branch", "--list", "wf/*"]).is_empty());
}
