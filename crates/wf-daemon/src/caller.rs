//! Whom a connection to the socket acts for. The daemon decides it from the token the client
//! presents when it connects, never from what a request says: the token of a session makes the
//! connection that session's agent, the principal token makes it the principal, and a connection
//! with neither is refused.
//!
//! The principal token is a file in the keys directory, which the sandbox keeps agents from
//! reading, as it does the keys. An agent without a sandbox can read it, as it can read the keys.

use std::collections::BTreeMap;
use std::str::FromStr;
use std::sync::Mutex;

use data_encoding::HEXLOWER;
use serde::{Deserialize, Serialize};
use wf_core::id::{SessionId, WorkforceId};

use crate::data_dir::DataDir;

/// Set by the runner for the processes of a session, next to `WF_SESSION`: the token that makes
/// a connection to the socket act as the session's agent.
pub const SESSION_TOKEN_VARIABLE: &str = "WF_SESSION_TOKEN";

/// Set by the runner for the processes of an agent session: what they write is the session's.
/// With a daemon, the daemon says whom a process acts for, from the token it presents.
pub const SESSION_VARIABLE: &str = "WF_SESSION";

/// Set by the runner for the processes of an agent session: the workforce the session is in. The
/// checkout of a session carries the workforce file as it was committed, which names an older
/// workforce when this one was created after the commit.
pub const WORKFORCE_VARIABLE: &str = "WF_WORKFORCE";

/// A secret the daemon hands out, which a client presents to act for someone.
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct Token([u8; 32]);

impl Token {
    fn generate() -> Self {
        Self(rand::random())
    }
}

/// Leaves the secret out of logs.
impl std::fmt::Debug for Token {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("Token(..)")
    }
}

impl std::fmt::Display for Token {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&HEXLOWER.encode(&self.0))
    }
}

#[derive(Debug, thiserror::Error)]
#[error("a token is 64 lowercase hex digits")]
pub struct TokenError;

impl FromStr for Token {
    type Err = TokenError;

    fn from_str(text: &str) -> Result<Self, TokenError> {
        let bytes = HEXLOWER.decode(text.as_bytes()).map_err(|_| TokenError)?;
        <[u8; 32]>::try_from(bytes.as_slice())
            .map(Self)
            .map_err(|_| TokenError)
    }
}

/// Whom a connection acts for.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Caller {
    /// The principal of this machine's device.
    Principal,
    /// The agent of a live session of this machine, in the workforce of the session only.
    Session {
        workforce: WorkforceId,
        session: SessionId,
    },
}

/// The tokens the daemon recognizes: its principal token, and one for each running session.
pub(crate) struct Callers {
    principal: Token,
    sessions: Mutex<BTreeMap<Token, (WorkforceId, SessionId)>>,
}

impl Default for Callers {
    fn default() -> Self {
        Self {
            principal: Token::generate(),
            sessions: Mutex::default(),
        }
    }
}

impl Callers {
    /// Writes the principal token to the keys directory of `data_dir`, where only the
    /// principal's processes can read it.
    pub(crate) fn publish(&self, data_dir: &DataDir) -> std::io::Result<()> {
        crate::data_dir::write_private(
            &data_dir.principal_token(),
            format!("{}\n", self.principal).as_bytes(),
        )
    }

    fn sessions(&self) -> std::sync::MutexGuard<'_, BTreeMap<Token, (WorkforceId, SessionId)>> {
        self.sessions
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Whom a connection that presents `token` acts for; `None` for a token the daemon did not
    /// hand out, or one of a session that is over.
    pub(crate) fn recognize(&self, token: &Token) -> Option<Caller> {
        if *token == self.principal {
            return Some(Caller::Principal);
        }
        self.sessions()
            .get(token)
            .map(|&(workforce, session)| Caller::Session { workforce, session })
    }

    /// A new token for a session whose harness starts, which replaces any earlier one.
    pub(crate) fn issue(&self, workforce: WorkforceId, session: SessionId) -> Token {
        let token = Token::generate();
        let mut sessions = self.sessions();
        sessions.retain(|_, &mut (_, issued)| issued != session);
        sessions.insert(token.clone(), (workforce, session));
        token
    }

    /// Forgets the token of a session whose harness is gone.
    pub(crate) fn revoke(&self, session: SessionId) {
        self.sessions()
            .retain(|_, &mut (_, issued)| issued != session);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn session(text: &[u8]) -> (WorkforceId, SessionId) {
        let workforce = WorkforceId::from(wf_core::id::EventId::of(b"workforce"));
        (workforce, SessionId::from(wf_core::id::EventId::of(text)))
    }

    #[test]
    fn the_principal_token_is_written_for_the_principal_and_recognized() {
        let directory = tempfile::tempdir().unwrap();
        let data = DataDir::new(directory.path().to_path_buf());
        let callers = Callers::default();
        callers.publish(&data).unwrap();
        let token = data.read_principal_token().expect("the principal token");
        assert_eq!(callers.recognize(&token), Some(Caller::Principal));

        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(data.principal_token())
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);

        let restarted = Callers::default();
        restarted.publish(&data).unwrap();
        assert_ne!(data.read_principal_token(), Some(token.clone()));
        assert_eq!(
            restarted.recognize(&token),
            None,
            "each daemon draws its own"
        );
    }

    #[test]
    fn a_session_token_acts_for_its_session_until_revoked() {
        let callers = Callers::default();
        let (workforce, first) = session(b"first");
        let (_, second) = session(b"second");
        let token = callers.issue(workforce, first);
        let other = callers.issue(workforce, second);
        assert_eq!(
            callers.recognize(&token),
            Some(Caller::Session {
                workforce,
                session: first
            })
        );

        let reissued = callers.issue(workforce, first);
        assert_eq!(callers.recognize(&token), None, "the old token is replaced");
        callers.revoke(first);
        assert_eq!(callers.recognize(&reissued), None);
        assert!(callers.recognize(&other).is_some());
        assert_eq!(callers.recognize(&Token::generate()), None);
    }

    #[test]
    fn a_token_reads_back_as_written_and_does_not_show_in_debug() {
        let token = Token::generate();
        assert_eq!(token.to_string().parse::<Token>().unwrap(), token);
        assert!("abc".parse::<Token>().is_err());
        assert_eq!(format!("{token:?}"), "Token(..)");
    }
}
