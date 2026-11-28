use super::*;

#[test]
fn network_path_detection_handles_unc_extended_and_local_paths() {
    let prober = WindowsSlowProber::new();

    // Standard and extended UNC paths must always be detected as network paths.
    assert!(is_unc_path(r"\\server\share\app.exe"));
    assert!(is_unc_path(r"\\?\UNC\server\share\app.exe"));
    assert!(is_unc_path(r"\\.\UNC\fileserver\tools\tool.exe"));
    assert!(is_unc_path("//server/share/app.exe"));
    assert!(prober.is_network_path(r"\\server\share\app.exe"));
    assert!(prober.is_network_path(r"\\?\UNC\server\share\app.exe"));

    // Local system drive paths must NOT be flagged as UNC or network paths.
    assert!(!is_unc_path(r"C:\Windows\notepad.exe"));
    assert!(!is_unc_path(r"\\?\C:\Windows\notepad.exe"));
    assert!(!prober.is_network_path(r"C:\Windows\notepad.exe"));
    assert!(!prober.is_network_path(r"\\?\C:\Windows\notepad.exe"));

    // Target keys group by drive letter or UNC share.
    assert_eq!(circuit_target_key(r"C:\Windows\notepad.exe"), "C:");
    assert_eq!(circuit_target_key(r"\\?\d:\Apps\demo.exe"), "D:");
    assert_eq!(
        circuit_target_key(r"\\filesrv\share\bin\tool.exe"),
        r"\\filesrv\share"
    );
    assert_eq!(
        circuit_target_key(r"\\?\UNC\filesrv\share\bin\tool.exe"),
        r"\\filesrv\share"
    );
}

#[test]
fn premultiplied_lanczos_preserves_transparent_edge_color_without_dark_halo() {
    // 4x4 image: left half solid white (255,255,255,255), right half fully transparent black (0,0,0,0).
    let mut src = vec![0u8; 4 * 4 * 4];
    for y in 0..4 {
        for x in 0..2 {
            let idx = (y * 4 + x) * 4;
            src[idx] = 255;
            src[idx + 1] = 255;
            src[idx + 2] = 255;
            src[idx + 3] = 255;
        }
    }
    // Downsample 4x4 -> 3x3 (fractional boundary where straight-alpha leaks dark fringe).
    let dst = resample_rgba_premultiplied(&src, 4, 4, 3, 3);
    assert_eq!(dst.len(), 3 * 3 * 4);
    // Any semi-transparent or opaque pixel must retain pure white RGB (255,255,255), never darkened gray!
    for px in dst.as_chunks::<4>().0 {
        if px[3] > 0 {
            assert!(
                px[0] >= 250 && px[1] >= 250 && px[2] >= 250,
                "expected clean white edge without dark halo, got {:?}",
                px
            );
        }
    }
}
