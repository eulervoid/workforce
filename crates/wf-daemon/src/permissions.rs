//! Permission requests that agents make and the principal answers.

use std::sync::Mutex;

use tokio::sync::oneshot;
use wf_core::id::{SessionId, WorkforceId};
use wf_harness::Permission;

use crate::proto::HeldPermission;

struct Request {
    workforce: WorkforceId,
    held: HeldPermission,
    reply: oneshot::Sender<Permission>,
}

/// Why a request was not answered.
#[derive(Debug, PartialEq, Eq, thiserror::Error)]
pub(crate) enum NotAnswered {
    #[error("that session is not waiting for an answer")]
    NoRequest,
    #[error("the oldest request of that session is another than the one answered; look again")]
    Another,
}

/// The requests that wait for the principal. Runtime state: a request does not outlive its session,
/// nor the daemon, and its agent is told no when it goes.
#[derive(Default)]
pub(crate) struct Held {
    requests: Mutex<Vec<Request>>,
}

impl Held {
    fn lock(&self) -> std::sync::MutexGuard<'_, Vec<Request>> {
        self.requests
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    pub(crate) fn hold(
        &self,
        workforce: WorkforceId,
        session: SessionId,
        what: &str,
        passed: Vec<String>,
    ) -> oneshot::Receiver<Permission> {
        let (reply, answer) = oneshot::channel();
        self.lock().push(Request {
            workforce,
            held: HeldPermission {
                session,
                what: what.to_string(),
                passed,
                since_ms: crate::now_ms(),
            },
            reply,
        });
        answer
    }

    pub(crate) fn list(&self, workforce: WorkforceId) -> Vec<HeldPermission> {
        self.lock()
            .iter()
            .filter(|request| request.workforce == workforce)
            .map(|request| request.held.clone())
            .collect()
    }

    /// Answers the oldest request of the session, if it is the one from `since_ms` the principal
    /// saw. Another one is left waiting: the principal has not read it.
    pub(crate) fn answer(
        &self,
        session: SessionId,
        since_ms: u64,
        answer: Permission,
    ) -> Result<HeldPermission, NotAnswered> {
        let mut requests = self.lock();
        let index = requests
            .iter()
            .position(|request| request.held.session == session)
            .ok_or(NotAnswered::NoRequest)?;
        if requests[index].held.since_ms != since_ms {
            return Err(NotAnswered::Another);
        }
        let request = requests.remove(index);
        let _ = request.reply.send(answer);
        Ok(request.held)
    }

    /// Drops the requests of a session that is over. Their agents are gone with it.
    pub(crate) fn forget(&self, session: SessionId) {
        self.lock()
            .retain(|request| request.held.session != session);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use wf_core::id::EventId;

    fn held_by(held: &Held, session: SessionId) -> oneshot::Receiver<Permission> {
        held.hold(
            WorkforceId::from(EventId::of(b"workforce")),
            session,
            "rm -rf target",
            Vec::new(),
        )
    }

    #[test]
    fn only_the_oldest_request_the_principal_saw_is_answered() {
        let held = Held::default();
        let session = SessionId::from(EventId::of(b"session"));
        let mut first = held_by(&held, session);
        let seen = held.requests.lock().unwrap()[0].held.since_ms;

        // Another request arrives after the principal looked; the answer was for `seen`.
        let _second = held_by(&held, session);
        assert_eq!(
            held.answer(session, seen + 1, Permission::Allow),
            Err(NotAnswered::Another)
        );
        assert!(first.try_recv().is_err(), "nothing was answered");

        assert!(held.answer(session, seen, Permission::Allow).is_ok());
        assert_eq!(first.try_recv().unwrap(), Permission::Allow);

        let other = SessionId::from(EventId::of(b"other"));
        assert_eq!(
            held.answer(other, seen, Permission::Deny),
            Err(NotAnswered::NoRequest)
        );
    }
}
