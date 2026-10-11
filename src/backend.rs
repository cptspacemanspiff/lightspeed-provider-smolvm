//! Machine lifecycle seam between the controller and smolvm.

use std::{collections::BTreeMap, path::Path};

use async_trait::async_trait;
use environment_protocol::shared::SecretString;

use crate::config::{CredentialRule, TemplatePolicy};

/// Observed machine state, before envd readiness is considered.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VmState {
    Running,
    /// RAM and disk saved, VMM released (`machine pause`).
    Suspended,
    /// VMM alive with execution frozen (a held branch point).
    Frozen,
    Stopped,
    Failed,
    Missing,
    /// Transitioning, or running but its agent is unreachable.
    Unknown,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MachineInfo {
    pub name: String,
    pub state: VmState,
    pub labels: BTreeMap<String, String>,
}

pub struct VmSpec<'a> {
    pub name: &'a str,
    pub template: &'a TemplatePolicy,
    /// Rules for this machine's credential substitution. Values arrive
    /// later, per process start, through `supply_credentials`.
    pub credentials: Vec<&'a CredentialRule>,
    /// Ownership labels written onto the machine.
    pub labels: BTreeMap<String, String>,
    /// Private per-machine directory: envd host socket and secret files.
    pub vm_dir: &'a Path,
    /// Host port for a `LoopbackForward` envd channel.
    pub host_port: Option<u16>,
}

#[async_trait]
pub trait VmBackend: Clone + Send + Sync + 'static {
    /// Short description for `/health`; errors when smolvm is unusable.
    async fn health(&self) -> anyhow::Result<String>;
    /// Create and boot a machine. Must fail rather than adopt an existing
    /// machine of the same name; the controller checks its store first.
    async fn create_and_start(&self, spec: &VmSpec<'_>) -> anyhow::Result<()>;
    async fn start(&self, name: &str) -> anyhow::Result<()>;
    async fn stop(&self, name: &str) -> anyhow::Result<()>;
    async fn suspend(&self, name: &str) -> anyhow::Result<()>;
    async fn resume(&self, name: &str) -> anyhow::Result<()>;
    /// Delete a machine and its disks. Deleting a missing machine succeeds.
    async fn delete(&self, name: &str) -> anyhow::Result<()>;
    /// Every machine smolvm knows about, managed or not.
    async fn machines(&self) -> anyhow::Result<Vec<MachineInfo>>;
    /// Hand current credential values to the machine's substitution proxy.
    /// Called before forwarding a start whose `secretEnv` was stripped.
    async fn supply_credentials(
        &self,
        name: &str,
        values: &BTreeMap<String, SecretString>,
    ) -> anyhow::Result<()>;

    async fn state(&self, name: &str) -> anyhow::Result<VmState> {
        Ok(self
            .machines()
            .await?
            .into_iter()
            .find(|machine| machine.name == name)
            .map_or(VmState::Missing, |machine| machine.state))
    }
}

#[cfg(test)]
pub(crate) mod fake {
    use std::sync::{Arc, Mutex};

    use super::*;

    #[derive(Clone, Default)]
    pub struct FakeBackend {
        pub machines: Arc<Mutex<BTreeMap<String, MachineInfo>>>,
        pub supplied: Arc<Mutex<BTreeMap<String, BTreeMap<String, String>>>>,
        pub creates: Arc<Mutex<u32>>,
        pub fail_create: Arc<Mutex<bool>>,
    }

    impl FakeBackend {
        pub fn state_of(&self, name: &str) -> VmState {
            self.machines
                .lock()
                .unwrap()
                .get(name)
                .map_or(VmState::Missing, |machine| machine.state)
        }

        pub fn insert(&self, name: &str, state: VmState, labels: BTreeMap<String, String>) {
            self.machines.lock().unwrap().insert(
                name.to_owned(),
                MachineInfo {
                    name: name.to_owned(),
                    state,
                    labels,
                },
            );
        }

        fn set_state(&self, name: &str, state: VmState) -> anyhow::Result<()> {
            let mut machines = self.machines.lock().unwrap();
            let machine = machines
                .get_mut(name)
                .ok_or_else(|| anyhow::anyhow!("no machine {name}"))?;
            machine.state = state;
            Ok(())
        }
    }

    #[async_trait]
    impl VmBackend for FakeBackend {
        async fn health(&self) -> anyhow::Result<String> {
            Ok("fake".to_owned())
        }

        async fn create_and_start(&self, spec: &VmSpec<'_>) -> anyhow::Result<()> {
            *self.creates.lock().unwrap() += 1;
            if *self.fail_create.lock().unwrap() {
                anyhow::bail!("injected create failure")
            }
            if self.machines.lock().unwrap().contains_key(spec.name) {
                anyhow::bail!("machine exists")
            }
            self.insert(spec.name, VmState::Running, spec.labels.clone());
            Ok(())
        }

        async fn start(&self, name: &str) -> anyhow::Result<()> {
            self.set_state(name, VmState::Running)
        }

        async fn stop(&self, name: &str) -> anyhow::Result<()> {
            self.set_state(name, VmState::Stopped)
        }

        async fn suspend(&self, name: &str) -> anyhow::Result<()> {
            self.set_state(name, VmState::Suspended)
        }

        async fn resume(&self, name: &str) -> anyhow::Result<()> {
            self.set_state(name, VmState::Running)
        }

        async fn delete(&self, name: &str) -> anyhow::Result<()> {
            self.machines.lock().unwrap().remove(name);
            Ok(())
        }

        async fn machines(&self) -> anyhow::Result<Vec<MachineInfo>> {
            Ok(self.machines.lock().unwrap().values().cloned().collect())
        }

        async fn supply_credentials(
            &self,
            name: &str,
            values: &BTreeMap<String, SecretString>,
        ) -> anyhow::Result<()> {
            let mut supplied = self.supplied.lock().unwrap();
            let entry = supplied.entry(name.to_owned()).or_default();
            for (key, value) in values {
                entry.insert(key.clone(), value.expose().to_owned());
            }
            Ok(())
        }
    }
}
