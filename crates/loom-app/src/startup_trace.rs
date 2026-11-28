//! Zero-overhead opt-in startup phase tracer (`--startup-trace[=<path>]`).
//!
//! When disabled (default), every [`mark`] call compiles down to a single relaxed atomic load.
//! When enabled, records microsecond offsets from process entry (`main_start`) and writes
//! line-buffered entries to `stderr` or an explicit file path (works cleanly under
//! `windows_subsystem = "windows"` without allocating a console).

use std::fs::{File, OpenOptions};
use std::io::Write as _;
use std::path::PathBuf;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;

use parking_lot::Mutex;

/// Output destination for `--startup-trace`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum StartupTraceTarget {
    /// Emit trace lines to standard error (visible when redirected or attached to a console).
    Stderr,
    /// Write trace lines directly to a file path (works even without an attached console).
    File(PathBuf),
}

struct TraceState {
    start: Instant,
    sink: Mutex<TraceSink>,
}

enum TraceSink {
    Stderr,
    File(File),
}

static ENABLED: AtomicBool = AtomicBool::new(false);
static STATE: OnceLock<TraceState> = OnceLock::new();

/// Formats a single startup trace line with millisecond precision (`+X.XXX ms`).
#[inline]
pub(crate) fn format_trace_line(elapsed_us: u128, phase: &str) -> String {
    let ms_whole = elapsed_us / 1_000;
    let ms_frac = elapsed_us % 1_000;
    format!("[startup-trace] +{ms_whole}.{ms_frac:03} ms | {phase}\n")
}

/// Initializes the global startup tracer if `target` is `Some(_)`.
///
/// Safe to call once at the very start of `main()`. Passing `None` leaves the tracer
/// completely inactive with zero allocations or file I/O.
pub(crate) fn init(target: Option<StartupTraceTarget>) {
    let Some(target) = target else {
        return;
    };

    let sink = match target {
        StartupTraceTarget::Stderr => TraceSink::Stderr,
        StartupTraceTarget::File(path) => {
            if let Some(parent) = path.parent()
                && !parent.as_os_str().is_empty()
            {
                let _ = std::fs::create_dir_all(parent);
            }
            match OpenOptions::new()
                .create(true)
                .write(true)
                .truncate(true)
                .open(&path)
            {
                Ok(file) => TraceSink::File(file),
                Err(_) => TraceSink::Stderr,
            }
        }
    };

    let state = TraceState {
        start: Instant::now(),
        sink: Mutex::new(sink),
    };
    if STATE.set(state).is_ok() {
        ENABLED.store(true, Ordering::Release);
        mark("main_start");
    }
}

/// Records a startup milestone if `--startup-trace` was enabled.
#[inline]
pub(crate) fn mark(phase: &'static str) {
    if !ENABLED.load(Ordering::Relaxed) {
        return;
    }
    mark_slow(phase);
}

/// Records a one-shot startup milestone guarded by `flag`.
#[inline]
pub(crate) fn mark_once(flag: &AtomicBool, phase: &'static str) {
    if !ENABLED.load(Ordering::Relaxed) {
        return;
    }
    if !flag.swap(true, Ordering::AcqRel) {
        mark_slow(phase);
    }
}

#[cold]
fn mark_slow(phase: &'static str) {
    let Some(state) = STATE.get() else {
        return;
    };
    let elapsed_us = state.start.elapsed().as_micros();
    let line = format_trace_line(elapsed_us, phase);
    let mut sink = state.sink.lock();
    match &mut *sink {
        TraceSink::Stderr => {
            let mut stderr = std::io::stderr().lock();
            let _ = stderr.write_all(line.as_bytes());
            let _ = stderr.flush();
        }
        TraceSink::File(file) => {
            let _ = file.write_all(line.as_bytes());
            let _ = file.flush();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::format_trace_line;

    #[test]
    fn format_trace_line_renders_millisecond_precision() {
        assert_eq!(
            format_trace_line(0, "main_start"),
            "[startup-trace] +0.000 ms | main_start\n"
        );
        assert_eq!(
            format_trace_line(12_345, "main_window_opened"),
            "[startup-trace] +12.345 ms | main_window_opened\n"
        );
        assert_eq!(
            format_trace_line(405_007, "first_frame_painted"),
            "[startup-trace] +405.007 ms | first_frame_painted\n"
        );
    }
}
