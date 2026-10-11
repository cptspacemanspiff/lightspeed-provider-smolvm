use std::{
    collections::BTreeMap,
    net::SocketAddr,
    ops::Deref,
    path::{Path, PathBuf},
    sync::Arc,
};

use anyhow::{Context as _, bail};
use clap::Parser;
use serde::{Deserialize, Serialize};

#[derive(Parser, Debug)]
#[command(name = "lightspeed-provider-smolvm", version)]
pub struct ProviderArgs {
    #[arg(long, env = "LIGHTSPEED_SMOLVM_PROVIDER_CONFIG")]
    pub config: PathBuf,
}

impl ProviderArgs {
    pub async fn load(self) -> anyhow::Result<Config> {
        let bytes = tokio::fs::read(&self.config)
            .await
            .with_context(|| format!("read {}", self.config.display()))?;
        Config::from_json(&bytes)
    }
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct RawConfig {
    controller_listen: SocketAddr,
    state_dir: PathBuf,
    #[serde(default)]
    smolvm: SmolvmConfig,
    templates: Vec<TemplatePolicy>,
    #[serde(default)]
    credentials: Vec<CredentialRule>,
    #[serde(default)]
    unknown_secret_env: UnknownSecretPolicy,
    relay_idle_seconds: Option<u64>,
    dial_timeout_seconds: Option<u64>,
}

#[derive(Clone, Debug)]
pub struct Config(Arc<ConfigInner>);

impl Deref for Config {
    type Target = ConfigInner;

    fn deref(&self) -> &ConfigInner {
        &self.0
    }
}

#[derive(Debug)]
pub struct ConfigInner {
    pub controller_listen: SocketAddr,
    /// Absolute: smolvm reads secret files and serves envd sockets by path.
    pub state_dir: PathBuf,
    pub smolvm: SmolvmConfig,
    pub templates: BTreeMap<String, TemplatePolicy>,
    /// Credential rules keyed by the environment-variable name Lightspeed
    /// binds (the `secretEnv` key).
    pub credentials: BTreeMap<String, CredentialRule>,
    pub unknown_secret_env: UnknownSecretPolicy,
    pub relay_idle_seconds: u64,
    pub dial_timeout_seconds: u64,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SmolvmConfig {
    #[serde(default = "default_binary")]
    pub binary: PathBuf,
    #[serde(default = "default_command_timeout_seconds")]
    pub command_timeout_seconds: u64,
}

impl Default for SmolvmConfig {
    fn default() -> Self {
        Self {
            binary: default_binary(),
            command_timeout_seconds: default_command_timeout_seconds(),
        }
    }
}

fn default_binary() -> PathBuf {
    PathBuf::from("smolvm")
}

fn default_command_timeout_seconds() -> u64 {
    120
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct TemplatePolicy {
    pub template_id: String,
    pub display_name: String,
    #[serde(default)]
    pub description: Option<String>,
    /// OCI image with `lightspeed-envd` installed, pinned by digest
    /// (`name@sha256:...`) so a moved tag cannot change what a template creates.
    pub image: String,
    /// Workload command (the image's entrypoint when empty). It must start
    /// envd listening where `envd` says.
    #[serde(default)]
    pub command: Vec<String>,
    #[serde(default)]
    pub cpus: Option<u8>,
    #[serde(default)]
    pub memory_mib: Option<u32>,
    #[serde(default)]
    pub envd: EnvdChannel,
    #[serde(default)]
    pub egress: EgressPolicy,
    /// Names of provider credential rules machines from this template get.
    /// Their hosts are added to the egress allowlist.
    #[serde(default)]
    pub credentials: Vec<String>,
    #[serde(default)]
    pub deprecated: bool,
}

/// How the provider reaches `lightspeed-envd` inside the guest.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum EnvdChannel {
    /// envd listens on a Unix socket in the guest, which smolvm bridges over
    /// vsock to a socket in the provider's private per-machine directory
    /// (`--expose-socket`). The guest needs no network for this channel.
    #[serde(rename_all = "camelCase")]
    ExposedSocket { guest_path: String },
    /// envd listens on a guest TCP port published to a host port. Works with
    /// a stock TCP envd; development only, since the host bind address is
    /// smolvm's choice.
    #[serde(rename_all = "camelCase")]
    LoopbackForward { guest_port: u16 },
}

impl Default for EnvdChannel {
    fn default() -> Self {
        Self::ExposedSocket {
            guest_path: "/run/lightspeed/envd.sock".to_owned(),
        }
    }
}

/// Outbound hosts the guest may reach. With no hosts here and no credential
/// rules, the machine gets no network at all.
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct EgressPolicy {
    #[serde(default)]
    pub allow_hosts: Vec<String>,
}

/// A Lightspeed credential binding the provider keeps out of the guest.
///
/// When a `process/start` or `job/start` carries `secretEnv[envName]`, the
/// provider strips the value before it reaches envd and writes it to the
/// machine's secret file. smolvm's credential substitution re-reads that file
/// on every request and puts the value only into HTTPS request headers sent to
/// `hosts`; the guest sees a placeholder.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct CredentialRule {
    pub env_name: String,
    pub hosts: Vec<String>,
}

/// What to do with a `secretEnv` entry that has no credential rule.
#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum UnknownSecretPolicy {
    /// Refuse the request so the secret never reaches the guest.
    #[default]
    Reject,
    /// Forward the real value to envd, as Lightspeed does without this
    /// provider. The secret then lives in guest memory.
    PassThrough,
}

/// `sun_path` is 104 bytes on macOS and 108 on Linux; leave room for
/// `vms/ls-<20 hex>/envd.sock` under the state directory.
const MAX_STATE_DIR_BYTES: usize = 64;

impl Config {
    pub fn from_json(bytes: &[u8]) -> anyhow::Result<Self> {
        let raw: RawConfig =
            serde_json::from_slice(bytes).context("decode provider config JSON")?;
        if !raw.state_dir.is_absolute() {
            bail!("stateDir must be an absolute path")
        }
        if raw.state_dir.as_os_str().len() > MAX_STATE_DIR_BYTES {
            bail!("stateDir must be at most {MAX_STATE_DIR_BYTES} bytes so envd socket paths fit")
        }
        let mut credentials = BTreeMap::new();
        for rule in raw.credentials {
            validate_env_name(&rule.env_name)?;
            if rule.hosts.is_empty() {
                bail!("credential {} must allow at least one host", rule.env_name)
            }
            for host in &rule.hosts {
                validate_host(host).with_context(|| format!("credential {}", rule.env_name))?;
            }
            if credentials.insert(rule.env_name.clone(), rule).is_some() {
                bail!("duplicate credential rule")
            }
        }
        if raw.templates.is_empty() {
            bail!("provider must configure at least one template")
        }
        let mut templates = BTreeMap::new();
        for template in raw.templates {
            validate_template(&template, &credentials)
                .with_context(|| format!("template {}", template.template_id))?;
            if templates
                .insert(template.template_id.clone(), template)
                .is_some()
            {
                bail!("duplicate template id")
            }
        }
        Ok(Self(Arc::new(ConfigInner {
            controller_listen: raw.controller_listen,
            state_dir: raw.state_dir,
            smolvm: raw.smolvm,
            templates,
            credentials,
            unknown_secret_env: raw.unknown_secret_env,
            relay_idle_seconds: raw.relay_idle_seconds.unwrap_or(300),
            dial_timeout_seconds: raw.dial_timeout_seconds.unwrap_or(5),
        })))
    }

    pub fn template(&self, template_id: &str) -> anyhow::Result<&TemplatePolicy> {
        self.templates
            .get(template_id)
            .ok_or_else(|| anyhow::anyhow!("unknown template: {template_id}"))
    }

    /// Credential rules machines from `template_id` receive. Unknown
    /// templates get none, so their secrets fall to `unknown_secret_env`.
    pub fn template_rules(&self, template_id: &str) -> BTreeMap<String, CredentialRule> {
        self.templates
            .get(template_id)
            .into_iter()
            .flat_map(|template| &template.credentials)
            .filter_map(|name| self.credentials.get(name))
            .map(|rule| (rule.env_name.clone(), rule.clone()))
            .collect()
    }

    pub fn state_dir(&self) -> &Path {
        &self.state_dir
    }
}

fn validate_template(
    template: &TemplatePolicy,
    credentials: &BTreeMap<String, CredentialRule>,
) -> anyhow::Result<()> {
    if template.template_id.is_empty() {
        bail!("template id must not be empty")
    }
    if !template.image.contains("@sha256:") {
        bail!("image must be pinned by digest (name@sha256:...)")
    }
    if template.cpus == Some(0) || template.memory_mib == Some(0) {
        bail!("cpus and memoryMib must be positive")
    }
    if let EnvdChannel::ExposedSocket { guest_path } = &template.envd
        && (!guest_path.starts_with('/') || guest_path.contains(':'))
    {
        bail!("envd guestPath must be an absolute guest path without ':'")
    }
    for host in &template.egress.allow_hosts {
        validate_host(host)?;
    }
    for name in &template.credentials {
        if !credentials.contains_key(name) {
            bail!("unknown credential rule {name}")
        }
    }
    Ok(())
}

/// Same rule Lightspeed applies to credential binding names.
fn validate_env_name(name: &str) -> anyhow::Result<()> {
    let mut chars = name.chars();
    let valid = name.len() <= 128
        && chars
            .next()
            .is_some_and(|first| first.is_ascii_alphabetic() || first == '_')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_');
    if !valid {
        bail!("invalid environment variable name: {name}")
    }
    Ok(())
}

/// Lowercase DNS name or whole-label `*.` wildcard; no scheme, port or path.
/// Matches what smolvm accepts for credential hosts.
fn validate_host(host: &str) -> anyhow::Result<()> {
    let name = host.strip_prefix("*.").unwrap_or(host);
    let valid = name.contains('.')
        && name.split('.').all(|label| {
            !label.is_empty()
                && label
                    .chars()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
        });
    if !valid {
        bail!("invalid host: {host}")
    }
    Ok(())
}

#[cfg(test)]
pub(crate) fn test_config(state_dir: &Path) -> Config {
    let json = include_str!("../config.example.json").replace(
        "/var/lib/lightspeed-provider-smolvm",
        state_dir.to_str().unwrap(),
    );
    Config::from_json(json.as_bytes()).expect("test config")
}

#[cfg(test)]
mod tests {
    use super::*;

    const EXAMPLE: &str = include_str!("../config.example.json");

    #[test]
    fn example_config_parses() {
        let config = Config::from_json(EXAMPLE.as_bytes()).expect("example config");
        assert!(config.templates.contains_key("dev-small-v1"));
        assert!(config.credentials.contains_key("GITHUB_TOKEN"));
        assert_eq!(config.unknown_secret_env, UnknownSecretPolicy::Reject);
        assert_eq!(
            config
                .template_rules("dev-small-v1")
                .keys()
                .collect::<Vec<_>>(),
            vec!["GITHUB_TOKEN"]
        );
        assert!(config.template_rules("missing").is_empty());
    }

    #[test]
    fn unpinned_images_are_rejected() {
        let json = EXAMPLE.replace(
            "ghcr.io/example/lightspeed-envd@sha256:0000000000000000000000000000000000000000000000000000000000000000",
            "ghcr.io/example/lightspeed-envd:latest",
        );
        let error = Config::from_json(json.as_bytes()).unwrap_err();
        assert!(format!("{error:#}").contains("pinned by digest"));
    }

    #[test]
    fn state_dir_must_be_absolute_and_short() {
        let relative = EXAMPLE.replace("/var/lib/lightspeed-provider-smolvm", "state");
        assert!(Config::from_json(relative.as_bytes()).is_err());
        let long = EXAMPLE.replace(
            "/var/lib/lightspeed-provider-smolvm",
            &format!("/{}", "x".repeat(80)),
        );
        assert!(Config::from_json(long.as_bytes()).is_err());
    }

    #[test]
    fn templates_may_only_reference_configured_credentials() {
        let json = EXAMPLE.replace(
            r#""credentials": ["GITHUB_TOKEN"]"#,
            r#""credentials": ["NOPE"]"#,
        );
        let error = Config::from_json(json.as_bytes()).unwrap_err();
        assert!(format!("{error:#}").contains("unknown credential rule"));
    }

    #[test]
    fn hosts_must_be_bare_lowercase_names() {
        assert!(validate_host("api.github.com").is_ok());
        assert!(validate_host("*.githubusercontent.com").is_ok());
        assert!(validate_host("https://api.github.com").is_err());
        assert!(validate_host("api.github.com:443").is_err());
        assert!(validate_host("API.github.com").is_err());
        assert!(validate_host("localhost").is_err());
        assert!(validate_host("").is_err());
    }
}
