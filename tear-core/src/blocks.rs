//! Pane-as-block — OSC 133 prompt-mark-driven block extractor.
//!
//! ## What this is
//!
//! Warp-class "every command + output is a discrete block"
//! semantics, daemon-native. Operators (and AI agents via mado
//! MCP) get to fetch one block instead of grepping a byte
//! stream. Sharper context, sharper answers, sharper UI.
//!
//! ## The OSC 133 contract
//!
//! Originated in iTerm2 / FinalTerm; adopted by VS Code, ghostty,
//! kitty, wezterm, and most modern terminal emulators. Shells
//! emit these markers via their PS1 (bash), prompt() (zsh
//! powerlevel10k), or starship's built-in support.
//!
//! ```text
//! OSC 133 ; A ST     — start of prompt
//! OSC 133 ; B ST     — end of prompt / start of command-line edit
//! OSC 133 ; C ST     — end of command-line / start of command output
//! OSC 133 ; D ; <n>  — end of output, exit code n
//! ```
//!
//! A single "block" is therefore a four-state state machine:
//! Idle → Prompt → Command → Output → Idle.
//!
//! ## What we capture
//!
//! For each completed block:
//! - `prompt`  — verbatim characters printed between A and B
//! - `command` — verbatim characters printed between B and C
//! - `output`  — verbatim characters printed between C and D
//! - `exit_code` — integer from the D marker (None if missing)
//! - `started_at_unix_ms` / `ended_at_unix_ms`
//!
//! ## Storage
//!
//! Per-pane `VecDeque<Block>` with a configurable cap (default
//! 10_000 blocks ≈ a few days of typical interactive shell).
//! Oldest blocks evict first when the cap is hit — same
//! ring-buffer shape `PaneRecording` uses.

use std::collections::VecDeque;
use std::time::{SystemTime, UNIX_EPOCH};

/// Re-export the wire shape so callers don't have to choose
/// between tear-core and tear-types — they're the same type.
pub use tear_types::Block;

/// Block-extractor state machine.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
enum Phase {
    /// Haven't seen an A marker yet, or the previous block
    /// closed and we're waiting for the next prompt.
    Idle,
    /// Inside the prompt — printable chars append to `prompt`.
    Prompt,
    /// Inside the command-line — printable chars append to
    /// `command` (operator's typed input, echoed back by the
    /// shell).
    Command,
    /// Inside output — printable chars append to `output`.
    Output,
}

pub struct BlockExtractor {
    blocks: VecDeque<Block>,
    cap: usize,
    current: Option<Block>,
    phase: Phase,
    next_index: u64,
    /// Last-known working directory from OSC 7. Captured into
    /// each block at prompt start so the block carries its
    /// own `cwd` even if the shell cd's mid-output.
    current_cwd: Option<String>,
    /// Provenance of the owning pane, stamped onto every block
    /// this extractor mints. Write-once via [`Self::stamp_yurai`].
    yurai: tear_types::Yurai,
}

impl Default for BlockExtractor {
    fn default() -> Self {
        Self::new(10_000)
    }
}

impl BlockExtractor {
    #[must_use]
    pub fn new(cap: usize) -> Self {
        Self {
            blocks: VecDeque::new(),
            cap,
            current: None,
            phase: Phase::Idle,
            next_index: 0,
            current_cwd: None,
            yurai: tear_types::Yurai::Unknown,
        }
    }

    /// Stamp the owning pane's provenance onto this extractor —
    /// **write-once**, and that is the whole point.
    ///
    /// [`Yurai`] is documented as a pane's provenance *for its
    /// whole life*, so a settable field would contradict the type
    /// it carries: an agent-spawned pane could re-stamp itself
    /// `Human` mid-session and every block after that point would
    /// lie, retroactively laundering the history a `freio` press
    /// is supposed to be able to trust.
    ///
    /// So the second call is refused rather than applied. Returns
    /// `true` when this call took effect. A refusal is not an
    /// error — re-stamping the SAME provenance is harmless and
    /// idempotent — but a refusal that would have CHANGED the
    /// value is a caller bug, and the return value is how a caller
    /// can notice.
    ///
    /// Tier-honest: **only-mitigated**. `Yurai::Automation` is a
    /// public variant, so in-process code can construct a
    /// provenance without holding an attested connection; what is
    /// closed here is *drift after the fact*, not *fabrication at
    /// the source*. Fabrication is bounded by
    /// [`tear_types::Yurai::from_shutai`] being the only
    /// production path, which is convention plus its own tests —
    /// not a compile error.
    ///
    /// [`Yurai`]: tear_types::Yurai
    pub fn stamp_yurai(&mut self, y: tear_types::Yurai) -> bool {
        if self.yurai == tear_types::Yurai::Unknown {
            self.yurai = y;
            true
        } else {
            false
        }
    }

    /// The provenance every block from this extractor carries.
    #[must_use]
    pub fn yurai(&self) -> &tear_types::Yurai {
        &self.yurai
    }

    /// Record the shell's current working directory. Called by
    /// the OSC 7 hook in GridState — the next prompt-start
    /// stamps this onto the new block. OSC 7 payload is
    /// typically `file://<host>/path/to/dir`; we strip the
    /// scheme + host to keep just the absolute path.
    pub fn set_cwd_from_osc7(&mut self, raw: &str) {
        if let Some(rest) = raw.strip_prefix("file://") {
            // Drop the host portion (anything before the first
            // '/' that starts the path).
            if let Some(slash) = rest.find('/') {
                self.current_cwd = Some(rest[slash..].to_owned());
                return;
            }
        }
        // Fallback: take the raw payload verbatim — some
        // shells emit just the path.
        self.current_cwd = Some(raw.to_owned());
    }

    /// Current OSC 7-set cwd, if any. Surfaced for tests and
    /// for renderers that want to display the active directory
    /// independent of any open block.
    #[must_use]
    pub fn current_cwd(&self) -> Option<&str> {
        self.current_cwd.as_deref()
    }

    /// Number of completed blocks currently retained.
    #[must_use]
    pub fn len(&self) -> usize {
        self.blocks.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.blocks.is_empty()
    }

    /// Iterate completed blocks oldest-first.
    pub fn iter(&self) -> impl Iterator<Item = &Block> {
        self.blocks.iter()
    }

    /// Fetch a single block by its per-pane index. Returns
    /// `None` when the block has been evicted (index < oldest)
    /// or never existed (index > latest completed).
    #[must_use]
    pub fn get(&self, index: u64) -> Option<&Block> {
        self.blocks.iter().find(|b| b.index == index)
    }

    /// In-progress block (if any). Useful for live "what's
    /// running right now" introspection.
    #[must_use]
    pub fn current(&self) -> Option<&Block> {
        self.current.as_ref()
    }

    /// Append a printable char to whatever phase we're in.
    /// No-op when Idle. Called from the vte Perform::print hook.
    pub fn on_print(&mut self, c: char) {
        let Some(block) = self.current.as_mut() else {
            return;
        };
        match self.phase {
            Phase::Prompt => block.prompt.push(c),
            Phase::Command => block.command.push(c),
            Phase::Output => block.output.push(c),
            Phase::Idle => {}
        }
    }

    /// Append raw bytes (for non-Print events like CR/LF/Esc).
    /// The output phase wants the full byte stream so a replayer
    /// can reproduce escape sequences. Phases Prompt/Command
    /// keep only printable chars (escapes there are usually
    /// terminal-renderer concerns).
    pub fn on_raw_byte(&mut self, b: u8) {
        let Some(block) = self.current.as_mut() else {
            return;
        };
        if matches!(self.phase, Phase::Output) {
            // Push as UTF-8 — \r\n stays as-is, escape bytes
            // become control chars in the String.
            block.output.push(b as char);
        }
    }

    /// Handle an OSC 133 marker. `marker` is the second OSC
    /// param (the `A` / `B` / `C` / `D[;<n>]` part).
    pub fn on_osc_133(&mut self, marker: &str) {
        let kind = marker.chars().next().unwrap_or(' ');
        match kind {
            'A' => self.start_prompt(),
            'B' => self.start_command(),
            'C' => self.start_output(),
            'D' => self.end_output(parse_exit_code(marker)),
            _ => {}
        }
    }

    fn start_prompt(&mut self) {
        // If a block is still open (e.g. shell emitted A
        // without a D), close it as orphaned and start fresh.
        if self.current.is_some() {
            self.finalize_current(None);
        }
        let now = now_ms();
        self.current = Some(Block {
            index: self.next_index,
            prompt: String::new(),
            command: String::new(),
            output: String::new(),
            exit_code: None,
            started_at_unix_ms: now,
            ended_at_unix_ms: None,
            cwd: self.current_cwd.clone(),
            // Stamped at prompt START, not at completion: the
            // question "who ran this" is settled when the block
            // is minted, so a block that never finishes still
            // carries its attribution.
            yurai: self.yurai.clone(),
        });
        self.next_index += 1;
        self.phase = Phase::Prompt;
    }

    fn start_command(&mut self) {
        if self.current.is_some() {
            self.phase = Phase::Command;
        }
    }

    fn start_output(&mut self) {
        if self.current.is_some() {
            self.phase = Phase::Output;
        }
    }

    fn end_output(&mut self, exit_code: Option<i32>) {
        self.finalize_current(exit_code);
    }

    fn finalize_current(&mut self, exit_code: Option<i32>) {
        let Some(mut block) = self.current.take() else {
            return;
        };
        block.exit_code = exit_code;
        block.ended_at_unix_ms = Some(now_ms());
        if self.blocks.len() == self.cap {
            self.blocks.pop_front();
        }
        self.blocks.push_back(block);
        self.phase = Phase::Idle;
    }
}

fn parse_exit_code(marker: &str) -> Option<i32> {
    // D ; <n>  — split on ';' and parse the second segment.
    marker.split(';').nth(1).and_then(|s| s.trim().parse().ok())
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn idle_extractor_drops_prints() {
        let mut bx = BlockExtractor::default();
        bx.on_print('x');
        assert!(bx.is_empty());
        assert!(bx.current().is_none());
    }

    #[test]
    fn full_block_lifecycle_captures_all_phases() {
        let mut bx = BlockExtractor::default();
        // OSC 133 A — prompt start
        bx.on_osc_133("A");
        for c in "$ ".chars() {
            bx.on_print(c);
        }
        // OSC 133 B — command start
        bx.on_osc_133("B");
        for c in "ls".chars() {
            bx.on_print(c);
        }
        // OSC 133 C — output start
        bx.on_osc_133("C");
        for c in "a b c".chars() {
            bx.on_print(c);
        }
        // OSC 133 D ; 0 — exit 0
        bx.on_osc_133("D;0");

        assert_eq!(bx.len(), 1);
        let b = bx.iter().next().unwrap();
        assert_eq!(b.prompt, "$ ");
        assert_eq!(b.command, "ls");
        assert_eq!(b.output, "a b c");
        assert_eq!(b.exit_code, Some(0));
        assert!(b.ended_at_unix_ms.is_some());
        assert_eq!(b.index, 0);
        assert!(bx.current().is_none());
    }

    #[test]
    fn exit_code_optional_when_d_marker_omits_it() {
        let mut bx = BlockExtractor::default();
        bx.on_osc_133("A");
        bx.on_osc_133("B");
        bx.on_osc_133("C");
        bx.on_osc_133("D");
        let b = bx.iter().next().unwrap();
        assert_eq!(b.exit_code, None);
    }

    #[test]
    fn unfinished_block_is_orphaned_when_next_prompt_starts() {
        let mut bx = BlockExtractor::default();
        bx.on_osc_133("A");
        for c in "p1".chars() {
            bx.on_print(c);
        }
        // Shell glitches — sends another A without D.
        bx.on_osc_133("A");
        for c in "p2".chars() {
            bx.on_print(c);
        }
        bx.on_osc_133("B");
        bx.on_osc_133("C");
        bx.on_osc_133("D;0");

        assert_eq!(bx.len(), 2);
        let mut iter = bx.iter();
        let first = iter.next().unwrap();
        let second = iter.next().unwrap();
        assert_eq!(first.prompt, "p1");
        assert_eq!(first.exit_code, None);
        assert_eq!(second.prompt, "p2");
        assert_eq!(second.exit_code, Some(0));
    }

    #[test]
    fn ring_buffer_caps_at_max() {
        let mut bx = BlockExtractor::new(3);
        for i in 0..5 {
            bx.on_osc_133("A");
            for c in format!("p{i}").chars() {
                bx.on_print(c);
            }
            bx.on_osc_133("D;0");
        }
        assert_eq!(bx.len(), 3);
        // Oldest two evicted; remaining indices are 2, 3, 4.
        let indices: Vec<u64> = bx.iter().map(|b| b.index).collect();
        assert_eq!(indices, vec![2, 3, 4]);
    }

    #[test]
    fn get_by_index_returns_block_or_none() {
        let mut bx = BlockExtractor::default();
        bx.on_osc_133("A");
        bx.on_osc_133("D;0");
        bx.on_osc_133("A");
        bx.on_osc_133("D;1");

        assert_eq!(bx.get(0).map(|b| b.exit_code), Some(Some(0)));
        assert_eq!(bx.get(1).map(|b| b.exit_code), Some(Some(1)));
        assert!(bx.get(99).is_none());
    }

    #[test]
    fn osc7_cwd_stamped_onto_next_block() {
        let mut bx = BlockExtractor::default();
        bx.set_cwd_from_osc7("file://localhost/Users/me/code");
        bx.on_osc_133("A");
        bx.on_osc_133("D;0");
        let b = bx.iter().next().unwrap();
        assert_eq!(b.cwd.as_deref(), Some("/Users/me/code"));
    }

    #[test]
    fn osc7_without_file_scheme_passes_through_verbatim() {
        let mut bx = BlockExtractor::default();
        bx.set_cwd_from_osc7("/tmp/raw-path");
        assert_eq!(bx.current_cwd(), Some("/tmp/raw-path"));
    }

    #[test]
    fn block_duration_ms_computes_on_finalize() {
        let mut bx = BlockExtractor::default();
        bx.on_osc_133("A");
        std::thread::sleep(std::time::Duration::from_millis(5));
        bx.on_osc_133("D;0");
        let b = bx.iter().next().unwrap();
        let d = b.duration_ms().expect("finalized block has duration");
        assert!(d < 5_000, "absurd duration: {d}ms");
    }

    #[test]
    fn parse_exit_code_handles_typical_shapes() {
        assert_eq!(parse_exit_code("D"), None);
        assert_eq!(parse_exit_code("D;"), None);
        assert_eq!(parse_exit_code("D;0"), Some(0));
        assert_eq!(parse_exit_code("D;127"), Some(127));
        assert_eq!(parse_exit_code("D ; 130"), Some(130));
    }

    // ── attribution (the naturalize(Superlogical) delta) ──────────
    //
    // Superlogical's product unit is the terminal block. Ours was
    // too, and was ANONYMOUS: an agent-run command and an
    // operator-run one produced identical rows. These pin the
    // field that makes a block history worth trusting.

    /// Drive one full A→B→C→D block through the extractor.
    fn one_block(ex: &mut BlockExtractor) {
        ex.on_osc_133("A");
        ex.on_osc_133("B");
        for c in "echo hi".chars() {
            ex.on_print(c);
        }
        ex.on_osc_133("C");
        ex.on_osc_133("D;0");
    }

    #[test]
    fn an_unstamped_extractor_mints_unknown_never_human() {
        let mut ex = BlockExtractor::new(8);
        one_block(&mut ex);
        assert_eq!(
            ex.get(0).unwrap().yurai,
            tear_types::Yurai::Unknown,
            "an unattributed block must stay Unknown — defaulting to Human \
             would launder every agent-run command in the history"
        );
    }

    #[test]
    fn a_stamped_extractor_marks_every_block_it_mints() {
        let mut ex = BlockExtractor::new(8);
        assert!(ex.stamp_yurai(tear_types::Yurai::Automation {
            label: Some("claude-code".into())
        }));
        one_block(&mut ex);
        one_block(&mut ex);
        for i in 0..2 {
            assert!(
                ex.get(i).unwrap().yurai.is_automation(),
                "block {i} lost its attribution"
            );
        }
    }

    /// The write-once rule, and why it is not merely tidiness: a
    /// re-stampable field would let an agent-spawned pane relabel
    /// itself `Human` mid-session, retroactively laundering every
    /// block after that point.
    #[test]
    fn re_stamping_is_refused_so_provenance_cannot_drift() {
        let mut ex = BlockExtractor::new(8);
        assert!(ex.stamp_yurai(tear_types::Yurai::Automation { label: None }));
        assert!(
            !ex.stamp_yurai(tear_types::Yurai::Human),
            "the second stamp must be REFUSED, not applied"
        );
        one_block(&mut ex);
        assert!(
            ex.get(0).unwrap().yurai.is_automation(),
            "an agent pane must not be able to relabel itself human"
        );
    }

    /// A block written before this field existed decodes as
    /// `Unknown` — which is what its absence honestly means.
    /// Same discipline as `Yurai`'s own pre-field decode test.
    #[test]
    fn a_pre_attribution_block_decodes_as_unknown() {
        let legacy = r#"{
            "index": 0, "prompt": "$ ", "command": "ls", "output": "a\n",
            "exit_code": 0, "started_at_unix_ms": 1, "ended_at_unix_ms": 2
        }"#;
        let b: Block = serde_json::from_str(legacy).expect("legacy block must still decode");
        assert_eq!(b.yurai, tear_types::Yurai::Unknown);
        assert_eq!(b.cwd, None);
    }
}
