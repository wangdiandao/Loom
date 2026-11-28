//! Caller-owned reusable scratch buffers for zero-allocation fast-lane sampling.

use loom_types::{GpuAdapterSample, NetAdapterSample, ProcessHotSample, SystemPerfSample};

/// Reusable scratch buffer passed to [`crate::FastSystemSampler::sample_processes`].
///
/// The engine allocates a single `ProcessSampleScratch` on the fast sampling thread
/// and reuses it every tick via [`Self::clear`], eliminating per-tick heap allocations
/// for both process sample vectors and process/window name strings.
#[derive(Debug, Clone)]
pub struct ProcessSampleScratch<Ext = ()> {
    /// Hot per-process samples populated during the current tick.
    pub samples: Vec<ProcessHotSample<Ext>>,
    /// Contiguous UTF-8 arena backing `(offset, len)` string references inside `samples`.
    pub string_buf: String,
    /// System-wide summary counters captured alongside the process snapshot.
    pub perf: SystemPerfSample,
}

impl<Ext> Default for ProcessSampleScratch<Ext> {
    fn default() -> Self {
        Self::new()
    }
}

impl<Ext> ProcessSampleScratch<Ext> {
    /// Creates an empty scratch buffer with zero initial capacity.
    #[inline]
    pub fn new() -> Self {
        Self {
            samples: Vec::new(),
            string_buf: String::new(),
            perf: SystemPerfSample::default(),
        }
    }

    /// Creates an empty scratch buffer pre-sized for `sample_cap` processes and
    /// `string_bytes` bytes of string data.
    pub fn with_capacity(sample_cap: usize, string_bytes: usize) -> Self {
        Self {
            samples: Vec::with_capacity(sample_cap),
            string_buf: String::with_capacity(string_bytes),
            perf: SystemPerfSample::default(),
        }
    }

    /// Clears all samples and strings while preserving allocated heap capacities.
    #[inline]
    pub fn clear(&mut self) {
        self.samples.clear();
        self.string_buf.clear();
        self.perf = SystemPerfSample::default();
    }

    /// Appends `s` into the string arena and returns `(byte_offset, byte_len)`.
    ///
    /// Strings longer than `u16::MAX` bytes are safely truncated on a UTF-8 character
    /// boundary. If the arena already exceeds `u32::MAX` bytes, returns `(0, 0)`.
    pub fn push_str(&mut self, s: &str) -> (u32, u16) {
        if s.is_empty() {
            return (0, 0);
        }
        let Ok(offset) = u32::try_from(self.string_buf.len()) else {
            return (0, 0);
        };
        let truncated = if s.len() <= usize::from(u16::MAX) {
            s
        } else {
            &s[..s.floor_char_boundary(usize::from(u16::MAX))]
        };
        self.string_buf.push_str(truncated);
        (offset, truncated.len() as u16)
    }

    /// Resolves a `(byte_offset, byte_len)` slice previously returned by [`Self::push_str`].
    ///
    /// Returns `""` if `len == 0` or if the range is out of bounds / not on UTF-8 boundaries.
    #[inline]
    pub fn str_slice(&self, offset: u32, len: u16) -> &str {
        if len == 0 {
            return "";
        }
        let start = offset as usize;
        let Some(end) = start.checked_add(usize::from(len)) else {
            return "";
        };
        self.string_buf.get(start..end).unwrap_or("")
    }
}

/// Reusable scratch buffer passed to [`crate::FastSystemSampler::sample_performance`].
#[derive(Debug, Default, Clone)]
pub struct PerfSampleScratch {
    /// Latest system-wide hardware performance sample.
    pub sample: SystemPerfSample,
    /// Latest per-logical-CPU utilization values in permille (`0..=1000`).
    pub per_core_permille: Vec<u16>,
    /// Latest per-network-adapter telemetry samples (Ethernet / Wi-Fi).
    pub networks: Vec<NetAdapterSample>,
    /// Latest per-GPU-adapter telemetry samples (`GPU 0..N`).
    pub gpus: Vec<GpuAdapterSample>,
}

impl PerfSampleScratch {
    /// Creates an empty performance scratch buffer.
    #[inline]
    pub fn new() -> Self {
        Self::default()
    }

    /// Resets counters and clears device vectors while retaining allocated capacity.
    #[inline]
    pub fn clear(&mut self) {
        self.sample = SystemPerfSample::default();
        self.per_core_permille.clear();
        self.networks.clear();
        self.gpus.clear();
    }
}
