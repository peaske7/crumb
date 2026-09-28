//! Env files crumb writes in a worktree so the apps there reach its lease.
//! crumb owns each file whole; the first line names the lease, and `down`
//! removes only files that carry it.

use std::path::Path;

use anyhow::{Context, Result, bail};
use serde::Serialize;

use crate::config::Wire;
use crate::template::Vars;

pub const MARKER: &str = "# crumb lease";

/// How a wired file compares with what the lease needs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Status {
    Ok,
    Missing,
    /// Points at a port or lease the lease no longer has.
    Stale,
    /// Points at a host other than loopback.
    Remote {
        url: String,
    },
}

/// The file's text for this lease.
pub fn content(wire: &Wire, lease: &str, vars: &Vars) -> String {
    let mut text = format!("{MARKER} {lease}\n");
    for (key, value) in &wire.set {
        text.push_str(&format!("{key}={}\n", vars.render(value)));
    }
    text
}

/// Whether crumb may write or remove `text`'s file: one crumb wrote, or one
/// the older tool named in `adopt` wrote.
pub fn owned(text: &str, wire: &Wire) -> bool {
    let first = text.lines().next().unwrap_or_default();
    first.starts_with(MARKER) || wire.adopt.as_deref().is_some_and(|a| first.starts_with(a))
}

pub fn judge(worktree: &Path, wire: &Wire, expected: &str) -> Status {
    let Ok(actual) = std::fs::read_to_string(worktree.join(&wire.file)) else {
        return Status::Missing;
    };
    if actual == expected {
        return Status::Ok;
    }
    match actual.lines().find_map(remote_url) {
        Some(url) => Status::Remote { url },
        None => Status::Stale,
    }
}

/// An `http(s)://` or `ws(s)://` URL in an env line whose host is not loopback.
pub fn remote_url(line: &str) -> Option<String> {
    let (_, value) = line.split_once('=')?;
    let value = value.trim().trim_matches(['"', '\'']);
    let rest = ["http://", "https://", "ws://", "wss://"]
        .iter()
        .find_map(|scheme| value.strip_prefix(scheme))?;
    let authority = rest.split(['/', '?']).next()?;
    let host = match authority.strip_prefix('[') {
        Some(v6) => v6.split(']').next()?,
        None => authority.rsplit_once(':').map_or(authority, |(h, _)| h),
    };
    let loopback = matches!(host, "localhost" | "127.0.0.1" | "::1") || host.starts_with("127.");
    (!loopback).then(|| value.to_string())
}

/// What `write` did, for the step output.
#[derive(Debug, PartialEq, Eq)]
pub struct Written {
    pub changed: bool,
    pub seeded: bool,
}

/// Writes the file, first copying its seed from the main checkout when the
/// worktree has none. Refuses to replace a file crumb did not write.
pub fn write(worktree: &Path, main: Option<&Path>, wire: &Wire, text: &str) -> Result<Written> {
    let mut seeded = false;
    if let (Some(seed), Some(main)) = (&wire.seed, main) {
        let target = worktree.join(seed);
        let source = main.join(seed);
        if !target.exists() && source.is_file() && main != worktree {
            if let Some(parent) = target.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::copy(&source, &target)
                .with_context(|| format!("copying {} from the main checkout", seed))?;
            seeded = true;
        }
    }
    let path = worktree.join(&wire.file);
    match std::fs::read_to_string(&path) {
        Ok(existing) if existing == text => {
            return Ok(Written {
                changed: false,
                seeded,
            });
        }
        Ok(existing) if !owned(&existing, wire) => bail!(
            "{} exists and crumb did not write it; point the wire at a file crumb can own",
            wire.file
        ),
        _ => {}
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(&path, text).with_context(|| format!("writing {}", wire.file))?;
    Ok(Written {
        changed: true,
        seeded,
    })
}

/// Removes the file when crumb wrote it. Returns whether it did.
pub fn remove(worktree: &Path, wire: &Wire) -> Result<bool> {
    let path = worktree.join(&wire.file);
    match std::fs::read_to_string(&path) {
        Ok(existing) if owned(&existing, wire) => {
            std::fs::remove_file(&path).with_context(|| format!("removing {}", wire.file))?;
            Ok(true)
        }
        _ => Ok(false),
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::path::PathBuf;

    use super::*;

    struct Scratch(PathBuf);

    impl Scratch {
        fn new(name: &str) -> Self {
            let dir =
                std::env::temp_dir().join(format!("crumb-wire-{name}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            Self(dir)
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn wire() -> Wire {
        Wire {
            file: "apps/hub/.env.development.local".into(),
            seed: Some("apps/hub/.env.local".into()),
            adopt: Some("# written by scripts/vps/worktree-backend.sh".into()),
            set: BTreeMap::from([(
                "VITE_API_URL".to_string(),
                "http://127.0.0.1:{local_port}".to_string(),
            )]),
        }
    }

    fn vars(port: &str) -> Vars {
        let mut vars = Vars::default();
        vars.set("local_port", port);
        vars
    }

    #[test]
    fn writes_seeds_judges_and_removes() {
        let tree = Scratch::new("tree");
        let main = Scratch::new("main");
        std::fs::create_dir_all(main.0.join("apps/hub")).unwrap();
        std::fs::write(main.0.join("apps/hub/.env.local"), "BASE=1\n").unwrap();

        let text = content(&wire(), "a", &vars("18101"));
        assert_eq!(
            text,
            "# crumb lease a\nVITE_API_URL=http://127.0.0.1:18101\n"
        );
        assert_eq!(judge(&tree.0, &wire(), &text), Status::Missing);

        let written = write(&tree.0, Some(&main.0), &wire(), &text).unwrap();
        assert_eq!(
            written,
            Written {
                changed: true,
                seeded: true
            }
        );
        assert!(tree.0.join("apps/hub/.env.local").is_file());
        assert_eq!(judge(&tree.0, &wire(), &text), Status::Ok);

        // A new port makes the file stale until it is written again.
        let moved = content(&wire(), "a", &vars("18102"));
        assert_eq!(judge(&tree.0, &wire(), &moved), Status::Stale);

        assert!(remove(&tree.0, &wire()).unwrap());
        assert_eq!(judge(&tree.0, &wire(), &text), Status::Missing);
    }

    #[test]
    fn a_tailnet_url_from_the_old_script_is_remote_and_adoptable() {
        let tree = Scratch::new("legacy");
        std::fs::create_dir_all(tree.0.join("apps/hub")).unwrap();
        let legacy = "# written by scripts/vps/worktree-backend.sh (lease a)\n\
                      VITE_API_URL=http://100.64.0.1:8107\n";
        std::fs::write(tree.0.join("apps/hub/.env.development.local"), legacy).unwrap();
        let text = content(&wire(), "a", &vars("18107"));
        assert_eq!(
            judge(&tree.0, &wire(), &text),
            Status::Remote {
                url: "http://100.64.0.1:8107".into()
            }
        );
        assert!(write(&tree.0, None, &wire(), &text).unwrap().changed);
    }

    #[test]
    fn never_replaces_a_file_it_did_not_write() {
        let tree = Scratch::new("foreign");
        std::fs::create_dir_all(tree.0.join("apps/hub")).unwrap();
        std::fs::write(tree.0.join("apps/hub/.env.development.local"), "MINE=1\n").unwrap();
        let text = content(&wire(), "a", &vars("18101"));
        assert!(write(&tree.0, None, &wire(), &text).is_err());
        assert!(!remove(&tree.0, &wire()).unwrap());
    }

    #[test]
    fn loopback_hosts() {
        assert_eq!(remote_url("A=http://127.0.0.1:1"), None);
        assert_eq!(remote_url("A=\"http://localhost:1/x\""), None);
        assert_eq!(remote_url("A=ws://[::1]:1"), None);
        assert_eq!(
            remote_url("A=http://indigo:8101"),
            Some("http://indigo:8101".into())
        );
        assert_eq!(remote_url("A=postgres://db:5432/x"), None);
    }
}
