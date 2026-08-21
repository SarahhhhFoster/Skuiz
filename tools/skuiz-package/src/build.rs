//! The cargo build step and artifact-path resolution.

use std::path::PathBuf;

use crate::args::Config;
use crate::meta::ProjectMeta;
use crate::plan::{BuildPlan, Os};
use crate::util;

/// Run the cargo builds the plan calls for.
pub fn build(cfg: &Config, meta: &ProjectMeta, plan: &BuildPlan) -> Result<(), String> {
    let manifest = meta.dir.join("Cargo.toml");
    let mut common: Vec<String> = vec![
        "build".into(),
        "--manifest-path".into(),
        manifest.to_string_lossy().into_owned(),
    ];
    if cfg.release {
        common.push("--release".into());
    }
    if !plan.features.is_empty() {
        common.push("--features".into());
        common.push(plan.features.join(","));
    }
    if let Some(t) = &cfg.target {
        common.push("--target".into());
        common.push(t.clone());
    }

    if plan.lib {
        let mut args = common.clone();
        args.push("--lib".into());
        util::run("cargo", &args)?;
    }
    if let Some(bin) = &plan.bin {
        let mut args = common;
        args.push("--bin".into());
        args.push(bin.clone());
        util::run("cargo", &args)?;
    }
    Ok(())
}

/// The directory cargo leaves this profile's artifacts in.
pub fn profile_dir(cfg: &Config, meta: &ProjectMeta) -> PathBuf {
    let profile = if cfg.release { "release" } else { "debug" };
    match &cfg.target {
        Some(t) => meta.target_dir.join(t).join(profile),
        None => meta.target_dir.join(profile),
    }
}

/// The built cdylib for this host.
pub fn lib_artifact(cfg: &Config, meta: &ProjectMeta, os: Os) -> PathBuf {
    let name = match os {
        Os::Macos => format!("lib{}.dylib", meta.lib_name),
        Os::Linux => format!("lib{}.so", meta.lib_name),
        Os::Windows => format!("{}.dll", meta.lib_name),
    };
    profile_dir(cfg, meta).join(name)
}

/// The built standalone binary for this host.
pub fn bin_artifact(cfg: &Config, meta: &ProjectMeta, os: Os, bin: &str) -> PathBuf {
    let name = match os {
        Os::Windows => format!("{bin}.exe"),
        _ => bin.to_string(),
    };
    profile_dir(cfg, meta).join(name)
}

/// Require an artifact to exist after the build, with a hint when it
/// does not (usually a missing `--features` for a cfg'd-out crate).
pub fn require(path: &std::path::Path) -> Result<(), String> {
    if path.is_file() {
        Ok(())
    } else {
        Err(format!(
            "expected build artifact {} — missing features or a failed build?",
            path.display()
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::args::Parsed;
    use std::path::Path;

    fn meta() -> ProjectMeta {
        ProjectMeta {
            dir: PathBuf::from("x"),
            crate_name: "my-gain".into(),
            lib_name: "my_gain".into(),
            display_name: "My Gain".into(),
            version: "1.0.0".into(),
            identifier: "org.example.my-gain".into(),
            standalone_bin: Some("my-gain-standalone".into()),
            target_dir: PathBuf::from("x/target"),
        }
    }

    fn cfg(args: &[&str]) -> Config {
        let args: Vec<String> = args.iter().map(|s| s.to_string()).collect();
        match crate::args::parse(&args).unwrap() {
            Parsed::Run(c) => *c,
            Parsed::Help => unreachable!(),
        }
    }

    #[test]
    fn artifact_names_follow_the_os() {
        let c = cfg(&[]);
        let m = meta();
        assert_eq!(
            lib_artifact(&c, &m, Os::Macos),
            Path::new("x/target/release/libmy_gain.dylib")
        );
        assert_eq!(
            lib_artifact(&c, &m, Os::Linux),
            Path::new("x/target/release/libmy_gain.so")
        );
        assert_eq!(
            lib_artifact(&c, &m, Os::Windows),
            Path::new("x/target/release/my_gain.dll")
        );
        assert_eq!(
            bin_artifact(&c, &m, Os::Windows, "my-gain-standalone"),
            Path::new("x/target/release/my-gain-standalone.exe")
        );
    }

    #[test]
    fn target_triple_and_debug_change_the_profile_dir() {
        let c = cfg(&["--debug", "--target", "x86_64-pc-windows-msvc"]);
        let m = meta();
        assert_eq!(
            profile_dir(&c, &m),
            Path::new("x/target/x86_64-pc-windows-msvc/debug")
        );
    }
}
