use loom_types::CpuAffinityMask;

use super::affinity::{affinity_mask_to_group_affinities, build_affinity_mask_from_groups};
use super::process::is_protected_suspend_image;
use super::shell::{
    BROWSE_EXECUTABLE_FILTER, format_aedebug_command, percent_encode_query, split_command_and_args,
};

#[test]
fn split_command_and_args_handles_quoted_and_unquoted_inputs() {
    assert_eq!(
        split_command_and_args(r#""C:\Program Files\Demo\app.exe" --flag 1"#),
        (r"C:\Program Files\Demo\app.exe", "--flag 1")
    );
    assert_eq!(
        split_command_and_args("notepad.exe C:\\test.txt"),
        ("notepad.exe", "C:\\test.txt")
    );
    assert_eq!(split_command_and_args("  cmd.exe  "), ("cmd.exe", ""));
}

#[test]
fn percent_encode_query_encodes_spaces_and_unicode() {
    assert_eq!(percent_encode_query("svchost.exe"), "svchost.exe");
    assert_eq!(
        percent_encode_query("svchost.exe high cpu"),
        "svchost.exe+high+cpu"
    );
    assert_eq!(percent_encode_query("a&b=c"), "a%26b%3Dc");
}

#[test]
fn multi_group_affinity_helpers_map_96_and_128_cpu_topologies() {
    // 96 logical CPUs split as two 48-CPU processor groups ([48, 48]).
    let groups_96 = [48u32, 48u32];
    let mut mask = CpuAffinityMask::empty(96);
    mask.set(0, true);
    mask.set(47, true);
    mask.set(48, true); // Group 1, bit 0
    mask.set(95, true); // Group 1, bit 47

    let ga = affinity_mask_to_group_affinities(&mask, &groups_96);
    assert_eq!(ga.len(), 2);
    assert_eq!(ga[0].Group, 0);
    assert_eq!(ga[0].Mask, (1usize << 0) | (1usize << 47));
    assert_eq!(ga[1].Group, 1);
    assert_eq!(ga[1].Mask, (1usize << 0) | (1usize << 47));

    let roundtrip = build_affinity_mask_from_groups(96, &groups_96, &[0, 1], None, &ga);
    assert_eq!(roundtrip, mask);

    // Single-group process pinned to group 1 on a 128-CPU ([64, 64]) system.
    let groups_128 = [64u32, 64u32];
    let single_g1 = build_affinity_mask_from_groups(128, &groups_128, &[1], Some(0b1011), &[]);
    assert_eq!(single_g1.enabled_count(), 3);
    assert!(!single_g1.is_enabled(0));
    assert!(single_g1.is_enabled(64));
    assert!(single_g1.is_enabled(65));
    assert!(!single_g1.is_enabled(66));
    assert!(single_g1.is_enabled(67));

    // Multi-group default process with no custom CPU set masks enables all CPUs in its groups.
    let all_groups = build_affinity_mask_from_groups(128, &groups_128, &[0, 1], None, &[]);
    assert_eq!(all_groups, CpuAffinityMask::all(128));
}

#[test]
fn browse_executable_filter_includes_expected_extensions_and_double_null() {
    assert!(BROWSE_EXECUTABLE_FILTER.ends_with("\0\0"));
    assert!(BROWSE_EXECUTABLE_FILTER.contains("*.exe;*.bat;*.cmd;*.msc;*.com"));
    assert!(BROWSE_EXECUTABLE_FILTER.contains("All Files (*.*)\0*.*\0\0"));
}

#[test]
fn format_aedebug_command_substitutes_pid_and_event_placeholders() {
    assert_eq!(
        format_aedebug_command(
            r#""C:\Windows\system32\vsjitdebugger.exe" -p %ld -e %ld"#,
            4242
        ),
        r#""C:\Windows\system32\vsjitdebugger.exe" -p 4242 -e 0"#
    );
    assert_eq!(
        format_aedebug_command("windbg.exe -p %u -e %u -g", 9001),
        "windbg.exe -p 9001 -e 0 -g"
    );
    assert_eq!(
        format_aedebug_command(r"%SystemRoot%\system32\vsjitdebugger.exe -p %d", 1234),
        r"%SystemRoot%\system32\vsjitdebugger.exe -p 1234"
    );
}

#[test]
fn protected_suspend_image_filter_blocks_dwm_and_core_system_processes() {
    assert!(is_protected_suspend_image("dwm.exe"));
    assert!(is_protected_suspend_image("DWM.EXE"));
    assert!(is_protected_suspend_image("csrss.exe"));
    assert!(is_protected_suspend_image("lsass.exe"));
    assert!(!is_protected_suspend_image("notepad.exe"));
    assert!(!is_protected_suspend_image("cargo.exe"));
}
