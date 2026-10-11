//! Permission requests that agents make and a person answers.

use std::sync::Mutex;

use tokio::sync::oneshot;
use wf_core::id::{SessionId, WorkforceId};
use wf_harness::PermissionMode;

use crate::proto::{Answer, HeldPermission};

struct Request {
    workforce: WorkforceId,
    held: HeldPermission,
    reply: oneshot::Sender<PermissionMode>,
}

/// The requests that wait for a person. Runtime state: a request does not outlive its session,
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
    ) -> oneshot::Receiver<PermissionMode> {
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

    /// Answers the oldest request of the session, if it has one.
    pub(crate) fn answer(&self, session: SessionId, answer: Answer) -> Option<HeldPermission> {
        let mut requests = self.lock();
        let index = requests
            .iter()
            .position(|request| request.held.session == session)?;
        let request = requests.remove(index);
        let _ = request.reply.send(match answer {
            Answer::Allow => PermissionMode::Allow,
            Answer::Deny => PermissionMode::Deny,
        });
        Some(request.held)
    }

    /// Drops the requests of a session that is over. Their agents are gone with it.
    pub(crate) fn forget(&self, session: SessionId) {
        self.lock()
            .retain(|request| request.held.session != session);
    }
}
