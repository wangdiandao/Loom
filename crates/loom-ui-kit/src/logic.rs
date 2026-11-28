//! Pure, GPUI-independent UI layout, chart scaling, engine selection, ordering,
//! and CSV export logic for Loom.
//!
//! Keeping these algorithms free of rendering dependencies allows fast,
//! deterministic unit testing and reuse across headless and UI layers.

/// Active utilization threshold (`1.0%`, or `10` permille) used by
/// [`select_active_gpu_engines`] to identify non-idle GPU engines.
pub const GPU_ENGINE_ACTIVE_THRESHOLD_PERMILLE: u16 = 10;

/// UTF-8 Byte Order Mark prepended to exported CSV files so Excel and Calc
/// detect UTF-8 encoding reliably on Windows.
pub const CSV_UTF8_BOM: char = '\u{FEFF}';

const KIB_U64: u64 = 1024;
const MIB_U64: u64 = 1024 * KIB_U64;
const GIB_U64: u64 = 1024 * MIB_U64;

const KIB_F64: f64 = KIB_U64 as f64;
const MIB_F64: f64 = MIB_U64 as f64;
const GIB_F64: f64 = GIB_U64 as f64;

const KBPS_U128: u128 = 1_000;
const MBPS_U128: u128 = 1_000_000;
const GBPS_U128: u128 = 1_000_000_000;

/// Bytes per second corresponding to 1 Kbps (`1_000 / 8`).
const BYTES_PER_KBPS: f64 = 125.0;
/// Bytes per second corresponding to 1 Mbps (`1_000_000 / 8`).
const BYTES_PER_MBPS: f64 = 125_000.0;
/// Bytes per second corresponding to 1 Gbps (`1_000_000_000 / 8`).
const BYTES_PER_GBPS: f64 = 125_000_000.0;

/// Snaps a peak storage/transfer rate (in bytes per second) to a clean,
/// jitter-resistant Y-axis ceiling.
///
/// Returns `(y_max_in_unit, unit_label, divisor)` where dividing any raw
/// `bytes_per_sec` sample by `divisor` maps it into `unit_label` within
/// `0.0..=y_max_in_unit`.
///
/// Fixed steps cover `100 KB/s` through `50 GB/s` using binary (`1024`-based)
/// units (`100`, `200`, `500 KB/s`, `1`, `2`, `5`, `10`, `20`, `50`, `100`,
/// `200`, `500 MB/s`, `1`, `2`, `5`, `10`, `20`, `50 GB/s`). Values above
/// `50 GB/s` extend cleanly along `1-2-5` decimal steps in `GB/s`.
pub fn stepped_throughput_ceiling(max_bytes_per_sec: u64) -> (f64, &'static str, f64) {
    const STEPS: &[(u64, f64, &str, f64)] = &[
        (100 * KIB_U64, 100.0, "KB/s", KIB_F64),
        (200 * KIB_U64, 200.0, "KB/s", KIB_F64),
        (500 * KIB_U64, 500.0, "KB/s", KIB_F64),
        (MIB_U64, 1.0, "MB/s", MIB_F64),
        (2 * MIB_U64, 2.0, "MB/s", MIB_F64),
        (5 * MIB_U64, 5.0, "MB/s", MIB_F64),
        (10 * MIB_U64, 10.0, "MB/s", MIB_F64),
        (20 * MIB_U64, 20.0, "MB/s", MIB_F64),
        (50 * MIB_U64, 50.0, "MB/s", MIB_F64),
        (100 * MIB_U64, 100.0, "MB/s", MIB_F64),
        (200 * MIB_U64, 200.0, "MB/s", MIB_F64),
        (500 * MIB_U64, 500.0, "MB/s", MIB_F64),
        (GIB_U64, 1.0, "GB/s", GIB_F64),
        (2 * GIB_U64, 2.0, "GB/s", GIB_F64),
        (5 * GIB_U64, 5.0, "GB/s", GIB_F64),
        (10 * GIB_U64, 10.0, "GB/s", GIB_F64),
        (20 * GIB_U64, 20.0, "GB/s", GIB_F64),
        (50 * GIB_U64, 50.0, "GB/s", GIB_F64),
    ];

    for &(limit, y_max, unit, divisor) in STEPS {
        if max_bytes_per_sec <= limit {
            return (y_max, unit, divisor);
        }
    }

    let gib_val = max_bytes_per_sec as f64 / GIB_F64;
    let stepped = ceil_1_2_5(gib_val);
    (stepped, "GB/s", GIB_F64)
}

/// Converts a peak network rate (in bytes per second) to decimal bits per
/// second and snaps it to a clean, jitter-resistant Y-axis ceiling.
///
/// Returns `(y_max_in_unit, unit_label, divisor)` where `divisor` converts raw
/// `bytes_per_sec` samples directly into `unit_label` (`125.0` for `Kbps`,
/// `125_000.0` for `Mbps`, `125_000_000.0` for `Gbps`), so
/// `sample_bytes_per_sec as f64 / divisor` maps directly onto
/// `0.0..=y_max_in_unit`.
///
/// Fixed steps cover `100 Kbps` through `100 Gbps` (`100`, `250`, `500 Kbps`,
/// `1`, `2`, `5`, `10`, `25`, `50`, `100`, `250`, `500 Mbps`, `1`, `2.5`, `5`,
/// `10`, `25`, `40`, `100 Gbps`). Values above `100 Gbps` extend cleanly along
/// `1-2-5` decimal steps in `Gbps`.
pub fn stepped_network_ceiling(max_bytes_per_sec: u64) -> (f64, &'static str, f64) {
    const STEPS: &[(u128, f64, &str, f64)] = &[
        (100 * KBPS_U128, 100.0, "Kbps", BYTES_PER_KBPS),
        (250 * KBPS_U128, 250.0, "Kbps", BYTES_PER_KBPS),
        (500 * KBPS_U128, 500.0, "Kbps", BYTES_PER_KBPS),
        (MBPS_U128, 1.0, "Mbps", BYTES_PER_MBPS),
        (2 * MBPS_U128, 2.0, "Mbps", BYTES_PER_MBPS),
        (5 * MBPS_U128, 5.0, "Mbps", BYTES_PER_MBPS),
        (10 * MBPS_U128, 10.0, "Mbps", BYTES_PER_MBPS),
        (25 * MBPS_U128, 25.0, "Mbps", BYTES_PER_MBPS),
        (50 * MBPS_U128, 50.0, "Mbps", BYTES_PER_MBPS),
        (100 * MBPS_U128, 100.0, "Mbps", BYTES_PER_MBPS),
        (250 * MBPS_U128, 250.0, "Mbps", BYTES_PER_MBPS),
        (500 * MBPS_U128, 500.0, "Mbps", BYTES_PER_MBPS),
        (GBPS_U128, 1.0, "Gbps", BYTES_PER_GBPS),
        (2_500_000_000, 2.5, "Gbps", BYTES_PER_GBPS),
        (5 * GBPS_U128, 5.0, "Gbps", BYTES_PER_GBPS),
        (10 * GBPS_U128, 10.0, "Gbps", BYTES_PER_GBPS),
        (25 * GBPS_U128, 25.0, "Gbps", BYTES_PER_GBPS),
        (40 * GBPS_U128, 40.0, "Gbps", BYTES_PER_GBPS),
        (100 * GBPS_U128, 100.0, "Gbps", BYTES_PER_GBPS),
    ];

    let max_bps = u128::from(max_bytes_per_sec) * 8;
    for &(limit_bps, y_max, unit, divisor) in STEPS {
        if max_bps <= limit_bps {
            return (y_max, unit, divisor);
        }
    }

    let gbps_val = max_bytes_per_sec as f64 / BYTES_PER_GBPS;
    let stepped = ceil_1_2_5(gbps_val);
    (stepped, "Gbps", BYTES_PER_GBPS)
}

/// Rounds a positive finite value up to the next `1`, `2`, or `5` * `10^k` step.
fn ceil_1_2_5(value: f64) -> f64 {
    if !value.is_finite() || value <= 0.0 {
        return 1.0;
    }
    let exp = value.log10().floor();
    let mag = 10.0_f64.powf(exp);
    let norm = value / mag;
    let factor = if norm <= 1.0 {
        1.0
    } else if norm <= 2.0 {
        2.0
    } else if norm <= 5.0 {
        5.0
    } else {
        10.0
    };
    factor * mag
}

/// Header density tier for individual logical-core chart cards.
///
/// Degrades progressively from [`Self::Full`] -> [`Self::Short`] -> [`Self::Hidden`]
/// as card height shrinks so the chart plot retains readable vertical space.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum CoreCardHeaderMode {
    /// Full header showing logical core index, core class badge, and utilization percentage.
    Full,
    /// Compact single-line header showing abbreviated index and percentage.
    Short,
    /// Header hidden to maximize sparkline plot height on dense high-core-count grids.
    Hidden,
}

/// Computed grid geometry for the Performance view's logical-processor grid.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CoreGridLayout {
    /// Selected column count (`0` only when `core_count == 0`, otherwise `1..=16`).
    pub columns: usize,
    /// Resulting row count (`ceil(core_count / columns)`).
    pub rows: usize,
    /// Header density tier chosen for the computed card height.
    pub header_mode: CoreCardHeaderMode,
    /// Computed outer height of each core card in logical pixels.
    pub card_height_px: f32,
}

/// Maximum number of columns considered by [`compute_core_grid_layout`].
const MAX_CORE_GRID_COLUMNS: usize = 16;
/// Gap between adjacent cards in logical pixels.
const CORE_GRID_GAP_PX: f32 = 8.0;
/// Horizontal inner padding + border allowance per card.
const CORE_CARD_PAD_X_PX: f32 = 12.0;
/// Vertical inner padding + border allowance per card.
const CORE_CARD_PAD_Y_PX: f32 = 10.0;
/// Header height for [`CoreCardHeaderMode::Full`].
const CORE_HEADER_FULL_PX: f32 = 24.0;
/// Header height for [`CoreCardHeaderMode::Short`].
const CORE_HEADER_SHORT_PX: f32 = 16.0;
/// Comfortable sparkline plot height before degrading `Full` -> `Short`.
const CORE_COMFORT_PLOT_HEIGHT_PX: f32 = 44.0;
/// Minimum readable sparkline plot height before introducing vertical scrolling.
const CORE_MIN_PLOT_HEIGHT_PX: f32 = 28.0;
/// Minimum comfortable card width before penalizing horizontal crowding.
const CORE_MIN_CARD_WIDTH_PX: f32 = 64.0;
/// Ideal plot aspect ratio (`plot_width / plot_height` ≈ `1.4:1`).
const CORE_TARGET_PLOT_ASPECT: f32 = 1.4;
/// Required relative cost improvement (`8%`) before switching away from `prev_columns`.
const CORE_GRID_HYSTERESIS_RATIO: f32 = 0.08;

#[derive(Debug, Clone, Copy)]
struct EvaluatedCoreCandidate {
    layout: CoreGridLayout,
    cost: f32,
}

fn evaluate_core_grid_candidate(
    core_count: usize,
    cols: usize,
    avail_width_px: f32,
    avail_height_px: f32,
) -> EvaluatedCoreCandidate {
    let rows = core_count.div_ceil(cols);
    let total_gap_x = CORE_GRID_GAP_PX * cols.saturating_sub(1) as f32;
    let total_gap_y = CORE_GRID_GAP_PX * rows.saturating_sub(1) as f32;

    let card_width = ((avail_width_px - total_gap_x) / cols as f32).max(1.0);
    let plot_width = (card_width - CORE_CARD_PAD_X_PX).max(1.0);

    let raw_fit_height = (avail_height_px - total_gap_y) / rows as f32;

    // Progressively degrade header density as available card height shrinks,
    // preserving readable plot height before compressing the plot itself.
    let full_threshold = CORE_CARD_PAD_Y_PX + CORE_HEADER_FULL_PX + CORE_COMFORT_PLOT_HEIGHT_PX;
    let short_threshold = CORE_CARD_PAD_Y_PX + CORE_HEADER_SHORT_PX + CORE_MIN_PLOT_HEIGHT_PX;
    let (header_mode, header_height, header_penalty) = if raw_fit_height >= full_threshold {
        (CoreCardHeaderMode::Full, CORE_HEADER_FULL_PX, 0.0_f32)
    } else if raw_fit_height >= short_threshold {
        (CoreCardHeaderMode::Short, CORE_HEADER_SHORT_PX, 0.05_f32)
    } else {
        (CoreCardHeaderMode::Hidden, 0.0_f32, 0.12_f32)
    };

    let min_card_height = CORE_CARD_PAD_Y_PX + header_height + CORE_MIN_PLOT_HEIGHT_PX;
    let card_height_px = raw_fit_height.max(min_card_height);
    let plot_height = (card_height_px - CORE_CARD_PAD_Y_PX - header_height).max(1.0);

    // Symmetric log-distance from the ideal ~1.4:1 plot aspect ratio.
    let aspect = plot_width / plot_height;
    let aspect_cost = (aspect.ln() - CORE_TARGET_PLOT_ASPECT.ln()).abs();

    // Penalize jagged trailing rows (empty slots / single-card orphans).
    let rem = core_count % cols;
    let orphan_cost = if rem == 0 {
        0.0
    } else {
        let empty_slots = cols - rem;
        let base = 0.45 * (empty_slots as f32 / cols as f32);
        if rem == 1 && cols > 2 {
            base + 0.15
        } else {
            base
        }
    };

    // Penalize vertical overflow that would force scrolling.
    let required_height = rows as f32 * card_height_px + total_gap_y;
    let overflow_cost = if required_height > avail_height_px {
        let overflow_ratio = (required_height - avail_height_px) / avail_height_px.max(1.0);
        0.35 + 1.5 * overflow_ratio
    } else {
        0.0
    };

    // Penalize overly narrow cards that squeeze sparkline readability.
    let narrow_cost = if card_width < CORE_MIN_CARD_WIDTH_PX {
        1.2 * (CORE_MIN_CARD_WIDTH_PX - card_width) / CORE_MIN_CARD_WIDTH_PX
    } else {
        0.0
    };

    let cost = aspect_cost + orphan_cost + overflow_cost + narrow_cost + header_penalty;

    EvaluatedCoreCandidate {
        layout: CoreGridLayout {
            columns: cols,
            rows,
            header_mode,
            card_height_px,
        },
        cost,
    }
}

/// Computes an adaptive logical-core grid layout for `core_count` cores inside
/// a viewport of `avail_width_px` × `avail_height_px`.
///
/// Candidate column counts (`1..=core_count.min(16)`) are scored by closeness
/// to an ideal `~1.4:1` sparkline plot aspect ratio while penalizing jagged
/// trailing rows, narrow cards, and vertical overflow.
///
/// When `prev_columns` is provided and remains valid (`1..=core_count.min(16)`),
/// an `8%` hysteresis dead-band prevents column-count oscillation during
/// interactive window resizing: the grid switches away from `prev_columns` only
/// when another column count lowers the layout cost by more than `8%`.
pub fn compute_core_grid_layout(
    core_count: usize,
    avail_width_px: f32,
    avail_height_px: f32,
    prev_columns: Option<usize>,
) -> CoreGridLayout {
    if core_count == 0 {
        return CoreGridLayout {
            columns: 0,
            rows: 0,
            header_mode: CoreCardHeaderMode::Full,
            card_height_px: 0.0,
        };
    }

    let width = if avail_width_px.is_finite() && avail_width_px > 1.0 {
        avail_width_px
    } else {
        1.0
    };
    let height = if avail_height_px.is_finite() && avail_height_px > 1.0 {
        avail_height_px
    } else {
        1.0
    };

    let max_cols = core_count.min(MAX_CORE_GRID_COLUMNS);
    let mut best: Option<EvaluatedCoreCandidate> = None;
    let mut prev_candidate: Option<EvaluatedCoreCandidate> = None;

    for cols in 1..=max_cols {
        let candidate = evaluate_core_grid_candidate(core_count, cols, width, height);
        if prev_columns == Some(cols) {
            prev_candidate = Some(candidate);
        }
        let is_better = match best {
            None => true,
            Some(current) => candidate.cost < current.cost,
        };
        if is_better {
            best = Some(candidate);
        }
    }

    let best = best.expect("core_count >= 1 guarantees at least one candidate");

    if let Some(prev) = prev_candidate {
        let switch_threshold = prev.cost * (1.0 - CORE_GRID_HYSTERESIS_RATIO);
        if best.cost >= switch_threshold {
            return prev.layout;
        }
    }

    best.layout
}

/// Selects the most relevant GPU engine indices to display across up to
/// `max_panes` chart panes.
///
/// Engines whose peak utilization reached at least
/// [`GPU_ENGINE_ACTIVE_THRESHOLD_PERMILLE`] (`10` permille / `1.0%`) in the
/// current window are selected first, ordered by peak utilization descending
/// and then engine index ascending.
///
/// If all engines are idle (`< 1.0%`), always falls back to a single engine:
/// the highest-peak engine (breaking ties toward the lowest index, e.g.,
/// index `0`). Returns an empty vector only if `engine_peaks_permille` is empty
/// or `max_panes == 0`.
pub fn select_active_gpu_engines(
    engine_peaks_permille: &[(usize, u16)],
    max_panes: usize,
) -> Vec<usize> {
    if engine_peaks_permille.is_empty() || max_panes == 0 {
        return Vec::new();
    }

    let mut active: Vec<(usize, u16)> = engine_peaks_permille
        .iter()
        .copied()
        .filter(|&(_, peak)| peak >= GPU_ENGINE_ACTIVE_THRESHOLD_PERMILLE)
        .collect();

    if !active.is_empty() {
        active.sort_unstable_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
        active.truncate(max_panes);
        return active.into_iter().map(|(idx, _)| idx).collect();
    }

    // All engines are idle: keep at least one engine (highest peak, tie-breaking
    // toward index 0 / smallest index).
    let fallback = engine_peaks_permille
        .iter()
        .copied()
        .min_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)))
        .expect("engine_peaks_permille is non-empty");

    vec![fallback.0]
}

/// Returns `true` if `value` begins with (or has leading whitespace followed by)
/// a spreadsheet formula trigger (`=`, `+`, `-`, `@`, `\t`, or `\r`).
fn is_csv_formula_trigger(value: &str) -> bool {
    for ch in value.chars() {
        if matches!(ch, '=' | '+' | '-' | '@' | '\t' | '\r') {
            return true;
        }
        if !ch.is_whitespace() {
            return false;
        }
    }
    false
}

/// Appends a single RFC 4180 + formula-injection-safe CSV cell directly into `out`.
fn push_csv_cell(out: &mut String, value: &str) {
    let needs_formula_escape = is_csv_formula_trigger(value);
    let needs_quoting = value.contains([',', '"', '\n', '\r']);

    if !needs_formula_escape && !needs_quoting {
        out.push_str(value);
        return;
    }

    if needs_quoting {
        out.push('"');
    }
    if needs_formula_escape {
        out.push('\'');
    }
    if needs_quoting {
        for ch in value.chars() {
            if ch == '"' {
                out.push('"');
            }
            out.push(ch);
        }
        out.push('"');
    } else {
        out.push_str(value);
    }
}

/// Formats a single CSV cell safely per RFC 4180 while defending against
/// spreadsheet formula injection (CSV / DDE injection).
///
/// If `value` begins with `=`, `+`, `-`, `@`, `\t`, or `\r` (including after
/// arbitrary leading whitespace), a leading single quote (`'`) is prepended so
/// Excel, LibreOffice Calc, and Google Sheets treat the cell strictly as text.
/// If the resulting text contains `,`, `"`, `\n`, or `\r`, it is enclosed in
/// double quotes with internal `"` characters doubled.
pub fn format_csv_cell(value: &str) -> String {
    let mut out = String::with_capacity(value.len() + 4);
    push_csv_cell(&mut out, value);
    out
}

/// Builds a complete UTF-8 BOM + CRLF RFC 4180 CSV document using
/// [`format_csv_cell`] on every header and data cell.
pub fn build_csv_document(headers: &[&str], rows: &[Vec<String>]) -> String {
    let estimated_bytes = 3
        + headers.iter().map(|h| h.len() + 1).sum::<usize>()
        + rows
            .iter()
            .map(|r| r.iter().map(|c| c.len() + 2).sum::<usize>() + 2)
            .sum::<usize>();
    let mut out = String::with_capacity(estimated_bytes);
    out.push(CSV_UTF8_BOM);

    if !headers.is_empty() {
        for (i, header) in headers.iter().enumerate() {
            if i > 0 {
                out.push(',');
            }
            push_csv_cell(&mut out, header);
        }
        out.push_str("\r\n");
    }

    for row in rows {
        for (i, cell) in row.iter().enumerate() {
            if i > 0 {
                out.push(',');
            }
            push_csv_cell(&mut out, cell);
        }
        out.push_str("\r\n");
    }

    out
}

/// Preserves the device/row ordering established when a view opened while
/// appending newly arrived keys at the end and omitting removed keys.
///
/// Updates `pinned_order` in place by appending any unseen keys from
/// `current_keys` in their order of appearance, then returns the index order
/// into `current_keys` matching `pinned_order` with absent keys omitted.
pub fn merge_stable_order<K: Eq + Clone>(
    pinned_order: &mut Vec<K>,
    current_keys: &[K],
) -> Vec<usize> {
    for key in current_keys {
        if !pinned_order.contains(key) {
            pinned_order.push(key.clone());
        }
    }

    let mut ordered_indices = Vec::with_capacity(current_keys.len());
    for pinned in pinned_order.iter() {
        for (idx, current) in current_keys.iter().enumerate() {
            if current == pinned {
                ordered_indices.push(idx);
            }
        }
    }
    ordered_indices
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stepped_throughput_ceiling_covers_all_clean_steps_and_high_rates() {
        const KIB: u64 = 1024;
        const MIB: u64 = 1024 * KIB;
        const GIB: u64 = 1024 * MIB;

        assert_eq!(stepped_throughput_ceiling(0), (100.0, "KB/s", KIB as f64));
        assert_eq!(
            stepped_throughput_ceiling(100 * KIB),
            (100.0, "KB/s", KIB as f64)
        );
        assert_eq!(
            stepped_throughput_ceiling(100 * KIB + 1),
            (200.0, "KB/s", KIB as f64)
        );
        assert_eq!(
            stepped_throughput_ceiling(350 * KIB),
            (500.0, "KB/s", KIB as f64)
        );
        assert_eq!(
            stepped_throughput_ceiling(500 * KIB + 1),
            (1.0, "MB/s", MIB as f64)
        );
        assert_eq!(
            stepped_throughput_ceiling(15 * MIB),
            (20.0, "MB/s", MIB as f64)
        );
        assert_eq!(
            stepped_throughput_ceiling(150 * MIB),
            (200.0, "MB/s", MIB as f64)
        );
        assert_eq!(
            stepped_throughput_ceiling(600 * MIB),
            (1.0, "GB/s", GIB as f64)
        );
        assert_eq!(
            stepped_throughput_ceiling(3 * GIB),
            (5.0, "GB/s", GIB as f64)
        );
        assert_eq!(
            stepped_throughput_ceiling(50 * GIB),
            (50.0, "GB/s", GIB as f64)
        );

        let (y_max, unit, div) = stepped_throughput_ceiling(50 * GIB + 1);
        assert_eq!((y_max, unit, div), (100.0, "GB/s", GIB as f64));
        assert!(y_max * div >= (50 * GIB + 1) as f64);

        let (y_max_hi, unit_hi, div_hi) = stepped_throughput_ceiling(120 * GIB);
        assert_eq!((y_max_hi, unit_hi, div_hi), (200.0, "GB/s", GIB as f64));
    }

    #[test]
    fn stepped_network_ceiling_snaps_bytes_to_clean_bit_steps() {
        assert_eq!(stepped_network_ceiling(0), (100.0, "Kbps", 125.0));
        assert_eq!(stepped_network_ceiling(12_500), (100.0, "Kbps", 125.0));
        assert_eq!(stepped_network_ceiling(12_501), (250.0, "Kbps", 125.0));
        assert_eq!(stepped_network_ceiling(40_000), (500.0, "Kbps", 125.0));
        assert_eq!(stepped_network_ceiling(125_000), (1.0, "Mbps", 125_000.0));
        assert_eq!(stepped_network_ceiling(200_000), (2.0, "Mbps", 125_000.0));
        assert_eq!(
            stepped_network_ceiling(2_000_000),
            (25.0, "Mbps", 125_000.0)
        );
        assert_eq!(
            stepped_network_ceiling(200_000_000),
            (2.5, "Gbps", 125_000_000.0)
        );
        assert_eq!(
            stepped_network_ceiling(4_000_000_000),
            (40.0, "Gbps", 125_000_000.0)
        );
        assert_eq!(
            stepped_network_ceiling(12_500_000_000),
            (100.0, "Gbps", 125_000_000.0)
        );

        let (y_hi, unit_hi, div_hi) = stepped_network_ceiling(15_000_000_000);
        assert_eq!((y_hi, unit_hi, div_hi), (200.0, "Gbps", 125_000_000.0));
        assert!(15_000_000_000.0 / div_hi <= y_hi);

        let (y_max, unit_max, div_max) = stepped_network_ceiling(u64::MAX);
        assert_eq!(unit_max, "Gbps");
        assert!(y_max.is_finite() && y_max * div_max >= u64::MAX as f64);
    }

    #[test]
    fn core_grid_layout_scoring_hysteresis_and_header_degradation() {
        let empty = compute_core_grid_layout(0, 800.0, 500.0, None);
        assert_eq!(empty.columns, 0);
        assert_eq!(empty.rows, 0);

        let l16 = compute_core_grid_layout(16, 1000.0, 600.0, None);
        assert_eq!(l16.columns, 4);
        assert_eq!(l16.rows, 4);
        assert_eq!(l16.header_mode, CoreCardHeaderMode::Full);

        let l24 = compute_core_grid_layout(24, 1000.0, 600.0, None);
        assert_eq!(l24.columns, 6);
        assert_eq!(l24.rows, 4);
        assert_eq!(l24.header_mode, CoreCardHeaderMode::Full);

        let l128 = compute_core_grid_layout(128, 1200.0, 500.0, None);
        assert!(l128.columns <= 16 && l128.columns >= 8);
        assert_eq!(l128.rows, 128usize.div_ceil(l128.columns));

        let tall = compute_core_grid_layout(64, 1600.0, 460.0, None);
        let medium = compute_core_grid_layout(64, 1600.0, 260.0, Some(tall.columns));
        let tight = compute_core_grid_layout(64, 1600.0, 180.0, Some(medium.columns));
        assert_eq!(tall.columns, 16);
        assert_eq!(tall.header_mode, CoreCardHeaderMode::Full);
        assert_eq!(medium.header_mode, CoreCardHeaderMode::Short);
        assert_eq!(tight.header_mode, CoreCardHeaderMode::Hidden);
        assert!(tall.card_height_px > medium.card_height_px);
        assert!(medium.card_height_px > tight.card_height_px);

        let mut transition_w = None;
        let mut prev_cold = compute_core_grid_layout(8, 300.0, 500.0, None).columns;
        for w in (302..=1200).step_by(2) {
            let cur = compute_core_grid_layout(8, w as f32, 500.0, None).columns;
            if cur != prev_cold {
                transition_w = Some((w as f32, prev_cold, cur));
                break;
            }
            prev_cold = cur;
        }
        let (cross_w, left_cols, right_cols) =
            transition_w.expect("8 cores should have a column crossover between 300px and 1200px");

        let held_right = compute_core_grid_layout(8, cross_w, 500.0, Some(left_cols));
        assert_eq!(held_right.columns, left_cols);

        let held_left = compute_core_grid_layout(8, cross_w - 2.0, 500.0, Some(right_cols));
        assert_eq!(held_left.columns, right_cols);
    }

    #[test]
    fn select_active_gpu_engines_filters_sorts_and_falls_back() {
        assert!(select_active_gpu_engines(&[], 4).is_empty());
        assert!(select_active_gpu_engines(&[(0, 500)], 0).is_empty());

        let active =
            select_active_gpu_engines(&[(0, 5), (1, 120), (2, 10), (3, 120), (4, 850), (5, 9)], 3);
        assert_eq!(active, vec![4, 1, 3]);

        let boundary = select_active_gpu_engines(&[(0, 9), (2, 10)], 4);
        assert_eq!(boundary, vec![2]);

        let all_zero = select_active_gpu_engines(&[(2, 0), (0, 0), (1, 0)], 4);
        assert_eq!(all_zero, vec![0]);

        let sub_threshold = select_active_gpu_engines(&[(0, 2), (1, 8), (2, 8)], 4);
        assert_eq!(sub_threshold, vec![1]);
    }

    #[test]
    fn csv_helpers_prevent_formula_injection_and_follow_rfc4180() {
        assert_eq!(format_csv_cell("loom.exe"), "loom.exe");
        assert_eq!(format_csv_cell("1234"), "1234");
        assert_eq!(format_csv_cell("a=b"), "a=b");

        assert_eq!(format_csv_cell("=1+1"), "'=1+1");
        assert_eq!(format_csv_cell("+cmd|' /C calc'!A0"), "'+cmd|' /C calc'!A0");
        assert_eq!(format_csv_cell("-2+3"), "'-2+3");
        assert_eq!(format_csv_cell("@SUM(A1:A2)"), "'@SUM(A1:A2)");
        assert_eq!(format_csv_cell("\t=1+1"), "'\t=1+1");
        assert_eq!(format_csv_cell("\rfoo"), "\"'\rfoo\"");

        assert_eq!(
            format_csv_cell("   =HYPERLINK(\"http://x\",\"y\")"),
            "\"'   =HYPERLINK(\"\"http://x\"\",\"\"y\"\")\""
        );
        assert_eq!(format_csv_cell("  +1"), "'  +1");
        assert_eq!(format_csv_cell(" \n -bad"), "\"' \n -bad\"");

        assert_eq!(format_csv_cell("a, b"), "\"a, b\"");
        assert_eq!(format_csv_cell("say \"hi\""), "\"say \"\"hi\"\"\"");
        assert_eq!(format_csv_cell("line1\nline2"), "\"line1\nline2\"");

        let doc = build_csv_document(
            &["Name", "PID", "Cmd"],
            &[
                vec!["app.exe".into(), "42".into(), "=cmd, \"arg\"".into()],
                vec!["safe.exe".into(), "99".into(), "normal".into()],
            ],
        );
        assert!(doc.starts_with(CSV_UTF8_BOM));
        assert_eq!(
            doc,
            "\u{FEFF}Name,PID,Cmd\r\napp.exe,42,\"'=cmd, \"\"arg\"\"\"\r\nsafe.exe,99,normal\r\n"
        );
    }

    #[test]
    fn merge_stable_order_preserves_open_order_appends_new_and_omits_removed() {
        let mut pinned: Vec<&str> = Vec::new();

        let first = merge_stable_order(&mut pinned, &["A", "B", "C"]);
        assert_eq!(first, vec![0, 1, 2]);
        assert_eq!(pinned, vec!["A", "B", "C"]);

        let second = merge_stable_order(&mut pinned, &["C", "A"]);
        assert_eq!(second, vec![1, 0]);

        let third = merge_stable_order(&mut pinned, &["D", "C", "A"]);
        assert_eq!(third, vec![2, 1, 0]);
        assert_eq!(pinned, vec!["A", "B", "C", "D"]);

        let fourth = merge_stable_order(&mut pinned, &["B", "D", "C", "A"]);
        assert_eq!(fourth, vec![3, 0, 2, 1]);
    }
}
