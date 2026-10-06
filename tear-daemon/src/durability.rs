use std::io;
use std::path::{Path, PathBuf};

use makimono::{JournalBounds, Store};
use tamotsu::HoldProgram;
use tear_config::{SessionDurability, SessionsConfig};
use tear_core::{Durable, InProcess, RestoreReport};
use tracing::{info, warn};

pub const HOLD_SUBCOMMAND: &str = "hold";

pub fn durable_from_config(
    sessions: &SessionsConfig,
    socket_path: Option<&Path>,
) -> io::Result<Option<Durable>> {
    if sessions.durability != SessionDurability::Held {
        return Ok(None);
    }
    let store_root = match sessions.store_dir.as_deref().map(PathBuf::from) {
        Some(p) if p.is_absolute() => p,
        Some(p) => {
            warn!(path = %p.display(), "sessions.store_dir is relative and was ignored");
            default_store_root()
        }
        None => default_store_root(),
    };
    let holder_dir = socket_path
        .and_then(Path::parent)
        .map_or_else(|| store_root.join("h"), |dir| dir.join("h"));
    let program = match sessions.holder_program.as_deref() {
        Some([program, prefix @ ..]) => HoldProgram {
            program: PathBuf::from(program),
            prefix: prefix.to_vec(),
        },
        Some([]) | None => {
            HoldProgram::current_exe_subcommand(HOLD_SUBCOMMAND).ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::NotFound,
                    "cannot resolve the running tear binary to launch holders from",
                )
            })?
        }
    };
    let j = &sessions.journal;
    Ok(Some(Durable {
        store: Store::open(store_root)?,
        holder_dir,
        program,
        bounds: JournalBounds {
            max_bytes: j.max_bytes_per_pane,
            segment_bytes: j.segment_bytes,
            fsync_interval_ms: j.fsync_interval_ms,
        },
    }))
}

pub fn default_store_root() -> PathBuf {
    crate::praca_store::state_dir()
        .join("tear")
        .join("makimono")
}

pub fn enable_and_restore(
    inproc: &InProcess,
    sessions: &SessionsConfig,
    socket_path: Option<&Path>,
) -> Option<RestoreReport> {
    match durable_from_config(sessions, socket_path) {
        Ok(Some(durable)) => {
            info!(
                store = %durable.store.root().display(),
                holders = %durable.holder_dir.display(),
                "tear-daemon: sessions are held — they survive daemon restarts and are revived after reboots"
            );
            inproc.enable_durability(Some(durable));
            Some(inproc.restore_durable())
        }
        Ok(None) => None,
        Err(e) => {
            warn!(error = %e, "tear-daemon: durable sessions requested but unavailable — sessions are process-bound for this run");
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn process_bound_config_builds_nothing() {
        assert!(
            durable_from_config(&SessionsConfig::default(), None)
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn held_config_derives_store_holders_and_program() {
        let root = std::env::temp_dir().join(format!("tear-durable-cfg-{}", std::process::id()));
        let sessions = SessionsConfig {
            durability: SessionDurability::Held,
            holder_program: Some(vec!["/opt/tear".into(), "hold".into()]),
            store_dir: Some(root.to_string_lossy().into_owned()),
            ..SessionsConfig::default()
        };
        let d = durable_from_config(&sessions, Some(Path::new("/run/u/tear.sock")))
            .unwrap()
            .unwrap();
        assert_eq!(d.store.root(), root.as_path());
        assert_eq!(d.holder_dir, PathBuf::from("/run/u/h"));
        assert_eq!(d.program.program, PathBuf::from("/opt/tear"));
        assert_eq!(d.program.prefix, vec!["hold".to_string()]);
        assert_eq!(d.bounds.max_bytes, 64 * 1024 * 1024);
        let _ = std::fs::remove_dir_all(root);
    }
}
