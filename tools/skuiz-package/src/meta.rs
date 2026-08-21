//! Project discovery: derive everything the packagers need from the
//! plugin source tree, mirroring the conventions of the example bundle
//! scripts (crate name → lib name, workspace-root version fallback,
//! `PluginInfo::id` as the bundle identifier).

use std::fs;
use std::path::{Path, PathBuf};

use crate::args::Config;

/// Everything the packagers need to know about the plugin project.
#[derive(Debug)]
pub struct ProjectMeta {
    /// Project directory (the positional arg, canonicalized-ish).
    pub dir: PathBuf,
    /// Crate name as written in the manifest (`ducking-compressor`).
    pub crate_name: String,
    /// Library name (`ducking_compressor`) — the cdylib file stem.
    pub lib_name: String,
    /// Display name for bundles/installers ("Ducking Compressor").
    pub display_name: String,
    /// Package version.
    pub version: String,
    /// Bundle identifier (`org.skuiz.ducking-compressor`).
    pub identifier: String,
    /// Standalone bin target name, if the project has one and the
    /// invocation wants it.
    pub standalone_bin: Option<String>,
    /// Cargo target dir the build artifacts land in.
    pub target_dir: PathBuf,
}

/// One `key = "value"` string within a `[section]` of a manifest.
fn manifest_value(manifest: &str, section: &str, key: &str) -> Option<String> {
    let mut in_section = false;
    for line in manifest.lines() {
        let line = line.trim();
        if line.starts_with('[') {
            in_section = line == format!("[{section}]");
            continue;
        }
        if in_section {
            if let Some(v) = line.strip_prefix(&format!("{key} = ")) {
                return Some(v.trim().trim_matches('"').to_string());
            }
            // Dotted workspace form: `version.workspace = true`.
            if line == format!("{key}.workspace = true") {
                return None;
            }
        }
    }
    None
}

/// True when the manifest says `key.workspace = true` in `section`.
fn is_workspace_inherited(manifest: &str, section: &str, key: &str) -> bool {
    let mut in_section = false;
    for line in manifest.lines() {
        let line = line.trim();
        if line.starts_with('[') {
            in_section = line == format!("[{section}]");
        } else if in_section && line == format!("{key}.workspace = true") {
            return true;
        }
    }
    false
}

/// The nearest ancestor (starting at `dir`) whose Cargo.toml declares a
/// `[workspace]` — cargo's build root for both workspace members and
/// scaffolded standalone projects (which carry their own `[workspace]`).
fn workspace_root(dir: &Path) -> Option<PathBuf> {
    let mut d = Some(dir);
    while let Some(p) = d {
        let manifest = p.join("Cargo.toml");
        if let Ok(text) = fs::read_to_string(&manifest) {
            if text.lines().any(|l| l.trim() == "[workspace]") {
                return Some(p.to_path_buf());
            }
        }
        d = p.parent();
    }
    None
}

/// First `key: "value"` in a Rust source file, optionally after an anchor
/// substring (so `name:` is found inside `PluginInfo`, not a `ParamDef`).
fn rust_value(source: &str, key: &str, after: Option<&str>) -> Option<String> {
    let hay = match after {
        Some(anchor) => &source[source.find(anchor)?..],
        None => source,
    };
    let needle = format!("{key}:");
    let pos = hay.find(&needle)?;
    let rest = hay[pos + needle.len()..].trim_start();
    let rest = rest.strip_prefix('"')?;
    let end = rest.find('"')?;
    Some(rest[..end].to_string())
}

/// "my-gain" → "My Gain" (same transform `new-plugin.sh` uses).
pub fn titleize(name: &str) -> String {
    name.split('-')
        .map(|w| {
            let mut c = w.chars();
            match c.next() {
                Some(first) => first.to_uppercase().collect::<String>() + c.as_str(),
                None => String::new(),
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

pub fn discover(cfg: &Config) -> Result<ProjectMeta, String> {
    let dir = cfg.path.clone();
    let manifest_path = dir.join("Cargo.toml");
    let manifest = fs::read_to_string(&manifest_path)
        .map_err(|e| format!("cannot read {}: {e}", manifest_path.display()))?;

    let crate_name =
        manifest_value(&manifest, "package", "name").ok_or("Cargo.toml has no [package] name")?;
    let lib_name =
        manifest_value(&manifest, "lib", "name").unwrap_or_else(|| crate_name.replace('-', "_"));

    let root = workspace_root(&dir).unwrap_or_else(|| dir.clone());
    let version = match cfg.version.clone() {
        Some(v) => v,
        None => {
            if is_workspace_inherited(&manifest, "package", "version") {
                let root_manifest = fs::read_to_string(root.join("Cargo.toml"))
                    .map_err(|e| format!("cannot read workspace root manifest: {e}"))?;
                manifest_value(&root_manifest, "workspace.package", "version")
                    .ok_or("workspace root has no [workspace.package] version")?
            } else {
                manifest_value(&manifest, "package", "version")
                    .ok_or("Cargo.toml has no version")?
            }
        }
    };

    // PluginInfo in src/lib.rs carries the id and display name; the id is
    // the anchor so `name:` doesn't match a ParamDef first.
    let lib_rs = fs::read_to_string(dir.join("src/lib.rs")).unwrap_or_default();
    let identifier = match cfg.identifier.clone() {
        Some(id) => id,
        None => rust_value(&lib_rs, "id", None)
            .ok_or("no bundle identifier: set `id:` in PluginInfo or pass --identifier")?,
    };
    let display_name = cfg
        .name
        .clone()
        .or_else(|| rust_value(&lib_rs, "name", Some(&format!("id: \"{identifier}\""))))
        .unwrap_or_else(|| titleize(&crate_name));

    let standalone_bin = if cfg.standalone {
        find_standalone_bin(&dir, &crate_name)
    } else {
        None
    };

    // CARGO_TARGET_DIR wins, matching cargo (absolute paths only — a
    // relative one is cargo-relative-to-root, resolve it the same way).
    let target_dir = match std::env::var_os("CARGO_TARGET_DIR") {
        Some(p) => {
            let p = PathBuf::from(p);
            if p.is_absolute() {
                p
            } else {
                root.join(p)
            }
        }
        None => root.join("target"),
    };

    Ok(ProjectMeta {
        dir,
        crate_name,
        lib_name,
        display_name,
        version,
        identifier,
        standalone_bin,
        target_dir,
    })
}

/// `src/bin/<crate>-standalone.rs` by convention; otherwise a lone
/// `src/bin/*.rs`; otherwise the project has no standalone app.
fn find_standalone_bin(dir: &Path, crate_name: &str) -> Option<String> {
    let bin_dir = dir.join("src/bin");
    let conventional = bin_dir.join(format!("{crate_name}-standalone.rs"));
    if conventional.is_file() {
        return Some(format!("{crate_name}-standalone"));
    }
    let entries: Vec<_> = fs::read_dir(&bin_dir)
        .ok()?
        .filter_map(|e| e.ok())
        .filter(|e| e.path().extension().is_some_and(|x| x == "rs"))
        .collect();
    if entries.len() == 1 {
        return entries[0]
            .path()
            .file_stem()
            .and_then(|s| s.to_str())
            .map(str::to_string);
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};

    static SEQ: AtomicU32 = AtomicU32::new(0);

    /// A throwaway project dir; removed on drop.
    struct Fixture(PathBuf);

    impl Fixture {
        fn new() -> Self {
            let dir = std::env::temp_dir().join(format!(
                "skuiz-package-meta-{}-{}",
                std::process::id(),
                SEQ.fetch_add(1, Ordering::SeqCst)
            ));
            fs::create_dir_all(dir.join("src")).unwrap();
            Self(dir)
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    const STANDALONE_MANIFEST: &str = r#"
[package]
name = "my-gain"
version = "1.2.3"

[lib]
crate-type = ["cdylib"]

[workspace]
"#;

    const LIB_RS: &str = r#"
impl Processor for MyGain {
    fn info() -> PluginInfo {
        PluginInfo {
            id: "org.example.my-gain",
            name: "My Gain",
            vendor: "t",
            version: "0",
            description: "",
        }
    }
    fn params() -> &'static [ParamDef] {
        &[ParamDef { id: 0, name: "Gain", ..DEFAULT }]
    }
}
"#;

    fn cfg_at(dir: &Path) -> Config {
        match crate::args::parse(&[dir.to_str().unwrap().to_string()]).unwrap() {
            crate::args::Parsed::Run(c) => *c,
            crate::args::Parsed::Help => unreachable!(),
        }
    }

    #[test]
    fn standalone_project_discovers_everything() {
        let fx = Fixture::new();
        fs::write(fx.0.join("Cargo.toml"), STANDALONE_MANIFEST).unwrap();
        fs::write(fx.0.join("src/lib.rs"), LIB_RS).unwrap();
        fs::create_dir_all(fx.0.join("src/bin")).unwrap();
        fs::write(fx.0.join("src/bin/my-gain-standalone.rs"), "fn main() {}").unwrap();

        let meta = discover(&cfg_at(&fx.0)).unwrap();
        assert_eq!(meta.crate_name, "my-gain");
        assert_eq!(meta.lib_name, "my_gain");
        assert_eq!(meta.version, "1.2.3");
        assert_eq!(meta.identifier, "org.example.my-gain");
        assert_eq!(meta.display_name, "My Gain");
        assert_eq!(meta.standalone_bin.as_deref(), Some("my-gain-standalone"));
        assert_eq!(meta.target_dir, fx.0.join("target"));
    }

    #[test]
    fn workspace_member_inherits_version_from_root() {
        let fx = Fixture::new();
        let member = fx.0.join("examples/w-member");
        fs::create_dir_all(member.join("src")).unwrap();
        fs::write(
            fx.0.join("Cargo.toml"),
            "[workspace]\nmembers = [\"examples/w-member\"]\n\n[workspace.package]\nversion = \"9.9.9\"\n",
        )
        .unwrap();
        fs::write(
            member.join("Cargo.toml"),
            "[package]\nname = \"w-member\"\nversion.workspace = true\n",
        )
        .unwrap();
        fs::write(member.join("src/lib.rs"), LIB_RS).unwrap();

        let meta = discover(&cfg_at(&member)).unwrap();
        assert_eq!(meta.version, "9.9.9");
        assert_eq!(meta.target_dir, fx.0.join("target"));
    }

    #[test]
    fn plugin_info_id_anchors_the_display_name() {
        // `rust_value` without the anchor would grab ParamDef's "Gain".
        assert_eq!(
            rust_value(LIB_RS, "name", Some("id: \"org.example.my-gain\"")),
            Some("My Gain".to_string())
        );
    }

    #[test]
    fn missing_identifier_is_an_error() {
        let fx = Fixture::new();
        fs::write(fx.0.join("Cargo.toml"), STANDALONE_MANIFEST).unwrap();
        fs::write(fx.0.join("src/lib.rs"), "struct Nothing;").unwrap();
        let err = discover(&cfg_at(&fx.0)).unwrap_err();
        assert!(err.contains("--identifier"), "{err}");
    }

    #[test]
    fn lone_bin_is_the_standalone() {
        let fx = Fixture::new();
        fs::write(fx.0.join("Cargo.toml"), STANDALONE_MANIFEST).unwrap();
        fs::write(fx.0.join("src/lib.rs"), LIB_RS).unwrap();
        fs::create_dir_all(fx.0.join("src/bin")).unwrap();
        fs::write(fx.0.join("src/bin/app.rs"), "fn main() {}").unwrap();
        assert_eq!(
            discover(&cfg_at(&fx.0)).unwrap().standalone_bin.as_deref(),
            Some("app")
        );
    }

    #[test]
    fn titleize_matches_the_scaffolder() {
        assert_eq!(titleize("my-gain"), "My Gain");
        assert_eq!(titleize("solid-synth"), "Solid Synth");
    }
}
