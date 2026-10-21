use std::io::{IsTerminal, Read, Write};
use std::sync::mpsc::Sender;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use crossterm::terminal::{disable_raw_mode, enable_raw_mode, size};
use signal_hook::consts::SIGWINCH;
use signal_hook::iterator::Signals;
use tear_attach::Event;

pub struct RawTerminal;

impl RawTerminal {
    pub fn enter() -> Result<Self> {
        if !std::io::stdin().is_terminal() || !std::io::stdout().is_terminal() {
            bail!(
                "tear attach needs a terminal on stdin and stdout; over ssh, use `ssh -t host tear attach`"
            );
        }
        enable_raw_mode().context("could not put the terminal in raw mode")?;
        Ok(Self)
    }
}

impl Drop for RawTerminal {
    fn drop(&mut self) {
        let _ = disable_raw_mode();
        let _ = std::io::stdout().flush();
    }
}

pub fn terminal_size() -> (u16, u16) {
    size().unwrap_or((80, 24))
}

pub fn spawn_input(tx: Sender<Event>) {
    std::thread::Builder::new()
        .name("tear-attach-stdin".into())
        .spawn(move || {
            let mut stdin = std::io::stdin().lock();
            let mut buf = [0u8; 4096];
            loop {
                match stdin.read(&mut buf) {
                    Ok(0) | Err(_) => {
                        let _ = tx.send(Event::StdinClosed);
                        return;
                    }
                    Ok(n) => {
                        if tx.send(Event::Stdin(buf[..n].to_vec())).is_err() {
                            return;
                        }
                    }
                }
            }
        })
        .ok();
}

pub fn spawn_winch(tx: Sender<Event>) -> Result<()> {
    let mut signals = Signals::new([SIGWINCH]).context("could not watch for window resizes")?;
    std::thread::Builder::new()
        .name("tear-attach-winch".into())
        .spawn(move || {
            for _ in signals.forever() {
                let (c, r) = terminal_size();
                if tx.send(Event::Resize(c, r)).is_err() {
                    return;
                }
            }
        })
        .ok();
    Ok(())
}

pub const SETTLE: Duration = Duration::from_millis(500);
