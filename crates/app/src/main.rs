//! The stock binary: the runtime with the plugins this build's features
//! name. A deployment with plugins of its own writes a binary just like this
//! one, in its own repo, and lists them in [`compiled_plugins`].

use std::process::ExitCode;
use std::sync::Arc;

use plugin_api::Plugin;
use runtime::Program;

#[tokio::main]
async fn main() -> ExitCode {
    let program = Program::new(env!("CARGO_BIN_NAME"), env!("CARGO_PKG_VERSION"))
        .with_plugins(compiled_plugins());
    runtime::main(program).await
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plugins_are_compiled_in_only_by_feature() {
        #[allow(unused_mut, reason = "one block per plugin feature")]
        let mut expected = 0;
        #[cfg(feature = "plugin-example")]
        {
            expected += 1;
        }
        assert_eq!(compiled_plugins().len(), expected);
    }
}
