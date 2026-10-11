//! Durable ownership store.
//!
//! smolvm has no labels or metadata, so the provider records which
//! universe, binding, environment and incarnation owns each machine. Lightspeed
//! remains the source of truth for environments; this store only lets the
//! provider answer `listTargets`/`getTarget`, fence cross-binding access, and
//! reconcile after a restart.
//!
//! One JSON file per target, written atomically (temp file + rename) in a
//! private directory. The directory is part of the trust boundary: on the
//! vsock channel, envd host sockets live beside it.

use std::{
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

use anyhow::{Context as _, bail};
use environment_protocol::control::targets::ProviderBindingContext;
use serde::{Deserialize, Serialize};
use tokio::io::AsyncWriteExt as _;

use crate::{
    config::EnvdChannel,
    policy::{self, Ownership},
};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum Lifecycle {
    /// Record written; the machine may not exist yet.
    Creating,
    Active,
    /// Close accepted; the machine may still exist.
    Closing,
    Failed,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TargetRecord {
    pub target_id: String,
    pub universe_id: String,
    pub binding_id: String,
    pub environment_id: String,
    pub incarnation_id: String,
    pub request_id: String,
    pub template_id: String,
    pub image: String,
    /// Fixed at creation so later template edits cannot strand the channel.
    pub envd: EnvdChannel,
    /// Host loopback port for `EnvdChannel::LoopbackForward`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub host_port: Option<u16>,
    pub lifecycle: Lifecycle,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub failure: Option<String>,
    pub created_at_ms: u64,
    pub updated_at_ms: u64,
}

impl TargetRecord {
    pub fn belongs_to(&self, binding: &ProviderBindingContext) -> bool {
        self.universe_id == binding.universe_id && self.binding_id == binding.binding_id
    }

    pub fn ownership(&self) -> Ownership {
        Ownership {
            universe_id: self.universe_id.clone(),
            binding_id: self.binding_id.clone(),
            environment_id: self.environment_id.clone(),
            incarnation_id: self.incarnation_id.clone(),
            request_id: self.request_id.clone(),
            template_id: self.template_id.clone(),
        }
    }
}

#[derive(Clone, Debug)]
pub struct Store {
    root: PathBuf,
}

impl Store {
    pub async fn open(state_dir: &Path) -> anyhow::Result<Self> {
        for dir in [
            state_dir.to_path_buf(),
            state_dir.join("targets"),
            state_dir.join("vms"),
        ] {
            create_private_dir(&dir).await?;
        }
        Ok(Self {
            root: state_dir.to_path_buf(),
        })
    }

    /// Per-machine directory for host-side sockets and other machine files.
    pub fn vm_dir(&self, target_id: &str) -> anyhow::Result<PathBuf> {
        ensure_target_name(target_id)?;
        Ok(self.root.join("vms").join(target_id))
    }

    /// Create the machine directory and its `secrets` subdirectory.
    pub async fn ensure_vm_dir(&self, target_id: &str) -> anyhow::Result<PathBuf> {
        let dir = self.vm_dir(target_id)?;
        create_private_dir(&dir).await?;
        create_private_dir(&dir.join("secrets")).await?;
        Ok(dir)
    }

    /// Credential value files smolvm re-reads on every substituted request.
    pub fn secrets_dir(&self, target_id: &str) -> anyhow::Result<PathBuf> {
        Ok(self.vm_dir(target_id)?.join("secrets"))
    }

    pub async fn get(&self, target_id: &str) -> anyhow::Result<Option<TargetRecord>> {
        let path = self.record_path(target_id)?;
        match tokio::fs::read(&path).await {
            Ok(bytes) => Ok(Some(
                serde_json::from_slice(&bytes)
                    .with_context(|| format!("decode {}", path.display()))?,
            )),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(error).with_context(|| format!("read {}", path.display())),
        }
    }

    pub async fn put(&self, record: &TargetRecord) -> anyhow::Result<()> {
        let path = self.record_path(&record.target_id)?;
        write_private_file(&path, &serde_json::to_vec_pretty(record)?).await
    }

    pub async fn remove(&self, target_id: &str) -> anyhow::Result<()> {
        let path = self.record_path(target_id)?;
        match tokio::fs::remove_file(&path).await {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error).with_context(|| format!("remove {}", path.display())),
        }
        match tokio::fs::remove_dir_all(self.vm_dir(target_id)?).await {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error.into()),
        }
    }

    pub async fn list(&self) -> anyhow::Result<Vec<TargetRecord>> {
        let mut records = Vec::new();
        let mut entries = tokio::fs::read_dir(self.root.join("targets")).await?;
        while let Some(entry) = entries.next_entry().await? {
            let name = entry.file_name();
            let Some(target_id) = name.to_str().and_then(|name| name.strip_suffix(".json")) else {
                continue;
            };
            if !policy::is_target_name(target_id) {
                continue;
            }
            if let Some(record) = self.get(target_id).await? {
                records.push(record);
            }
        }
        records.sort_by(|left, right| left.target_id.cmp(&right.target_id));
        Ok(records)
    }

    fn record_path(&self, target_id: &str) -> anyhow::Result<PathBuf> {
        ensure_target_name(target_id)?;
        Ok(self.root.join("targets").join(format!("{target_id}.json")))
    }
}

/// Replace `path` atomically with a file only the provider user can read.
/// The rename also guarantees the leaf is a regular file, never a symlink.
pub async fn write_private_file(path: &Path, bytes: &[u8]) -> anyhow::Result<()> {
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .context("private file needs a file name")?;
    let temp = path.with_file_name(format!(".{file_name}.tmp"));
    let mut options = tokio::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    options.mode(0o600);
    let mut file = options
        .open(&temp)
        .await
        .with_context(|| format!("open {}", temp.display()))?;
    file.write_all(bytes).await?;
    file.sync_all().await?;
    drop(file);
    tokio::fs::rename(&temp, path)
        .await
        .with_context(|| format!("replace {}", path.display()))
}

pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_millis() as u64)
}

fn ensure_target_name(target_id: &str) -> anyhow::Result<()> {
    if !policy::is_target_name(target_id) {
        bail!("not a provider target id: {target_id}")
    }
    Ok(())
}

async fn create_private_dir(dir: &Path) -> anyhow::Result<()> {
    tokio::fs::create_dir_all(dir)
        .await
        .with_context(|| format!("create {}", dir.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        tokio::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))
            .await
            .with_context(|| format!("restrict {}", dir.display()))?;
    }
    // Windows: the directory inherits ACLs from its parent; place stateDir
    // under a location only the provider account can read.
    Ok(())
}

#[cfg(test)]
pub(crate) fn sample_record(target_id: &str) -> TargetRecord {
    TargetRecord {
        target_id: target_id.to_owned(),
        universe_id: "u".to_owned(),
        binding_id: "b".to_owned(),
        environment_id: "e".to_owned(),
        incarnation_id: "i".to_owned(),
        request_id: "r".to_owned(),
        template_id: "dev-small-v1".to_owned(),
        image: "img@sha256:00".to_owned(),
        envd: EnvdChannel::default(),
        host_port: Some(40000),
        lifecycle: Lifecycle::Active,
        failure: None,
        created_at_ms: 1,
        updated_at_ms: 1,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn records_round_trip_and_list_in_order() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).await.unwrap();
        let first = policy::target_name("u", "b", "e1", "i");
        let second = policy::target_name("u", "b", "e2", "i");
        assert!(store.get(&first).await.unwrap().is_none());

        store.put(&sample_record(&second)).await.unwrap();
        store.put(&sample_record(&first)).await.unwrap();
        let mut updated = sample_record(&first);
        updated.lifecycle = Lifecycle::Closing;
        store.put(&updated).await.unwrap();

        assert_eq!(store.get(&first).await.unwrap(), Some(updated));
        let ids: Vec<_> = store
            .list()
            .await
            .unwrap()
            .into_iter()
            .map(|record| record.target_id)
            .collect();
        let mut expected = vec![first.clone(), second];
        expected.sort();
        assert_eq!(ids, expected);

        store.ensure_vm_dir(&first).await.unwrap();
        store.remove(&first).await.unwrap();
        store.remove(&first).await.unwrap();
        assert!(store.get(&first).await.unwrap().is_none());
        assert!(!store.vm_dir(&first).unwrap().exists());
    }

    #[tokio::test]
    async fn foreign_ids_never_become_paths() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).await.unwrap();
        assert!(store.get("../escape").await.is_err());
        assert!(store.vm_dir("ls-../../x").is_err());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn state_is_private_to_the_provider_user() {
        use std::os::unix::fs::PermissionsExt as _;
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(&dir.path().join("state")).await.unwrap();
        let id = policy::target_name("u", "b", "e", "i");
        store.put(&sample_record(&id)).await.unwrap();
        let dir_mode = std::fs::metadata(dir.path().join("state/targets"))
            .unwrap()
            .permissions()
            .mode();
        let file_mode = std::fs::metadata(dir.path().join(format!("state/targets/{id}.json")))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(dir_mode & 0o777, 0o700);
        assert_eq!(file_mode & 0o777, 0o600);
    }
}
