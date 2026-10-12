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

use crate::envd;

#[derive(Parser, Debug)]
#[command(name = "lightspeed-provider-smolvm", version)]
pub struct ProviderArgs {
    #[arg(long, env = "LIGHTSPEED_SMOLVM_PROVIDER_CONFIG")]
    pub config: PathBuf,
    /// Log line format on stdout.
    #[arg(
        long,
        env = "LIGHTSPEED_SMOLVM_PROVIDER_LOG_FORMAT",
        value_enum,
        default_value_t
    )]
    pub log_format: crate::telemetry::LogFormat,
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
    #[serde(default)]
    state_dir: Option<PathBuf>,
    #[serde(default)]
    smolvm: SmolvmConfig,
    #[serde(default)]
    envd: EnvdConfig,
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
    /// The envd binary for this host's architecture, which `injected`
    /// templates mount. Present whenever such a template exists.
    pub envd_binary: Option<EnvdBinary>,
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

/// envd binaries the provider injects, keyed by guest architecture
/// (`std::env::consts::ARCH`; the guest's equals the host's).
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct EnvdConfig {
    #[serde(default)]
    pub binaries: BTreeMap<String, EnvdBinary>,
}

/// A static (musl) `lightspeed-envd`, pinned by hash.
#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct EnvdBinary {
    pub path: PathBuf,
    pub sha256: String,
}

/// Guest architectures an envd binary may be configured for.
const ENVD_ARCHES: [&str; 2] = ["x86_64", "aarch64"];

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct TemplatePolicy {
    pub template_id: String,
    pub display_name: String,
    #[serde(default)]
    pub description: Option<String>,
    /// A registry reference pinned by digest (`name@sha256:...`), or an
    /// absolute path to a local `docker save` archive pinned by
    /// `image_sha256`. Either way a moved tag or rewritten file cannot change
    /// what a template creates. Any Linux image with `/bin/sh` works with an
    /// injected envd.
    pub image: String,
    /// SHA-256 of the local archive in `image`; checked before each create.
    #[serde(default)]
    pub image_sha256: Option<String>,
    /// Where envd comes from. Defaults to `injected`.
    #[serde(default)]
    pub envd_source: EnvdSource,
    /// The user an injected envd runs as, and so the user agent commands run
    /// as. Defaults to a dedicated unprivileged `lightspeed-envd` user.
    #[serde(default)]
    pub envd_user: EnvdUser,
    /// Workload command, only for `envdSource: "image"` (the image's
    /// entrypoint when empty). It must start envd listening where `envd`
    /// says. An injected envd's command is generated.
    #[serde(default)]
    pub command: Vec<String>,
    /// Commands run once, as root, on the machine's first start (smolvm
    /// `--init`), after the provider's own envd user setup.
    #[serde(default)]
    pub init: Vec<String>,
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

#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum EnvdSource {
    /// The provider mounts its staged envd read-only into the machine and
    /// runs it as the workload. The image needs no envd.
    #[default]
    Injected,
    /// The image ships envd and the template's `command` starts it.
    Image,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
pub enum EnvdUser {
    /// Created on first start if the image lacks it.
    #[default]
    #[serde(rename = "lightspeed-envd")]
    LightspeedEnvd,
    /// For images where creating a user is impractical. Agent commands then
    /// run as root, and the microVM is the only isolation boundary.
    #[serde(rename = "root")]
    Root,
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
            guest_path: "/var/lib/lightspeed-envd/envd.sock".to_owned(),
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
        let state_dir = match raw.state_dir {
            Some(state_dir) => state_dir,
            None => default_state_dir()?,
        };
        if !state_dir.is_absolute() {
            bail!("stateDir must be an absolute path")
        }
        if state_dir.as_os_str().len() > MAX_STATE_DIR_BYTES {
            bail!(
                "stateDir {} is longer than {MAX_STATE_DIR_BYTES} bytes; envd socket paths would not fit",
                state_dir.display()
            )
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
        let envd_binary = envd_binary(&raw.envd, std::env::consts::ARCH)?;
        let mut templates = BTreeMap::new();
        for template in raw.templates {
            if template.envd_source == EnvdSource::Injected && envd_binary.is_none() {
                bail!(
                    "template {}: no envd binary configured for {} (envd.binaries)",
                    template.template_id,
                    std::env::consts::ARCH
                )
            }
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
            state_dir,
            smolvm: raw.smolvm,
            envd_binary,
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

/// Per-user default, so the provider runs as the user who owns the smolvm
/// machines (smolvm keeps its machine database per user):
/// `$XDG_STATE_HOME/lightspeed-provider-smolvm`, else
/// `~/.local/state/lightspeed-provider-smolvm`; `%LOCALAPPDATA%` on Windows.
fn default_state_dir() -> anyhow::Result<PathBuf> {
    const NAME: &str = "lightspeed-provider-smolvm";
    let non_empty = |key: &str| std::env::var_os(key).filter(|value| !value.is_empty());
    if cfg!(windows) {
        let base = non_empty("LOCALAPPDATA").context("LOCALAPPDATA is not set; set stateDir")?;
        return Ok(PathBuf::from(base).join(NAME));
    }
    if let Some(base) = non_empty("XDG_STATE_HOME") {
        return Ok(PathBuf::from(base).join(NAME));
    }
    let home = non_empty("HOME").context("HOME is not set; set stateDir")?;
    Ok(PathBuf::from(home).join(".local/state").join(NAME))
}

/// Validate every configured binary and pick the one for `arch`.
fn envd_binary(config: &EnvdConfig, arch: &str) -> anyhow::Result<Option<EnvdBinary>> {
    for (key, binary) in &config.binaries {
        if !ENVD_ARCHES.contains(&key.as_str()) {
            bail!("envd.binaries: unknown architecture {key}")
        }
        if !binary.path.is_absolute() {
            bail!("envd.binaries.{key}.path must be absolute")
        }
        if !envd::is_sha256(&binary.sha256) {
            bail!("envd.binaries.{key}.sha256 must be 64 lowercase hex digits")
        }
    }
    Ok(config.binaries.get(arch).cloned())
}

fn validate_template(
    template: &TemplatePolicy,
    credentials: &BTreeMap<String, CredentialRule>,
) -> anyhow::Result<()> {
    if template.template_id.is_empty() {
        bail!("template id must not be empty")
    }
    match template.local_archive() {
        Some(_) => {
            if !template
                .image_sha256
                .as_deref()
                .is_some_and(envd::is_sha256)
            {
                bail!("a local image archive needs imageSha256 (64 lowercase hex digits)")
            }
        }
        None if looks_local(&template.image) => {
            bail!("local image archives must be absolute paths")
        }
        None => {
            if !template.image.contains("@sha256:") {
                bail!(
                    "image must be pinned by digest (name@sha256:...) or be an absolute \
                     path to a docker save archive"
                )
            }
            if template.image_sha256.is_some() {
                bail!("imageSha256 only applies to local image archives")
            }
        }
    }
    if template.cpus == Some(0) || template.memory_mib == Some(0) {
        bail!("cpus and memoryMib must be positive")
    }
    if let EnvdChannel::ExposedSocket { guest_path } = &template.envd
        && (!guest_path.starts_with('/') || guest_path.contains(':'))
    {
        bail!("envd guestPath must be an absolute guest path without ':'")
    }
    match template.envd_source {
        EnvdSource::Injected => {
            if !template.command.is_empty() {
                bail!(
                    "command is generated for an injected envd; set envdSource \"image\" to use your own"
                )
            }
            // The path is spliced into the generated user-setup script.
            if let EnvdChannel::ExposedSocket { guest_path } = &template.envd
                && !guest_path
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || matches!(c, '/' | '.' | '_' | '-'))
            {
                bail!("envd guestPath may only contain letters, digits, '/', '.', '_' and '-'")
            }
        }
        EnvdSource::Image => {
            if template.envd_user != EnvdUser::default() {
                bail!("envdUser only applies to an injected envd; the image chooses its own user")
            }
        }
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

const ARCHIVE_SUFFIXES: [&str; 3] = [".tar", ".tar.gz", ".tgz"];

/// What smolvm would read as a local source rather than a registry reference.
fn looks_local(image: &str) -> bool {
    image.starts_with('/')
        || image.starts_with("./")
        || image.starts_with("../")
        || ARCHIVE_SUFFIXES
            .iter()
            .any(|suffix| image.ends_with(suffix))
}

impl TemplatePolicy {
    /// The local `docker save` archive this template boots from, if any.
    pub fn local_archive(&self) -> Option<&Path> {
        let path = Path::new(&self.image);
        (path.is_absolute()
            && ARCHIVE_SUFFIXES
                .iter()
                .any(|suffix| self.image.ends_with(suffix)))
        .then_some(path)
    }
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
    Config::from_json(with_state_dir(state_dir.to_str().unwrap()).as_bytes()).expect("test config")
}

#[cfg(test)]
fn with_state_dir(state_dir: &str) -> String {
    let mut json: serde_json::Value =
        serde_json::from_str(include_str!("../config.example.json")).unwrap();
    json["stateDir"] = state_dir.into();
    json.to_string()
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
        let json = with_image("ghcr.io/example/lightspeed-envd:latest", None);
        let error = Config::from_json(json.as_bytes()).unwrap_err();
        assert!(format!("{error:#}").contains("pinned by digest"));
    }

    fn with_image(image: &str, sha: Option<&str>) -> String {
        let mut json: serde_json::Value = serde_json::from_str(EXAMPLE).unwrap();
        json["templates"][0]["image"] = image.into();
        if let Some(sha) = sha {
            json["templates"][0]["imageSha256"] = sha.into();
        }
        json.to_string()
    }

    #[test]
    fn local_archives_must_be_absolute_and_hash_pinned() {
        let sha = "a".repeat(64);
        let ok = Config::from_json(with_image("/images/envd.tar", Some(&sha)).as_bytes()).unwrap();
        assert!(ok.templates["dev-small-v1"].local_archive().is_some());
        assert!(Config::from_json(with_image("/images/envd.tar", None).as_bytes()).is_err());
        assert!(Config::from_json(with_image("/images/envd.tar", Some("abc")).as_bytes()).is_err());
        assert!(Config::from_json(with_image("./envd.tar", Some(&sha)).as_bytes()).is_err());
        assert!(Config::from_json(with_image("envd.tar", Some(&sha)).as_bytes()).is_err());
        let registry = "ghcr.io/x/y@sha256:".to_owned() + &"0".repeat(64);
        assert!(Config::from_json(with_image(&registry, Some(&sha)).as_bytes()).is_err());
    }

    fn with_template(field: &str, value: serde_json::Value) -> String {
        let mut json: serde_json::Value = serde_json::from_str(EXAMPLE).unwrap();
        json["templates"][0][field] = value;
        json.to_string()
    }

    #[test]
    fn injected_templates_get_a_generated_command() {
        let config = Config::from_json(EXAMPLE.as_bytes()).unwrap();
        let template = &config.templates["dev-small-v1"];
        assert_eq!(template.envd_source, EnvdSource::Injected);
        assert_eq!(template.envd_user, EnvdUser::LightspeedEnvd);
        let error =
            Config::from_json(with_template("command", serde_json::json!(["envd"])).as_bytes())
                .unwrap_err();
        assert!(format!("{error:#}").contains("generated"));
        let unsafe_path =
            serde_json::json!({"kind": "exposedSocket", "guestPath": "/run/a b.sock"});
        assert!(Config::from_json(with_template("envd", unsafe_path).as_bytes()).is_err());
        let root = Config::from_json(with_template("envdUser", "root".into()).as_bytes()).unwrap();
        assert_eq!(root.templates["dev-small-v1"].envd_user, EnvdUser::Root);
        assert!(Config::from_json(with_template("envdUser", "agent".into()).as_bytes()).is_err());
    }

    #[test]
    fn image_templates_keep_their_own_command_and_user() {
        let mut json: serde_json::Value = serde_json::from_str(EXAMPLE).unwrap();
        json["templates"][0]["envdSource"] = "image".into();
        json["templates"][0]["command"] = serde_json::json!(["lightspeed-envd"]);
        json.as_object_mut().unwrap().remove("envd");
        let config = Config::from_json(json.to_string().as_bytes()).unwrap();
        assert!(config.envd_binary.is_none());
        json["templates"][0]["envdUser"] = "root".into();
        assert!(Config::from_json(json.to_string().as_bytes()).is_err());
    }

    #[test]
    fn injected_templates_need_a_valid_binary_for_this_host() {
        let mut json: serde_json::Value = serde_json::from_str(EXAMPLE).unwrap();
        json.as_object_mut().unwrap().remove("envd");
        let error = Config::from_json(json.to_string().as_bytes()).unwrap_err();
        assert!(format!("{error:#}").contains("no envd binary configured"));

        let binary = |path: &str, sha: &str| EnvdBinary {
            path: path.into(),
            sha256: sha.to_owned(),
        };
        let sha = "a".repeat(64);
        let config = |key: &str, value: EnvdBinary| EnvdConfig {
            binaries: BTreeMap::from([(key.to_owned(), value)]),
        };
        let x86 = config("x86_64", binary("/opt/envd", &sha));
        assert_eq!(
            envd_binary(&x86, "x86_64").unwrap(),
            Some(binary("/opt/envd", &sha))
        );
        assert_eq!(envd_binary(&x86, "aarch64").unwrap(), None);
        assert!(envd_binary(&config("riscv64", binary("/opt/envd", &sha)), "x86_64").is_err());
        assert!(envd_binary(&config("x86_64", binary("envd", &sha)), "x86_64").is_err());
        assert!(envd_binary(&config("x86_64", binary("/opt/envd", "abc")), "x86_64").is_err());
    }

    #[test]
    fn state_dir_must_be_absolute_and_short() {
        assert!(Config::from_json(with_state_dir("state").as_bytes()).is_err());
        let long = format!("/{}", "x".repeat(80));
        assert!(Config::from_json(with_state_dir(&long).as_bytes()).is_err());
        assert!(Config::from_json(with_state_dir("/tmp/lsvm").as_bytes()).is_ok());
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
