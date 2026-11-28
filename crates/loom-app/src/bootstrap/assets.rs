//! Bundled GPUI [`AssetSource`] combining `gpui-kit`'s default icons with Loom's custom SVGs.

use std::borrow::Cow;

use gpui_kit::assets::{Assets, icon_assets};
use gpui_kit::{AssetSource, SharedString};

icon_assets!(
    LoomExtraIcons,
    [
        Activity,
        CircleQuestionMark,
        Download,
        FileLock,
        LayoutList,
        List,
        ListTree,
    ]
);

/// Application asset source combining `gpui-kit`'s default component bundle with
/// Loom's additional shell & view-mode icons.
pub(super) struct LoomAssets;

impl AssetSource for LoomAssets {
    fn load(&self, path: &str) -> gpui_kit::Result<Option<Cow<'static, [u8]>>> {
        if path == loom_ui_kit::ECO_LEAF_ICON_PATH {
            return Ok(Some(Cow::Borrowed(loom_ui_kit::ECO_LEAF_SVG)));
        }
        if let Ok(Some(data)) = LoomExtraIcons.load(path) {
            return Ok(Some(data));
        }
        Assets.load(path)
    }

    fn list(&self, path: &str) -> gpui_kit::Result<Vec<SharedString>> {
        let mut items = Assets.list(path)?;
        items.extend(LoomExtraIcons.list(path)?);
        if path.is_empty() || path == "icons" || path == "icons/" {
            items.push(SharedString::new_static(loom_ui_kit::ECO_LEAF_ICON_PATH));
        }
        Ok(items)
    }
}

#[cfg(test)]
mod tests {
    use gpui_kit::AssetSource as _;
    use gpui_kit::assets::IconName as AssetIconName;

    use super::LoomAssets;

    #[test]
    fn loom_assets_loads_all_extra_and_feature_icons() {
        let icons = [
            AssetIconName::Activity,
            AssetIconName::CircleQuestionMark,
            AssetIconName::Copy,
            AssetIconName::Download,
            AssetIconName::EyeOff,
            AssetIconName::FileLock,
            AssetIconName::LayoutList,
            AssetIconName::List,
            AssetIconName::ListTree,
            AssetIconName::RotateCw,
        ];

        for icon in icons {
            let path = icon.path();
            let loaded = LoomAssets
                .load(path.as_ref())
                .expect("asset load should not error");
            assert!(
                loaded.as_ref().is_some_and(|bytes| !bytes.is_empty()),
                "icon `{path}` must resolve from LoomAssets"
            );
        }

        let eco = LoomAssets
            .load(loom_ui_kit::ECO_LEAF_ICON_PATH)
            .expect("eco leaf load should not error");
        assert!(eco.as_ref().is_some_and(|bytes| !bytes.is_empty()));
    }
}
