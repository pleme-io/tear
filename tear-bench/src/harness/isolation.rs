use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process::Command;

pub const ROOT_ENV: &str = "TEARBENCH_ROOT";
pub const FORBID_ENV: &str = "TEARBENCH_FORBID";
pub const PATH_ENV: &str = "TEARBENCH_PATH";
pub const DEFAULT_PATH: &str = "/usr/bin:/bin:/usr/sbin:/sbin";

pub const GUARDED: [&str; 8] = [
    "HOME",
    "XDG_CONFIG_HOME",
    "XDG_STATE_HOME",
    "XDG_DATA_HOME",
    "XDG_RUNTIME_DIR",
    "XDG_CACHE_HOME",
    "TMPDIR",
    "KANSHOU_SOCKET_DIR",
];

const DIRS: [(&str, &str); 8] = [
    ("HOME", "home"),
    ("XDG_CONFIG_HOME", "config"),
    ("XDG_STATE_HOME", "state"),
    ("XDG_DATA_HOME", "data"),
    ("XDG_CACHE_HOME", "cache"),
    ("XDG_RUNTIME_DIR", "runtime"),
    ("TMPDIR", "tmp"),
    ("KANSHOU_SOCKET_DIR", "kanshou"),
];

pub fn env_for(dir: &Path, path_env: &str) -> io::Result<Vec<(String, String)>> {
    let mut out = Vec::new();
    for (key, sub) in DIRS {
        let d = dir.join(sub);
        fs::create_dir_all(&d)?;
        out.push((key.to_string(), d.to_string_lossy().into_owned()));
    }
    fs::create_dir_all(dir.join("config").join("tear"))?;
    for key in ["USER", "LOGNAME"] {
        if let Ok(v) = std::env::var(key) {
            out.push((key.to_string(), v));
        }
    }
    out.push(("PATH".into(), path_env.to_string()));
    out.push(("SHELL".into(), "/bin/sh".into()));
    out.push(("LANG".into(), "en_US.UTF-8".into()));
    Ok(out)
}

pub const LIVE_TEAR_DIRS: [&str; 3] = [".local/state/tear", ".local/share/tear", ".config/tear"];

pub fn resolved(p: &Path) -> PathBuf {
    let mut missing = Vec::new();
    let mut at = p.to_path_buf();
    loop {
        if let Ok(real) = fs::canonicalize(&at) {
            return missing.iter().rev().fold(real, |acc, c| acc.join(c));
        }
        match (at.file_name().map(ToOwned::to_owned), at.parent()) {
            (Some(name), Some(parent)) => {
                missing.push(name);
                at = parent.to_path_buf();
            }
            _ => return p.to_path_buf(),
        }
    }
}

pub fn root_allowed(root: &Path, forbid: &Path) -> Result<(), String> {
    if forbid.as_os_str().is_empty() {
        return Ok(());
    }
    let root = resolved(root);
    let forbid = resolved(forbid);
    if forbid.starts_with(&root) {
        return Err(format!(
            "the run root {} is {} or holds it",
            root.display(),
            forbid.display()
        ));
    }
    for live in LIVE_TEAR_DIRS.map(|d| forbid.join(d)) {
        if live.starts_with(&root) || root.starts_with(&live) {
            return Err(format!(
                "the run root {} overlaps tear's live directory {}",
                root.display(),
                live.display()
            ));
        }
    }
    Ok(())
}

pub fn relaunch(root: &Path, path_env: &str, forbid: &Path, args: &[String]) -> io::Result<i32> {
    let root = if root.is_absolute() {
        root.to_path_buf()
    } else {
        std::env::current_dir()?.join(root)
    };
    root_allowed(&root, forbid).map_err(io::Error::other)?;
    fs::create_dir_all(&root)?;
    let env = env_for(&root.join("iso").join("bench"), path_env)?;
    let mut cmd = Command::new(std::env::current_exe()?);
    cmd.args(args)
        .current_dir(&root)
        .env_clear()
        .envs(env)
        .env(ROOT_ENV, &root)
        .env(PATH_ENV, path_env)
        .env(FORBID_ENV, forbid);
    let st = cmd.status()?;
    Ok(st.code().unwrap_or(1))
}

pub fn guard() -> Result<PathBuf, String> {
    let root = std::env::var(ROOT_ENV).map_err(|_| format!("{ROOT_ENV} is not set"))?;
    let root = fs::canonicalize(&root).map_err(|e| format!("{ROOT_ENV}={root}: {e}"))?;
    for key in GUARDED {
        let v = std::env::var(key).unwrap_or_default();
        let resolved = fs::canonicalize(&v).unwrap_or_else(|_| PathBuf::from(&v));
        if v.is_empty() || !resolved.starts_with(&root) {
            return Err(format!("{key}={v:?} is not under {}", root.display()));
        }
    }
    let cwd = std::env::current_dir().map_err(|e| format!("cwd: {e}"))?;
    let cwd = fs::canonicalize(&cwd).map_err(|e| format!("cwd: {e}"))?;
    if cwd != root {
        return Err(format!("cwd {} is not {}", cwd.display(), root.display()));
    }
    let forbid = std::env::var_os(FORBID_ENV)
        .map(PathBuf::from)
        .unwrap_or_default();
    root_allowed(&root, &forbid)?;
    Ok(root)
}

fn paths_in(line: &str) -> impl Iterator<Item = &Path> {
    line.char_indices()
        .filter(|(i, c)| *c == '/' && (*i == 0 || line[..*i].ends_with(char::is_whitespace)))
        .map(|(i, _)| Path::new(&line[i..]))
}

#[must_use]
pub fn violations(listing: &str, root: &Path, forbid: &Path) -> Vec<String> {
    if forbid.as_os_str().is_empty() || forbid == Path::new("/") {
        return Vec::new();
    }
    listing
        .lines()
        .filter(|l| paths_in(l).any(|p| p.starts_with(forbid) && !p.starts_with(root)))
        .map(str::to_string)
        .collect()
}

pub fn open_files(pid: i32) -> io::Result<String> {
    if cfg!(target_os = "linux") {
        let mut out = String::new();
        for e in fs::read_dir(format!("/proc/{pid}/fd"))?.flatten() {
            if let Ok(t) = fs::read_link(e.path()) {
                out.push_str(&t.to_string_lossy());
                out.push('\n');
            }
        }
        return Ok(out);
    }
    let o = Command::new("lsof")
        .args(["-n", "-P", "-p", &pid.to_string()])
        .output()?;
    if !o.status.success() && o.stdout.is_empty() {
        return Err(io::Error::other(format!(
            "lsof -p {pid} exited {} with no listing",
            o.status
        )));
    }
    Ok(String::from_utf8_lossy(&o.stdout).into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_file_under_the_forbidden_root_is_a_violation_unless_it_is_ours() {
        let listing = "tear 1 u txt REG /nix/store/x/bin/tear\ntear 1 u 5 REG /Users/op/.local/state/tear/x\ntear 1 u 6 REG /Users/op/bench/run/a\n";
        let v = violations(
            listing,
            Path::new("/Users/op/bench"),
            Path::new("/Users/op"),
        );
        assert_eq!(v.len(), 1);
        assert!(v[0].contains(".local/state"));
        assert!(violations(listing, Path::new("/x"), Path::new("")).is_empty());
    }

    #[test]
    fn a_sibling_that_shares_the_root_as_a_prefix_is_still_a_violation() {
        let listing = "tear 1 u 5 REG /Users/op/bench2/run/a\ntear 1 u 6 REG /private/Users/op/x\ntear 1 u 7 REG /Users/op/bench/run/b\n";
        let v = violations(
            listing,
            Path::new("/Users/op/bench"),
            Path::new("/Users/op"),
        );
        assert_eq!(v, vec!["tear 1 u 5 REG /Users/op/bench2/run/a".to_string()]);
    }

    #[test]
    fn a_root_that_is_or_holds_the_home_or_overlaps_tears_live_dirs_is_refused() {
        let home = Path::new("/nonexistent-home/op");
        assert!(root_allowed(home, home).is_err());
        assert!(root_allowed(Path::new("/nonexistent-home"), home).is_err());
        assert!(root_allowed(Path::new("/"), home).is_err());
        assert!(root_allowed(Path::new("/nonexistent-home/op/.local"), home).is_err());
        assert!(
            root_allowed(
                Path::new("/nonexistent-home/op/.local/state/tear/bench"),
                home
            )
            .is_err()
        );
        assert!(root_allowed(Path::new("/nonexistent-home/op/.config/tear"), home).is_err());
        assert!(root_allowed(Path::new("/nonexistent-home/op/bench"), home).is_ok());
        assert!(root_allowed(Path::new("/nonexistent-home/op2"), home).is_ok());
        assert!(root_allowed(Path::new("/tmp/tearbench"), home).is_ok());
    }
}
