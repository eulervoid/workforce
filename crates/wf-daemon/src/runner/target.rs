//! Keeping the target branch of `<wf>/git` where the log says it is (SPEC §10). A push of the
//! principal is written as `TargetMoved` onto the tip the log names; the branch follows the end of
//! the chain, forward or, after a move of this device lost a fork, back to the winner; and an item
//! whose merge lost goes back to `review` to be merged again. A branch that shares no history with
//! the tip is none of these: it waits for the principal, who adopts it or puts it back.

use std::collections::BTreeSet;
use std::sync::{Arc, Weak};
use std::time::Duration;

use wf_core::claims::Presence;
use wf_core::event::{Commented, EventBody, TargetMoved};
use wf_core::id::{CommentId, ItemId, WorkforceId};
use wf_core::identity::{Actor, DeviceKey, PrincipalKey};
use wf_core::item::ItemState;
use wf_core::session::CommitId;
use wf_core::state::State;
use wf_vcs::{GitCli, VcsError};

use super::{SessionError, blocking};
use crate::{Daemon, Loaded};

/// How often the files of the target branch are read for a push of the principal, which nothing
/// announces. Git runs only when they say the branch moved.
const LOOK_FOR_PUSHES: Duration = Duration::from_secs(5);

/// Where this daemon last saw the target branch of `<wf>/git`. Held while the branch moves and
/// while the event that says so is written, so that a merge of this daemon is never taken for a
/// push, nor a push for a merge.
#[derive(Default)]
pub(crate) struct TargetBranch {
    pub(super) seen: tokio::sync::Mutex<Option<CommitId>>,
    /// For the tests: the heads of the log the follower last finished with.
    #[cfg(test)]
    followed: tokio::sync::watch::Sender<BTreeSet<wf_core::id::EventId>>,
}

/// Whether the target branch of `<wf>/git` is where the log says after a pass.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum TargetAt {
    /// At `at`, the end of the chain, or where it was when the log names no tip yet: a merge may
    /// start on it, and only on it.
    Tip { at: CommitId },
    /// Elsewhere: the tip is still to arrive, or a push was just written and the log has not
    /// settled on it. A merge waits.
    Elsewhere,
    /// At `branch`, which shares no history with `tip`, the tip the log names. No pass changes
    /// that: a merge waits for the principal.
    Unrelated { branch: CommitId, tip: CommitId },
}

/// What a pass does with the target branch.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Follow {
    /// It is where the log says, at `at`.
    Stays { at: CommitId },
    /// It cannot follow yet: the tip has not arrived.
    Waits,
    /// It shares no history with the tip: rewritten, or not the repository it should be. It stays
    /// where it is until the principal says.
    Unrelated { branch: CommitId, tip: CommitId },
    /// The principal pushed: write the move.
    Record { commit: CommitId, onto: CommitId },
    /// Forward to the tip, or back to it from a move that lost a fork.
    Moves { from: CommitId, to: CommitId },
}

/// What a pass reads from the log.
struct Reading {
    target: String,
    tip: Option<CommitId>,
    /// Every commit a move of the target left or made, on the chain or not.
    been_at: BTreeSet<CommitId>,
    principal: Option<PrincipalKey>,
    /// The items whose merge lost a fork that this device sends back, with the commit it made.
    lost_merges: Vec<(ItemId, CommitId)>,
}

fn read(state: &State, own: DeviceKey, presence: &Presence, now_ms: u64) -> Option<Reading> {
    let policy = &state.policy()?.policy;
    let lost_merges = state
        .lost_merges(own, presence, now_ms)
        .into_iter()
        .filter_map(|item| {
            let wf_core::item::ItemEntry::Live(live) = state.item(item)? else {
                return None;
            };
            let lost = live.all_merges().max_by_key(|merge| merge.merged_at())?;
            Some((item, lost.commit))
        })
        .collect();
    Some(Reading {
        target: policy.merge.target.clone(),
        tip: state.target(),
        been_at: state
            .target_moves()
            .flat_map(|step| [step.commit, step.onto])
            .collect(),
        principal: state.principal_of(&own),
        lost_merges,
    })
}

/// Where the branch is, and what to do with it.
fn decide(
    git: &GitCli,
    reading: &Reading,
    seen: Option<CommitId>,
) -> Result<(Option<CommitId>, Follow), VcsError> {
    let branch = match git.branch_head(&reading.target) {
        Ok(branch) => branch,
        Err(VcsError::NoBranch(_)) => return Ok((None, Follow::Waits)),
        Err(error) => return Err(error),
    };
    let follow = match reading.tip {
        Some(tip) if tip == branch => Follow::Stays { at: tip },
        Some(tip) if !git.has_commit(tip)? => Follow::Waits,
        // A move of the log, or behind its tip: the branch goes to the tip, forward or back.
        Some(tip) if reading.been_at.contains(&branch) || git.is_ancestor(branch, tip)? => {
            Follow::Moves {
                from: branch,
                to: tip,
            }
        }
        // A push: from where it forked off the chain, which is the tip when it fast-forwards.
        Some(tip) => match git.merge_base(branch, tip)? {
            Some(onto) => Follow::Record {
                commit: branch,
                onto,
            },
            None => Follow::Unrelated { branch, tip },
        },
        // No move named a tip yet: a push is one from where this daemon last saw the branch.
        None => match seen {
            Some(seen) if seen != branch && !reading.been_at.contains(&branch) => Follow::Record {
                commit: branch,
                onto: seen,
            },
            Some(_) | None => Follow::Stays { at: branch },
        },
    };
    Ok((Some(branch), follow))
}

/// What the log says of the target that a pass acts on. A change of the log that leaves it as it
/// was needs no pass, so that not every event runs git.
#[derive(PartialEq, Eq)]
struct Watched {
    target: Option<String>,
    tip: Option<CommitId>,
    moves: usize,
    lost_merges: Vec<ItemId>,
}

/// What woke the follower.
enum Woke {
    LogChanged,
    CommitsArrived,
    LookForPushes,
}

/// Follows the target of a workforce for as long as the daemon runs and its storage does: when
/// what the log says of it changes, when commits arrived, and when the branch moved, a push that
/// nothing announces.
pub(crate) async fn run_target(daemon: Weak<Daemon>, workforce: WorkforceId, loaded: Arc<Loaded>) {
    let Some(mut halted) = daemon.upgrade().map(|strong| strong.halted.subscribe()) else {
        return;
    };
    let mut heads = loaded.storage.watch_heads();
    let mut arrived = loaded.commits.arrived.subscribe();
    let mut watched = None;
    // The branch as its files said before the last pass that left it at the tip; `None` when the
    // last pass did not, so that the next look runs one.
    let mut followed_from = None;
    // Ticks however often the log changes; the first tick is at once.
    let mut look_for_pushes = tokio::time::interval(LOOK_FOR_PUSHES);
    look_for_pushes.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        let woke = tokio::select! {
            changed = heads.changed() => {
                if changed.is_err() {
                    return;
                }
                heads.borrow_and_update();
                Woke::LogChanged
            }
            () = async { let _ = halted.wait_for(|halted| *halted).await; } => return,
            _ = arrived.changed() => Woke::CommitsArrived,
            _ = look_for_pushes.tick() => Woke::LookForPushes,
        };
        let Some(strong) = daemon.upgrade() else {
            return;
        };
        // What the state read below holds at least.
        #[cfg(test)]
        let reading = heads.borrow().clone();
        let (own, presence, now_ms) = (strong.device(), strong.presence_now(), wf_store::now_ms());
        let now = strong
            .read(&loaded, move |state| Watched {
                target: state
                    .policy()
                    .map(|policy| policy.policy.merge.target.clone()),
                tip: state.target(),
                moves: state.target_moves().count(),
                lost_merges: state.lost_merges(own, &presence, now_ms),
            })
            .await
            .ok();
        let branch = branch_from_files(&loaded, now.as_ref()).await;
        let due = match woke {
            Woke::LogChanged => false,
            // The tip may be what arrived.
            Woke::CommitsArrived => true,
            Woke::LookForPushes => branch.is_none() || branch != followed_from,
        };
        if due || now != watched {
            watched = now;
            followed_from = match strong.follow_target(workforce, &loaded).await {
                // Only a change of the branch or of the log changes what a pass finds.
                Ok(TargetAt::Tip { .. } | TargetAt::Unrelated { .. }) => branch,
                Ok(TargetAt::Elsewhere) => None,
                Err(error) => {
                    tracing::warn!(
                        "could not follow the target branch: {}",
                        wf_core::error_chain(&error)
                    );
                    None
                }
            };
        }
        #[cfg(test)]
        loaded.target.followed.send_replace(reading);
        drop(strong);
    }
}

/// Where the target branch is, as its files in `<wf>/git` say without git. `None` when they do not
/// tell, or the repository has not been opened yet.
async fn branch_from_files(loaded: &Loaded, watched: Option<&Watched>) -> Option<CommitId> {
    let git = loaded.bare.get()?.clone();
    let target = watched?.target.clone()?;
    tokio::task::spawn_blocking(move || git.branch_head_from_files(&target))
        .await
        .ok()
        .flatten()
}

impl Daemon {
    /// One pass: writes a push of the principal as a move of the target, moves the branch to the
    /// end of the chain, and sends the items whose merge lost a fork back to `review`.
    pub(super) async fn follow_target(
        &self,
        workforce: WorkforceId,
        loaded: &Loaded,
    ) -> Result<TargetAt, SessionError> {
        let Some(git) = self.open_bare_repository(workforce, loaded).await? else {
            return Ok(TargetAt::Elsewhere);
        };
        let mut seen = loaded.target.seen.lock().await;
        // A push written in the first round is settled by the log in the second.
        let mut at = TargetAt::Elsewhere;
        for _ in 0..2 {
            let Some((follow, reading)) = self.look(loaded, &git, &mut seen).await? else {
                return Ok(TargetAt::Elsewhere);
            };
            self.send_back_lost_merges(loaded, &reading).await?;
            match follow {
                Follow::Stays { .. }
                | Follow::Waits
                | Follow::Record { .. }
                | Follow::Moves { .. } => {
                    self.faults.target_followed(workforce, &reading.target);
                }
                Follow::Unrelated { branch, tip } => {
                    self.faults
                        .note_unrelated_target(workforce, &reading.target, branch, tip);
                }
            }
            match follow {
                Follow::Stays { at } => return Ok(TargetAt::Tip { at }),
                Follow::Waits => return Ok(TargetAt::Elsewhere),
                Follow::Unrelated { branch, tip } => {
                    return Ok(TargetAt::Unrelated { branch, tip });
                }
                Follow::Moves { from, to } => {
                    let (git, target) = (git.clone(), reading.target.clone());
                    let moved = blocking(move || git.move_target(&target, from, to)).await??;
                    if moved {
                        *seen = Some(to);
                        return Ok(TargetAt::Tip { at: to });
                    }
                    // Something moved it meanwhile, a push say: the next pass sees it.
                    return Ok(TargetAt::Elsewhere);
                }
                Follow::Record { commit, onto } => {
                    let Some(principal) = reading.principal else {
                        return Err(SessionError::NotBound);
                    };
                    self.author(
                        loaded,
                        Actor::Daemon(principal),
                        EventBody::TargetMoved(TargetMoved { commit, onto }),
                    )
                    .await?;
                    at = TargetAt::Elsewhere;
                }
            }
        }
        Ok(at)
    }

    /// Reads the log and the branch, and works out what a pass does. `None` without a policy.
    async fn look(
        &self,
        loaded: &Loaded,
        git: &GitCli,
        seen: &mut Option<CommitId>,
    ) -> Result<Option<(Follow, Reading)>, SessionError> {
        let own = self.device();
        let (presence, now_ms) = (self.presence_now(), wf_store::now_ms());
        let reading = self
            .read(loaded, move |state| read(state, own, &presence, now_ms))
            .await?;
        let Some(reading) = reading else {
            return Ok(None);
        };
        let (pass_git, last) = (git.clone(), *seen);
        let ((branch, follow), reading) =
            blocking(move || Ok::<_, VcsError>((decide(&pass_git, &reading, last)?, reading)))
                .await??;
        *seen = branch.or(*seen);
        Ok(Some((follow, reading)))
    }

    /// The principal says that the target branch, unrelated to the tip the log names, is right:
    /// writes it as a move from that tip. It must still be at `commit`, the one they looked at.
    pub(crate) async fn adopt_target(
        &self,
        workforce: WorkforceId,
        loaded: &Loaded,
        commit: CommitId,
    ) -> Result<(), SessionError> {
        let Some(git) = self.open_bare_repository(workforce, loaded).await? else {
            return Err(SessionError::TargetNotUnrelated);
        };
        let mut seen = loaded.target.seen.lock().await;
        let Some((follow, reading)) = self.look(loaded, &git, &mut seen).await? else {
            return Err(SessionError::NoPolicy);
        };
        let onto = match follow {
            Follow::Unrelated { branch, tip } if branch == commit => tip,
            Follow::Unrelated { branch, .. } => {
                return Err(SessionError::TargetElsewhere { branch });
            }
            Follow::Stays { .. } | Follow::Waits | Follow::Record { .. } | Follow::Moves { .. } => {
                return Err(SessionError::TargetNotUnrelated);
            }
        };
        let principal = reading.principal.ok_or(SessionError::NotBound)?;
        self.author(
            loaded,
            Actor::Daemon(principal),
            EventBody::TargetMoved(TargetMoved { commit, onto }),
        )
        .await?;
        drop(seen);
        self.follow_target(workforce, loaded).await?;
        Ok(())
    }

    /// Moves each item whose merge lost a fork back to `review`, if this device made it or its
    /// maker is gone, with a comment that says why. There it waits for its merge again, which
    /// runs its checks again.
    async fn send_back_lost_merges(
        &self,
        loaded: &Loaded,
        reading: &Reading,
    ) -> Result<(), SessionError> {
        let Some(principal) = reading.principal else {
            return Ok(());
        };
        for (item, merged) in &reading.lost_merges {
            let text = format!(
                "The merge {} lost: another device moved {} from the same tip first. The work is \
                 not in {} and waits for its merge again, checks included.",
                merged.short(),
                reading.target,
                reading.target
            );
            let comment = self
                .author(
                    loaded,
                    Actor::Daemon(principal),
                    EventBody::Commented(Commented::plain(*item, text)),
                )
                .await?;
            let because = Some(CommentId::from(comment.id()));
            self.move_item(loaded, principal, *item, ItemState::Review, because, None)
                .await?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use tempfile::TempDir;
    use wf_core::event::Event;
    use wf_core::target::MoveCause;
    use wf_core::testing::*;
    use wf_net::Network;
    use wf_store::{RedbStore, Replica};
    use wf_vcs::testing::{TestRepository, git_in};

    use super::super::merge::{Attempt, Claiming, MergeRequest};
    use super::*;
    use crate::{DataDir, Launch};

    /// A daemon on device 0 of a workforce with two devices, whose `<wf>/git` has the `main` of
    /// `repository`.
    struct Scene {
        _directory: TempDir,
        fixture: Fixture,
        daemon: Arc<Daemon>,
        loaded: Arc<Loaded>,
        repository: TestRepository,
        clock: u64,
    }

    impl Scene {
        async fn new() -> Self {
            let directory = TempDir::new().unwrap();
            let fixture = Fixture::new(2);
            let workforce = fixture.workforce();
            let store_directory = directory.path().join(workforce.to_string());
            std::fs::create_dir_all(&store_directory).unwrap();
            let mut replica = Replica::open(
                RedbStore::open(store_directory.join(crate::data_dir::STORE_FILE)).unwrap(),
                workforce,
            )
            .unwrap();
            replica.receive(fixture.genesis.clone()).unwrap();
            drop(replica);
            // Merged unsigned, whatever the git config of the one running the tests says.
            let machine_config = directory.path().join("config.toml");
            std::fs::write(&machine_config, "[commits]\nmerge_signing = \"unsigned\"\n").unwrap();
            let daemon = Daemon::start(
                Launch {
                    data_dir: DataDir::new(directory.path().to_path_buf()),
                    socket: directory.path().join("daemon.sock"),
                    network: Network::Direct,
                    harnesses: Default::default(),
                    machine_config: Some(machine_config),
                    wf_binary: None,
                },
                fixture.devices[0].clone(),
            )
            .await
            .unwrap();
            let loaded = daemon.workforce(workforce).await.unwrap();
            let scene = Self {
                _directory: directory,
                fixture,
                daemon,
                loaded,
                repository: TestRepository::new(),
                clock: 2_000,
            };
            scene.fetch("main", "refs/heads/main");
            scene
        }

        fn bare(&self) -> PathBuf {
            self.daemon.bare_repository(self.fixture.workforce())
        }

        /// Fetches `branch` of the repository into `reference` of `<wf>/git`.
        fn fetch(&self, branch: &str, reference: &str) {
            let source = self.repository.directory().display().to_string();
            let refspec = format!("+refs/heads/{branch}:{reference}");
            git_in(&self.bare(), &["fetch", "--quiet", &source, &refspec]);
        }

        /// A commit on `branch` of the repository, present in `<wf>/git` without moving `main`.
        fn commit(&self, branch: &str) -> CommitId {
            let commit = self
                .repository
                .work(branch, &format!("{branch}.txt"), "x\n");
            self.fetch(branch, &format!("refs/test/{branch}"));
            commit
        }

        /// The principal pushes `branch` of the repository to `main`.
        fn push(&self, branch: &str) {
            self.fetch(branch, "refs/heads/main");
        }

        /// A commit with the tree of `main` and no history, as a rewrite of the root leaves: in
        /// `<wf>/git`, without moving `main`.
        fn rewritten(&self) -> CommitId {
            let bare = self.bare();
            let commit = git_in(
                &bare,
                &[
                    "-c",
                    "user.name=principal",
                    "-c",
                    "user.email=principal@example.com",
                    "commit-tree",
                    "main^{tree}",
                    "-m",
                    "rewritten",
                ],
            );
            git_in(&bare, &["update-ref", "refs/test/rewritten", &commit]);
            CommitId::Git(commit.parse().unwrap())
        }

        /// `main` of `<wf>/git` rewritten by a direct write of the ref, as no push can.
        fn rewrite(&self) -> CommitId {
            let rewritten = self.rewritten();
            let bare = self.bare();
            git_in(
                &bare,
                &["update-ref", "refs/heads/main", &rewritten.to_string()],
            );
            rewritten
        }

        /// An item in `review` with work submitted at `reviewed`.
        async fn submitted(&mut self, reviewed: CommitId) -> (ItemId, wf_core::id::SessionId) {
            let founder = self.fixture.founder_actor();
            self.write(0, founder.clone(), define_agent("coder")).await;
            let item: ItemId = self
                .write(0, founder.clone(), create("task", "work"))
                .await
                .id()
                .into();
            let session = self
                .write(0, founder.clone(), start_session("coder", item))
                .await
                .id()
                .into();
            self.write(0, founder.clone(), moved(item, ItemState::Review))
                .await;
            self.write(0, founder, submit_session(session, reviewed))
                .await;
            (item, session)
        }

        async fn adopt(&self, commit: CommitId) -> Result<(), SessionError> {
            self.daemon
                .adopt_target(self.fixture.workforce(), &self.loaded, commit)
                .await
        }

        fn unrelated(&self) -> Option<crate::UnrelatedTarget> {
            self.daemon
                .faults
                .current_in(self.fixture.workforce())
                .unrelated_target
        }

        /// Waits for what the dispatcher does to the item to show in the state.
        async fn wait_for(&self, what: &str, done: impl Fn(&State) -> bool) -> State {
            let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
            loop {
                let state = self.state().await;
                if done(&state) {
                    return state;
                }
                assert!(tokio::time::Instant::now() < deadline, "{what}");
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        }

        fn main(&self) -> CommitId {
            GitCli::open(&self.bare())
                .unwrap()
                .branch_head("main")
                .unwrap()
        }

        fn daemon_actor(&self) -> Actor {
            Actor::Daemon(self.fixture.founder.key())
        }

        /// Writes `body` as `actor` on device `device`.
        async fn write(&mut self, device: usize, actor: Actor, body: EventBody) -> Event {
            self.clock += 1;
            let (signer, now) = (self.fixture.devices[device].clone(), self.clock);
            self.loaded
                .storage
                .with(move |replica| replica.author(&signer, actor, now, body))
                .await
                .unwrap()
                .unwrap()
        }

        async fn state(&self) -> State {
            State::clone(&self.daemon.snapshot(&self.loaded).await.unwrap())
        }

        /// What device 1 writes on `state`, as it saw the log then: it wins a fork with anything
        /// written here after it.
        fn written_apart(&self, state: &State, body: EventBody) -> Event {
            self.fixture
                .author_as(state, 1, self.daemon_actor(), self.clock, body)
        }

        async fn receive(&self, event: Event) {
            self.loaded
                .storage
                .with(move |replica| replica.receive(event).map(|_| ()))
                .await
                .unwrap()
                .unwrap();
        }

        /// Where a pass leaves the branch, if at the tip.
        async fn follow(&self) -> Option<CommitId> {
            match self
                .daemon
                .follow_target(self.fixture.workforce(), &self.loaded)
                .await
                .unwrap()
            {
                TargetAt::Tip { at } => Some(at),
                TargetAt::Elsewhere | TargetAt::Unrelated { .. } => None,
            }
        }

        /// The log names `main` of the repository as the tip, moved there by device 1.
        async fn tip_at_main(&mut self) -> CommitId {
            let main = self.repository.main();
            let before = self.repository.work("before", "before.txt", "x\n");
            let actor = self.daemon_actor();
            self.write(1, actor, target_moved(main, before)).await;
            main
        }
    }

    #[tokio::test]
    async fn a_push_of_the_principal_is_written_onto_the_tip_the_log_names() {
        let mut scene = Scene::new().await;
        let main = scene.tip_at_main().await;
        assert_eq!(scene.follow().await, Some(main));

        let pushed = scene.commit("pushed");
        scene.push("pushed");
        assert_eq!(scene.follow().await, Some(pushed));
        let state = scene.state().await;
        assert_eq!(state.target(), Some(pushed));
        let tip = state.target_tip().unwrap();
        assert_eq!(
            (tip.onto, tip.cause, tip.device),
            (main, MoveCause::Push, scene.fixture.devices[0].key())
        );
        // Written once.
        scene.follow().await;
        assert_eq!(scene.state().await.target_moves().count(), 2);
    }

    /// Nothing wakes the follower for a push: it reads the branch now and then, and writes it.
    #[tokio::test]
    async fn the_follower_sees_a_push_by_itself() {
        let mut scene = Scene::new().await;
        let main = scene.tip_at_main().await;
        assert_eq!(scene.follow().await, Some(main));
        let mut heads = scene.loaded.storage.watch_heads();
        let now = heads.borrow().clone();
        let mut followed = scene.loaded.target.followed.subscribe();
        followed
            .wait_for(|followed| *followed == now)
            .await
            .unwrap();
        let pushed = scene.commit("pushed");
        scene.push("pushed");

        let written = async {
            while scene.state().await.target() != Some(pushed) {
                heads.changed().await.unwrap();
            }
        };
        tokio::time::timeout(LOOK_FOR_PUSHES * 4, written)
            .await
            .expect("the push is written");
        assert_eq!(scene.state().await.target_tip().unwrap().onto, main);
    }

    #[tokio::test]
    async fn a_target_rewritten_past_the_tip_stays_there_and_is_reported_once() {
        let mut scene = Scene::new().await;
        let main = scene.tip_at_main().await;
        assert_eq!(scene.follow().await, Some(main));
        let rewritten = scene.rewrite();

        let pass = scene
            .daemon
            .follow_target(scene.fixture.workforce(), &scene.loaded)
            .await
            .unwrap();
        assert_eq!(
            pass,
            TargetAt::Unrelated {
                branch: rewritten,
                tip: main
            }
        );
        let found = scene.unrelated().expect("the daemon keeps it");
        assert_eq!((found.branch, found.tip), (rewritten, main));
        assert_eq!(scene.main(), rewritten, "the branch is not put back");
        assert_eq!(scene.state().await.target(), Some(main), "nothing written");

        assert_eq!(scene.follow().await, None);
        assert_eq!(
            scene.unrelated(),
            Some(found),
            "the same one, not a new one"
        );
    }

    #[tokio::test]
    async fn a_target_put_back_at_the_tip_is_no_longer_unrelated() {
        let mut scene = Scene::new().await;
        let main = scene.tip_at_main().await;
        scene.rewrite();
        assert_eq!(scene.follow().await, None);

        git_in(
            &scene.bare(),
            &["update-ref", "refs/heads/main", &main.to_string()],
        );
        assert_eq!(scene.follow().await, Some(main));
        assert_eq!(scene.unrelated(), None);
    }

    #[tokio::test]
    async fn an_adopted_rewrite_is_written_onto_the_tip_and_the_chain_goes_on_from_it() {
        let mut scene = Scene::new().await;
        let main = scene.tip_at_main().await;
        let rewritten = scene.rewrite();
        assert_eq!(scene.follow().await, None);

        scene.adopt(rewritten).await.unwrap();
        let state = scene.state().await;
        assert_eq!(state.target(), Some(rewritten));
        let tip = state.target_tip().unwrap();
        assert_eq!((tip.onto, tip.cause), (main, MoveCause::Push));
        assert_eq!(scene.unrelated(), None);
        assert_eq!(scene.follow().await, Some(rewritten));
        assert_eq!(
            scene.state().await.target_moves().count(),
            2,
            "written once"
        );
    }

    #[tokio::test]
    async fn only_the_commit_the_target_is_at_can_be_adopted_and_only_while_it_is_unrelated() {
        let mut scene = Scene::new().await;
        let main = scene.tip_at_main().await;
        assert!(matches!(
            scene.adopt(main).await,
            Err(SessionError::TargetNotUnrelated)
        ));

        let rewritten = scene.rewrite();
        let another = scene.commit("another");
        match scene.adopt(another).await {
            Err(SessionError::TargetElsewhere { branch }) => assert_eq!(branch, rewritten),
            other => panic!("{other:?}"),
        }
        assert_eq!(scene.state().await.target(), Some(main));
    }

    /// The other device has the old history: its branch goes to the rewrite by compare-and-swap,
    /// like any tip, and nothing there asks the principal again.
    #[tokio::test]
    async fn a_device_on_the_old_tip_follows_a_rewrite_adopted_elsewhere() {
        let mut scene = Scene::new().await;
        let main = scene.tip_at_main().await;
        assert_eq!(scene.follow().await, Some(main));
        let rewritten = scene.rewritten();
        let actor = scene.daemon_actor();
        scene.write(1, actor, target_moved(rewritten, main)).await;

        assert_eq!(scene.follow().await, Some(rewritten));
        assert_eq!(scene.main(), rewritten);
        assert_eq!(scene.unrelated(), None);
        assert_eq!(scene.state().await.target_moves().count(), 2);
    }

    #[tokio::test]
    async fn a_device_behind_the_old_tip_follows_a_rewrite_adopted_elsewhere() {
        let mut scene = Scene::new().await;
        let main = scene.tip_at_main().await;
        // Where the first move of the log started, which no move made.
        scene.fetch("before", "refs/heads/main");
        let rewritten = scene.rewritten();
        let actor = scene.daemon_actor();
        scene.write(1, actor, target_moved(rewritten, main)).await;

        assert_eq!(scene.follow().await, Some(rewritten));
        assert_eq!(scene.main(), rewritten);
    }

    /// A merge of another device that crossed the rewrite wins or loses like any fork. When it
    /// wins, the branch goes back to the old history, and the push that lost says so.
    #[tokio::test]
    async fn an_adopted_rewrite_that_lost_a_fork_is_a_lost_push() {
        let mut scene = Scene::new().await;
        let main = scene.tip_at_main().await;
        let winner = scene.commit("winner");
        let apart = scene.written_apart(&scene.state().await, target_moved(winner, main));
        let rewritten = scene.rewrite();
        scene.adopt(rewritten).await.unwrap();

        scene.receive(apart).await;
        assert_eq!(scene.follow().await, Some(winner));
        assert_eq!(scene.main(), winner);
        let lost: Vec<_> = scene
            .state()
            .await
            .lost_pushes(scene.fixture.founder.key())
            .iter()
            .map(|push| push.commit)
            .collect();
        assert_eq!(lost, [rewritten]);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_merge_on_an_unrelated_target_claims_nothing_and_starts_once_it_is_adopted() {
        let mut scene = Scene::new().await;
        let main = scene.tip_at_main().await;
        let rewritten = scene.rewrite();
        let bare = scene.bare();
        // Submitted on the rewritten history, as the sessions after the rewrite are.
        let reviewed = git_in(
            &bare,
            &[
                "-c",
                "user.name=agent",
                "-c",
                "user.email=agent@example.com",
                "commit-tree",
                "main^{tree}",
                "-p",
                "main",
                "-m",
                "work",
            ],
        );
        git_in(&bare, &["update-ref", "refs/test/reviewed", &reviewed]);
        let reviewed = CommitId::Git(reviewed.parse().unwrap());
        let (item, session) = scene.submitted(reviewed).await;

        scene
            .daemon
            .merge_item(
                scene.fixture.workforce(),
                &scene.loaded,
                MergeRequest {
                    item,
                    commit: reviewed,
                    because: None,
                    attempt: Attempt::First { session },
                },
            )
            .await
            .unwrap();
        let state = scene.state().await;
        let Some(wf_core::item::ItemEntry::Live(live)) = state.item(item) else {
            panic!("the item is live");
        };
        assert_eq!(live.merge_claims().count(), 0, "a merge that cannot start");
        assert_eq!(live.state(), ItemState::Review);
        assert_eq!(
            live.comments().len(),
            1,
            "the session that did the work hears"
        );
        assert_eq!(state.target(), Some(main));

        scene.adopt(rewritten).await.unwrap();
        let state = scene
            .wait_for("the dispatcher merges it", |state| {
                state.live_state(item) == Some(ItemState::Done)
            })
            .await;
        let tip = state.target_tip().unwrap();
        assert_eq!(
            (tip.onto, tip.cause),
            (rewritten, MoveCause::Merge { item }),
            "the merge went onto the rewrite"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn work_on_the_old_history_goes_back_to_a_session_once_the_rewrite_is_adopted() {
        let mut scene = Scene::new().await;
        let main = scene.tip_at_main().await;
        let reviewed = scene.commit("reviewed");
        let rewritten = scene.rewrite();
        assert_eq!(scene.follow().await, None);
        scene.adopt(rewritten).await.unwrap();

        let (item, _) = scene.submitted(reviewed).await;
        let state = scene
            .wait_for("the dispatcher sends it back", |state| {
                state.live_state(item) == Some(ItemState::Open)
            })
            .await;
        let Some(wf_core::item::ItemEntry::Live(live)) = state.item(item) else {
            panic!("the item is live");
        };
        assert_eq!(live.merges().count(), 0);
        assert_eq!(live.comments().len(), 1, "it says why");
        let comment = *live.comments().keys().next().unwrap();
        let said = scene
            .loaded
            .storage
            .with(move |replica| {
                wf_store::EventStore::event(replica.store(), &wf_core::id::EventId::from(comment))
                    .unwrap()
                    .unwrap()
            })
            .await
            .unwrap();
        let EventBody::Commented(said) = said.content.body else {
            panic!("a comment");
        };
        assert!(
            said.text
                .contains(&format!("`git rebase --onto {rewritten} ")),
            "an agent repository has no ref for the target: {}",
            said.text
        );
        assert_eq!(state.target(), Some(rewritten));
        assert_ne!(main, rewritten);
    }

    #[tokio::test]
    async fn the_branch_follows_the_tip_once_its_commit_is_here() {
        let mut scene = Scene::new().await;
        let main = scene.tip_at_main().await;
        let ahead = scene.repository.work("ahead", "ahead.txt", "x\n");
        let actor = scene.daemon_actor();
        scene.write(1, actor, target_moved(ahead, main)).await;
        assert_eq!(scene.follow().await, None, "not here yet");
        assert_eq!(scene.main(), main);

        scene.fetch("ahead", "refs/test/ahead");
        assert_eq!(scene.follow().await, Some(ahead));
        assert_eq!(scene.main(), ahead);
    }

    #[tokio::test]
    async fn a_push_that_lost_a_fork_is_reset_to_the_winner() {
        let mut scene = Scene::new().await;
        let main = scene.tip_at_main().await;
        let winner = scene.commit("winner");
        let apart = scene.written_apart(&scene.state().await, target_moved(winner, main));

        scene.commit("pushed");
        scene.push("pushed");
        scene.follow().await;
        scene.receive(apart.clone()).await;
        assert_eq!(scene.follow().await, Some(winner));
        assert_eq!(scene.main(), winner);
        let state = scene.state().await;
        assert_eq!(state.target(), Some(winner));
        let lost: Vec<_> = state
            .lost_moves(scene.fixture.devices[0].key())
            .map(|step| step.cause)
            .collect();
        assert_eq!(lost, vec![MoveCause::Push]);
    }

    /// The principal pushes after the merge found the target at the tip, and before it reads the
    /// target to merge onto, while nothing wakes the follower to write the push down.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_push_just_before_a_merge_is_written_and_the_merge_goes_onto_it() {
        let mut scene = Scene::new().await;
        let main = scene.tip_at_main().await;
        assert_eq!(scene.follow().await, Some(main));
        let pushed = scene.repository.work("pushed", "pushed.txt", "x\n");
        let (source, bare) = (scene.repository.directory().to_path_buf(), scene.bare());
        let loaded = Arc::clone(&scene.loaded);
        *scene.daemon.before_merge_tree.lock().unwrap() = Some(Box::new(move || {
            // The follower's pass for the last write ends first.
            let heads = loaded.storage.watch_heads().borrow().clone();
            let mut followed = loaded.target.followed.subscribe();
            tokio::task::block_in_place(|| {
                tokio::runtime::Handle::current().block_on(async {
                    followed
                        .wait_for(|followed| *followed == heads)
                        .await
                        .unwrap();
                })
            });
            let source = source.display().to_string();
            git_in(
                &bare,
                &["fetch", "--quiet", &source, "+pushed:refs/heads/main"],
            );
        }));

        let founder = scene.fixture.founder_actor();
        scene.write(0, founder.clone(), define_agent("coder")).await;
        let item: ItemId = scene
            .write(0, founder.clone(), create("task", "work"))
            .await
            .id()
            .into();
        let session = scene
            .write(0, founder.clone(), start_session("coder", item))
            .await
            .id()
            .into();
        scene
            .write(0, founder.clone(), moved(item, ItemState::Review))
            .await;
        let reviewed = scene.commit("reviewed");
        // Claimed already, so that the merge writes nothing before it reads the target.
        let daemon = scene.daemon_actor();
        scene.write(0, daemon, merge_claimed(item, reviewed)).await;
        scene
            .write(0, founder, submit_session(session, reviewed))
            .await;

        let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
        let state = loop {
            let state = scene.state().await;
            if state.live_state(item) == Some(ItemState::Done) {
                break state;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "the dispatcher merges it"
            );
            tokio::time::sleep(Duration::from_millis(50)).await;
        };
        let steps: Vec<_> = state
            .target_moves()
            .map(|step| (step.onto, step.cause))
            .collect();
        assert!(
            steps.contains(&(main, MoveCause::Push)),
            "the push is written"
        );
        let tip = state.target_tip().unwrap();
        assert_eq!(
            (tip.onto, tip.cause),
            (pushed, MoveCause::Merge { item }),
            "the merge went onto the push"
        );
        assert!(state.lost_pushes(scene.fixture.founder.key()).is_empty());
        assert_eq!(scene.main(), tip.commit);
    }

    #[tokio::test]
    async fn an_item_whose_merge_lost_a_fork_goes_back_to_review() {
        let mut scene = Scene::new().await;
        let main = scene.tip_at_main().await;
        let founder = scene.fixture.founder_actor();
        scene.write(0, founder.clone(), define_agent("coder")).await;
        let item: ItemId = scene
            .write(0, founder.clone(), create("task", "work"))
            .await
            .id()
            .into();
        let session = scene
            .write(0, founder.clone(), start_session("coder", item))
            .await
            .id()
            .into();
        scene
            .write(0, founder.clone(), moved(item, ItemState::Review))
            .await;
        // Not here, so that the dispatcher does not merge it again behind the test's back.
        let reviewed = CommitId::Git("00112233445566778899aabbccddeeff00112233".parse().unwrap());
        scene
            .write(0, founder.clone(), submit_session(session, reviewed))
            .await;
        let winner = scene.commit("winner");
        let apart = scene.written_apart(&scene.state().await, target_moved(winner, main));

        // This device merged it, as the merge task does: holding the branch until it is written.
        let merge = scene.commit("merge");
        let loaded = Arc::clone(&scene.loaded);
        let mut seen = loaded.target.seen.lock().await;
        scene.push("merge");
        let daemon = scene.daemon_actor();
        scene
            .write(0, daemon, merged_onto(item, reviewed, merge, main))
            .await;
        *seen = Some(merge);
        drop(seen);
        scene.write(0, founder, moved(item, ItemState::Done)).await;
        assert_eq!(scene.follow().await, Some(merge));
        assert_eq!(scene.state().await.live_state(item), Some(ItemState::Done));

        scene.receive(apart).await;
        assert_eq!(scene.follow().await, Some(winner));
        assert_eq!(scene.main(), winner);
        let state = scene.state().await;
        assert_eq!(state.live_state(item), Some(ItemState::Review));
        let own = scene.fixture.devices[0].key();
        assert!(
            state
                .lost_merges(own, &scene.daemon.presence_now(), wf_store::now_ms())
                .is_empty()
        );
        assert!(
            state
                .pending_merges()
                .iter()
                .any(|pending| pending.item == item && pending.commit == reviewed)
        );
    }

    #[tokio::test]
    async fn a_lost_merge_of_a_device_that_is_gone_goes_back_to_review_here() {
        let mut scene = Scene::new().await;
        let main = scene.tip_at_main().await;
        let founder = scene.fixture.founder_actor();
        scene.write(0, founder.clone(), define_agent("coder")).await;
        let item: ItemId = scene
            .write(0, founder.clone(), create("task", "work"))
            .await
            .id()
            .into();
        let session = scene
            .write(0, founder.clone(), start_session("coder", item))
            .await
            .id()
            .into();
        scene
            .write(0, founder.clone(), moved(item, ItemState::Review))
            .await;
        let reviewed = CommitId::Git("00112233445566778899aabbccddeeff00112233".parse().unwrap());
        scene
            .write(0, founder.clone(), submit_session(session, reviewed))
            .await;
        let winner = scene.commit("winner");
        let apart = scene.written_apart(&scene.state().await, target_moved(winner, main));
        // Device 1 merged it onto the same tip later, and was not heard from since.
        let lost = CommitId::Git("33221100ffeeddccbbaa99887766554433221100".parse().unwrap());
        let daemon = scene.daemon_actor();
        scene
            .write(1, daemon, merged_onto(item, reviewed, lost, main))
            .await;
        scene.write(1, founder, moved(item, ItemState::Done)).await;
        scene.receive(apart).await;
        assert_eq!(scene.follow().await, Some(winner));
        assert_eq!(
            scene.state().await.live_state(item),
            Some(ItemState::Done),
            "device 1 may still send it back itself"
        );

        let lease = scene.state().await.lease();
        scene
            .daemon
            .backdate_presence(lease + Duration::from_secs(60));
        assert_eq!(scene.follow().await, Some(winner));
        assert_eq!(
            scene.state().await.live_state(item),
            Some(ItemState::Review)
        );
    }

    async fn hold(
        scene: &Scene,
        principal: PrincipalKey,
        item: ItemId,
        reviewed: CommitId,
        claiming: Claiming,
    ) -> bool {
        scene
            .daemon
            .hold_merge(
                scene.fixture.workforce(),
                &scene.loaded,
                principal,
                item,
                reviewed,
                claiming,
            )
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn the_earliest_claim_of_a_device_that_is_there_holds_a_merge() {
        let mut scene = Scene::new().await;
        let founder = scene.fixture.founder_actor();
        let item: ItemId = scene
            .write(0, founder, create("task", "work"))
            .await
            .id()
            .into();
        let reviewed = scene.repository.main();
        let principal = scene.fixture.founder.key();
        assert!(!hold(&scene, principal, item, reviewed, Claiming::Keep).await);
        let daemon = scene.daemon_actor();
        scene.write(1, daemon, merge_claimed(item, reviewed)).await;
        assert!(
            !hold(&scene, principal, item, reviewed, Claiming::Claim).await,
            "device 1 claimed it first, and is there"
        );

        let lease = scene.state().await.lease();
        scene
            .daemon
            .backdate_presence(lease + Duration::from_secs(60));
        assert!(hold(&scene, principal, item, reviewed, Claiming::Claim).await);
        let state = scene.state().await;
        let claims: Vec<_> = match state.item(item) {
            Some(wf_core::item::ItemEntry::Live(live)) => {
                live.merge_claims().map(|claim| claim.device).collect()
            }
            _ => panic!("the item is live"),
        };
        assert_eq!(
            claims.len(),
            2,
            "device 0 claimed it once device 1 was gone"
        );
        assert!(hold(&scene, principal, item, reviewed, Claiming::Claim).await);
    }

    #[tokio::test]
    async fn a_device_claims_no_merge_before_it_has_the_commit() {
        let mut scene = Scene::new().await;
        let founder = scene.fixture.founder_actor();
        let item: ItemId = scene
            .write(0, founder, create("task", "work"))
            .await
            .id()
            .into();
        let principal = scene.fixture.founder.key();
        let reviewed = scene.repository.work("reviewed", "reviewed.txt", "x\n");
        assert!(!hold(&scene, principal, item, reviewed, Claiming::Claim).await);
        let claims = |state: &State| match state.item(item) {
            Some(wf_core::item::ItemEntry::Live(live)) => live.merge_claims().count(),
            _ => panic!("the item is live"),
        };
        assert_eq!(claims(&scene.state().await), 0, "not claimed");

        scene.fetch("reviewed", "refs/test/reviewed");
        assert!(hold(&scene, principal, item, reviewed, Claiming::Claim).await);
        assert_eq!(claims(&scene.state().await), 1);
    }
}
