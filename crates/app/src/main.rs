//! The command-line entry point. For now it only reports its version and
//! compiled-in plugins; the server arrives in Phase 2a.

use std::sync::Arc;

use plugin_api::Plugin;

/// The version this binary was built as, from Cargo.
const VERSION: &str = env!("CARGO_PKG_VERSION");

fn main() {
    println!("{}", banner());
}

/// The plugins this build was compiled with, one cargo feature each.
#[allow(
    unused_mut,
    clippy::vec_init_then_push,
    reason = "one line per plugin feature"
)]
fn compiled_plugins() -> Vec<Arc<dyn Plugin>> {
    let mut plugins: Vec<Arc<dyn Plugin>> = Vec::new();
    #[cfg(feature = "plugin-example")]
    plugins.push(Arc::new(plugin_example::ExamplePlugin));
    plugins
}

/// The line printed on start.
fn banner() -> String {
    format!(
        "{} {VERSION} ({} plugins compiled in)",
        env!("CARGO_BIN_NAME"),
        compiled_plugins().len()
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn banner_names_the_version() {
        assert!(banner().contains(VERSION));
    }

    #[test]
    fn plugins_are_compiled_in_only_by_feature() {
        let expected = usize::from(cfg!(feature = "plugin-example"));
        assert_eq!(compiled_plugins().len(), expected);
    }
}
