//! Command-line surface: flags → [`Config`]. Hand-rolled (the crate is
//! std-only); every conflict check lands here so `plan` can assume a
//! coherent configuration.

use std::path::PathBuf;

/// A package format the tool can emit.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Format {
    /// macOS disk image (needs macOS: `hdiutil`).
    Dmg,
    /// Linux AppImage of the standalone app (needs Linux: `appimagetool`).
    AppImage,
    /// Windows standalone `.exe`, optionally with an Inno installer.
    Exe,
}

/// Parsed, conflict-checked invocation.
pub struct Config {
    /// Plugin project directory.
    pub path: PathBuf,
    /// Explicitly requested formats; `None` = every format this host can
    /// build (`--all`).
    pub formats: Option<Vec<Format>>,
    /// With `Exe`: also emit an Inno Setup installer.
    pub installer: bool,
    /// Release build unless `--debug`.
    pub release: bool,
    /// Extra cargo features (`--features a,b`, repeatable).
    pub features: Vec<String>,
    /// Include VST3: adds the `vst3` cargo feature.
    pub vst3: bool,
    /// `cargo build --target` override.
    pub target: Option<String>,
    /// Build/package the standalone app (false after `--no-standalone`).
    pub standalone: bool,
    /// Build/package plugin bundles (false after `--no-plugins`).
    pub plugins: bool,
    /// Display name override.
    pub name: Option<String>,
    /// Version override.
    pub version: Option<String>,
    /// Bundle identifier override.
    pub identifier: Option<String>,
    /// Icon (`.icns` for macOS bundles, `.png` for AppImage).
    pub icon: Option<PathBuf>,
    /// Output directory (default: `<project>/dist`).
    pub out: Option<PathBuf>,
    /// `appimagetool` location (else PATH lookup).
    pub appimagetool: Option<PathBuf>,
    /// `iscc` location (else PATH lookup).
    pub iscc: Option<PathBuf>,
    /// Skip the clap-validator run after bundle assembly.
    pub skip_validation: bool,
    /// Print the resolved plan without building anything.
    pub dry_run: bool,
}

/// What the parser decided: show help, or run with this config.
pub enum Parsed {
    Help,
    Run(Box<Config>),
}

pub const USAGE: &str = "\
skuiz-package — build and package a Skuiz plugin

usage: skuiz-package [path] [flags]

  path                    plugin project dir (default: .)

formats (default: every format this host can build):
  --dmg                   macOS disk image with plugin bundles + standalone .app
  --appimage              Linux AppImage of the standalone app
  --exe                   Windows standalone .exe (+ staged plugin files)
  --all                   same as the default
  --installer             with --exe: also emit an Inno Setup installer

build:
  --debug                 debug build (default: release)
  --features a,b          extra cargo features (repeatable)
  --vst3                  include VST3 (adds the vst3 cargo feature)
  --target <triple>       cargo target override
  --no-standalone         skip the standalone app
  --no-plugins            skip plugin bundles

identity (defaults derived from the project):
  --name <display name>   e.g. \"My Gain\"
  --version <semver>
  --identifier <id>       bundle id, e.g. org.example.my-gain
  --icon <path>           .icns (macOS) or .png (AppImage)

output and tools:
  --out <dir>             default: <project>/dist
  --appimagetool <path>   else searched on PATH
  --iscc <path>           else searched on PATH
  --skip-validation       do not run clap-validator on the .clap bundle
  --dry-run               print the resolved plan; build nothing
  -h, --help              this text
";

pub fn parse(args: &[String]) -> Result<Parsed, String> {
    let mut cfg = Config {
        path: PathBuf::from("."),
        formats: None,
        installer: false,
        release: true,
        features: Vec::new(),
        vst3: false,
        target: None,
        standalone: true,
        plugins: true,
        name: None,
        version: None,
        identifier: None,
        icon: None,
        out: None,
        appimagetool: None,
        iscc: None,
        skip_validation: false,
        dry_run: false,
    };
    let mut path_seen = false;
    let mut formats: Vec<Format> = Vec::new();
    let mut all = false;

    let mut i = 0;
    while i < args.len() {
        let arg = args[i].as_str();
        // Flags taking a value consume the next argument.
        let mut value = |name: &str| -> Result<String, String> {
            i += 1;
            args.get(i)
                .cloned()
                .ok_or_else(|| format!("{name} needs a value"))
        };
        match arg {
            "-h" | "--help" => return Ok(Parsed::Help),
            "--dmg" => formats.push(Format::Dmg),
            "--appimage" => formats.push(Format::AppImage),
            "--exe" => formats.push(Format::Exe),
            "--all" => all = true,
            "--installer" => cfg.installer = true,
            "--debug" => cfg.release = false,
            "--features" => {
                let v = value("--features")?;
                cfg.features.extend(
                    v.split(',')
                        .map(|s| s.trim().to_string())
                        .filter(|s| !s.is_empty()),
                );
            }
            "--vst3" => cfg.vst3 = true,
            "--target" => cfg.target = Some(value("--target")?),
            "--no-standalone" => cfg.standalone = false,
            "--no-plugins" => cfg.plugins = false,
            "--name" => cfg.name = Some(value("--name")?),
            "--version" => cfg.version = Some(value("--version")?),
            "--identifier" => cfg.identifier = Some(value("--identifier")?),
            "--icon" => cfg.icon = Some(PathBuf::from(value("--icon")?)),
            "--out" => cfg.out = Some(PathBuf::from(value("--out")?)),
            "--appimagetool" => cfg.appimagetool = Some(PathBuf::from(value("--appimagetool")?)),
            "--iscc" => cfg.iscc = Some(PathBuf::from(value("--iscc")?)),
            "--skip-validation" => cfg.skip_validation = true,
            "--dry-run" => cfg.dry_run = true,
            _ if arg.starts_with('-') => return Err(format!("unknown flag: {arg}")),
            _ => {
                if path_seen {
                    return Err(format!("unexpected extra argument: {arg}"));
                }
                path_seen = true;
                cfg.path = PathBuf::from(arg);
            }
        }
        i += 1;
    }

    if all && !formats.is_empty() {
        return Err("--all cannot be combined with explicit format flags".into());
    }
    if !all && !formats.is_empty() {
        cfg.formats = Some(formats);
    }
    if cfg.installer && !matches!(&cfg.formats, Some(f) if f.contains(&Format::Exe)) {
        return Err("--installer only makes sense with --exe".into());
    }
    if !cfg.standalone && !cfg.plugins {
        return Err("--no-standalone and --no-plugins leave nothing to package".into());
    }
    Ok(Parsed::Run(Box::new(cfg)))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_ok(args: &[&str]) -> Config {
        let args: Vec<String> = args.iter().map(|s| s.to_string()).collect();
        match parse(&args) {
            Ok(Parsed::Run(c)) => *c,
            Ok(Parsed::Help) => panic!("unexpected help"),
            Err(e) => panic!("parse failed: {e}"),
        }
    }

    fn parse_err(args: &[&str]) -> String {
        let args: Vec<String> = args.iter().map(|s| s.to_string()).collect();
        match parse(&args) {
            Err(e) => e,
            Ok(_) => panic!("expected an error"),
        }
    }

    #[test]
    fn defaults_are_sane() {
        let c = parse_ok(&[]);
        assert_eq!(c.path, PathBuf::from("."));
        assert!(c.formats.is_none(), "default = host picks formats");
        assert!(c.release);
        assert!(c.standalone && c.plugins);
        assert!(!c.installer && !c.vst3 && !c.dry_run);
    }

    #[test]
    fn format_flags_select() {
        let c = parse_ok(&["my-plugin", "--dmg", "--exe"]);
        assert_eq!(c.path, PathBuf::from("my-plugin"));
        assert_eq!(c.formats, Some(vec![Format::Dmg, Format::Exe]));
    }

    #[test]
    fn installer_requires_exe() {
        assert!(parse_err(&["--installer"]).contains("--exe"));
        let c = parse_ok(&["--exe", "--installer"]);
        assert!(c.installer);
    }

    #[test]
    fn features_split_on_commas_and_repeat() {
        let c = parse_ok(&["--features", "libpd,foo", "--features", "bar"]);
        assert_eq!(c.features, vec!["libpd", "foo", "bar"]);
    }

    #[test]
    fn rejects_bad_combinations() {
        assert!(parse_err(&["--all", "--dmg"]).contains("--all"));
        assert!(parse_err(&["--no-standalone", "--no-plugins"]).contains("nothing"));
        assert!(parse_err(&["--bogus"]).contains("unknown flag"));
        assert!(parse_err(&["a", "b"]).contains("extra argument"));
        assert!(parse_err(&["--name"]).contains("needs a value"));
    }

    #[test]
    fn help_is_help() {
        let args = vec!["--help".to_string()];
        assert!(matches!(parse(&args), Ok(Parsed::Help)));
    }
}
