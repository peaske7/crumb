//! `crumb init`: looks at the repository, asks at most three questions with
//! the detected answer preselected, and writes a commented crumb.toml.

use std::io::{BufRead, IsTerminal, Write};
use std::path::{Path, PathBuf};

use anyhow::{Result, bail};

/// What the repository and this machine suggest.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Detected {
    pub compose: Option<String>,
    pub service: Option<String>,
    pub port: Option<u16>,
    /// `ssh://host` from mutagen.yml or the SSH config, else local.
    pub host: Option<String>,
    pub mutagen_yml: bool,
    pub lockfile: Option<String>,
    pub migrations: Option<(String, &'static str)>,
    /// Env files with a URL to a local port: `(dir, key)`.
    pub env_urls: Vec<(String, String)>,
}

const SKIP: &[&str] = &["node_modules", "dist", "build", "target", ".git", "vendor"];

pub fn detect(repo: &Path) -> Detected {
    let mut found = Detected::default();
    let files = walk(repo, 3);
    let relative = |p: &Path| {
        p.strip_prefix(repo)
            .unwrap_or(p)
            .to_string_lossy()
            .into_owned()
    };

    let mut composes: Vec<&PathBuf> = files
        .iter()
        .filter(|p| {
            let name = p.file_name().unwrap_or_default().to_string_lossy();
            (name.starts_with("compose") || name.starts_with("docker-compose"))
                && (name.ends_with(".yml") || name.ends_with(".yaml"))
        })
        .collect();
    // A compose file made for worktrees wins, then the shallowest.
    composes.sort_by_key(|p| {
        let name = p.to_string_lossy();
        (
            !name.contains("worktree"),
            p.components().count(),
            name.into_owned(),
        )
    });
    if let Some(compose) = composes.first() {
        found.compose = Some(relative(compose));
        if let Ok(text) = std::fs::read_to_string(compose) {
            (found.service, found.port) = service_with_port(&text);
        }
    }

    let mutagen = repo.join("mutagen.yml");
    if mutagen.is_file() {
        found.mutagen_yml = true;
        if let Ok(text) = std::fs::read_to_string(&mutagen) {
            found.host = text.lines().find_map(|line| {
                let value = line.trim().strip_prefix("beta:")?.trim().trim_matches('"');
                let (host, _) = value.split_once(':')?;
                let host = host.rsplit('@').next()?;
                (!host.is_empty() && !host.starts_with('/')).then(|| format!("ssh://{host}"))
            });
        }
    }

    found.lockfile = [
        "pnpm-lock.yaml",
        "package-lock.json",
        "yarn.lock",
        "Cargo.lock",
        "uv.lock",
        "Gemfile.lock",
    ]
    .into_iter()
    .find(|name| repo.join(name).is_file())
    .map(str::to_string);

    found.migrations = files
        .iter()
        .filter_map(|p| p.parent())
        .filter(|dir| {
            let name = dir.file_name().unwrap_or_default().to_string_lossy();
            name == "migrations" || name == "migrate"
        })
        .find_map(|dir| {
            let tool = if dir.join("atlas.sum").is_file() {
                "atlas"
            } else if dir.ends_with("db/migrate") {
                "rails"
            } else {
                return None;
            };
            Some((relative(dir), tool))
        });

    for path in &files {
        let name = path.file_name().unwrap_or_default().to_string_lossy();
        if !name.starts_with(".env") || name.contains("example") || name.contains("production") {
            continue;
        }
        let Ok(text) = std::fs::read_to_string(path) else {
            continue;
        };
        for line in text.lines() {
            let Some((key, value)) = line.split_once('=') else {
                continue;
            };
            let value = value.trim().trim_matches(['"', '\'']);
            let local = ["http://localhost:", "http://127.0.0.1:"]
                .iter()
                .any(|prefix| value.starts_with(prefix));
            let dir = path.parent().map(&relative).unwrap_or_default();
            let entry = (dir, key.trim().to_string());
            if local && key.contains("API") && !found.env_urls.contains(&entry) {
                found.env_urls.push(entry);
            }
        }
    }
    found
}

/// Files under `root`, `depth` directories deep, skipping dependencies.
fn walk(root: &Path, depth: usize) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut stack = vec![(root.to_path_buf(), 0)];
    while let Some((dir, level)) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let name = entry.file_name().to_string_lossy().into_owned();
            let Ok(kind) = entry.file_type() else {
                continue;
            };
            if kind.is_dir() {
                if level < depth && !SKIP.contains(&name.as_str()) && !name.starts_with('.') {
                    stack.push((path, level + 1));
                }
            } else if kind.is_file() {
                out.push(path);
            }
        }
    }
    out.sort();
    out
}

/// The first service that publishes a port, and its container port, read
/// line by line from a compose file.
fn service_with_port(text: &str) -> (Option<String>, Option<u16>) {
    let mut in_services = false;
    let mut service: Option<String> = None;
    let mut first: Option<String> = None;
    let mut in_ports = false;
    for line in text.lines() {
        if line.trim_start().starts_with('#') || line.trim().is_empty() {
            continue;
        }
        let indent = line.len() - line.trim_start().len();
        if indent == 0 {
            in_services = line.starts_with("services:");
            continue;
        }
        if !in_services {
            continue;
        }
        if indent == 2 && line.trim_end().ends_with(':') {
            service = Some(line.trim().trim_end_matches(':').to_string());
            first.get_or_insert_with(|| service.clone().unwrap_or_default());
            in_ports = false;
            continue;
        }
        if indent == 4 {
            in_ports = line.trim() == "ports:";
            continue;
        }
        if in_ports && line.trim().starts_with('-') {
            let port = line
                .trim()
                .trim_start_matches('-')
                .trim()
                .trim_matches(['"', '\''])
                .rsplit(':')
                .next()
                .and_then(|p| p.split('/').next())
                .and_then(|p| p.parse().ok());
            if port.is_some() {
                return (service, port);
            }
        }
    }
    (first, None)
}

/// The answers to write, after questions.
#[derive(Debug, Clone)]
pub struct Answers {
    pub host: String,
    pub compose: Option<String>,
    pub service: Option<String>,
    pub project: String,
}

pub fn ask(detected: &Detected, repo: &Path, yes: bool) -> Result<Answers> {
    let slug = crate::worktree::slug(
        &repo
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| "app".into()),
    );
    let mut answers = Answers {
        host: detected.host.clone().unwrap_or_else(|| "local".into()),
        compose: detected.compose.clone(),
        service: detected.service.clone(),
        project: format!("{slug}_{{lease}}"),
    };
    if yes {
        return Ok(answers);
    }
    if !std::io::stdin().is_terminal() {
        bail!("crumb init asks questions; pass --yes to take the detected answers");
    }
    answers.host = prompt("Where do leases run? local or ssh://<host>", &answers.host)?;
    let compose = prompt("Compose file", answers.compose.as_deref().unwrap_or("none"))?;
    answers.compose = (compose != "none").then_some(compose);
    if answers.compose.is_some() {
        let service = prompt(
            "Service to wait for",
            answers.service.as_deref().unwrap_or("app"),
        )?;
        answers.service = Some(service);
    }
    Ok(answers)
}

fn prompt(question: &str, default: &str) -> Result<String> {
    eprint!("{question} [{default}]: ");
    std::io::stderr().flush()?;
    let mut line = String::new();
    std::io::stdin().lock().read_line(&mut line)?;
    let line = line.trim();
    Ok(if line.is_empty() { default } else { line }.to_string())
}

/// The commented crumb.toml.
pub fn render(detected: &Detected, answers: &Answers) -> String {
    let remote = answers.host != "local";
    let mut out = String::from(
        "# crumb: one backend per git worktree. `crumb` lists them; `crumb up` in a\n\
         # worktree starts its lease. Machine-specific values (another host) go in\n\
         # ~/.config/crumb/config.toml. https://github.com/peaske7/crumb\n",
    );
    out.push_str(&format!("host = \"{}\"\n", answers.host));
    if remote {
        out.push_str("\n[code]\nsync = \"mutagen\"\nroot = \"~/crumb\"\n");
        if detected.mutagen_yml {
            out.push_str(
                "# The same ignore list as your main Mutagen session.\nignore = \"mutagen.yml\"\n",
            );
        } else {
            out.push_str("ignore = [\"node_modules/\", \"dist/\", \".turbo/\"]\n");
        }
    }
    out.push_str("\n[runtime]\n");
    match &answers.compose {
        Some(compose) => {
            out.push_str(&format!("compose = \"{compose}\"\n"));
            out.push_str(&format!("project = \"{}\"\n", answers.project));
            if let Some(service) = &answers.service {
                out.push_str(&format!("service = \"{service}\"\n"));
            }
            if let Some(port) = detected.port {
                out.push_str(&format!("port = {port}\n"));
            }
            out.push_str(
                "# The compose file publishes the port as 127.0.0.1:${CRUMB_PORT}:<port>\n\
                 # and may use ${CRUMB_DATABASE} and ${CRUMB_LEASE}.\n",
            );
        }
        None => {
            out.push_str(&format!("project = \"{}\"\n", answers.project));
            out.push_str(
                "# No compose file: run a command in tmux instead.\n\
                 start = \"npm run dev -- --port {port}\"\n\
                 ready = \"http://127.0.0.1:{port}/\"\n",
            );
        }
    }
    out.push_str(&format!(
        "\n[ports]\nhost_base = {}\n",
        if remote { 8100 } else { 4100 }
    ));
    if remote {
        out.push_str("local_base = 18100\n");
    }
    out.push_str(
        "\n# [database]\n# name = \"app_{lease}\"\n# server = \"docker://postgres\"   # or a postgres:// URL\n# from = \"app_development\"         # copied for each lease\n# create = \"./scripts/db create {lease}\"  # or your own command\n",
    );
    let mut checks = Vec::new();
    if let (Some(lockfile), Some(_)) = (&detected.lockfile, &answers.compose) {
        checks.push(format!(
            "deps = {{ lockfile = \"{lockfile}\", image_path = \"/app/{lockfile}\" }}"
        ));
    }
    if let Some((dir, tool)) = &detected.migrations {
        let query = match *tool {
            "atlas" => "select max(version) from atlas_schema_revisions.atlas_schema_revisions",
            _ => "select max(version) from schema_migrations",
        };
        checks.push(format!(
            "schema = {{ migrations = \"{dir}\", query = \"{query}\" }}"
        ));
    }
    if !checks.is_empty() {
        out.push_str(&format!("\n[checks]\n{}\n", checks.join("\n")));
    }
    for (dir, key) in &detected.env_urls {
        let file = if dir.is_empty() {
            ".env.development.local".to_string()
        } else {
            format!("{dir}/.env.development.local")
        };
        out.push_str(&format!(
            "\n[[wire]]\nfile = \"{file}\"\nset.{key} = \"http://127.0.0.1:{{local_port}}\"\n"
        ));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_the_service_that_publishes_a_port() {
        let text = "services:\n  redis:\n    image: redis\n  backend:\n    image: x\n    ports:\n      - '127.0.0.1:${CRUMB_PORT:?}:8080'\n";
        assert_eq!(
            service_with_port(text),
            (Some("backend".into()), Some(8080))
        );
    }

    #[test]
    fn a_detected_remote_repo_renders_a_config_crumb_reads() {
        let detected = Detected {
            compose: Some("scripts/vps/compose.worktree.yml".into()),
            service: Some("backend".into()),
            port: Some(8080),
            host: Some("ssh://indigo".into()),
            mutagen_yml: true,
            lockfile: Some("pnpm-lock.yaml".into()),
            migrations: Some(("packages/db/migrations".into(), "atlas")),
            env_urls: vec![("apps/hub".into(), "VITE_LYMO_BACKEND_API_URL".into())],
        };
        let answers = Answers {
            host: "ssh://indigo".into(),
            compose: detected.compose.clone(),
            service: detected.service.clone(),
            project: "wt_{lease}".into(),
        };
        let text = render(&detected, &answers);
        let config = crate::config::Config::parse(&text).unwrap();
        assert!(config.mutagen);
        assert_eq!(config.service.as_deref(), Some("backend"));
        assert!(config.deps.is_some() && config.schema.is_some());
        assert_eq!(config.wires.len(), 1);
    }

    #[test]
    fn a_local_repo_without_compose_uses_a_process() {
        let answers = Answers {
            host: "local".into(),
            compose: None,
            service: None,
            project: "app_{lease}".into(),
        };
        let config = crate::config::Config::parse(&render(&Detected::default(), &answers)).unwrap();
        assert!(matches!(
            config.runtime,
            crate::config::Runtime::Process { .. }
        ));
    }
}
