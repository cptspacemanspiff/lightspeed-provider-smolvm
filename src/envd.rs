//! `lightspeed-envd` supplied by the provider rather than the image.
//!
//! The configured static binary is copied into a content-addressed directory,
//! `<stateDir>/envd/<sha256>/lightspeed-envd`, which machines mount read-only
//! at [`GUEST_DIR`] and run as their workload command. A published directory
//! is never modified: running machines have it mounted, and smolvm reuses the
//! same mount on every restart, so a machine keeps the envd it was created
//! with. Directories no machine references are removed at startup.

use std::{
    collections::BTreeSet,
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
};

use anyhow::{Context as _, bail};

use crate::config::{EnvdBinary, EnvdChannel, EnvdUser, TemplatePolicy};

/// Where the staged directory is mounted in the guest.
pub const GUEST_DIR: &str = "/opt/lightspeed-envd";
/// File name of the binary in the staged directory.
pub const BINARY_NAME: &str = "lightspeed-envd";
/// The dedicated, unprivileged user envd runs as by default. Agent commands
/// run as envd's user, so its access is the agent's access.
pub const ENVD_USER: &str = "lightspeed-envd";
/// That user's home directory, and envd's default state directory.
pub const ENVD_HOME: &str = "/var/lib/lightspeed-envd";
/// envd's working directory. smolvm mounts the machine's storage disk here.
pub const WORKDIR: &str = "/workspace";
/// Machine label recording which staged envd a machine mounts.
pub const LABEL_ENVD_SHA256: &str = "lightspeed.envd-sha256";

/// The staged envd a new machine mounts.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EnvdMount {
    /// Configured binary, copied into `dir` if it is not staged yet.
    pub source: PathBuf,
    pub sha256: String,
    /// `<stateDir>/envd/<sha256>`.
    pub dir: PathBuf,
}

impl EnvdMount {
    pub fn new(state_dir: &Path, binary: &EnvdBinary) -> Self {
        Self {
            source: binary.path.clone(),
            sha256: binary.sha256.clone(),
            dir: staged_dir(state_dir, &binary.sha256),
        }
    }
}

pub fn staged_dir(state_dir: &Path, sha256: &str) -> PathBuf {
    state_dir.join("envd").join(sha256)
}

pub fn is_sha256(value: &str) -> bool {
    value.len() == 64
        && value
            .chars()
            .all(|c| c.is_ascii_digit() || ('a'..='f').contains(&c))
}

/// Lowercase hex SHA-256 of a file.
pub async fn sha256_file(path: &Path) -> anyhow::Result<String> {
    let owned = path.to_path_buf();
    tokio::task::spawn_blocking(move || -> anyhow::Result<String> {
        use sha2::{Digest as _, Sha256};
        use std::io::Read as _;
        let mut file =
            std::fs::File::open(&owned).with_context(|| format!("open {}", owned.display()))?;
        let mut hash = Sha256::new();
        let mut buffer = vec![0; 1 << 20];
        loop {
            let read = file.read(&mut buffer)?;
            if read == 0 {
                break;
            }
            hash.update(&buffer[..read]);
        }
        Ok(hash
            .finalize()
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect())
    })
    .await?
}

/// Publish the binary into its content-addressed directory, unless it is
/// already there. The copy, not the source, is hashed, so what machines mount
/// is what the config pins.
pub async fn stage(mount: &EnvdMount) -> anyhow::Result<()> {
    if tokio::fs::try_exists(mount.dir.join(BINARY_NAME)).await? {
        tracing::debug!(sha256 = %mount.sha256, "envd already staged");
        return Ok(());
    }
    tracing::info!(source = %mount.source.display(), sha256 = %mount.sha256, "staging envd");
    let parent = mount.dir.parent().context("staged envd has no parent")?;
    create_dir_mode(parent, 0o700).await?;
    // Unique per attempt: creates for different bindings run concurrently.
    static ATTEMPT: AtomicU64 = AtomicU64::new(0);
    let temp = parent.join(format!(
        ".tmp-{}-{}-{}",
        mount.sha256,
        std::process::id(),
        ATTEMPT.fetch_add(1, Ordering::Relaxed)
    ));
    let published = async {
        // 0755 so the guest's mapped uid can read and execute it.
        create_dir_mode(&temp, 0o755).await?;
        let binary = temp.join(BINARY_NAME);
        tokio::fs::copy(&mount.source, &binary)
            .await
            .with_context(|| format!("copy envd from {}", mount.source.display()))?;
        set_mode(&binary, 0o755).await?;
        let actual = sha256_file(&binary).await?;
        if actual != mount.sha256 {
            bail!(
                "envd binary {} has sha256 {actual}, but the config pins {}",
                mount.source.display(),
                mount.sha256
            )
        }
        match tokio::fs::rename(&temp, &mount.dir).await {
            Ok(()) => Ok(()),
            // Another create published it first; that copy is just as good.
            Err(_) if tokio::fs::try_exists(mount.dir.join(BINARY_NAME)).await? => Ok(()),
            Err(error) => Err(error).with_context(|| format!("publish {}", mount.dir.display())),
        }
    }
    .await;
    if published.is_err() || tokio::fs::try_exists(&temp).await.unwrap_or(false) {
        let _ = tokio::fs::remove_dir_all(&temp).await;
    }
    published
}

/// Remove staged directories outside `keep`, and leftover temp directories.
/// Returns the removed directory names.
pub async fn collect_garbage(
    state_dir: &Path,
    keep: &BTreeSet<String>,
) -> anyhow::Result<Vec<String>> {
    let root = state_dir.join("envd");
    let mut entries = match tokio::fs::read_dir(&root).await {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error).with_context(|| format!("read {}", root.display())),
    };
    let mut removed = Vec::new();
    while let Some(entry) = entries.next_entry().await? {
        let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
            continue;
        };
        let unreferenced = is_sha256(&name) && !keep.contains(&name);
        if !(unreferenced || name.starts_with(".tmp-")) || !entry.file_type().await?.is_dir() {
            continue;
        }
        tokio::fs::remove_dir_all(entry.path())
            .await
            .with_context(|| format!("remove {}", entry.path().display()))?;
        removed.push(name);
    }
    removed.sort();
    Ok(removed)
}

/// envd's state directory: the socket's directory, so the socket lives in a
/// directory envd's user owns.
pub fn state_dir(template: &TemplatePolicy) -> &str {
    match &template.envd {
        EnvdChannel::ExposedSocket { guest_path } => guest_path
            .rsplit_once('/')
            .map(|(dir, _)| dir)
            .filter(|dir| !dir.is_empty())
            .unwrap_or("/"),
        EnvdChannel::LoopbackForward { .. } => ENVD_HOME,
    }
}

/// The workload command for an injected envd.
pub fn command(template: &TemplatePolicy) -> Vec<String> {
    let listen = match &template.envd {
        EnvdChannel::ExposedSocket { guest_path } => format!("unix://{guest_path}"),
        EnvdChannel::LoopbackForward { guest_port } => format!("0.0.0.0:{guest_port}"),
    };
    vec![
        format!("{GUEST_DIR}/{BINARY_NAME}"),
        "--listen".to_owned(),
        listen,
        "--cwd".to_owned(),
        WORKDIR.to_owned(),
        "--state-dir".to_owned(),
        state_dir(template).to_owned(),
    ]
}

/// First-start command that creates envd's user and hands it its state
/// directory and the workspace (smolvm mounts a root-owned disk there). It
/// runs as root through `sh -c`, so it must work on any distro: `useradd` on
/// Debian, Ubuntu and Fedora, BusyBox `adduser` on Alpine. `None` when envd
/// runs as root.
pub fn user_init(template: &TemplatePolicy) -> Option<String> {
    if template.envd_user == EnvdUser::Root {
        return None;
    }
    let state = state_dir(template);
    let dirs = if state == ENVD_HOME {
        format!("{ENVD_HOME} {WORKDIR}")
    } else {
        format!("{ENVD_HOME} {state} {WORKDIR}")
    };
    Some(format!(
        "set -e
id -u {ENVD_USER} >/dev/null 2>&1 || {{
  if command -v useradd >/dev/null; then
    useradd --system --create-home --home-dir {ENVD_HOME} --shell /bin/sh {ENVD_USER}
  elif command -v adduser >/dev/null; then
    adduser -S -D -h {ENVD_HOME} -s /bin/sh {ENVD_USER}
  else
    echo 'lightspeed: the image has no useradd or adduser to create {ENVD_USER}' >&2
    exit 1
  fi
}}
mkdir -p {dirs}
chown {ENVD_USER} {dirs}"
    ))
}

async fn create_dir_mode(dir: &Path, mode: u32) -> anyhow::Result<()> {
    tokio::fs::create_dir_all(dir)
        .await
        .with_context(|| format!("create {}", dir.display()))?;
    set_mode(dir, mode).await
}

async fn set_mode(path: &Path, mode: u32) -> anyhow::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        tokio::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))
            .await
            .with_context(|| format!("chmod {}", path.display()))?;
    }
    #[cfg(not(unix))]
    let _ = (path, mode);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    // sha256("abc")
    const ABC: &str = "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad";

    fn mount(dir: &Path, sha256: &str) -> EnvdMount {
        let source = dir.join("envd-src");
        std::fs::write(&source, b"abc").unwrap();
        EnvdMount::new(
            &dir.join("state"),
            &EnvdBinary {
                path: source,
                sha256: sha256.to_owned(),
            },
        )
    }

    #[tokio::test]
    async fn staging_publishes_an_immutable_content_addressed_copy() {
        let dir = tempfile::tempdir().unwrap();
        let mount = mount(dir.path(), ABC);
        stage(&mount).await.unwrap();
        let binary = mount.dir.join(BINARY_NAME);
        assert_eq!(mount.dir, dir.path().join("state/envd").join(ABC));
        assert_eq!(std::fs::read(&binary).unwrap(), b"abc");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let mode = |path: &Path| std::fs::metadata(path).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode(&binary), 0o755);
            assert_eq!(mode(&mount.dir), 0o755);
        }

        // A published directory is never rewritten, even if the source moves.
        std::fs::write(&mount.source, b"changed").unwrap();
        stage(&mount).await.unwrap();
        assert_eq!(std::fs::read(&binary).unwrap(), b"abc");
        let leftovers: Vec<_> = std::fs::read_dir(dir.path().join("state/envd"))
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect();
        assert_eq!(leftovers, vec![std::ffi::OsString::from(ABC)]);
    }

    #[tokio::test]
    async fn a_hash_mismatch_publishes_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let mount = mount(dir.path(), &"0".repeat(64));
        let error = stage(&mount).await.unwrap_err();
        assert!(format!("{error:#}").contains("config pins"));
        assert!(!mount.dir.exists());
        assert_eq!(
            std::fs::read_dir(dir.path().join("state/envd"))
                .unwrap()
                .count(),
            0
        );
    }

    #[tokio::test]
    async fn garbage_collection_keeps_referenced_directories() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("envd");
        let kept = "a".repeat(64);
        let stale = "b".repeat(64);
        for name in [kept.as_str(), stale.as_str(), ".tmp-x-1", "not-a-sha"] {
            std::fs::create_dir_all(root.join(name)).unwrap();
        }
        let removed = collect_garbage(dir.path(), &BTreeSet::from([kept.clone()]))
            .await
            .unwrap();
        assert_eq!(removed, vec![".tmp-x-1".to_owned(), stale.clone()]);
        assert!(root.join(&kept).exists());
        assert!(root.join("not-a-sha").exists());
        assert!(!root.join(&stale).exists());
        let empty = tempfile::tempdir().unwrap();
        assert!(
            collect_garbage(empty.path(), &BTreeSet::new())
                .await
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn hashes_are_lowercase_hex() {
        assert!(is_sha256(ABC));
        assert!(!is_sha256(&ABC.to_uppercase()));
        assert!(!is_sha256("abc"));
    }
}
