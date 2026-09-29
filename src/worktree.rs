//! Which git worktree crumb runs in, and the lease name it gets.

use std::path::{Path, PathBuf};

use anyhow::{Result, bail};

use crate::run::Runner;

/// A git worktree and the repository's main checkout.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Checkout {
    pub root: PathBuf,
    pub main: PathBuf,
}

impl Checkout {
    pub fn is_main(&self) -> bool {
        self.root == self.main
    }

    /// The lease name this worktree gets by default.
    pub fn lease(&self) -> String {
        slug(
            &self
                .root
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default(),
        )
    }
}

/// The worktree containing `dir`, if it is in a git repository.
pub fn find(runner: &Runner, dir: &Path) -> Option<Checkout> {
    let root = PathBuf::from(git(runner, dir, &["rev-parse", "--show-toplevel"])?.trim());
    // The first entry of `git worktree list` is always the main checkout.
    let main = git(runner, dir, &["worktree", "list", "--porcelain"])?
        .lines()
        .find_map(|line| line.strip_prefix("worktree "))
        .map(PathBuf::from)
        .unwrap_or_else(|| root.clone());
    Some(Checkout { root, main })
}

/// A git command's stdout, run in `dir`, or nothing when it fails.
pub fn git(runner: &Runner, dir: &Path, args: &[&str]) -> Option<String> {
    let mut argv = vec!["-C".to_string(), dir.to_string_lossy().into_owned()];
    argv.extend(args.iter().map(|a| a.to_string()));
    let output = runner
        .run(format!("git {}", args.join(" ")), "git", &argv, None)
        .ok()?;
    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).into_owned())
}

/// Lowercase, with anything outside `[a-z0-9]` turned into `_`.
pub fn slug(name: &str) -> String {
    name.chars()
        .map(|c| match c.to_ascii_lowercase() {
            c @ ('a'..='z' | '0'..='9') => c,
            _ => '_',
        })
        .collect()
}

/// Lease names reach shells and file paths, so they stay plain.
pub fn check_name(name: &str) -> Result<()> {
    if name.is_empty()
        || name.len() > 48
        || !name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
    {
        bail!("lease names use a-z, 0-9 and _ (at most 48), got {name:?}");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slugs() {
        assert_eq!(slug("lym-1119-2"), "lym_1119_2");
        assert_eq!(slug("Feature.X"), "feature_x");
    }

    #[test]
    fn names() {
        assert!(check_name("lym_1119").is_ok());
        assert!(check_name("x; rm").is_err());
        assert!(check_name("").is_err());
    }

    #[test]
    fn finds_this_repository() {
        let checkout = find(&Runner::default(), Path::new(env!("CARGO_MANIFEST_DIR"))).unwrap();
        assert!(checkout.root.join("Cargo.toml").is_file());
    }
}
