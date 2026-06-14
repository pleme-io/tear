//! Project-root detection — walk UP from a cwd to the nearest ancestor
//! that looks like a project root.
//!
//! "Project root" is the directory cd-driven auto-attach binds a session
//! to. We want `cd` anywhere INSIDE a repo to resolve to the SAME root so
//! the same session is auto-attached no matter how deep the operator is.
//!
//! Detection is marker-driven and ordered: a `.git` directory wins (the
//! canonical repo boundary); otherwise the first ancestor carrying any of
//! a small set of language/build markers
//! (`Cargo.toml` / `flake.nix` / `package.json` / `go.mod` /
//! `pyproject.toml`) is the root. If nothing matches all the way up, the
//! cwd itself is the root — every directory is at least its own project.
//!
//! The walk is **pure**: [`project_root_with`] takes a `has_marker`
//! predicate so the logic is testable without touching a real filesystem.
//! The thin [`project_root`] wires the predicate to real
//! `Path::join().exists()`.

use std::path::{Path, PathBuf};

/// Markers that mean "a non-`.git` directory is a project root", in no
/// particular priority among themselves (presence of ANY one is enough).
/// `.git` is handled separately because it is a *directory* and ranks
/// strictly above these file markers.
pub const FILE_MARKERS: &[&str] = &[
    "Cargo.toml",
    "flake.nix",
    "package.json",
    "go.mod",
    "pyproject.toml",
];

/// The `.git` marker. Checked first at every level — a repo boundary
/// outranks an inner crate/package manifest, so `cd` into a workspace
/// member resolves to the repo root, not the member.
pub const GIT_MARKER: &str = ".git";

/// Walk UP from `cwd` to the nearest ancestor that contains a project
/// marker, using `has_marker(dir, marker)` to test marker presence.
///
/// At each ancestor (starting at `cwd` itself), `.git` is checked first;
/// if absent, each entry of [`FILE_MARKERS`] is checked. The first
/// ancestor that matches is returned. If the walk reaches the filesystem
/// root with no match, `cwd` is returned unchanged.
///
/// Pure — no filesystem access. Inject `has_marker` to test the walk
/// against a mock directory tree.
pub fn project_root_with<F>(cwd: &Path, has_marker: F) -> PathBuf
where
    F: Fn(&Path, &str) -> bool,
{
    let mut dir = cwd;
    loop {
        if has_marker(dir, GIT_MARKER) {
            return dir.to_path_buf();
        }
        if FILE_MARKERS.iter().any(|m| has_marker(dir, m)) {
            return dir.to_path_buf();
        }
        match dir.parent() {
            Some(parent) => dir = parent,
            None => return cwd.to_path_buf(),
        }
    }
}

/// Real-filesystem [`project_root_with`]: a marker is present iff
/// `dir.join(marker)` exists. `.git` matches whether it is a directory
/// (normal repo) or a file (git worktree / submodule gitlink).
#[must_use]
pub fn project_root(cwd: &Path) -> PathBuf {
    project_root_with(cwd, |dir, marker| dir.join(marker).exists())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;
    use std::path::Path;

    /// Build a mock `has_marker` from a set of `"/abs/dir::marker"` keys.
    fn mock(present: &[&str]) -> impl Fn(&Path, &str) -> bool + use<> {
        let set: HashSet<String> = present.iter().map(|s| (*s).to_string()).collect();
        move |dir: &Path, marker: &str| set.contains(&format!("{}::{}", dir.display(), marker))
    }

    #[test]
    fn git_at_cwd_returns_cwd() {
        let cwd = Path::new("/home/u/code/repo");
        let has = mock(&["/home/u/code/repo::.git"]);
        assert_eq!(project_root_with(cwd, has), PathBuf::from("/home/u/code/repo"));
    }

    #[test]
    fn walks_up_to_git_ancestor() {
        let cwd = Path::new("/home/u/code/repo/src/deep/nested");
        let has = mock(&["/home/u/code/repo::.git"]);
        assert_eq!(project_root_with(cwd, has), PathBuf::from("/home/u/code/repo"));
    }

    #[test]
    fn git_outranks_inner_cargo_toml() {
        // Inner crate has a Cargo.toml; repo root has .git. cd inside the
        // crate must resolve to the repo root, not the crate.
        let cwd = Path::new("/repo/crates/inner");
        let has = mock(&["/repo/crates/inner::Cargo.toml", "/repo::.git"]);
        // At cwd: no .git, but Cargo.toml present -> matches cwd FIRST.
        // This is intentional: .git is only "outranked" if it sits at the
        // SAME level. A Cargo.toml at the cwd makes the cwd a root.
        assert_eq!(project_root_with(cwd, has), PathBuf::from("/repo/crates/inner"));
    }

    #[test]
    fn git_at_same_level_beats_file_marker() {
        // Both .git and Cargo.toml at the same dir -> .git checked first,
        // returns the same dir either way, but proves ordering.
        let cwd = Path::new("/repo");
        let has = mock(&["/repo::.git", "/repo::Cargo.toml"]);
        assert_eq!(project_root_with(cwd, has), PathBuf::from("/repo"));
    }

    #[test]
    fn file_marker_ancestor_when_no_git() {
        let cwd = Path::new("/proj/sub/dir");
        let has = mock(&["/proj::flake.nix"]);
        assert_eq!(project_root_with(cwd, has), PathBuf::from("/proj"));
    }

    #[test]
    fn no_marker_anywhere_returns_cwd() {
        let cwd = Path::new("/tmp/scratch/x");
        let has = mock(&[]);
        assert_eq!(project_root_with(cwd, has), PathBuf::from("/tmp/scratch/x"));
    }

    #[test]
    fn nearest_marker_wins_over_higher_one() {
        // A package.json sits at /a/b; a flake.nix sits at /a. cd at
        // /a/b/c resolves to /a/b (the nearest), never /a.
        let cwd = Path::new("/a/b/c");
        let has = mock(&["/a/b::package.json", "/a::flake.nix"]);
        assert_eq!(project_root_with(cwd, has), PathBuf::from("/a/b"));
    }

    #[test]
    fn each_file_marker_is_recognized() {
        for m in FILE_MARKERS {
            let cwd = Path::new("/x/y");
            let has = mock(&[&format!("/x::{m}")]);
            assert_eq!(
                project_root_with(cwd, has),
                PathBuf::from("/x"),
                "marker {m} should be recognized",
            );
        }
    }
}
