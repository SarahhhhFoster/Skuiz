//! Windows packaging: the standalone binary as a versioned `.exe`, the
//! plugin bundles staged beside it, and — with `--installer` — an Inno
//! Setup script compiled by `iscc` into a real installer.

use std::path::{Path, PathBuf};

use crate::args::Config;
use crate::meta::ProjectMeta;
use crate::util;

/// The standalone binary as a distributable file.
pub fn package_exe(meta: &ProjectMeta, bin_path: &Path, out: &Path) -> Result<PathBuf, String> {
    let exe = out.join(format!("{}-{}-windows.exe", meta.crate_name, meta.version));
    util::copy(bin_path, &exe)?;
    Ok(exe)
}

/// The Inno Setup script. Paths arrive already Windows-flavored (the
/// installer only builds on a Windows host).
pub fn iss_script(
    meta: &ProjectMeta,
    stage_dir: &Path,
    plugins: &[PathBuf],
    exe: Option<&Path>,
    out: &Path,
) -> String {
    let mut files = String::new();
    for p in plugins {
        let name = p.file_name().unwrap().to_string_lossy();
        if p.is_dir() {
            // .vst3 bundle tree → %COMMONPROGRAMFILES%\VST3\<name>.vst3
            files += &format!(
                "Source: \"{}\\*\"; DestDir: \"{{commoncf}}\\VST3\\{name}\"; Flags: recursesubdirs\n",
                p.display()
            );
        } else {
            // bare .clap dll → %COMMONPROGRAMFILES%\CLAP
            files += &format!(
                "Source: \"{}\"; DestDir: \"{{commoncf}}\\CLAP\"\n",
                p.display()
            );
        }
    }
    if let Some(exe) = exe {
        files += &format!("Source: \"{}\"; DestDir: \"{{app}}\"\n", exe.display());
    }
    let _ = stage_dir; // the script references absolute paths directly
    format!(
        "[Setup]\n\
         AppId={{{{{identifier}}}}}\n\
         AppName={name}\n\
         AppVersion={version}\n\
         DefaultDirName={{autopf}}\\{name}\n\
         OutputDir={out}\n\
         OutputBaseFilename={crate_name}-{version}-setup\n\
         Compression=lzma2\n\
         \n\
         [Files]\n\
         {files}",
        identifier = meta.identifier,
        name = meta.display_name,
        version = meta.version,
        crate_name = meta.crate_name,
        out = out.display(),
    )
}

/// Compile the installer with Inno Setup's `iscc`.
pub fn package_installer(
    cfg: &Config,
    meta: &ProjectMeta,
    plugins: &[PathBuf],
    exe: Option<&Path>,
    out: &Path,
) -> Result<PathBuf, String> {
    let stage = out.join("installer-stage");
    util::fresh_dir(&stage)?;
    let iss = stage.join(format!("{}.iss", meta.crate_name));
    std::fs::write(&iss, iss_script(meta, &stage, plugins, exe, out)).map_err(|e| e.to_string())?;

    let iscc = util::tool(
        &cfg.iscc,
        "iscc",
        "install Inno Setup (https://jrsoftware.org/isinfo.php) or pass --iscc",
    )?;
    util::run(
        &iscc.to_string_lossy(),
        &[iss.to_string_lossy().into_owned()],
    )?;
    Ok(out.join(format!("{}-{}-setup.exe", meta.crate_name, meta.version)))
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
    fn iss_installs_plugins_and_app_to_their_canonical_dirs() {
        let dir = std::env::temp_dir().join(format!("skuiz-package-iss-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let clap = dir.join("my-gain.clap");
        let vst3 = dir.join("my-gain.vst3");
        let exe = dir.join("my-gain-1.2.3-windows.exe");
        std::fs::create_dir_all(&vst3).unwrap();
        std::fs::write(&clap, "dll").unwrap();
        std::fs::write(&exe, "exe").unwrap();

        let script = iss_script(
            &meta(),
            &dir,
            &[clap.clone(), vst3.clone()],
            Some(&exe),
            &dir,
        );
        assert!(script.contains("AppId={{org.example.my-gain}}"));
        assert!(script.contains("AppName=My Gain"));
        assert!(script.contains("AppVersion=1.2.3"));
        assert!(script.contains("OutputBaseFilename=my-gain-1.2.3-setup"));
        assert!(script.contains(&format!(
            "Source: \"{}\"; DestDir: \"{{commoncf}}\\CLAP\"",
            clap.display()
        )));
        assert!(script.contains(&format!(
            "Source: \"{}\\*\"; DestDir: \"{{commoncf}}\\VST3\\my-gain.vst3\"; Flags: recursesubdirs",
            vst3.display()
        )));
        assert!(script.contains(&format!(
            "Source: \"{}\"; DestDir: \"{{app}}\"",
            exe.display()
        )));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn iss_omits_the_app_when_there_is_no_standalone() {
        let dir = std::env::temp_dir().join(format!("skuiz-package-iss2-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let script = iss_script(&meta(), &dir, &[], None, &dir);
        assert!(!script.contains("{app}"));
        assert!(script.contains("[Files]"));
    }
}
