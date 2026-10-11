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
        "git {args:?}: {}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap().trim().to_string()
}

/// A workforce in a git repository, a daemon, and agents that follow their scripts.
struct World {
    wf: Sandbox,
    daemon: Arc<Daemon>,
    shutdown: Option<tokio::sync::oneshot::Sender<()>>,
}

impl World {
    /// One agent `coder` that follows `script`.
    async fn new(script: &str) -> Self {
        Self::agents(&[("coder", script)]).await
    }

    async fn agents(agents: &[(&str, &str)]) -> Self {
        Self::without_daemon(agents).start_daemon().await
    }

    fn without_daemon(agents: &[(&str, &str)]) -> WorldSetup {
        let wf = Sandbox::new();
        wf.run(&["init", "--name", "demo", "--no-agents"]);
        let policy = wf.run(&["policy", "show"]);
        let path = wf.root.path().join("unreviewed-policy.toml");
        std::fs::write(&path, policy.replace("review = \"principal\"\n", "")).unwrap();
        wf.run(&["policy", "apply", path.to_str().unwrap()]);
        git(
            &wf.repository,
            &["init", "--quiet", "--initial-branch=main"],
        );
        git(&wf.repository, &["config", "user.name", "Test"]);
        git(
            &wf.repository,
            &["config", "user.email", "test@example.com"],
        );
        std::fs::write(wf.repository.join("file.txt"), "one\ntwo\nthree\n").unwrap();
        git(&wf.repository, &["add", "--all"]);
        git(&wf.repository, &["commit", "--quiet", "-m", "start"]);
        let mut scripts = BTreeMap::new();
        for (name, script) in agents {
            let path = wf.root.path().join(format!("script-{name}.txt"));
            std::fs::write(&path, script).unwrap();
            let harness = format!("fake-{name}");
            wf.run(&["agent", "add", name, "--harness", &harness]);
            scripts.insert(harness, path);
        }
        WorldSetup { wf, scripts }
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

    /// Lets the dispatcher start `agent`, which works items of `lane`, one session at a time.
    fn automate(&self, agent: &str, lane: &str) {
        self.wf
            .run(&["agent", "edit", agent, "--start", "auto", "--lane", lane]);
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
                && session["status"] != "parked"
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
    /// The script of each harness.
    scripts: BTreeMap<String, PathBuf>,
}

impl WorldSetup {
    async fn start_daemon(self) -> World {
        let data = wf_cli::keys::DataDir::new(self.wf.data.join("workforce"));
        let device = data.device(false).unwrap().unwrap();
        let mut harnesses = BTreeMap::new();
        for (harness, script) in &self.scripts {
            harnesses.insert(
                HarnessName::new(harness).unwrap(),
                AcpAgentConfig::new(fake_agent())
                    .env("FAKE_AGENT_SCRIPT", script.to_string_lossy())
                    .env("WF_BIN", env!("CARGO_BIN_EXE_wf"))
                    .env("XDG_DATA_HOME", self.wf.data.to_string_lossy())
                    .env(
                        "XDG_RUNTIME_DIR",
                        self.wf.root.path().join("run").to_string_lossy(),
                    )
                    .env("USER", "joshua"),
            );
        }
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
async fn manual_start_uses_the_policy_agent_unless_overridden() {
    let world = World::agents(&[("coder", "say default\n"), ("other", "say override\n")]).await;
    let item = world.wf.add(&["--title", "Default agent"]);
    let error = world.wf.fail(&["session", "start", short(&item)]);
    assert!(error.contains("default_agent"), "{error}");
    assert!(world.sessions().is_empty());

    let policy = world.wf.run(&["policy", "show"]);
    let path = world.wf.root.path().join("policy.toml");
    std::fs::write(&path, format!("default_agent = \"coder\"\n{policy}")).unwrap();
    world.wf.run(&["policy", "apply", path.to_str().unwrap()]);
    let started = world.wf(&["session", "start", short(&item)]);
    let session = world.wf(&["session", "show", started["event"].as_str().unwrap()]);
    assert!(
        session["agent"].as_str().unwrap().contains("coder"),
        "{session}"
    );

    let other_item = world.wf.add(&["--title", "Override agent"]);
    let started = world.wf(&["session", "start", short(&other_item), "-a", "other"]);
    let session = world.wf(&["session", "show", started["event"].as_str().unwrap()]);
    assert!(
        session["agent"].as_str().unwrap().contains("other"),
        "{session}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn interrupting_start_watch_detaches_without_stopping_the_agent() {
    use std::process::Stdio;

    let world = World::new(
        "say watching\nsh while [ ! -f \"$XDG_DATA_HOME/go\" ]; do sleep 0.1; done\nwf work release --note finished\n",
    ).await;
    let item = world.wf.add(&["--title", "Keep running after detach"]);
    let error = world.wf.fail(&[
        "session",
        "start",
        short(&item),
        "-a",
        "coder",
        "--watch",
        "--raw",
        "--json",
    ]);
    assert!(error.contains("--json"), "{error}");
    assert!(
        world.sessions().is_empty(),
        "JSON rejection must not start a session"
    );

    let output_path = world.wf.root.path().join("watch.txt");
    let mut watcher = world
        .wf
        .command(&[
            "session",
            "start",
            short(&item),
            "-a",
            "coder",
            "--watch",
            "--raw",
        ])
        .stdout(Stdio::from(std::fs::File::create(&output_path).unwrap()))
        .spawn()
        .unwrap();
    world
        .wait("watch output", |_| {
            std::fs::read_to_string(&output_path)
                .unwrap()
                .contains("watching")
        })
        .await;
    assert!(
        Command::new("/bin/kill")
            .args(["-INT", &watcher.id().to_string()])
            .status()
            .unwrap()
            .success()
    );
    watcher.wait().unwrap();
    assert_eq!(world.sessions()[0]["status"], "live");
    std::fs::write(world.wf.data.join("go"), "").unwrap();
    let ended = world.ended_session().await;
    assert_eq!(ended["status"], "released", "{ended}");
    let mut completed_watch = world
        .wf
        .command(&["session", "watch", short(&item)])
        .stdout(Stdio::from(std::fs::File::create(&output_path).unwrap()))
        .spawn()
        .unwrap();
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    loop {
        if let Some(status) = completed_watch.try_wait().unwrap() {
            assert!(status.success());
            break;
        }
        if std::time::Instant::now() >= deadline {
            completed_watch.kill().unwrap();
            completed_watch.wait().unwrap();
            panic!("watch must finish after draining an ended session");
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let transcript = std::fs::read_to_string(&output_path).unwrap();
    assert!(
        transcript.contains("finished"),
        "the detached agent must continue: {transcript}"
    );
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
    let moves: Vec<String> = world.wf(&["log"])["events"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|event| event["kind"] == "ItemMoved")
        .map(|event| event["summary"].as_str().unwrap().to_string())
        .collect();
    let for_session = format!("for session {}", short(session["id"].as_str().unwrap()));
    assert_eq!(moves.len(), 2, "{moves:?}");
    assert!(
        moves.iter().all(|summary| summary.ends_with(&for_session)),
        "the start and the end move the item for the session: {moves:?}"
    );

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
            .any(|note| note["kind"] == "comment" && note["text"] == "all done")
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
        transcript.contains("You are working on item"),
        "{transcript}"
    );
    assert!(transcript.contains("hello"), "{transcript}");
    assert!(
        transcript.contains("end_turn") && !transcript.contains("cancelled"),
        "an agent that submits finishes its turn: {transcript}"
    );

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

    git(
        &world.wf.repository,
        &["checkout", "--quiet", "--", ".workforce.toml"],
    );
    world
        .wf
        .run(&["comment", short(&item), "the checkout is clean again"]);
    world
        .wait("the waiting merge", |world| {
            world.item(&item)["state"] == "done"
        })
        .await;
    assert!(world.wf.repository.join("hello.txt").is_file());
    let waits = world.item(&item)["notes"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|note| {
            note["text"]
                .as_str()
                .is_some_and(|text| text.starts_with("Waiting to merge"))
        })
        .count();
    assert_eq!(waits, 1, "waiting again does not say so again");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_session_left_live_by_an_earlier_daemon_is_failed_when_the_next_one_starts() {
    let setup = World::without_daemon(&[("coder", "")]);
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
            .author(wf_core::event::EventBody::ItemMoved(
                wf_core::event::ItemMoved {
                    item: id,
                    to: wf_core::name::StateName::new("in_progress").unwrap(),
                    by_session: None,
                    because: None,
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
    assert_eq!(world.sessions()[0]["status"], "parked");

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

const COMMIT_AND_SUBMIT: &str = "\
sh echo hi > hello.txt && git add hello.txt && git commit -q -m hello
wf work submit
";

#[tokio::test(flavor = "multi_thread")]
async fn an_auto_agent_takes_the_ready_items_one_after_the_other() {
    let script = "\
sh f=$(date +%N).txt && echo hi > $f && git add $f && git commit -q -m work
wf work submit
";
    let world = World::new(script).await;
    world.automate("coder", "default");
    world.wf.run(&["device", "edit", "--agent", "coder=1"]);
    let first = world.wf.add(&["--title", "First"]);
    let second = world.wf.add(&["--title", "Second"]);
    world
        .wait("both items done", |world| {
            world.item(&first)["state"] == "done" && world.item(&second)["state"] == "done"
        })
        .await;
    let sessions = world.sessions();
    assert_eq!(sessions.len(), 2);
    assert!(
        sessions
            .iter()
            .all(|session| session["status"] == "submitted")
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn nothing_is_started_for_agents_the_device_does_not_host() {
    let world = World::new(COMMIT_AND_SUBMIT).await;
    world.automate("coder", "default");
    let item = world.wf.add(&["--title", "Unhosted"]);
    tokio::time::sleep(Duration::from_millis(1_000)).await;
    assert!(world.sessions().is_empty());
    assert_eq!(world.item(&item)["state"], "open");

    world.wf.run(&["device", "edit", "--agent", "coder=1"]);
    world
        .wait("the item done", |world| {
            world.item(&item)["state"] == "done"
        })
        .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_supervisor_answers_the_question_of_a_coder_and_the_coder_goes_on() {
    let coder = "\
wf work ask \"which crate?\"
turn
sh echo done > a.txt && git add a.txt && git commit -q -m a
wf work submit
";
    let supervisor = "wf work submit --note \"Use humantime\"\n";
    let world = World::agents(&[("coder", coder), ("supervisor", supervisor)]).await;
    world.automate("coder", "default");
    world.automate("supervisor", "supervision");
    world.wf.run(&[
        "device",
        "edit",
        "--max-sessions",
        "2",
        "--agent",
        "coder=1",
        "--agent",
        "supervisor=1",
    ]);
    let item = world.wf.add(&["--title", "Parse durations"]);
    world
        .wait("the item done", |world| {
            world.item(&item)["state"] == "done"
        })
        .await;
    let question = world.questions()[0]["id"].as_str().unwrap().to_string();
    assert_eq!(world.item(&question)["state"], "answered");
    let sessions = world.sessions();
    assert_eq!(sessions.len(), 2, "{sessions:?}");
    assert!(
        sessions
            .iter()
            .all(|session| session["status"] == "submitted")
    );
    let transcript = world
        .wf
        .run(&["session", "watch", short(&item), "--no-follow"]);
    assert!(transcript.contains("Use humantime"), "{transcript}");
}

#[tokio::test(flavor = "multi_thread")]
async fn an_answer_under_review_counts_once_a_person_approves_it() {
    let coder = "\
wf work ask \"which crate?\"
turn
sh echo done > a.txt && git add a.txt && git commit -q -m a
wf work submit
";
    let supervisor = "wf work submit --note \"Use humantime\"\n";
    let world = World::agents(&[("coder", coder), ("supervisor", supervisor)]).await;
    let policy = world.wf.run(&["policy", "show"]);
    let path = world.wf.root.path().join("reviewed-questions.toml");
    std::fs::write(
        &path,
        policy.replace(
            "[types.question]\nreview = \"none\"",
            "[types.question]\nreview = \"principal\"",
        ),
    )
    .unwrap();
    world.wf.run(&["policy", "apply", path.to_str().unwrap()]);
    world.automate("coder", "default");
    world.automate("supervisor", "supervision");
    world.wf.run(&[
        "device",
        "edit",
        "--max-sessions",
        "2",
        "--agent",
        "coder=1",
        "--agent",
        "supervisor=1",
    ]);
    let item = world.wf.add(&["--title", "Parse durations"]);
    world
        .wait("the answer waiting for review", |world| {
            world
                .questions()
                .first()
                .is_some_and(|question| question["state"] == "needs_human")
                && world
                    .sessions()
                    .iter()
                    .any(|session| session["status"] == "submitted" && session["role"] == "work")
        })
        .await;
    let question = world.questions()[0]["id"].as_str().unwrap().to_string();
    assert_ne!(world.item(&item)["state"], "done");
    world.wf.run(&["review", short(&question), "--approve"]);
    world
        .wait("the item done", |world| {
            world.item(&item)["state"] == "done"
        })
        .await;
    assert_eq!(world.item(&question)["state"], "answered");
}

#[tokio::test(flavor = "multi_thread")]
async fn only_the_items_an_agent_would_be_given_are_listed_for_it() {
    let world = World::new("").await;
    world.automate("coder", "default");
    world.wf.add(&["--title", "Plain"]);
    world
        .wf
        .add(&["--title", "Needs a gpu", "--requires", "gpu"]);
    world.wf.run(&["device", "edit", "--hardware", "linux"]);
    let listed = world.wf(&["item", "list", "--ready", "--for", "coder"]);
    let titles: Vec<&str> = listed
        .as_array()
        .unwrap()
        .iter()
        .map(|item| item["title"].as_str().unwrap())
        .collect();
    assert_eq!(titles, ["Plain"]);
    let message = world
        .wf
        .fail(&["item", "list", "--ready", "--for", "ghost"]);
    assert!(message.contains("not an agent of yours"), "{message}");
}

/// A coder and a reviewer, both started by the dispatcher.
async fn review_world(coder: &str, reviewer: &str) -> World {
    let world = World::agents(&[("coder", coder), ("reviewer", reviewer)]).await;
    world.automate("coder", "default");
    world.wf.run(&[
        "agent",
        "edit",
        "reviewer",
        "--start",
        "auto",
        "--capability",
        "review",
        "--capability",
        "strong-model",
    ]);
    world.wf.run(&[
        "device",
        "edit",
        "--max-sessions",
        "2",
        "--agent",
        "coder=1",
        "--agent",
        "reviewer=1",
    ]);
    world
}

fn statuses(world: &World) -> Vec<String> {
    let mut sessions = world.sessions();
    sessions.reverse();
    sessions
        .iter()
        .map(|session| {
            format!(
                "{} {}",
                session["agent"]
                    .as_str()
                    .unwrap()
                    .rsplit('/')
                    .next()
                    .unwrap(),
                session["status"].as_str().unwrap()
            )
        })
        .collect()
}

#[tokio::test(flavor = "multi_thread")]
async fn work_approved_by_a_reviewer_agent_is_merged() {
    let world = review_world(
        COMMIT_AND_SUBMIT,
        "wf review --approve --note \"looks good\"\n",
    )
    .await;
    let item = world.wf.add(&["--title", "Add hello", "--review", "light"]);
    world
        .wait("the item done", |world| {
            world.item(&item)["state"] == "done"
        })
        .await;
    let shown = world.item(&item);
    assert_eq!(shown["review_status"]["status"], "merged", "{shown}");
    assert!(
        shown["notes"]
            .as_array()
            .unwrap()
            .iter()
            .any(|note| note["kind"] == "comment" && note["text"] == "looks good")
    );
    assert_eq!(statuses(&world), ["coder submitted", "reviewer reviewed"]);

    let repository = &world.wf.repository;
    assert!(repository.join("hello.txt").is_file());
    assert_eq!(git(repository, &["worktree", "list"]).lines().count(), 1);
    assert!(git(repository, &["branch", "--list", "wf/*"]).is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn requested_changes_lead_to_a_second_session_on_the_same_branch() {
    let coder = "\
sh if [ -f hello.txt ]; then echo test > test.txt && git add test.txt && git commit -q -m test; else echo hi > hello.txt && git add hello.txt && git commit -q -m hello; fi
wf work submit
";
    let reviewer = "\
sh if [ -f test.txt ]; then \"$WF_BIN\" review --approve; else \"$WF_BIN\" review --request-changes --note \"add a test\"; fi
";
    let world = review_world(coder, reviewer).await;
    let item = world
        .wf
        .add(&["--title", "Add hello", "--review", "standard"]);
    world
        .wait("the item done", |world| {
            world.item(&item)["state"] == "done"
        })
        .await;
    assert_eq!(
        statuses(&world),
        [
            "coder submitted",
            "reviewer reviewed",
            "coder submitted",
            "reviewer reviewed"
        ]
    );
    let repository = &world.wf.repository;
    assert!(repository.join("hello.txt").is_file() && repository.join("test.txt").is_file());

    let mut sessions = world.sessions();
    sessions.reverse();
    let second_work = sessions
        .iter()
        .filter(|session| session["role"] == "work")
        .nth(1)
        .unwrap();
    let transcript = world.wf.run(&[
        "session",
        "watch",
        short(second_work["id"].as_str().unwrap()),
        "--no-follow",
    ]);
    assert!(
        transcript.contains("add a test"),
        "the reviewer's note reaches the next session"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_critical_item_waits_for_a_person_after_the_agent_approves() {
    let world = review_world(COMMIT_AND_SUBMIT, "wf review --approve\n").await;
    let item = world
        .wf
        .add(&["--title", "Add hello", "--review", "critical"]);
    world
        .wait("the human step", |world| {
            world.item(&item)["review_status"]["step"] == "human"
        })
        .await;
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert_eq!(world.item(&item)["state"], "in_review");
    assert!(!world.wf.repository.join("hello.txt").exists());

    world
        .wf
        .run(&["review", short(&item), "--approve", "--note", "ship it"]);
    world
        .wait("the item done", |world| {
            world.item(&item)["state"] == "done"
        })
        .await;
    assert!(world.wf.repository.join("hello.txt").is_file());
}

#[tokio::test(flavor = "multi_thread")]
async fn an_escalated_review_goes_to_a_person() {
    let world = review_world(
        COMMIT_AND_SUBMIT,
        "wf review --escalate --note \"unsure\"\n",
    )
    .await;
    let item = world
        .wf
        .add(&["--title", "Add hello", "--review", "standard"]);
    world
        .wait("the human step", |world| {
            world.item(&item)["review_status"]["step"] == "human"
        })
        .await;
    world
        .wf
        .run(&["review", short(&item), "--request-changes", "--note", "no"]);
    assert_eq!(world.item(&item)["state"], "open");
    let message = world.wf.fail(&["review", short(&item), "--escalate"]);
    assert!(message.contains("only an agent can escalate"), "{message}");
}

#[tokio::test(flavor = "multi_thread")]
async fn work_that_conflicts_with_the_target_goes_back_to_open() {
    let script = "\
sh while [ ! -f \"$XDG_DATA_HOME/go\" ]; do sleep 0.1; done
sh echo branch > file.txt && git add file.txt && git commit -q -m branch
wf work submit
";
    let world = World::new(script).await;
    let item = world.wf.add(&["--title", "Change the file"]);
    start_session(&world, &item);
    let repository = &world.wf.repository;
    std::fs::write(repository.join("file.txt"), "main\n").unwrap();
    git(repository, &["commit", "--quiet", "-am", "main moves on"]);
    let main = git(repository, &["rev-parse", "main"]);
    std::fs::write(world.wf.data.join("go"), "").unwrap();

    let session = world.ended_session().await;
    assert_eq!(session["status"], "submitted");
    let shown = world.item(&item);
    assert_eq!(shown["state"], "open", "{shown}");
    assert!(shown["notes"].as_array().unwrap().iter().any(|note| {
        note["text"]
            .as_str()
            .is_some_and(|text| text.contains("conflicts in file.txt"))
    }));
    assert_eq!(git(repository, &["rev-parse", "main"]), main);
    assert_eq!(
        std::fs::read_to_string(repository.join("file.txt")).unwrap(),
        "main\n"
    );
}

fn inbox(world: &World) -> Vec<Value> {
    world.wf(&["inbox"]).as_array().unwrap().clone()
}

#[tokio::test(flavor = "multi_thread")]
async fn a_permission_request_waits_in_the_inbox_until_a_person_answers() {
    let script = "\
permission
sh echo hi > hello.txt && git add hello.txt && git commit -q -m hello
wf work submit
";
    let world = World::new(script).await;
    let item = world.wf.add(&["--title", "Needs permission"]);
    start_session(&world, &item);
    world
        .wait("the request", |world| {
            inbox(world)
                .iter()
                .any(|entry| entry["kind"] == "permission")
        })
        .await;
    let entry = inbox(&world)
        .into_iter()
        .find(|entry| entry["kind"] == "permission")
        .unwrap();
    assert!(
        entry["what"].as_str().unwrap().contains("Edit file"),
        "{entry}"
    );
    assert_eq!(world.sessions()[0]["status"], "live");

    let session = entry["session"].as_str().unwrap();
    let said = world.wf.run(&["session", "allow", short(session)]);
    assert!(said.contains("allowed: Edit file"), "{said}");
    let session = world.ended_session().await;
    assert_eq!(session["status"], "submitted");
    let transcript = world
        .wf
        .run(&["session", "watch", short(&item), "--no-follow"]);
    assert!(transcript.contains("permission: yes"), "{transcript}");
    assert!(inbox(&world).is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn a_denied_permission_is_told_no_and_stopping_a_session_drops_its_requests() {
    let world = World::new("permission\nsay done\nhang\n").await;
    let item = world.wf.add(&["--title", "Needs permission"]);
    start_session(&world, &item);
    world
        .wait("the request", |world| {
            inbox(world)
                .iter()
                .any(|entry| entry["kind"] == "permission")
        })
        .await;
    let id = world.sessions()[0]["id"].as_str().unwrap().to_string();
    world.wf.run(&["session", "deny", short(&id)]);
    let message = world.wf.fail(&["session", "deny", short(&id)]);
    assert!(message.contains("not waiting for an answer"), "{message}");
    world.wf.run(&["session", "stop", short(&id)]);
    world.ended_session().await;
    let transcript = world
        .wf
        .run(&["session", "watch", short(&item), "--no-follow"]);
    assert!(transcript.contains("permission: no"), "{transcript}");
}

#[tokio::test(flavor = "multi_thread")]
async fn the_inbox_lists_what_waits_on_a_person() {
    let world = World::new("").await;
    assert!(inbox(&world).is_empty());
    let question = world
        .wf
        .add(&["--type", "question", "--title", "Which database?"]);
    assert!(
        inbox(&world).is_empty(),
        "an open question waits on a supervisor"
    );
    world
        .wf
        .run(&["item", "move", short(&question), "needs_human"]);
    let entries = inbox(&world);
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0]["kind"], "blocked");
    assert_eq!(entries[0]["title"], "Which database?");
    let text = world.wf.run(&["inbox"]);
    assert!(
        text.contains("Which database?") && text.contains("needs_human"),
        "{text}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_session_belongs_to_its_workforce_whatever_its_committed_workforce_file_says() {
    let world = World::new(COMMIT_AND_SUBMIT).await;
    let repository = &world.wf.repository;
    let file = repository.join(".workforce.toml");
    let right = std::fs::read_to_string(&file).unwrap();
    // The file was committed when an older workforce was here; the working copy is the new one.
    std::fs::write(&file, format!("id = \"{}\"\n", "a".repeat(52))).unwrap();
    git(
        repository,
        &["commit", "--quiet", "-am", "an older workforce"],
    );
    std::fs::write(&file, right).unwrap();

    let item = world.wf.add(&["--title", "Add hello"]);
    start_session(&world, &item);
    let session = world.ended_session().await;
    assert_eq!(session["status"], "submitted", "{session}");
}

#[tokio::test(flavor = "multi_thread")]
async fn an_item_with_a_live_session_is_not_started_twice() {
    let world = World::new("hang\n").await;
    let item = world.wf.add(&["--title", "Busy"]);
    start_session(&world, &item);
    let message = world
        .wf
        .fail(&["session", "start", short(&item), "--agent", "coder"]);
    assert!(message.contains("already has a live session"), "{message}");
    assert_eq!(world.sessions().len(), 1);
}
