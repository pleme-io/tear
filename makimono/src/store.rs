use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use tear_types::{PaneId, SessionId, TearSession};

use crate::atomic;
use crate::ending::Ending;
use crate::journal::{Journal, JournalBounds};

const SESSIONS: &str = "sessions";
const ENDED: &str = "ended";
const SESSION_DOC: &str = "session.json";
const PANES: &str = "panes";
const META: &str = "meta.json";
const TOMBSTONE: &str = "ending.json";
const POLLED_CWD: &str = "cwd";
const JOURNAL: &str = "journal";
const HOLDER_LOG: &str = "holder.log";
const DOC_VERSION: u32 = 1;

#[derive(Clone, Debug)]
pub struct Store {
    root: PathBuf,
}

#[derive(Clone, Debug)]
pub struct SessionDir {
    id: SessionId,
    path: PathBuf,
}

#[derive(Clone, Debug)]
pub struct PaneDir {
    id: PaneId,
    path: PathBuf,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PaneMeta {
    pub shell: String,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub spawn_cwd: Option<String>,
    #[serde(default)]
    pub last_cwd: Option<String>,
    #[serde(default)]
    pub title: String,
    pub size: (u16, u16),
    pub holder_socket: PathBuf,
    #[serde(default)]
    pub holder_pid: Option<u32>,
    #[serde(default)]
    pub overrides: Vec<(String, String)>,
    #[serde(default)]
    pub resurrections: u32,
}

#[derive(Serialize, Deserialize)]
struct SessionDoc {
    version: u32,
    session: TearSession,
}

impl Store {
    pub fn open(root: impl Into<PathBuf>) -> io::Result<Self> {
        let root = root.into();
        atomic::create_private_dir(&root.join(SESSIONS))?;
        Ok(Self { root })
    }

    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }

    #[must_use]
    pub fn session(&self, id: SessionId) -> SessionDir {
        SessionDir {
            id,
            path: self.root.join(SESSIONS).join(id.to_string()),
        }
    }

    pub fn sessions(&self) -> io::Result<Vec<SessionDir>> {
        let mut out = Vec::new();
        for entry in fs::read_dir(self.root.join(SESSIONS))? {
            let entry = entry?;
            if !entry.file_type()?.is_dir() {
                continue;
            }
            let Some(id) = entry.file_name().to_str().and_then(|s| s.parse().ok()) else {
                continue;
            };
            out.push(SessionDir {
                id,
                path: entry.path(),
            });
        }
        out.sort_by_key(|s| s.id);
        Ok(out)
    }

    pub fn archive(&self, dir: &SessionDir, keep: usize) -> io::Result<()> {
        let ended = self.root.join(ENDED);
        atomic::create_private_dir(&ended)?;
        let dest = ended.join(format!("{}-{}", dir.id, crate::ending::now_unix()));
        match fs::rename(&dir.path, &dest) {
            Ok(()) => {}
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
            Err(e) => return Err(e),
        }
        let mut kept: Vec<(std::time::SystemTime, PathBuf)> = fs::read_dir(&ended)?
            .filter_map(Result::ok)
            .filter_map(|e| {
                let modified = e.metadata().ok()?.modified().ok()?;
                Some((modified, e.path()))
            })
            .collect();
        kept.sort_by(|a, b| b.0.cmp(&a.0));
        for (_, stale) in kept.into_iter().skip(keep) {
            let _ = fs::remove_dir_all(stale);
        }
        Ok(())
    }
}

impl SessionDir {
    #[must_use]
    pub fn id(&self) -> SessionId {
        self.id
    }

    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    #[must_use]
    pub fn pane(&self, id: PaneId) -> PaneDir {
        PaneDir {
            id,
            path: self.path.join(PANES).join(id.to_string()),
        }
    }

    pub fn panes(&self) -> io::Result<Vec<PaneDir>> {
        let dir = self.path.join(PANES);
        let entries = match fs::read_dir(&dir) {
            Ok(e) => e,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(e),
        };
        let mut out = Vec::new();
        for entry in entries {
            let entry = entry?;
            let Some(id) = entry.file_name().to_str().and_then(|s| s.parse().ok()) else {
                continue;
            };
            out.push(PaneDir {
                id,
                path: entry.path(),
            });
        }
        out.sort_by_key(|p| p.id);
        Ok(out)
    }

    pub fn write_doc(&self, session: &TearSession) -> io::Result<()> {
        atomic::write_json(
            &self.path.join(SESSION_DOC),
            &SessionDoc {
                version: DOC_VERSION,
                session: session.clone(),
            },
        )
    }

    pub fn read_doc(&self) -> io::Result<Option<TearSession>> {
        let doc: Option<SessionDoc> = atomic::read_json(&self.path.join(SESSION_DOC))?;
        match doc {
            Some(d) if d.version == DOC_VERSION => Ok(Some(d.session)),
            Some(d) => Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "session document version {} is not {DOC_VERSION}",
                    d.version
                ),
            )),
            None => Ok(None),
        }
    }

    pub fn all_panes_ended(&self) -> io::Result<bool> {
        for pane in self.panes()? {
            if pane.tombstone()?.is_none() {
                return Ok(false);
            }
        }
        Ok(true)
    }
}

impl PaneDir {
    #[must_use]
    pub fn at(path: impl Into<PathBuf>) -> Self {
        let path = path.into();
        let id = path
            .file_name()
            .and_then(|n| n.to_str())
            .and_then(|s| s.parse().ok())
            .unwrap_or(PaneId::NULL);
        Self { id, path }
    }

    #[must_use]
    pub fn id(&self) -> PaneId {
        self.id
    }

    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn create(&self) -> io::Result<()> {
        atomic::create_private_dir(&self.path)
    }

    #[must_use]
    pub fn holder_log(&self) -> PathBuf {
        self.path.join(HOLDER_LOG)
    }

    pub fn write_meta(&self, meta: &PaneMeta) -> io::Result<()> {
        atomic::write_json(&self.path.join(META), meta)
    }

    pub fn read_meta(&self) -> io::Result<Option<PaneMeta>> {
        atomic::read_json(&self.path.join(META))
    }

    pub fn tombstone(&self) -> io::Result<Option<Ending>> {
        atomic::read_json(&self.path.join(TOMBSTONE))
    }

    pub fn write_tombstone(&self, ending: &Ending) -> io::Result<bool> {
        if self.tombstone()?.is_some() {
            return Ok(false);
        }
        atomic::write_json(&self.path.join(TOMBSTONE), ending)?;
        Ok(true)
    }

    pub fn write_polled_cwd(&self, cwd: &str) -> io::Result<()> {
        if self.polled_cwd().as_deref() == Some(cwd) {
            return Ok(());
        }
        atomic::write(&self.path.join(POLLED_CWD), cwd.as_bytes())
    }

    #[must_use]
    pub fn polled_cwd(&self) -> Option<String> {
        fs::read_to_string(self.path.join(POLLED_CWD))
            .ok()
            .filter(|s| !s.is_empty())
    }

    #[must_use]
    pub fn revive_cwd(&self, meta: &PaneMeta) -> Option<String> {
        [
            self.polled_cwd(),
            meta.last_cwd.clone(),
            meta.spawn_cwd.clone(),
        ]
        .into_iter()
        .flatten()
        .find(|d| Path::new(d).is_dir())
    }

    pub fn open_journal(&self, bounds: JournalBounds) -> io::Result<Journal> {
        Journal::open(&self.path.join(JOURNAL), bounds)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testdir::TempDir;
    use std::collections::BTreeMap;
    use tear_types::{SessionSource, SessionState, WindowId};

    fn session(id: SessionId) -> TearSession {
        TearSession {
            id,
            name: "work".into(),
            windows: BTreeMap::new(),
            panes: BTreeMap::new(),
            active_window: WindowId::NULL,
            state: SessionState::Active,
            created_at_unix: 1,
            description: String::new(),
            source: SessionSource::Human,
            freio: tear_types::Freio::default(),
        }
    }

    fn meta(sock: &Path) -> PaneMeta {
        PaneMeta {
            shell: "/bin/sh".into(),
            args: vec![],
            spawn_cwd: Some("/".into()),
            last_cwd: None,
            title: String::new(),
            size: (80, 24),
            holder_socket: sock.to_path_buf(),
            holder_pid: None,
            overrides: vec![],
            resurrections: 0,
        }
    }

    #[test]
    fn session_doc_and_pane_meta_round_trip() {
        let t = TempDir::new("store-rt");
        let store = Store::open(t.path()).unwrap();
        let sid = SessionId::from_seed("s");
        let dir = store.session(sid);
        dir.write_doc(&session(sid)).unwrap();
        assert_eq!(dir.read_doc().unwrap().unwrap().id, sid);
        let pane = dir.pane(PaneId::from_seed("p"));
        pane.create().unwrap();
        let m = meta(&t.path().join("h.sock"));
        pane.write_meta(&m).unwrap();
        assert_eq!(pane.read_meta().unwrap().unwrap(), m);
        let listed = store.sessions().unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].id(), sid);
        assert_eq!(listed[0].panes().unwrap()[0].id(), pane.id());
        assert_eq!(PaneDir::at(pane.path()).id(), pane.id());
    }

    #[test]
    fn the_first_ending_wins() {
        let t = TempDir::new("store-tomb");
        let store = Store::open(t.path()).unwrap();
        let pane = store
            .session(SessionId::from_seed("s"))
            .pane(PaneId::from_seed("p"));
        pane.create().unwrap();
        assert!(pane.tombstone().unwrap().is_none());
        assert!(pane.write_tombstone(&Ending::by_control()).unwrap());
        assert!(
            !pane
                .write_tombstone(&Ending::Exited { code: Some(0) })
                .unwrap()
        );
        assert!(matches!(
            pane.tombstone().unwrap(),
            Some(Ending::EndedBy { .. })
        ));
    }

    #[test]
    fn revive_cwd_prefers_the_polled_directory_then_osc7_then_spawn() {
        let t = TempDir::new("store-cwd");
        let store = Store::open(t.path()).unwrap();
        let pane = store
            .session(SessionId::from_seed("s"))
            .pane(PaneId::from_seed("p"));
        pane.create().unwrap();
        let mut m = meta(&t.path().join("h.sock"));
        assert_eq!(pane.revive_cwd(&m).as_deref(), Some("/"));
        m.last_cwd = Some(t.path().to_string_lossy().into_owned());
        assert_eq!(pane.revive_cwd(&m), m.last_cwd);
        let polled = t.path().join("sessions");
        pane.write_polled_cwd(&polled.to_string_lossy()).unwrap();
        assert_eq!(
            pane.revive_cwd(&m).as_deref(),
            Some(polled.to_string_lossy().as_ref())
        );
        m.last_cwd = Some("/definitely/not/here".into());
        pane.write_polled_cwd("/also/not/here").unwrap();
        assert_eq!(pane.revive_cwd(&m).as_deref(), Some("/"));
    }

    #[test]
    fn archive_moves_the_session_out_and_bounds_retention() {
        let t = TempDir::new("store-archive");
        let store = Store::open(t.path()).unwrap();
        for i in 0..5 {
            let dir = store.session(SessionId::from_seed(&format!("s{i}")));
            dir.write_doc(&session(dir.id())).unwrap();
            store.archive(&dir, 3).unwrap();
            assert!(!dir.path().exists());
        }
        assert!(store.sessions().unwrap().is_empty());
        let ended = fs::read_dir(t.path().join(ENDED)).unwrap().count();
        assert!(ended <= 3, "retention bound exceeded: {ended}");
    }
}
