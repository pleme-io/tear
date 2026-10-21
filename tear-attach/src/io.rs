use tear_client::Client;
use tear_types::{Capability, ControlError, MultiplexerControl, PaneId};

#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum IoError {
    #[error("the pane is gone")]
    Gone,
    #[error("the daemon connection was lost: {0}")]
    Lost(String),
    #[error("the daemon refused: {0}")]
    Rejected(String),
}

impl From<ControlError> for IoError {
    fn from(e: ControlError) -> Self {
        match e {
            ControlError::NoSuchPane(_) | ControlError::NoSuchSession(_) => Self::Gone,
            ControlError::Rejected(m) => Self::Rejected(m),
            other => Self::Lost(other.to_string()),
        }
    }
}

pub type Sink = Box<dyn FnMut(&[u8]) + Send>;
pub type Subscription = Box<dyn Send>;

pub trait PaneIo: Send + Sync {
    fn resize(&self, pane: PaneId, cols: u16, rows: u16) -> Result<(), IoError>;
    fn subscribe(&self, pane: PaneId, sink: Sink) -> Result<Subscription, IoError>;
    fn send(&self, pane: PaneId, bytes: &[u8]) -> Result<(), IoError>;
    fn stream_carries_replay(&self) -> bool;
    fn snapshot_paint(&self, pane: PaneId) -> Result<Vec<u8>, IoError>;
    fn pane_alive(&self, pane: PaneId) -> Result<bool, IoError>;
}

impl PaneIo for Client {
    fn resize(&self, pane: PaneId, cols: u16, rows: u16) -> Result<(), IoError> {
        Ok(self.pane_resize_absolute(pane, cols, rows)?)
    }

    fn subscribe(&self, pane: PaneId, sink: Sink) -> Result<Subscription, IoError> {
        let mut sink = sink;
        let handle = self.subscribe_pane_bytes(pane, move |b| sink(b))?;
        Ok(Box::new(handle))
    }

    fn send(&self, pane: PaneId, bytes: &[u8]) -> Result<(), IoError> {
        Ok(self.send_keys(pane, bytes)?)
    }

    fn stream_carries_replay(&self) -> bool {
        self.daemon().has(Capability::ReplayModes)
    }

    fn snapshot_paint(&self, pane: PaneId) -> Result<Vec<u8>, IoError> {
        let snap = self.pane_snapshot(pane)?;
        let mut out = Vec::new();
        for (i, row) in snap.to_text_rows().iter().enumerate() {
            if i > 0 {
                out.extend_from_slice(b"\r\n");
            }
            out.extend_from_slice(row.trim_end().as_bytes());
        }
        out.extend_from_slice(
            format!("\x1b[{};{}H", snap.cursor_row + 1, snap.cursor_col + 1).as_bytes(),
        );
        Ok(out)
    }

    fn pane_alive(&self, pane: PaneId) -> Result<bool, IoError> {
        match self.get_pane(pane) {
            Ok(p) => Ok(!matches!(p.state, tear_types::PaneState::Exited { .. })),
            Err(ControlError::NoSuchPane(_)) => Ok(false),
            Err(e) => Err(e.into()),
        }
    }
}
