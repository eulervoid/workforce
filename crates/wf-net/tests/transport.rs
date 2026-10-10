//! Real iroh endpoints in one process over loopback, with relays and address lookup off.

use wf_core::identity::DeviceSigner;
use wf_core::testing::*;
use wf_net::{Network, Node, SharedReplica};
use wf_store::{RedbStore, Replica};

fn replica(fixture: &Fixture) -> SharedReplica<RedbStore> {
    let mut replica = Replica::open(RedbStore::in_memory().unwrap(), fixture.workforce()).unwrap();
    replica.receive(fixture.genesis.clone()).unwrap();
    SharedReplica::new(replica)
}

fn write(replica: &SharedReplica<RedbStore>, fixture: &Fixture, device: usize, title: &str) {
    let now = 2_000 + replica.lock().state().event_count() as u64;
    replica
        .lock()
        .author(
            &fixture.devices[device],
            fixture.founder_actor(),
            now,
            create("task", title),
        )
        .unwrap();
}

async fn node(
    fixture: &Fixture,
    signer: &DeviceSigner,
    replica: &SharedReplica<RedbStore>,
) -> Node<SharedReplica<RedbStore>> {
    let node = Node::start(signer, Network::Direct).await.unwrap();
    node.serve(fixture.workforce(), replica.clone());
    node
}

#[tokio::test]
async fn two_endpoints_exchange_their_writes() {
    let fixture = Fixture::new(3);
    let (first, second, third) = (replica(&fixture), replica(&fixture), replica(&fixture));
    write(&first, &fixture, 0, "from the laptop");
    write(&second, &fixture, 1, "from the vm");
    let laptop = node(&fixture, &fixture.devices[0], &first).await;
    let vm = node(&fixture, &fixture.devices[1], &second).await;
    let desktop = node(&fixture, &fixture.devices[2], &third).await;

    let report = laptop
        .sync_with(fixture.workforce(), vm.local_addr())
        .await
        .unwrap();
    assert_eq!((report.applied, report.sent), (1, 1));
    desktop
        .sync_with(fixture.workforce(), laptop.local_addr())
        .await
        .unwrap();

    let state = |replica: &SharedReplica<RedbStore>| replica.lock().state().clone();
    assert_eq!(state(&first), state(&second));
    assert_eq!(state(&first), state(&third));
    assert_eq!(state(&third).items().count(), 2);

    for node in [laptop, vm, desktop] {
        node.shutdown().await;
    }
}

#[tokio::test]
async fn devices_that_are_not_bound_are_refused() {
    let fixture = Fixture::new(1);
    let served = replica(&fixture);
    write(&served, &fixture, 0, "private");
    let member = node(&fixture, &fixture.devices[0], &served).await;

    let stranger_signer = DeviceSigner::from_bytes(&[200; 32]).unwrap();
    let stranger_replica = replica(&fixture);
    let stranger = node(&fixture, &stranger_signer, &stranger_replica).await;
    let result = stranger
        .sync_with(fixture.workforce(), member.local_addr())
        .await;
    assert!(result.is_err());
    assert_eq!(stranger_replica.lock().state().event_count(), 1);

    stranger.shutdown().await;
    member.shutdown().await;
}

#[tokio::test]
async fn an_unserved_workforce_is_refused() {
    let fixture = Fixture::new(2);
    let first = replica(&fixture);
    let second = replica(&fixture);
    let laptop = node(&fixture, &fixture.devices[0], &first).await;
    let vm = Node::start(&fixture.devices[1], Network::Direct)
        .await
        .unwrap();
    vm.serve(fixture.workforce(), second);
    let other = wf_core::id::WorkforceId::from(wf_core::id::EventId::of(b"other"));
    assert!(laptop.sync_with(other, vm.local_addr()).await.is_err());
    laptop
        .sync_with(fixture.workforce(), vm.local_addr())
        .await
        .unwrap();
    laptop.shutdown().await;
    vm.shutdown().await;
}

mod joining {
    use wf_core::event::DeviceProfile;
    use wf_core::identity::InviteSigner;
    use wf_core::invite::{InviteCode, JoinRefusal};
    use wf_net::replica::bind_device;

    use super::*;

    struct Invited {
        member: Node<SharedReplica<RedbStore>>,
        member_replica: SharedReplica<RedbStore>,
        code: InviteCode,
    }

    async fn invited(fixture: &Fixture, expires_ms: u64) -> Invited {
        let member_replica = replica(fixture);
        write(&member_replica, fixture, 0, "existing work");
        let (event, secret) = {
            let replica = member_replica.lock();
            fixture.invite(replica.state(), 5_000, expires_ms)
        };
        member_replica.lock().receive(event.clone()).unwrap();
        let member = node(fixture, &fixture.devices[0], &member_replica).await;
        let code = InviteCode::new(
            fixture.workforce(),
            event.id(),
            &secret,
            vec![member.device()],
        );
        Invited {
            member,
            member_replica,
            code,
        }
    }

    fn far_future() -> u64 {
        u64::MAX / 2
    }

    fn profile() -> DeviceProfile {
        DeviceProfile {
            name: "vm".into(),
            hardware: "test".into(),
            agents: Default::default(),
            max_sessions: 1,
        }
    }

    fn local_replica(fixture: &Fixture) -> SharedReplica<RedbStore> {
        SharedReplica::new(
            Replica::open(RedbStore::in_memory().unwrap(), fixture.workforce()).unwrap(),
        )
    }

    /// Joins with the log in `local`. With `stop_after_binding` the join fails once the events of
    /// the new device are written and before they are sent, like a connection that drops there.
    async fn join_into(
        invited: &Invited,
        code: &InviteCode,
        signer: &DeviceSigner,
        local: &SharedReplica<RedbStore>,
        stop_after_binding: bool,
    ) -> Result<(), wf_net::NetError> {
        let joiner = Node::start(signer, Network::Direct).await.unwrap();
        let author_replica = local.clone();
        let invite = code.invite;
        let proof = code.prove(signer.key()).unwrap();
        let signer_for_author = signer.clone();
        let result = joiner
            .join(
                code,
                invited.member.local_addr(),
                local,
                move || async move {
                    bind_device(
                        &mut author_replica.lock(),
                        &signer_for_author,
                        invite,
                        proof,
                        profile(),
                        9_000,
                    )?;
                    if stop_after_binding {
                        return Err("the connection dropped".into());
                    }
                    Ok(())
                },
            )
            .await;
        joiner.shutdown().await;
        result
    }

    async fn join(
        fixture: &Fixture,
        invited: &Invited,
        code: &InviteCode,
        signer: &DeviceSigner,
    ) -> (Result<(), wf_net::NetError>, SharedReplica<RedbStore>) {
        let local = local_replica(fixture);
        let result = join_into(invited, code, signer, &local, false).await;
        (result, local)
    }

    #[tokio::test]
    async fn a_device_joins_and_the_member_learns_of_it() {
        let fixture = Fixture::new(1);
        let invited = invited(&fixture, far_future()).await;
        let vm = DeviceSigner::from_bytes(&[90; 32]).unwrap();

        let (result, local) = join(&fixture, &invited, &invited.code, &vm).await;
        result.unwrap();

        let state = local.lock().state().clone();
        assert_eq!(state.principal_of(&vm.key()), Some(fixture.founder.key()));
        assert_eq!(state.items().count(), 1);
        let member_state = invited.member_replica.lock().state().clone();
        assert_eq!(
            member_state.principal_of(&vm.key()),
            Some(fixture.founder.key())
        );
        assert_eq!(member_state.event_count(), state.event_count());
        invited.member.shutdown().await;
    }

    #[tokio::test]
    async fn an_invite_works_once() {
        let fixture = Fixture::new(1);
        let invited = invited(&fixture, far_future()).await;
        let first = DeviceSigner::from_bytes(&[91; 32]).unwrap();
        let second = DeviceSigner::from_bytes(&[92; 32]).unwrap();
        join(&fixture, &invited, &invited.code, &first)
            .await
            .0
            .unwrap();
        let (result, local) = join(&fixture, &invited, &invited.code, &second).await;
        assert!(matches!(
            result,
            Err(wf_net::NetError::JoinRefused {
                reason: JoinRefusal::Redeemed,
                ..
            })
        ));
        assert_eq!(local.lock().state().event_count(), 0);
        invited.member.shutdown().await;
    }

    #[tokio::test]
    async fn a_join_that_stopped_halfway_can_be_run_again_by_the_same_device() {
        let fixture = Fixture::new(1);
        let invited = invited(&fixture, far_future()).await;
        let vm = DeviceSigner::from_bytes(&[95; 32]).unwrap();
        let local = local_replica(&fixture);

        let result = join_into(&invited, &invited.code, &vm, &local, true).await;
        assert!(matches!(result, Err(wf_net::NetError::JoinAuthoring(_))));
        assert!(local.lock().state().principal_of(&vm.key()).is_some());
        assert!(
            invited
                .member_replica
                .lock()
                .state()
                .principal_of(&vm.key())
                .is_none(),
            "the member has not heard of the device"
        );

        // Someone else cannot take the invite in the meantime.
        let other = DeviceSigner::from_bytes(&[96; 32]).unwrap();
        let (taken, _) = join(&fixture, &invited, &invited.code, &other).await;
        assert!(matches!(
            taken,
            Err(wf_net::NetError::JoinRefused {
                reason: JoinRefusal::Reserved,
                ..
            })
        ));

        let events_before = local.lock().state().event_count();
        join_into(&invited, &invited.code, &vm, &local, false)
            .await
            .unwrap();
        assert_eq!(local.lock().state().event_count(), events_before);
        let member_state = invited.member_replica.lock().state().clone();
        assert_eq!(
            member_state.principal_of(&vm.key()),
            Some(fixture.founder.key())
        );
        assert_eq!(member_state.event_count(), events_before);
        assert_eq!(member_state.equivocations().count(), 0);
        invited.member.shutdown().await;
    }

    #[tokio::test]
    async fn an_expired_invite_is_refused() {
        let fixture = Fixture::new(1);
        let invited = invited(&fixture, 6_000).await;
        let vm = DeviceSigner::from_bytes(&[93; 32]).unwrap();
        let (result, _) = join(&fixture, &invited, &invited.code, &vm).await;
        assert!(matches!(
            result,
            Err(wf_net::NetError::JoinRefused {
                reason: JoinRefusal::Expired,
                ..
            })
        ));
        invited.member.shutdown().await;
    }

    #[tokio::test]
    async fn a_code_with_the_wrong_secret_is_refused() {
        let fixture = Fixture::new(1);
        let invited = invited(&fixture, far_future()).await;
        let forged = InviteCode::new(
            fixture.workforce(),
            invited.code.invite,
            &InviteSigner::generate(),
            invited.code.bootstrap.clone(),
        );
        let vm = DeviceSigner::from_bytes(&[94; 32]).unwrap();
        let (result, _) = join(&fixture, &invited, &forged, &vm).await;
        assert!(matches!(
            result,
            Err(wf_net::NetError::JoinRefused {
                reason: JoinRefusal::BadProof,
                ..
            })
        ));
        invited.member.shutdown().await;
    }
}

mod gossip {
    use std::collections::BTreeSet;
    use std::time::Duration;

    use wf_core::id::EventId;
    use wf_net::heads_digest;

    use super::*;

    #[tokio::test]
    async fn devices_hear_each_others_heads() {
        let fixture = Fixture::new(2);
        let (first, second) = (replica(&fixture), replica(&fixture));
        let laptop = node(&fixture, &fixture.devices[0], &first).await;
        let vm = node(&fixture, &fixture.devices[1], &second).await;
        laptop.remember(vm.local_addr());
        vm.remember(laptop.local_addr());

        let workforce = fixture.workforce();
        let laptop_topic = laptop
            .heads_topic(&workforce, &[vm.device()])
            .await
            .unwrap();
        let mut vm_topic = vm
            .heads_topic(&workforce, &[laptop.device()])
            .await
            .unwrap();

        let heads = BTreeSet::from([EventId::of(b"head")]);
        let heard = tokio::time::timeout(Duration::from_secs(20), async {
            loop {
                laptop_topic.announce(&heads).await.unwrap();
                if let Ok(Some(heard)) =
                    tokio::time::timeout(Duration::from_millis(500), vm_topic.heard.recv()).await
                {
                    return heard;
                }
            }
        })
        .await
        .expect("the announcement arrives");
        assert_eq!(heard.device, laptop.device());
        assert_eq!(heard.digest, heads_digest(&heads));

        laptop.shutdown().await;
        vm.shutdown().await;
    }

    #[tokio::test]
    async fn gossip_from_devices_that_are_not_bound_is_refused() {
        let fixture = Fixture::new(1);
        let served = replica(&fixture);
        let member = node(&fixture, &fixture.devices[0], &served).await;
        let stranger_signer = DeviceSigner::from_bytes(&[201; 32]).unwrap();
        let stranger = node(&fixture, &stranger_signer, &replica(&fixture)).await;
        stranger.remember(member.local_addr());
        member.remember(stranger.local_addr());

        let workforce = fixture.workforce();
        let mut member_topic = member.heads_topic(&workforce, &[]).await.unwrap();
        let stranger_topic = stranger
            .heads_topic(&workforce, &[member.device()])
            .await
            .unwrap();
        let heads = BTreeSet::from([EventId::of(b"head")]);
        for _ in 0..6 {
            stranger_topic.announce(&heads).await.unwrap();
            let heard =
                tokio::time::timeout(Duration::from_millis(400), member_topic.heard.recv()).await;
            assert!(heard.is_err(), "nothing gets through from a stranger");
        }
        stranger.shutdown().await;
        member.shutdown().await;
    }
}
