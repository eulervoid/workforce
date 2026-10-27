//! `wf policy`: showing and changing the policy of the workforce.

use anyhow::Context;
use wf_core::event::{Event, EventBody, PolicySet};
use wf_core::identity::{PrincipalSigner, Signed};
use wf_core::policy::Policy;
use wf_core::state::State;

use super::{done, edit_until_valid, emit};
use crate::cli::*;
use crate::workspace::Workspace;

pub(super) fn policy(
    json: bool,
    command: PolicyCommand,
    workspace: &mut Workspace,
) -> anyhow::Result<()> {
    let current = workspace
        .state()
        .policy()
        .expect("a workforce has a policy")
        .source
        .clone();
    if let Some(error) = workspace.state().unreadable_policy() {
        eprintln!(
            "this version of wf cannot read the policy set last ({}); this is the last one it could read",
            wf_core::error_chain(&error)
        );
    }
    match command {
        PolicyCommand::Show => emit(json, &current, |source| source.clone()),
        PolicyCommand::Apply { file } => {
            let source = std::fs::read_to_string(&file)
                .with_context(|| format!("reading {}", file.display()))?;
            apply_policy(json, workspace, source)
        }
        PolicyCommand::Edit => {
            let edited = edit_until_valid(&current, ".toml", |source| {
                Policy::parse(source)?;
                Ok(source.to_string())
            })?;
            if edited == current {
                return emit(json, &"policy unchanged", |message| format!("{message}\n"));
            }
            apply_policy(json, workspace, edited)
        }
    }
}

fn apply_policy(json: bool, workspace: &mut Workspace, source: String) -> anyhow::Result<()> {
    Policy::parse(&source)?;
    let principal = workspace.principal_signer()?;
    let event = author_policy(workspace, &principal, source)?;
    done(json, "policy replaced".to_string(), &event, None)
}

/// How often a policy is signed before a refusal stands, while events keep landing.
const POLICY_ATTEMPTS: usize = 5;

/// Where a policy is authored. The state is read before the event is written, and the daemon may
/// take other events in between.
trait Authoring {
    fn state(&self) -> &State;
    fn author(&mut self, body: EventBody) -> anyhow::Result<Event>;
    fn refresh(&mut self) -> anyhow::Result<()>;
}

impl Authoring for Workspace {
    fn state(&self) -> &State {
        Workspace::state(self)
    }

    fn author(&mut self, body: EventBody) -> anyhow::Result<Event> {
        Workspace::author(self, body)
    }

    fn refresh(&mut self) -> anyhow::Result<()> {
        Workspace::refresh(self)
    }
}

/// Signs `source` for the hlc of the next event and authors it. The signature binds the policy to
/// the event that carries it, so that a statement cannot roll the policy back; an event that
/// landed after the state was read takes that hlc away, and the policy is signed again.
fn author_policy(
    authoring: &mut impl Authoring,
    principal: &PrincipalSigner,
    source: String,
) -> anyhow::Result<Event> {
    let mut attempt = 1;
    loop {
        let state = authoring.state();
        let read = state.heads().clone();
        let workforce = state.workforce_id().expect("a workforce has an id");
        let hlc = state.next_hlc(wf_store::now_ms())?;
        let statement = Signed::sign(
            principal,
            Some(workforce),
            PolicySet {
                policy: source.clone(),
                hlc,
            },
        )?;
        let error = match authoring.author(EventBody::PolicySet(statement)) {
            Ok(event) => return Ok(event),
            Err(error) => error,
        };
        // Whatever the refusal, it was judged against events the signature did not see.
        authoring.refresh()?;
        if attempt == POLICY_ATTEMPTS || authoring.state().heads() == &read {
            return Err(error);
        }
        attempt += 1;
    }
}

#[cfg(test)]
mod tests {
    use wf_core::state::Trust;
    use wf_core::testing::{Fixture, create};

    use super::*;

    /// A daemon whose state the CLI read before `interlopers` more events landed, one before each
    /// event the CLI authors.
    struct Lagging {
        fixture: Fixture,
        read: State,
        latest: State,
        interlopers: usize,
    }

    impl Lagging {
        fn new(interlopers: usize) -> Self {
            let fixture = Fixture::new(2);
            let state = fixture.state();
            Self {
                fixture,
                read: state.clone(),
                latest: state,
                interlopers,
            }
        }
    }

    impl Authoring for Lagging {
        fn state(&self) -> &State {
            &self.read
        }

        fn author(&mut self, body: EventBody) -> anyhow::Result<Event> {
            let now_ms = wf_store::now_ms();
            if self.interlopers > 0 {
                self.interlopers -= 1;
                let landed = create("task", "landed in between");
                self.fixture
                    .apply(&mut self.latest, 1, now_ms + 1_000, landed);
            }
            let EventBody::PolicySet(signed) = &body else {
                panic!("only policies are authored here");
            };
            let device = &self.fixture.devices[0];
            let event = self
                .latest
                .builder(device.key(), self.fixture.founder_actor(), now_ms)?
                .with_hlc(signed.statement.hlc)
                .sign(device, body)?;
            self.latest.receive(event.clone(), Trust::Untrusted)?;
            Ok(event)
        }

        fn refresh(&mut self) -> anyhow::Result<()> {
            self.read = self.latest.clone();
            Ok(())
        }
    }

    fn policy() -> String {
        wf_core::policy::DEFAULT_POLICY_TOML.replace("review = \"manual\"\n", "")
    }

    #[test]
    fn a_policy_is_signed_again_when_an_event_landed_after_the_state_was_read() {
        let mut lagging = Lagging::new(2);
        let founder = lagging.fixture.founder.clone();
        let event = author_policy(&mut lagging, &founder, policy()).unwrap();
        assert!(lagging.latest.heads().contains(&event.id()));
        assert_eq!(lagging.interlopers, 0);
    }

    #[test]
    fn the_refusal_stands_when_events_keep_landing() {
        let mut lagging = Lagging::new(POLICY_ATTEMPTS);
        let founder = lagging.fixture.founder.clone();
        let error = author_policy(&mut lagging, &founder, policy()).unwrap_err();
        assert_eq!(
            error.downcast_ref::<wf_core::validate::Invalid>(),
            Some(&wf_core::validate::Invalid::HlcNotAfterParents)
        );
    }
}
