//! The app's asset source: gpui-kit's default icons plus the few extra Lucide
//! icons the CI states, watching, the stack, review threads, the key list and
//! folding a sidebar section need.

use std::borrow::Cow;

use gpui::{AssetSource, Result, SharedString};
use gpui_kit::assets::{Assets, icon_assets};

icon_assets!(
    ExtraIcons,
    [
        BellOff,
        CircleDashed,
        CircleMinus,
        GitBranch,
        MessageSquare,
        FileDiff,
        Keyboard,
        Circle,
        Pin,
        ListTree,
        MessageSquareDot,
        SquareTerminal,
        Monitor,
        Server,
        ChevronsDownUp
    ]
);

pub struct AppAssets;

impl AssetSource for AppAssets {
    fn load(&self, path: &str) -> Result<Option<Cow<'static, [u8]>>> {
        if let Some(bytes) = ExtraIcons.load(path)? {
            return Ok(Some(bytes));
        }
        Assets.load(path)
    }

    fn list(&self, path: &str) -> Result<Vec<SharedString>> {
        let mut paths = Assets.list(path)?;
        paths.extend(ExtraIcons.list(path)?);
        paths.sort();
        paths.dedup();
        Ok(paths)
    }
}
