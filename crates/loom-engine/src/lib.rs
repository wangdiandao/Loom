//! Headless dual-lane sampling and snapshot orchestration engine for Loom.
//!
//! `loom-engine` strictly prohibits UI (`gpui` / `gpui-kit`) and multi-threaded
//! `tokio` runtime dependencies, using compile-time monomorphization over
//! [`loom_pal_api::PlatformBackend`] so the entire engine is deterministically
//! testable with mock backends.
//!
//! # Architecture
//!
//! - [`EngineCoordinator`] runs at most `3` background threads (`1` fast-lane
//!   sampling & projection thread + `1..=2` slow-lane bulkhead worker threads).
//! - [`MonitorLeaseRegistry`] / [`MonitorLease`] gate on-demand domains such as
//!   `Details`, `Performance`, `Services`, `Startup`, `Users`, and `AppHistory`.
//! - [`ProcessTree`] and [`collect_bottom_up_kill_order`] guard against OS PID
//!   reuse and ensure watchdog parents are terminated strictly after descendants.
//! - [`CircuitBreaker`] and [`ColdMetaCache`] isolate slow disks / UNC paths and
//!   deduplicate static binary assets across multi-process applications.

#![warn(missing_docs)]

pub mod command;
pub mod coordinator;
pub mod lease;
pub mod projection;
pub mod slow_lane;
pub mod tree;

pub use command::{
    CommandEvent, CommandKind, CommandOutcome, CommandSuccess, EngineCommand, EngineCommandError,
};
pub use coordinator::{EngineConfig, EngineCoordinator};
pub use lease::{MonitorLease, MonitorLeaseRegistry};
pub use projection::{ProjectionEngine, ViewProjectionConfig};
pub use slow_lane::{CircuitBreaker, ColdMetaCache, normalize_exe_path_key};
pub use tree::{ProcessTree, collect_bottom_up_kill_order};
