//! Process and filesystem helpers shared by the executors.

use std::path::PathBuf;
use std::process::Command;

/// Run a command, inheriting stdio, and require success.
pub fn run(tool: &str, args: &[String]) -> Result<(), String> {
    eprintln!("+ {tool} {}", args.join(" "));
    let status = Command::new(tool)
        .args(args)
        .status()
        .map_err(|e| format!("failed to launch {tool}: {e}"))?;
    if status.success() {
        Ok(())
    } else {
        Err(format!("{tool} exited with {status}"))
    }
}

/// Find an executable on PATH (no extension games — this runs on the host
/// OS only, and the tools we look up are Unix-style names).
pub fn which(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path) {
        let candidate = dir.join(name);
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}

/// Resolve a tool: explicit flag path, else PATH, else an error naming
/// what to install.
pub fn tool(explicit: &Option<PathBuf>, name: &str, hint: &str) -> Result<PathBuf, String> {
    if let Some(p) = explicit {
        return Ok(p.clone());
    }
    which(name).ok_or_else(|| format!("{name} not found on PATH — {hint}"))
}

/// Remove and recreate a directory.
pub fn fresh_dir(path: &std::path::Path) -> Result<(), String> {
    if path.exists() {
        std::fs::remove_dir_all(path)
            .map_err(|e| format!("cannot clear {}: {e}", path.display()))?;
    }
    std::fs::create_dir_all(path).map_err(|e| format!("cannot create {}: {e}", path.display()))
}

/// Copy a file, creating the parent directory.
pub fn copy(from: &std::path::Path, to: &std::path::Path) -> Result<(), String> {
    if let Some(parent) = to.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| format!("cannot create {}: {e}", parent.display()))?;
    }
    std::fs::copy(from, to)
        .map_err(|e| format!("cannot copy {} to {}: {e}", from.display(), to.display()))?;
    Ok(())
}

/// Copy a directory tree (bundles are trees).
pub fn copy_tree(from: &std::path::Path, to: &std::path::Path) -> Result<(), String> {
    for entry in
        std::fs::read_dir(from).map_err(|e| format!("cannot read {}: {e}", from.display()))?
    {
        let entry = entry.map_err(|e| e.to_string())?;
        let dest = to.join(entry.file_name());
        let ty = entry.file_type().map_err(|e| e.to_string())?;
        if ty.is_dir() {
            std::fs::create_dir_all(&dest).map_err(|e| e.to_string())?;
            copy_tree(&entry.path(), &dest)?;
        } else {
            copy(&entry.path(), &dest)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn which_finds_sh() {
        assert!(which("sh").is_some());
        assert!(which("definitely-not-a-real-tool-xyz").is_none());
    }

    #[test]
    fn copy_tree_roundtrip() {
        let base = std::env::temp_dir().join(format!("skuiz-package-util-{}", std::process::id()));
        let src = base.join("a");
        std::fs::create_dir_all(src.join("Contents/MacOS")).unwrap();
        std::fs::write(src.join("Contents/Info.plist"), "plist").unwrap();
        std::fs::write(src.join("Contents/MacOS/bin"), "bin").unwrap();
        let dest = base.join("b");
        copy_tree(&src, &dest).unwrap();
        assert_eq!(
            std::fs::read_to_string(dest.join("Contents/Info.plist")).unwrap(),
            "plist"
        );
        assert_eq!(
            std::fs::read_to_string(dest.join("Contents/MacOS/bin")).unwrap(),
            "bin"
        );
        let _ = std::fs::remove_dir_all(&base);
    }
}
