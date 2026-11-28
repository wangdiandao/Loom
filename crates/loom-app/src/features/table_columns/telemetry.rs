//! Zero-allocation atomic list-rendering telemetry for 120Hz frame-budget diagnostics.

use std::sync::atomic::{AtomicU64, Ordering};

/// High-frequency virtualized table views tracked by [`ListTelemetryRegistry`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(usize)]
pub(crate) enum ListViewTelemetryKind {
    Processes = 0,
    Details = 1,
}

impl ListViewTelemetryKind {
    #[inline]
    const fn index(self) -> usize {
        self as usize
    }

    #[inline]
    const fn label(self) -> &'static str {
        match self {
            Self::Processes => "Processes",
            Self::Details => "Details",
        }
    }
}

/// Immutable point-in-time snapshot of one table's rendering telemetry counters.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ListTelemetrySnapshot {
    pub(crate) commits: u64,
    pub(crate) viewport_fp_skips: u64,
    pub(crate) scroll_defers: u64,
    pub(crate) icon_uploads: u64,
    pub(crate) icon_defers: u64,
    pub(crate) last_commit_us: u64,
    pub(crate) max_commit_us: u64,
    pub(crate) render_td_calls: u64,
}

struct ListViewTelemetryCounters {
    commits: AtomicU64,
    viewport_fp_skips: AtomicU64,
    scroll_defers: AtomicU64,
    icon_uploads: AtomicU64,
    icon_defers: AtomicU64,
    last_commit_us: AtomicU64,
    max_commit_us: AtomicU64,
    render_td_calls: AtomicU64,
}

impl ListViewTelemetryCounters {
    const fn new() -> Self {
        Self {
            commits: AtomicU64::new(0),
            viewport_fp_skips: AtomicU64::new(0),
            scroll_defers: AtomicU64::new(0),
            icon_uploads: AtomicU64::new(0),
            icon_defers: AtomicU64::new(0),
            last_commit_us: AtomicU64::new(0),
            max_commit_us: AtomicU64::new(0),
            render_td_calls: AtomicU64::new(0),
        }
    }

    #[inline]
    fn snapshot(&self) -> ListTelemetrySnapshot {
        ListTelemetrySnapshot {
            commits: self.commits.load(Ordering::Relaxed),
            viewport_fp_skips: self.viewport_fp_skips.load(Ordering::Relaxed),
            scroll_defers: self.scroll_defers.load(Ordering::Relaxed),
            icon_uploads: self.icon_uploads.load(Ordering::Relaxed),
            icon_defers: self.icon_defers.load(Ordering::Relaxed),
            last_commit_us: self.last_commit_us.load(Ordering::Relaxed),
            max_commit_us: self.max_commit_us.load(Ordering::Relaxed),
            render_td_calls: self.render_td_calls.load(Ordering::Relaxed),
        }
    }
}

/// Lock-free, zero-allocation atomic counter registry tracking UI frame-budget efficiency
/// for high-frequency virtualized tables.
///
/// Contains strictly aggregate numeric counters (no PIDs, names, or paths) and is safe to
/// surface in `tracing::debug!` logs and the privacy-safe Diagnostic Summary.
pub(crate) struct ListTelemetryRegistry {
    views: [ListViewTelemetryCounters; 2],
}

static LIST_TELEMETRY: ListTelemetryRegistry = ListTelemetryRegistry::new();

/// Returns the process-wide [`ListTelemetryRegistry`].
#[inline]
pub(crate) fn list_telemetry() -> &'static ListTelemetryRegistry {
    &LIST_TELEMETRY
}

impl ListTelemetryRegistry {
    /// Creates a zeroed [`ListTelemetryRegistry`].
    pub(crate) const fn new() -> Self {
        Self {
            views: [
                ListViewTelemetryCounters::new(),
                ListViewTelemetryCounters::new(),
            ],
        }
    }

    /// Records a `commit_snapshot` completion, its wall duration in microseconds, and
    /// whether the viewport fingerprint check triggered a repaint (`table_notified`).
    #[inline]
    pub(crate) fn record_commit(
        &self,
        kind: ListViewTelemetryKind,
        duration_us: u64,
        table_notified: bool,
    ) {
        let c = &self.views[kind.index()];
        let commits = c.commits.fetch_add(1, Ordering::Relaxed) + 1;
        if !table_notified {
            c.viewport_fp_skips.fetch_add(1, Ordering::Relaxed);
        }
        c.last_commit_us.store(duration_us, Ordering::Relaxed);
        c.max_commit_us.fetch_max(duration_us, Ordering::Relaxed);

        if commits.is_multiple_of(64) {
            let snap = c.snapshot();
            tracing::debug!(
                view = kind.label(),
                commits = snap.commits,
                fp_skips = snap.viewport_fp_skips,
                scroll_defers = snap.scroll_defers,
                icon_uploads = snap.icon_uploads,
                icon_defers = snap.icon_defers,
                last_commit_us = snap.last_commit_us,
                max_commit_us = snap.max_commit_us,
                render_td_calls = snap.render_td_calls,
                "list render telemetry sample"
            );
        }
    }

    /// Records a snapshot deferred during an active scroll burst.
    #[inline]
    pub(crate) fn record_scroll_defer(&self, kind: ListViewTelemetryKind) {
        self.views[kind.index()]
            .scroll_defers
            .fetch_add(1, Ordering::Relaxed);
    }

    /// Records a newly converted GPU `RenderImage` icon upload.
    #[inline]
    pub(crate) fn record_icon_upload(&self, kind: ListViewTelemetryKind) {
        self.views[kind.index()]
            .icon_uploads
            .fetch_add(1, Ordering::Relaxed);
    }

    /// Records an uncached icon deferred to the next frame by the per-frame upload cap.
    #[inline]
    pub(crate) fn record_icon_defer(&self, kind: ListViewTelemetryKind) {
        self.views[kind.index()]
            .icon_defers
            .fetch_add(1, Ordering::Relaxed);
    }

    /// Records a single `TableDelegate::render_td` cell evaluation.
    #[inline]
    pub(crate) fn record_render_td(&self, kind: ListViewTelemetryKind) {
        self.views[kind.index()]
            .render_td_calls
            .fetch_add(1, Ordering::Relaxed);
    }

    /// Reads an atomic snapshot of counters for `kind`.
    #[inline]
    pub(crate) fn snapshot(&self, kind: ListViewTelemetryKind) -> ListTelemetrySnapshot {
        self.views[kind.index()].snapshot()
    }

    /// Formats a privacy-safe single-line diagnostic summary across views that recorded
    /// at least one snapshot commit, or `None` if no list commits have occurred yet.
    pub(crate) fn format_diagnostic_line(&self) -> Option<String> {
        use std::fmt::Write as _;

        let mut parts: Vec<String> = Vec::with_capacity(2);
        for kind in [
            ListViewTelemetryKind::Processes,
            ListViewTelemetryKind::Details,
        ] {
            let s = self.snapshot(kind);
            if s.commits == 0 {
                continue;
            }
            let mut seg = String::with_capacity(96);
            let _ = write!(
                seg,
                "{}(commits={}, fp_skips={}, scroll_defers={}, icon_uploads={}, icon_defers={}, max_commit={}us)",
                kind.label(),
                s.commits,
                s.viewport_fp_skips,
                s.scroll_defers,
                s.icon_uploads,
                s.icon_defers,
                s.max_commit_us,
            );
            parts.push(seg);
        }
        if parts.is_empty() {
            None
        } else {
            Some(parts.join("; "))
        }
    }
}
