//! `VmBackend` implemented by driving the `smolvm` CLI.
//!
//! Flags are checked against the CLI definitions in `third_party/smolvm`
//! (`src/cli/machine.rs`). Bump the submodule and re-check them together.

use std::{collections::BTreeMap, ffi::OsString, process::Stdio, time::Duration};

use anyhow::{Context as _, bail};
use async_trait::async_trait;
use environment_protocol::shared::SecretString;
use serde::Deserialize;

use crate::{
    backend::{MachineInfo, VmBackend, VmSpec, VmState},
    channel::ENVD_SOCKET,
    config::{Config, EnvdChannel},
    store::{Store, write_private_file},
};

#[derive(Clone)]
pub struct SmolvmCli {
    binary: std::path::PathBuf,
    timeout: Duration,
    store: Store,
}

impl SmolvmCli {
    pub fn new(config: &Config, store: Store) -> Self {
        Self {
            binary: config.smolvm.binary.clone(),
            timeout: Duration::from_secs(config.smolvm.command_timeout_seconds),
            store,
        }
    }

    async fn run<I, S>(&self, args: I) -> anyhow::Result<String>
    where
        I: IntoIterator<Item = S>,
        S: Into<OsString>,
    {
        let args: Vec<OsString> = args.into_iter().map(Into::into).collect();
        let rendered = args
            .iter()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect::<Vec<_>>()
            .join(" ");
        let child = tokio::process::Command::new(&self.binary)
            .args(&args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .with_context(|| format!("spawn {}", self.binary.display()))?;
        let output = tokio::time::timeout(self.timeout, child.wait_with_output())
            .await
            .with_context(|| format!("smolvm {rendered} timed out"))??;
        if !output.status.success() {
            bail!(
                "smolvm {rendered} failed ({}): {}",
                output.status,
                String::from_utf8_lossy(&output.stderr).trim()
            )
        }
        Ok(String::from_utf8_lossy(&output.stdout).into_owned())
    }
}

/// Arguments for `smolvm machine create`.
///
/// Network rule: the machine gets `--net` only together with an allowlist
/// (template egress hosts plus credential hosts). `--net` alone would open
/// the whole internet.
pub fn create_args(spec: &VmSpec<'_>) -> anyhow::Result<Vec<String>> {
    let template = spec.template;
    let mut args: Vec<String> = vec![
        "machine".into(),
        "create".into(),
        "--name".into(),
        spec.name.into(),
        "--image".into(),
        template.image.clone(),
    ];
    for (key, value) in &spec.labels {
        args.extend(["--label".into(), format!("{key}={value}")]);
    }
    if let Some(cpus) = template.cpus {
        args.extend(["--cpus".into(), cpus.to_string()]);
    }
    if let Some(memory) = template.memory_mib {
        args.extend(["--mem".into(), memory.to_string()]);
    }

    match &template.envd {
        EnvdChannel::ExposedSocket { guest_path } => {
            let host = spec.vm_dir.join(ENVD_SOCKET);
            let host = host.to_str().context("envd socket path is not UTF-8")?;
            args.extend(["--expose-socket".into(), format!("{guest_path}:{host}")]);
        }
        EnvdChannel::LoopbackForward { guest_port } => {
            let host_port = spec
                .host_port
                .context("loopback envd channel needs a host port")?;
            args.extend(["--port".into(), format!("{host_port}:{guest_port}")]);
        }
    }

    let mut allowed: Vec<&str> = template
        .egress
        .allow_hosts
        .iter()
        .map(String::as_str)
        .collect();
    for rule in &spec.credentials {
        allowed.extend(rule.hosts.iter().map(String::as_str));
    }
    allowed.sort_unstable();
    allowed.dedup();
    if !allowed.is_empty() {
        args.push("--net".into());
        for host in allowed {
            if host.starts_with("*.") {
                args.extend(["--allow-host-pattern".into(), host.into()]);
            } else {
                args.extend(["--allow-host".into(), host.into()]);
            }
        }
    }

    // The guest gets a placeholder in ENV; smolvm resolves the real value
    // from the same-named secret file on every request to the rule's hosts,
    // and withholds that secret from the guest environment.
    for rule in &spec.credentials {
        let name = &rule.env_name;
        let file = spec.vm_dir.join("secrets").join(name);
        let file = file.to_str().context("secret path is not UTF-8")?;
        args.extend([
            "--credential".into(),
            format!(
                "{}={name}@{}",
                name.to_ascii_lowercase(),
                rule.hosts.join(",")
            ),
            "--secret-file".into(),
            format!("{name}={file}"),
        ]);
    }

    if !template.command.is_empty() {
        args.push("--".into());
        args.extend(template.command.iter().cloned());
    }
    Ok(args)
}

#[derive(Deserialize)]
struct LsEntry {
    name: String,
    #[serde(default)]
    state: String,
    #[serde(default)]
    labels: BTreeMap<String, String>,
}

/// `RecordState` display names from smolvm's `config.rs`.
fn parse_state(state: &str) -> VmState {
    match state {
        "running" => VmState::Running,
        "paused" => VmState::Suspended,
        "frozen" => VmState::Frozen,
        "stopped" | "created" => VmState::Stopped,
        "failed" => VmState::Failed,
        _ => VmState::Unknown,
    }
}

#[async_trait]
impl VmBackend for SmolvmCli {
    async fn health(&self) -> anyhow::Result<String> {
        Ok(self.run(["--version"]).await?.trim().to_owned())
    }

    async fn create_and_start(&self, spec: &VmSpec<'_>) -> anyhow::Result<()> {
        // Secret files must exist before the machine reads them; an empty
        // value fails closed at the upstream until a start supplies one.
        for rule in &spec.credentials {
            let path = spec.vm_dir.join("secrets").join(&rule.env_name);
            if tokio::fs::metadata(&path).await.is_err() {
                write_private_file(&path, b"").await?;
            }
        }
        self.run(create_args(spec)?).await?;
        self.run(["machine", "start", "--name", spec.name]).await?;
        Ok(())
    }

    async fn start(&self, name: &str) -> anyhow::Result<()> {
        self.run(["machine", "start", "--name", name])
            .await
            .map(drop)
    }

    async fn stop(&self, name: &str) -> anyhow::Result<()> {
        self.run(["machine", "stop", "--name", name])
            .await
            .map(drop)
    }

    async fn suspend(&self, name: &str) -> anyhow::Result<()> {
        self.run(["machine", "pause", "--name", name])
            .await
            .map(drop)
    }

    async fn resume(&self, name: &str) -> anyhow::Result<()> {
        self.run(["machine", "resume", "--name", name])
            .await
            .map(drop)
    }

    async fn delete(&self, name: &str) -> anyhow::Result<()> {
        if self.state(name).await? == VmState::Missing {
            return Ok(());
        }
        self.run(["machine", "delete", "--name", name, "--force"])
            .await
            .map(drop)
    }

    async fn machines(&self) -> anyhow::Result<Vec<MachineInfo>> {
        let stdout = self.run(["machine", "ls", "--json"]).await?;
        let entries: Vec<LsEntry> = if stdout.trim().is_empty() {
            Vec::new()
        } else {
            serde_json::from_str(&stdout).context("decode `smolvm machine ls --json`")?
        };
        Ok(entries
            .into_iter()
            .map(|entry| MachineInfo {
                state: parse_state(&entry.state),
                name: entry.name,
                labels: entry.labels,
            })
            .collect())
    }

    async fn supply_credentials(
        &self,
        name: &str,
        values: &BTreeMap<String, SecretString>,
    ) -> anyhow::Result<()> {
        let secrets = self.store.secrets_dir(name)?;
        for (env_name, value) in values {
            let path = secrets.join(env_name);
            // Only rules registered at create time have a file; never
            // create one for a name smolvm was not told about.
            if tokio::fs::metadata(&path).await.is_err() {
                bail!("machine has no credential binding for {env_name}")
            }
            write_private_file(&path, value.expose().as_bytes()).await?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::*;
    use crate::config::{CredentialRule, EgressPolicy, TemplatePolicy};

    fn template(envd: EnvdChannel, allow_hosts: &[&str]) -> TemplatePolicy {
        TemplatePolicy {
            template_id: "t".to_owned(),
            display_name: "T".to_owned(),
            description: None,
            image: "img@sha256:00".to_owned(),
            command: vec!["lightspeed-envd".to_owned()],
            cpus: Some(2),
            memory_mib: Some(2048),
            envd,
            egress: EgressPolicy {
                allow_hosts: allow_hosts.iter().map(|host| host.to_string()).collect(),
            },
            credentials: Vec::new(),
            deprecated: false,
        }
    }

    fn args_for(template: &TemplatePolicy, credentials: Vec<&CredentialRule>) -> Vec<String> {
        create_args(&VmSpec {
            name: "ls-0123456789abcdef0123",
            template,
            credentials,
            labels: BTreeMap::from([("lightspeed.managed".to_owned(), "true".to_owned())]),
            vm_dir: Path::new("/state/vms/ls-0123456789abcdef0123"),
            host_port: Some(40001),
        })
        .unwrap()
    }

    #[test]
    fn isolated_machines_get_no_network_and_an_exposed_envd_socket() {
        let args = args_for(&template(EnvdChannel::default(), &[]), Vec::new());
        assert!(!args.contains(&"--net".to_owned()));
        let joined = args.join(" ");
        assert!(joined.contains(
            "--expose-socket /run/lightspeed/envd.sock:/state/vms/ls-0123456789abcdef0123/envd.sock"
        ));
        assert!(joined.contains("--label lightspeed.managed=true"));
        assert!(joined.contains("--cpus 2 --mem 2048"));
        assert!(joined.ends_with("-- lightspeed-envd"));
    }

    #[test]
    fn network_is_only_enabled_with_an_allowlist() {
        let args = args_for(
            &template(EnvdChannel::default(), &["pypi.org", "*.pythonhosted.org"]),
            Vec::new(),
        )
        .join(" ");
        assert!(
            args.contains("--net --allow-host-pattern *.pythonhosted.org --allow-host pypi.org")
        );
    }

    #[test]
    fn credentials_bind_secret_files_and_extend_the_allowlist() {
        let rule = CredentialRule {
            env_name: "GITHUB_TOKEN".to_owned(),
            hosts: vec!["api.github.com".to_owned(), "github.com".to_owned()],
        };
        let args = args_for(&template(EnvdChannel::default(), &[]), vec![&rule]).join(" ");
        assert!(args.contains("--net --allow-host api.github.com --allow-host github.com"));
        assert!(args.contains(
            "--credential github_token=GITHUB_TOKEN@api.github.com,github.com \
             --secret-file GITHUB_TOKEN=/state/vms/ls-0123456789abcdef0123/secrets/GITHUB_TOKEN"
        ));
    }

    #[test]
    fn loopback_channel_publishes_the_envd_port() {
        let args = args_for(
            &template(EnvdChannel::LoopbackForward { guest_port: 19091 }, &[]),
            Vec::new(),
        )
        .join(" ");
        assert!(args.contains("--port 40001:19091"));
        assert!(!args.contains("--net"));
    }

    #[test]
    fn states_follow_smolvm_record_names() {
        assert_eq!(parse_state("running"), VmState::Running);
        assert_eq!(parse_state("paused"), VmState::Suspended);
        assert_eq!(parse_state("created"), VmState::Stopped);
        assert_eq!(parse_state("unreachable"), VmState::Unknown);
    }

    #[tokio::test]
    async fn supplied_credentials_only_replace_registered_files() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).await.unwrap();
        let name = crate::policy::target_name("u", "b", "e", "i");
        let vm_dir = store.ensure_vm_dir(&name).await.unwrap();
        write_private_file(&vm_dir.join("secrets/GITHUB_TOKEN"), b"")
            .await
            .unwrap();
        let config = crate::config::test_config(dir.path());
        let backend = SmolvmCli::new(&config, store);

        let values = BTreeMap::from([("GITHUB_TOKEN".to_owned(), SecretString::new("ghp_1"))]);
        backend.supply_credentials(&name, &values).await.unwrap();
        assert_eq!(
            std::fs::read_to_string(vm_dir.join("secrets/GITHUB_TOKEN")).unwrap(),
            "ghp_1"
        );

        let unknown = BTreeMap::from([("OTHER".to_owned(), SecretString::new("x"))]);
        assert!(backend.supply_credentials(&name, &unknown).await.is_err());
        assert!(!vm_dir.join("secrets/OTHER").exists());
    }
}
