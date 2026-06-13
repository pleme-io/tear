//! [`SpawnEnv`] — the typed seam an embedder (mado) uses to stamp its
//! OWN capability env + working directory onto every child PTY tear
//! spawns, applied AFTER the backend's inherited + fallback env.
//!
//! **Why this exists** (operator report 2026-06-12: "vim is grey +
//! wrong font in the embedded-tear default window"): tear-core's
//! `InProcess::spawn_pty_for` inherits the daemon/process env and
//! stamps conservative fallbacks (`TERM=xterm-256color`, no
//! `TERMINFO`). The embedder (mado) advertises a richer capability set
//! (`TERM=xterm-ghostty` + a vendored `TERMINFO` + `COLORTERM`) that
//! the local-PTY path already projected — but the embedded-tear spawn
//! had no way to push those through, so vim there saw no truecolor
//! (grey) + the wrong terminfo. `SpawnEnv` is that channel: the
//! embedder hands tear a typed override set, tear applies it AFTER the
//! inherited env so the embedder's `TERM` wins over the fallback, and
//! also stamps `PWD` consistently with the cwd (so a child shell can
//! never inherit a stale parent `PWD`).
//!
//! The override is OURS, not the backend's: tear does not invent these
//! values. It carries the embedder's intent verbatim. This keeps
//! tear-core agnostic about WHAT capabilities the embedder advertises
//! while still letting the embedder be the source of truth.

/// A typed env + cwd override an embedder stamps on every child PTY.
/// Applied by the spawn backend AFTER the inherited + fallback env, so
/// each `(key, value)` here OVERRIDES whatever the backend defaulted.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SpawnEnv {
    /// Env pairs that override the backend's inherited/fallback env.
    /// Order is preserved; later duplicates win (the backend's `env`
    /// vec is a positional list `CommandBuilder` walks in order).
    pub overrides: Vec<(String, String)>,
    /// Working directory for the child. When `Some`, the spawn backend
    /// also stamps `PWD=<dir>` so a child shell's `$PWD` matches its
    /// real cwd (the cwd-handshake hygiene). `None` leaves the
    /// backend's cwd untouched.
    pub cwd: Option<String>,
}

impl SpawnEnv {
    /// An empty override — the backend's inherited + fallback env is
    /// used verbatim (the pre-seam behaviour).
    #[must_use]
    pub fn none() -> Self {
        Self::default()
    }

    /// Build from typed env pairs (the embedder's capability
    /// projection) with no cwd override.
    #[must_use]
    pub fn from_overrides(overrides: Vec<(String, String)>) -> Self {
        Self {
            overrides,
            cwd: None,
        }
    }

    /// Set the working directory the child spawns in (and the `PWD`
    /// the backend stamps to match it).
    #[must_use]
    pub fn with_cwd(mut self, cwd: Option<String>) -> Self {
        self.cwd = cwd;
        self
    }

    /// Whether this override carries anything — an empty override lets
    /// the backend skip the apply pass entirely.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.overrides.is_empty() && self.cwd.is_none()
    }

    /// Apply this override onto a backend-built `env` vec IN PLACE:
    /// every override key replaces an existing entry (or pushes a new
    /// one), then `PWD` is reconciled with the cwd — stamped when a
    /// cwd is set, removed when it is not, so a child can never inherit
    /// a stale parent `PWD`. This is the ONE place the apply order is
    /// defined; both the in-process and daemon backends call it.
    pub fn apply_to(&self, env: &mut Vec<(String, String)>) {
        for (k, v) in &self.overrides {
            if let Some(slot) = env.iter_mut().find(|(ek, _)| ek == k) {
                slot.1.clone_from(v);
            } else {
                env.push((k.clone(), v.clone()));
            }
        }
        // PWD hygiene: a child shell trusts inherited PWD. Stamp it to
        // the real cwd when we have one; otherwise strip any inherited
        // PWD so a stale parent PWD can never leak (cwd handshake,
        // operator report 2026-06-12).
        match &self.cwd {
            Some(dir) => {
                if let Some(slot) = env.iter_mut().find(|(k, _)| k == "PWD") {
                    slot.1.clone_from(dir);
                } else {
                    env.push(("PWD".to_owned(), dir.clone()));
                }
            }
            None => env.retain(|(k, _)| k != "PWD"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::SpawnEnv;

    #[test]
    fn override_replaces_an_existing_key() {
        let mut env = vec![
            ("TERM".to_owned(), "xterm-256color".to_owned()),
            ("PATH".to_owned(), "/usr/bin".to_owned()),
        ];
        SpawnEnv::from_overrides(vec![("TERM".to_owned(), "xterm-ghostty".to_owned())])
            .apply_to(&mut env);
        assert_eq!(
            env.iter().find(|(k, _)| k == "TERM").map(|(_, v)| v.as_str()),
            Some("xterm-ghostty"),
            "the embedder's TERM must override the backend fallback"
        );
        // PATH untouched; no duplicate TERM.
        assert_eq!(env.iter().filter(|(k, _)| k == "TERM").count(), 1);
    }

    #[test]
    fn override_pushes_a_missing_key() {
        let mut env = vec![("PATH".to_owned(), "/usr/bin".to_owned())];
        SpawnEnv::from_overrides(vec![("COLORTERM".to_owned(), "truecolor".to_owned())])
            .apply_to(&mut env);
        assert_eq!(
            env.iter()
                .find(|(k, _)| k == "COLORTERM")
                .map(|(_, v)| v.as_str()),
            Some("truecolor")
        );
    }

    #[test]
    fn cwd_some_stamps_pwd_to_match() {
        let mut env = vec![("PWD".to_owned(), "/stale/parent".to_owned())];
        SpawnEnv::none()
            .with_cwd(Some("/real/child".to_owned()))
            .apply_to(&mut env);
        assert_eq!(
            env.iter().find(|(k, _)| k == "PWD").map(|(_, v)| v.as_str()),
            Some("/real/child"),
            "a set cwd must overwrite a stale inherited PWD"
        );
    }

    #[test]
    fn cwd_none_strips_any_inherited_pwd() {
        let mut env = vec![("PWD".to_owned(), "/stale/parent".to_owned())];
        SpawnEnv::none().apply_to(&mut env);
        assert!(
            !env.iter().any(|(k, _)| k == "PWD"),
            "no cwd → no inherited PWD may leak to the child"
        );
    }

    #[test]
    fn empty_override_is_a_noop_except_pwd_strip() {
        // The pre-seam env had no PWD; an empty SpawnEnv leaves it
        // exactly as-is.
        let mut env = vec![("TERM".to_owned(), "xterm-256color".to_owned())];
        let before = env.clone();
        SpawnEnv::none().apply_to(&mut env);
        assert_eq!(env, before);
    }
}
