//! Controller plane and data routes.
//!
//! Same wire contract as Lightspeed's Incus provider: JSON-RPC over the
//! `/control` WebSocket, and Lightspeed-initiated data connections on
//! `/routes/{universe}/{binding}/{environment}/{incarnation}/{target}`.

use std::{collections::BTreeMap, sync::Arc, time::Duration};

use axum::{
    Json, Router,
    extract::{
        Path, State, WebSocketUpgrade,
        ws::{Message, WebSocket},
    },
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::get,
};
use environment_protocol::{
    control::{
        handshake::{
            ControllerCapabilities, ControllerInitializeParams, ControllerInitializeResponse,
        },
        methods::*,
        targets::*,
    },
    error::{EnvironmentProtocolError, EnvironmentProtocolErrorCode},
    shared::{
        CURRENT_PROTOCOL_VERSION, EnvironmentCapabilities, EnvironmentScope, ImplementationInfo,
        ProviderTargetId,
    },
};
use futures_util::{StreamExt as _, stream};
use serde::de::DeserializeOwned;
use serde_json::{Value, json};

use crate::{
    backend::{VmBackend, VmSpec, VmState},
    channel,
    config::{Config, EnvdChannel},
    policy,
    relay::{self, RelayContext},
    store::{Lifecycle, Store, TargetRecord, now_ms},
};

/// `machine pause` saves RAM and disk and releases the VMM, which is
/// Lightspeed's `suspended`.
pub const SUPPORTED_POWER_STATES: [PowerState; 3] = [
    PowerState::Running,
    PowerState::Suspended,
    PowerState::Stopped,
];

type BindingKey = (String, String);
type BindingLock = Arc<tokio::sync::Mutex<()>>;

#[derive(Clone)]
pub struct App<B> {
    config: Config,
    backend: B,
    store: Store,
    binding_locks: Arc<tokio::sync::Mutex<BTreeMap<BindingKey, BindingLock>>>,
}

impl<B: VmBackend> App<B> {
    pub fn new(config: Config, backend: B, store: Store) -> Self {
        Self {
            config,
            backend,
            store,
            binding_locks: Arc::default(),
        }
    }

    async fn binding_lock(&self, binding: &ProviderBindingContext) -> BindingLock {
        self.binding_locks
            .lock()
            .await
            .entry((binding.universe_id.clone(), binding.binding_id.clone()))
            .or_default()
            .clone()
    }

    fn dial_timeout(&self) -> Duration {
        Duration::from_secs(self.config.dial_timeout_seconds)
    }
}

pub async fn serve<B: VmBackend>(app: App<B>) -> anyhow::Result<()> {
    let listener = tokio::net::TcpListener::bind(app.config.controller_listen).await?;
    tracing::info!(listen = %app.config.controller_listen, "controller listening");
    let router = Router::new()
        .route("/health", get(health::<B>))
        .route("/control", get(control_upgrade::<B>))
        .route(
            "/routes/{universe}/{binding}/{environment}/{incarnation}/{target}",
            get(data_upgrade::<B>),
        )
        .with_state(app);
    axum::serve(listener, router).await?;
    Ok(())
}

async fn health<B: VmBackend>(State(app): State<App<B>>) -> Response {
    match app.backend.health().await {
        Ok(version) => Json(json!({"smolvm": version})).into_response(),
        Err(_) => StatusCode::SERVICE_UNAVAILABLE.into_response(),
    }
}

async fn control_upgrade<B: VmBackend>(
    State(app): State<App<B>>,
    upgrade: WebSocketUpgrade,
) -> Response {
    upgrade
        .on_upgrade(move |socket| control_connection(app, socket))
        .into_response()
}

async fn control_connection<B: VmBackend>(app: App<B>, mut socket: WebSocket) {
    while let Some(Ok(message)) = socket.recv().await {
        match message {
            Message::Text(text) => {
                let response = dispatch(&app, &text).await;
                if socket
                    .send(Message::Text(response.to_string().into()))
                    .await
                    .is_err()
                {
                    break;
                }
            }
            Message::Ping(bytes) => {
                let _ = socket.send(Message::Pong(bytes)).await;
            }
            Message::Close(_) => break,
            _ => {}
        }
    }
    let _ = socket.send(Message::Close(None)).await;
}

async fn data_upgrade<B: VmBackend>(
    State(app): State<App<B>>,
    Path((universe, binding_id, environment, incarnation, target_id)): Path<(
        String,
        String,
        String,
        String,
        String,
    )>,
    upgrade: WebSocketUpgrade,
) -> Response {
    if policy::target_name(&universe, &binding_id, &environment, &incarnation) != target_id {
        return StatusCode::CONFLICT.into_response();
    }
    let record = match app.store.get(&target_id).await {
        Ok(Some(record)) => record,
        Ok(None) => return StatusCode::NOT_FOUND.into_response(),
        Err(_) => return StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    };
    let binding = ProviderBindingContext {
        universe_id: universe,
        binding_id,
    };
    if !record.belongs_to(&binding)
        || record.environment_id != environment
        || record.incarnation_id != incarnation
    {
        return StatusCode::CONFLICT.into_response();
    }
    if record.lifecycle != Lifecycle::Active {
        return StatusCode::SERVICE_UNAVAILABLE.into_response();
    }
    let Ok(vm_dir) = app.store.vm_dir(&record.target_id) else {
        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
    };
    let Ok(endpoint) = channel::endpoint(&record, &vm_dir) else {
        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
    };
    let guest = match channel::dial(&endpoint, app.dial_timeout()).await {
        Ok(guest) => guest,
        Err(_) => return StatusCode::SERVICE_UNAVAILABLE.into_response(),
    };
    let context = RelayContext {
        backend: app.backend.clone(),
        rules: Arc::new(app.config.template_rules(&record.template_id)),
        unknown: app.config.unknown_secret_env,
        idle: Duration::from_secs(app.config.relay_idle_seconds),
        target_id: record.target_id,
    };
    upgrade
        .max_message_size(64 * 1024 * 1024)
        .on_upgrade(move |socket| relay::proxy(context, socket, guest))
        .into_response()
}

async fn dispatch<B: VmBackend>(app: &App<B>, text: &str) -> Value {
    let request: Value = match serde_json::from_str(text) {
        Ok(value) => value,
        Err(error) => {
            return error_value(
                Value::Null,
                EnvironmentProtocolErrorCode::InvalidRequest,
                error.to_string(),
            );
        }
    };
    let id = request.get("id").cloned().unwrap_or(Value::Null);
    let method = request
        .get("method")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let params = request.get("params").cloned().unwrap_or_else(|| json!({}));
    let result = match method {
        INITIALIZE_METHOD => initialize(decode(params)).and_then(encode),
        LIST_TEMPLATES_METHOD => list_templates(app, decode(params)).and_then(encode),
        LIST_TARGETS_METHOD => list_targets(app, decode(params)).await.and_then(encode),
        CREATE_TARGET_METHOD => create_target(app, decode(params)).await.and_then(encode),
        GET_TARGET_METHOD => get_target(app, decode(params)).await.and_then(encode),
        CLOSE_TARGET_METHOD => close_target(app, decode(params)).await.and_then(encode),
        SET_TARGET_POWER_METHOD => set_target_power(app, decode(params)).await.and_then(encode),
        ADOPT_TARGET_METHOD | ENSURE_INGRESS_METHOD | REMOVE_INGRESS_METHOD => {
            return error_value(
                id,
                EnvironmentProtocolErrorCode::Unsupported,
                format!("{method} is not supported by the smolvm provider"),
            );
        }
        _ => {
            return error_value(
                id,
                EnvironmentProtocolErrorCode::Unsupported,
                format!("unknown controller method: {method}"),
            );
        }
    };
    match result {
        Ok(result) => json!({"jsonrpc": "2.0", "id": id, "result": result}),
        Err(error) => error_value(
            id,
            EnvironmentProtocolErrorCode::Internal,
            format!("{error:#}"),
        ),
    }
}

fn initialize(
    params: anyhow::Result<ControllerInitializeParams>,
) -> anyhow::Result<ControllerInitializeResponse> {
    if params?.protocol_version != CURRENT_PROTOCOL_VERSION {
        anyhow::bail!("unsupported protocol version")
    }
    Ok(ControllerInitializeResponse {
        protocol_version: CURRENT_PROTOCOL_VERSION,
        capabilities: ControllerCapabilities {
            list_templates: true,
            list_targets: true,
            create_target: true,
            adopt_target: false,
            get_target: true,
            close_target: true,
            set_target_power: true,
            ingress: false,
        },
        implementation: ImplementationInfo {
            name: env!("CARGO_PKG_NAME").to_owned(),
            version: Some(env!("CARGO_PKG_VERSION").to_owned()),
            git_sha: None,
            target: None,
        },
    })
}

fn list_templates<B: VmBackend>(
    app: &App<B>,
    params: anyhow::Result<ListTemplatesParams>,
) -> anyhow::Result<ListTemplatesResponse> {
    let _params = params?;
    Ok(ListTemplatesResponse {
        templates: app
            .config
            .templates
            .values()
            .map(|template| {
                let mut metadata = BTreeMap::from([("image".to_owned(), template.image.clone())]);
                if let Some(cpus) = template.cpus {
                    metadata.insert("cpus".to_owned(), cpus.to_string());
                }
                if let Some(memory) = template.memory_mib {
                    metadata.insert("memoryMib".to_owned(), memory.to_string());
                }
                EnvironmentTemplate {
                    template_id: template.template_id.clone(),
                    display_name: template.display_name.clone(),
                    description: template.description.clone(),
                    public_ingress: false,
                    deprecated: template.deprecated,
                    metadata,
                }
            })
            .collect(),
    })
}

async fn list_targets<B: VmBackend>(
    app: &App<B>,
    params: anyhow::Result<ListTargetsParams>,
) -> anyhow::Result<ListTargetsResponse> {
    let params = params?;
    let records: Vec<_> = app
        .store
        .list()
        .await?
        .into_iter()
        .filter(|record| record.belongs_to(&params.binding))
        .collect();
    let mut targets = stream::iter(records)
        .map(|record| async move { summary_of(app, record).await })
        .buffer_unordered(16)
        .collect::<Vec<_>>()
        .await;
    if let Some(status) = params.status {
        targets.retain(|target| target.status == status);
    }
    targets.sort_by(|left, right| left.target_id.cmp(&right.target_id));
    Ok(ListTargetsResponse { targets })
}

async fn get_target<B: VmBackend>(
    app: &App<B>,
    params: anyhow::Result<GetTargetParams>,
) -> anyhow::Result<GetTargetResponse> {
    let params = params?;
    let record = owned_record(app, &params.binding, &params.target_id).await?;
    Ok(GetTargetResponse {
        target: summary_of(app, record).await,
    })
}

async fn create_target<B: VmBackend>(
    app: &App<B>,
    params: anyhow::Result<CreateTargetParams>,
) -> anyhow::Result<CreateTargetResponse> {
    let params = params?;
    let lock = app.binding_lock(&params.binding).await;
    let _guard = lock.lock().await;
    let template = app.config.template(&params.template_id)?;
    let target_id = policy::target_name(
        &params.binding.universe_id,
        &params.binding.binding_id,
        &params.environment_id,
        &params.incarnation_id,
    );

    // Retries converge on the deterministic target, or on the earlier
    // target created for the same request.
    let existing = match app.store.get(&target_id).await? {
        Some(record) => Some(record),
        None => app.store.list().await?.into_iter().find(|record| {
            record.belongs_to(&params.binding) && record.request_id == params.request_id
        }),
    };
    if let Some(record) = existing {
        verify_create(&record, &params)?;
        return Ok(CreateTargetResponse {
            target: summary_of(app, record).await,
        });
    }

    let host_port = match template.envd {
        EnvdChannel::LoopbackForward { .. } => Some(channel::allocate_loopback_port()?),
        EnvdChannel::ExposedSocket { .. } => None,
    };
    let now = now_ms();
    let mut record = TargetRecord {
        target_id: target_id.clone(),
        universe_id: params.binding.universe_id.clone(),
        binding_id: params.binding.binding_id.clone(),
        environment_id: params.environment_id.clone(),
        incarnation_id: params.incarnation_id.clone(),
        request_id: params.request_id.clone(),
        template_id: template.template_id.clone(),
        image: template.image.clone(),
        envd: template.envd.clone(),
        host_port,
        lifecycle: Lifecycle::Creating,
        failure: None,
        created_at_ms: now,
        updated_at_ms: now,
    };
    // Record intent first: a crash after this leaves a known record for
    // reconciliation rather than an unowned machine.
    app.store.put(&record).await?;
    let vm_dir = app.store.ensure_vm_dir(&target_id).await?;
    let rules = app.config.template_rules(&template.template_id);
    let spec = VmSpec {
        name: &target_id,
        template,
        credentials: rules.values().collect(),
        labels: record.ownership().labels(),
        vm_dir: &vm_dir,
        host_port,
    };
    let created = app.backend.create_and_start(&spec).await;
    record.updated_at_ms = now_ms();
    match created {
        Ok(()) => record.lifecycle = Lifecycle::Active,
        Err(error) => {
            tracing::warn!(%target_id, error = format!("{error:#}"), "machine creation failed");
            record.lifecycle = Lifecycle::Failed;
            record.failure = Some(format!("{error:#}"));
        }
    }
    app.store.put(&record).await?;
    Ok(CreateTargetResponse {
        target: summary_of(app, record).await,
    })
}

async fn close_target<B: VmBackend>(
    app: &App<B>,
    params: anyhow::Result<CloseTargetParams>,
) -> anyhow::Result<CloseTargetResponse> {
    let params = params?;
    verify_target_handle(
        &params.binding,
        &params.environment_id,
        &params.incarnation_id,
        &params.target_id,
    )?;
    let Some(mut record) = app.store.get(params.target_id.as_str()).await? else {
        return Ok(CloseTargetResponse {
            target_id: params.target_id,
            status: ProviderTargetStatus::Closed,
        });
    };
    verify_ownership(
        &record,
        &params.binding,
        &params.environment_id,
        &params.incarnation_id,
    )?;
    record.lifecycle = Lifecycle::Closing;
    record.updated_at_ms = now_ms();
    app.store.put(&record).await?;
    app.backend.delete(&record.target_id).await?;
    app.store.remove(&record.target_id).await?;
    Ok(CloseTargetResponse {
        target_id: params.target_id,
        status: ProviderTargetStatus::Closed,
    })
}

async fn set_target_power<B: VmBackend>(
    app: &App<B>,
    params: anyhow::Result<SetTargetPowerParams>,
) -> anyhow::Result<SetTargetPowerResponse> {
    let params = params?;
    verify_target_handle(
        &params.binding,
        &params.environment_id,
        &params.incarnation_id,
        &params.target_id,
    )?;
    let record = owned_record(app, &params.binding, &params.target_id).await?;
    verify_ownership(
        &record,
        &params.binding,
        &params.environment_id,
        &params.incarnation_id,
    )?;
    if record.lifecycle != Lifecycle::Active {
        anyhow::bail!("target is not active")
    }
    let name = record.target_id.as_str();
    let state = app.backend.state(name).await?;
    match (params.power, state) {
        (PowerState::Running, VmState::Running)
        | (PowerState::Suspended, VmState::Suspended)
        | (PowerState::Stopped, VmState::Stopped) => {}
        (PowerState::Running, VmState::Suspended) => app.backend.resume(name).await?,
        (PowerState::Running, VmState::Stopped) => app.backend.start(name).await?,
        (PowerState::Suspended, VmState::Running) => app.backend.suspend(name).await?,
        (PowerState::Stopped, VmState::Running | VmState::Suspended) => {
            app.backend.stop(name).await?
        }
        (PowerState::Paused, _) => anyhow::bail!("paused is not supported; use suspended"),
        (power, state) => anyhow::bail!("cannot move a {state:?} machine to {power}"),
    }
    Ok(SetTargetPowerResponse {
        target: summary_of(app, record).await,
    })
}

async fn owned_record<B: VmBackend>(
    app: &App<B>,
    binding: &ProviderBindingContext,
    target_id: &ProviderTargetId,
) -> anyhow::Result<TargetRecord> {
    let record = app
        .store
        .get(target_id.as_str())
        .await?
        .ok_or_else(|| anyhow::anyhow!("target not found"))?;
    if !record.belongs_to(binding) {
        anyhow::bail!("target belongs to another binding")
    }
    Ok(record)
}

fn verify_target_handle(
    binding: &ProviderBindingContext,
    environment_id: &str,
    incarnation_id: &str,
    target_id: &ProviderTargetId,
) -> anyhow::Result<()> {
    let expected = policy::target_name(
        &binding.universe_id,
        &binding.binding_id,
        environment_id,
        incarnation_id,
    );
    if target_id.as_str() != expected {
        anyhow::bail!("target handle does not match deterministic ownership identity")
    }
    Ok(())
}

fn verify_ownership(
    record: &TargetRecord,
    binding: &ProviderBindingContext,
    environment_id: &str,
    incarnation_id: &str,
) -> anyhow::Result<()> {
    if !record.belongs_to(binding)
        || record.environment_id != environment_id
        || record.incarnation_id != incarnation_id
    {
        anyhow::bail!("target ownership does not match request")
    }
    Ok(())
}

fn verify_create(record: &TargetRecord, params: &CreateTargetParams) -> anyhow::Result<()> {
    if !record.belongs_to(&params.binding)
        || record.environment_id != params.environment_id
        || record.incarnation_id != params.incarnation_id
        || record.request_id != params.request_id
        || record.template_id != params.template_id
    {
        anyhow::bail!("existing target metadata conflicts with provision request")
    }
    Ok(())
}

async fn observe_status<B: VmBackend>(app: &App<B>, record: &TargetRecord) -> ProviderTargetStatus {
    match record.lifecycle {
        Lifecycle::Creating => return ProviderTargetStatus::Creating,
        Lifecycle::Closing => return ProviderTargetStatus::Closing,
        Lifecycle::Failed => return ProviderTargetStatus::Failed,
        Lifecycle::Active => {}
    }
    let Ok(state) = app.backend.state(&record.target_id).await else {
        return ProviderTargetStatus::Unknown;
    };
    match state {
        VmState::Running => {
            let ready = match app.store.vm_dir(&record.target_id) {
                Ok(vm_dir) => match channel::endpoint(record, &vm_dir) {
                    Ok(endpoint) => channel::probe(&endpoint, app.dial_timeout()).await,
                    Err(_) => false,
                },
                Err(_) => false,
            };
            if ready {
                ProviderTargetStatus::Ready
            } else {
                ProviderTargetStatus::Starting
            }
        }
        VmState::Suspended => ProviderTargetStatus::Suspended,
        VmState::Frozen => ProviderTargetStatus::Paused,
        VmState::Stopped => ProviderTargetStatus::Stopped,
        VmState::Failed | VmState::Missing => ProviderTargetStatus::Failed,
        VmState::Unknown => ProviderTargetStatus::Unknown,
    }
}

async fn summary_of<B: VmBackend>(app: &App<B>, record: TargetRecord) -> ProviderTargetSummary {
    let status = observe_status(app, &record).await;
    let mut metadata = BTreeMap::from([
        ("templateId".to_owned(), record.template_id.clone()),
        ("image".to_owned(), record.image.clone()),
    ]);
    if let Some(failure) = &record.failure {
        metadata.insert("failure".to_owned(), failure.clone());
    }
    let network = app
        .config
        .templates
        .get(&record.template_id)
        .is_some_and(|template| {
            !template.egress.allow_hosts.is_empty() || !template.credentials.is_empty()
        });
    let mut capabilities = EnvironmentCapabilities::filesystem(true, true)
        .with_filesystem_search()
        .with_process()
        .with_jobs();
    // Keep aligned with envd's handshake; the runtime reads this summary
    // before opening the data connection.
    capabilities.network = network;
    capabilities.filesystem_glob = true;
    capabilities.filesystem_ranged_read = true;
    capabilities.process_output_notifications = false;
    capabilities.process_pty = true;
    ProviderTargetSummary {
        target_id: ProviderTargetId::new(record.target_id),
        display_name: Some(record.environment_id),
        status,
        scope: EnvironmentScope::Default,
        capabilities,
        default_cwd: None,
        power_states: SUPPORTED_POWER_STATES.to_vec(),
        metadata,
    }
}

fn decode<T: DeserializeOwned>(value: Value) -> anyhow::Result<T> {
    Ok(serde_json::from_value(value)?)
}

fn encode<T: serde::Serialize>(value: T) -> anyhow::Result<Value> {
    Ok(serde_json::to_value(value)?)
}

fn error_value(id: Value, code: EnvironmentProtocolErrorCode, message: String) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "error": EnvironmentProtocolError::new(code, message)})
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{backend::fake::FakeBackend, config::test_config};

    async fn app() -> (App<FakeBackend>, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let config = test_config(dir.path());
        let store = Store::open(dir.path()).await.unwrap();
        (App::new(config, FakeBackend::default(), store), dir)
    }

    fn binding() -> ProviderBindingContext {
        ProviderBindingContext {
            universe_id: "u".to_owned(),
            binding_id: "b".to_owned(),
        }
    }

    fn create_params(environment: &str, request: &str) -> CreateTargetParams {
        CreateTargetParams {
            request_id: request.to_owned(),
            environment_id: environment.to_owned(),
            incarnation_id: "i".to_owned(),
            binding: binding(),
            template_id: "dev-small-v1".to_owned(),
        }
    }

    async fn call(app: &App<FakeBackend>, method: &str, params: Value) -> Value {
        dispatch(
            app,
            &json!({"jsonrpc": "2.0", "id": 1, "method": method, "params": params}).to_string(),
        )
        .await
    }

    #[tokio::test]
    async fn initialize_advertises_only_what_is_implemented() {
        let (app, _dir) = app().await;
        let response = call(
            &app,
            INITIALIZE_METHOD,
            json!({"protocolVersion": CURRENT_PROTOCOL_VERSION, "clientName": "test"}),
        )
        .await;
        let capabilities = &response["result"]["capabilities"];
        assert_eq!(capabilities["createTarget"], true);
        assert_eq!(capabilities["adoptTarget"], false);
        assert_eq!(capabilities["ingress"], false);
        let adopt = call(&app, ADOPT_TARGET_METHOD, json!({})).await;
        assert_eq!(adopt["error"]["code"], "unsupported");
    }

    #[tokio::test]
    async fn create_is_idempotent_and_labels_the_machine() {
        let (app, _dir) = app().await;
        let first = create_target(&app, Ok(create_params("e", "r1")))
            .await
            .unwrap();
        let again = create_target(&app, Ok(create_params("e", "r1")))
            .await
            .unwrap();
        assert_eq!(first.target.target_id, again.target.target_id);
        assert_eq!(*app.backend.creates.lock().unwrap(), 1);
        // No envd behind the socket in tests, so a running machine is
        // reported as still starting.
        assert_eq!(first.target.status, ProviderTargetStatus::Starting);
        assert_eq!(first.target.power_states, SUPPORTED_POWER_STATES.to_vec());

        let name = first.target.target_id.as_str();
        let machine = app.backend.machines.lock().unwrap()[name].clone();
        let ownership = policy::Ownership::from_labels(name, &machine.labels).unwrap();
        assert_eq!(ownership.request_id, "r1");
        assert!(app.store.secrets_dir(name).unwrap().is_dir());
    }

    #[tokio::test]
    async fn conflicting_retries_are_refused() {
        let (app, _dir) = app().await;
        create_target(&app, Ok(create_params("e", "r1")))
            .await
            .unwrap();
        assert!(
            create_target(&app, Ok(create_params("e", "r2")))
                .await
                .is_err()
        );
        assert!(
            create_target(&app, Ok(create_params("other", "r1")))
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn failed_creation_is_recorded_not_lost() {
        let (app, _dir) = app().await;
        *app.backend.fail_create.lock().unwrap() = true;
        let response = create_target(&app, Ok(create_params("e", "r1")))
            .await
            .unwrap();
        assert_eq!(response.target.status, ProviderTargetStatus::Failed);
        assert!(response.target.metadata["failure"].contains("injected"));
    }

    #[tokio::test]
    async fn other_bindings_cannot_see_or_reach_a_target() {
        let (app, _dir) = app().await;
        let created = create_target(&app, Ok(create_params("e", "r1")))
            .await
            .unwrap();
        let other = ProviderBindingContext {
            universe_id: "u2".to_owned(),
            binding_id: "b".to_owned(),
        };
        let listed = list_targets(
            &app,
            Ok(ListTargetsParams {
                binding: other.clone(),
                status: None,
            }),
        )
        .await
        .unwrap();
        assert!(listed.targets.is_empty());
        assert!(
            get_target(
                &app,
                Ok(GetTargetParams {
                    binding: other,
                    target_id: created.target.target_id
                })
            )
            .await
            .is_err()
        );
    }

    #[tokio::test]
    async fn power_moves_between_running_suspended_and_stopped() {
        let (app, _dir) = app().await;
        let created = create_target(&app, Ok(create_params("e", "r1")))
            .await
            .unwrap();
        let target_id = created.target.target_id.clone();
        let power = |power| SetTargetPowerParams {
            request_id: "p".to_owned(),
            environment_id: "e".to_owned(),
            incarnation_id: "i".to_owned(),
            binding: binding(),
            target_id: target_id.clone(),
            power,
        };
        let suspended = set_target_power(&app, Ok(power(PowerState::Suspended)))
            .await
            .unwrap();
        assert_eq!(suspended.target.status, ProviderTargetStatus::Suspended);
        let stopped = set_target_power(&app, Ok(power(PowerState::Stopped)))
            .await
            .unwrap();
        assert_eq!(stopped.target.status, ProviderTargetStatus::Stopped);
        assert!(
            set_target_power(&app, Ok(power(PowerState::Suspended)))
                .await
                .is_err()
        );
        assert!(
            set_target_power(&app, Ok(power(PowerState::Paused)))
                .await
                .is_err()
        );
        let running = set_target_power(&app, Ok(power(PowerState::Running)))
            .await
            .unwrap();
        assert_eq!(running.target.status, ProviderTargetStatus::Starting);
    }

    #[tokio::test]
    async fn close_removes_machine_and_record_and_is_repeatable() {
        let (app, _dir) = app().await;
        let created = create_target(&app, Ok(create_params("e", "r1")))
            .await
            .unwrap();
        let close = || CloseTargetParams {
            request_id: "c".to_owned(),
            environment_id: "e".to_owned(),
            incarnation_id: "i".to_owned(),
            binding: binding(),
            target_id: created.target.target_id.clone(),
            force: false,
        };
        let closed = close_target(&app, Ok(close())).await.unwrap();
        assert_eq!(closed.status, ProviderTargetStatus::Closed);
        let name = created.target.target_id.as_str();
        assert_eq!(app.backend.state_of(name), VmState::Missing);
        assert!(app.store.get(name).await.unwrap().is_none());
        assert!(close_target(&app, Ok(close())).await.is_ok());
    }

    #[tokio::test]
    async fn handles_must_match_the_ownership_tuple() {
        let (app, _dir) = app().await;
        let created = create_target(&app, Ok(create_params("e", "r1")))
            .await
            .unwrap();
        let mismatched = CloseTargetParams {
            request_id: "c".to_owned(),
            environment_id: "different".to_owned(),
            incarnation_id: "i".to_owned(),
            binding: binding(),
            target_id: created.target.target_id.clone(),
            force: false,
        };
        assert!(close_target(&app, Ok(mismatched)).await.is_err());
        assert_eq!(
            app.backend.state_of(created.target.target_id.as_str()),
            VmState::Running
        );
    }
}
