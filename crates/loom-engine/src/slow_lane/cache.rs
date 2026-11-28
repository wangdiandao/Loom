//! Two-level deduplicated cold metadata cache (`ProcessKey -> CachedProcessMeta` and
//! `normalized_path -> Arc<ExeStaticAsset>`).

use std::collections::{HashMap, HashSet};
use std::hash::BuildHasher;
use std::sync::Arc;

use loom_types::{ExeStaticAsset, ProcessColdMeta, ProcessKey, UacVirtualization, WebView2Role};

use crate::tree::FastBuildHasher;
use crate::webview2::{classify_webview2, is_webview2_image};

/// Maximum number of path-deduplicated static assets retained in Level-2 cache.
const MAX_ASSET_CACHE_ENTRIES: usize = 4_096;

/// Normalizes an executable path into a deduplication key for the Level-2 asset cache.
pub fn normalize_exe_path_key(path: &str) -> Arc<str> {
    let trimmed = path.trim();
    let stripped = trimmed.strip_prefix(r"\\?\").unwrap_or(trimmed);
    let bytes = stripped.as_bytes();
    let is_windows_style = (bytes.len() >= 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':')
        || stripped.starts_with(r"\\");

    if is_windows_style {
        let mut normalized = String::with_capacity(stripped.len());
        for ch in stripped.chars() {
            if ch == '/' {
                normalized.push('\\');
            } else {
                normalized.push(ch.to_ascii_lowercase());
            }
        }
        Arc::<str>::from(normalized)
    } else {
        Arc::<str>::from(stripped)
    }
}

/// Cached Level-1 per-process cold metadata paired with precomputed derived fields.
///
/// Precomputing [`Self::webview2_role`], [`Self::webview2_host_exe`], and
/// [`Self::user_name_short`] on insertion eliminates repeated command-line scanning
/// and domain-prefix parsing during fast-lane projection ticks.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CachedProcessMeta {
    /// Underlying per-process cold metadata resolved on the slow lane.
    pub meta: Arc<ProcessColdMeta>,
    /// Precomputed WebView2 subprocess role when this process is `msedgewebview2.exe`.
    pub webview2_role: Option<WebView2Role>,
    /// Precomputed WebView2 host executable filename from `--webview-exe-name=`, if present.
    pub webview2_host_exe: Option<Arc<str>>,
    /// Precomputed short user account name with any leading `DOMAIN\` or `HOST\` prefix stripped.
    pub user_name_short: Option<Arc<str>>,
}

impl CachedProcessMeta {
    fn from_meta(meta: Arc<ProcessColdMeta>) -> Self {
        let (webview2_role, webview2_host_exe) = precompute_webview2_fields(&meta);
        let user_name_short = meta.user_name.as_ref().and_then(strip_user_domain_prefix);
        Self {
            meta,
            webview2_role,
            webview2_host_exe,
            user_name_short,
        }
    }
}

/// Internal Level-1 slot pairing public [`CachedProcessMeta`] with its precomputed
/// normalized executable path key so Level-2 asset backfills perform zero allocations.
#[derive(Debug, Clone)]
struct CachedProcessSlot {
    entry: CachedProcessMeta,
    exe_path_key: Option<Arc<str>>,
}

/// Extracts the executable filename from a path or command-line `argv[0]` token.
#[inline]
fn extract_exe_basename(raw: &str) -> &str {
    raw.rsplit(['\\', '/']).next().unwrap_or(raw).trim()
}

/// Extracts the leading executable token (`argv[0]`) from a command line, supporting quoted paths.
fn extract_cmdline_argv0(cmd: &str) -> &str {
    let trimmed = cmd.trim_start();
    if let Some(after_quote) = trimmed.strip_prefix('"') {
        let end = after_quote.find('"').unwrap_or(after_quote.len());
        &after_quote[..end]
    } else {
        let end = trimmed
            .find(|c: char| c.is_ascii_whitespace())
            .unwrap_or(trimmed.len());
        &trimmed[..end]
    }
}

fn precompute_webview2_fields(meta: &ProcessColdMeta) -> (Option<WebView2Role>, Option<Arc<str>>) {
    let is_wv2 =
        meta.exe_path
            .as_deref()
            .is_some_and(|p| is_webview2_image(extract_exe_basename(p)))
            || meta.command_line.as_deref().is_some_and(|cmd| {
                is_webview2_image(extract_exe_basename(extract_cmdline_argv0(cmd)))
            });
    if !is_wv2 {
        return (None, None);
    }
    match classify_webview2("msedgewebview2.exe", meta.command_line.as_deref()) {
        Some(parsed) => (
            Some(parsed.role),
            parsed.host_exe_name.map(Arc::<str>::from),
        ),
        None => (None, None),
    }
}

fn strip_user_domain_prefix(raw: &Arc<str>) -> Option<Arc<str>> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }
    let short = match trimmed.rsplit_once('\\') {
        Some((_, account)) if !account.trim().is_empty() => account.trim(),
        _ => trimmed,
    };
    if short.len() == raw.len() {
        Some(Arc::clone(raw))
    } else {
        Some(Arc::from(short))
    }
}

/// Two-level deduplicated cold metadata cache owned exclusively by the fast-lane thread.
///
/// - **Level 1**: `ProcessKey -> CachedProcessMeta` (evicts exited processes on tick).
/// - **Level 2**: `normalized_path -> Arc<ExeStaticAsset>` (persists across process instances
///   so dozens of instances of the same executable share a single `Arc<ExeStaticAsset>`).
#[derive(Debug, Default)]
pub struct ColdMetaCache {
    proc_meta: HashMap<ProcessKey, CachedProcessSlot, FastBuildHasher>,
    exe_assets: HashMap<Arc<str>, Arc<ExeStaticAsset>, FastBuildHasher>,
    eco_overrides: HashMap<ProcessKey, bool, FastBuildHasher>,
    uac_overrides: HashMap<ProcessKey, UacVirtualization, FastBuildHasher>,
    version: u64,
}

impl ColdMetaCache {
    /// Creates an empty two-level cold metadata cache.
    pub fn new() -> Self {
        Self::default()
    }

    /// Monotonic revision incremented whenever Level-1 process metadata or overrides change.
    #[inline]
    pub const fn version(&self) -> u64 {
        self.version
    }

    #[inline]
    fn bump_version(&mut self) {
        self.version = self.version.wrapping_add(1);
    }

    /// Looks up cached per-process cold metadata for `key`.
    #[inline]
    pub fn get_process(&self, key: ProcessKey) -> Option<&Arc<ProcessColdMeta>> {
        if self.proc_meta.is_empty() {
            return None;
        }
        self.proc_meta.get(&key).map(|slot| &slot.entry.meta)
    }

    /// Looks up the full Level-1 cache entry (including precomputed derived fields) for `key`.
    #[inline]
    pub fn get_entry(&self, key: ProcessKey) -> Option<&CachedProcessMeta> {
        if self.proc_meta.is_empty() {
            return None;
        }
        self.proc_meta.get(&key).map(|slot| &slot.entry)
    }

    /// Returns the precomputed [`WebView2Role`] for `key`, if classified on insertion.
    #[inline]
    pub fn webview2_role(&self, key: ProcessKey) -> Option<WebView2Role> {
        self.get_entry(key).and_then(|entry| entry.webview2_role)
    }

    /// Returns the precomputed WebView2 host executable name for `key`, if extracted on insertion.
    #[inline]
    pub fn webview2_host_exe(&self, key: ProcessKey) -> Option<&str> {
        self.get_entry(key)
            .and_then(|entry| entry.webview2_host_exe.as_deref())
    }

    /// Returns the precomputed short user name (with `DOMAIN\` / `HOST\` stripped) for `key`.
    #[inline]
    pub fn user_name_short(&self, key: ProcessKey) -> Option<&str> {
        self.get_entry(key)
            .and_then(|entry| entry.user_name_short.as_deref())
    }

    /// Returns whether Efficiency Mode (`EcoQoS`) is known to be enabled for `key`.
    #[inline]
    pub fn efficiency_mode(&self, key: ProcessKey) -> bool {
        if let Some(slot) = self.proc_meta.get(&key) {
            return slot.entry.meta.efficiency_mode;
        }
        self.eco_overrides.get(&key).copied().unwrap_or(false)
    }

    /// Returns whether `key` is known to be running with an elevated token.
    #[inline]
    pub fn is_elevated(&self, key: ProcessKey) -> bool {
        self.proc_meta
            .get(&key)
            .is_some_and(|slot| slot.entry.meta.is_elevated)
    }

    /// Immediately updates the cached Efficiency Mode (`EcoQoS`) flag for `key`.
    ///
    /// If `key` is already in the Level-1 cache, its `ProcessColdMeta` is updated
    /// immediately; otherwise, the state is staged in `eco_overrides` until the slow-lane
    /// probe populates the entry (preventing stale in-flight probes from clobbering the toggle).
    pub fn set_efficiency_mode(&mut self, key: ProcessKey, enabled: bool) {
        if let Some(slot) = self.proc_meta.get_mut(&key) {
            if slot.entry.meta.efficiency_mode != enabled {
                let mut cloned = (*slot.entry.meta).clone();
                cloned.efficiency_mode = enabled;
                slot.entry.meta = Arc::new(cloned);
                self.bump_version();
            }
            self.eco_overrides.remove(&key);
        } else if self.eco_overrides.insert(key, enabled) != Some(enabled) {
            self.bump_version();
        }
    }

    /// Immediately updates the cached UAC virtualization state for `key`.
    ///
    /// If `key` is already in Level 1, its `ProcessColdMeta` is updated immediately;
    /// otherwise, the override is staged until the slow-lane probe populates the entry.
    pub fn set_uac_virtualization(&mut self, key: ProcessKey, enabled: bool) {
        let next = if enabled {
            UacVirtualization::Enabled
        } else {
            UacVirtualization::Disabled
        };
        if let Some(slot) = self.proc_meta.get_mut(&key) {
            if slot.entry.meta.uac_virtualization != next {
                let mut cloned = (*slot.entry.meta).clone();
                cloned.uac_virtualization = next;
                slot.entry.meta = Arc::new(cloned);
                self.bump_version();
            }
            self.uac_overrides.remove(&key);
        } else if self.uac_overrides.insert(key, next) != Some(next) {
            self.bump_version();
        }
    }

    /// Looks up a cached path-deduplicated executable asset by normalized key.
    #[inline]
    pub fn get_asset(&self, cache_key: &str) -> Option<&Arc<ExeStaticAsset>> {
        self.exe_assets.get(cache_key)
    }

    /// Returns the number of cached Level-1 per-process entries.
    #[inline]
    pub fn proc_cache_len(&self) -> usize {
        self.proc_meta.len()
    }

    /// Returns the number of cached Level-2 executable asset entries.
    #[inline]
    pub fn asset_cache_len(&self) -> usize {
        self.exe_assets.len()
    }

    /// Inserts resolved [`ProcessColdMeta`] into Level 1 and reconciles Level 2.
    ///
    /// Precomputes [`CachedProcessMeta::webview2_role`], [`CachedProcessMeta::webview2_host_exe`],
    /// and [`CachedProcessMeta::user_name_short`] once at insertion time.
    ///
    /// Returns `Some((raw_exe_path, normalized_cache_key))` when the executable's
    /// static asset is not yet present in Level 2 and should be probed.
    pub fn insert_process(&mut self, mut meta: ProcessColdMeta) -> Option<(Arc<str>, Arc<str>)> {
        let mut needs_asset_probe = None;
        let exe_path_key = meta.exe_path.as_deref().map(normalize_exe_path_key);

        if let Some(asset) = meta.asset.take() {
            let key = normalize_exe_path_key(&asset.normalized_path);
            let shared = self.insert_asset_arc(key.clone(), asset);
            if let Some(ref exe_key) = exe_path_key
                && *exe_key != key
            {
                self.insert_asset_arc(Arc::clone(exe_key), Arc::clone(&shared));
            }
            meta.asset = Some(shared);
        } else if let (Some(exe_path), Some(cache_key)) =
            (meta.exe_path.as_ref(), exe_path_key.as_ref())
        {
            if let Some(shared) = self.exe_assets.get(cache_key) {
                meta.asset = Some(Arc::clone(shared));
            } else {
                needs_asset_probe = Some((Arc::clone(exe_path), Arc::clone(cache_key)));
            }
        }

        let proc_key = meta.key;
        if let Some(overridden_eco) = self.eco_overrides.remove(&proc_key) {
            meta.efficiency_mode = overridden_eco;
        }
        if let Some(overridden_uac) = self.uac_overrides.remove(&proc_key) {
            meta.uac_virtualization = overridden_uac;
        }
        let entry = CachedProcessMeta::from_meta(Arc::new(meta));
        self.proc_meta.insert(
            proc_key,
            CachedProcessSlot {
                entry,
                exe_path_key,
            },
        );
        self.bump_version();
        needs_asset_probe
    }

    /// Inserts a resolved [`ExeStaticAsset`] into Level 2 and backfills any live
    /// Level-1 [`ProcessColdMeta`] entries referencing the same normalized path.
    ///
    /// Returns `true` if any Level-1 process entry was updated.
    pub fn insert_asset(&mut self, cache_key: Arc<str>, asset: ExeStaticAsset) -> bool {
        let asset_arc = Arc::new(asset);
        let alt_key = normalize_exe_path_key(&asset_arc.normalized_path);
        let shared = self.insert_asset_arc(Arc::clone(&cache_key), asset_arc);
        if alt_key != cache_key {
            self.insert_asset_arc(alt_key, Arc::clone(&shared));
        }

        let mut updated_any = false;
        for slot in self.proc_meta.values_mut() {
            let matches_path = slot
                .exe_path_key
                .as_deref()
                .is_some_and(|k| k == cache_key.as_ref());
            if matches_path && slot.entry.meta.asset.is_none() {
                let mut cloned = (*slot.entry.meta).clone();
                cloned.asset = Some(Arc::clone(&shared));
                slot.entry.meta = Arc::new(cloned);
                updated_any = true;
            }
        }
        if updated_any {
            self.bump_version();
        }
        updated_any
    }

    /// Evicts exited processes from Level 1 while preserving Level-2 executable assets.
    pub fn retain_live_processes(&mut self, live: &HashSet<ProcessKey>) {
        self.retain_live_processes_with(live);
    }

    /// Evicts exited processes from Level 1 using any caller-chosen `HashSet` hasher.
    pub(crate) fn retain_live_processes_with<S: BuildHasher>(
        &mut self,
        live: &HashSet<ProcessKey, S>,
    ) {
        let prev_proc_len = self.proc_meta.len();
        let prev_eco_len = self.eco_overrides.len();
        let prev_uac_len = self.uac_overrides.len();
        self.proc_meta.retain(|k, _| live.contains(k));
        self.eco_overrides.retain(|k, _| live.contains(k));
        self.uac_overrides.retain(|k, _| live.contains(k));
        if self.proc_meta.len() != prev_proc_len
            || self.eco_overrides.len() != prev_eco_len
            || self.uac_overrides.len() != prev_uac_len
        {
            self.bump_version();
        }
    }

    fn insert_asset_arc(
        &mut self,
        key: Arc<str>,
        asset: Arc<ExeStaticAsset>,
    ) -> Arc<ExeStaticAsset> {
        if self.exe_assets.len() >= MAX_ASSET_CACHE_ENTRIES && !self.exe_assets.contains_key(&key) {
            // Simple bounded eviction to prevent unbounded memory growth over long uptimes.
            if let Some(evict_key) = self.exe_assets.keys().next().cloned() {
                self.exe_assets.remove(&evict_key);
            }
        }
        Arc::clone(self.exe_assets.entry(key).or_insert(asset))
    }
}
