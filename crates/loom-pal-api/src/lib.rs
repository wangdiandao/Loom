//! Static Platform Abstraction Layer (PAL) contracts for Loom.
//!
//! This crate defines OS-independent traits, reusable zero-allocation scratch buffers,
//! and structured [`PalError`] types. It depends exclusively on `loom-types` and
//! `thiserror`, exposing no OS handles or UI toolkit types.

#![warn(missing_docs)]

pub mod error;
pub mod scratch;
pub mod traits;

pub use error::PalError;
pub use scratch::{PerfSampleScratch, ProcessSampleScratch};
pub use traits::{
    AppHistoryReader, FastSystemSampler, PlatformBackend, ServiceManager, SessionManager,
    SlowMetadataProber, StartupManager, SystemOperator,
};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn process_sample_scratch_reuses_capacity_and_slices_strings() {
        let mut scratch = ProcessSampleScratch::<()>::with_capacity(16, 128);
        let (off1, len1) = scratch.push_str("explorer.exe");
        let (off2, len2) = scratch.push_str("Loom 任务管理器");
        let (off3, len3) = scratch.push_str("");

        assert_eq!(scratch.str_slice(off1, len1), "explorer.exe");
        assert_eq!(scratch.str_slice(off2, len2), "Loom 任务管理器");
        assert_eq!(scratch.str_slice(off3, len3), "");
        assert_eq!(scratch.str_slice(9999, 10), "");

        let prev_cap = scratch.string_buf.capacity();
        scratch.clear();
        assert!(scratch.samples.is_empty());
        assert!(scratch.string_buf.is_empty());
        assert_eq!(scratch.string_buf.capacity(), prev_cap);
    }

    #[test]
    fn perf_sample_scratch_clear_retains_capacity() {
        let mut scratch = PerfSampleScratch::new();
        scratch
            .per_core_permille
            .extend_from_slice(&[100, 200, 300]);
        scratch
            .networks
            .push(loom_types::NetAdapterSample::default());
        scratch.gpus.push(loom_types::GpuAdapterSample::default());
        let cap = scratch.per_core_permille.capacity();
        let net_cap = scratch.networks.capacity();
        let gpu_cap = scratch.gpus.capacity();
        scratch.clear();
        assert!(scratch.per_core_permille.is_empty());
        assert!(scratch.networks.is_empty());
        assert!(scratch.gpus.is_empty());
        assert_eq!(scratch.per_core_permille.capacity(), cap);
        assert_eq!(scratch.networks.capacity(), net_cap);
        assert_eq!(scratch.gpus.capacity(), gpu_cap);
    }
}
