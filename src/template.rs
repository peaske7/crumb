//! `{name}` placeholders in config values: `{lease}`, `{port}`,
//! `{local_port}`, `{worktree}`, `{database}`, and any field a create
//! command printed, such as `{database.url}`.

use std::collections::BTreeMap;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Vars(BTreeMap<String, String>);

impl Vars {
    pub fn set(&mut self, key: &str, value: impl Into<String>) -> &mut Self {
        self.0.insert(key.to_string(), value.into());
        self
    }

    pub fn extend(&mut self, other: Vars) {
        self.0.extend(other.0);
    }

    #[cfg(test)]
    pub fn get(&self, key: &str) -> Option<&str> {
        self.0.get(key).map(String::as_str)
    }

    /// Replaces each known `{key}`; unknown placeholders stay as written, so
    /// a missing value is visible rather than silently empty.
    pub fn render(&self, template: &str) -> String {
        let mut out = String::with_capacity(template.len());
        let mut rest = template;
        while let Some(open) = rest.find('{') {
            out.push_str(&rest[..open]);
            let after = &rest[open + 1..];
            match after.find('}') {
                Some(close) if self.0.contains_key(&after[..close]) => {
                    out.push_str(&self.0[&after[..close]]);
                    rest = &after[close + 1..];
                }
                _ => {
                    out.push('{');
                    rest = after;
                }
            }
        }
        out.push_str(rest);
        out
    }

    /// Placeholders `template` uses that have no value. `${VAR}` is the
    /// shell's, not crumb's.
    pub fn missing(&self, template: &str) -> Vec<String> {
        let mut missing = Vec::new();
        let mut rest = template;
        while let Some(open) = rest.find('{') {
            let shell = rest[..open].ends_with('$');
            let after = &rest[open + 1..];
            let Some(close) = after.find('}') else { break };
            let key = &after[..close];
            let plain = !key.is_empty()
                && key
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '.'));
            if plain && !shell && !self.0.contains_key(key) {
                missing.push(key.to_string());
            }
            rest = &after[close + 1..];
        }
        missing
    }

    /// The values as `CRUMB_*` environment variables for commands:
    /// `local_port` becomes `CRUMB_LOCAL_PORT`, `database.url` becomes
    /// `CRUMB_DATABASE_URL`.
    pub fn env(&self) -> Vec<(String, String)> {
        self.0
            .iter()
            .map(|(key, value)| {
                let name: String = key
                    .chars()
                    .map(|c| match c {
                        '.' => '_',
                        c => c.to_ascii_uppercase(),
                    })
                    .collect();
                (format!("CRUMB_{name}"), value.clone())
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn vars() -> Vars {
        let mut vars = Vars::default();
        vars.set("lease", "lym_1119")
            .set("local_port", "18107")
            .set("database.url", "postgres://x/y");
        vars
    }

    #[test]
    fn renders_known_placeholders_and_keeps_the_rest() {
        assert_eq!(
            vars().render("http://127.0.0.1:{local_port}/{lease} {nope} ${HOME} {"),
            "http://127.0.0.1:18107/lym_1119 {nope} ${HOME} {"
        );
        assert_eq!(vars().render("{database.url}"), "postgres://x/y");
    }

    #[test]
    fn lists_missing_placeholders() {
        assert_eq!(vars().missing("{lease} {port} ${HOME}"), ["port"]);
    }

    #[test]
    fn env_names() {
        let env = vars().env();
        assert!(env.contains(&("CRUMB_LOCAL_PORT".into(), "18107".into())));
        assert!(env.contains(&("CRUMB_DATABASE_URL".into(), "postgres://x/y".into())));
    }
}
