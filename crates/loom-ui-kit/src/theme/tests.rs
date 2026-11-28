use gpui_kit::component::{Theme, ThemeColor, ThemeMode};

use crate::format::HeatmapLevel;

use super::*;

fn light_theme() -> Theme {
    let mut theme = Theme::from(ThemeColor::light().as_ref());
    theme.mode = ThemeMode::Light;
    theme
}

fn dark_theme() -> Theme {
    let mut theme = Theme::from(ThemeColor::dark().as_ref());
    theme.mode = ThemeMode::Dark;
    theme
}

#[test]
fn semantic_colors_adapt_to_light_and_dark_themes() {
    let light = LoomSemanticColors::from_theme(&light_theme());
    let dark = LoomSemanticColors::from_theme(&dark_theme());

    assert!(!light.is_dark);
    assert!(dark.is_dark);

    for palette in [&light, &dark] {
        assert_eq!(palette.heatmap_bg(HeatmapLevel::None).a, 0.0);
        let low = palette.heatmap_bg(HeatmapLevel::Low).a;
        let med = palette.heatmap_bg(HeatmapLevel::Medium).a;
        let high = palette.heatmap_bg(HeatmapLevel::High).a;
        let crit = palette.heatmap_bg(HeatmapLevel::Critical).a;
        assert!(0.0 < low && low < med && med < high && high < crit);
    }

    assert_ne!(light.status_eco_fg, dark.status_eco_fg);
    assert_ne!(light.status_hung_fg, dark.status_hung_fg);
    assert_ne!(light.chart_cpu, dark.chart_cpu);
    assert_ne!(light.chart_cpu, light.chart_cpu_kernel);
}

#[test]
fn wcag_contrast_math_and_solver_targets() {
    assert!((srgb_relative_luminance(0.0, 0.0, 0.0) - 0.0).abs() < 1e-6);
    assert!((srgb_relative_luminance(1.0, 1.0, 1.0) - 1.0).abs() < 1e-6);
    assert!((wcag_contrast_ratio((0.0, 0.0, 0.0), (1.0, 1.0, 1.0)) - 21.0).abs() < 1e-4);
    assert!((wcag_contrast_ratio((0.5, 0.5, 0.5), (0.5, 0.5, 0.5)) - 1.0).abs() < 1e-6);

    let dark_bg = gpui_kit::hsla(220.0 / 360.0, 0.14, 0.10, 1.0);
    let light_bg = gpui_kit::hsla(0.0, 0.0, 0.98, 1.0);
    let low_contrast_blue = gpui_kit::hsla(210.0 / 360.0, 0.70, 0.45, 1.0);
    let low_contrast_yellow = gpui_kit::hsla(48.0 / 360.0, 0.90, 0.72, 1.0);

    for bg in [dark_bg, light_bg] {
        for fg in [low_contrast_blue, low_contrast_yellow] {
            let aa = ensure_wcag_contrast(fg, bg, WCAG_AA_TEXT_CONTRAST);
            assert!(
                wcag_contrast_ratio(hsla_to_srgb(aa), hsla_to_srgb(bg))
                    >= WCAG_AA_TEXT_CONTRAST - 1e-3,
                "AA (4.5:1) failed for fg={fg:?} on bg={bg:?} -> {aa:?}"
            );
            assert!((aa.h - fg.h).abs() < 1e-6);
            assert!((aa.s - fg.s).abs() < 1e-6);

            let aaa = ensure_wcag_contrast(fg, bg, WCAG_AAA_TEXT_CONTRAST);
            assert!(
                wcag_contrast_ratio(hsla_to_srgb(aaa), hsla_to_srgb(bg))
                    >= WCAG_AAA_TEXT_CONTRAST - 1e-3,
                "AAA (7.0:1) failed for fg={fg:?} on bg={bg:?} -> {aaa:?}"
            );
            assert!((aaa.h - fg.h).abs() < 1e-6);
            assert!((aaa.s - fg.s).abs() < 1e-6);
        }
    }
}

#[test]
fn semantic_palette_meets_wcag_contrast_in_light_and_dark_themes() {
    for palette in [
        LoomSemanticColors::from_theme(&light_theme()),
        LoomSemanticColors::from_theme(&dark_theme()),
    ] {
        let bg = hsla_to_srgb(palette.card_bg);

        let text_tokens = [
            ("text_primary", palette.text_primary),
            ("text_muted", palette.text_muted),
            ("status_eco_fg", palette.status_eco_fg),
            ("status_suspended_fg", palette.status_suspended_fg),
            ("status_hung_fg", palette.status_hung_fg),
            ("leak_warning", palette.leak_warning),
            ("sort_paused", palette.sort_paused),
        ];
        for (name, color) in text_tokens {
            let ratio = wcag_contrast_ratio(hsla_to_srgb(color), bg);
            assert!(
                ratio >= WCAG_AA_TEXT_CONTRAST - 1e-3,
                "{name} contrast {ratio:.2}:1 < 4.5:1 (is_dark={})",
                palette.is_dark
            );
        }

        let ui_tokens = [
            ("chart_cpu", palette.chart_cpu),
            ("chart_cpu_kernel", palette.chart_cpu_kernel),
            ("cpu_core_p", palette.cpu_core_p),
            ("cpu_core_e", palette.cpu_core_e),
            ("cpu_core_lp_e", palette.cpu_core_lp_e),
            ("chart_memory", palette.chart_memory),
            ("chart_disk", palette.chart_disk),
            ("chart_disk_write", palette.chart_disk_write),
            ("chart_network", palette.chart_network),
            ("chart_network_send", palette.chart_network_send),
            ("chart_gpu", palette.chart_gpu),
            ("chart_gpu_shared", palette.chart_gpu_shared),
        ];
        for (name, color) in ui_tokens {
            let ratio = wcag_contrast_ratio(hsla_to_srgb(color), bg);
            assert!(
                ratio >= WCAG_AA_UI_CONTRAST - 1e-3,
                "{name} contrast {ratio:.2}:1 < 3.0:1 (is_dark={})",
                palette.is_dark
            );
        }

        assert_ne!(palette.cpu_core_p, palette.cpu_core_e);
        assert_ne!(palette.cpu_core_e, palette.cpu_core_lp_e);
        assert_ne!(palette.cpu_core_p, palette.cpu_core_lp_e);
    }
}

#[test]
fn cached_semantic_colors_hits_and_invalidates_on_theme_changes() {
    let mut cache = CachedSemanticColors::new();
    let light = light_theme();
    let dark = dark_theme();

    let expected_light = LoomSemanticColors::from_theme(&light);
    let expected_dark = LoomSemanticColors::from_theme(&dark);

    assert_eq!(*cache.get(&light), expected_light);
    assert_eq!(cache.get_copy(&light), expected_light);

    assert_eq!(*cache.get(&dark), expected_dark);
    assert_eq!(cache.get_copy(&dark), expected_dark);

    let mut modified = dark.clone();
    modified.primary = gpui_kit::hsla(0.5, 0.8, 0.5, 1.0);
    let updated = cache.get_copy(&modified);
    assert_eq!(updated.card_selected_border, modified.primary);
    assert_ne!(updated, expected_dark);

    let mut modified_border = modified.clone();
    modified_border.border = gpui_kit::hsla(0.25, 0.5, 0.4, 1.0);
    assert_eq!(
        cache.get_copy(&modified_border).card_border,
        modified_border.border
    );

    let mut modified_hover = modified_border.clone();
    modified_hover.list_hover = gpui_kit::hsla(0.1, 0.2, 0.3, 1.0);
    assert_eq!(
        cache.get_copy(&modified_hover).card_hover_bg,
        modified_hover.list_hover
    );

    cache.invalidate();
    assert_eq!(*cache.get(&light), expected_light);
}

#[test]
fn rgb_hsla_roundtrip_and_accent_theme_enforces_wcag_aa_across_extremes() {
    let samples = [
        DEFAULT_ACCENT_RGB,
        (0x1D, 0xC5, 0xF4),
        (0xFF, 0xFF, 0xFF),
        (0x00, 0x00, 0x00),
        (0xFF, 0xFF, 0x00),
        (0x00, 0xFF, 0x00),
        (0x87, 0x64, 0xB8),
        (0xD1, 0x34, 0x38),
        (0x0A, 0x0E, 0x2A),
    ];

    for rgb in samples
        .into_iter()
        .chain(ACCENT_COLOR_PRESETS.iter().map(|(_, c)| *c))
    {
        assert_eq!(hsla_to_rgb_u8(rgb_to_hsla(rgb)), rgb);

        for mut theme in [light_theme(), dark_theme()] {
            let is_dark = theme.is_dark();
            apply_accent_to_theme(&mut theme, rgb);

            let surface_rgb = hsla_to_srgb(theme.background);
            let primary_rgb = hsla_to_srgb(theme.primary);
            let primary_fg_rgb = hsla_to_srgb(theme.primary_foreground);
            let ring_rgb = hsla_to_srgb(theme.ring);
            let link_rgb = hsla_to_srgb(theme.link);
            let sidebar_bg = hsla_to_srgb(theme.sidebar);
            let sidebar_fg = hsla_to_srgb(theme.sidebar_accent_foreground);

            assert!(
                wcag_contrast_ratio(primary_rgb, surface_rgb) >= WCAG_AA_UI_CONTRAST - 1e-2,
                "primary vs surface < 3.0:1 for {rgb:?} (is_dark={is_dark})"
            );
            assert!(
                wcag_contrast_ratio(primary_fg_rgb, primary_rgb) >= WCAG_AA_TEXT_CONTRAST - 1e-2,
                "primary_fg vs primary < 4.5:1 for {rgb:?} (is_dark={is_dark})"
            );
            assert_eq!(theme.button_primary, theme.primary);
            assert_eq!(theme.button_primary_foreground, theme.primary_foreground);

            assert!(
                wcag_contrast_ratio(ring_rgb, surface_rgb) >= WCAG_AA_UI_CONTRAST - 1e-2,
                "ring vs surface < 3.0:1 for {rgb:?} (is_dark={is_dark})"
            );

            assert!(
                wcag_contrast_ratio(link_rgb, surface_rgb) >= WCAG_AA_TEXT_CONTRAST - 1e-2,
                "link vs surface < 4.5:1 for {rgb:?} (is_dark={is_dark})"
            );
            assert!(
                wcag_contrast_ratio(sidebar_fg, sidebar_bg) >= WCAG_AA_TEXT_CONTRAST - 1e-2,
                "sidebar_accent_foreground vs sidebar < 4.5:1 for {rgb:?} (is_dark={is_dark})"
            );

            let mut cache = CachedSemanticColors::new();
            let semantic = cache.get_copy(&theme);
            assert_eq!(semantic.card_selected_border, theme.primary);
            assert_eq!(semantic.card_selected_bg, theme.list_active);
        }
    }
}
