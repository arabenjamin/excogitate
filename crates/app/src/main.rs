//! The command-line entry point. For now it only reports its version; the
//! server arrives in Phase 2a.

/// The version this binary was built as, from Cargo.
const VERSION: &str = env!("CARGO_PKG_VERSION");

fn main() {
    println!("{}", banner());
}

/// The line printed on start.
fn banner() -> String {
    format!("{} {VERSION}", env!("CARGO_BIN_NAME"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn banner_names_the_version() {
        assert!(banner().ends_with(VERSION));
    }
}
