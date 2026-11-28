//! Formula-safe UTF-8 BOM CSV export helpers shared by Processes and Details views.

use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use gpui_kit::component::button::Button;
use gpui_kit::component::notification::Notification;
use gpui_kit::component::{Sizable as _, WindowExt as _};
use gpui_kit::{App, Entity, Window};
use loom_ui_kit::build_csv_document;

use crate::bridge::{UiCommands, UiStore};
use crate::i18n;

/// Formats a UTC timestamped CSV filename (`"<prefix>-YYYYMMDD-HHMMSS.csv"`).
pub(crate) fn format_csv_export_filename(prefix: &str, epoch_secs: u64) -> String {
    let days = (epoch_secs / 86_400) as i64;
    let rem_secs = epoch_secs % 86_400;
    let hour = rem_secs / 3_600;
    let minute = (rem_secs % 3_600) / 60;
    let second = rem_secs % 60;

    // Howard Hinnant's proleptic Gregorian `civil_from_days` algorithm.
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = y + i64::from(month <= 2);

    format!("{prefix}-{year:04}{month:02}{day:02}-{hour:02}{minute:02}{second:02}.csv")
}

/// Resolves a writable user export directory (`Downloads` -> `Desktop` -> `Documents` -> temp).
pub(crate) fn default_csv_export_dir() -> PathBuf {
    let home = std::env::var_os("USERPROFILE")
        .or_else(|| std::env::var_os("HOME"))
        .map(PathBuf::from);
    if let Some(base) = home {
        for sub in ["Downloads", "Desktop", "Documents"] {
            let candidate = base.join(sub);
            if candidate.is_dir() {
                return candidate;
            }
        }
        if base.is_dir() {
            return base;
        }
    }
    std::env::temp_dir()
}

/// Serializes `headers` and `rows` via `loom_ui_kit::build_csv_document` (UTF-8 BOM +
/// RFC 4180 + formula-injection neutralization) and writes the file to `dir.join(filename)`.
pub(crate) fn write_csv_export_file(
    dir: &Path,
    filename: &str,
    headers: &[String],
    rows: &[Vec<String>],
) -> std::io::Result<PathBuf> {
    let header_refs: Vec<&str> = headers.iter().map(String::as_str).collect();
    let content = build_csv_document(&header_refs, rows);
    let dest = dir.join(filename);
    std::fs::write(&dest, content.as_bytes())?;
    Ok(dest)
}

/// Exports visible table headers & rows to a timestamped CSV file off the UI thread and
/// posts a confirmation (or error) [`Notification`] upon completion.
pub(crate) fn spawn_csv_export(
    store: Entity<UiStore>,
    prefix: &'static str,
    headers: Vec<String>,
    rows: Vec<Vec<String>>,
    window: &mut Window,
    cx: &mut App,
) {
    let epoch_secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let filename = format_csv_export_filename(prefix, epoch_secs);

    window
        .spawn(cx, async move |cx| {
            let write_result = cx
                .background_executor()
                .spawn(async move {
                    let dir = default_csv_export_dir();
                    write_csv_export_file(&dir, &filename, &headers, &rows)
                })
                .await;

            let _ = cx.update(|window, cx| match write_result {
                Ok(saved_path) => {
                    let display_path = saved_path.display().to_string();
                    let msg =
                        rust_i18n::t!("csv_export_success_message", path = display_path.as_str())
                            .into_owned();
                    let open_label = i18n::tr("csv_export_open_location");
                    let store_open = store.clone();
                    let path_open = saved_path.clone();
                    let notice = Notification::success(msg)
                        .title(i18n::tr("csv_export_success_title"))
                        .action(move |_, _, cx| {
                            let store_click = store_open.clone();
                            let path_click = path_open.clone();
                            Button::new("csv-export-open-location")
                                .outline()
                                .xsmall()
                                .label(open_label.clone())
                                .on_click(cx.listener(move |this, _, window, cx| {
                                    UiCommands::open_file_location(
                                        &store_click,
                                        path_click.clone(),
                                        cx,
                                    );
                                    this.dismiss(window, cx);
                                }))
                        });
                    window.push_notification(notice, cx);
                }
                Err(err) => {
                    let notice = Notification::error(err.to_string())
                        .title(i18n::tr("csv_export_error_title"));
                    window.push_notification(notice, cx);
                }
            });
        })
        .detach();
}
