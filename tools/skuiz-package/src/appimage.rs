//! Linux packaging: an AppImage of the standalone app. AppImages are
//! application containers by nature — plugin `.clap`/`.vst3` files on
//! Linux are bare artifacts in `bundles/`, not part of the image.

use std::path::{Path, PathBuf};

use crate::args::Config;
use crate::meta::ProjectMeta;
use crate::util;

/// A tiny valid placeholder icon (64×64 solid square), used when the
/// project passes no `--icon`. AppImage tooling wants *an* icon present.
const DEFAULT_ICON: &[u8] = include_bytes!("../assets/icon.png");

/// The desktop entry inside the AppDir.
pub fn desktop_file(meta: &ProjectMeta, bin: &str) -> String {
    format!(
        "[Desktop Entry]\n\
         Type=Application\n\
         Name={}\n\
         Exec={bin}\n\
         Icon={}\n\
         Categories=AudioVideo;Audio;\n\
         Terminal=false\n",
        meta.display_name, meta.crate_name
    )
}

/// Stage the AppDir and call `appimagetool`.
pub fn package_appimage(
    cfg: &Config,
    meta: &ProjectMeta,
    bin_path: &Path,
    bin: &str,
    out: &Path,
) -> Result<PathBuf, String> {
    let appdir = out
        .join("appimage-stage")
        .join(format!("{}.AppDir", meta.display_name));
    util::fresh_dir(&appdir)?;

    util::copy(bin_path, &appdir.join(format!("usr/bin/{bin}")))?;
    std::fs::write(
        appdir.join(format!("{}.desktop", meta.crate_name)),
        desktop_file(meta, bin),
    )
    .map_err(|e| e.to_string())?;
    let icon = appdir.join(format!("{}.png", meta.crate_name));
    match &cfg.icon {
        Some(png) => util::copy(png, &icon)?,
        None => std::fs::write(&icon, DEFAULT_ICON).map_err(|e| e.to_string())?,
    }
    // AppRun is the entry point: a relative symlink to the binary.
    std::os::unix::fs::symlink(format!("usr/bin/{bin}"), appdir.join("AppRun"))
        .map_err(|e| format!("cannot create AppRun symlink: {e}"))?;

    let tool = util::tool(
        &cfg.appimagetool,
        "appimagetool",
        "get it from https://appimage.github.io/appimagetool/",
    )?;
    let image = out.join(format!(
        "{}-{}-linux.AppImage",
        meta.crate_name, meta.version
    ));
    util::run(
        &tool.to_string_lossy(),
        &[
            appdir.to_string_lossy().into_owned(),
            image.to_string_lossy().into_owned(),
        ],
    )?;
    Ok(image)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn desktop_file_is_complete() {
        let meta = ProjectMeta {
            dir: PathBuf::from("x"),
            crate_name: "my-gain".into(),
            lib_name: "my_gain".into(),
            display_name: "My Gain".into(),
            version: "1.2.3".into(),
            identifier: "org.example.my-gain".into(),
            standalone_bin: Some("my-gain-standalone".into()),
            target_dir: PathBuf::from("x/target"),
        };
        let text = desktop_file(&meta, "my-gain-standalone");
        assert!(text.contains("Type=Application"));
        assert!(text.contains("Name=My Gain"));
        assert!(text.contains("Exec=my-gain-standalone"));
        assert!(text.contains("Icon=my-gain"));
        assert!(text.contains("Categories=AudioVideo;Audio;"));
    }

    #[test]
    fn default_icon_is_a_real_png() {
        assert_eq!(&DEFAULT_ICON[..8], b"\x89PNG\r\n\x1a\n");
    }
}
