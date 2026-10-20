//! Every body tag decodes from the bytes earlier versions wrote.
//!
//! `golden_bodies.txt` holds, per tag in `mod tag`, one whole event as the version that added the
//! tag wrote it, as `<tag> <hex>`. It is only ever appended to: a new tag adds a line, and an
//! existing line never changes, not even when the layout of a body does. A change that makes a
//! line fail to decode would make every stored event of that tag fail, so it must instead take a
//! new tag and keep decoding the old one. Lines are made with
//! `cargo test -p wf-core print_golden_bodies -- --ignored --nocapture` and are copied in once.

use super::*;
use crate::identity::{InviteSigner, PrincipalSigner};
use crate::name::{Handle, ReviewStepName};

const FIXTURE: &str = include_str!("golden_bodies.txt");

/// Every tag of `mod tag` (24 is retired). A tag missing here is not covered; the test below fails for one
/// that has no line.
const ALL_TAGS: [u16; 29] = [
    tag::GENESIS,
    tag::POLICY_SET,
    tag::AGENT_DEFINED,
    tag::INVITE_CREATED,
    tag::DEVICE_BOUND,
    tag::DEVICE_PROFILE,
    tag::FORK_ANNOUNCED,
    tag::AGENT_REMOVED,
    tag::ITEM_CREATED_WITHOUT_STATE,
    tag::ITEM_EDITED,
    tag::ITEM_MOVED,
    tag::ITEM_LINKED,
    tag::ITEM_ARCHIVED,
    tag::ITEM_UNARCHIVED,
    tag::COMMENT,
    tag::ITEM_CREATED,
    tag::SESSION_STARTED,
    tag::SESSION_ENDED_WITHOUT_COMMENT,
    tag::REVIEW,
    tag::MERGED,
    tag::SESSION_ENDED,
    tag::COMMENT_ON_LINES,
    tag::COMMENT_CHECK_FAILURE,
    tag::BOUND_POLICY_SET,
    tag::SESSION_STARTED_FOR_STEP,
    tag::COMMENT_WITH_HEAD,
    tag::SESSION_ENDED_WITH_HEAD,
    tag::SESSION_ENDED_CLAIM_LOST,
    tag::REVIEW_AT_ROUND_LIMIT,
];

fn fixture_lines() -> Vec<(u16, Vec<u8>)> {
    FIXTURE
        .lines()
        .map(|line| {
            let (tag, hex) = line.split_once(' ').expect("a line is `<tag> <hex>`");
            (
                tag.parse().expect("the tag is a number"),
                data_encoding::HEXLOWER.decode(hex.as_bytes()).unwrap(),
            )
        })
        .collect()
}

fn body_tag(event: &Event) -> u16 {
    let tagged: TaggedBody =
        encoding::decode(&encoding::encode(&event.content.body).unwrap()).unwrap();
    tagged.tag
}

#[test]
fn every_tag_decodes_from_its_golden_event_and_encodes_to_the_same_bytes() {
    for (tag, bytes) in fixture_lines() {
        let event = Event::decode(&bytes).unwrap_or_else(|error| panic!("tag {tag}: {error}"));
        assert_eq!(
            body_tag(&event),
            tag,
            "tag {tag}: decoded under another tag"
        );
        assert_eq!(
            event.encode().unwrap(),
            bytes,
            "tag {tag}: re-encoded to other bytes"
        );
        event
            .verify_signature()
            .unwrap_or_else(|error| panic!("tag {tag}: {error}"));
    }
}

#[test]
fn every_tag_has_a_golden_event() {
    let covered: Vec<u16> = fixture_lines().iter().map(|(tag, _)| *tag).collect();
    for tag in ALL_TAGS.into_iter() {
        assert!(
            covered.contains(&tag),
            "tag {tag} has no line in golden_bodies.txt"
        );
    }
}

fn bodies() -> Vec<EventBody> {
    let principal = PrincipalSigner::from_bytes(&[1; 32]).unwrap();
    let device = DeviceSigner::from_bytes(&[2; 32]).unwrap();
    let invite = InviteSigner::generate([6; 32]);
    let workforce = WorkforceId::from(EventId::of(b"workforce"));
    let item = ItemId::from_bytes([1; 32]);
    let session = SessionId::from_bytes([2; 32]);
    let comment = CommentId::from_bytes([3; 32]);
    let commit = CommitId::Git("00112233445566778899aabbccddeeff00112233".parse().unwrap());
    let agent = AgentName::new("coder").unwrap();
    let seed = MemberSeed {
        principal: principal.key(),
        handle: Handle::new("ada").unwrap(),
        devices: vec![device.key()],
        previous: vec![],
    };
    let plain_comment = Commented::plain(item, "why");
    let created = ItemCreated {
        item_type: TypeName::new("task").unwrap(),
        title: "t".into(),
        body: "- [ ] step".into(),
        priority: Some(Priority::P2),
        lane: Some(LaneName::new("backend").unwrap()),
        labels: vec![Label::new("db").unwrap()],
        requires: vec![Requirement::new("gpu").unwrap()],
        review: Some(ReviewLevelName::new("strict").unwrap()),
        state: None,
    };
    fn signed_by<T: Serialize>(
        principal: &PrincipalSigner,
        workforce: WorkforceId,
        statement: T,
    ) -> Signed<T> {
        Signed::sign(principal, Some(workforce), statement).unwrap()
    }
    vec![
        EventBody::Genesis(
            Signed::sign(
                &principal,
                None,
                Genesis {
                    name: "demo".into(),
                    founder: seed,
                    members: vec![],
                    policy: "[merge]\ntarget = \"main\"\n".into(),
                    forked_from: Some(ForkPoint {
                        workforce,
                        heads: vec![EventId::of(b"head")],
                    }),
                },
            )
            .unwrap(),
        ),
        EventBody::PolicySet(signed_by(
            &principal,
            workforce,
            PolicySet {
                policy: "[merge]\ntarget = \"trunk\"\n".into(),
            },
        )),
        EventBody::BoundPolicySet(signed_by(
            &principal,
            workforce,
            BoundPolicySet {
                policy: "[merge]\ntarget = \"trunk\"\n".into(),
                hlc: Hlc::next_after(Hlc::ZERO, 1_000).unwrap(),
            },
        )),
        EventBody::AgentDefined(AgentDefined {
            name: agent.clone(),
            harness: HarnessName::new("claude").unwrap(),
            model: Some("opus".into()),
            capabilities: vec![Requirement::new("gpu").unwrap()],
            lanes: vec![LaneName::new("backend").unwrap()],
            autonomy: Autonomy::default(),
            instructions: Some("Be brief.".into()),
        }),
        EventBody::InviteCreated(signed_by(
            &principal,
            workforce,
            InviteCreated {
                kind: InviteKind::Device,
                invite: invite.key(),
                expires_ms: 5_000,
            },
        )),
        EventBody::DeviceBound(DeviceBound {
            invite: EventId::of(b"invite"),
            proof: invite
                .prove(workforce, EventId::of(b"invite"), device.key())
                .unwrap(),
        }),
        EventBody::DeviceProfile(DeviceProfile {
            name: "laptop".into(),
            hardware: "gpu, os:linux".into(),
            agents: BTreeMap::from([(agent.clone(), 2)]),
            max_sessions: 4,
        }),
        EventBody::ForkAnnounced(signed_by(
            &principal,
            workforce,
            ForkAnnounced {
                to: workforce,
                members: vec![principal.key()],
            },
        )),
        EventBody::AgentRemoved(AgentRemoved {
            name: agent.clone(),
        }),
        EventBody::ItemCreated(created.clone()),
        EventBody::ItemEdited(ItemEdited {
            item,
            changes: vec![Change::Title("new".into()), Change::Priority(None)],
        }),
        EventBody::ItemMoved(ItemMoved {
            item,
            to: WrittenState::as_written("in_progress"),
            by_session: Some(session),
            because: Some(comment),
        }),
        EventBody::ItemLinked(ItemLinked {
            item,
            link: Link::Supersedes(ItemId::from_bytes([4; 32])),
        }),
        EventBody::ItemArchived(ItemRef { item }),
        EventBody::ItemUnarchived(ItemRef { item }),
        EventBody::Commented(plain_comment.clone()),
        EventBody::ItemCreated(ItemCreated {
            state: Some(ItemState::Draft.into()),
            ..created
        }),
        EventBody::SessionStarted(SessionStarted {
            agent: agent.clone(),
            item,
            role: SessionRole::Review {
                commit: Some(commit),
                step: None,
            },
        }),
        EventBody::SessionStarted(SessionStarted {
            agent: agent.clone(),
            item,
            role: SessionRole::Review {
                commit: Some(commit),
                step: Some(ReviewStepName::new("security").unwrap()),
            },
        }),
        EventBody::SessionEnded(SessionEnded {
            session,
            reason: EndReason::Released,
            because: None,
            head: None,
        }),
        EventBody::Reviewed(Reviewed {
            item,
            commit: Some(commit),
            verdict: Verdict::RequestChanges,
            because: Some(comment),
            round_limit_reached: None,
        }),
        EventBody::Merged(Merged {
            item,
            reviewed: commit,
            commit,
        }),
        EventBody::SessionEnded(SessionEnded {
            session,
            reason: EndReason::Released,
            because: Some(comment),
            head: None,
        }),
        EventBody::Commented(Commented {
            lines: Some(LineAnchor {
                commit,
                path: "src/lib.rs".into(),
                side: DiffSide::New,
                first: 12,
                last: 13,
                quote: "+fn f() {}".into(),
            }),
            ..plain_comment.clone()
        }),
        EventBody::Commented(Commented {
            kind: CommentKind::CheckFailure,
            ..plain_comment.clone()
        }),
        EventBody::Commented(Commented {
            head: Some(commit),
            ..plain_comment
        }),
        EventBody::SessionEnded(SessionEnded {
            session,
            reason: EndReason::Released,
            because: Some(comment),
            head: Some(commit),
        }),
        EventBody::SessionEnded(SessionEnded {
            session,
            reason: EndReason::LeaseExpired,
            because: None,
            head: None,
        }),
        EventBody::Reviewed(Reviewed {
            item,
            commit: Some(commit),
            verdict: Verdict::RequestChanges,
            because: Some(comment),
            round_limit_reached: Some(3),
        }),
    ]
}

/// Prints the lines for `golden_bodies.txt` as the current version writes them.
#[test]
#[ignore = "prints; run by hand when a tag is added"]
fn print_golden_bodies() {
    let principal = PrincipalSigner::from_bytes(&[1; 32]).unwrap();
    let device = DeviceSigner::from_bytes(&[2; 32]).unwrap();
    for body in bodies() {
        let event = EventBuilder::genesis(device.key(), Actor::Principal(principal.key()), 1_000)
            .sign(&device, body)
            .unwrap();
        println!(
            "{} {}",
            body_tag(&event),
            data_encoding::HEXLOWER.encode(&event.encode().unwrap())
        );
    }
}
