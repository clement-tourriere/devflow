//! Embedded assets: the component library's default icon set plus the few
//! extra Lucide icons the app draws itself. Only the listed icons are linked
//! in (a few kilobytes), not the complete 1 MiB catalog.
use gpui_kit::{
    AssetSource, Result, SharedString,
    assets::{Assets, icon_assets},
};
use std::borrow::Cow;

icon_assets!(
    AppIcons,
    [
        Camera,
        Database,
        FileCog,
        FileDiff,
        FilePlus,
        FolderGit2,
        FolderPlus,
        Gauge,
        GitBranch,
        KeyRound,
        Layers,
        Link,
        ListChecks,
        Package,
        Pencil,
        Rocket,
        Ruler,
        Server,
        ShieldAlert,
        ShieldCheck,
        SlidersHorizontal,
        SunMoon,
        Terminal,
        Timer,
        Trash,
        Variable,
        WandSparkles,
        Wrench,
        Zap,
    ]
);

/// The default component icons, then the app's own.
pub struct AppAssets;

impl AssetSource for AppAssets {
    fn load(&self, path: &str) -> Result<Option<Cow<'static, [u8]>>> {
        if let Some(bytes) = AppIcons.load(path)? {
            return Ok(Some(bytes));
        }
        Assets.load(path)
    }

    fn list(&self, path: &str) -> Result<Vec<SharedString>> {
        let mut paths = Assets.list(path)?;
        paths.extend(AppIcons.list(path)?);
        paths.sort();
        paths.dedup();
        Ok(paths)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui_kit::assets::IconName;

    #[test]
    fn extra_icons_resolve_and_defaults_still_do() {
        for icon in [
            IconName::Camera,
            IconName::Database,
            IconName::FileCog,
            IconName::FileDiff,
            IconName::FilePlus,
            IconName::FolderGit2,
            IconName::FolderPlus,
            IconName::Gauge,
            IconName::GitBranch,
            IconName::KeyRound,
            IconName::Layers,
            IconName::Link,
            IconName::ListChecks,
            IconName::Package,
            IconName::Pencil,
            IconName::Rocket,
            IconName::Ruler,
            IconName::Server,
            IconName::ShieldAlert,
            IconName::ShieldCheck,
            IconName::SlidersHorizontal,
            IconName::SunMoon,
            IconName::Terminal,
            IconName::Timer,
            IconName::Trash,
            IconName::Variable,
            IconName::WandSparkles,
            IconName::Wrench,
            IconName::Zap,
            // From the default bundle.
            IconName::Sun,
            IconName::Moon,
            IconName::Search,
            IconName::Plus,
        ] {
            let bytes = AppAssets
                .load(&icon.path())
                .unwrap()
                .unwrap_or_else(|| panic!("{icon:?} is not embedded"));
            assert!(bytes.starts_with(b"<svg"), "{icon:?} is not an svg");
        }
        // An unknown path is the default bundle's error, as before.
        assert!(AppAssets.load("icons/not-an-icon.svg").is_err());
        assert!(
            AppAssets.list("icons/").unwrap().len() > Assets.list("icons/").unwrap().len(),
            "the extras are listed beside the defaults"
        );
    }
}
