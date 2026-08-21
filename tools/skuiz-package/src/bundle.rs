//! Bundle assembly: turn built artifacts into the layout each format
//! expects, inside the output directory's `bundles/`.

use std::path::{Path, PathBuf};

use crate::meta::ProjectMeta;
use crate::plan::Os;
use crate::util;

/// Minimal Info.plist, mirroring the example bundle scripts.
pub fn plist(
    executable: &str,
    identifier: &str,
    name: &str,
    version: &str,
    signature: Option<&str>,
) -> String {
    let signature = signature
        .map(|s| format!("    <key>CFBundleSignature</key>\n    <string>{s}</string>\n"))
        .unwrap_or_default();
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>CFBundleExecutable</key>
    <string>{executable}</string>
    <key>CFBundleIdentifier</key>
    <string>{identifier}</string>
    <key>CFBundleName</key>
    <string>{name}</string>
    <key>CFBundlePackageType</key>
    <string>BNDL</string>
    <key>CFBundleVersion</key>
    <string>{version}</string>
{signature}</dict>
</plist>
"#
    )
}

fn write_plist(
    bundle: &Path,
    executable: &str,
    identifier: &str,
    meta: &ProjectMeta,
    signature: Option<&str>,
) -> Result<(), String> {
    let text = plist(
        executable,
        identifier,
        &meta.display_name,
        &meta.version,
        signature,
    );
    let path = bundle.join("Contents/Info.plist");
    std::fs::write(&path, text).map_err(|e| format!("cannot write {}: {e}", path.display()))
}

/// The `.clap`: a bundle on macOS, a bare renamed library elsewhere.
pub fn assemble_clap(
    meta: &ProjectMeta,
    os: Os,
    lib: &Path,
    out: &Path,
    icon: Option<&Path>,
) -> Result<PathBuf, String> {
    let bundle = out.join(format!("{}.clap", meta.crate_name));
    match os {
        Os::Macos => {
            util::fresh_dir(&bundle)?;
            util::copy(
                lib,
                &bundle.join(format!("Contents/MacOS/{}", meta.crate_name)),
            )?;
            write_plist(&bundle, &meta.crate_name, &meta.identifier, meta, None)?;
            if let Some(icns) = icon {
                util::copy(icns, &bundle.join("Contents/Resources/icon.icns"))?;
            }
        }
        Os::Linux | Os::Windows => {
            util::fresh_dir(out)?;
            util::copy(lib, &bundle)?;
        }
    }
    Ok(bundle)
}

/// The `.vst3`: a directory bundle on every platform (layout per the
/// Steinberg spec).
pub fn assemble_vst3(
    meta: &ProjectMeta,
    os: Os,
    lib: &Path,
    out: &Path,
    icon: Option<&Path>,
) -> Result<PathBuf, String> {
    let bundle = out.join(format!("{}.vst3", meta.crate_name));
    util::fresh_dir(&bundle)?;
    let identifier = format!("{}.vst3", meta.identifier);
    match os {
        Os::Macos => {
            util::copy(
                lib,
                &bundle.join(format!("Contents/MacOS/{}", meta.crate_name)),
            )?;
            write_plist(&bundle, &meta.crate_name, &identifier, meta, Some("????"))?;
            if let Some(icns) = icon {
                util::copy(icns, &bundle.join("Contents/Resources/icon.icns"))?;
            }
        }
        Os::Linux => {
            util::copy(
                lib,
                &bundle.join(format!("Contents/x86_64-linux/{}.so", meta.crate_name)),
            )?;
        }
        Os::Windows => {
            util::copy(
                lib,
                &bundle.join(format!("Contents/x86_64-win/{}.vst3", meta.crate_name)),
            )?;
        }
    }
    Ok(bundle)
}

/// The standalone `.app` (macOS only).
pub fn assemble_app(
    meta: &ProjectMeta,
    bin_path: &Path,
    bin: &str,
    out: &Path,
    icon: Option<&Path>,
) -> Result<PathBuf, String> {
    let bundle = out.join(format!("{}.app", meta.display_name));
    util::fresh_dir(&bundle)?;
    util::copy(bin_path, &bundle.join(format!("Contents/MacOS/{bin}")))?;
    write_plist(
        &bundle,
        bin,
        &format!("{}.standalone", meta.identifier),
        meta,
        None,
    )?;
    if let Some(icns) = icon {
        util::copy(icns, &bundle.join("Contents/Resources/icon.icns"))?;
    }
    Ok(bundle)
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

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "skuiz-package-bundle-{name}-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn plist_carries_the_identity() {
        let text = plist(
            "my-gain",
            "org.example.my-gain",
            "My Gain",
            "1.2.3",
            Some("????"),
        );
        assert!(text.contains("<string>my-gain</string>"));
        assert!(text.contains("<string>org.example.my-gain</string>"));
        assert!(text.contains("<string>My Gain</string>"));
        assert!(text.contains("<string>1.2.3</string>"));
        assert!(text.contains("CFBundleSignature"));
        assert!(!plist("a", "b", "c", "d", None).contains("CFBundleSignature"));
    }

    #[test]
    fn macos_clap_is_a_bundle() {
        let dir = scratch("macos-clap");
        let lib = dir.join("libmy_gain.dylib");
        std::fs::write(&lib, "binary").unwrap();
        let bundle = assemble_clap(&meta(), Os::Macos, &lib, &dir.join("out"), None).unwrap();
        assert_eq!(
            std::fs::read_to_string(bundle.join("Contents/MacOS/my-gain")).unwrap(),
            "binary"
        );
        let plist = std::fs::read_to_string(bundle.join("Contents/Info.plist")).unwrap();
        assert!(plist.contains("org.example.my-gain"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn linux_clap_is_the_bare_library() {
        let dir = scratch("linux-clap");
        let lib = dir.join("libmy_gain.so");
        std::fs::write(&lib, "binary").unwrap();
        let bundle = assemble_clap(&meta(), Os::Linux, &lib, &dir.join("out"), None).unwrap();
        assert_eq!(std::fs::read_to_string(&bundle).unwrap(), "binary");
        assert_eq!(bundle.file_name().unwrap(), "my-gain.clap");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn windows_vst3_uses_the_spec_layout() {
        let dir = scratch("win-vst3");
        let lib = dir.join("my_gain.dll");
        std::fs::write(&lib, "binary").unwrap();
        let bundle = assemble_vst3(&meta(), Os::Windows, &lib, &dir.join("out"), None).unwrap();
        assert_eq!(
            std::fs::read_to_string(bundle.join("Contents/x86_64-win/my-gain.vst3")).unwrap(),
            "binary"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
