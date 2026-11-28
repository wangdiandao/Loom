//! Bounded slow-lane bulkhead worker pool, 2-level deduplicated cold cache,
//! and latency-based [`CircuitBreaker`] protection.
//!
//! All operations that may touch disks, network shares, PE version resources,
//! shell icons, or on-demand OS managers run exclusively on at most `2`
//! dedicated slow-lane threads. The fast sampling thread only performs
//! non-blocking `try_send` submissions and non-blocking `try_iter` result drains,
//! guaranteeing that a stalled drive or 5-second slow probe never delays fast ticks.

mod cache;
mod circuit_breaker;
mod pool;

pub use cache::{CachedProcessMeta, ColdMetaCache, normalize_exe_path_key};
pub use circuit_breaker::CircuitBreaker;
pub(crate) use pool::{
    MgmtDomainRefresh, SlowLanePool, SlowMgmtOp, SlowPriorityJob, SlowProbeJob, SlowResult,
    SlowShellOp,
};

#[cfg(test)]
mod tests {
    use std::collections::HashSet;
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    use loom_types::{
        ExeStaticAsset, ProcessArch, ProcessColdMeta, ProcessKey, UacVirtualization, WebView2Role,
    };

    use super::*;

    #[test]
    fn circuit_breaker_trips_and_expires_after_cooldown() {
        let mut cb = CircuitBreaker::new(Duration::from_millis(300), Duration::from_secs(60));
        let t0 = Instant::now();

        assert!(!cb.is_open("D:", t0));
        assert!(!cb.record_probe("D:", Duration::from_millis(50), false, t0));
        assert!(!cb.is_open("D:", t0));

        assert!(cb.record_probe("D:", Duration::from_millis(350), false, t0));
        assert!(cb.is_open("D:", t0 + Duration::from_secs(10)));
        assert!(!cb.is_open("D:", t0 + Duration::from_secs(61)));
    }

    #[test]
    fn target_key_extracts_drive_and_unc_share() {
        assert_eq!(
            CircuitBreaker::target_key_for_path(r"d:\Games\App\foo.exe"),
            "D:"
        );
        assert_eq!(
            CircuitBreaker::target_key_for_path(r"\\?\C:\Windows\explorer.exe"),
            "C:"
        );
        assert_eq!(
            CircuitBreaker::target_key_for_path(r"\\FileServer\Public\tool.exe"),
            r"\\fileserver\public"
        );
    }

    #[test]
    fn cold_meta_cache_deduplicates_assets_across_process_instances() {
        let mut cache = ColdMetaCache::new();
        let p1 = ProcessKey::new(10, 100);
        let p2 = ProcessKey::new(20, 200);

        let meta1 = ProcessColdMeta {
            key: p1,
            exe_path: Some(Arc::from(r"C:\Program Files\Chrome\chrome.exe")),
            command_line: None,
            user_name: Some(Arc::from("User")),
            arch: ProcessArch::X64,
            uac_virtualization: UacVirtualization::NotAllowed,
            efficiency_mode: false,
            is_elevated: false,
            app_group_id: None,
            asset: None,
        };
        let needs = cache.insert_process(meta1);
        let (_, cache_key) = needs.expect("first process should require asset probe");

        let asset = ExeStaticAsset {
            normalized_path: Arc::from(r"C:\Program Files\Chrome\chrome.exe"),
            publisher: Some(Arc::from("Google LLC")),
            file_description: Some(Arc::from("Google Chrome")),
            icon: None,
            is_network_path: false,
        };
        assert!(cache.insert_asset(cache_key, asset));

        // Second instance with different casing shares the exact same Arc<ExeStaticAsset>
        let meta2 = ProcessColdMeta {
            key: p2,
            exe_path: Some(Arc::from(r"c:/program files/chrome/chrome.exe")),
            command_line: None,
            user_name: Some(Arc::from("User")),
            arch: ProcessArch::X64,
            uac_virtualization: UacVirtualization::NotAllowed,
            efficiency_mode: true,
            is_elevated: true,
            app_group_id: None,
            asset: None,
        };
        assert!(
            cache.insert_process(meta2).is_none(),
            "second instance should hit Level-2 asset cache immediately"
        );

        let a1 = cache.get_process(p1).unwrap().asset.as_ref().unwrap();
        let a2 = cache.get_process(p2).unwrap().asset.as_ref().unwrap();
        assert!(Arc::ptr_eq(a1, a2));
        assert!(cache.efficiency_mode(p2));
        assert!(cache.is_elevated(p2));

        // Immediate EcoQoS toggle updates cached state both for cached and pre-probe processes.
        cache.set_efficiency_mode(p2, false);
        assert!(!cache.efficiency_mode(p2));
        let p3 = ProcessKey::new(30, 300);
        cache.set_efficiency_mode(p3, true);
        assert!(cache.efficiency_mode(p3));

        // Evicting p1 removes Level-1 entry but preserves Level-2 asset
        let live: HashSet<ProcessKey> = [p2].into_iter().collect();
        cache.retain_live_processes(&live);
        assert_eq!(cache.proc_cache_len(), 1);
        assert_eq!(cache.asset_cache_len(), 1);
        assert!(!cache.efficiency_mode(p3));
    }

    #[test]
    fn cold_meta_cache_precomputes_webview2_role_host_exe_and_short_user_name() {
        let mut cache = ColdMetaCache::new();
        let k_wv1 = ProcessKey::new(100, 10);
        let k_wv2 = ProcessKey::new(101, 20);
        let k_chrome = ProcessKey::new(102, 30);
        let k_sys = ProcessKey::new(103, 40);
        let k_blank = ProcessKey::new(104, 50);

        cache.insert_process(ProcessColdMeta {
            key: k_wv1,
            exe_path: Some(Arc::from(r"C:\Program Files\WebView2\msedgewebview2.exe")),
            command_line: Some(Arc::from(
                r#"msedgewebview2.exe --type=renderer --webview-exe-name=Teams.exe"#,
            )),
            user_name: Some(Arc::from(r"CORP\Alice")),
            arch: ProcessArch::X64,
            uac_virtualization: UacVirtualization::NotAllowed,
            efficiency_mode: false,
            is_elevated: false,
            app_group_id: None,
            asset: None,
        });
        cache.insert_process(ProcessColdMeta {
            key: k_wv2,
            exe_path: None,
            command_line: Some(Arc::from(
                r#""C:\Program Files\WebView2\msedgewebview2.exe" --type=gpu-process --webview-exe-name="My Cool App.exe""#,
            )),
            user_name: Some(Arc::from(r"  WORKSTATION\  Bob  ")),
            arch: ProcessArch::X64,
            uac_virtualization: UacVirtualization::NotAllowed,
            efficiency_mode: false,
            is_elevated: false,
            app_group_id: None,
            asset: None,
        });
        cache.insert_process(ProcessColdMeta {
            key: k_chrome,
            exe_path: Some(Arc::from(r"C:\Program Files\Chrome\chrome.exe")),
            command_line: Some(Arc::from("chrome.exe --type=renderer")),
            user_name: Some(Arc::from("SYSTEM")),
            arch: ProcessArch::X64,
            uac_virtualization: UacVirtualization::NotAllowed,
            efficiency_mode: false,
            is_elevated: true,
            app_group_id: None,
            asset: None,
        });
        cache.insert_process(ProcessColdMeta {
            key: k_sys,
            exe_path: None,
            command_line: None,
            user_name: Some(Arc::from(r"NT AUTHORITY\LOCAL SERVICE")),
            arch: ProcessArch::X64,
            uac_virtualization: UacVirtualization::Unknown,
            efficiency_mode: false,
            is_elevated: true,
            app_group_id: None,
            asset: None,
        });
        cache.insert_process(ProcessColdMeta {
            key: k_blank,
            exe_path: None,
            command_line: None,
            user_name: Some(Arc::from("   ")),
            arch: ProcessArch::Unknown,
            uac_virtualization: UacVirtualization::Unknown,
            efficiency_mode: false,
            is_elevated: false,
            app_group_id: None,
            asset: None,
        });

        assert_eq!(cache.webview2_role(k_wv1), Some(WebView2Role::Renderer));
        assert_eq!(cache.webview2_host_exe(k_wv1), Some("Teams.exe"));
        assert_eq!(cache.user_name_short(k_wv1), Some("Alice"));

        assert_eq!(cache.webview2_role(k_wv2), Some(WebView2Role::Gpu));
        assert_eq!(cache.webview2_host_exe(k_wv2), Some("My Cool App.exe"));
        assert_eq!(cache.user_name_short(k_wv2), Some("Bob"));

        assert_eq!(cache.webview2_role(k_chrome), None);
        assert_eq!(cache.webview2_host_exe(k_chrome), None);
        assert_eq!(cache.user_name_short(k_chrome), Some("SYSTEM"));
        let chrome_entry = cache.get_entry(k_chrome).unwrap();
        assert!(Arc::ptr_eq(
            chrome_entry.meta.user_name.as_ref().unwrap(),
            chrome_entry.user_name_short.as_ref().unwrap()
        ));

        assert_eq!(cache.user_name_short(k_sys), Some("LOCAL SERVICE"));
        assert_eq!(cache.user_name_short(k_blank), None);

        // In-place updates (EcoQoS, UAC, asset backfill) preserve precomputed fields.
        cache.set_efficiency_mode(k_wv1, true);
        cache.set_uac_virtualization(k_wv1, true);
        assert_eq!(cache.webview2_role(k_wv1), Some(WebView2Role::Renderer));
        assert_eq!(cache.user_name_short(k_wv1), Some("Alice"));
    }
}
