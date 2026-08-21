//! skuiz-package — build a Skuiz plugin project and package it as .dmg,
//! .AppImage, and/or .exe. See `args::USAGE` for the full surface.
//!
//! The pipeline: parse flags → discover project metadata → resolve the
//! plan (pure, in `plan.rs`) → cargo build → assemble bundles → validate
//! → package. Everything policy-shaped lives in `plan.rs`; this file only
//! sequences the executors.

mod appimage;
mod args;
mod build;
mod bundle;
mod dmg;
mod exe;
mod meta;
mod plan;
mod util;

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use args::{Config, Parsed};
use meta::ProjectMeta;
use plan::{Artifact, Os, Package, Plan};

fn main() -> ExitCode {
    let argv: Vec<String> = std::env::args().skip(1).collect();
    match args::parse(&argv) {
        Ok(Parsed::Help) => {
            print!("{}", args::USAGE);
            ExitCode::SUCCESS
        }
        Ok(Parsed::Run(cfg)) => match run(&cfg) {
            Ok(()) => ExitCode::SUCCESS,
            Err(e) => {
                eprintln!("skuiz-package: {e}");
                ExitCode::FAILURE
            }
        },
        Err(e) => {
            eprintln!("skuiz-package: {e}\n\n{}", args::USAGE);
            ExitCode::FAILURE
        }
    }
}

fn run(cfg: &Config) -> Result<(), String> {
    let os = Os::host();
    let meta = meta::discover(cfg)?;
    let plan = plan::plan(cfg, &meta, os)?;
    let out = cfg.out.clone().unwrap_or_else(|| meta.dir.join("dist"));
    // File shapes follow the build target, which differs from the host
    // only for cross-compiled --exe runs.
    let target_os = plan::artifact_os(cfg, os);

    if cfg.dry_run {
        print_plan(cfg, &meta, &plan, &out, os);
        return Ok(());
    }

    for note in &plan.notes {
        eprintln!("note: {note}");
    }

    // Build.
    build::build(cfg, &meta, &plan.build)?;

    // Assemble bundles into dist/bundles/.
    let bundles_dir = out.join("bundles");
    let mut assembled: Vec<PathBuf> = Vec::new();
    let mut standalone_path: Option<PathBuf> = None;
    for artifact in &plan.artifacts {
        match artifact {
            Artifact::Clap => {
                let lib = build::lib_artifact(cfg, &meta, target_os);
                build::require(&lib)?;
                let b = bundle::assemble_clap(
                    &meta,
                    target_os,
                    &lib,
                    &bundles_dir,
                    cfg.icon.as_deref(),
                )?;
                if plan.validate_clap {
                    validate_clap(&b)?;
                }
                assembled.push(b);
            }
            Artifact::Vst3 => {
                // The VST3 build is the same cdylib compiled with the vst3
                // feature; both exports live in one binary.
                let lib = build::lib_artifact(cfg, &meta, target_os);
                build::require(&lib)?;
                assembled.push(bundle::assemble_vst3(
                    &meta,
                    target_os,
                    &lib,
                    &bundles_dir,
                    cfg.icon.as_deref(),
                )?);
            }
            Artifact::Standalone => {
                let bin = meta.standalone_bin.as_ref().expect("plan gated this");
                let bin_path = build::bin_artifact(cfg, &meta, target_os, bin);
                build::require(&bin_path)?;
                standalone_path = Some(if target_os == Os::Macos {
                    bundle::assemble_app(&meta, &bin_path, bin, &bundles_dir, cfg.icon.as_deref())?
                } else {
                    bin_path
                });
            }
        }
    }
    if let Some(app) = &standalone_path {
        if target_os == Os::Macos {
            assembled.push(app.clone());
        }
    }

    // Package.
    let mut produced: Vec<PathBuf> = Vec::new();
    for package in &plan.packages {
        produced.push(match package {
            Package::Dmg => dmg::package_dmg(&meta, &assembled, &out)?,
            Package::AppImage => appimage::package_appimage(
                cfg,
                &meta,
                standalone_path.as_ref().expect("plan gated this"),
                meta.standalone_bin.as_ref().expect("plan gated this"),
                &out,
            )?,
            Package::Exe => exe::package_exe(
                &meta,
                standalone_path.as_ref().expect("plan gated this"),
                &out,
            )?,
            Package::InnoInstaller => {
                exe::package_installer(cfg, &meta, &assembled, standalone_path.as_deref(), &out)?
            }
        });
    }

    println!("done:");
    for p in produced {
        println!("  {}", p.display());
    }
    Ok(())
}

/// Run clap-validator when it is on PATH; absence is a note, not an error.
fn validate_clap(bundle: &Path) -> Result<(), String> {
    match util::which("clap-validator") {
        Some(v) => util::run(
            &v.to_string_lossy(),
            &["validate".into(), bundle.to_string_lossy().into_owned()],
        ),
        None => {
            eprintln!("note: clap-validator not on PATH; skipping validation");
            Ok(())
        }
    }
}

fn print_plan(cfg: &Config, meta: &ProjectMeta, plan: &Plan, out: &Path, os: Os) {
    println!("host:           {os:?}");
    println!(
        "project:        {} ({})",
        meta.dir.display(),
        meta.crate_name
    );
    println!("display name:   {}", meta.display_name);
    println!("version:        {}", meta.version);
    println!("identifier:     {}", meta.identifier);
    println!("output:         {}", out.display());
    println!(
        "profile:        {}",
        if cfg.release { "release" } else { "debug" }
    );
    if let Some(t) = &cfg.target {
        println!("target:         {t}");
    }
    println!(
        "cargo builds:   {}{}{}",
        if plan.build.lib { "--lib " } else { "" },
        plan.build
            .bin
            .as_ref()
            .map(|b| format!("--bin {b} "))
            .unwrap_or_default(),
        if plan.build.features.is_empty() {
            String::new()
        } else {
            format!("--features {}", plan.build.features.join(","))
        }
    );
    println!("artifacts:      {:?}", plan.artifacts);
    println!("packages:       {:?}", plan.packages);
    println!("validate .clap: {}", plan.validate_clap);
    for note in &plan.notes {
        println!("note:           {note}");
    }
}
