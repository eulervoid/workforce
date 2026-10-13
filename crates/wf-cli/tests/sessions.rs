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

use common::{Sandbox, comments, short};

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
    /// The script of each harness.
    scripts: BTreeMap<String, PathBuf>,
    /// How the fake agents offer to reopen a session: `resume`, `load` or not at all.
    reopens: &'static str,
}

/// The fake agents run without Fence, so what they are allowed to do is asked.
const SANDBOX_OFF: &str = "[sandbox]\nkind = \"off\"\n\n";

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
        wf.run(&["init", "--name", "demo"]);
        let policy = wf.run(&["policy", "show"]);
        let path = wf.root.path().join("unreviewed-policy.toml");
        std::fs::write(&path, policy.replace("review = \"manual\"\n", "")).unwrap();
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
        std::fs::write(wf.root.path().join("config.toml"), SANDBOX_OFF).unwrap();
        WorldSetup {
            wf,
            scripts,
            reopens: "resume",
        }
    }

    /// Stops the daemon as `wf daemon stop` does and starts another on the same data.
    async fn restart(&mut self) {
        if let Some(shutdown) = self.shutdown.take() {
            let _ = shutdown.send(());
        }
        self.daemon.halt().await;
        let (daemon, shutdown) = start_daemon(&self.wf, &self.scripts, self.reopens).await;
        self.daemon = daemon;
        self.shutdown = Some(shutdown);
    }

    fn transcript(&self, item: &str) -> String {
        self.wf
            .run(&["session", "watch", short(item), "--no-follow"])
    }

    /// Writes the machine config of the daemon: `text`, with the fake agents unsandboxed.
    fn machine_config(&self, text: &str) {
        std::fs::write(
            self.wf.root.path().join("config.toml"),
            format!("{SANDBOX_OFF}{text}"),
        )
        .unwrap();
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
        let sessions = self.sessions();
        let mut transcripts = String::new();
        for session in &sessions {
            transcripts.push_str(&self.wf.run(&[
                "session",
                "watch",
                short(session["id"].as_str().unwrap()),
                "--no-follow",
            ]));
        }
        panic!("timed out waiting for {what}: {sessions:#?} {transcripts}");
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
    reopens: &'static str,
}

impl WorldSetup {
    async fn start_daemon(self) -> World {
        let (daemon, shutdown) = start_daemon(&self.wf, &self.scripts, self.reopens).await;
        World {
            wf: self.wf,
            daemon,
            shutdown: Some(shutdown),
            scripts: self.scripts,
            reopens: self.reopens,
        }
    }
}

/// Starts a daemon whose harnesses are the fake agent following `scripts`, and serves it.
async fn start_daemon(
    wf: &Sandbox,
    scripts: &BTreeMap<String, PathBuf>,
    reopens: &str,
) -> (Arc<Daemon>, tokio::sync::oneshot::Sender<()>) {
    let data = wf_cli::data_dir::DataDir::new(wf.data.join("workforce"));
    let device = data.device(false).unwrap().unwrap();
    let mut harnesses = BTreeMap::new();
    for (harness, script) in scripts {
        harnesses.insert(
            HarnessName::new(harness).unwrap(),
            AcpAgentConfig::new(fake_agent())
                .env("FAKE_AGENT_SCRIPT", script.to_string_lossy())
                .env("WF_BIN", env!("CARGO_BIN_EXE_wf"))
                .env("XDG_DATA_HOME", wf.data.to_string_lossy())
                .env(
                    "XDG_RUNTIME_DIR",
                    wf.root.path().join("run").to_string_lossy(),
                )
                .env("USER", "joshua")
                .env("FAKE_AGENT_REOPENS", reopens),
        );
    }
    let socket = wf.root.path().join("run/workforce/daemon.sock");
    let daemon = Daemon::start(
        Config {
            data_dir: data.root().to_path_buf(),
            socket: socket.clone(),
            network: Network::Direct,
            harnesses,
            machine_config: Some(wf.root.path().join("config.toml")),
            // A fresh copy of a binary is slow to start the first time on macOS, and the
            // fake agent runs `$WF_BIN`.
            wf_binary: None,
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
            return (daemon, shutdown);
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    panic!("the daemon did not start");
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
wf comment {item} \"wrote the file\" --step 1 --done
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
    let notes = comments(&shown);
    let notes = notes.as_array().unwrap();
    assert!(
        notes
            .iter()
            .any(|note| note["step"]["number"] == 1 && note["text"] == "wrote the file")
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
    assert!(transcript.contains("working on item"), "{transcript}");
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
async fn agents_commit_unsigned_and_the_merge_is_signed_as_git_is_configured() {
    let script = "\
sh echo hi > hello.txt && git add hello.txt && git commit -q -m \"add hello\"
wf work submit
";
    let world = World::new(script).await;
    // Signs with a stand-in for ssh-keygen, which writes a signature beside the data it gets.
    let signer = world.wf.root.path().join("fake-sign");
    std::fs::write(
        &signer,
        "#!/bin/sh\nfor last; do :; done\necho 'fake signature' > \"$last.sig\"\n",
    )
    .unwrap();
    std::fs::set_permissions(&signer, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
    for (key, value) in [
        ("commit.gpgsign", "true"),
        ("gpg.format", "ssh"),
        ("user.signingkey", "key::ssh-ed25519 AAAAfake"),
        ("gpg.ssh.program", signer.to_str().unwrap()),
    ] {
        git(&world.wf.repository, &["config", key, value]);
    }
    let item = world.wf.add(&["--title", "Add hello"]);
    start_session(&world, &item);
    assert_eq!(world.ended_session().await["status"], "submitted");
    world
        .wait("the merge", |world| world.item(&item)["state"] == "done")
        .await;
    let signed = |commit: &str| {
        git(&world.wf.repository, &["cat-file", "commit", commit]).contains("gpgsig")
    };
    assert!(signed("main"), "the merge commit is signed");
    assert!(!signed("main^2"), "the agent's commit is not");
    assert_eq!(
        git(
            &world.wf.repository,
            &["log", "-1", "--format=%an|%cn", "main^2"]
        ),
        "@joshua/coder|@joshua/coder",
        "the agent commits as itself"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_merge_signed_by_the_device_verifies_against_the_workforce() {
    let script = "\
sh echo hi > hello.txt && git add hello.txt && git commit -q -m \"add hello\"
wf work submit
";
    let world = World::new(script).await;
    world.machine_config("[commits]\nmerge_signing = \"device\"\n");
    let item = world.wf.add(&["--title", "Add hello"]);
    start_session(&world, &item);
    assert_eq!(world.ended_session().await["status"], "submitted");
    world
        .wait("the merge", |world| world.item(&item)["state"] == "done")
        .await;
    let allowed = world.wf.root.path().join("allowed_signers");
    std::fs::write(&allowed, world.wf.run(&["device", "allowed-signers"])).unwrap();
    let verified = Command::new("git")
        .args([
            "-c",
            &format!("gpg.ssh.allowedSignersFile={}", allowed.display()),
        ])
        .args(["verify-commit", "main"])
        .current_dir(&world.wf.repository)
        .output()
        .unwrap();
    let said = String::from_utf8_lossy(&verified.stderr);
    assert!(verified.status.success(), "{said}");
    assert!(said.contains("joshua@"), "{said}");
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
        comments(&shown)
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
    let gave_up = comments(&shown).as_array().unwrap().iter().any(|note| {
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
    assert!(comments(&shown).as_array().unwrap().iter().any(|note| {
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
    let waits = comments(&world.item(&item))
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
async fn a_merge_that_git_refuses_waits_in_review_and_is_tried_again() {
    let script = "\
sh echo hi > hello.txt && git add hello.txt && git commit -q -m hello
wf work submit
";
    let world = World::new(script).await;
    let item = world.wf.add(&["--title", "Add hello"]);
    // With main not checked out the merge only moves the ref, which the lock refuses.
    git(&world.wf.repository, &["checkout", "--quiet", "--detach"]);
    let lock = world.wf.repository.join(".git/refs/heads/main.lock");
    std::fs::write(&lock, "").unwrap();
    start_session(&world, &item);
    let session = world.ended_session().await;
    assert_eq!(session["status"], "submitted");
    let shown = world.item(&item);
    assert_eq!(shown["state"], "in_review", "not left active: {shown}");
    assert!(comments(&shown).as_array().unwrap().iter().any(|note| {
        note["text"]
            .as_str()
            .is_some_and(|text| text.starts_with("Could not merge into main"))
    }));

    std::fs::remove_file(&lock).unwrap();
    world.wf.run(&["comment", short(&item), "the lock is gone"]);
    world
        .wait("the merge tried again", |world| {
            world.item(&item)["state"] == "done"
        })
        .await;
    let merged = std::process::Command::new("git")
        .args(["show", "main:hello.txt"])
        .current_dir(&world.wf.repository)
        .output()
        .unwrap();
    assert!(merged.status.success(), "hello.txt is on main");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_session_left_live_by_an_earlier_daemon_is_interrupted_when_the_next_one_starts() {
    let setup = World::without_daemon(&[("coder", "")]);
    let item = setup.wf.add(&["--title", "Orphaned"]);
    {
        let data = wf_cli::data_dir::DataDir::new(setup.wf.data.join("workforce"));
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
    assert_eq!(session["status"], "interrupted");
    assert_eq!(world.item(&item)["state"], "open");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_session_goes_on_in_its_conversation_after_the_daemon_restarts() {
    let script = "\
say started
hang
sh echo done > a.txt && git add a.txt && git commit -q -m a
wf work submit
";
    let mut world = World::new(script).await;
    let item = world.wf.add(&["--title", "Survives a restart"]);
    start_session(&world, &item);
    world
        .wait("the first turn", |world| {
            world.transcript(&item).contains("started")
        })
        .await;
    world.restart().await;
    let session = world.ended_session().await;
    assert_eq!(world.sessions().len(), 1, "the same session went on");
    assert_eq!(session["status"], "submitted", "{session}");
    let transcript = world.transcript(&item);
    assert!(
        transcript.contains("wf restarted while you were working"),
        "{transcript}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_parked_session_stays_parked_across_a_restart() {
    let script = "\
wf comment {item} \"which crate?\" --block
turn
say got {item}
wf work submit
";
    let mut world = World::new(script).await;
    let item = world.wf.add(&["--title", "Waits over a restart"]);
    start_session(&world, &item);
    world
        .wait("the session to park", |world| {
            world.sessions()[0]["status"] == "parked"
        })
        .await;
    world.restart().await;
    tokio::time::sleep(Duration::from_millis(1_000)).await;
    assert_eq!(world.sessions()[0]["status"], "parked");
    world.wf.run(&[
        "comment",
        short(&item),
        "Use humantime",
        "--resolve-blockers",
    ]);
    let session = world.ended_session().await;
    assert_eq!(world.sessions().len(), 1);
    assert_eq!(session["status"], "submitted", "{session}");
    let transcript = world.transcript(&item);
    assert!(transcript.contains("Use humantime"), "{transcript}");
    assert!(
        !transcript.contains("wf restarted while you were working"),
        "a parked session is told what it waited for, nothing else: {transcript}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_session_whose_agent_cannot_reopen_it_is_interrupted_after_a_restart() {
    let mut setup = World::without_daemon(&[("coder", "say started\nhang\n")]);
    setup.reopens = "";
    let mut world = setup.start_daemon().await;
    let item = world.wf.add(&["--title", "Lost on restart"]);
    start_session(&world, &item);
    world
        .wait("the first turn", |world| {
            world.transcript(&item).contains("started")
        })
        .await;
    world.restart().await;
    let session = world.ended_session().await;
    assert_eq!(session["status"], "interrupted", "{session}");
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
async fn a_blocker_parks_the_session_until_it_is_resolved() {
    let script = "\
wf comment {item} \"which crate?\" --block
turn
sh echo done > a.txt && git add a.txt && git commit -q -m a
wf work submit
";
    let world = World::new(script).await;
    let item = world.wf.add(&["--title", "Parse durations"]);
    start_session(&world, &item);
    world
        .wait("the blocker", |world| {
            !world.item(&item)["open"].as_array().unwrap().is_empty()
        })
        .await;
    assert_eq!(world.item(&item)["state"], "in_progress");
    tokio::time::sleep(Duration::from_millis(1_000)).await;
    assert_eq!(world.sessions()[0]["status"], "parked");
    let live = world.wf.json(&["session", "list", "--live"]);
    assert_eq!(
        live.as_array().unwrap().len(),
        1,
        "parked sessions are live"
    );
    let inbox = inbox(&world);
    assert_eq!(inbox.len(), 1, "no supervisor, so it waits on a person");
    assert_eq!(inbox[0]["kind"], "blocker");

    world.wf.run(&[
        "comment",
        short(&item),
        "Use humantime",
        "--resolve-blockers",
    ]);
    let session = world.ended_session().await;
    assert_eq!(world.sessions().len(), 1, "the same session continued");
    assert_eq!(session["status"], "submitted", "{session}");
    assert_eq!(world.item(&item)["state"], "done");
    let transcript = world
        .wf
        .run(&["session", "watch", short(&item), "--no-follow"]);
    assert!(transcript.contains("Use humantime"), "{transcript}");
    let live = world.wf.json(&["session", "list", "--live"]);
    assert!(live.as_array().unwrap().is_empty(), "{live}");
}

#[tokio::test(flavor = "multi_thread")]
async fn the_principal_is_told_of_a_blocker_once_even_across_a_restart() {
    let mut world = World::new("wf comment {item} \"which crate?\" --block\n").await;
    let told = world.wf.root.path().join("told.txt");
    world.machine_config(&format!(
        "[notify]\ncommand = ['sh', '-c', 'printf \"%s|%s|%s\\n\" \"$1\" \"$2\" \"$3\" >> \"$0\"', '{}', '{{kind}}', '{{item}}', '{{what}}']\n",
        told.display()
    ));
    let lines = |_: &World| -> Vec<String> {
        std::fs::read_to_string(&told)
            .unwrap_or_default()
            .lines()
            .map(str::to_string)
            .collect()
    };
    let item = world.wf.add(&["--title", "Parse durations"]);
    start_session(&world, &item);
    world
        .wait("the blocker is told of", |world| lines(world).len() == 1)
        .await;
    let first = &lines(&world)[0];
    assert!(
        first.starts_with(&format!("blocker|{}|blocker ", short(&item)))
            && first.contains("--resolve"),
        "{first}"
    );

    world.restart().await;
    world
        .wf
        .run(&["comment", short(&item), "Which database?", "--block"]);
    world
        .wait("the second blocker is told of", |world| {
            lines(world).len() >= 2
        })
        .await;
    tokio::time::sleep(Duration::from_millis(500)).await;
    let told = lines(&world);
    assert_eq!(
        told.len(),
        2,
        "the first blocker is not told of again: {told:?}"
    );
    assert_ne!(told[0], told[1]);
}

#[tokio::test(flavor = "multi_thread")]
async fn cancelling_an_item_stops_its_session() {
    let world = World::new("wf comment {item} \"which crate?\" --block\n").await;
    let item = world.wf.add(&["--title", "Not needed after all"]);
    start_session(&world, &item);
    world
        .wait("the blocker", |world| {
            !world.item(&item)["open"].as_array().unwrap().is_empty()
        })
        .await;
    world
        .wf
        .run(&["item", "cancel", short(&item), "--note", "done elsewhere"]);
    let session = world.ended_session().await;
    assert_eq!(session["status"], "cancelled", "{session}");
    assert_eq!(world.item(&item)["state"], "dropped");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_session_that_waits_too_long_for_an_answer_times_out() {
    let script = "wf comment {item} \"anyone there?\" --block\n";
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
async fn submitting_without_commits_finishes_the_item_and_leaves_no_branch() {
    let world = World::new("wf work submit --note \"Nothing to change\"\n").await;
    let item = world
        .wf
        .add(&["--title", "Check the parser", "--review", "none"]);
    start_session(&world, &item);
    let session = world.ended_session().await;
    assert_eq!(session["status"], "submitted");
    assert_eq!(world.item(&item)["state"], "done");
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
    // What the daemon does by itself is not written in the name of the person.
    let log = world.wf.run(&["log"]);
    for kind in ["SessionStarted", "Merged", "ItemMoved"] {
        let lines: Vec<&str> = log.lines().filter(|line| line.contains(kind)).collect();
        assert!(!lines.is_empty(), "{kind}: {log}");
        assert!(
            lines.iter().all(|line| line.contains("daemon")),
            "{lines:?}"
        );
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn nothing_is_started_for_agents_the_device_does_not_host() {
    let world = World::new(COMMIT_AND_SUBMIT).await;
    world.automate("coder", "default");
    // Defining an automatic agent hosts it here; hosting only another agent takes it away again.
    world
        .wf
        .run(&["device", "edit", "--agent", "someone-else=1"]);
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
async fn a_supervisor_answers_the_blocker_of_a_coder_and_the_coder_goes_on() {
    let coder = "\
wf comment {item} \"which crate?\" --block
turn
sh echo done > a.txt && git add a.txt && git commit -q -m a
wf work submit
";
    let supervisor = "wf comment {item} \"Use humantime\" --resolve-blockers\n";
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
    let mut statuses: Vec<String> = world
        .sessions()
        .iter()
        .map(|session| format!("{} {}", session["role"], session["status"]))
        .collect();
    statuses.sort();
    assert_eq!(
        statuses,
        ["\"supervise\" \"supervised\"", "\"work\" \"submitted\""]
    );
    let transcript = world
        .wf
        .run(&["session", "watch", short(&item), "--no-follow"]);
    assert!(transcript.contains("Use humantime"), "{transcript}");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_diagnosis_comes_before_the_retry_and_the_principal_decides_after() {
    let coder = "wf work release --note \"npm cannot reach the registry\"\n";
    let doctor = "wf comment {item} \"The sandbox blocks the registry; vendor the package\"\n";
    let world = World::agents(&[("coder", coder), ("doctor", doctor)]).await;
    world.automate("coder", "default");
    world.automate("doctor", "supervision");
    world
        .wf
        .run(&["agent", "edit", "doctor", "--capability", "review"]);
    world.wf.run(&[
        "device",
        "edit",
        "--max-sessions",
        "2",
        "--agent",
        "coder=1",
        "--agent",
        "doctor=1",
    ]);
    let item = world
        .wf
        .add(&["--title", "Install the parser", "--recovery", "diagnose"]);
    world
        .wait("the item left to the principal", |world| {
            world
                .wf
                .json(&["inbox"])
                .as_array()
                .is_some_and(|entries| entries.iter().any(|entry| entry["kind"] == "recovery"))
        })
        .await;
    let mut statuses: Vec<String> = world
        .sessions()
        .iter()
        .map(|session| format!("{} {}", session["role"], session["status"]))
        .collect();
    statuses.sort();
    assert_eq!(
        statuses,
        [
            "\"diagnose\" \"diagnosed\"",
            "\"work\" \"released\"",
            "\"work\" \"released\""
        ]
    );
    let shown = world.wf.run(&["item", "show", short(&item)]);
    assert!(shown.contains("vendor the package"), "{shown}");
    assert!(shown.contains("waits for you"), "{shown}");
    let status = world.wf.json(&["status"]);
    assert_eq!(status["daemon"], true);
    assert_eq!(
        status["after_failures"].as_array().unwrap().len(),
        1,
        "{status}"
    );
    assert!(
        status["next"].as_str().unwrap().contains("wf inbox"),
        "{status}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_supervisor_escalates_a_blocker_to_a_person() {
    let coder = "\
wf comment {item} \"is this still wanted?\" --block
turn
sh echo done > a.txt && git add a.txt && git commit -q -m a
wf work submit
";
    let supervisor =
        "wf comment {item} \"@joshua this is about intent\" --block --resolve-blockers\n";
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
        .wait("the escalation in the inbox", |world| {
            inbox(world).iter().any(|entry| entry["kind"] == "blocker")
        })
        .await;
    assert_ne!(world.item(&item)["state"], "done");
    world
        .wf
        .run(&["comment", short(&item), "Yes, go on", "--resolve-blockers"]);
    world
        .wait("the item done", |world| {
            world.item(&item)["state"] == "done"
        })
        .await;
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
        comments(&shown)
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
sh if [ -f hello.txt ]; then echo test > test.txt && git add test.txt && git commit -q -m test && \"$WF_BIN\" comment {item} \"added one\" --resolve-change-requests; else echo hi > hello.txt && git add hello.txt && git commit -q -m hello; fi
wf work submit
";
    let reviewer = "\
sh if [ -f test.txt ]; then \"$WF_BIN\" review --approve; else \"$WF_BIN\" comment {item} \"add a test\" --request-change && \"$WF_BIN\" review --request-changes; fi
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
        .wait("the principal step", |world| {
            world.item(&item)["review_status"]["step"] == "principal"
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
        .wait("the principal step", |world| {
            world.item(&item)["review_status"]["step"] == "principal"
        })
        .await;
    let refused = world
        .wf
        .fail(&["review", short(&item), "--request-changes", "--note", "no"]);
    assert!(
        refused.contains("needs at least one open change request"),
        "{refused}"
    );
    world
        .wf
        .run(&["comment", short(&item), "not like this", "--request-change"]);
    world.wf.run(&["review", short(&item), "--request-changes"]);
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
    assert!(comments(&shown).as_array().unwrap().iter().any(|note| {
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
async fn the_work_of_an_item_can_be_read_and_checked_out() {
    let script = "\
sh echo four >> file.txt && git commit -q -am four
wf work release --note later
";
    let world = World::new(script).await;
    let item = world.wf.add(&["--title", "Adds a line"]);
    start_session(&world, &item);
    world.ended_session().await;

    let stat = world.wf.run(&["item", "diff", short(&item), "--stat"]);
    assert!(stat.contains("file.txt | 1 +"), "{stat}");
    let diff = world
        .wf
        .json(&["item", "diff", short(&item), "--", "file.txt"]);
    assert!(diff["diff"].as_str().unwrap().contains("+four"), "{diff}");

    let path = world.wf.run(&["item", "path", short(&item)]);
    assert!(
        std::path::Path::new(path.trim()).join("file.txt").is_file(),
        "{path}"
    );

    let checkout = world.wf.json(&["item", "checkout", short(&item)]);
    let head = git(&world.wf.repository, &["rev-parse", "HEAD"]);
    assert_eq!(checkout["commit"], head);
    assert!(
        std::fs::read_to_string(world.wf.repository.join("file.txt"))
            .unwrap()
            .contains("four")
    );
    world.wf.run(&["item", "checkout", "-"]);
    assert_eq!(
        git(&world.wf.repository, &["rev-parse", "--abbrev-ref", "HEAD"]),
        "main"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn watch_prints_events_as_they_arrive() {
    let world =
        World::new("say hello\nwf comment {item} \"halfway there\"\nwf work release --note done\n")
            .await;
    let item = world.wf.add(&["--title", "Watched"]);
    let mut watch = world
        .wf
        .command(&["watch", "--json"])
        .stdout(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let stdout = watch.stdout.take().unwrap();
    let (lines, received) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        for line in std::io::BufRead::lines(std::io::BufReader::new(stdout)) {
            let Ok(line) = line else { return };
            if lines.send(line).is_err() {
                return;
            }
        }
    });
    // The backlog ends with the item.
    loop {
        let line = received.recv_timeout(Duration::from_secs(10)).unwrap();
        if line.contains("\"created\"") {
            break;
        }
    }

    start_session(&world, &item);
    world.ended_session().await;
    let mut kinds = Vec::new();
    while let Ok(line) = received.recv_timeout(Duration::from_secs(5)) {
        let line: Value = serde_json::from_str(&line).unwrap();
        kinds.push(line["data"]["kind"].as_str().unwrap().to_string());
        if kinds.last().map(String::as_str) == Some("session_ended") {
            break;
        }
    }
    watch.kill().unwrap();
    watch.wait().unwrap();
    assert!(kinds.contains(&"session_started".to_string()), "{kinds:?}");
    assert!(kinds.contains(&"comment".to_string()), "{kinds:?}");
    assert_eq!(
        kinds.last().map(String::as_str),
        Some("session_ended"),
        "{kinds:?}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn running_wf_is_allowed_without_asking_anyone() {
    let script = "\
permission wf item show {item} 2>&1 | head -40
wf work submit
";
    let world = World::new(script).await;
    let item = world.wf.add(&["--title", "Reads itself"]);
    start_session(&world, &item);
    let session = world.ended_session().await;
    assert_eq!(session["status"], "submitted");
    let transcript = world
        .wf
        .run(&["session", "watch", short(&item), "--no-follow"]);
    assert!(transcript.contains("allowed by rules"), "{transcript}");
    assert!(transcript.contains("permission: yes"), "{transcript}");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_hook_of_the_machine_config_decides_before_a_person_is_asked() {
    let script = "\
permission
say done
wf work release --note done
";
    let world = World::new(script).await;
    world.machine_config(
        r#"
[[permissions.decider]]
kind = "rules"

[[permissions.decider]]
kind = "hook"
command = ["sh", "-c", "grep -q '\"title\":\"Edit file\"' && echo '{\"permission\":\"deny\",\"reason\":\"no edits today\"}'"]
"#,
    );
    let item = world.wf.add(&["--title", "Wants to edit"]);
    start_session(&world, &item);
    world.ended_session().await;
    let transcript = world
        .wf
        .run(&["session", "watch", short(&item), "--no-follow"]);
    assert!(
        transcript.contains("denied by hook sh: no edits today"),
        "{transcript}"
    );
    assert!(transcript.contains("permission: no"), "{transcript}");
}

#[tokio::test(flavor = "multi_thread")]
async fn an_agent_of_the_workforce_can_decide() {
    let world = World::agents(&[
        (
            "coder",
            "permission\nsay done\nwf work release --note done\n",
        ),
        (
            "judge",
            "say Editing files is what the item is for.\n\
             say {\"permission\": \"allow\", \"reason\": \"edits serve the item\"}\n",
        ),
    ])
    .await;
    world.machine_config("[[permissions.decider]]\nkind = \"agent\"\nagent = \"judge\"\n");
    let item = world.wf.add(&["--title", "Wants to edit"]);
    start_session(&world, &item);
    world.ended_session().await;
    let transcript = world
        .wf
        .run(&["session", "watch", short(&item), "--no-follow"]);
    assert!(
        transcript.contains("allowed by agent judge: edits serve the item"),
        "{transcript}"
    );
    assert!(transcript.contains("permission: yes"), "{transcript}");
    let sessions = world.sessions();
    assert_eq!(sessions.len(), 1, "a judge is no wf session: {sessions:?}");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_request_every_decider_passes_on_says_why_in_the_inbox() {
    let world = World::new("permission\nsay done\nhang\n").await;
    world.machine_config(
        "[[permissions.decider]]\nkind = \"hook\"\ncommand = [\"sh\", \"-c\", \"echo '{\\\"permission\\\":\\\"ask\\\",\\\"reason\\\":\\\"not sure\\\"}'\"]\n",
    );
    let item = world.wf.add(&["--title", "Unsure"]);
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
    assert_eq!(entry["passed"], serde_json::json!(["hook sh: not sure"]));
    world
        .wf
        .run(&["session", "deny", short(entry["session"].as_str().unwrap())]);
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
async fn stopping_a_session_that_waits_on_a_permission_answers_it_cancelled() {
    let world = World::new("permission\nsay never\n").await;
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
    world.wf.run(&["session", "stop", short(&id)]);
    let session = world.ended_session().await;
    assert_eq!(session["status"], "cancelled");
    assert!(
        !inbox(&world)
            .iter()
            .any(|entry| entry["kind"] == "permission"),
        "the request is gone"
    );
    let transcript = world
        .wf
        .run(&["session", "watch", short(&item), "--no-follow"]);
    assert!(transcript.contains("permission: cancelled"), "{transcript}");
    assert!(!transcript.contains("never"), "{transcript}");
}

#[tokio::test(flavor = "multi_thread")]
async fn the_inbox_lists_what_waits_on_a_person() {
    let world = World::new("").await;
    assert!(inbox(&world).is_empty());
    let item = world.wf.add(&["--title", "Pick a database"]);
    world
        .wf
        .run(&["comment", short(&item), "Which database?", "--block"]);
    let entries = inbox(&world);
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0]["kind"], "blocker");
    assert_eq!(entries[0]["title"], "Pick a database");
    let text = world.wf.run(&["inbox"]);
    assert!(
        text.contains("Pick a database") && text.contains("--resolve"),
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
async fn an_agent_reads_its_brief_with_its_instructions_again_with_wf_work_brief() {
    let script = "\
sh \"$WF_BIN\" work brief > brief.md && git add brief.md && git commit -q -m brief
wf work submit
";
    let world = World::new(script).await;
    world.wf.run(&[
        "agent",
        "edit",
        "coder",
        "--instructions",
        "Prefer the standard library.",
    ]);
    let item = world
        .wf
        .add(&["--title", "Add hello", "--body", "- [ ] write the file"]);
    start_session(&world, &item);
    let session = world.ended_session().await;
    assert_eq!(session["status"], "submitted", "{session}");
    let brief = git(&world.wf.repository, &["show", "main:brief.md"]);
    assert!(
        brief.contains("## Instructions\n\nPrefer the standard library.\n"),
        "{brief}"
    );
    assert!(
        brief.starts_with(&format!(
            "You are the agent `coder`, working on item #{}: Add hello\n",
            short(&item)
        )),
        "{brief}"
    );
    assert!(brief.contains("- [ ] 1. write the file"), "{brief}");
    assert!(brief.contains("`wf work submit --note"), "{brief}");

    let outside = world.wf.fail(&["work", "brief"]);
    assert!(outside.contains("WF_SESSION"), "{outside}");
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
