use std::collections::HashMap;
use std::convert::Infallible;
use std::sync::Arc;

use tear_picker::{
    BadgeMode, ControlSessionReconciler, FuzzyPicker, IndexReconciler, PickerAction, PickerKey,
    RenameAction, RowKind, SessionPickerRow, UnionOptions, picker_action, rename_action,
    session_rows,
};
use tear_types::{
    ControlError, MultiplexerControl, PaneId, PaneState, SessionId, SessionState, TearSession,
};

pub type Row = SessionPickerRow<Infallible, Infallible>;

#[derive(Clone, Debug)]
pub struct Entry {
    pub id: SessionId,
    pub name: String,
    pub panes: usize,
    pub live: bool,
    pub created_at_unix: u64,
    pub pane: Option<PaneId>,
}

impl Entry {
    #[must_use]
    pub fn from_session(s: &TearSession) -> Self {
        let live = s.state == SessionState::Active
            && s.panes
                .values()
                .any(|p| matches!(p.state, PaneState::Running | PaneState::Spawning));
        let pane = s
            .windows
            .get(&s.active_window)
            .or_else(|| s.windows.values().next())
            .map(|w| w.active_pane);
        Self {
            id: s.id,
            name: s.name.clone(),
            panes: s.panes.len(),
            live,
            created_at_unix: s.created_at_unix,
            pane,
        }
    }
}

pub struct Snapshot {
    pub entries: HashMap<SessionId, Entry>,
    pub praca: praca::Praca,
    pub now: u64,
}

impl Snapshot {
    pub fn load(control: &Arc<dyn MultiplexerControl>, now: u64) -> Result<Self, ControlError> {
        let sessions = control.list_sessions()?;
        let mut praca = praca::Praca::new();
        ControlSessionReconciler::new(Arc::clone(control)).reconcile(&mut praca, now);
        for s in &sessions {
            if let Some(rec) = praca.index.get(s.id).cloned() {
                let mut rec = rec;
                rec.last_seen = s.created_at_unix;
                praca.index.upsert(rec);
            }
        }
        let entries = sessions
            .iter()
            .map(|s| (s.id, Entry::from_session(s)))
            .collect();
        Ok(Self {
            entries,
            praca,
            now,
        })
    }

    #[must_use]
    pub fn rows(&self, query: &str) -> Vec<Row> {
        let pane_of = |id: SessionId| self.entries.get(&id).and_then(|e| e.pane);
        session_rows(
            &self.praca,
            &pane_of,
            UnionOptions {
                surface_presets: false,
                badges: BadgeMode::Off,
            },
            query,
            self.now,
            Vec::new(),
            0,
        )
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Mode {
    Browse,
    Rename { id: SessionId, buf: String },
    ConfirmKill { id: SessionId, name: String },
    NewName { buf: String },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CreateRequest {
    Named(String),
    Preset(usize),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Outcome {
    Continue,
    Quit,
    Attach(SessionId),
    Create(CreateRequest),
    Kill(SessionId),
    Rename(SessionId, String),
}

pub struct PickerModel {
    pub picker: FuzzyPicker<Row>,
    pub snap: Snapshot,
    pub mode: Mode,
    pub shortcuts: bool,
    pub daemon_lost: bool,
}

impl PickerModel {
    #[must_use]
    pub fn new(snap: Snapshot, shortcuts: bool) -> Self {
        let mut picker = FuzzyPicker::new();
        picker.open(snap.rows(""), false);
        Self {
            picker,
            snap,
            mode: Mode::Browse,
            shortcuts,
            daemon_lost: false,
        }
    }

    pub fn refresh(&mut self, snap: Snapshot) {
        self.snap = snap;
        self.daemon_lost = false;
        let rows = self.snap.rows(&self.picker.query);
        self.picker
            .set_results_preserving(rows, |r: &Row| r.kind.clone());
    }

    pub fn mark_daemon_lost(&mut self) {
        self.daemon_lost = true;
    }

    fn refilter(&mut self) {
        let rows = self.snap.rows(&self.picker.query);
        self.picker.set_results(rows);
    }

    #[must_use]
    pub fn selected_entry(&self) -> Option<&Entry> {
        match self.picker.selected_row().map(|r| &r.kind) {
            Some(RowKind::Switch(id)) => self.snap.entries.get(id),
            _ => None,
        }
    }

    fn default_name(&self) -> String {
        format!("session-{}", self.snap.entries.len() + 1)
    }

    pub fn key(&mut self, key: &PickerKey) -> Outcome {
        self.picker.notice = None;
        match self.mode.clone() {
            Mode::Browse => self.browse_key(key),
            Mode::Rename { id, mut buf } => {
                match rename_action(key) {
                    RenameAction::Cancel => self.mode = Mode::Browse,
                    RenameAction::Commit => {
                        self.mode = Mode::Browse;
                        return Outcome::Rename(id, buf);
                    }
                    RenameAction::Backspace => {
                        buf.pop();
                        self.mode = Mode::Rename { id, buf };
                    }
                    RenameAction::Push(t) => {
                        buf.push_str(&t);
                        self.mode = Mode::Rename { id, buf };
                    }
                    RenameAction::Ignore => {}
                }
                Outcome::Continue
            }
            Mode::NewName { mut buf } => {
                match rename_action(key) {
                    RenameAction::Cancel => self.mode = Mode::Browse,
                    RenameAction::Commit => {
                        self.mode = Mode::Browse;
                        let name = if buf.trim().is_empty() {
                            self.default_name()
                        } else {
                            buf.trim().to_owned()
                        };
                        return Outcome::Create(CreateRequest::Named(name));
                    }
                    RenameAction::Backspace => {
                        buf.pop();
                        self.mode = Mode::NewName { buf };
                    }
                    RenameAction::Push(t) => {
                        buf.push_str(&t);
                        self.mode = Mode::NewName { buf };
                    }
                    RenameAction::Ignore => {}
                }
                Outcome::Continue
            }
            Mode::ConfirmKill { id, .. } => {
                self.mode = Mode::Browse;
                if key.plain && key.text.as_deref() == Some("y") {
                    Outcome::Kill(id)
                } else {
                    Outcome::Continue
                }
            }
        }
    }

    fn begin_kill(&mut self) {
        match self.selected_entry() {
            Some(e) => {
                self.mode = Mode::ConfirmKill {
                    id: e.id,
                    name: e.name.clone(),
                };
            }
            None => self.picker.notice = Some("nothing to kill on this row".into()),
        }
    }

    fn browse_key(&mut self, key: &PickerKey) -> Outcome {
        if self.shortcuts && self.picker.query.is_empty() && key.plain {
            match key.text.as_deref() {
                Some("q") => return Outcome::Quit,
                Some("n") => {
                    self.mode = Mode::NewName { buf: String::new() };
                    return Outcome::Continue;
                }
                Some("x") => {
                    self.begin_kill();
                    return Outcome::Continue;
                }
                _ => {}
            }
        }
        match picker_action(key) {
            PickerAction::Close => return Outcome::Quit,
            PickerAction::Accept => return self.accept(),
            PickerAction::MoveUp => self.picker.move_up(),
            PickerAction::MoveDown => self.picker.move_down(),
            PickerAction::Backspace => {
                self.picker.query.pop();
                self.refilter();
            }
            PickerAction::Push(t) => {
                self.picker.query.push_str(&t);
                self.refilter();
            }
            PickerAction::RenameBegin => match self.selected_entry() {
                Some(e) => {
                    self.mode = Mode::Rename {
                        id: e.id,
                        buf: e.name.clone(),
                    };
                }
                None => self.picker.notice = Some("nothing to rename on this row".into()),
            },
            PickerAction::Delete => self.begin_kill(),
            PickerAction::Reattach => {
                self.picker.notice =
                    Some("reattach is not available in the terminal client".into());
            }
            PickerAction::Ignore => {}
        }
        Outcome::Continue
    }

    fn accept(&mut self) -> Outcome {
        let Some(row) = self.picker.selected_row().cloned() else {
            return Outcome::Continue;
        };
        match row.kind {
            RowKind::Switch(id) => match self.snap.entries.get(&id) {
                Some(e) if e.live && e.pane.is_some() => Outcome::Attach(id),
                Some(e) => {
                    self.picker.notice = Some(format!(
                        "`{}` is latent: no running pane to attach to (x kills it)",
                        e.name
                    ));
                    Outcome::Continue
                }
                None => {
                    self.picker.notice = Some("that session just ended".into());
                    Outcome::Continue
                }
            },
            RowKind::Create(tear_picker::CreateSpec::Named { name }) => {
                Outcome::Create(CreateRequest::Named(name))
            }
            RowKind::Create(tear_picker::CreateSpec::Preset { pool_index }) => {
                Outcome::Create(CreateRequest::Preset(pool_index))
            }
            RowKind::Instantiate(_) | RowKind::Suggestion(_) => Outcome::Continue,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tear_picker::PickerKeyClass;
    use tear_types::SessionSource;

    fn control() -> (Arc<tear_core::InProcess>, Arc<dyn MultiplexerControl>) {
        let inproc = Arc::new(tear_core::InProcess::new());
        inproc.set_spawn_env(tear_types::SpawnEnv::none());
        let dynamic: Arc<dyn MultiplexerControl> = inproc.clone();
        (inproc, dynamic)
    }

    fn spawn(c: &tear_core::InProcess, name: &str) -> SessionId {
        c.new_session_with_source_and_size(name, "/bin/sh", &[], SessionSource::Human, (80, 24))
            .unwrap()
    }

    fn model_with(names: &[&str]) -> (Arc<tear_core::InProcess>, PickerModel) {
        let (inproc, dynamic) = control();
        for n in names {
            spawn(&inproc, n);
        }
        let snap = Snapshot::load(&dynamic, 1000).unwrap();
        (inproc, PickerModel::new(snap, true))
    }

    fn type_text(m: &mut PickerModel, s: &str) {
        for ch in s.chars() {
            m.key(&PickerKey::text(&ch.to_string()));
        }
    }

    fn names(m: &PickerModel) -> Vec<String> {
        m.picker.results.iter().map(|r| r.label.clone()).collect()
    }

    #[test]
    fn every_session_is_listed_on_an_empty_query() {
        let (_c, m) = model_with(&["alpha", "bravo", "charlie"]);
        assert_eq!(m.picker.results.len(), 3);
    }

    #[test]
    fn typing_filters_fuzzily() {
        let (_c, mut m) = model_with(&["alpha", "bravo", "charlie"]);
        type_text(&mut m, "brv");
        assert_eq!(names(&m), vec!["bravo".to_owned()]);
    }

    #[test]
    fn backspace_widens_the_filter_again() {
        let (_c, mut m) = model_with(&["alpha", "bravo"]);
        type_text(&mut m, "zz");
        m.key(&PickerKey::class(PickerKeyClass::Backspace));
        m.key(&PickerKey::class(PickerKeyClass::Backspace));
        assert_eq!(m.picker.results.len(), 2);
    }

    #[test]
    fn enter_attaches_the_highlighted_live_session() {
        let (c, mut m) = model_with(&["alpha"]);
        let id = c.list_sessions().unwrap()[0].id;
        assert_eq!(
            m.key(&PickerKey::class(PickerKeyClass::Enter)),
            Outcome::Attach(id)
        );
    }

    #[test]
    fn arrows_move_the_highlight() {
        let (_c, mut m) = model_with(&["alpha", "bravo", "charlie"]);
        let first = m.picker.selected;
        m.key(&PickerKey::class(PickerKeyClass::Down));
        assert_ne!(m.picker.selected, first);
        m.key(&PickerKey::class(PickerKeyClass::Up));
        assert_eq!(m.picker.selected, first);
    }

    #[test]
    fn escape_and_q_quit() {
        let (_c, mut m) = model_with(&["alpha"]);
        assert_eq!(
            m.key(&PickerKey::class(PickerKeyClass::Escape)),
            Outcome::Quit
        );
        assert_eq!(m.key(&PickerKey::text("q")), Outcome::Quit);
    }

    #[test]
    fn q_filters_once_the_query_is_not_empty() {
        let (_c, mut m) = model_with(&["alpha", "quux"]);
        type_text(&mut m, "a");
        assert_eq!(m.key(&PickerKey::text("q")), Outcome::Continue);
        assert_eq!(m.picker.query, "aq");
    }

    #[test]
    fn disabled_shortcuts_make_q_a_filter_character() {
        let (_c, dynamic) = control();
        let snap = Snapshot::load(&dynamic, 1).unwrap();
        let mut m = PickerModel::new(snap, false);
        assert_eq!(m.key(&PickerKey::text("q")), Outcome::Continue);
        assert_eq!(m.picker.query, "q");
    }

    #[test]
    fn n_asks_for_a_name_then_creates() {
        let (_c, mut m) = model_with(&["alpha"]);
        m.key(&PickerKey::text("n"));
        assert!(matches!(m.mode, Mode::NewName { .. }));
        type_text(&mut m, "work");
        assert_eq!(
            m.key(&PickerKey::class(PickerKeyClass::Enter)),
            Outcome::Create(CreateRequest::Named("work".into()))
        );
        assert_eq!(m.mode, Mode::Browse);
    }

    #[test]
    fn an_empty_new_name_gets_a_default() {
        let (_c, mut m) = model_with(&["alpha"]);
        m.key(&PickerKey::text("n"));
        assert_eq!(
            m.key(&PickerKey::class(PickerKeyClass::Enter)),
            Outcome::Create(CreateRequest::Named("session-2".into()))
        );
    }

    #[test]
    fn x_asks_for_confirmation_and_only_y_kills() {
        let (c, mut m) = model_with(&["alpha"]);
        let id = c.list_sessions().unwrap()[0].id;
        m.key(&PickerKey::text("x"));
        assert!(matches!(m.mode, Mode::ConfirmKill { .. }));
        assert_eq!(m.key(&PickerKey::text("n")), Outcome::Continue);
        assert_eq!(m.mode, Mode::Browse);
        m.key(&PickerKey::text("x"));
        assert_eq!(m.key(&PickerKey::text("y")), Outcome::Kill(id));
    }

    #[test]
    fn ctrl_d_also_asks_before_killing() {
        let (_c, mut m) = model_with(&["alpha"]);
        assert_eq!(
            m.key(&PickerKey::class(PickerKeyClass::CtrlD)),
            Outcome::Continue
        );
        assert!(matches!(m.mode, Mode::ConfirmKill { .. }));
    }

    #[test]
    fn ctrl_e_renames_the_highlighted_session() {
        let (c, mut m) = model_with(&["alpha"]);
        let id = c.list_sessions().unwrap()[0].id;
        m.key(&PickerKey::class(PickerKeyClass::CtrlE));
        for _ in 0..5 {
            m.key(&PickerKey::class(PickerKeyClass::Backspace));
        }
        type_text(&mut m, "omega");
        assert_eq!(
            m.key(&PickerKey::class(PickerKeyClass::Enter)),
            Outcome::Rename(id, "omega".into())
        );
    }

    #[test]
    fn an_empty_daemon_offers_create_rows_not_a_panic() {
        let (_c, mut m) = model_with(&[]);
        assert!(m.snap.entries.is_empty());
        assert!(!m.picker.results.is_empty());
        let out = m.key(&PickerKey::class(PickerKeyClass::Enter));
        assert!(matches!(out, Outcome::Create(_)));
    }

    #[test]
    fn a_typed_name_with_no_match_offers_to_create_it() {
        let (_c, mut m) = model_with(&["alpha"]);
        type_text(&mut m, "zzzqqq");
        assert_eq!(
            m.key(&PickerKey::class(PickerKeyClass::Enter)),
            Outcome::Create(CreateRequest::Named("zzzqqq".into()))
        );
    }

    #[test]
    fn a_refresh_keeps_the_highlight_on_its_session() {
        let (c, dynamic) = control();
        spawn(&c, "alpha");
        spawn(&c, "bravo");
        let mut m = PickerModel::new(Snapshot::load(&dynamic, 1).unwrap(), true);
        m.key(&PickerKey::class(PickerKeyClass::Down));
        let on = m.picker.selected_row().unwrap().kind.clone();
        spawn(&c, "charlie");
        m.refresh(Snapshot::load(&dynamic, 2).unwrap());
        assert_eq!(m.picker.selected_row().unwrap().kind, on);
        assert_eq!(m.picker.results.len(), 3);
    }

    #[test]
    fn a_latent_session_refuses_attach_with_a_notice() {
        let (c, dynamic) = control();
        let id = spawn(&c, "alpha");
        let mut snap = Snapshot::load(&dynamic, 1).unwrap();
        snap.entries.get_mut(&id).unwrap().live = false;
        let mut m = PickerModel::new(snap, true);
        assert_eq!(
            m.key(&PickerKey::class(PickerKeyClass::Enter)),
            Outcome::Continue
        );
        assert!(m.picker.notice.as_deref().unwrap().contains("latent"));
    }

    #[test]
    fn an_unreachable_daemon_is_a_typed_error_not_an_empty_list() {
        let dynamic: Arc<dyn MultiplexerControl> = Arc::new(tear_picker::testing::Unreachable);
        assert!(Snapshot::load(&dynamic, 1).is_err());
    }
}
