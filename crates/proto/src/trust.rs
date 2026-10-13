//! Who is trusted: the host's paired devices and the viewer's known hosts.
//!
//! These lists are authorization data, not secrets: anyone who can edit them can grant
//! access. Files are written atomically and, on Unix, readable and writable only by the
//! owner; on Windows they live in the user's profile, whose ACL is per-user.

use crate::identity::Fingerprint;
use crate::Error;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

const FORMAT_VERSION: u32 = 1;

fn now_secs() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs())
}

fn load_json<T: for<'de> Deserialize<'de> + Default>(path: &Path) -> Result<T, Error> {
    match std::fs::read(path) {
        Ok(data) => Ok(serde_json::from_slice(&data)?),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(T::default()),
        Err(e) => Err(e.into()),
    }
}

fn save_json<T: Serialize>(path: &Path, value: &T) -> Result<(), Error> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = path.with_extension("tmp");
    let data = serde_json::to_vec_pretty(value)?;
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    {
        use std::io::Write;
        let mut file = options.open(&tmp)?;
        file.write_all(&data)?;
        file.sync_all()?;
    }
    std::fs::rename(&tmp, path)?;
    Ok(())
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct PairedClient {
    pub fingerprint: Fingerprint,
    pub name: String,
    pub paired_at: u64,
    #[serde(default)]
    pub revoked: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClientStatus {
    Paired,
    Revoked,
    Unknown,
}

#[derive(Serialize, Deserialize)]
struct ClientsFile {
    version: u32,
    clients: Vec<PairedClient>,
}

impl Default for ClientsFile {
    fn default() -> Self {
        ClientsFile { version: FORMAT_VERSION, clients: Vec::new() }
    }
}

/// The host's paired devices. With a path, every read goes to disk, so a change made
/// by another process (such as a revocation from the command line) applies to the
/// next connection.
pub struct PairedClients {
    path: Option<PathBuf>,
    memory: ClientsFile,
}

impl PairedClients {
    pub fn in_memory() -> Self {
        PairedClients { path: None, memory: ClientsFile::default() }
    }

    pub fn at(path: impl Into<PathBuf>) -> Self {
        PairedClients { path: Some(path.into()), memory: ClientsFile::default() }
    }

    fn read(&self) -> Result<ClientsFile, Error> {
        match &self.path {
            Some(p) => {
                let file: ClientsFile = load_json(p)?;
                if file.version != FORMAT_VERSION {
                    return Err(Error::Protocol(format!("{} has unsupported format version {}", p.display(), file.version)));
                }
                Ok(file)
            }
            None => Ok(ClientsFile { version: FORMAT_VERSION, clients: self.memory.clients.clone() }),
        }
    }

    fn write(&mut self, file: ClientsFile) -> Result<(), Error> {
        match &self.path {
            Some(p) => save_json(p, &file),
            None => {
                self.memory = file;
                Ok(())
            }
        }
    }

    pub fn list(&self) -> Result<Vec<PairedClient>, Error> {
        Ok(self.read()?.clients)
    }

    /// Unreadable storage counts as unknown, so it can never grant access.
    pub fn status(&self, fingerprint: &Fingerprint) -> ClientStatus {
        match self.read().map(|f| f.clients.into_iter().find(|c| &c.fingerprint == fingerprint)) {
            Ok(Some(c)) if c.revoked => ClientStatus::Revoked,
            Ok(Some(_)) => ClientStatus::Paired,
            _ => ClientStatus::Unknown,
        }
    }

    pub fn add(&mut self, fingerprint: Fingerprint, name: &str) -> Result<(), Error> {
        let mut file = self.read()?;
        if file.clients.iter().any(|c| c.fingerprint == fingerprint) {
            return Err(Error::Protocol("device is already listed".into()));
        }
        file.clients.push(PairedClient { fingerprint, name: name.to_string(), paired_at: now_secs(), revoked: false });
        self.write(file)
    }

    /// Returns false if the device was not listed. Revoked devices stay listed so they
    /// cannot simply pair again.
    pub fn revoke(&mut self, fingerprint: &Fingerprint) -> Result<bool, Error> {
        let mut file = self.read()?;
        let Some(client) = file.clients.iter_mut().find(|c| &c.fingerprint == fingerprint) else {
            return Ok(false);
        };
        client.revoked = true;
        self.write(file)?;
        Ok(true)
    }
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct KnownHost {
    /// The address as the user typed it (lowercased), e.g. "macbook-air:47900".
    pub address: String,
    pub fingerprint: Fingerprint,
    pub name: String,
    pub added_at: u64,
}

#[derive(Serialize, Deserialize)]
struct HostsFile {
    version: u32,
    hosts: Vec<KnownHost>,
}

impl Default for HostsFile {
    fn default() -> Self {
        HostsFile { version: FORMAT_VERSION, hosts: Vec::new() }
    }
}

/// The viewer's pinned hosts, like SSH's known_hosts.
pub struct KnownHosts {
    path: PathBuf,
}

impl KnownHosts {
    pub fn at(path: impl Into<PathBuf>) -> Self {
        KnownHosts { path: path.into() }
    }

    fn read(&self) -> Result<HostsFile, Error> {
        let file: HostsFile = load_json(&self.path)?;
        if file.version != FORMAT_VERSION {
            return Err(Error::Protocol(format!("{} has unsupported format version {}", self.path.display(), file.version)));
        }
        Ok(file)
    }

    pub fn get(&self, address: &str) -> Result<Option<KnownHost>, Error> {
        let address = address.to_lowercase();
        Ok(self.read()?.hosts.into_iter().find(|h| h.address == address))
    }

    pub fn add(&self, address: &str, fingerprint: Fingerprint, name: &str) -> Result<(), Error> {
        let address = address.to_lowercase();
        let mut file = self.read()?;
        file.hosts.retain(|h| h.address != address);
        file.hosts.push(KnownHost { address, fingerprint, name: name.to_string(), added_at: now_secs() });
        save_json(&self.path, &file)
    }

    pub fn forget(&self, address: &str) -> Result<bool, Error> {
        let address = address.to_lowercase();
        let mut file = self.read()?;
        let before = file.hosts.len();
        file.hosts.retain(|h| h.address != address);
        let removed = file.hosts.len() != before;
        if removed {
            save_json(&self.path, &file)?;
        }
        Ok(removed)
    }
}
