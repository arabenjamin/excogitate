//! The command line, for any binary built on the runtime. The stock binary
//! and a deployment's own one, with its own plugins compiled in, both call
//! [`main`], so neither needs a change to the core to add a plugin.

use std::process::ExitCode;
use std::sync::Arc;

use plugin_api::Plugin;

use crate::{RunError, run_connect, run_server, run_stdio};

/// What a binary is: the name that prefixes its settings, its version, and
/// the plugins compiled into it.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct Program {
    /// The binary's name, as in `env!("CARGO_BIN_NAME")`. Settings are read
    /// from variables that start with it in capitals.
    pub name: &'static str,
    /// Its version, as in `env!("CARGO_PKG_VERSION")`.
    pub version: &'static str,
    /// The plugins it was built with. `plugins.yaml` still decides what each
    /// may use.
    pub plugins: Vec<Arc<dyn Plugin>>,
}

impl Program {
    /// A binary with no plugins compiled in. Pass `env!("CARGO_BIN_NAME")`
    /// and `env!("CARGO_PKG_VERSION")`.
    #[must_use]
    pub const fn new(name: &'static str, version: &'static str) -> Self {
        Self {
            name,
            version,
            plugins: Vec::new(),
        }
    }

    /// The same binary with one more plugin compiled in.
    #[must_use]
    pub fn with_plugin(mut self, plugin: Arc<dyn Plugin>) -> Self {
        self.plugins.push(plugin);
        self
    }

    /// The same binary with these plugins compiled in as well.
    #[must_use]
    pub fn with_plugins(mut self, plugins: impl IntoIterator<Item = Arc<dyn Plugin>>) -> Self {
        self.plugins.extend(plugins);
        self
    }

    /// The line printed for `version`.
    #[must_use]
    pub fn banner(&self) -> String {
        format!(
            "{} {} ({} plugins compiled in)",
            self.name,
            self.version,
            self.plugins.len()
        )
    }
}

/// Runs the command line: `serve` (MCP over stdio), `server` (HTTP, workers
/// and the scheduler), `connect [url]`, or `version`.
pub async fn main(program: Program) -> ExitCode {
    let arguments: Vec<String> = std::env::args().skip(1).collect();
    let arguments: Vec<&str> = arguments.iter().map(String::as_str).collect();
    let result = match arguments.as_slice() {
        [] | ["--version" | "version"] => {
            println!("{}", program.banner());
            return ExitCode::SUCCESS;
        }
        ["serve"] => run_stdio(program.name, program.version, &program.plugins).await,
        ["server"] => run_server(program.name, program.version, &program.plugins).await,
        ["connect"] => run_connect(program.name, None).await,
        ["connect", url] => run_connect(program.name, Some(url)).await,
        _ => {
            eprintln!(
                "usage: {} [serve | server | connect [url] | version]",
                program.name
            );
            return ExitCode::from(2);
        }
    };
    exit_code(result)
}

fn exit_code(result: Result<(), RunError>) -> ExitCode {
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("error: {error}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_banner_names_the_version_and_the_plugins() {
        let program = Program::new("app", "9.9.9");
        assert_eq!(program.banner(), "app 9.9.9 (0 plugins compiled in)");
    }
}
