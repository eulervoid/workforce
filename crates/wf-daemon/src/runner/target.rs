//! Keeping the target branch of `<wf>/git` where the log says it is (SPEC §10). A push of the
//! principal is written as `TargetMoved` onto the tip the log names; the branch follows the end of
//! the chain, forward or, after a move of this device lost a fork, back to the winner; and an item
//! whose merge lost goes back to `review` to be merged again.

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

/// How often the target branch is read for a push of the principal, which nothing announces.
const LOOK_FOR_PUSHES: Duration = Duration::from_secs(5);

/// Where this daemon last saw the target branch of `<wf>/git`. Held while the branch moves and
/// while the event that says so is written, so that a merge of this daemon is never taken for a
/// push, nor a push for a merge.
#[derive(Default)]
pub(crate) struct TargetBranch {
    pub(super) seen: tokio::sync::Mutex<Option<CommitId>>,
}

/// Whether the target branch of `<wf>/git` is where the log says after a pass.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum TargetAt {
    /// At the end of the chain, or the log names no tip yet: a merge may start on it.
    Tip,
    /// Elsewhere: the tip is still to arrive, or a push was just written and the log has not
    /// settled on it. A merge waits.
    Elsewhere,
}

/// What a pass does with the target branch.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Follow {
    /// It is where the log says.
    Stays,
    /// It cannot follow yet: the tip has not arrived, or it is unrelated to the tip.
    Waits,
    /// The principal pushed: write the move.
    Record { commit: CommitId, onto: CommitId },
    /// Forward to the tip, or back to it from a move that lost a fork.
    Moves { from: CommitId, to: CommitId },
}

/// What a pass reads from the log.
struct Reading {
    target: String,
    tip: Option<CommitId>,
    /// Every commit a move of the target made, on the chain or not.
    made: BTreeSet<CommitId>,
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
        made: state.target_moves().map(|step| step.commit).collect(),
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
        Some(tip) if tip == branch => Follow::Stays,
        Some(tip) if !git.has_commit(tip)? => Follow::Waits,
        // A move of the log, or behind its tip: the branch goes to the tip, forward or back.
        Some(tip) if reading.made.contains(&branch) || git.is_ancestor(branch, tip)? => {
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
            None => Follow::Waits,
        },
        // No move named a tip yet: a push is one from where this daemon last saw the branch.
        None => match seen {
            Some(seen) if seen != branch && !reading.made.contains(&branch) => Follow::Record {
                commit: branch,
                onto: seen,
            },
            Some(_) | None => Follow::Stays,
        },
    };
    Ok((Some(branch), follow))
}

/// Follows the target of a workforce for as long as the daemon runs and its storage does: when
/// the log changes, and now and then for pushes.
pub(crate) async fn run_target(daemon: Weak<Daemon>, workforce: WorkforceId, loaded: Arc<Loaded>) {
    let Some(mut halted) = daemon.upgrade().map(|strong| strong.halted.subscribe()) else {
        return;
    };
    let mut heads = loaded.storage.watch_heads();
    loop {
        let Some(strong) = daemon.upgrade() else {
            return;
        };
        if let Err(error) = strong.follow_target(workforce, &loaded).await {
            tracing::warn!(
                "could not follow the target branch: {}",
                wf_core::error_chain(&error)
            );
        }
        drop(strong);
        tokio::select! {
            changed = heads.changed() => {
                if changed.is_err() {
                    return;
                }
                heads.borrow_and_update();
            }
            () = async { let _ = halted.wait_for(|halted| *halted).await; } => return,
            () = tokio::time::sleep(LOOK_FOR_PUSHES) => {}
        }
    }
}

impl Daemon {
    /// One pass: writes a push of the principal as a move of the target, moves the branch to the
    /// end of the chain, and sends the items whose merge lost a fork back to `review`.
    pub(super) async fn follow_target(
        &self,
        workforce: WorkforceId,
        loaded: &Loaded,
    ) -> Result<TargetAt, SessionError> {
        let own = self.device();
        let repository = self.bare_repository(workforce);
        if !repository.join("HEAD").is_file() {
            return Ok(TargetAt::Elsewhere);
        }
        let mut seen = loaded.target.seen.lock().await;
        // A push written in the first round is settled by the log in the second.
        let mut at = TargetAt::Elsewhere;
        for _ in 0..2 {
            let (presence, now_ms) = (self.presence_now(), wf_store::now_ms());
            let reading = self
                .read(loaded, move |state| read(state, own, &presence, now_ms))
                .await?;
            let Some(reading) = reading else {
                return Ok(TargetAt::Elsewhere);
            };
            let (git_repository, last) = (repository.clone(), *seen);
            let ((branch, follow), reading) = blocking(move || {
                let git = GitCli::open(&git_repository)?;
                Ok::<_, VcsError>((decide(&git, &reading, last)?, reading))
            })
            .await??;
            *seen = branch.or(*seen);
            self.send_back_lost_merges(loaded, &reading).await?;
            match follow {
                Follow::Stays => return Ok(TargetAt::Tip),
                Follow::Waits => return Ok(TargetAt::Elsewhere),
                Follow::Moves { from, to } => {
                    let (repository, target) = (repository.clone(), reading.target.clone());
                    let moved =
                        blocking(move || GitCli::open(&repository)?.move_target(&target, from, to))
                            .await??;
                    if moved {
                        *seen = Some(to);
                        return Ok(TargetAt::Tip);
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

    /// Moves each item whose merge lost a fork back to `review`, if this device made it or its
    /// maker is gone, with a comment that says why. There it waits for its merge again, which runs its checks again.
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

    use super::super::merge::Claiming;
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
            let daemon = Daemon::start(
                Launch {
                    data_dir: DataDir::new(directory.path().to_path_buf()),
                    socket: directory.path().join("daemon.sock"),
                    network: Network::Direct,
                    harnesses: Default::default(),
                    machine_config: None,
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
            self.daemon.snapshot(&self.loaded).await.unwrap()
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

        async fn follow(&self) -> TargetAt {
            self.daemon
                .follow_target(self.fixture.workforce(), &self.loaded)
                .await
                .unwrap()
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
        assert_eq!(scene.follow().await, TargetAt::Tip);

        let pushed = scene.commit("pushed");
        scene.push("pushed");
        assert_eq!(scene.follow().await, TargetAt::Tip);
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

    #[tokio::test]
    async fn the_branch_follows_the_tip_once_its_commit_is_here() {
        let mut scene = Scene::new().await;
        let main = scene.tip_at_main().await;
        let ahead = scene.repository.work("ahead", "ahead.txt", "x\n");
        let actor = scene.daemon_actor();
        scene.write(1, actor, target_moved(ahead, main)).await;
        assert_eq!(scene.follow().await, TargetAt::Elsewhere, "not here yet");
        assert_eq!(scene.main(), main);

        scene.fetch("ahead", "refs/test/ahead");
        assert_eq!(scene.follow().await, TargetAt::Tip);
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
        assert_eq!(scene.follow().await, TargetAt::Tip);
        assert_eq!(scene.main(), winner);
        let state = scene.state().await;
        assert_eq!(state.target(), Some(winner));
        let lost: Vec<_> = state
            .lost_moves(scene.fixture.devices[0].key())
            .map(|step| step.cause)
            .collect();
        assert_eq!(lost, vec![MoveCause::Push]);
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
        assert_eq!(scene.follow().await, TargetAt::Tip);
        assert_eq!(scene.state().await.live_state(item), Some(ItemState::Done));

        scene.receive(apart).await;
        assert_eq!(scene.follow().await, TargetAt::Tip);
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
        assert_eq!(scene.follow().await, TargetAt::Tip);
        assert_eq!(
            scene.state().await.live_state(item),
            Some(ItemState::Done),
            "device 1 may still send it back itself"
        );

        let lease = scene.state().await.lease();
        scene
            .daemon
            .backdate_presence(lease + Duration::from_secs(60));
        assert_eq!(scene.follow().await, TargetAt::Tip);
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
            .hold_merge(&scene.loaded, principal, item, reviewed, claiming)
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
}
