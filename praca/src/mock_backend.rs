//! A recording [`MultiplexerControl`] test double — the Environment seam
//! made usable.
//!
//! ## Why this exists
//!
//! `MultiplexerControl` was already the injectable Environment: `instantiate`
//! takes `&dyn MultiplexerControl`, so a double and the real
//! `tear-core::InProcess` are interchangeable by construction. But no double
//! existed, so every `instantiate` test drove the **real** backend — which
//! spawns real PTYs, forks real shells, and can only be asserted on its
//! end state.
//!
//! Two things that costs, and this closes both:
//!
//! 1. **Hermeticity.** A test of pure `(definition) -> actions` logic should
//!    not depend on `/bin/sh` existing, on fork succeeding, or on how fast a
//!    child starts. The org's default delivery method is explicit that if a
//!    test needs real process state, the seam is in the wrong place.
//! 2. **The call sequence is unobservable against a real backend.** You can
//!    see that a tree came out with two panes; you cannot see that
//!    `instantiate` asked for them in the right ORDER with the right shells.
//!    That is exactly where a latent bug hides — and it is what this double
//!    records.
//!
//! ## Tier honesty
//!
//! Mock-green proves the interpreter's `(state, phase) -> actions` shape and
//! **nothing about the real backend**. A green run here is not evidence that
//! `InProcess` spawns correctly; those remain separate rows against the real
//! thing. Never read one as the other.

use std::collections::BTreeMap;
use std::sync::Mutex;

use tear_types::{
    ControlError, ControlResult, Direction, LayoutKind, MultiplexerControl, PaneId, PaneSnapshot,
    SessionId, TearPane, TearSession, TearWindow, WindowId,
    layout::LayoutNode,
    pane::InputPolicy,
    session::SessionSource,
};

/// One recorded call, in the order the interpreter made it.
///
/// Only the verbs an interpreter actually drives are distinguished; the
/// read-only accessors are deliberately not recorded, because asserting on
/// how many times something was *read* pins refactors rather than behaviour.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Call {
    NewSession {
        name: String,
        shell: String,
        args: Vec<String>,
        source: SessionSource,
        size_cells: (u16, u16),
    },
    SplitPane {
        origin: PaneId,
        direction: Direction,
        shell: String,
        args: Vec<String>,
    },
    NewWindow {
        session: SessionId,
        name: String,
        shell: String,
        args: Vec<String>,
    },
    KillSession(SessionId),
    KillWindow(WindowId),
    KillPane(PaneId),
    SelectWindow(WindowId),
    SelectPane(PaneId),
    RenameSession(SessionId, String),
    ResizePane(PaneId, Direction, i16),
    PaneResizeAbsolute(PaneId, u16, u16),
    ApplyLayout(WindowId, LayoutKind),
    SendKeys(PaneId, Vec<u8>),
    SetInputPolicy(PaneId, InputPolicy),
}

/// A `MultiplexerControl` that records what it was asked to do and maintains
/// just enough state for an interpreter to walk a plan.
pub struct MockBackend {
    inner: Mutex<State>,
}

struct State {
    calls: Vec<Call>,
    sessions: BTreeMap<SessionId, TearSession>,
    next: u64,
    /// When set, the next `split_pane` fails with this error — for driving
    /// an interpreter's rollback path without breaking a real system.
    fail_next_split: Option<ControlError>,
}

impl Default for MockBackend {
    fn default() -> Self {
        Self::new()
    }
}

impl MockBackend {
    #[must_use]
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(State {
                calls: Vec::new(),
                sessions: BTreeMap::new(),
                next: 1,
                fail_next_split: None,
            }),
        }
    }

    /// Every call the interpreter made, in order.
    #[must_use]
    pub fn calls(&self) -> Vec<Call> {
        self.inner.lock().unwrap().calls.clone()
    }

    /// Arm a one-shot `split_pane` failure to exercise a rollback path.
    pub fn fail_next_split(&self, err: ControlError) {
        self.inner.lock().unwrap().fail_next_split = Some(err);
    }

    fn mint(st: &mut State) -> u64 {
        let id = st.next;
        st.next += 1;
        id
    }

    /// A minimal running pane — the same shape `registry.rs` builds.
    fn pane(id: PaneId, shell: &str, args: &[String]) -> TearPane {
        TearPane {
            id,
            shell: shell.into(),
            args: args.to_vec(),
            cwd: None,
            env: Vec::new(),
            size_cells: (80, 24),
            origin_cells: (0, 0),
            state: tear_types::pane::PaneState::Running,
            title: shell.into(),
            input_policy: InputPolicy::default(),
        }
    }

    fn window(id: WindowId, name: &str, pane: PaneId) -> TearWindow {
        TearWindow {
            id,
            name: name.into(),
            layout: LayoutNode::leaf(pane),
            active_pane: pane,
            size_cells: (80, 24),
            state: tear_types::window::WindowState::Active,
        }
    }
}

impl MultiplexerControl for MockBackend {
    fn list_sessions(&self) -> ControlResult<Vec<TearSession>> {
        Ok(self.inner.lock().unwrap().sessions.values().cloned().collect())
    }

    fn get_session(&self, id: SessionId) -> ControlResult<TearSession> {
        self.inner
            .lock()
            .unwrap()
            .sessions
            .get(&id)
            .cloned()
            .ok_or(ControlError::NoSuchSession(id))
    }

    fn get_window(&self, id: WindowId) -> ControlResult<(SessionId, TearWindow)> {
        let st = self.inner.lock().unwrap();
        for (sid, s) in &st.sessions {
            if let Some(w) = s.windows.get(&id) {
                return Ok((*sid, w.clone()));
            }
        }
        Err(ControlError::NoSuchWindow(id))
    }

    fn get_pane(&self, id: PaneId) -> ControlResult<TearPane> {
        let st = self.inner.lock().unwrap();
        for s in st.sessions.values() {
            if let Some(p) = s.panes.get(&id) {
                return Ok(p.clone());
            }
        }
        Err(ControlError::NoSuchPane(id))
    }

    fn new_session_with_source_and_size(
        &self,
        name: &str,
        shell: &str,
        args: &[String],
        source: SessionSource,
        size_cells: (u16, u16),
    ) -> ControlResult<SessionId> {
        let mut st = self.inner.lock().unwrap();
        st.calls.push(Call::NewSession {
            name: name.to_string(),
            shell: shell.to_string(),
            args: args.to_vec(),
            source: source.clone(),
            size_cells,
        });
        let sid = SessionId(Self::mint(&mut st));
        let wid = WindowId(Self::mint(&mut st));
        let pid = PaneId(Self::mint(&mut st));

        let mut session = TearSession {
            id: sid,
            name: name.into(),
            windows: BTreeMap::new(),
            panes: BTreeMap::new(),
            active_window: wid,
            state: tear_types::session::SessionState::Active,
            // Time is INJECTED in praça and never read from a clock — a
            // fixed stamp keeps this double deterministic.
            created_at_unix: 0,
            description: String::new(),
            source,
        };
        session.windows.insert(wid, Self::window(wid, name, pid));
        session.panes.insert(pid, Self::pane(pid, shell, args));
        st.sessions.insert(sid, session);
        Ok(sid)
    }

    fn rename_session(&self, id: SessionId, new_name: &str) -> ControlResult<()> {
        let mut st = self.inner.lock().unwrap();
        st.calls.push(Call::RenameSession(id, new_name.to_string()));
        match st.sessions.get_mut(&id) {
            Some(s) => {
                s.name = new_name.to_string();
                Ok(())
            }
            None => Err(ControlError::NoSuchSession(id)),
        }
    }

    fn kill_session(&self, id: SessionId) -> ControlResult<()> {
        let mut st = self.inner.lock().unwrap();
        st.calls.push(Call::KillSession(id));
        st.sessions
            .remove(&id)
            .map(|_| ())
            .ok_or(ControlError::NoSuchSession(id))
    }

    fn new_window(
        &self,
        session: SessionId,
        name: &str,
        shell: &str,
        args: &[String],
    ) -> ControlResult<WindowId> {
        let mut st = self.inner.lock().unwrap();
        st.calls.push(Call::NewWindow {
            session,
            name: name.to_string(),
            shell: shell.to_string(),
            args: args.to_vec(),
        });
        if !st.sessions.contains_key(&session) {
            return Err(ControlError::NoSuchSession(session));
        }
        let wid = WindowId(Self::mint(&mut st));
        let pid = PaneId(Self::mint(&mut st));
        let win = Self::window(wid, name, pid);
        let pane = Self::pane(pid, shell, args);
        let s = st.sessions.get_mut(&session).unwrap();
        s.windows.insert(wid, win);
        s.panes.insert(pid, pane);
        Ok(wid)
    }

    fn kill_window(&self, id: WindowId) -> ControlResult<()> {
        let mut st = self.inner.lock().unwrap();
        st.calls.push(Call::KillWindow(id));
        for s in st.sessions.values_mut() {
            if s.windows.remove(&id).is_some() {
                return Ok(());
            }
        }
        Err(ControlError::NoSuchWindow(id))
    }

    fn select_window(&self, id: WindowId) -> ControlResult<()> {
        let mut st = self.inner.lock().unwrap();
        st.calls.push(Call::SelectWindow(id));
        for s in st.sessions.values_mut() {
            if s.windows.contains_key(&id) {
                s.active_window = id;
                return Ok(());
            }
        }
        Err(ControlError::NoSuchWindow(id))
    }

    fn split_pane(
        &self,
        origin: PaneId,
        direction: Direction,
        shell: &str,
        args: &[String],
    ) -> ControlResult<PaneId> {
        let mut st = self.inner.lock().unwrap();
        st.calls.push(Call::SplitPane {
            origin,
            direction,
            shell: shell.to_string(),
            args: args.to_vec(),
        });
        if let Some(err) = st.fail_next_split.take() {
            return Err(err);
        }
        let new_pid = PaneId(Self::mint(&mut st));
        let pane = Self::pane(new_pid, shell, args);
        for s in st.sessions.values_mut() {
            let target = s
                .windows
                .values_mut()
                .find(|w| w.layout.panes().contains(&origin));
            if let Some(w) = target {
                if !w.layout.split_leaf(origin, new_pid, direction, 0.5) {
                    return Err(ControlError::NoSuchPane(origin));
                }
                w.active_pane = new_pid;
                s.panes.insert(new_pid, pane);
                return Ok(new_pid);
            }
        }
        Err(ControlError::NoSuchPane(origin))
    }

    fn kill_pane(&self, id: PaneId) -> ControlResult<()> {
        let mut st = self.inner.lock().unwrap();
        st.calls.push(Call::KillPane(id));
        for s in st.sessions.values_mut() {
            if s.panes.remove(&id).is_some() {
                for w in s.windows.values_mut() {
                    w.layout.remove_leaf(id);
                }
                return Ok(());
            }
        }
        Err(ControlError::NoSuchPane(id))
    }

    fn select_pane(&self, id: PaneId) -> ControlResult<()> {
        let mut st = self.inner.lock().unwrap();
        st.calls.push(Call::SelectPane(id));
        for s in st.sessions.values_mut() {
            if s.panes.contains_key(&id) {
                for w in s.windows.values_mut() {
                    if w.layout.panes().contains(&id) {
                        w.active_pane = id;
                    }
                }
                return Ok(());
            }
        }
        Err(ControlError::NoSuchPane(id))
    }

    fn resize_pane(&self, id: PaneId, direction: Direction, delta_cells: i16) -> ControlResult<()> {
        let mut st = self.inner.lock().unwrap();
        st.calls.push(Call::ResizePane(id, direction, delta_cells));
        Ok(())
    }

    fn pane_resize_absolute(&self, id: PaneId, cols: u16, rows: u16) -> ControlResult<()> {
        let mut st = self.inner.lock().unwrap();
        st.calls.push(Call::PaneResizeAbsolute(id, cols, rows));
        Ok(())
    }

    fn apply_layout(&self, window: WindowId, kind: LayoutKind) -> ControlResult<()> {
        let mut st = self.inner.lock().unwrap();
        st.calls.push(Call::ApplyLayout(window, kind));
        Ok(())
    }

    fn send_keys(&self, id: PaneId, bytes: &[u8]) -> ControlResult<()> {
        let mut st = self.inner.lock().unwrap();
        st.calls.push(Call::SendKeys(id, bytes.to_vec()));
        Ok(())
    }

    fn pane_subscriber_count(&self, _id: PaneId) -> ControlResult<u32> {
        Ok(0)
    }

    fn set_input_policy(&self, id: PaneId, policy: InputPolicy) -> ControlResult<()> {
        let mut st = self.inner.lock().unwrap();
        st.calls.push(Call::SetInputPolicy(id, policy));
        Ok(())
    }

    fn pane_snapshot(&self, _id: PaneId) -> ControlResult<PaneSnapshot> {
        Ok(PaneSnapshot {
            rows: 24,
            cols: 80,
            cells: vec![vec![tear_types::Cell::BLANK; 80]; 24],
            cursor_row: 0,
            cursor_col: 0,
            alt_screen_active: false,
            cursor_visible: true,
            title: None,
            cursor_keys_mode: false,
            scrollback: Vec::new(),
            combining: Vec::new(),
            modes: tear_types::ModeSet::default(),
        })
    }
}
