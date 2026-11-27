//! Lightweight crash-safe local application usage history store for Windows.
//!
//! Persists cumulative application history baseline under `%LOCALAPPDATA%\Loom\app_history.dat`
//! using atomic temporary-file write -> `sync_all` -> rename with zero extra external crate
//! dependencies.

use std::fs::{self, File};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use loom_pal_api::{AppHistoryReader, PalError};
use loom_types::AppHistoryRecord;
use parking_lot::Mutex;

const FORMAT_VERSION_LINE: &str = "version=1";

static TEMP_FILE_SEQ: AtomicU64 = AtomicU64::new(0);

/// Windows persistent application history store implementing [`AppHistoryReader`].
#[derive(Debug)]
pub struct WindowsAppHistoryReader {
    store_path: PathBuf,
    io_lock: Mutex<()>,
}

impl Default for WindowsAppHistoryReader {
    fn default() -> Self {
        Self::new()
    }
}

impl WindowsAppHistoryReader {
    /// Creates a reader backed by `%LOCALAPPDATA%\Loom\app_history.dat`
    /// (falling back to `%TEMP%\Loom\app_history.dat` when `LOCALAPPDATA` is unset).
    pub fn new() -> Self {
        let base = std::env::var_os("LOCALAPPDATA")
            .map(PathBuf::from)
            .unwrap_or_else(std::env::temp_dir);
        Self::with_path(base.join("Loom").join("app_history.dat"))
    }

    /// Creates a reader backed by an explicit file path (ideal for isolated tests).
    pub fn with_path(store_path: PathBuf) -> Self {
        Self {
            store_path,
            io_lock: Mutex::new(()),
        }
    }

    /// Returns the filesystem path of the backing history store.
    #[inline]
    pub fn store_path(&self) -> &Path {
        &self.store_path
    }

    /// Atomically persists `(since_epoch_secs, items)` to disk.
    pub fn save_history(
        &self,
        since_epoch_secs: u64,
        items: &[AppHistoryRecord],
    ) -> Result<(), PalError> {
        let _guard = self.io_lock.lock();
        write_store_atomic(&self.store_path, since_epoch_secs, items)
    }
}

impl AppHistoryReader for WindowsAppHistoryReader {
    fn read_history(&self) -> Result<(Option<u64>, Vec<AppHistoryRecord>), PalError> {
        let _guard = self.io_lock.lock();
        match fs::read_to_string(&self.store_path) {
            Ok(content) => {
                if let Some((since, records)) = parse_store(&content) {
                    Ok((Some(since), records))
                } else {
                    // Corrupted or unreadable file: recover cleanly with a fresh epoch.
                    let now = current_unix_epoch_secs();
                    let _ = write_store_atomic(&self.store_path, now, &[]);
                    Ok((Some(now), Vec::new()))
                }
            }
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                let now = current_unix_epoch_secs();
                // Best-effort initial creation so `since_epoch_secs` stays stable across restarts.
                let _ = write_store_atomic(&self.store_path, now, &[]);
                Ok((Some(now), Vec::new()))
            }
            Err(err) => Err(PalError::Io(err)),
        }
    }

    fn clear_history(&self) -> Result<(), PalError> {
        let _guard = self.io_lock.lock();
        let now = current_unix_epoch_secs();
        write_store_atomic(&self.store_path, now, &[])
    }

    fn save_history(
        &self,
        since_epoch_secs: u64,
        records: &[AppHistoryRecord],
    ) -> Result<(), PalError> {
        WindowsAppHistoryReader::save_history(self, since_epoch_secs, records)
    }
}

fn current_unix_epoch_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn write_store_atomic(
    path: &Path,
    since_epoch_secs: u64,
    items: &[AppHistoryRecord],
) -> Result<(), PalError> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }

    let serialized = serialize_store(since_epoch_secs, items);
    let seq = TEMP_FILE_SEQ.fetch_add(1, Ordering::Relaxed);
    let tmp_name = format!(
        ".{}.tmp.{}.{seq}",
        path.file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("app_history.dat"),
        std::process::id(),
    );
    let tmp_path = path.with_file_name(tmp_name);

    let write_res = (|| -> std::io::Result<()> {
        let mut file = File::create(&tmp_path)?;
        file.write_all(serialized.as_bytes())?;
        file.sync_all()?;
        drop(file);
        fs::rename(&tmp_path, path)?;
        Ok(())
    })();

    if let Err(err) = write_res {
        let _ = fs::remove_file(&tmp_path);
        return Err(PalError::Io(err));
    }

    Ok(())
}

fn serialize_store(since_epoch_secs: u64, items: &[AppHistoryRecord]) -> String {
    let mut out = String::with_capacity(64 + items.len() * 64);
    out.push_str(FORMAT_VERSION_LINE);
    out.push('\n');
    out.push_str("since_epoch_secs=");
    out.push_str(&since_epoch_secs.to_string());
    out.push('\n');

    for item in items {
        out.push_str("entry=");
        out.push_str(&item.cpu_time_100ns.to_string());
        out.push('\t');
        out.push_str(&item.network_bytes.to_string());
        out.push('\t');
        out.push_str(&item.metered_network_bytes.to_string());
        out.push('\t');
        escape_field(&item.app_id, &mut out);
        out.push('\t');
        escape_field(&item.name, &mut out);
        out.push('\n');
    }

    out
}

fn parse_store(content: &str) -> Option<(u64, Vec<AppHistoryRecord>)> {
    let mut since_epoch_secs: Option<u64> = None;
    let mut items = Vec::new();
    let mut saw_version = false;

    for raw_line in content.lines() {
        let line = raw_line.trim_end_matches('\r');
        if line.is_empty() {
            continue;
        }
        if line == FORMAT_VERSION_LINE {
            saw_version = true;
            continue;
        }
        if let Some(val) = line.strip_prefix("since_epoch_secs=") {
            since_epoch_secs = val.trim().parse::<u64>().ok();
            continue;
        }
        if let Some(rest) = line.strip_prefix("entry=") {
            let mut parts = rest.splitn(5, '\t');
            let (Some(cpu_s), Some(net_s), Some(metered_s), Some(id_s), Some(name_s)) = (
                parts.next(),
                parts.next(),
                parts.next(),
                parts.next(),
                parts.next(),
            ) else {
                continue;
            };
            let (Ok(cpu_time_100ns), Ok(network_bytes), Ok(metered_network_bytes)) = (
                cpu_s.parse::<u64>(),
                net_s.parse::<u64>(),
                metered_s.parse::<u64>(),
            ) else {
                continue;
            };
            let app_id = unescape_field(id_s);
            let name = unescape_field(name_s);
            if app_id.is_empty() {
                continue;
            }
            items.push(AppHistoryRecord {
                app_id: Arc::from(app_id),
                name: Arc::from(name),
                cpu_time_100ns,
                network_bytes,
                metered_network_bytes,
            });
        }
    }

    if !saw_version {
        return None;
    }
    Some((since_epoch_secs?, items))
}

fn escape_field(input: &str, out: &mut String) {
    for ch in input.chars() {
        match ch {
            '\\' => out.push_str(r"\\"),
            '\t' => out.push_str(r"\t"),
            '\n' => out.push_str(r"\n"),
            '\r' => out.push_str(r"\r"),
            other => out.push(other),
        }
    }
}

fn unescape_field(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut chars = input.chars();
    while let Some(ch) = chars.next() {
        if ch == '\\' {
            match chars.next() {
                Some('\\') => out.push('\\'),
                Some('t') => out.push('\t'),
                Some('n') => out.push('\n'),
                Some('r') => out.push('\r'),
                Some(other) => {
                    out.push('\\');
                    out.push(other);
                }
                None => out.push('\\'),
            }
        } else {
            out.push(ch);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn app_history_store_round_trips_and_clears_atomically() {
        let dir = std::env::temp_dir().join(format!(
            "loom-app-history-test-{}-{}",
            std::process::id(),
            current_unix_epoch_secs()
        ));
        let path = dir.join("app_history.dat");
        let reader = WindowsAppHistoryReader::with_path(path.clone());

        // First read initializes epoch and empty list.
        let (since1, items1) = reader.read_history().expect("initial read_history");
        assert!(since1.is_some());
        assert!(items1.is_empty());

        // Save entries including special characters (tabs, backslashes, Unicode).
        let records = vec![
            AppHistoryRecord {
                app_id: Arc::from(r"Microsoft.Edge\App"),
                name: Arc::from("Microsoft Edge\tBrowser"),
                cpu_time_100ns: 123_456_789,
                network_bytes: 9_876_543,
                metered_network_bytes: 1_024,
            },
            AppHistoryRecord {
                app_id: Arc::from("loom.exe"),
                name: Arc::from("Loom 任务管理器"),
                cpu_time_100ns: 50_000,
                network_bytes: 0,
                metered_network_bytes: 0,
            },
        ];
        reader
            .save_history(1_700_000_000, &records)
            .expect("save_history");

        let (since2, items2) = reader.read_history().expect("read back saved history");
        assert_eq!(since2, Some(1_700_000_000));
        assert_eq!(items2, records);

        // Clear resets since_epoch_secs and removes all items.
        reader.clear_history().expect("clear_history");
        let (since3, items3) = reader.read_history().expect("read after clear");
        assert!(since3.is_some_and(|s| s >= 1_700_000_000));
        assert!(items3.is_empty());

        let _ = fs::remove_dir_all(&dir);
    }
}
