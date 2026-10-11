//! Startup reconciliation between the ownership store and smolvm.
//!
//! A crash can leave a record without a machine (create interrupted) or a
//! machine without a record (record lost). Records are repaired. A machine
//! without a record is re-recorded from its ownership labels when they are
//! complete and its template still exists; otherwise it is reported and left
//! alone, since nothing proves who owns it.

use crate::{
    backend::{VmBackend, VmState},
    config::{Config, EnvdChannel},
    policy::{self, Ownership},
    store::{Lifecycle, Store, TargetRecord, now_ms},
};

#[derive(Debug, Default, PartialEq, Eq)]
pub struct ReconcileReport {
    pub failed_creates: Vec<String>,
    pub resumed_creates: Vec<String>,
    pub finished_closes: Vec<String>,
    pub recovered: Vec<String>,
    pub orphans: Vec<String>,
}

pub async fn reconcile<B: VmBackend>(
    config: &Config,
    store: &Store,
    backend: &B,
) -> anyhow::Result<ReconcileReport> {
    let mut report = ReconcileReport::default();
    let machines = backend.machines().await?;
    let state_of = |name: &str| {
        machines
            .iter()
            .find(|machine| machine.name == name)
            .map_or(VmState::Missing, |machine| machine.state)
    };
    let records = store.list().await?;
    for mut record in records.iter().cloned() {
        match record.lifecycle {
            Lifecycle::Creating => {
                if state_of(&record.target_id) == VmState::Missing {
                    record.lifecycle = Lifecycle::Failed;
                    record.failure =
                        Some("provider stopped before the machine was created".to_owned());
                    report.failed_creates.push(record.target_id.clone());
                } else {
                    record.lifecycle = Lifecycle::Active;
                    report.resumed_creates.push(record.target_id.clone());
                }
                record.updated_at_ms = now_ms();
                store.put(&record).await?;
            }
            Lifecycle::Closing => {
                backend.delete(&record.target_id).await?;
                store.remove(&record.target_id).await?;
                report.finished_closes.push(record.target_id.clone());
            }
            Lifecycle::Active | Lifecycle::Failed => {}
        }
    }
    for machine in &machines {
        if !policy::is_target_name(&machine.name)
            || records
                .iter()
                .any(|record| record.target_id == machine.name)
        {
            continue;
        }
        match recover(config, &machine.name, &machine.labels) {
            Some(record) => {
                store.ensure_vm_dir(&record.target_id).await?;
                store.put(&record).await?;
                tracing::info!(machine = %machine.name, "re-recorded machine from its labels");
                report.recovered.push(machine.name.clone());
            }
            None => {
                tracing::warn!(machine = %machine.name, "machine has no usable ownership; leaving it");
                report.orphans.push(machine.name.clone());
            }
        }
    }
    Ok(report)
}

/// Loopback-channel machines cannot be recovered: their host port was only
/// recorded in the lost record.
fn recover(
    config: &Config,
    name: &str,
    labels: &std::collections::BTreeMap<String, String>,
) -> Option<TargetRecord> {
    let ownership = Ownership::from_labels(name, labels)?;
    let template = config.templates.get(&ownership.template_id)?;
    if !matches!(template.envd, EnvdChannel::ExposedSocket { .. }) {
        return None;
    }
    let now = now_ms();
    Some(TargetRecord {
        target_id: name.to_owned(),
        universe_id: ownership.universe_id,
        binding_id: ownership.binding_id,
        environment_id: ownership.environment_id,
        incarnation_id: ownership.incarnation_id,
        request_id: ownership.request_id,
        template_id: ownership.template_id,
        image: template.image.clone(),
        envd: template.envd.clone(),
        host_port: None,
        lifecycle: Lifecycle::Active,
        failure: None,
        created_at_ms: now,
        updated_at_ms: now,
    })
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::*;
    use crate::{backend::fake::FakeBackend, config::test_config, store::sample_record};

    #[tokio::test]
    async fn interrupted_work_is_repaired_and_lost_records_recovered() {
        let dir = tempfile::tempdir().unwrap();
        let config = test_config(dir.path());
        let store = Store::open(dir.path()).await.unwrap();
        let backend = FakeBackend::default();

        let lost_create = policy::target_name("u", "b", "lost", "i");
        let done_create = policy::target_name("u", "b", "done", "i");
        let closing = policy::target_name("u", "b", "closing", "i");
        for (id, lifecycle) in [
            (&lost_create, Lifecycle::Creating),
            (&done_create, Lifecycle::Creating),
            (&closing, Lifecycle::Closing),
        ] {
            let mut record = sample_record(id);
            record.lifecycle = lifecycle;
            store.put(&record).await.unwrap();
        }
        let labelled = Ownership {
            universe_id: "u".to_owned(),
            binding_id: "b".to_owned(),
            environment_id: "labelled".to_owned(),
            incarnation_id: "i".to_owned(),
            request_id: "r".to_owned(),
            template_id: "dev-small-v1".to_owned(),
        };
        let recovered = labelled.target_name();
        let orphan = policy::target_name("u", "b", "orphan", "i");
        backend.insert(&done_create, VmState::Running, BTreeMap::new());
        backend.insert(&closing, VmState::Running, BTreeMap::new());
        backend.insert(&recovered, VmState::Running, labelled.labels());
        backend.insert(&orphan, VmState::Running, BTreeMap::new());
        backend.insert("someone-elses-vm", VmState::Running, BTreeMap::new());

        let report = reconcile(&config, &store, &backend).await.unwrap();

        assert_eq!(report.failed_creates, vec![lost_create.clone()]);
        assert_eq!(report.resumed_creates, vec![done_create.clone()]);
        assert_eq!(report.finished_closes, vec![closing.clone()]);
        assert_eq!(report.recovered, vec![recovered.clone()]);
        assert_eq!(report.orphans, vec![orphan.clone()]);
        assert_eq!(
            store.get(&lost_create).await.unwrap().unwrap().lifecycle,
            Lifecycle::Failed
        );
        assert!(store.get(&closing).await.unwrap().is_none());
        assert_eq!(backend.state_of(&closing), VmState::Missing);
        let record = store.get(&recovered).await.unwrap().unwrap();
        assert_eq!(record.ownership(), labelled);
        assert_eq!(backend.state_of(&orphan), VmState::Running);
    }
}
