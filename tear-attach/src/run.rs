use std::io::Write;
use std::sync::Arc;
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender};
use std::time::Duration;

use ishou_tokens::{FleetSessionNames, SessionName};
use tear_config::AttachSizing;
use tear_types::{MultiplexerControl, PaneId, SessionId, SessionSource};

use crate::bindings::Bindings;
use crate::input::{InputAction, InputFilter, decode_picker_keys};
use crate::io::{IoError, PaneIo, Subscription};
use crate::model::{CreateRequest, Entry, Outcome, PickerModel, Snapshot};
use crate::render::render_picker;

pub const RESET: &str = "\x1b[?1049l\x1b[0m\x1b[?25h\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006l\x1b[?2004l\x1b[?1l\x1b>\x1b[<u";
pub const CLEAR: &str = "\x1b[2J\x1b[H";
pub const ENTER_ALT: &str = "\x1b[?1049h";

#[derive(Debug)]
pub enum Event {
    Stdin(Vec<u8>),
    StdinClosed,
    Pane { generation: u64, bytes: Vec<u8> },
    Resize(u16, u16),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Start {
    Picker,
    Target(String),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Exit {
    Detached,
    Quit,
    PaneEnded,
    InputClosed,
}

#[derive(Debug, thiserror::Error)]
pub enum AttachError {
    #[error("tear-daemon is not reachable: {0}")]
    DaemonUnreachable(String),
    #[error("lost the connection to tear-daemon: {0}")]
    DaemonLost(String),
    #[error("no session matches `{0}`")]
    TargetNotFound(String),
    #[error("`{target}` matches more than one session: {}", .matches.join(", "))]
    TargetAmbiguous {
        target: String,
        matches: Vec<String>,
    },
    #[error("session `{0}` is latent: it has no running pane to attach to")]
    TargetLatent(String),
    #[error("attach is switched off in the config (attach.enabled: false)")]
    Disabled,
    #[error("could not write to the terminal: {0}")]
    Terminal(String),
}

impl From<std::io::Error> for AttachError {
    fn from(e: std::io::Error) -> Self {
        Self::Terminal(e.to_string())
    }
}

pub struct Attach {
    pub control: Arc<dyn MultiplexerControl>,
    pub io: Arc<dyn PaneIo>,
    pub bindings: Bindings,
    pub sizing: AttachSizing,
    pub shortcuts: bool,
    pub shell: String,
    pub tick: Duration,
    pub clock: Box<dyn Fn() -> u64 + Send + Sync>,
}

pub fn resolve_target(snap: &Snapshot, target: &str) -> Result<Entry, AttachError> {
    let mut entries: Vec<&Entry> = snap.entries.values().collect();
    entries.sort_by(|a, b| a.name.cmp(&b.name));
    let exact_name: Vec<&&Entry> = entries.iter().filter(|e| e.name == target).collect();
    let pick = |found: Vec<&&Entry>| -> Result<Option<Entry>, AttachError> {
        match found.len() {
            0 => Ok(None),
            1 => Ok(Some((*found[0]).clone())),
            _ => Err(AttachError::TargetAmbiguous {
                target: target.to_owned(),
                matches: found
                    .iter()
                    .map(|e| format!("{} ({})", e.name, e.id))
                    .collect(),
            }),
        }
    };
    if let Some(e) = pick(exact_name)? {
        return Ok(e);
    }
    let exact_id: Vec<&&Entry> = entries
        .iter()
        .filter(|e| e.id.to_string() == target)
        .collect();
    if let Some(e) = pick(exact_id)? {
        return Ok(e);
    }
    let prefix: Vec<&&Entry> = entries
        .iter()
        .filter(|e| e.name.starts_with(target) || e.id.to_string().starts_with(target))
        .collect();
    pick(prefix)?.ok_or_else(|| AttachError::TargetNotFound(target.to_owned()))
}

pub fn live_pane(entry: Entry) -> Result<PaneId, AttachError> {
    match (entry.live, entry.pane) {
        (true, Some(pane)) => Ok(pane),
        _ => Err(AttachError::TargetLatent(entry.name)),
    }
}

struct Ctx<'a> {
    size: (u16, u16),
    events: &'a Receiver<Event>,
    tx: &'a Sender<Event>,
    out: &'a mut dyn Write,
    generation: u64,
    filter: InputFilter,
}

enum Next {
    Pane {
        pane: PaneId,
        from_picker: bool,
    },
    Pick {
        notice: Option<String>,
        overlay_from: Option<PaneId>,
        rest: Vec<u8>,
    },
    Done(Exit),
}

impl Attach {
    pub fn run(
        &self,
        start: Start,
        size: (u16, u16),
        events: &Receiver<Event>,
        tx: &Sender<Event>,
        out: &mut dyn Write,
    ) -> Result<Exit, AttachError> {
        let mut ctx = Ctx {
            size,
            events,
            tx,
            out,
            generation: 0,
            filter: InputFilter::new(self.bindings.clone()),
        };
        let mut next = match start {
            Start::Target(t) => {
                let snap = self.snapshot()?;
                let pane = live_pane(resolve_target(&snap, &t)?)?;
                Next::Pane {
                    pane,
                    from_picker: false,
                }
            }
            Start::Picker => {
                ctx.out.write_all(ENTER_ALT.as_bytes())?;
                Next::Pick {
                    notice: None,
                    overlay_from: None,
                    rest: Vec::new(),
                }
            }
        };
        loop {
            next = match next {
                Next::Done(exit) => {
                    ctx.out.write_all(RESET.as_bytes())?;
                    ctx.out.flush()?;
                    return Ok(exit);
                }
                Next::Pane { pane, from_picker } => self.run_pane(&mut ctx, pane, from_picker)?,
                Next::Pick {
                    notice,
                    overlay_from,
                    rest,
                } => self.run_pick(&mut ctx, notice, overlay_from, rest)?,
            };
        }
    }

    fn snapshot(&self) -> Result<Snapshot, AttachError> {
        Snapshot::load(&self.control, (self.clock)())
            .map_err(|e| AttachError::DaemonUnreachable(e.to_string()))
    }

    fn lost(e: IoError) -> AttachError {
        AttachError::DaemonLost(e.to_string())
    }

    fn enter_pane(
        &self,
        ctx: &mut Ctx<'_>,
        pane: PaneId,
    ) -> Result<Option<Subscription>, AttachError> {
        ctx.out.write_all(RESET.as_bytes())?;
        ctx.out.write_all(CLEAR.as_bytes())?;
        ctx.out.flush()?;
        ctx.generation += 1;
        ctx.filter.reset();
        let generation = ctx.generation;
        let tx = ctx.tx.clone();
        let sink: crate::io::Sink = Box::new(move |b: &[u8]| {
            let _ = tx.send(Event::Pane {
                generation,
                bytes: b.to_vec(),
            });
        });
        let (cols, rows) = ctx.size;
        let sub = match self.sizing {
            AttachSizing::BeforeReplay => {
                match self.io.resize(pane, cols, rows) {
                    Ok(()) | Err(IoError::Rejected(_)) => {}
                    Err(IoError::Gone) => return Ok(None),
                    Err(e) => return Err(Self::lost(e)),
                }
                match self.io.subscribe(pane, sink) {
                    Ok(s) => s,
                    Err(IoError::Gone) => return Ok(None),
                    Err(e) => return Err(Self::lost(e)),
                }
            }
            AttachSizing::AfterReplay => {
                let s = match self.io.subscribe(pane, sink) {
                    Ok(s) => s,
                    Err(IoError::Gone) => return Ok(None),
                    Err(e) => return Err(Self::lost(e)),
                };
                match self.io.resize(pane, cols, rows) {
                    Ok(()) | Err(IoError::Rejected(_)) => {}
                    Err(IoError::Gone) => return Ok(None),
                    Err(e) => return Err(Self::lost(e)),
                }
                s
            }
        };
        if !self.io.stream_carries_replay() {
            match self.io.snapshot_paint(pane) {
                Ok(bytes) => {
                    ctx.out.write_all(&bytes)?;
                    ctx.out.flush()?;
                }
                Err(IoError::Gone) => return Ok(None),
                Err(e) => return Err(Self::lost(e)),
            }
        }
        Ok(Some(sub))
    }

    fn notice_line(ctx: &mut Ctx<'_>, msg: &str) -> Result<(), AttachError> {
        let row = ctx.size.1.max(1);
        write!(ctx.out, "\x1b7\x1b[{row};1H\x1b[7m {msg} \x1b[0m\x1b8")?;
        ctx.out.flush()?;
        Ok(())
    }

    fn run_pane(
        &self,
        ctx: &mut Ctx<'_>,
        pane: PaneId,
        from_picker: bool,
    ) -> Result<Next, AttachError> {
        let Some(sub) = self.enter_pane(ctx, pane)? else {
            return Ok(self.pane_ended(from_picker));
        };
        let generation = ctx.generation;
        let leave = |sub: Subscription| drop(sub);
        let result = loop {
            match ctx.events.recv_timeout(self.tick) {
                Ok(Event::Pane {
                    generation: g,
                    bytes,
                }) => {
                    if g == generation {
                        ctx.out.write_all(&bytes)?;
                        ctx.out.flush()?;
                    }
                }
                Ok(Event::Resize(c, r)) => {
                    ctx.size = (c, r);
                    match self.io.resize(pane, c, r) {
                        Ok(()) | Err(IoError::Gone | IoError::Rejected(_)) => {}
                        Err(e) => break Err(Self::lost(e)),
                    }
                }
                Ok(Event::StdinClosed) => break Ok(Next::Done(Exit::InputClosed)),
                Ok(Event::Stdin(bytes)) => {
                    let mut next = None;
                    for act in ctx.filter.feed(&bytes) {
                        match act {
                            InputAction::Forward(b) => match self.io.send(pane, &b) {
                                Ok(()) => {}
                                Err(IoError::Gone) => {
                                    next = Some(self.pane_ended(from_picker));
                                    break;
                                }
                                Err(IoError::Rejected(m)) => {
                                    Self::notice_line(ctx, &format!("input refused: {m}"))?;
                                }
                                Err(e) => return Err(Self::lost(e)),
                            },
                            InputAction::OpenPicker => {
                                next = Some(Next::Pick {
                                    notice: None,
                                    overlay_from: Some(pane),
                                    rest: Vec::new(),
                                });
                            }
                            InputAction::Rest(r) => {
                                if let Some(Next::Pick { rest, .. }) = next.as_mut() {
                                    *rest = r;
                                }
                            }
                            InputAction::Detach => {
                                next = Some(Next::Done(Exit::Detached));
                                break;
                            }
                        }
                    }
                    if let Some(n) = next {
                        break Ok(n);
                    }
                }
                Err(RecvTimeoutError::Timeout) => match self.io.pane_alive(pane) {
                    Ok(true) => {}
                    Ok(false) | Err(IoError::Gone) => break Ok(self.pane_ended(from_picker)),
                    Err(e) => break Err(Self::lost(e)),
                },
                Err(RecvTimeoutError::Disconnected) => break Ok(Next::Done(Exit::InputClosed)),
            }
        };
        leave(sub);
        result
    }

    fn pane_ended(&self, from_picker: bool) -> Next {
        if from_picker {
            Next::Pick {
                notice: Some("that session ended".into()),
                overlay_from: None,
                rest: Vec::new(),
            }
        } else {
            Next::Done(Exit::PaneEnded)
        }
    }

    fn create(
        &self,
        ctx: &Ctx<'_>,
        model: &PickerModel,
        req: CreateRequest,
    ) -> Result<Entry, String> {
        let name = match req {
            CreateRequest::Named(n) => n,
            CreateRequest::Preset(i) => SessionName {
                identity: FleetSessionNames::identity(i as u64),
                style: model.snap.praca.name_style,
            }
            .to_string(),
        };
        let id: SessionId = self
            .control
            .new_session_with_source_and_size(
                &name,
                &self.shell,
                &[],
                SessionSource::Human,
                ctx.size,
            )
            .map_err(|e| format!("could not create `{name}`: {e}"))?;
        let s = self
            .control
            .get_session(id)
            .map_err(|e| format!("created `{name}` but could not read it: {e}"))?;
        Ok(Entry::from_session(&s))
    }

    fn run_pick(
        &self,
        ctx: &mut Ctx<'_>,
        notice: Option<String>,
        overlay_from: Option<PaneId>,
        rest: Vec<u8>,
    ) -> Result<Next, AttachError> {
        ctx.out.write_all(RESET.as_bytes())?;
        let mut overlay_from = overlay_from;
        let mut model = PickerModel::new(self.snapshot()?, self.shortcuts);
        model.picker.notice = notice;
        let from_picker = true;
        let mut last = String::new();
        let mut pending = if rest.is_empty() { None } else { Some(rest) };
        loop {
            let frame = render_picker(&model, ctx.size.0, ctx.size.1);
            if frame != last {
                ctx.out.write_all(frame.as_bytes())?;
                ctx.out.flush()?;
                last = frame;
            }
            let bytes = if let Some(b) = pending.take() {
                b
            } else {
                match ctx.events.recv_timeout(self.tick) {
                    Ok(Event::Stdin(b)) => b,
                    Ok(Event::Resize(c, r)) => {
                        ctx.size = (c, r);
                        last.clear();
                        continue;
                    }
                    Ok(Event::Pane { .. }) => continue,
                    Ok(Event::StdinClosed) | Err(RecvTimeoutError::Disconnected) => {
                        return Ok(Next::Done(Exit::InputClosed));
                    }
                    Err(RecvTimeoutError::Timeout) => {
                        match Snapshot::load(&self.control, (self.clock)()) {
                            Ok(s) => model.refresh(s),
                            Err(_) => model.mark_daemon_lost(),
                        }
                        continue;
                    }
                }
            };
            for key in decode_picker_keys(&bytes) {
                match model.key(&key) {
                    Outcome::Continue => {}
                    Outcome::Quit => {
                        return Ok(match overlay_from {
                            Some(pane) => Next::Pane { pane, from_picker },
                            None => Next::Done(Exit::Quit),
                        });
                    }
                    Outcome::Attach(id) => {
                        if let Some(pane) = model.snap.entries.get(&id).and_then(|e| e.pane) {
                            return Ok(Next::Pane { pane, from_picker });
                        }
                    }
                    Outcome::Create(req) => match self.create(ctx, &model, req) {
                        Ok(entry) => match entry.pane {
                            Some(pane) => return Ok(Next::Pane { pane, from_picker }),
                            None => {
                                model.picker.notice = Some("the new session has no pane".into())
                            }
                        },
                        Err(msg) => model.picker.notice = Some(msg),
                    },
                    Outcome::Kill(id) => {
                        if let Some(p) = model.snap.entries.get(&id).and_then(|e| e.pane) {
                            if overlay_from == Some(p) {
                                overlay_from = None;
                            }
                        }
                        match self.control.kill_session(id) {
                            Ok(()) => {
                                if let Ok(s) = self.snapshot() {
                                    model.refresh(s);
                                }
                            }
                            Err(e) => model.picker.notice = Some(format!("could not kill: {e}")),
                        }
                    }
                    Outcome::Rename(id, name) => match self.control.rename_session(id, &name) {
                        Ok(()) => {
                            if let Ok(s) = self.snapshot() {
                                model.refresh(s);
                            }
                        }
                        Err(e) => model.picker.notice = Some(format!("could not rename: {e}")),
                    },
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(live: bool, pane: Option<PaneId>) -> Entry {
        Entry {
            id: SessionId::from_seed("s"),
            name: "alpha".into(),
            panes: 1,
            live,
            created_at_unix: 0,
            pane,
        }
    }

    #[test]
    fn a_live_session_with_a_pane_attaches() {
        let p = PaneId::from_seed("p");
        assert_eq!(live_pane(entry(true, Some(p))).unwrap(), p);
    }

    #[test]
    fn a_latent_session_is_a_typed_refusal() {
        let p = PaneId::from_seed("p");
        assert!(matches!(
            live_pane(entry(false, Some(p))),
            Err(AttachError::TargetLatent(n)) if n == "alpha"
        ));
        assert!(matches!(
            live_pane(entry(true, None)),
            Err(AttachError::TargetLatent(_))
        ));
    }
}
