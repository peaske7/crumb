//! Checks that say a running lease is behind its worktree: files changed
//! after the backend started, or a lockfile the image doesn't match.

use std::collections::HashMap;
use std::path::Path;

use jiff::{SignedDuration, Timestamp};
use sha2::{Digest, Sha256};

use crate::config::Config;
use crate::lease::{Lease, Reason, State};
use crate::run::{Runner, quote};

/// Directories that never hold source crumb should watch. Dot-directories are
/// skipped as well.
const SKIP: &[&str] = &[
    "node_modules",
    "dist",
    "build",
    "target",
    "out",
    "coverage",
    "__pycache__",
];

/// Grace for clock differences between this machine and the host.
const SKEW: SignedDuration = SignedDuration::from_secs(2);

/// Facts about images, fetched once per image id and kept for the session.
#[derive(Default)]
pub struct Images {
    known: HashMap<String, ImageFacts>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImageFacts {
    pub built: Option<Timestamp>,
    pub lockfile_sha: Option<String>,
}

impl Images {
    /// Images the deps check needs but hasn't looked at yet.
    pub fn missing(&self, config: &Config, leases: &[Lease]) -> Vec<String> {
        if config.deps.is_none() {
            return Vec::new();
        }
        let mut ids: Vec<String> = leases
            .iter()
            .filter(|l| !l.worktree_gone())
            .filter_map(|l| l.image.clone())
            .filter(|id| safe_image_id(id) && !self.known.contains_key(id))
            .collect();
        ids.sort();
        ids.dedup();
        ids
    }

    /// Reads when the image was built and hashes its lockfile, in one host
    /// command. It costs about a second, so it runs once per image.
    pub fn fetch(&mut self, runner: &Runner, config: &Config, id: &str) {
        let Some(deps) = &config.deps else {
            return;
        };
        let command = format!(
            "docker image inspect --format '{{{{.Created}}}}' {id} && \
             docker run --rm --network none --entrypoint sha256sum {id} {}",
            quote(&deps.image_path)
        );
        let facts = match runner.command(&config.host, &command) {
            Ok(output) => parse_image_facts(&String::from_utf8_lossy(&output.stdout)),
            Err(_) => ImageFacts {
                built: None,
                lockfile_sha: None,
            },
        };
        self.known.insert(id.to_string(), facts);
    }

    #[cfg(test)]
    fn insert(&mut self, id: &str, facts: ImageFacts) {
        self.known.insert(id.to_string(), facts);
    }
}

fn parse_image_facts(stdout: &str) -> ImageFacts {
    let mut lines = stdout.lines();
    let built = lines.next().and_then(|line| line.trim().parse().ok());
    let lockfile_sha = lines
        .next()
        .and_then(|line| line.split_whitespace().next())
        .filter(|sha| sha.len() == 64 && sha.chars().all(|c| c.is_ascii_hexdigit()))
        .map(str::to_string);
    ImageFacts {
        built,
        lockfile_sha,
    }
}

/// Image ids go into a shell command, so only plain ids are used.
fn safe_image_id(id: &str) -> bool {
    !id.is_empty() && id.chars().all(|c| c.is_ascii_alphanumeric() || c == ':')
}

/// Adds the build and deps reasons to leases whose worktree is on this machine.
pub fn apply(config: &Config, images: &Images, leases: &mut [Lease]) {
    for lease in leases.iter_mut() {
        let Some(worktree) = lease
            .worktree
            .as_ref()
            .filter(|w| w.exists)
            .map(|w| w.path.clone())
        else {
            continue;
        };
        let worktree = Path::new(&worktree);
        if running(&lease.state)
            && let Some(since) = lease.since
            && let Some(file) = changed_since(worktree, &lease.watch, since)
        {
            lease.changed_file = Some(file);
            lease.reasons.push(Reason::RestartPending);
        }
        if let (Some(deps), Some(image)) = (&config.deps, &lease.image)
            && let Some(facts) = images.known.get(image)
        {
            lease.image_built = facts.built;
            if let (Some(in_image), Some(local)) = (
                &facts.lockfile_sha,
                file_sha256(&worktree.join(&deps.lockfile)),
            ) && *in_image != local
            {
                lease.reasons.push(Reason::DepsBehind);
            }
        }
        lease.regroup();
    }
}

fn running(state: &State) -> bool {
    matches!(
        state,
        State::Healthy | State::Running | State::Starting | State::Unhealthy
    )
}

/// The first file under the watched paths of `root` modified after `since`,
/// relative to `root`. Stops at the first hit, so a changed worktree costs
/// almost nothing. Nothing watched means nothing to say.
pub fn changed_since(root: &Path, watch: &[String], since: Timestamp) -> Option<String> {
    let cutoff = since.checked_add(SKEW).ok()?;
    let newer = |path: &Path| {
        std::fs::metadata(path)
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| Timestamp::try_from(t).ok())
            .is_some_and(|m| m > cutoff)
    };
    let mut stack = Vec::new();
    for relative in watch {
        let path = root.join(relative);
        if path.is_dir() {
            stack.push(path);
        } else if newer(&path) {
            return Some(relative.clone());
        }
    }
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if name.starts_with('.') || SKIP.contains(&name.as_ref()) {
                continue;
            }
            let Ok(kind) = entry.file_type() else {
                continue;
            };
            if kind.is_dir() {
                stack.push(entry.path());
                continue;
            }
            if kind.is_file() && newer(&entry.path()) {
                let path = entry.path();
                let relative = path.strip_prefix(root).unwrap_or(&path);
                return Some(relative.to_string_lossy().into_owned());
            }
        }
    }
    None
}

fn file_sha256(path: &Path) -> Option<String> {
    let bytes = std::fs::read(path).ok()?;
    Some(
        Sha256::digest(&bytes)
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect(),
    )
}

#[cfg(test)]
mod tests {
    use std::fs::{self, File};
    use std::path::PathBuf;
    use std::time::{Duration, SystemTime};

    use super::*;
    use crate::config::{DepsCheck, Host, Pattern};
    use crate::lease::{Group, Worktree};

    /// A scratch worktree under the system temp dir, removed on drop.
    struct Scratch(PathBuf);

    impl Scratch {
        fn new(name: &str) -> Self {
            let dir = std::env::temp_dir().join(format!("crumb-{name}-{}", std::process::id()));
            let _ = fs::remove_dir_all(&dir);
            fs::create_dir_all(&dir).unwrap();
            Self(dir)
        }

        /// Writes a file and backdates it by `age`.
        fn file(&self, relative: &str, contents: &str, age: Duration) {
            let path = self.0.join(relative);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(&path, contents).unwrap();
            File::options()
                .write(true)
                .open(&path)
                .unwrap()
                .set_modified(SystemTime::now() - age)
                .unwrap();
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    const HOUR: Duration = Duration::from_secs(3_600);

    fn ago(d: Duration) -> Timestamp {
        Timestamp::try_from(SystemTime::now() - d).unwrap()
    }

    #[test]
    fn finds_a_source_file_changed_after_the_start() {
        let tree = Scratch::new("changed");
        tree.file("src/old.ts", "", 2 * HOUR);
        tree.file("src/new.ts", "", Duration::ZERO);
        let watch = ["src".to_string()];
        assert_eq!(
            changed_since(&tree.0, &watch, ago(HOUR)).as_deref(),
            Some("src/new.ts")
        );
        assert_eq!(
            changed_since(&tree.0, &watch, ago(Duration::ZERO) + SKEW * 2),
            None
        );
    }

    #[test]
    fn ignores_dependencies_build_output_and_dot_directories() {
        let tree = Scratch::new("ignored");
        tree.file("src/app.ts", "", 2 * HOUR);
        tree.file("src/node_modules/x/index.js", "", Duration::ZERO);
        tree.file("packages/a/dist/index.js", "", Duration::ZERO);
        tree.file("packages/a/.turbo/log", "", Duration::ZERO);
        let watch = ["src".to_string(), "packages".to_string()];
        assert_eq!(changed_since(&tree.0, &watch, ago(HOUR)), None);
    }

    #[test]
    fn only_watched_paths_count() {
        let tree = Scratch::new("watched");
        tree.file("apps/web/page.tsx", "", Duration::ZERO);
        tree.file("apps/backend/tsdown.config.ts", "", Duration::ZERO);
        let backend = ["apps/backend/src".to_string()];
        assert_eq!(changed_since(&tree.0, &backend, ago(HOUR)), None);
        let config = ["apps/backend/tsdown.config.ts".to_string()];
        assert_eq!(
            changed_since(&tree.0, &config, ago(HOUR)).as_deref(),
            Some("apps/backend/tsdown.config.ts")
        );
        assert_eq!(changed_since(&tree.0, &[], ago(HOUR)), None);
    }

    #[test]
    fn reads_image_facts() {
        let facts = parse_image_facts(
            "2026-09-25T10:00:41.719941027Z\n\
             d064de469684a4704c60c99a63cd2f42c5be8a83fce36667be71e55b6a63602d  /app/pnpm-lock.yaml\n",
        );
        assert_eq!(
            facts.built,
            Some("2026-09-25T10:00:41.719941027Z".parse().unwrap())
        );
        assert_eq!(
            facts.lockfile_sha.as_deref(),
            Some("d064de469684a4704c60c99a63cd2f42c5be8a83fce36667be71e55b6a63602d")
        );
        assert_eq!(parse_image_facts("").lockfile_sha, None);
    }

    fn config() -> Config {
        Config {
            host: Host::Local,
            mutagen: false,
            label_keys: vec![],
            project: Pattern::parse("wt_{lease}").unwrap(),
            service: None,
            port: None,
            database: None,
            deps: Some(DepsCheck {
                lockfile: "pnpm-lock.yaml".into(),
                image_path: "/app/pnpm-lock.yaml".into(),
            }),
        }
    }

    fn lease(tree: &Scratch, started: Timestamp) -> Lease {
        Lease {
            name: "a".into(),
            group: Group::Running,
            state: State::Healthy,
            reasons: vec![],
            worktree: Some(Worktree {
                path: tree.0.to_string_lossy().into_owned(),
                exists: true,
            }),
            container: Some("wt_a-backend-1".into()),
            image: Some("sha256:abc".into()),
            watch: vec!["src".into()],
            changed_file: None,
            image_built: None,
            port: None,
            memory_bytes: None,
            restarts: 0,
            since: Some(started),
            sync: None,
            database: None,
        }
    }

    #[test]
    fn a_lockfile_the_image_does_not_have_is_deps_behind() {
        let tree = Scratch::new("deps");
        tree.file("pnpm-lock.yaml", "new", 2 * HOUR);
        let mut images = Images::default();
        images.insert(
            "sha256:abc",
            ImageFacts {
                built: Some(ago(3 * HOUR)),
                lockfile_sha: Some("0".repeat(64)),
            },
        );
        let mut leases = [lease(&tree, ago(HOUR))];
        apply(&config(), &images, &mut leases);
        assert_eq!(leases[0].reasons, [Reason::DepsBehind]);
        // Behind, not broken: it stays with the running leases.
        assert_eq!(leases[0].group, Group::Running);
        assert!(leases[0].image_built.is_some());
    }

    #[test]
    fn a_matching_lockfile_and_an_untouched_tree_add_nothing() {
        let tree = Scratch::new("clean");
        tree.file("pnpm-lock.yaml", "same", 2 * HOUR);
        let mut images = Images::default();
        images.insert(
            "sha256:abc",
            ImageFacts {
                built: None,
                lockfile_sha: file_sha256(&tree.0.join("pnpm-lock.yaml")),
            },
        );
        let mut leases = [lease(&tree, ago(HOUR))];
        apply(&config(), &images, &mut leases);
        assert!(leases[0].reasons.is_empty());
        assert!(images.missing(&config(), &leases).is_empty());
    }

    #[test]
    fn an_edit_after_the_start_is_a_pending_restart() {
        let tree = Scratch::new("pending");
        tree.file("src/app.ts", "", Duration::ZERO);
        let mut leases = [lease(&tree, ago(HOUR))];
        apply(&config(), &Images::default(), &mut leases);
        assert_eq!(leases[0].reasons, [Reason::RestartPending]);
        assert_eq!(leases[0].changed_file.as_deref(), Some("src/app.ts"));
        assert_eq!(
            Images::default().missing(&config(), &leases),
            ["sha256:abc"]
        );
    }
}
