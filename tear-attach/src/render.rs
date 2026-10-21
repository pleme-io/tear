use std::fmt::Write as _;

use tear_picker::RowKind;
use unicode_width::UnicodeWidthStr;

use crate::model::{Mode, PickerModel};

pub const REVERSE: &str = "\x1b[7m";
pub const DIM: &str = "\x1b[2m";
pub const BOLD: &str = "\x1b[1m";
pub const OFF: &str = "\x1b[0m";
pub const LIVE: &str = "\u{25cf}";
pub const LATENT: &str = "\u{25cb}";

#[must_use]
pub fn age(secs: u64) -> String {
    match secs {
        0..=59 => "now".to_owned(),
        60..=3599 => format!("{}m", secs / 60),
        3600..=86399 => format!("{}h", secs / 3600),
        _ => format!("{}d", secs / 86400),
    }
}

fn fit(text: &str, width: usize) -> String {
    let mut out = String::new();
    let mut used = 0;
    for ch in text.chars() {
        let w = unicode_width::UnicodeWidthChar::width(ch).unwrap_or(0);
        if used + w > width {
            break;
        }
        out.push(ch);
        used += w;
    }
    out
}

fn pad(text: &str, width: usize) -> String {
    let fitted = fit(text, width);
    let w = UnicodeWidthStr::width(fitted.as_str());
    let mut s = fitted;
    for _ in w..width {
        s.push(' ');
    }
    s
}

#[must_use]
pub fn render_picker(model: &PickerModel, cols: u16, rows: u16) -> String {
    let cols = usize::from(cols.max(20));
    let rows = usize::from(rows.max(6));
    let mut out = String::from("\x1b[?25l\x1b[H\x1b[2J");
    let sessions = model.snap.entries.len();
    let live = model.snap.entries.values().filter(|e| e.live).count();
    let _ = write!(
        out,
        "{BOLD}{}{OFF}\r\n",
        fit(
            &format!("tear sessions  {sessions} total, {live} live"),
            cols
        )
    );
    if model.daemon_lost {
        let _ = write!(
            out,
            "\u{26a0} daemon unreachable: showing the last list\r\n"
        );
    } else if sessions == 0 {
        let _ = write!(out, "{DIM}no sessions yet: n or Enter creates one{OFF}\r\n");
    } else {
        out.push_str("\r\n");
    }
    let prompt = match &model.mode {
        Mode::Browse => format!("> {}", model.picker.query),
        Mode::Rename { buf, .. } => format!("rename: {buf}"),
        Mode::NewName { buf } => format!("new session name: {buf}"),
        Mode::ConfirmKill { name, .. } => format!("kill `{name}`? y = yes, any other key cancels"),
    };
    let _ = write!(out, "{}\r\n", fit(&prompt, cols));
    let list_rows = rows.saturating_sub(6).max(1);
    let total = model.picker.results.len();
    let start = if model.picker.selected >= list_rows {
        model.picker.selected + 1 - list_rows
    } else {
        0
    };
    for (i, row) in model
        .picker
        .results
        .iter()
        .enumerate()
        .skip(start)
        .take(list_rows)
    {
        let (badge, detail) = match &row.kind {
            RowKind::Switch(id) => match model.snap.entries.get(id) {
                Some(e) => {
                    let when = age(model.snap.now.saturating_sub(e.created_at_unix));
                    (
                        if e.live { LIVE } else { LATENT },
                        format!(
                            "{} pane{}  {}{}",
                            e.panes,
                            if e.panes == 1 { "" } else { "s" },
                            when,
                            if e.live { "" } else { "  latent" }
                        ),
                    )
                }
                None => (LATENT, String::new()),
            },
            _ => (" ", String::new()),
        };
        let detail_w = UnicodeWidthStr::width(detail.as_str());
        let name_w = cols.saturating_sub(detail_w + 5).max(4);
        let line = format!(
            "{badge} {} {}",
            pad(&row.label, name_w),
            fit(&detail, cols.saturating_sub(name_w + 4))
        );
        if i == model.picker.selected {
            let _ = write!(out, "{REVERSE}{}{OFF}\r\n", pad(&line, cols));
        } else {
            let _ = write!(out, "{}\r\n", fit(&line, cols));
        }
    }
    if total == 0 {
        let _ = write!(out, "{DIM}no matches{OFF}\r\n");
    }
    if let Some(n) = &model.picker.notice {
        let _ = write!(out, "{}\r\n", fit(n, cols));
    }
    let _ = write!(
        out,
        "\x1b[{rows};1H{DIM}{}{OFF}",
        fit(
            "Enter attach  n new  x kill  ctrl+e rename  Esc/q quit  type to filter",
            cols
        )
    );
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Snapshot;
    use std::sync::Arc;
    use tear_types::{MultiplexerControl, SessionSource};

    fn model(names: &[&str]) -> PickerModel {
        let inproc = Arc::new(tear_core::InProcess::new());
        inproc.set_spawn_env(tear_types::SpawnEnv::none());
        for n in names {
            inproc
                .new_session_with_source_and_size(n, "/bin/sh", &[], SessionSource::Human, (80, 24))
                .unwrap();
        }
        let dynamic: Arc<dyn MultiplexerControl> = inproc;
        PickerModel::new(Snapshot::load(&dynamic, u64::MAX / 2).unwrap(), true)
    }

    #[test]
    fn rows_show_name_badge_pane_count_and_age() {
        let m = model(&["alpha"]);
        let s = render_picker(&m, 80, 24);
        assert!(s.contains("alpha"));
        assert!(s.contains(LIVE));
        assert!(s.contains("1 pane"));
        assert!(s.contains("tear sessions  1 total, 1 live"));
    }

    #[test]
    fn a_latent_row_wears_the_latent_badge() {
        let mut m = model(&["alpha"]);
        for e in m.snap.entries.values_mut() {
            e.live = false;
        }
        let s = render_picker(&m, 80, 24);
        assert!(s.contains(LATENT));
        assert!(s.contains("latent"));
    }

    #[test]
    fn the_highlighted_row_is_reversed() {
        let m = model(&["alpha"]);
        assert!(render_picker(&m, 80, 24).contains(REVERSE));
    }

    #[test]
    fn an_empty_daemon_says_so() {
        let m = model(&[]);
        assert!(render_picker(&m, 80, 24).contains("no sessions yet"));
    }

    #[test]
    fn a_lost_daemon_is_flagged_not_blank() {
        let mut m = model(&["alpha"]);
        m.mark_daemon_lost();
        let s = render_picker(&m, 80, 24);
        assert!(s.contains("daemon unreachable"));
        assert!(s.contains("alpha"));
    }

    #[test]
    fn the_confirm_prompt_names_the_session() {
        let mut m = model(&["alpha"]);
        m.key(&tear_picker::PickerKey::text("x"));
        assert!(render_picker(&m, 80, 24).contains("kill `alpha`?"));
    }

    #[test]
    fn narrow_terminals_do_not_overflow() {
        let m = model(&["a-very-long-session-name-indeed"]);
        for line in render_picker(&m, 24, 10).split("\r\n") {
            let visible: String = {
                let mut v = String::new();
                let mut esc = false;
                for ch in line.chars() {
                    if esc {
                        if ch.is_ascii_alphabetic() {
                            esc = false;
                        }
                    } else if ch == '\x1b' {
                        esc = true;
                    } else {
                        v.push(ch);
                    }
                }
                v
            };
            assert!(UnicodeWidthStr::width(visible.as_str()) <= 24, "{visible}");
        }
    }

    #[test]
    fn ages_round_down_to_the_largest_unit() {
        assert_eq!(age(5), "now");
        assert_eq!(age(120), "2m");
        assert_eq!(age(7200), "2h");
        assert_eq!(age(172_800), "2d");
    }
}
