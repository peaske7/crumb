//! The CLI's output: cargo-style steps on stderr, and confirmations.

use std::io::{BufRead, IsTerminal, Write};

use anyhow::{Result, bail};

use crate::ops::Progress;

/// Prints each step with its verb right-aligned, the way cargo does.
pub struct Printer {
    color: bool,
}

impl Printer {
    pub fn new() -> Self {
        Self {
            color: std::io::stderr().is_terminal() && std::env::var_os("NO_COLOR").is_none(),
        }
    }

    fn paint(&self, text: &str, code: &str) -> String {
        if self.color {
            format!("\x1b[{code}m{text}\x1b[0m")
        } else {
            text.to_string()
        }
    }
}

impl Progress for Printer {
    fn step(&self, verb: &str, text: &str) {
        let code = if verb == "Failed" { "1;31" } else { "1;32" };
        eprintln!("{} {text}", self.paint(&format!("{verb:>12}"), code));
    }

    fn detail(&self, text: &str) {
        eprintln!("{:>12} {}", "", self.paint(text, "2"));
    }
}

/// Asks a yes/no question on the terminal. Without one, the answer is no.
pub fn confirm(question: &str) -> Result<bool> {
    if !std::io::stdin().is_terminal() {
        bail!("{question} needs a terminal to confirm; pass --yes to skip the question");
    }
    eprint!("{question} [y/N] ");
    std::io::stderr().flush()?;
    let mut answer = String::new();
    std::io::stdin().lock().read_line(&mut answer)?;
    Ok(matches!(answer.trim(), "y" | "Y" | "yes"))
}

/// Asks the user to type `expected` back, for changes that lose data.
pub fn confirm_typed(prompt: &str, expected: &str) -> Result<bool> {
    if !std::io::stdin().is_terminal() {
        bail!("{prompt}: needs a terminal; pass --confirm {expected} instead");
    }
    eprint!("{prompt}: ");
    std::io::stderr().flush()?;
    let mut answer = String::new();
    std::io::stdin().lock().read_line(&mut answer)?;
    Ok(answer.trim() == expected)
}
