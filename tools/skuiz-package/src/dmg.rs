//! macOS packaging: stage the bundles into a folder and wrap it with
//! `hdiutil` into a compressed disk image.

use std::path::{Path, PathBuf};

use crate::meta::ProjectMeta;
use crate::util;

/// The INSTALL note that ships inside the disk image.
pub fn install_note(meta: &ProjectMeta, clap: bool, vst3: bool, app: bool) -> String {
    let mut s = format!(
        "{} {}\n\nTo install, copy:\n",
        meta.display_name, meta.version
    );
    if clap {
        s += &format!(
            "  {}.clap   →  ~/Library/Audio/Plug-Ins/CLAP/\n",
            meta.crate_name
        );
    }
    if vst3 {
        s += &format!(
            "  {}.vst3   →  ~/Library/Audio/Plug-Ins/VST3/\n",
            meta.crate_name
        );
    }
    if app {
        s += &format!(
            "  {}.app →  /Applications (or anywhere)\n",
            meta.display_name
        );
    }
    s += "\nThese builds are unsigned; on first launch, right-click → Open.\n";
    s
}

/// Stage `bundles` + INSTALL.txt and wrap them into
/// `<out>/<crate>-<version>-macos.dmg`.
pub fn package_dmg(meta: &ProjectMeta, bundles: &[PathBuf], out: &Path) -> Result<PathBuf, String> {
    let stage = out.join("dmg-stage");
    util::fresh_dir(&stage)?;
    for b in bundles {
        let dest = stage.join(b.file_name().ok_or("bundle has no file name")?);
        if b.is_dir() {
            std::fs::create_dir_all(&dest).map_err(|e| e.to_string())?;
            util::copy_tree(b, &dest)?;
        } else {
            util::copy(b, &dest)?;
        }
    }
    let has = |suffix: &str| {
        bundles
            .iter()
            .any(|b| b.to_string_lossy().ends_with(suffix))
    };
    let note = install_note(meta, has(".clap"), has(".vst3"), has(".app"));
    std::fs::write(stage.join("INSTALL.txt"), note).map_err(|e| e.to_string())?;

    let dmg = out.join(format!("{}-{}-macos.dmg", meta.crate_name, meta.version));
    util::run(
        "hdiutil",
        &[
            "create".into(),
            "-volname".into(),
            format!("{} {}", meta.display_name, meta.version),
            "-srcfolder".into(),
            stage.to_string_lossy().into_owned(),
            "-ov".into(),
            "-format".into(),
            "UDZO".into(),
            dmg.to_string_lossy().into_owned(),
        ],
    )?;
    util::fresh_dir(&stage)?; // clear the stage: the dmg is the artifact
    let _ = std::fs::remove_dir(&stage);
    Ok(dmg)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn meta() -> ProjectMeta {
        ProjectMeta {
            dir: PathBuf::from("x"),
            crate_name: "my-gain".into(),
            lib_name: "my_gain".into(),
            display_name: "My Gain".into(),
            version: "1.2.3".into(),
            identifier: "org.example.my-gain".into(),
            standalone_bin: None,
            target_dir: PathBuf::from("x/target"),
        }
    }

    #[test]
    fn install_note_lists_only_what_is_inside() {
        let note = install_note(&meta(), true, true, true);
        assert!(note.contains("my-gain.clap"));
        assert!(note.contains("my-gain.vst3"));
        assert!(note.contains("My Gain.app"));
        assert!(note.contains("1.2.3"));

        let note = install_note(&meta(), true, false, false);
        assert!(note.contains("my-gain.clap"));
        assert!(!note.contains("vst3"));
        assert!(!note.contains(".app"));
    }
}
