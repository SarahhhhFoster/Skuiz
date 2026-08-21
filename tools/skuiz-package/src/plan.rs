//! The plan: turn a coherent [`Config`] + [`ProjectMeta`] + host OS into
//! an ordered list of build/artifact/package steps. Pure and total — all
//! host-capability gating lives here, unit-tested for every OS, so the
//! executors in `main` never make a policy decision.

use crate::args::{Config, Format};
use crate::meta::ProjectMeta;

/// The operating system the tool is running on. Explicit (not `cfg!`) so
/// tests can exercise every host from any host.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Os {
    Macos,
    Linux,
    Windows,
}

impl Os {
    pub fn host() -> Self {
        if cfg!(target_os = "macos") {
            Os::Macos
        } else if cfg!(target_os = "windows") {
            Os::Windows
        } else {
            Os::Linux
        }
    }

    /// Which package formats this host can physically produce.
    pub fn buildable_formats(self) -> &'static [Format] {
        match self {
            Os::Macos => &[Format::Dmg],
            Os::Linux => &[Format::AppImage],
            Os::Windows => &[Format::Exe],
        }
    }
}

/// Which cargo builds to run.
#[derive(Debug, PartialEq, Eq)]
pub struct BuildPlan {
    /// Build the cdylib (`--lib`).
    pub lib: bool,
    /// Build the standalone app (`--bin <name>`).
    pub bin: Option<String>,
    /// Full feature list (`--features` + `vst3` when selected).
    pub features: Vec<String>,
}

/// A bundle to assemble into the staging area.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Artifact {
    /// The CLAP plugin (bundle dir on macOS, bare renamed library elsewhere).
    Clap,
    /// The VST3 plugin (directory bundle).
    Vst3,
    /// The standalone app (`.app` on macOS, raw binary elsewhere).
    Standalone,
}

/// A package file to emit.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Package {
    Dmg,
    AppImage,
    /// The standalone binary as `<name>-<version>-windows.exe`.
    Exe,
    /// Inno Setup installer wrapping plugins + app.
    InnoInstaller,
}

/// The resolved run.
#[derive(Debug)]
pub struct Plan {
    pub build: BuildPlan,
    pub artifacts: Vec<Artifact>,
    pub packages: Vec<Package>,
    /// Run clap-validator on the assembled .clap (when on PATH).
    pub validate_clap: bool,
    /// Non-fatal notes worth showing the user (e.g. skipped defaults).
    pub notes: Vec<String>,
}

/// The OS the *artifacts* target: the host, unless `--target` names a
/// different one (cross-compiled binaries take the target's file shapes —
/// `.dll`, `.exe`, VST3 `x86_64-win` layout).
pub fn artifact_os(cfg: &Config, host: Os) -> Os {
    match &cfg.target {
        Some(t) if t.contains("windows") => Os::Windows,
        Some(t) if t.contains("apple") || t.contains("darwin") => Os::Macos,
        Some(t) if t.contains("linux") => Os::Linux,
        _ => host,
    }
}

pub fn plan(cfg: &Config, meta: &ProjectMeta, os: Os) -> Result<Plan, String> {
    let mut notes = Vec::new();

    // Formats: explicit selection, or everything this host can build.
    let formats: Vec<Format> = match &cfg.formats {
        Some(f) => f.clone(),
        None => os.buildable_formats().to_vec(),
    };

    // Host gating: an explicitly requested format the host cannot build is
    // an error, never a silent skip. The exe is the exception: it is a
    // plain binary, so a cross toolchain (`--target`) makes it buildable
    // anywhere.
    for f in &formats {
        let requested = cfg.formats.is_some();
        let buildable =
            os.buildable_formats().contains(f) || (*f == Format::Exe && cfg.target.is_some());
        if requested && !buildable {
            let host = match f {
                Format::Dmg => "macOS (hdiutil)",
                Format::AppImage => "Linux (appimagetool)",
                Format::Exe => "Windows (or --target with a cross toolchain)",
            };
            return Err(format!("{f:?} can only be built on {host}"));
        }
    }

    if cfg.installer {
        if os != Os::Windows {
            return Err("--installer needs Windows (Inno Setup's iscc)".into());
        }
        if !formats.contains(&Format::Exe) {
            return Err("--installer only makes sense with --exe".into());
        }
    }

    let target_os = artifact_os(cfg, os);

    // AppImage wraps the standalone app; there is nothing else rational to
    // put in one.
    let has_standalone = meta.standalone_bin.is_some();
    if formats.contains(&Format::AppImage) && !has_standalone {
        if cfg.formats.is_some() {
            return Err(
                "--appimage needs a standalone app, but the project has no src/bin binary".into(),
            );
        }
        notes.push("skipping AppImage: the project has no standalone binary".into());
    }

    let mut features = cfg.features.clone();
    if cfg.vst3 && !features.iter().any(|f| f == "vst3") {
        features.push("vst3".into());
    }

    // Artifacts: everything built, per the selected contents policy.
    let mut artifacts = Vec::new();
    if cfg.plugins {
        artifacts.push(Artifact::Clap);
        if cfg.vst3 {
            artifacts.push(Artifact::Vst3);
        }
    }
    if has_standalone {
        artifacts.push(Artifact::Standalone);
    }

    let mut packages = Vec::new();
    for f in &formats {
        match f {
            Format::Dmg => packages.push(Package::Dmg),
            Format::AppImage => {
                if has_standalone {
                    packages.push(Package::AppImage);
                }
            }
            Format::Exe => {
                if has_standalone {
                    packages.push(Package::Exe);
                }
                if cfg.installer {
                    packages.push(Package::InnoInstaller);
                }
                if !has_standalone && !cfg.installer {
                    return Err(
                        "--exe packages the standalone app, but the project has no src/bin \
                         binary (or pass --installer to package only the plugins)"
                            .into(),
                    );
                }
            }
        }
    }

    let dmg_needs_content = packages.contains(&Package::Dmg) && artifacts.is_empty();
    if dmg_needs_content {
        return Err("nothing to put in the .dmg (both plugins and standalone are off)".into());
    }

    // The dmg and AppImage wrap host-native apps; cross-target binaries
    // don't belong in them.
    if target_os != os && (formats.contains(&Format::Dmg) || formats.contains(&Format::AppImage)) {
        return Err("--dmg/--appimage wrap host-native builds; drop --target or the format".into());
    }

    // The validator only runs host-native Unix binaries — not a
    // cross-compiled Windows .dll.
    let validate_clap = cfg.plugins
        && !cfg.skip_validation
        && target_os == os
        && matches!(os, Os::Macos | Os::Linux);

    Ok(Plan {
        build: BuildPlan {
            lib: cfg.plugins,
            bin: meta.standalone_bin.clone(),
            features,
        },
        artifacts,
        packages,
        validate_clap,
        notes,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn meta(standalone: bool) -> ProjectMeta {
        ProjectMeta {
            dir: PathBuf::from("x"),
            crate_name: "my-gain".into(),
            lib_name: "my_gain".into(),
            display_name: "My Gain".into(),
            version: "1.0.0".into(),
            identifier: "org.example.my-gain".into(),
            standalone_bin: standalone.then(|| "my-gain-standalone".to_string()),
            target_dir: PathBuf::from("x/target"),
        }
    }

    fn cfg(args: &[&str]) -> Config {
        let args: Vec<String> = args.iter().map(|s| s.to_string()).collect();
        match crate::args::parse(&args).unwrap() {
            crate::args::Parsed::Run(c) => *c,
            crate::args::Parsed::Help => unreachable!(),
        }
    }

    #[test]
    fn defaults_follow_the_host() {
        let m = meta(true);
        assert_eq!(
            plan(&cfg(&[]), &m, Os::Macos).unwrap().packages,
            [Package::Dmg]
        );
        assert_eq!(
            plan(&cfg(&[]), &m, Os::Linux).unwrap().packages,
            [Package::AppImage]
        );
        assert_eq!(
            plan(&cfg(&[]), &m, Os::Windows).unwrap().packages,
            [Package::Exe]
        );
    }

    #[test]
    fn explicit_format_on_the_wrong_host_errors() {
        let m = meta(true);
        assert!(plan(&cfg(&["--dmg"]), &m, Os::Linux).is_err());
        assert!(plan(&cfg(&["--appimage"]), &m, Os::Macos).is_err());
        assert!(plan(&cfg(&["--exe"]), &m, Os::Macos).is_err());
        // ...unless cross-compiling the binary.
        let c = cfg(&["--exe", "--target", "x86_64-pc-windows-msvc"]);
        assert!(plan(&c, &m, Os::Macos).is_ok());
    }

    #[test]
    fn appimage_needs_a_standalone_bin() {
        let m = meta(false);
        assert!(plan(&cfg(&["--appimage"]), &m, Os::Linux).is_err());
        // As a default it is skipped with a note instead.
        let p = plan(&cfg(&[]), &m, Os::Linux).unwrap();
        assert!(p.packages.is_empty());
        assert_eq!(p.notes.len(), 1);
    }

    #[test]
    fn installer_is_windows_only_and_needs_exe() {
        let m = meta(true);
        assert!(plan(&cfg(&["--exe", "--installer"]), &m, Os::Macos).is_err());
        let p = plan(&cfg(&["--exe", "--installer"]), &m, Os::Windows).unwrap();
        assert_eq!(p.packages, [Package::Exe, Package::InnoInstaller]);
    }

    #[test]
    fn vst3_adds_the_feature_and_artifact() {
        let m = meta(true);
        let p = plan(&cfg(&["--vst3"]), &m, Os::Macos).unwrap();
        assert!(p.build.features.contains(&"vst3".to_string()));
        assert_eq!(
            p.artifacts,
            [Artifact::Clap, Artifact::Vst3, Artifact::Standalone]
        );
    }

    #[test]
    fn validation_runs_on_unix_plugins_only() {
        let m = meta(true);
        assert!(plan(&cfg(&[]), &m, Os::Macos).unwrap().validate_clap);
        assert!(plan(&cfg(&[]), &m, Os::Linux).unwrap().validate_clap);
        assert!(!plan(&cfg(&[]), &m, Os::Windows).unwrap().validate_clap);
        assert!(
            !plan(&cfg(&["--skip-validation"]), &m, Os::Macos)
                .unwrap()
                .validate_clap
        );
        assert!(
            !plan(&cfg(&["--no-plugins"]), &m, Os::Macos)
                .unwrap()
                .validate_clap
        );
    }
}
