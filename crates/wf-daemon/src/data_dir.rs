//! The data directory, `$XDG_DATA_HOME/workforce/`: the one place that spells out what is in it.
//!
//! ```text
//! keys/device.key             the device key
//! keys/device-ssh             the device key as an SSH key, for signing merges
//! keys/principal.token        the token that makes a connection act as the principal
//! <workforce>/principal.key   the principal key of the workforce
//! <workforce>/store.redb      the store
//! <workforce>/transcripts/    the transcripts of its sessions
//! <workforce>/worktrees/      the worktrees of its items
//! ```
//!
//! The key files are mode 0600, `keys/` is 0700, and the sandbox denies agents what
//! [`DataDir::secrets`] lists. A key-provider hook (1Password, ssh-agent) replaces the key files
//! later.

use std::io::Write;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use data_encoding::HEXLOWER;
use wf_core::id::{SessionId, WorkforceId};
use wf_core::identity::{DeviceSigner, PrincipalSigner};

use crate::caller::{SESSION_TOKEN_VARIABLE, Token};
use crate::client::{ClientError, DaemonClient};

/// Named once, for the path and for the glob that denies it to agents.
const PRINCIPAL_KEY_FILE: &str = "principal.key";

#[derive(Debug, thiserror::Error)]
pub enum DataDirError {
    #[error("HOME is not set")]
    NoHome,
    #[error("{path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("{0} is not a hex key")]
    NotHex(PathBuf),
    #[error("{0} must hold 32 bytes")]
    WrongLength(PathBuf),
    #[error("{SESSION_TOKEN_VARIABLE} is not a token")]
    NotAToken,
    #[error(transparent)]
    Client(#[from] ClientError),
    #[error("the daemon owns the stores, so {0} needs it stopped first: `wf daemon stop`")]
    DaemonRuns(String),
}

/// Where wf keeps everything outside the repository.
#[derive(Clone, Debug)]
pub struct DataDir {
    root: PathBuf,
}

impl DataDir {
    pub fn new(root: PathBuf) -> Self {
        Self { root }
    }

    /// `$XDG_DATA_HOME/workforce`, falling back to `~/.local/share/workforce`.
    pub fn from_env() -> Result<Self, DataDirError> {
        let base = match std::env::var_os("XDG_DATA_HOME").filter(|value| !value.is_empty()) {
            Some(path) => PathBuf::from(path),
            None => PathBuf::from(std::env::var_os("HOME").ok_or(DataDirError::NoHome)?)
                .join(".local/share"),
        };
        Ok(Self::new(base.join("workforce")))
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// The directory of the keys of the device, and the tokens the daemon hands out.
    pub fn keys(&self) -> PathBuf {
        self.root.join("keys")
    }

    pub fn device_key(&self) -> PathBuf {
        self.keys().join("device.key")
    }

    /// The device key as an SSH key file that `ssh-keygen` signs merges with.
    pub fn device_ssh_key(&self) -> PathBuf {
        self.keys().join("device-ssh")
    }

    /// The token that makes a connection act as the principal.
    pub fn principal_token(&self) -> PathBuf {
        self.keys().join("principal.token")
    }

    /// The `wf` binary agents run, installed by the daemon.
    pub fn bin(&self) -> PathBuf {
        self.root.join("bin")
    }

    /// The `CARGO_HOME` of sandboxed agents: the registry and git checkouts they download, whose
    /// build scripts the principal's own cargo never runs.
    pub fn agent_cargo_home(&self) -> PathBuf {
        self.root.join("cargo")
    }

    pub fn workforce(&self, id: &WorkforceId) -> PathBuf {
        self.root.join(id.to_string())
    }

    /// The principal key of one workforce, which lives next to its store.
    pub fn principal_key(&self, id: &WorkforceId) -> PathBuf {
        self.workforce(id).join(PRINCIPAL_KEY_FILE)
    }

    pub fn store(&self, id: &WorkforceId) -> PathBuf {
        self.workforce(id).join(STORE_FILE)
    }

    pub fn transcripts(&self, id: &WorkforceId) -> PathBuf {
        self.workforce(id).join("transcripts")
    }

    pub fn transcript(&self, id: &WorkforceId, session: SessionId) -> PathBuf {
        self.transcripts(id).join(format!("{session}.jsonl"))
    }

    /// The directory the sessions of a workforce on this machine run in.
    pub fn repositories(&self, id: &WorkforceId) -> PathBuf {
        wf_vcs::repositories(&self.workforce(id))
    }

    /// The unix socket of the daemon of this machine.
    pub fn daemon_socket(&self) -> PathBuf {
        let runtime = std::env::var_os("XDG_RUNTIME_DIR")
            .filter(|value| !value.is_empty())
            .map(PathBuf::from);
        crate::proto::socket_path(&self.root, runtime.as_deref())
    }

    /// The process id of the daemon of this machine, while it runs.
    pub fn daemon_pid(&self) -> PathBuf {
        self.root.join("daemon.pid")
    }

    /// What a daemon started with `wf daemon start` writes.
    pub fn daemon_log(&self) -> PathBuf {
        self.root.join("daemon.log")
    }

    /// What agents must not read or write: with the keys an agent signs as the device or the
    /// principal, and with the token it has the daemon act for the principal. The principal keys
    /// are a glob over the workforces.
    pub fn secrets(&self) -> Vec<PathBuf> {
        vec![self.keys(), self.root.join("*").join(PRINCIPAL_KEY_FILE)]
    }

    /// The principal token of the daemon that runs on this directory, if this process may read it.
    pub fn read_principal_token(&self) -> Option<Token> {
        std::fs::read_to_string(self.principal_token())
            .ok()?
            .trim()
            .parse()
            .ok()
    }

    /// The daemon of this machine, if one is listening, acting for the session this process
    /// runs in when `WF_SESSION_TOKEN` is set, and for the principal otherwise.
    pub fn connect_daemon(&self) -> Result<Option<DaemonClient>, DataDirError> {
        let token = match std::env::var(SESSION_TOKEN_VARIABLE) {
            Ok(token) => Some(token.parse().map_err(|_| DataDirError::NotAToken)?),
            Err(_) => self.read_principal_token(),
        };
        Ok(DaemonClient::connect(&self.daemon_socket(), token)?)
    }

    /// Fails while a daemon listens: what only a process that owns the stores may do, `what`,
    /// is not done behind its back.
    pub fn require_no_daemon(&self, what: &str) -> Result<(), DataDirError> {
        if self.connect_daemon()?.is_some() {
            return Err(DataDirError::DaemonRuns(what.to_string()));
        }
        Ok(())
    }

    /// The device key of this machine, if it has one.
    pub fn device(&self) -> Result<Option<DeviceSigner>, DataDirError> {
        Ok(read_secret(&self.device_key())?
            .map(|secret| DeviceSigner::from_bytes(&secret).expect("32 bytes")))
    }

    /// The device key of this machine, created when there is none yet.
    pub fn device_or_create(&self) -> Result<DeviceSigner, DataDirError> {
        if let Some(signer) = self.device()? {
            return Ok(signer);
        }
        let signer = DeviceSigner::generate(rand::random());
        write_secret(&self.device_key(), &signer.to_bytes())?;
        Ok(signer)
    }

    /// The principal key of one workforce. A principal has a key per workforce, so losing or
    /// replacing one touches nothing else.
    pub fn principal(&self, id: &WorkforceId) -> Result<Option<PrincipalSigner>, DataDirError> {
        Ok(read_secret(&self.principal_key(id))?
            .map(|secret| PrincipalSigner::from_bytes(&secret).expect("32 bytes")))
    }

    /// Stores the principal key of a workforce that has none yet.
    pub fn store_principal(
        &self,
        id: &WorkforceId,
        signer: &PrincipalSigner,
    ) -> Result<(), DataDirError> {
        let path = self.principal_key(id);
        if path.exists() {
            return Err(DataDirError::Io {
                source: std::io::ErrorKind::AlreadyExists.into(),
                path,
            });
        }
        write_secret(&path, &signer.to_bytes())
    }
}

/// The file of the store in the directory of a workforce.
pub const STORE_FILE: &str = "store.redb";

fn read_secret(path: &Path) -> Result<Option<[u8; 32]>, DataDirError> {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(source) => {
            return Err(DataDirError::Io {
                path: path.to_path_buf(),
                source,
            });
        }
    };
    let bytes = HEXLOWER
        .decode(text.trim().as_bytes())
        .map_err(|_| DataDirError::NotHex(path.to_path_buf()))?;
    match <[u8; 32]>::try_from(bytes.as_slice()) {
        Ok(secret) => Ok(Some(secret)),
        Err(_) => Err(DataDirError::WrongLength(path.to_path_buf())),
    }
}

fn write_secret(path: &Path, secret: &[u8; 32]) -> Result<(), DataDirError> {
    write_private(path, format!("{}\n", HEXLOWER.encode(secret)).as_bytes()).map_err(|source| {
        DataDirError::Io {
            path: path.to_path_buf(),
            source,
        }
    })
}

/// Creates `directory` and its parents, readable only by its owner where it is new.
pub fn create_private_directory(directory: &Path) -> std::io::Result<()> {
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(directory)
}

/// Replaces the file at `path` with `contents`, readable only by its owner, in one step: a
/// temporary file of mode 0600 beside it is renamed over it, so a reader sees the old file or
/// the new one and never half of one, and the mode holds for a file that existed before.
pub fn write_private(path: &Path, contents: &[u8]) -> std::io::Result<()> {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let directory = path.parent().unwrap_or(Path::new("."));
    create_private_directory(directory)?;
    let name = path.file_name().unwrap_or_default().to_string_lossy();
    let staged = directory.join(format!(
        ".{name}.{}.{}",
        std::process::id(),
        COUNTER.fetch_add(1, Ordering::Relaxed)
    ));
    let written = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&staged)
        .and_then(|mut file| file.write_all(contents).and_then(|()| file.sync_all()))
        .and_then(|()| std::fs::rename(&staged, path));
    if written.is_err() {
        let _ = std::fs::remove_file(&staged);
    }
    written
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn mode(path: &Path) -> u32 {
        std::fs::metadata(path).unwrap().permissions().mode() & 0o777
    }

    /// Whether `secret`, with `*` standing for any one component, is `path` or a parent of it.
    fn covers(secret: &Path, path: &Path) -> bool {
        let mut components = path.components();
        secret.components().all(|part| {
            components
                .next()
                .is_some_and(|other| part.as_os_str() == "*" || part == other)
        })
    }

    #[test]
    fn the_sandbox_denies_every_key_path() {
        let data = DataDir::new(PathBuf::from("/data/workforce"));
        let id = WorkforceId::from(wf_core::id::EventId::of(b"workforce"));
        let secrets = data.secrets();
        for path in [
            data.device_key(),
            data.device_ssh_key(),
            data.principal_token(),
            data.principal_key(&id),
        ] {
            assert!(
                secrets.iter().any(|secret| covers(secret, &path)),
                "{} is readable to agents",
                path.display()
            );
        }
        assert!(
            !secrets
                .iter()
                .any(|secret| covers(secret, &data.store(&id)))
        );
    }

    #[test]
    fn keys_are_created_once_with_private_permissions() {
        let directory = tempfile::tempdir().unwrap();
        let data = DataDir::new(directory.path().to_path_buf());
        assert!(data.device().unwrap().is_none());
        let created = data.device_or_create().unwrap();
        let loaded = data.device().unwrap().unwrap();
        assert_eq!(created.key(), loaded.key());
        assert_eq!(data.device_or_create().unwrap().key(), created.key());
        assert_eq!(mode(&data.device_key()), 0o600);
        assert_eq!(mode(&data.keys()), 0o700);
    }

    #[test]
    fn every_workforce_has_its_own_principal_key() {
        let directory = tempfile::tempdir().unwrap();
        let data = DataDir::new(directory.path().to_path_buf());
        let first = WorkforceId::from(wf_core::id::EventId::of(b"first"));
        let second = WorkforceId::from(wf_core::id::EventId::of(b"second"));
        assert!(data.principal(&first).unwrap().is_none());
        let signer = PrincipalSigner::generate(rand::random());
        data.store_principal(&first, &signer).unwrap();
        assert_eq!(
            data.principal(&first).unwrap().unwrap().to_bytes(),
            signer.to_bytes()
        );
        assert!(data.principal(&second).unwrap().is_none());
        assert!(
            data.store_principal(&first, &signer).is_err(),
            "never overwritten"
        );
        assert_eq!(mode(&data.principal_key(&first)), 0o600);
    }

    #[test]
    fn malformed_key_files_are_reported() {
        let directory = tempfile::tempdir().unwrap();
        let data = DataDir::new(directory.path().to_path_buf());
        std::fs::create_dir_all(data.keys()).unwrap();
        std::fs::write(data.device_key(), "not hex").unwrap();
        assert!(data.device().is_err());
        std::fs::write(data.device_key(), "abcd").unwrap();
        assert!(data.device().is_err());
    }

    #[test]
    fn private_files_are_replaced_whole_and_made_private() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("keys").join("secret");
        write_private(&path, b"one").unwrap();
        assert_eq!(mode(path.parent().unwrap()), 0o700);
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        write_private(&path, b"two").unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"two");
        assert_eq!(mode(&path), 0o600);
        let left: Vec<_> = std::fs::read_dir(path.parent().unwrap()).unwrap().collect();
        assert_eq!(left.len(), 1, "no temporary file is left");
    }
}
