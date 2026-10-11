//! The chain of deciders a permission request passes through before a person is asked.

pub mod rules;

use std::sync::Arc;

use wf_core::id::{SessionId, WorkforceId};
use wf_harness::{Decision, PermissionMode, PermissionRequest, Permissions};

use crate::permissions::Held;

/// Answers the requests of a session whose agent asks: the built-in rules first, then a person,
/// through `wf inbox` and `wf session allow|deny`.
pub(crate) struct Deciding {
    pub(crate) held: Arc<Held>,
    pub(crate) workforce: WorkforceId,
    pub(crate) session: SessionId,
}

impl Permissions for Deciding {
    fn decide(&self, request: PermissionRequest) -> wf_harness::BoxFuture<'_, Decision> {
        Box::pin(async move {
            if let Some(reason) = rules::allows(&request) {
                return Decision {
                    permission: PermissionMode::Allow,
                    by: "rules".to_string(),
                    reason: Some(reason),
                };
            }
            let answer = self.held.hold(self.workforce, self.session, &request.title);
            Decision {
                permission: answer.await.unwrap_or(PermissionMode::Deny),
                by: "a person".to_string(),
                reason: None,
            }
        })
    }
}
