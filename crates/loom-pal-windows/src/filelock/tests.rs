use std::fs::{self, OpenOptions};
use std::io::Write;
use std::mem::{offset_of, size_of};
use std::sync::Arc;

use loom_pal_api::PalError;
use loom_types::FileLockHolder;

use super::handle_scan::{
    SystemHandleInformationExHeader, SystemHandleTableEntryInfoEx, is_blocking_pipe_access_mask,
    scan_file_locks_via_handles,
};
use super::restart_manager::{RmProcessInfo, RmUniqueProcess};
use super::{find_file_locks, merge_lock_holders, normalize_final_path, path_matches_target};

#[test]
fn abi_rm_process_info_and_extended_handle_layouts_match_windows_sdk() {
    assert_eq!(size_of::<RmUniqueProcess>(), 12);
    assert_eq!(size_of::<RmProcessInfo>(), 668);
    assert_eq!(offset_of!(RmProcessInfo, process), 0);
    assert_eq!(offset_of!(RmProcessInfo, str_app_name), 12);
    assert_eq!(offset_of!(RmProcessInfo, str_service_short_name), 524);
    assert_eq!(offset_of!(RmProcessInfo, application_type), 652);
    assert_eq!(offset_of!(RmProcessInfo, app_status), 656);
    assert_eq!(offset_of!(RmProcessInfo, ts_session_id), 660);
    assert_eq!(offset_of!(RmProcessInfo, b_restartable), 664);

    #[cfg(target_pointer_width = "64")]
    {
        assert_eq!(size_of::<SystemHandleInformationExHeader>(), 16);
        assert_eq!(size_of::<SystemHandleTableEntryInfoEx>(), 40);
        assert_eq!(
            offset_of!(SystemHandleTableEntryInfoEx, unique_process_id),
            8
        );
        assert_eq!(offset_of!(SystemHandleTableEntryInfoEx, handle_value), 16);
        assert_eq!(offset_of!(SystemHandleTableEntryInfoEx, granted_access), 24);
        assert_eq!(
            offset_of!(SystemHandleTableEntryInfoEx, object_type_index),
            30
        );
    }
}

#[test]
fn path_normalization_matching_and_holder_merge_rules() {
    assert_eq!(
        normalize_final_path(r"\\?\C:\Temp\foo.txt"),
        r"C:\Temp\foo.txt"
    );
    assert_eq!(normalize_final_path(r"\\?\C:\Temp\"), r"C:\Temp");
    assert_eq!(normalize_final_path(r"\\?\C:\"), r"C:\");
    assert_eq!(
        normalize_final_path(r"\\?\UNC\server\share\dir\"),
        r"\\server\share\dir"
    );

    assert!(path_matches_target(
        r"C:\Temp\Foo.txt",
        r"c:\temp\foo.txt",
        false
    ));
    assert!(!path_matches_target(
        r"C:\Temp\Foo.txt.bak",
        r"C:\Temp\Foo.txt",
        false
    ));
    assert!(path_matches_target(r"C:\Temp\Dir", r"C:\Temp\Dir", true));
    assert!(path_matches_target(
        r"C:\Temp\Dir\sub\a.dat",
        r"C:\Temp\Dir",
        true
    ));
    assert!(!path_matches_target(
        r"C:\Temp\DirOther\a.dat",
        r"C:\Temp\Dir",
        true
    ));

    assert!(is_blocking_pipe_access_mask(0x0010_0000));
    assert!(!is_blocking_pipe_access_mask(0x0012_019F));
    assert!(!is_blocking_pipe_access_mask(0x0012_0089));

    let mut primary = vec![FileLockHolder {
        pid: 100,
        app_name: Arc::from("RichApp"),
        service_name: Some(Arc::from("svc")),
        app_type: Arc::from("Service"),
        restartable: true,
    }];
    merge_lock_holders(
        &mut primary,
        vec![
            FileLockHolder {
                pid: 100,
                app_name: Arc::from("raw.exe"),
                service_name: None,
                app_type: Arc::from("Process"),
                restartable: false,
            },
            FileLockHolder {
                pid: 200,
                app_name: Arc::from("cmd.exe"),
                service_name: None,
                app_type: Arc::from("Process"),
                restartable: false,
            },
        ],
    );
    assert_eq!(primary.len(), 2);
    assert_eq!(primary[0].app_name.as_ref(), "RichApp");
    assert_eq!(primary[1].pid, 200);
}

#[test]
fn find_file_locks_queries_open_file_and_handle_scanner_detects_open_file_and_directory() {
    let dir = std::env::temp_dir().join(format!("loom-filelock-test-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).expect("create test dir");

    // Empty directory returns empty result without error.
    let empty = find_file_locks(&dir).expect("empty dir scan");
    assert!(empty.is_empty());

    // Missing path returns InvalidArgument.
    let missing = dir.join("does_not_exist.bin");
    assert!(matches!(
        find_file_locks(&missing),
        Err(PalError::InvalidArgument(_))
    ));

    // Open a file handle in the current process and verify both Restart Manager and
    // SystemExtendedHandleInformation detect it cleanly.
    let file_path = dir.join("locked.dat");
    let mut file = OpenOptions::new()
        .create(true)
        .truncate(true)
        .read(true)
        .write(true)
        .open(&file_path)
        .expect("open temp file");
    file.write_all(b"loom lock test").expect("write temp file");
    file.flush().expect("flush temp file");

    let holders = find_file_locks(&file_path).expect("find_file_locks on open file");
    for holder in &holders {
        assert!(holder.pid > 0);
        assert!(!holder.app_type.is_empty());
    }

    let handle_holders =
        scan_file_locks_via_handles(&file_path, false).expect("handle scan on open file");
    let my_pid = std::process::id();
    assert!(
        handle_holders.iter().any(|h| h.pid == my_pid),
        "SystemExtendedHandleInformation scanner must detect current process holding open file"
    );

    let dir_handle_holders =
        scan_file_locks_via_handles(&dir, true).expect("handle scan on dir with open child");
    assert!(
        dir_handle_holders.iter().any(|h| h.pid == my_pid),
        "SystemExtendedHandleInformation directory scan must detect process holding child file"
    );

    // Directory scan covering the open file also succeeds cleanly.
    let dir_holders = find_file_locks(&dir).expect("find_file_locks on dir");
    assert!(dir_holders.len() >= holders.len());

    drop(file);
    let _ = fs::remove_dir_all(&dir);
}
