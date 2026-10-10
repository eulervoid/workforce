use std::str::FromStr;

use crate::event::{DeviceBound, Event, EventBody};
use crate::hlc::Hlc;
use crate::identity::{DeviceSigner, InviteSigner};
use crate::invite::{InviteCode, JoinRefusal};
use crate::state::{Disposition, State, Trust};
use crate::testing::*;
use crate::validate::Invalid;

fn receive(state: &mut State, event: Event) -> Result<Disposition, Invalid> {
    state
        .receive(event, Trust::Untrusted)
        .map(|received| received.disposition)
}

fn joiner(seed: u8) -> DeviceSigner {
    DeviceSigner::from_bytes(&[seed; 32]).unwrap()
}

#[test]
fn a_device_joins_with_an_invite_and_its_events_are_admitted() {
    let fixture = Fixture::new(1);
    let mut state = fixture.state();
    let (invite, secret) = fixture.invite(&state, 2_000, 10_000);
    receive(&mut state, invite.clone()).unwrap();
    let vm = joiner(60);
    assert!(state.principal_of(&vm.key()).is_none());

    let bound = fixture.bind(&state, &vm, &invite, &secret, 2_001);
    assert_eq!(receive(&mut state, bound), Ok(Disposition::Applied));
    assert_eq!(state.principal_of(&vm.key()), Some(fixture.founder.key()));
    assert!(
        state
            .member(&fixture.founder.key())
            .unwrap()
            .devices
            .contains(&vm.key())
    );

    let item = state
        .builder(vm.key(), fixture.founder_actor(), 2_002)
        .unwrap()
        .sign(&vm, create("task", "from the vm"))
        .unwrap();
    assert_eq!(receive(&mut state, item), Ok(Disposition::Applied));
}

#[test]
fn an_unbound_device_cannot_write_anything_else() {
    let fixture = Fixture::new(1);
    let mut state = fixture.state();
    let stranger = joiner(61);
    let event = state
        .builder(stranger.key(), fixture.founder_actor(), 2_000)
        .unwrap()
        .sign(&stranger, create("task", "x"))
        .unwrap();
    assert_eq!(
        receive(&mut state, event),
        Err(Invalid::UnknownDevice(stranger.key()))
    );
}

#[test]
fn binding_rules() {
    let fixture = Fixture::new(1);
    let mut state = fixture.state();
    let (invite, secret) = fixture.invite(&state, 2_000, 10_000);
    receive(&mut state, invite.clone()).unwrap();
    let vm = joiner(62);
    let other = joiner(63);

    let base = state.clone();
    let forged = |signer: &DeviceSigner, edit: &dyn Fn(&mut crate::event::EventContent)| {
        let mut content = fixture.bind(&base, signer, &invite, &secret, 2_001).content;
        edit(&mut content);
        Event::sign_content(content, signer).unwrap()
    };

    // A proof made for another device.
    let proof_for_other = secret
        .prove(fixture.workforce(), invite.id(), other.key())
        .unwrap();
    let wrong_device = forged(&vm, &|content| {
        content.body = EventBody::DeviceBound(DeviceBound {
            invite: invite.id(),
            proof: proof_for_other,
        })
    });
    assert_eq!(
        receive(&mut state, wrong_device),
        Err(Invalid::BadInviteProof)
    );

    // A proof from a key that is not the invite's.
    let impostor = InviteSigner::generate();
    let wrong_key = forged(&vm, &|content| {
        content.body = EventBody::DeviceBound(DeviceBound {
            invite: invite.id(),
            proof: impostor
                .prove(fixture.workforce(), invite.id(), vm.key())
                .unwrap(),
        })
    });
    assert_eq!(receive(&mut state, wrong_key), Err(Invalid::BadInviteProof));

    // The invite is not among the parents.
    let detached = forged(&vm, &|content| {
        content.deps = vec![fixture.genesis.id()];
    });
    assert_eq!(
        receive(&mut state, detached),
        Err(Invalid::InviteNotInParents)
    );

    // The invite does not exist.
    let missing = forged(&vm, &|content| {
        let EventBody::DeviceBound(bound) = &mut content.body else {
            unreachable!()
        };
        bound.invite = fixture.genesis.id();
        content.deps = vec![fixture.genesis.id()];
    });
    assert_eq!(
        receive(&mut state, missing),
        Err(Invalid::UnknownInvite(fixture.genesis.id()))
    );

    // Not the first event of the device.
    let late = forged(&vm, &|content| {
        content.seq = 2;
        content.prev = Some(invite.id());
    });
    assert_eq!(
        receive(&mut state, late),
        Err(Invalid::PrevMismatch { author: vm.key() })
    );
    let late_with_prev = {
        let first = fixture.bind(&state, &vm, &invite, &secret, 2_001);
        let mut after = state.clone();
        receive(&mut after, first.clone()).unwrap();
        let mut content = fixture.bind(&after, &vm, &invite, &secret, 2_002).content;
        content.hlc = Hlc::next_after(first.content.hlc, 2_002);
        (after, Event::sign_content(content, &vm).unwrap())
    };
    let (mut after, second) = late_with_prev;
    assert_eq!(receive(&mut after, second), Err(Invalid::BindingNotFirst));

    // Claiming to act for someone else.
    let impersonation = forged(&vm, &|content| {
        content.actor = crate::identity::Actor::Human(fixture.member.key())
    });
    assert_eq!(
        receive(&mut state, impersonation),
        Err(Invalid::ActorMismatch)
    );
}

#[test]
fn only_members_can_publish_invites() {
    let fixture = Fixture::new(1);
    let mut state = fixture.state();
    let (invite, _) = fixture.invite(&state, 2_000, 10_000);
    let EventBody::InviteCreated(signed) = &invite.content.body else {
        unreachable!()
    };
    let outsider = crate::identity::PrincipalSigner::generate();
    let forged_body = EventBody::InviteCreated(
        crate::identity::Signed::sign(
            &outsider,
            Some(fixture.workforce()),
            signed.statement.clone(),
        )
        .unwrap(),
    );
    let event = fixture.author(&state, 0, 2_001, forged_body);
    assert_eq!(
        receive(&mut state, event),
        Err(Invalid::NotAMember(outsider.key()))
    );

    // A member signing for another principal.
    let for_other = EventBody::InviteCreated(
        crate::identity::Signed::sign(
            &fixture.member,
            Some(fixture.workforce()),
            signed.statement.clone(),
        )
        .unwrap(),
    );
    let event = fixture.author(&state, 0, 2_002, for_other);
    assert_eq!(receive(&mut state, event), Err(Invalid::ActorMismatch));
}

#[test]
fn the_handshake_refuses_expired_redeemed_and_forged_requests() {
    let fixture = Fixture::new(1);
    let mut state = fixture.state();
    let (invite, secret) = fixture.invite(&state, 2_000, 10_000);
    receive(&mut state, invite.clone()).unwrap();
    let vm = joiner(64);
    let proof = secret
        .prove(fixture.workforce(), invite.id(), vm.key())
        .unwrap();

    assert_eq!(
        state.check_join(&invite.id(), vm.key(), &proof, 5_000),
        Ok(())
    );
    assert_eq!(
        state.check_join(&invite.id(), vm.key(), &proof, 10_000),
        Err(JoinRefusal::Expired)
    );
    assert_eq!(
        state.check_join(&fixture.genesis.id(), vm.key(), &proof, 5_000),
        Err(JoinRefusal::UnknownInvite)
    );
    let other = joiner(65);
    assert_eq!(
        state.check_join(&invite.id(), other.key(), &proof, 5_000),
        Err(JoinRefusal::BadProof)
    );

    let bound = fixture.bind(&state, &vm, &invite, &secret, 2_001);
    receive(&mut state, bound).unwrap();
    assert_eq!(
        state.check_join(&invite.id(), vm.key(), &proof, 5_000),
        Ok(()),
        "the device that redeemed it may ask again"
    );
    let other_proof = secret
        .prove(fixture.workforce(), invite.id(), other.key())
        .unwrap();
    assert_eq!(
        state.check_join(&invite.id(), other.key(), &other_proof, 5_000),
        Err(JoinRefusal::Redeemed)
    );
}

#[test]
fn a_leaked_invite_redeemed_twice_binds_both_and_flags_them_on_every_peer() {
    let fixture = Fixture::new(1);
    let mut state = fixture.state();
    let (invite, secret) = fixture.invite(&state, 2_000, 10_000);
    receive(&mut state, invite.clone()).unwrap();
    let (first, second) = (joiner(66), joiner(67));
    let bound_first = fixture.bind(&state, &first, &invite, &secret, 2_001);
    let bound_second = fixture.bind(&state, &second, &invite, &secret, 2_002);

    let mut one = state.clone();
    receive(&mut one, bound_first.clone()).unwrap();
    receive(&mut one, bound_second.clone()).unwrap();
    let mut other = state.clone();
    receive(&mut other, bound_second).unwrap();
    receive(&mut other, bound_first).unwrap();
    assert_eq!(one, other);

    let flagged: Vec<_> = one
        .devices()
        .filter(|info| info.invite_redeemed_twice)
        .map(|info| info.device)
        .collect();
    assert_eq!(flagged.len(), 2);
    assert!(flagged.contains(&first.key()) && flagged.contains(&second.key()));
    assert!(
        !one.devices()
            .find(|info| info.device == fixture.devices[0].key())
            .unwrap()
            .invite_redeemed_twice
    );
}

#[test]
fn a_device_bound_twice_by_forked_roots_has_one_principal_everywhere() {
    let fixture = Fixture::new(1);
    let mut state = fixture.state();
    let (invite, secret) = fixture.invite(&state, 2_000, 10_000);
    receive(&mut state, invite.clone()).unwrap();
    let vm = joiner(68);
    let early = fixture.bind(&state, &vm, &invite, &secret, 2_001);
    let late = fixture.bind(&state, &vm, &invite, &secret, 2_005);
    let mut one = state.clone();
    receive(&mut one, early.clone()).unwrap();
    receive(&mut one, late.clone()).unwrap();
    let mut other = state.clone();
    receive(&mut other, late).unwrap();
    receive(&mut other, early).unwrap();
    assert_eq!(one, other);
    assert_eq!(one.equivocations().count(), 1);
}

#[test]
fn invite_codes_round_trip() {
    let fixture = Fixture::new(2);
    let secret = InviteSigner::generate();
    let invite = crate::id::EventId::of(b"invite");
    let code = InviteCode::new(
        fixture.workforce(),
        invite,
        &secret,
        vec![fixture.devices[0].key(), fixture.devices[1].key()],
    );
    let text = code.to_string();
    assert!(text.starts_with("wfi1"));
    let parsed = InviteCode::from_str(&text).unwrap();
    assert_eq!(parsed, code);
    let device = joiner(70);
    let proof = parsed.prove(device.key()).unwrap();
    assert!(
        secret
            .key()
            .verify_proof(fixture.workforce(), invite, device.key(), &proof)
            .is_ok()
    );
    assert!(InviteCode::from_str("wfx1abc").is_err());
    assert!(InviteCode::from_str("wfi1!!!").is_err());
    assert!(InviteCode::from_str(&text[..text.len() - 4]).is_err());
}
