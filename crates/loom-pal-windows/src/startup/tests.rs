use std::sync::Arc;

use super::registry::{
    decode_startup_approved_bytes, encode_startup_approved_bytes, extract_executable_path_with_env,
    pick_win32_run_display_name,
};
use super::uwp::{ParsedStartupTask, parse_appx_manifest, score_uwp_logo_qualifiers};
use super::{FileStartupMeta, StartupIdTarget, parse_startup_id};

#[test]
fn startup_approved_binary_decode_and_encode_round_trip() {
    assert_eq!(decode_startup_approved_bytes(&[]), None);
    assert_eq!(
        decode_startup_approved_bytes(&[0x02, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]),
        Some(true)
    );
    assert_eq!(
        decode_startup_approved_bytes(&[0x06, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]),
        Some(true)
    );
    assert_eq!(
        decode_startup_approved_bytes(&[0x03, 0, 0, 0, 1, 2, 3, 4, 5, 6, 7, 8]),
        Some(false)
    );
    assert_eq!(
        decode_startup_approved_bytes(&[0x07, 0, 0, 0, 1, 2, 3, 4, 5, 6, 7, 8]),
        Some(false)
    );

    let enabled_bytes = encode_startup_approved_bytes(true, 0x1122_3344_5566_7788);
    assert_eq!(enabled_bytes[0], 0x02);
    assert_eq!(&enabled_bytes[4..12], &[0u8; 8]);
    assert_eq!(decode_startup_approved_bytes(&enabled_bytes), Some(true));

    let ft = 133_700_000_000_000_000u64;
    let disabled_bytes = encode_startup_approved_bytes(false, ft);
    assert_eq!(disabled_bytes[0], 0x03);
    assert_eq!(
        u64::from_le_bytes(disabled_bytes[4..12].try_into().unwrap()),
        ft
    );
    assert_eq!(decode_startup_approved_bytes(&disabled_bytes), Some(false));
}

#[test]
fn extract_executable_path_handles_quotes_spaces_and_env_vars() {
    let env = |var: &str| match var {
        "ProgramFiles" => Some(r"C:\Program Files".to_owned()),
        "LOCALAPPDATA" => Some(r"C:\Users\Alice\AppData\Local".to_owned()),
        _ => None,
    };

    assert_eq!(
        extract_executable_path_with_env(
            r#""C:\Program Files\My App\launcher.exe" --silent -bg"#,
            env
        )
        .as_deref(),
        Some(r"C:\Program Files\My App\launcher.exe")
    );
    assert_eq!(
        extract_executable_path_with_env(r"C:\Program Files\Vendor Suite\agent.exe /autorun", env)
            .as_deref(),
        Some(r"C:\Program Files\Vendor Suite\agent.exe")
    );
    assert_eq!(
        extract_executable_path_with_env(r#""%ProgramFiles%\Foo\bar.exe" -m"#, env).as_deref(),
        Some(r"C:\Program Files\Foo\bar.exe")
    );
    assert_eq!(
        extract_executable_path_with_env(r"%LOCALAPPDATA%\Loom\loom.exe --tray", env).as_deref(),
        Some(r"C:\Users\Alice\AppData\Local\Loom\loom.exe")
    );
    assert_eq!(
        extract_executable_path_with_env("notepad.exe notes.txt", env).as_deref(),
        Some("notepad.exe")
    );
    assert_eq!(extract_executable_path_with_env("   ", env), None);
}

#[test]
fn parse_startup_id_validates_all_sources_and_rejects_malformed() {
    assert_eq!(
        parse_startup_id("hkcu-run:OneDrive"),
        Some(StartupIdTarget::HkcuRun("OneDrive"))
    );
    assert_eq!(
        parse_startup_id("hklm-run:SecurityHealth"),
        Some(StartupIdTarget::HklmRun("SecurityHealth"))
    );
    assert_eq!(
        parse_startup_id("hklm-run32:LegacyTool"),
        Some(StartupIdTarget::HklmRun32("LegacyTool"))
    );
    assert_eq!(
        parse_startup_id("user-folder:MyApp.lnk"),
        Some(StartupIdTarget::UserFolder("MyApp.lnk"))
    );
    assert_eq!(
        parse_startup_id("common-folder:Shared.lnk"),
        Some(StartupIdTarget::CommonFolder("Shared.lnk"))
    );
    assert_eq!(
        parse_startup_id("uwp:Microsoft.Todos_8wekyb3d8bbwe|TodoStartupTask"),
        Some(StartupIdTarget::Uwp {
            package: "Microsoft.Todos_8wekyb3d8bbwe",
            task: "TodoStartupTask",
        })
    );

    assert_eq!(parse_startup_id(""), None);
    assert_eq!(parse_startup_id("hkcu-run:"), None);
    assert_eq!(parse_startup_id("unknown:foo"), None);
    assert_eq!(parse_startup_id("uwp:missing_pipe"), None);
    assert_eq!(parse_startup_id("uwp:|task"), None);
    assert_eq!(parse_startup_id("uwp:pkg|"), None);
}

#[test]
fn parse_appx_manifest_extracts_properties_visual_elements_and_startup_tasks() {
    let xml = r#"<?xml version="1.0" encoding="utf-8"?>
<Package xmlns="http://schemas.microsoft.com/appx/manifest/foundation/windows10"
         xmlns:uap="http://schemas.microsoft.com/appx/manifest/uap/windows10"
         xmlns:desktop="http://schemas.microsoft.com/appx/manifest/desktop/windows10">
  <!-- <desktop:StartupTask TaskId="IgnoredComment" DisplayName="Nope" /> -->
  <Properties>
    <DisplayName>Watt Toolkit &amp; Tools</DisplayName>
    <PublisherDisplayName>软妹币玩家</PublisherDisplayName>
    <Logo>images\StoreLogo.png</Logo>
  </Properties>
  <Applications>
    <Application Id="App" Executable="bin\Steam++.exe">
      <uap:VisualElements DisplayName="ms-resource:AppName" Square44x44Logo="images\Square44x44Logo.png" />
      <Extensions>
        <desktop:Extension Category="windows.startupTask" Executable="bin\BootHelper.exe">
          <desktop:StartupTask TaskId="BootAutoStartTask" Enabled="true" DisplayName="Steam++ System Boot Run" />
        </desktop:Extension>
      </Extensions>
    </Application>
  </Applications>
</Package>"#;

    let parsed = parse_appx_manifest(xml);
    assert_eq!(
        parsed.package_display_name.as_deref(),
        Some("Watt Toolkit & Tools")
    );
    assert_eq!(parsed.publisher_display_name.as_deref(), Some("软妹币玩家"));
    assert_eq!(parsed.store_logo.as_deref(), Some(r"images\StoreLogo.png"));
    assert_eq!(
        parsed.app_display_name.as_deref(),
        Some("ms-resource:AppName")
    );
    assert_eq!(
        parsed.square44_logo.as_deref(),
        Some(r"images\Square44x44Logo.png")
    );
    assert_eq!(
        parsed.default_executable.as_deref(),
        Some(r"bin\Steam++.exe")
    );
    assert_eq!(
        parsed.startup_tasks,
        vec![ParsedStartupTask {
            task_id: "BootAutoStartTask".to_owned(),
            display_name: Some("Steam++ System Boot Run".to_owned()),
            executable: Some(r"bin\BootHelper.exe".to_owned()),
        }]
    );
}

#[test]
fn win32_run_display_name_prefers_file_description_then_product_then_filename() {
    let with_desc = FileStartupMeta {
        publisher: Some(Arc::from("Google LLC")),
        file_description: Some(Arc::from("Google Chrome")),
        product_name: Some(Arc::from("Google Chrome")),
        icon: None,
    };
    assert_eq!(
        pick_win32_run_display_name(
            "GoogleChromeAutoLaunch_332B101ED56858E",
            Some(r"C:\Program Files\Google\Chrome\Application\chrome.exe"),
            Some(&with_desc),
        ),
        "Google Chrome"
    );

    let only_product = FileStartupMeta {
        publisher: Some(Arc::from("Skutta Software GmbH")),
        file_description: None,
        product_name: Some(Arc::from("Wallpaper Engine")),
        icon: None,
    };
    assert_eq!(
        pick_win32_run_display_name(
            "WallpaperEngine",
            Some(r"F:\SteamLibrary\wallpaper32.exe"),
            Some(&only_product),
        ),
        "Wallpaper Engine"
    );

    let empty_meta = FileStartupMeta::default();
    assert_eq!(
        pick_win32_run_display_name("RawKeyName", Some(r"C:\Apps\widget.exe"), Some(&empty_meta),),
        "widget.exe"
    );

    assert_eq!(
        pick_win32_run_display_name("OrphanKey", None, None),
        "OrphanKey"
    );
}

#[test]
fn uwp_logo_qualifier_scoring_prefers_exact_unplated_icons_over_contrast_and_upscale() {
    let exact_16 = score_uwp_logo_qualifiers("targetsize-16_altform-unplated", 16);
    let larger_24 = score_uwp_logo_qualifiers("targetsize-24_altform-unplated", 16);
    let scale_100 = score_uwp_logo_qualifiers("scale-100", 16);
    let exact = score_uwp_logo_qualifiers("", 16);
    let contrast = score_uwp_logo_qualifiers("targetsize-16_contrast-black", 16);
    assert!(exact_16 > larger_24);
    assert!(larger_24 > scale_100);
    assert!(scale_100 > exact);
    assert!(exact > contrast);

    // At 150% DPI (target_dim = 24), exact 24 beats 16 (which would require upscaling).
    let hidpi_24 = score_uwp_logo_qualifiers("targetsize-24_altform-unplated", 24);
    let hidpi_16 = score_uwp_logo_qualifiers("targetsize-16_altform-unplated", 24);
    assert!(hidpi_24 > hidpi_16);
}
