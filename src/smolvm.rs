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
    config::{Config, EnvdChannel, EnvdSource, EnvdUser},
    envd,
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
        tracing::debug!(command = %rendered, "running smolvm");
        let started = std::time::Instant::now();
        let child = tokio::process::Command::new(&self.binary)
            .args(&args)
            // Plain text: stderr is surfaced in errors and target metadata.
            .env("NO_COLOR", "1")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .with_context(|| format!("spawn {}", self.binary.display()))?;
        let output = match tokio::time::timeout(self.timeout, child.wait_with_output()).await {
            Ok(output) => output?,
            Err(_) => {
                tracing::warn!(command = %rendered, timeout_s = self.timeout.as_secs(), "smolvm timed out");
                bail!("smolvm {rendered} timed out")
            }
        };
        let elapsed_ms = started.elapsed().as_millis() as u64;
        if !output.status.success() {
            tracing::debug!(
                command = %rendered,
                elapsed_ms,
                status = %output.status,
                stderr = %String::from_utf8_lossy(&output.stderr).trim(),
                "smolvm failed"
            );
            bail!(
                "smolvm {rendered} failed ({}): {}",
                output.status,
                String::from_utf8_lossy(&output.stderr).trim()
            )
        }
        tracing::debug!(command = %rendered, elapsed_ms, "smolvm finished");
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

    let command = match template.envd_source {
        EnvdSource::Image => template.command.clone(),
        EnvdSource::Injected => {
            let mount = spec
                .envd
                .as_ref()
                .context("an injected envd needs a staged binary")?;
            let dir = mount
                .dir
                .to_str()
                .context("staged envd path is not UTF-8")?;
            args.extend(["--volume".into(), format!("{dir}:{}:ro", envd::GUEST_DIR)]);
            if template.envd_user == EnvdUser::LightspeedEnvd {
                // smolvm applies --user to the workload; init still runs as
                // root, so it can create the user first.
                args.extend(["--user".into(), envd::ENVD_USER.into()]);
            }
            if let Some(init) = envd::user_init(template) {
                args.extend(["--init".into(), init]);
            }
            envd::command(template)
        }
    };

    for command in &template.init {
        args.extend(["--init".into(), command.clone()]);
    }

    if !command.is_empty() {
        args.push("--".into());
        args.extend(command);
    }
    Ok(args)
}

/// Check a local image archive against its pinned hash. smolvm hashes and
/// stages the archive itself during create, so a swap between this check and
/// that read needs write access to the archive, which is trusted config.
async fn verify_archive(path: &std::path::Path, expected: &str) -> anyhow::Result<()> {
    let actual = envd::sha256_file(path)
        .await
        .context("hash image archive")?;
    if actual != expected {
        bail!(
            "image archive {} has sha256 {actual}, but the template pins {expected}",
            path.display()
        )
    }
    Ok(())
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
        if let Some(archive) = spec.template.local_archive() {
            let expected = spec.template.image_sha256.as_deref().unwrap_or_default();
            verify_archive(archive, expected).await?;
        }
        if let Some(mount) = &spec.envd {
            envd::stage(mount).await?;
        }
        tracing::info!(machine = %spec.name, image = %spec.template.image, "creating machine");
        self.run(create_args(spec)?).await?;
        tracing::info!(machine = %spec.name, "starting machine");
        if let Err(error) = self.run(["machine", "start", "--name", spec.name]).await {
            // Don't leave a created-but-never-started machine behind. The
            // failed record stays, so Lightspeed still sees the failure.
            if let Err(cleanup) = self.delete(spec.name).await {
                tracing::warn!(machine = %spec.name, error = format!("{cleanup:#}"), "could not remove machine after failed start");
            }
            return Err(error);
        }
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
            tracing::debug!(machine = %name, "machine already gone");
            return Ok(());
        }
        tracing::info!(machine = %name, "deleting machine");
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
        // Names only; values never reach the log.
        tracing::info!(machine = %name, env = ?values.keys().collect::<Vec<_>>(), "supplied credentials");
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::*;
    use crate::config::{CredentialRule, EgressPolicy, TemplatePolicy};

    const VM_DIR: &str = "/state/vms/ls-0123456789abcdef0123";

    fn template(envd: EnvdChannel, allow_hosts: &[&str]) -> TemplatePolicy {
        TemplatePolicy {
            template_id: "t".to_owned(),
            display_name: "T".to_owned(),
            description: None,
            image: "img@sha256:00".to_owned(),
            image_sha256: None,
            envd_source: EnvdSource::Image,
            envd_user: EnvdUser::LightspeedEnvd,
            command: vec!["lightspeed-envd".to_owned()],
            init: vec!["chown agent:agent /workspace".to_owned()],
            cpus: Some(2),
            memory_mib: Some(2048),
            envd,
            egress: EgressPolicy {
                allow_hosts: allow_hosts.iter().map(|host| host.to_string()).collect(),
            },
            credentials: Vec::new(),
            display: false,
            deprecated: false,
        }
    }

    fn injected(envd_user: EnvdUser) -> TemplatePolicy {
        TemplatePolicy {
            envd_source: EnvdSource::Injected,
            envd_user,
            command: Vec::new(),
            init: vec!["apt-get install -y git".to_owned()],
            ..template(EnvdChannel::default(), &[])
        }
    }

    fn spec<'a>(
        template: &'a TemplatePolicy,
        credentials: Vec<&'a CredentialRule>,
        envd: Option<envd::EnvdMount>,
    ) -> VmSpec<'a> {
        VmSpec {
            name: "ls-0123456789abcdef0123",
            template,
            credentials,
            labels: BTreeMap::from([("lightspeed.managed".to_owned(), "true".to_owned())]),
            vm_dir: Path::new(VM_DIR),
            host_port: Some(40001),
            envd,
        }
    }

    fn args_for(template: &TemplatePolicy, credentials: Vec<&CredentialRule>) -> Vec<String> {
        create_args(&spec(template, credentials, None)).unwrap()
    }

    fn staged() -> envd::EnvdMount {
        envd::EnvdMount {
            source: "/opt/envd".into(),
            sha256: "a".repeat(64),
            dir: format!("/state/envd/{}", "a".repeat(64)).into(),
        }
    }

    /// The values that follow each occurrence of `flag`.
    fn values<'a>(args: &'a [String], flag: &str) -> Vec<&'a str> {
        args.windows(2)
            .filter(|pair| pair[0] == flag)
            .map(|pair| pair[1].as_str())
            .collect()
    }

    fn workload(args: &[String]) -> &[String] {
        let separator = args.iter().position(|arg| arg == "--").unwrap();
        &args[separator + 1..]
    }

    #[test]
    fn injected_envd_is_mounted_read_only_and_run_as_its_own_user() {
        let template = injected(EnvdUser::LightspeedEnvd);
        let args = create_args(&spec(&template, Vec::new(), Some(staged()))).unwrap();
        assert_eq!(
            values(&args, "--volume"),
            vec![format!(
                "/state/envd/{}:/opt/lightspeed-envd:ro",
                "a".repeat(64)
            )]
        );
        assert_eq!(values(&args, "--user"), vec!["lightspeed-envd"]);
        let init = values(&args, "--init");
        assert_eq!(init.len(), 2);
        assert!(init[0].contains("useradd --system"));
        assert!(init[0].contains("adduser -S"));
        assert!(init[0].ends_with("chown lightspeed-envd /var/lib/lightspeed-envd /workspace"));
        assert_eq!(init[1], "apt-get install -y git");
        assert_eq!(
            workload(&args).join(" "),
            "/opt/lightspeed-envd/lightspeed-envd --listen unix:///var/lib/lightspeed-envd/envd.sock \
             --cwd /workspace --state-dir /var/lib/lightspeed-envd"
        );
        // The socket bridge is the same as for an image envd.
        assert_eq!(
            values(&args, "--expose-socket"),
            vec![format!(
                "/var/lib/lightspeed-envd/envd.sock:{VM_DIR}/envd.sock"
            )]
        );
    }

    #[test]
    fn root_envd_skips_user_setup() {
        let template = injected(EnvdUser::Root);
        let args = create_args(&spec(&template, Vec::new(), Some(staged()))).unwrap();
        assert!(values(&args, "--user").is_empty());
        assert_eq!(values(&args, "--init"), vec!["apt-get install -y git"]);
        assert_eq!(workload(&args)[0], "/opt/lightspeed-envd/lightspeed-envd");
    }

    #[test]
    fn injected_envd_on_a_custom_socket_owns_its_directory() {
        let template = TemplatePolicy {
            envd: EnvdChannel::ExposedSocket {
                guest_path: "/run/envd/envd.sock".to_owned(),
            },
            ..injected(EnvdUser::LightspeedEnvd)
        };
        let args = create_args(&spec(&template, Vec::new(), Some(staged()))).unwrap();
        assert!(
            values(&args, "--init")[0]
                .contains("mkdir -p /var/lib/lightspeed-envd /run/envd /workspace")
        );
        assert!(workload(&args).join(" ").ends_with("--state-dir /run/envd"));
    }

    #[test]
    fn injected_envd_needs_a_staged_binary() {
        let template = injected(EnvdUser::LightspeedEnvd);
        assert!(create_args(&spec(&template, Vec::new(), None)).is_err());
    }

    #[test]
    fn image_envd_mounts_nothing() {
        let args = args_for(&template(EnvdChannel::default(), &[]), Vec::new());
        assert!(values(&args, "--volume").is_empty());
        assert!(values(&args, "--user").is_empty());
    }

    #[test]
    fn isolated_machines_get_no_network_and_an_exposed_envd_socket() {
        let args = args_for(&template(EnvdChannel::default(), &[]), Vec::new());
        assert!(!args.contains(&"--net".to_owned()));
        let joined = args.join(" ");
        assert!(joined.contains(
            "--expose-socket /var/lib/lightspeed-envd/envd.sock:/state/vms/ls-0123456789abcdef0123/envd.sock"
        ));
        assert!(joined.contains("--label lightspeed.managed=true"));
        assert!(joined.contains("--cpus 2 --mem 2048"));
        assert!(joined.ends_with("--init chown agent:agent /workspace -- lightspeed-envd"));
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

    #[tokio::test]
    async fn archives_are_checked_against_their_pinned_hash() {
        let dir = tempfile::tempdir().unwrap();
        let archive = dir.path().join("envd.tar");
        std::fs::write(&archive, b"abc").unwrap();
        // sha256("abc")
        let sha = "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad";
        verify_archive(&archive, sha).await.unwrap();
        let error = verify_archive(&archive, &"0".repeat(64)).await.unwrap_err();
        assert!(error.to_string().contains("pins"));
        assert!(
            verify_archive(&dir.path().join("missing.tar"), sha)
                .await
                .is_err()
        );
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
