//! Headless dual-lane sampling and snapshot orchestration engine for Loom.
//!
//! `loom-engine` strictly prohibits UI (`gpui` / `gpui-kit`) and multi-threaded
//! `tokio` runtime dependencies, using compile-time monomorphization over
//! [`PlatformBackend`] so the entire engine is deterministically testable with
//! mock backends.

#![warn(missing_docs)]

use std::sync::Arc;

use arc_swap::ArcSwap;
use loom_config::AppSettings;
use loom_pal_api::{PalError, PlatformBackend, SystemOperator};
use loom_types::AppSnapshot;

/// Headless engine coordinator parameterized statically over a [`PlatformBackend`].
///
/// Full dual-lane fast/slow loop, diffing, anti-PID-reuse process tree,
/// pre-sorted view projections, and RAII `MonitorLease` support are built out
/// in subsequent `0.0.1` tasks.
pub struct EngineCoordinator<B: PlatformBackend> {
    backend: Arc<B>,
    settings: AppSettings,
    snapshot: Arc<ArcSwap<AppSnapshot>>,
}

impl<B: PlatformBackend> EngineCoordinator<B> {
    /// Initializes an [`EngineCoordinator`] with the supplied platform `backend`
    /// and initial `settings`.
    pub fn new(backend: B, settings: AppSettings) -> Result<Self, PalError> {
        let initial = AppSnapshot {
            capabilities: B::CAPABILITIES,
            is_elevated: backend.operator().is_elevated(),
            ..AppSnapshot::default()
        };
        Ok(Self {
            backend: Arc::new(backend),
            settings,
            snapshot: Arc::new(ArcSwap::from_pointee(initial)),
        })
    }

    /// Loads the latest lock-free immutable [`AppSnapshot`].
    #[inline]
    pub fn snapshot(&self) -> Arc<AppSnapshot> {
        self.snapshot.load_full()
    }

    /// Returns a reference to the active [`AppSettings`].
    #[inline]
    pub fn settings(&self) -> &AppSettings {
        &self.settings
    }

    /// Returns a reference to the underlying static platform backend.
    #[inline]
    pub fn backend(&self) -> &B {
        &self.backend
    }
}
