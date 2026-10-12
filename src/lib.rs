//! Lightspeed environment provider backed by smolvm microVMs.
//!
//! The provider is the only network-reachable component. Lightspeed talks to
//! its controller (`/control`) and opens per-target data routes
//! (`/routes/...`); the provider checks ownership against its local store and
//! relays the data protocol to the `lightspeed-envd` inside the VM over a
//! host-local channel. The guest image is generic: it carries no tenant
//! identity, and environment credentials never enter it.

pub mod backend;
pub mod channel;
pub mod config;
pub mod credentials;
pub mod envd;
pub mod policy;
pub mod reconcile;
pub mod relay;
pub mod server;
pub mod smolvm;
pub mod store;
pub mod telemetry;

pub use backend::VmBackend;
pub use config::{Config, ProviderArgs};
pub use store::Store;
