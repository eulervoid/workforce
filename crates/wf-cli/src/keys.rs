//! Key files, mode 0600, under `$XDG_DATA_HOME/workforce/`: `keys/device.key` for the machine and
//! `<workforce>/principal.key` for each workforce.
//! A key-provider hook (1Password, ssh-agent) replaces this later.

use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};

use anyhow::{Context, bail};
use data_encoding::HEXLOWER;
use wf_core::id::WorkforceId;
use wf_core::identity::{DeviceSigner, PrincipalSigner};

/// Where `wf` keeps everything outside the repository.
#[derive(Clone, Debug)]
pub struct DataDir {
    root: PathBuf,
}

impl DataDir {
    pub fn new(root: PathBuf) -> Self {
        Self { root }
    }

    /// `$XDG_DATA_HOME/workforce`, falling back to `~/.local/share/workforce`.
    pub fn from_env() -> anyhow::Result<Self> {
        let base = match std::env::var_os("XDG_DATA_HOME").filter(|value| !value.is_empty()) {
            Some(path) => PathBuf::from(path),
            None => PathBuf::from(std::env::var_os("HOME").context("HOME is not set")?)
                .join(".local/share"),
        };
        Ok(Self::new(base.join("workforce")))
    }

    pub fn keys(&self) -> PathBuf {
        self.root.join("keys")
    }

    /// The unix socket of the daemon of this machine.
    pub fn daemon_socket(&self) -> PathBuf {
        let runtime = std::env::var_os("XDG_RUNTIME_DIR")
            .filter(|value| !value.is_empty())
            .map(PathBuf::from);
        wf_daemon::proto::socket_path(&self.root, runtime.as_deref())
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn workforce(&self, id: &WorkforceId) -> PathBuf {
        self.root.join(id.to_string())
    }

    /// Tells the daemon where the repository of the workforce is on this machine, which is where
    /// the last `wf` command ran. Best effort: the daemon only needs it to start sessions.
    pub fn remember_repository(&self, id: &WorkforceId, repository: &Path) {
        let directory = self.workforce(id);
        if directory.is_dir() {
            let _ = wf_daemon::remember_repository(&directory, repository);
        }
    }
}

fn read_secret(path: &Path) -> anyhow::Result<Option<[u8; 32]>> {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error).with_context(|| format!("reading {}", path.display())),
    };
    let bytes = HEXLOWER
        .decode(text.trim().as_bytes())
        .with_context(|| format!("{} is not a hex key", path.display()))?;
    match <[u8; 32]>::try_from(bytes.as_slice()) {
        Ok(secret) => Ok(Some(secret)),
        Err(_) => bail!("{} must hold 32 bytes", path.display()),
    }
}

fn write_secret(path: &Path, secret: &[u8; 32]) -> anyhow::Result<()> {
    std::fs::create_dir_all(path.parent().context("key path has a parent")?)?;
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
        .with_context(|| format!("creating {}", path.display()))?;
    writeln!(file, "{}", HEXLOWER.encode(secret))?;
    Ok(())
}

fn load<T>(
    path: PathBuf,
    create: bool,
    generate: impl Fn() -> ([u8; 32], T),
    restore: impl Fn(&[u8; 32]) -> T,
) -> anyhow::Result<Option<T>> {
    match read_secret(&path)? {
        Some(secret) => Ok(Some(restore(&secret))),
        None if create => {
            let (secret, signer) = generate();
            write_secret(&path, &secret)?;
            Ok(Some(signer))
        }
        None => Ok(None),
    }
}

impl DataDir {
    pub fn device(&self, create: bool) -> anyhow::Result<Option<DeviceSigner>> {
        load(
            self.keys().join("device.key"),
            create,
            || {
                let signer = DeviceSigner::generate();
                (signer.to_bytes(), signer)
            },
            |secret| DeviceSigner::from_bytes(secret).expect("32 bytes"),
        )
    }

    /// The principal key of one workforce, which lives next to its store. A principal has a key per
    /// workforce, so losing or replacing one touches nothing else.
    pub fn principal(&self, workforce: &WorkforceId) -> anyhow::Result<Option<PrincipalSigner>> {
        Ok(read_secret(&self.principal_path(workforce))?
            .map(|secret| PrincipalSigner::from_bytes(&secret).expect("32 bytes")))
    }

    /// Stores the principal key of a workforce that has none yet.
    pub fn store_principal(
        &self,
        workforce: &WorkforceId,
        signer: &PrincipalSigner,
    ) -> anyhow::Result<()> {
        write_secret(&self.principal_path(workforce), &signer.to_bytes())
    }

    fn principal_path(&self, workforce: &WorkforceId) -> PathBuf {
        self.workforce(workforce).join("principal.key")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn keys_are_created_once_with_private_permissions() {
        let directory = tempfile::tempdir().unwrap();
        let data = DataDir::new(directory.path().to_path_buf());
        assert!(data.device(false).unwrap().is_none());
        let created = data.device(true).unwrap().unwrap();
        let loaded = data.device(false).unwrap().unwrap();
        assert_eq!(created.key(), loaded.key());
        let mode = std::fs::metadata(data.keys().join("device.key"))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);
    }

    #[test]
    fn every_workforce_has_its_own_principal_key() {
        let directory = tempfile::tempdir().unwrap();
        let data = DataDir::new(directory.path().to_path_buf());
        let first = WorkforceId::from(wf_core::id::EventId::of(b"first"));
        let second = WorkforceId::from(wf_core::id::EventId::of(b"second"));
        assert!(data.principal(&first).unwrap().is_none());
        let signer = PrincipalSigner::generate();
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
        let mode = std::fs::metadata(data.workforce(&first).join("principal.key"))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);
    }

    #[test]
    fn malformed_key_files_are_reported() {
        let directory = tempfile::tempdir().unwrap();
        let data = DataDir::new(directory.path().to_path_buf());
        std::fs::create_dir_all(data.keys()).unwrap();
        std::fs::write(data.keys().join("device.key"), "not hex").unwrap();
        assert!(data.device(false).is_err());
        std::fs::write(data.keys().join("device.key"), "abcd").unwrap();
        assert!(data.device(false).is_err());
    }
}
