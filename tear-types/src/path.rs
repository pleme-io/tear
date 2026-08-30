//! Path-string helpers shared across tear crates. Kept tiny and
//! dependency-free so every consumer can reuse without pulling
//! `shellexpand` or other heavy crates.

/// Pure helper: expand a leading `~/` to `<home>/<rest>`. Returns
/// the input unchanged when the string doesn't start with `~/` or
/// when `home` is `None`. The pure shape keeps it testable without
/// touching the process environment.
#[must_use]
pub fn expand_tilde_with_home(s: &str, home: Option<&str>) -> String {
    if let Some(rest) = s.strip_prefix("~/") {
        if let Some(h) = home {
            return format!("{h}/{rest}");
        }
    }
    s.to_string()
}

/// Convenience: expand a leading `~/` to `$HOME/`. Returns the
/// input unchanged when the string does not start with `~/` or
/// when `HOME` is not set in the environment. Used by every tear
/// surface that takes an operator-supplied path string (audit log
/// path, recording dir, MCP socket, etc.).
#[must_use]
pub fn expand_tilde(s: &str) -> String {
    let home = std::env::var("HOME").ok();
    expand_tilde_with_home(s, home.as_deref())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn expand_tilde_with_home_set_replaces_prefix() {
        assert_eq!(
            expand_tilde_with_home("~/notes/x", Some("/tmp/fake-home")),
            "/tmp/fake-home/notes/x"
        );
    }

    #[test]
    fn expand_tilde_passes_through_paths_without_prefix() {
        assert_eq!(expand_tilde_with_home("/abs", Some("/h")), "/abs");
        assert_eq!(expand_tilde_with_home("rel", Some("/h")), "rel");
        // A bare `~` (no slash) is intentionally NOT expanded — the
        // spec is "expand a leading `~/`".
        assert_eq!(expand_tilde_with_home("~", Some("/h")), "~");
    }

    #[test]
    fn expand_tilde_returns_input_when_home_missing() {
        assert_eq!(expand_tilde_with_home("~/something", None), "~/something");
    }

    #[test]
    fn expand_tilde_env_wrapper_runs_without_panicking() {
        // We can't assert the result (depends on the runner's
        // $HOME) but we can prove the call path itself works.
        let _ = expand_tilde("~/x");
        let _ = expand_tilde("/abs");
    }
}
