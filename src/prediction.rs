//! Speculative local echo: mosh-go pending-list core + stock overlay semantics.
//!
//! Base API matches [mosh-go `predict.go`](https://github.com/unixshells/mosh-go):
//! pending `(rune, x, y)`, Confirm, Overlay, single Diff paint path.
//!
//! Prediction rules follow stock mobile-shell/mosh `terminaloverlay.cc` /
//! `terminaloverlay.h` (not system mosh-client, terminfo, or Cygwin):
//! - Epoch start `prediction=1` / `confirmed=0` — hide until credited Correct
//! - `reset()` only becomes tentative (does not re-align confirmed)
//! - Underline **flagging** hysteresis (80/50 ms), separate from show
//! - Overlay: blank-on-blank no under; unknown underline-only (skip last col);
//!   known cells apply only when differing from host, then flag under
//! - Insert/BS shift the **full remaining row** (stock); overwrite BS → space
//! - Glitch: any non-zero `glitch_trigger` forces show; `> REPAIR_COUNT` flags
//! - Frame-ack Pending via late_ack (`echo_ack_num`) vs expiration_sent
//!
//! Never dual-write raw glyphs beside HostBytes. Pure Rust binary only.

use std::time::{Duration, Instant};

use crate::framebuffer::Framebuffer;

/// Stock adaptive hysteresis (terminaloverlay.h):
/// - HIGH: start showing predictions
/// - LOW: stop only when no pending predictions are active
const SRTT_TRIGGER_HIGH: Duration = Duration::from_millis(30);
const SRTT_TRIGGER_LOW: Duration = Duration::from_millis(20);

/// Stock underline flagging hysteresis.
const FLAG_TRIGGER_HIGH: Duration = Duration::from_millis(80);
const FLAG_TRIGGER_LOW: Duration = Duration::from_millis(50);

/// Stock glitch thresholds (ms).
const GLITCH_THRESHOLD: Duration = Duration::from_millis(250);
const GLITCH_FLAG_THRESHOLD: Duration = Duration::from_millis(5000);
const GLITCH_REPAIR_COUNT: u32 = 10;
const GLITCH_REPAIR_MININTERVAL: Duration = Duration::from_millis(150);

/// Bound pending lifetime. Longer than mosh-go's 500ms so high-latency
/// links can still confirm (stock uses frame-ack expiry, not a short wall clock).
const PREDICTION_TIMEOUT: Duration = Duration::from_secs(15);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DisplayPreference {
    Always,
    Never,
    Adaptive,
}

impl DisplayPreference {
    pub fn from_env_value(raw: &str) -> Self {
        match raw.trim().to_ascii_lowercase().as_str() {
            "always" | "yes" | "1" | "true" | "on" => Self::Always,
            "never" | "no" | "0" | "false" | "off" => Self::Never,
            "adaptive" | "" => Self::Adaptive,
            _ => Self::Adaptive,
        }
    }

    /// Default **adaptive** (stock mosh default) once the paint path is
    /// Framebuffer-safe. Set `MOSH_PREDICTION_DISPLAY=never` to force off.
    pub fn from_env() -> Self {
        match std::env::var("MOSH_PREDICTION_DISPLAY") {
            Ok(v) => Self::from_env_value(&v),
            Err(_) => Self::Adaptive,
        }
    }
}

#[derive(Debug, Clone)]
struct Prediction {
    ch: char,
    x: usize,
    y: usize,
    /// Stock `tentative_until_epoch`.
    epoch: u64,
    at: Instant,
    /// Stock `expiration_frame` proxy: Pending while acked < this sent watermark.
    expiration_sent: u64,
    /// Content present on host at predict time (CorrectNoCredit if match is noop).
    original_ch: char,
    /// Stock `unknown` — never diverge; CorrectNoCredit only.
    unknown: bool,
    /// After a credited Correct, stock copies host renditions to the rest of the row.
    overlay_attr: Option<crate::framebuffer::Attr>,
}

/// mosh-go pending list + stock tentative / frame-ack / flagging / BS / arrows.
#[derive(Debug)]
pub struct Predictor {
    pending: Vec<Prediction>,
    cur_x: usize,
    cur_y: usize,
    /// Stock `prediction_epoch` — new preds get this as tentative_until.
    prediction_epoch: u64,
    /// Stock `confirmed_epoch` — preds with epoch > this are hidden (tentative).
    confirmed_epoch: u64,
    active: bool,
    confirmed: usize,
    preference: DisplayPreference,
    show: bool,
    flagging: bool,
    glitch_trigger: u32,
    esc_buf: Vec<u8>,
    /// Stock local_frame_sent / local_frame_acked (SSP early ack = transport).
    local_frame_sent: u64,
    local_frame_acked: u64,
    /// Stock `local_frame_late_acked` from HostInstruction.echo_ack_num.
    local_frame_late_acked: u64,
    /// Cursor-only prediction expiry (stock ConditionalCursorMove).
    cursor_exp_sent: Option<u64>,
    last_quick_confirmation: Option<Instant>,
    /// Stock predict_overwrite (env MOSH_PREDICTION_OVERWRITE).
    overwrite: bool,
}

impl Predictor {
    pub fn new(preference: DisplayPreference) -> Self {
        Self {
            pending: Vec::new(),
            cur_x: 0,
            cur_y: 0,
            // Stock PredictionEngine: prediction_epoch=1, confirmed_epoch=0 so
            // the first band is tentative until a credited Correct proves it.
            prediction_epoch: 1,
            confirmed_epoch: 0,
            active: false,
            confirmed: 0,
            preference,
            // Stock: Always forces *show*, not flagging. Flagging follows
            // send_interval hysteresis (and big glitch) in set_srtt.
            show: matches!(preference, DisplayPreference::Always),
            flagging: false,
            glitch_trigger: 0,
            esc_buf: Vec::new(),
            local_frame_sent: 0,
            local_frame_acked: 0,
            local_frame_late_acked: 0,
            cursor_exp_sent: None,
            last_quick_confirmation: None,
            overwrite: std::env::var("MOSH_PREDICTION_OVERWRITE")
                .map(|v| matches!(v.to_ascii_lowercase().as_str(), "1" | "true" | "yes" | "on"))
                .unwrap_or(false),
        }
    }

    pub fn preference(&self) -> DisplayPreference {
        self.preference
    }

    /// Update SSP frame watermarks.
    ///
    /// - `sent` / `early_acked`: transport SSP state numbers
    /// - `late_acked`: stock `echo_ack` from HostInstruction (Pending gate)
    ///
    /// Confirm Pending uses **late_acked** (stock `get_validity` ignores early_ack).
    pub fn set_frames(&mut self, sent: u64, early_acked: u64, late_acked: u64) {
        if sent > self.local_frame_sent {
            self.local_frame_sent = sent;
        }
        if early_acked > self.local_frame_acked {
            self.local_frame_acked = early_acked;
        }
        if late_acked > self.local_frame_late_acked {
            self.local_frame_late_acked = late_acked;
        }
    }

    /// Convenience for tests: set sent + both acks to the same watermark.
    #[cfg(test)]
    pub fn set_frames_simple_for_test(&mut self, sent: u64, acked: u64) {
        self.set_frames(sent, acked, acked);
    }

    /// Effective Pending watermark (stock late_ack).
    fn late_ack(&self) -> u64 {
        self.local_frame_late_acked
    }

    /// Stock hysteresis for show + flagging + glitch sampling.
    pub fn set_srtt(&mut self, srtt: Option<Duration>) {
        match self.preference {
            DisplayPreference::Never => {
                self.show = false;
                self.flagging = false;
                return;
            }
            DisplayPreference::Always => {
                // Stock: Always only forces *display* of predictions.
                self.show = true;
            }
            DisplayPreference::Adaptive => {
                let Some(d) = srtt else {
                    return;
                };
                // Show trigger (stock SRTT_TRIGGER_*)
                if d > SRTT_TRIGGER_HIGH {
                    self.show = true;
                } else if d <= SRTT_TRIGGER_LOW {
                    // Stock: clear srtt_trigger only when !active().
                    // active() includes cursor-only Pending (cursor_exp_sent).
                    if self.pending.is_empty() && self.cursor_exp_sent.is_none() {
                        self.show = false;
                        self.active = false;
                    }
                }
            }
        }

        // Flagging hysteresis is independent of Always (stock FLAG_TRIGGER_*).
        if let Some(d) = srtt {
            if d > FLAG_TRIGGER_HIGH {
                self.flagging = true;
            } else if d <= FLAG_TRIGGER_LOW {
                self.flagging = false;
            }
        }
        // Stock: any non-zero glitch_trigger participates in show;
        // really-big glitches also force underlining.
        if self.glitch_trigger > 0 {
            self.show = true;
        }
        if self.glitch_trigger > GLITCH_REPAIR_COUNT {
            self.flagging = true;
        }
    }

    /// Whether overlays should be applied (preference + adaptive trigger).
    pub fn should_show(&self) -> bool {
        self.show
    }

    /// Whether predicted cells get underline (stock flagging).
    pub fn flagging(&self) -> bool {
        self.flagging
    }

    pub fn pending_len(&self) -> usize {
        self.pending.len()
    }

    /// Pending prediction rune at index (tests / diagnostics).
    pub fn pending_char(&self, index: usize) -> Option<char> {
        self.pending.get(index).map(|p| p.ch)
    }

    /// Pending prediction position at index (tests / diagnostics).
    pub fn pending_pos(&self, index: usize) -> Option<(usize, usize)> {
        self.pending.get(index).map(|p| (p.x, p.y))
    }

    pub fn cur_x(&self) -> usize {
        self.cur_x
    }

    pub fn cur_y(&self) -> usize {
        self.cur_y
    }

    /// mosh-go `Active`.
    pub fn active(&self) -> bool {
        // Include cursor-only prediction (arrows / BS with empty pending).
        self.active
    }

    /// Process keystrokes. `fb` is the host Framebuffer (for width / last-col).
    /// Stock Adaptive still builds predictions when not showing; only Overlay is gated.
    pub fn keystroke(&mut self, input: &[u8], fb: &Framebuffer) {
        if self.preference == DisplayPreference::Never {
            self.reset();
            return;
        }
        let data: Vec<u8> = if self.esc_buf.is_empty() {
            input.to_vec()
        } else {
            let mut v = std::mem::take(&mut self.esc_buf);
            v.extend_from_slice(input);
            v
        };
        let mut i = 0;
        while i < data.len() {
            if data[i] == 0x1b {
                match self.try_parse_arrow(&data[i..], fb) {
                    ArrowParse::NeedMore => {
                        self.esc_buf = data[i..].to_vec();
                        return;
                    }
                    ArrowParse::Handled(n) => {
                        i += n;
                        continue;
                    }
                    ArrowParse::NotArrow(n) => {
                        i += n.max(1);
                        self.become_tentative();
                        self.esc_buf.clear();
                        continue;
                    }
                }
            }

            // Incomplete multi-byte UTF-8 across keystroke chunks (ConPTY splits).
            match decode_utf8_input(&data, i) {
                Utf8Input::NeedMore => {
                    self.esc_buf = data[i..].to_vec();
                    return;
                }
                Utf8Input::Invalid(n) => {
                    i += n.max(1);
                    self.become_tentative();
                    continue;
                }
                Utf8Input::Char(ch, len) => {
                    i += len;
                    self.handle_decoded_char(ch, fb);
                }
            }
        }
    }

    fn handle_decoded_char(&mut self, ch: char, fb: &Framebuffer) {
        if ch == '\u{FFFD}' {
            self.become_tentative();
            return;
        }
        // Stock: only DEL (0x7f) is predicted BS. BS (0x08) is Execute → tentative.
        if ch == '\u{7f}' {
            self.predict_backspace(fb);
            return;
        }
        if ch == '\u{08}' {
            self.become_tentative();
            return;
        }
        if (ch as u32) < 0x20 {
            self.become_tentative();
            if ch == '\r' {
                self.newline_carriage_return(fb);
            }
            return;
        }
        if !is_print(ch) {
            return;
        }
        let w = unicode_width_approx(ch);
        // Stock: non-width-1 (wide / combining / control print) → tentative only.
        if w != 1 {
            self.become_tentative();
            return;
        }
        let cx = self.cur_x;
        let cy = self.cur_y;
        // Last column: stock becomes tentative, places a *known* glyph, then
        // becomes tentative again and wraps (newline_carriage_return).
        let at_last_col = cx + 1 >= fb.cols;
        if at_last_col {
            self.become_tentative();
        }
        // Insert-mode: stock full-row shift via dense row map (one cell per col).
        if !self.overwrite && !at_last_col {
            self.predict_host_insert(fb, cx, cy, ch);
        } else if self.overwrite {
            let orig = fb.cell_at(cx, cy).map(|c| c.ch).unwrap_or(' ');
            // One cell per column: replace any prior pred at this coordinate
            // (retype in overwrite must not stack duplicates for Confirm).
            self.pending.retain(|p| !(p.y == cy && p.x == cx));
            // Stock places known glyph even on last col (epoch hide covers ambiguity).
            self.pending
                .push(self.make_pred(ch, cx, cy, orig, false));
            if at_last_col {
                self.become_tentative();
                self.newline_carriage_return(fb);
            } else {
                self.cur_x = cx.saturating_add(1);
                self.active = true;
                self.cursor_exp_sent = Some(self.local_frame_sent.saturating_add(1));
            }
            self.sort_pending();
        } else {
            // Insert at last col: place *known* glyph (not unknown), then wrap.
            let orig = fb.cell_at(cx, cy).map(|c| c.ch).unwrap_or(' ');
            self.pending.retain(|p| !(p.y == cy && p.x == cx));
            self.pending.push(self.make_pred(ch, cx, cy, orig, false));
            self.become_tentative();
            self.newline_carriage_return(fb);
            self.sort_pending();
        }
    }

    /// Stock `newline_carriage_return`: col=0; advance row or blank-predict last row.
    fn newline_carriage_return(&mut self, fb: &Framebuffer) {
        let exp = self.local_frame_sent.saturating_add(1);
        self.cur_x = 0;
        if self.cur_y + 1 >= fb.rows {
            // Bottom row: do not predict scroll; blank-predict every column.
            let ep = self.prediction_epoch;
            let now = Instant::now();
            let cy = self.cur_y;
            self.pending.retain(|p| p.y != cy);
            for x in 0..fb.cols {
                let orig = fb.cell_at(x, cy).map(|c| c.ch).unwrap_or(' ');
                self.pending.push(Prediction {
                    ch: ' ',
                    x,
                    y: cy,
                    epoch: ep,
                    at: now,
                    expiration_sent: exp,
                    original_ch: orig,
                    unknown: false,
                    overlay_attr: None,
                });
            }
            self.active = true;
            self.cursor_exp_sent = Some(exp);
            self.sort_pending();
        } else {
            self.cur_y += 1;
            self.active = true;
            self.cursor_exp_sent = Some(exp);
        }
    }

    fn try_parse_arrow(&mut self, bytes: &[u8], fb: &Framebuffer) -> ArrowParse {
        // Buffer lone ESC for a follow-up chunk (CSI may arrive split).
        if bytes.len() < 2 {
            return ArrowParse::NeedMore;
        }
        if bytes[0] != 0x1b {
            return ArrowParse::NotArrow(1);
        }
        let kind = bytes[1];
        if kind == b'O' {
            if bytes.len() < 3 {
                return ArrowParse::NeedMore;
            }
            return match bytes[2] {
                b'C' => {
                    self.move_cursor_right(fb);
                    ArrowParse::Handled(3)
                }
                b'D' => {
                    self.move_cursor_left();
                    ArrowParse::Handled(3)
                }
                // ESC O X (SS3 up/down/home/…): consume all 3, do not predict
                // printables from O/X (stock tentatives unknown CSI/SS3).
                _ => {
                    self.become_tentative();
                    self.esc_buf.clear();
                    ArrowParse::Handled(3)
                }
            };
        }
        if kind != b'[' {
            // Stock Esc_Dispatch: become_tentative only — do NOT re-feed the
            // second byte as a printable (Alt-x / ESC D / etc.).
            if bytes.len() < 2 {
                return ArrowParse::NeedMore;
            }
            self.become_tentative();
            self.esc_buf.clear();
            return ArrowParse::Handled(2);
        }
        // CSI [params] final — L/R arrows accept optional count (CSI n C / CSI n D).
        let mut j = 2;
        let mut param: u32 = 0;
        let mut saw_digit = false;
        while j < bytes.len() {
            let c = bytes[j];
            if (b'0'..=b'9').contains(&c) {
                saw_digit = true;
                param = param.saturating_mul(10).saturating_add(u32::from(c - b'0'));
                j += 1;
                continue;
            }
            if c == b';' {
                // Extra params: skip to final (only first count used for C/D).
                j += 1;
                while j < bytes.len() && ((b'0'..=b'9').contains(&bytes[j]) || bytes[j] == b';') {
                    j += 1;
                }
                continue;
            }
            if (b'@'..=b'~').contains(&c) {
                j += 1;
                // Stock CSI C/D ignores parameters and always moves by one.
                let _ = (saw_digit, param);
                return match c {
                    b'C' => {
                        self.move_cursor_right_n(1, fb);
                        ArrowParse::Handled(j)
                    }
                    b'D' => {
                        self.move_cursor_left_n(1);
                        ArrowParse::Handled(j)
                    }
                    // Other CSI finals: tentative, fully consume (no glyph pollution).
                    _ => {
                        self.become_tentative();
                        self.esc_buf.clear();
                        ArrowParse::Handled(j)
                    }
                };
            }
            if (b' '..=b'/').contains(&c) {
                // CSI intermediate: keep scanning to final; do not re-feed
                // intermediates/finals as printables.
                j += 1;
                continue;
            }
            // Unexpected byte inside CSI — skip one and stop (tentative).
            self.become_tentative();
            self.esc_buf.clear();
            return ArrowParse::Handled((j + 1).max(1));
        }
        ArrowParse::NeedMore
    }

    fn move_cursor_left(&mut self) {
        self.move_cursor_left_n(1);
    }

    fn move_cursor_right(&mut self, fb: &Framebuffer) {
        self.move_cursor_right_n(1, fb);
    }

    fn move_cursor_left_n(&mut self, n: usize) {
        let n = n.max(1);
        if self.cur_x == 0 {
            return;
        }
        self.cur_x = self.cur_x.saturating_sub(n);
        self.active = true;
        self.cursor_exp_sent = Some(self.local_frame_sent.saturating_add(1));
    }

    fn move_cursor_right_n(&mut self, n: usize, fb: &Framebuffer) {
        let n = n.max(1);
        let max_x = fb.cols.saturating_sub(1);
        if self.cur_x >= max_x {
            return;
        }
        self.cur_x = (self.cur_x + n).min(max_x);
        self.active = true;
        self.cursor_exp_sent = Some(self.local_frame_sent.saturating_add(1));
    }

    /// Stock insert-mode printable: shift **every** column from rightmost down to
    /// cursor+1 (overlay-or-host), place `ch` at cursor. One cell per column.
    fn predict_host_insert(&mut self, fb: &Framebuffer, cx: usize, cy: usize, ch: char) {
        let exp = self.local_frame_sent.saturating_add(1);
        let ep = self.prediction_epoch;
        let now = Instant::now();
        let width = fb.cols;
        let last = width.saturating_sub(1);

        // Sparse overlay → dense row map (stock ConditionalOverlayRow).
        let mut row: Vec<Option<Prediction>> = vec![None; width];
        let mut rest = Vec::with_capacity(self.pending.len());
        for p in self.pending.drain(..) {
            if p.y == cy && p.x < width {
                let x = p.x;
                row[x] = Some(p);
            } else {
                rest.push(p);
            }
        }

        // rightmost → cx+1: cell[i] ← cell[i-1] (overlay) or host[i-1]
        if last > cx {
            for x in (cx + 1..=last).rev() {
                let orig = fb.cell_at(x, cy).map(|c| c.ch).unwrap_or(' ');
                let (sch, unknown) = if x == last {
                    // Last column: always unknown after insert shift.
                    (' ', true)
                } else if let Some(prev) = row[x - 1].as_ref() {
                    if prev.unknown {
                        (' ', true)
                    } else {
                        (prev.ch, false)
                    }
                } else {
                    let src = fb.cell_at(x - 1, cy);
                    let sch = src
                        .map(|c| if c.ch == '\0' { ' ' } else { c.ch })
                        .unwrap_or(' ');
                    (sch, false)
                };
                row[x] = Some(Prediction {
                    ch: sch,
                    x,
                    y: cy,
                    epoch: ep,
                    at: now,
                    expiration_sent: exp,
                    original_ch: orig,
                    unknown,
                    overlay_attr: None,
                });
            }
        }

        let orig = fb.cell_at(cx, cy).map(|c| c.ch).unwrap_or(' ');
        row[cx] = Some(Prediction {
            ch,
            x: cx,
            y: cy,
            epoch: ep,
            at: now,
            expiration_sent: exp,
            original_ch: orig,
            unknown: false,
            overlay_attr: None,
        });

        self.pending = rest;
        for cell in row.into_iter().flatten() {
            self.pending.push(cell);
        }
        self.cur_x = cx.saturating_add(1);
        self.active = true;
        self.cursor_exp_sent = Some(self.local_frame_sent.saturating_add(1));
        self.sort_pending();
    }

    fn predict_backspace(&mut self, fb: &Framebuffer) {
        if self.cur_x == 0 {
            return;
        }
        let cx = self.cur_x - 1;
        let cy = self.cur_y;
        // Fast path (insert mode only): undo last same-epoch glyph we just placed.
        // Stock overwrite BS never undoes — it always predicts a space.
        if !self.overwrite {
            if let Some(last) = self.pending.last() {
                if last.epoch == self.prediction_epoch
                    && last.x == cx
                    && last.y == cy
                    && !last.unknown
                {
                    let only = !self.pending.iter().any(|p| {
                        p.y == cy && p.x > cx && p.epoch == self.prediction_epoch
                    });
                    if only {
                        self.pending.pop();
                        self.cur_x = cx;
                        self.active = true;
                        self.cursor_exp_sent =
                            Some(self.local_frame_sent.saturating_add(1));
                        return;
                    }
                }
            }
        }
        // Overwrite-mode BS: stock clears cell to space (no row shift).
        if self.overwrite {
            let orig = fb.cell_at(cx, cy).map(|c| c.ch).unwrap_or(' ');
            self.pending.retain(|p| !(p.y == cy && p.x == cx));
            self.pending.push(self.make_pred(' ', cx, cy, orig, false));
            self.cur_x = cx;
            self.active = true;
            self.cursor_exp_sent = Some(self.local_frame_sent.saturating_add(1));
            self.sort_pending();
            return;
        }

        // Stock insert-mode BS: for i from cursor to width-1, if i+2 < width copy
        // from i+1 else mark unknown. That makes the *last two* columns unknown
        // (penultimate never receives former last glyph) — match stock exactly.
        let exp = self.local_frame_sent.saturating_add(1);
        let ep = self.prediction_epoch;
        let now = Instant::now();
        let width = fb.cols;

        let mut row: Vec<Option<Prediction>> = vec![None; width];
        let mut rest = Vec::with_capacity(self.pending.len());
        for p in self.pending.drain(..) {
            if p.y == cy && p.x < width {
                let x = p.x;
                row[x] = Some(p);
            } else {
                rest.push(p);
            }
        }

        let mut new_row: Vec<Option<Prediction>> = vec![None; width];
        for x in cx..width {
            let orig = fb.cell_at(x, cy).map(|c| c.ch).unwrap_or(' ');
            if x + 2 < width {
                let (ch, unknown) = if let Some(next) = row[x + 1].as_ref() {
                    if next.unknown {
                        (' ', true)
                    } else {
                        (next.ch, false)
                    }
                } else {
                    let src = fb.cell_at(x + 1, cy);
                    let ch = src
                        .map(|c| if c.ch == '\0' { ' ' } else { c.ch })
                        .unwrap_or(' ');
                    (ch, false)
                };
                new_row[x] = Some(Prediction {
                    ch,
                    x,
                    y: cy,
                    epoch: ep,
                    at: now,
                    expiration_sent: exp,
                    original_ch: orig,
                    unknown,
                    overlay_attr: None,
                });
            } else {
                // Stock: last two columns are unknown after insert-mode BS.
                new_row[x] = Some(Prediction {
                    ch: ' ',
                    x,
                    y: cy,
                    epoch: ep,
                    at: now,
                    expiration_sent: exp,
                    original_ch: orig,
                    unknown: true,
                    overlay_attr: None,
                });
            }
        }
        // Keep columns left of cx unchanged from prior row map.
        for x in 0..cx {
            new_row[x] = row[x].take();
        }

        self.pending = rest;
        for cell in new_row.into_iter().flatten() {
            self.pending.push(cell);
        }
        self.cur_x = cx;
        self.active = true;
        self.cursor_exp_sent = Some(self.local_frame_sent.saturating_add(1));
        self.sort_pending();
    }

    /// Stock become_tentative: bump prediction_epoch only.
    /// Existing pending stay; those with epoch > confirmed_epoch are hidden.
    pub fn become_tentative(&mut self) {
        self.prediction_epoch = self.prediction_epoch.wrapping_add(1);
    }

    /// Kill pending in a failed tentative band (stock kill_epoch).
    /// Stock removes every cell with `tentative(epoch - 1)` i.e. epoch ≥ failed,
    /// snaps cursor to host, then become_tentative.
    ///
    /// Do **not** invent a sticky `cursor_exp_sent` when no real cursor prediction
    /// remains — that freezes `set_cursor` and Adaptive demote until a future ack.
    fn kill_epoch(&mut self, epoch: u64, fb: &Framebuffer) {
        self.pending.retain(|p| p.epoch < epoch);
        self.cur_x = fb.cur_x;
        self.cur_y = fb.cur_y;
        self.become_tentative();
        if self.pending.is_empty() {
            self.active = false;
            self.cursor_exp_sent = None;
        } else {
            // Older proven-band cells may remain; keep active for them.
            self.active = true;
            // Drop synthetic cursor expiry from the failed band.
            self.cursor_exp_sent = None;
        }
    }

    /// Full reset (resize, demote, huge paste, screen clear).
    /// Stock: clear overlays/cursors then become_tentative only — does **not**
    /// re-align `confirmed_epoch`, and does **not** clear glitch_trigger.
    pub fn reset(&mut self) {
        self.pending.clear();
        self.become_tentative();
        self.active = false;
        self.confirmed = 0;
        // Stock reset does not zero glitch_trigger / last_quick_confirmation.
        self.esc_buf.clear();
        self.cursor_exp_sent = None;
    }

    /// mosh-go `SetCursor` — only tracks server cursor when inactive.
    /// Stock: prove anew on each row — row change becomes tentative.
    pub fn set_cursor(&mut self, x: usize, y: usize) {
        if !self.active {
            if y != self.cur_y {
                self.become_tentative();
            }
            self.cur_x = x;
            self.cur_y = y;
        }
    }

    /// Stable L→R, top→bottom order for Confirm (after mid-line insert/BS).
    fn sort_pending(&mut self) {
        self.pending.sort_by(|a, b| (a.y, a.x).cmp(&(b.y, b.x)));
    }

    /// Drop cursor-only prediction latch and snap to host.
    fn clear_cursor_prediction(&mut self, fb: &Framebuffer) {
        self.active = false;
        self.cursor_exp_sent = None;
        self.cur_x = fb.cur_x;
        self.cur_y = fb.cur_y;
    }

    fn make_pred(
        &self,
        ch: char,
        x: usize,
        y: usize,
        original_ch: char,
        unknown: bool,
    ) -> Prediction {
        Prediction {
            ch,
            x,
            y,
            epoch: self.prediction_epoch,
            at: Instant::now(),
            expiration_sent: self.local_frame_sent.saturating_add(1),
            original_ch,
            unknown,
            overlay_attr: None,
        }
    }

    /// mosh-go ExpireStale + stock glitch sampling on oldest pending age.
    pub fn expire_stale(&mut self, now: Instant) {
        // Glitch: true oldest by wall clock (pending is sorted L→R, not by time).
        if let Some(oldest_at) = self.pending.iter().map(|p| p.at).min() {
            let age = now.saturating_duration_since(oldest_at);
            if age >= GLITCH_FLAG_THRESHOLD {
                self.glitch_trigger = GLITCH_REPAIR_COUNT * 2;
                self.show = true;
                self.flagging = true;
            } else if age >= GLITCH_THRESHOLD && self.glitch_trigger < GLITCH_REPAIR_COUNT {
                self.glitch_trigger = GLITCH_REPAIR_COUNT;
                self.show = true;
            }
        }

        let cutoff = now.checked_sub(PREDICTION_TIMEOUT).unwrap_or(now);
        let sent = self.local_frame_sent;
        let late = self.late_ack();
        let before = self.pending.len();
        self.pending.retain(|p| {
            if p.at >= cutoff {
                return true;
            }
            // Keep frame-Pending preds until late_ack (stock echo_ack).
            if sent > 0 && late < p.expiration_sent {
                return true;
            }
            false
        });
        if self.pending.len() != before && self.pending.is_empty() {
            // Wall-clock expire of all cells: drop active + cursor latch.
            // (Frame-Pending cells are kept above, so this is true abandonment.)
            self.active = false;
            self.cursor_exp_sent = None;
        }
    }

    /// Test helper: backdate the oldest pending prediction.
    #[cfg(test)]
    pub fn backdate_oldest_for_test(&mut self, ago: Duration) {
        if let Some(p) = self.pending.first_mut() {
            p.at = Instant::now().checked_sub(ago).unwrap_or_else(Instant::now);
        }
    }

    /// Test helper: backdate every pending prediction (full-row insert shares time).
    #[cfg(test)]
    pub fn backdate_all_for_test(&mut self, ago: Duration) {
        let t = Instant::now()
            .checked_sub(ago)
            .unwrap_or_else(Instant::now);
        for p in &mut self.pending {
            p.at = t;
        }
    }

    #[cfg(test)]
    pub fn glitch_trigger_for_test(&self) -> u32 {
        self.glitch_trigger
    }

    #[cfg(test)]
    pub fn set_overwrite_for_test(&mut self, v: bool) {
        self.overwrite = v;
    }

    #[cfg(test)]
    pub fn set_unknown_pending_for_test(&mut self, index: usize) {
        if let Some(p) = self.pending.get_mut(index) {
            p.unknown = true;
        }
    }

    #[cfg(test)]
    pub fn has_esc_buf_for_test(&self) -> bool {
        !self.esc_buf.is_empty()
    }

    #[cfg(test)]
    pub fn prediction_epoch_for_test(&self) -> u64 {
        self.prediction_epoch
    }

    #[cfg(test)]
    pub fn confirmed_epoch_for_test(&self) -> u64 {
        self.confirmed_epoch
    }

    /// Confirm against host FB with stock Pending (frame-ack) semantics.
    pub fn confirm(&mut self, fb: &Framebuffer) {
        if self.pending.is_empty() {
            // Cursor-only predictions (arrows / CR) must survive until frame ack.
            if let Some(exp) = self.cursor_exp_sent {
                if self.local_frame_sent == 0 || self.late_ack() >= exp {
                    if fb.cur_x == self.cur_x && fb.cur_y == self.cur_y {
                        self.active = false;
                        self.cursor_exp_sent = None;
                        self.cur_x = fb.cur_x;
                        self.cur_y = fb.cur_y;
                    } else if self.local_frame_sent > 0 {
                        // Host cursor disagrees after late_ack — stock IncorrectOrExpired.
                        self.reset();
                        self.cur_x = fb.cur_x;
                        self.cur_y = fb.cur_y;
                    } else {
                        self.clear_cursor_prediction(fb);
                    }
                }
                // else still Pending: keep predicted cursor + active
                return;
            }
            self.clear_cursor_prediction(fb);
            return;
        }
        if !self.active {
            self.cur_x = fb.cur_x;
            self.cur_y = fb.cur_y;
            return;
        }

        // Stock cull walks every cell independently: Pending is *skipped*, not a
        // hard stop. Overwrite mid-line retype can leave higher expiration_sent
        // at a lower column — break-on-Pending would strand later Correct cells.
        let mut remove: Vec<usize> = Vec::new();
        let mut quick = false;
        let mut i = 0usize;
        while i < self.pending.len() {
            let pred = &self.pending[i];
            // Framed Pending: skip this cell, keep checking others.
            if self.local_frame_sent > 0 && self.late_ack() < pred.expiration_sent {
                i += 1;
                continue;
            }
            let pred_epoch = pred.epoch;
            let pred_x = pred.x;
            let pred_y = pred.y;
            let pred_ch = pred.ch;
            let pred_at = pred.at;
            let pred_original = pred.original_ch;
            let pred_unknown = pred.unknown;

            let Some(cell) = fb.cell_at(pred_x, pred_y) else {
                // Drop already-matched cells, then kill/reset on this one.
                for &ri in remove.iter().rev() {
                    self.pending.remove(ri);
                }
                if pred_epoch > self.confirmed_epoch {
                    self.kill_epoch(pred_epoch, fb);
                } else {
                    self.reset();
                    self.cur_x = fb.cur_x;
                    self.cur_y = fb.cur_y;
                }
                return;
            };
            // Stock get_validity after Pending:
            // unknown → CorrectNoCredit; blank replacement → CorrectNoCredit;
            // contents match → Correct or CorrectNoCredit; else IncorrectOrExpired.
            if pred_unknown || is_blank_ch(pred_ch) {
                remove.push(i);
                i += 1;
                continue;
            }
            if cell.ch == pred_ch {
                let no_credit = is_default_blank(cell) || pred_ch == pred_original;
                if !no_credit {
                    if Instant::now().saturating_duration_since(pred_at) < GLITCH_THRESHOLD {
                        quick = true;
                    }
                    if pred_epoch > self.confirmed_epoch {
                        self.confirmed_epoch = pred_epoch;
                    }
                    let host_attr = cell.attr;
                    for p in self.pending.iter_mut().skip(i) {
                        if p.y == pred_y {
                            p.overlay_attr = Some(host_attr);
                        }
                    }
                }
                remove.push(i);
                i += 1;
            } else if (cell.ch == ' ' || cell.ch == '\0') && self.local_frame_sent == 0 {
                // Unframed: stall this cell (and later) like still waiting.
                break;
            } else if pred_epoch > self.confirmed_epoch {
                for &ri in remove.iter().rev() {
                    self.pending.remove(ri);
                }
                self.kill_epoch(pred_epoch, fb);
                return;
            } else {
                self.reset();
                self.cur_x = fb.cur_x;
                self.cur_y = fb.cur_y;
                return;
            }
        }

        if !remove.is_empty() {
            let n = remove.len();
            for &ri in remove.iter().rev() {
                self.pending.remove(ri);
            }
            self.confirmed = self.confirmed.saturating_add(n);
            if quick && self.glitch_trigger > 0 {
                let now = Instant::now();
                let ok = self
                    .last_quick_confirmation
                    .map(|t| now.saturating_duration_since(t) >= GLITCH_REPAIR_MININTERVAL)
                    .unwrap_or(true);
                if ok {
                    self.glitch_trigger -= 1;
                    self.last_quick_confirmation = Some(now);
                }
            }
        }

        if self.pending.is_empty() {
            // Stock does not zero glitch_trigger when pending empties; only
            // credited Correct decrements it.
            if let Some(exp) = self.cursor_exp_sent {
                if self.local_frame_sent == 0 || self.late_ack() >= exp {
                    if fb.cur_x == self.cur_x && fb.cur_y == self.cur_y {
                        self.clear_cursor_prediction(fb);
                    } else if self.local_frame_sent > 0 {
                        // Host cursor disagrees after late_ack — stock IncorrectOrExpired.
                        self.reset();
                        self.cur_x = fb.cur_x;
                        self.cur_y = fb.cur_y;
                    } else {
                        self.clear_cursor_prediction(fb);
                    }
                }
            } else {
                self.clear_cursor_prediction(fb);
            }
        }
    }

    /// Overlay predictions with stock `ConditionalOverlayCell::apply` rules.
    /// Tentative preds (epoch > confirmed_epoch) are hidden until proven.
    pub fn overlay(&self, fb: &mut Framebuffer) {
        if !self.active || !self.show {
            return;
        }
        let mut any_visible = false;
        let fb_cols = fb.cols;
        for pred in &self.pending {
            // Stock: if tentative(confirmed_epoch) skip
            if pred.epoch > self.confirmed_epoch {
                continue;
            }
            any_visible = true;
            let left_attr = if pred.x > 0 {
                fb.cell_at(pred.x - 1, pred.y).map(|c| c.attr)
            } else {
                None
            };
            let Some(cell) = fb.cell_at_mut(pred.x, pred.y) else {
                continue;
            };
            // Stock blank-on-blank: force flag off (no underline).
            let mut flag = self.flagging;
            if is_blank_ch(pred.ch) && is_blank_ch(cell.ch) {
                flag = false;
            }
            // Stock unknown: underline only (never replace glyph); skip last col.
            if pred.unknown {
                if flag && pred.x + 1 < fb_cols {
                    cell.attr.under = true;
                }
                continue;
            }
            // Build replacement attrs (without forced underline yet).
            let mut rep_attr = if let Some(attr) = pred.overlay_attr {
                attr
            } else if let Some(attr) = left_attr {
                attr
            } else {
                cell.attr
            };
            // Preserve host under only if we're not about to set flag under.
            rep_attr.under = cell.attr.under && !flag;

            // Stock: only write when cell differs from replacement; then flag under.
            let same = cell.ch == pred.ch
                && cell.width == 1
                && attr_eq_ignoring_under(&cell.attr, &rep_attr);
            if !same {
                cell.ch = pred.ch;
                cell.width = 1;
                cell.attr = rep_attr;
                if flag {
                    cell.attr.under = true;
                }
            }
        }
        // Stock ConditionalCursorMove::apply: skip tentative cursors.
        // Never use any_visible as a shortcut — CR/wrap from a newer tentative
        // epoch must not move the glass while older cells are still shown.
        let _ = any_visible;
        if self.active && self.prediction_epoch <= self.confirmed_epoch {
            fb.cur_x = self.cur_x.min(fb.cols.saturating_sub(1));
            fb.cur_y = self.cur_y.min(fb.rows.saturating_sub(1));
        }
    }

    /// Test helper: simulate a credited Correct that proves the current band
    /// (sets confirmed_epoch = prediction_epoch) without draining pending.
    #[cfg(test)]
    pub fn prove_band_for_test(&mut self) {
        self.confirmed_epoch = self.prediction_epoch;
    }

    /// Test helper: known (non-unknown) pending glyph at (x,y), if any.
    #[cfg(test)]
    pub fn pending_known_char_at(&self, x: usize, y: usize) -> Option<char> {
        self.pending
            .iter()
            .find(|p| p.x == x && p.y == y && !p.unknown)
            .map(|p| p.ch)
    }

    /// Test helper: whether any pending cell is marked unknown at (x,y).
    #[cfg(test)]
    pub fn pending_unknown_at(&self, x: usize, y: usize) -> bool {
        self.pending
            .iter()
            .any(|p| p.x == x && p.y == y && p.unknown)
    }
}

/// Approximate terminal width: 0 combining/ZW, 1 normal, 2 CJK/wide.
fn unicode_width_approx(ch: char) -> i8 {
    let c = ch as u32;
    // Zero-width / combining (stock wcwidth != 1 → tentative).
    if (0x0300..=0x036F).contains(&c)
        || (0x1AB0..=0x1AFF).contains(&c)
        || (0x1DC0..=0x1DFF).contains(&c)
        || (0x20D0..=0x20FF).contains(&c)
        || (0xFE20..=0xFE2F).contains(&c)
        || matches!(c, 0x200B | 0x200C | 0x200D | 0x2060 | 0xFEFF)
    {
        return 0;
    }
    if c < 0x1100 {
        return 1;
    }
    // Common wide ranges (simplified; good enough for tentative vs predict)
    if (0x1100..=0x115F).contains(&c)
        || (0x2E80..=0xA4CF).contains(&c)
        || (0xAC00..=0xD7A3).contains(&c)
        || (0xF900..=0xFAFF).contains(&c)
        || (0xFE10..=0xFE6F).contains(&c)
        || (0xFF00..=0xFF60).contains(&c)
        || (0xFFE0..=0xFFE6).contains(&c)
        || (0x20000..=0x2FFFD).contains(&c)
        || (0x30000..=0x3FFFD).contains(&c)
    {
        2
    } else {
        1
    }
}

enum ArrowParse {
    Handled(usize),
    NeedMore,
    NotArrow(usize),
}

fn is_print(ch: char) -> bool {
    !ch.is_control()
}

fn is_blank_ch(ch: char) -> bool {
    ch == ' ' || ch == '\0'
}

fn is_default_blank(cell: &crate::framebuffer::Cell) -> bool {
    is_blank_ch(cell.ch) && cell.attr == crate::framebuffer::Attr::default()
}

fn attr_eq_ignoring_under(a: &crate::framebuffer::Attr, b: &crate::framebuffer::Attr) -> bool {
    a.bold == b.bold
        && a.dim == b.dim
        && a.italic == b.italic
        && a.blink == b.blink
        && a.reverse == b.reverse
        && a.strike == b.strike
        && a.fg == b.fg
        && a.bg == b.bg
}

/// True if stream contains **geometry** ops that invalidate pending cell (x,y).
///
/// Do **not** treat EL/ICH/DCH/ECH as hard reset — stock confirms against the
/// final cell grid after those redraws (readline often uses CUP+EL+reprint that
/// still matches predictions). Line insert/delete and full ED change layout.
fn hoststring_is_geometry_break(data: &[u8]) -> bool {
    // CSI L/M (IL/DL), J (ED — region/full erase rewrites layout).
    let mut i = 0;
    while i + 1 < data.len() {
        if data[i] == 0x1b && data[i + 1] == b'[' {
            let mut j = i + 2;
            while j < data.len() {
                let c = data[j];
                j += 1;
                if matches!(c, b'L' | b'M' | b'J') {
                    return true;
                }
                if (b'@'..=b'~').contains(&c) {
                    break;
                }
            }
            i = j;
            continue;
        }
        i += 1;
    }
    false
}

enum Utf8Input {
    Char(char, usize),
    /// Incomplete multi-byte sequence at end of buffer — carry for next chunk.
    NeedMore,
    /// Invalid lead/continuation; consume n bytes and become_tentative.
    Invalid(usize),
}

fn decode_utf8_input(data: &[u8], i: usize) -> Utf8Input {
    let b0 = data[i];
    if b0 < 0x80 {
        return Utf8Input::Char(b0 as char, 1);
    }
    let width = if b0 & 0xE0 == 0xC0 {
        2
    } else if b0 & 0xF0 == 0xE0 {
        3
    } else if b0 & 0xF8 == 0xF0 {
        4
    } else {
        return Utf8Input::Invalid(1);
    };
    if i + width > data.len() {
        return Utf8Input::NeedMore;
    }
    match std::str::from_utf8(&data[i..i + width]) {
        Ok(s) => Utf8Input::Char(s.chars().next().unwrap_or('\u{FFFD}'), width),
        Err(_) => Utf8Input::Invalid(1),
    }
}

// ---------------------------------------------------------------------------
// Display pipeline: single paint path (mosh-go WASM stateTracker shape)
// ---------------------------------------------------------------------------

/// Owns host FB + last shown + predictor. All PTY output goes through
/// [`DisplayPipeline::render`]-style Diffs when prediction is enabled.
#[derive(Debug)]
pub struct DisplayPipeline {
    host_fb: Framebuffer,
    last_shown: Option<Framebuffer>,
    predictor: Predictor,
    /// Sticky SGR across HostBytes chunks.
    pen: crate::ansi_apply::AnsiPen,
    /// When true, we use Diff-based paint; when false (never / cold adaptive),
    /// HostBytes are passed through and last_shown tracks host_fb only.
    using_overlay_path: bool,
}

impl DisplayPipeline {
    pub fn new(cols: usize, rows: usize, preference: DisplayPreference) -> Self {
        Self {
            host_fb: Framebuffer::new(cols, rows),
            last_shown: None,
            predictor: Predictor::new(preference),
            pen: crate::ansi_apply::AnsiPen::default(),
            using_overlay_path: matches!(preference, DisplayPreference::Always),
        }
    }

    pub fn predictor(&self) -> &Predictor {
        &self.predictor
    }

    pub fn host_fb(&self) -> &Framebuffer {
        &self.host_fb
    }

    /// Last painted framebuffer (tests / diagnostics).
    pub fn last_shown(&self) -> Option<&Framebuffer> {
        self.last_shown.as_ref()
    }

    pub fn using_overlay_path(&self) -> bool {
        self.using_overlay_path
    }

    /// Feed SSP sent / early-ack / late-ack (echo_ack) into the predictor.
    ///
    /// When late_ack advances with **no** accompanying HostBytes, stock still
    /// culls Pending predictions. Returns Diff paint if overlay must update.
    pub fn set_frames(&mut self, sent: u64, early_acked: u64, late_acked: u64) -> Vec<u8> {
        let before_pending = self.predictor.pending_len();
        let before_active = self.predictor.active();
        let before_cur = (self.predictor.cur_x(), self.predictor.cur_y());
        self.predictor.set_frames(sent, early_acked, late_acked);
        // Ack-only packets never call on_host_bytes; still Confirm/Pending drain.
        if before_pending > 0 || before_active {
            self.predictor.confirm(&self.host_fb);
            self.predictor.expire_stale(Instant::now());
            let after_pending = self.predictor.pending_len();
            let after_active = self.predictor.active();
            let after_cur = (self.predictor.cur_x(), self.predictor.cur_y());
            let changed = before_pending != after_pending
                || before_active != after_active
                || before_cur != after_cur;
            if changed {
                if self.predictor.should_show() {
                    self.using_overlay_path = true;
                    if self.last_shown.is_none() {
                        self.last_shown = Some(self.host_fb.clone());
                    }
                    return self.render_overlay_path();
                }
                if self.using_overlay_path {
                    self.using_overlay_path = false;
                    return self.render_host_only();
                }
            }
        }
        Vec::new()
    }

    #[cfg(test)]
    pub fn set_frames_for_test(&mut self, sent: u64, acked: u64) -> Vec<u8> {
        // Tests without separate echo_ack: advance late with early.
        self.set_frames(sent, acked, acked)
    }

    #[cfg(test)]
    pub fn set_frames_late_for_test(&mut self, sent: u64, early: u64, late: u64) -> Vec<u8> {
        self.set_frames(sent, early, late)
    }

    /// Resize local model; returns a full redraw for the PTY when size changes.
    pub fn resize(&mut self, cols: usize, rows: usize) -> Vec<u8> {
        if cols == self.host_fb.cols && rows == self.host_fb.rows {
            return Vec::new();
        }
        self.host_fb.resize(cols, rows);
        self.predictor.reset();
        self.predictor
            .set_cursor(self.host_fb.cur_x, self.host_fb.cur_y);
        self.pen = crate::ansi_apply::AnsiPen::default();
        // Force full redraw baseline (stock new_frame on size mismatch).
        let paint = self.host_fb.diff(None);
        self.last_shown = Some(self.host_fb.clone());
        self.using_overlay_path = self.predictor.should_show();
        paint
    }

    /// Returns any ANSI that must be written when adaptive mode flips.
    pub fn set_srtt(&mut self, srtt: Option<Duration>) -> Vec<u8> {
        let was_show = self.predictor.should_show();
        let was_flag = self.predictor.flagging();
        self.predictor.set_srtt(srtt);
        let now_show = self.predictor.should_show();
        let now_flag = self.predictor.flagging();
        if was_show && !now_show {
            // Stock keeps background predictions; only stop painting overlays.
            self.using_overlay_path = false;
            return self.render_host_only();
        }
        if !was_show && now_show {
            if self.last_shown.is_none() {
                self.last_shown = Some(self.host_fb.clone());
            }
            self.using_overlay_path = true;
            // Reveal any background-proven pending immediately.
            if self.predictor.active() || self.predictor.pending_len() > 0 {
                return self.render_overlay_path();
            }
        }
        // Flagging flip must re-Diff so underlines appear/clear (stock redraws).
        if now_show && was_flag != now_flag && self.using_overlay_path {
            return self.render_overlay_path();
        }
        Vec::new()
    }

    /// Idle tick: expire stale predictions and repaint if the overlay changed.
    pub fn tick(&mut self, now: Instant) -> Vec<u8> {
        if !self.predictor.should_show() && !self.using_overlay_path {
            return Vec::new();
        }
        let before_len = self.predictor.pending_len();
        let before_flag = self.predictor.flagging();
        let before_show = self.predictor.should_show();
        self.predictor.expire_stale(now);
        let after_len = self.predictor.pending_len();
        let after_flag = self.predictor.flagging();
        let after_show = self.predictor.should_show();
        if before_len != after_len || before_flag != after_flag || before_show != after_show {
            if after_len == 0 && !after_show {
                self.using_overlay_path = false;
                return self.render_host_only();
            }
            if after_show {
                self.using_overlay_path = true;
            }
            return self.render_overlay_path();
        }
        Vec::new()
    }

    /// HostBytes (or raw hoststring) arrived from mosh-server.
    pub fn on_host_bytes(&mut self, hoststring: &[u8]) -> Vec<u8> {
        // Structural scan must see sticky carry + this chunk (same reassembly
        // apply_ansi uses); otherwise split CSI like "\x1b[2" + "@" misses ICH.
        let structural_scan: Vec<u8> = if self.pen.carry.is_empty() {
            hoststring.to_vec()
        } else {
            let mut v = self.pen.carry.clone();
            v.extend_from_slice(hoststring);
            v
        };
        let geometry = hoststring_is_geometry_break(&structural_scan);
        let gen_before = self.host_fb.scroll_generation;
        crate::ansi_apply::apply_ansi_with_pen(&mut self.host_fb, &mut self.pen, hoststring);
        // Line geometry + any host scroll wipe pending. Content redraws
        // (EL/ICH/DCH/ECH) go through Confirm against the final grid instead.
        let scrolled = self.host_fb.scroll_generation != gen_before;
        if geometry || scrolled {
            self.predictor.reset();
        }
        self.predictor
            .set_cursor(self.host_fb.cur_x, self.host_fb.cur_y);
        self.predictor.confirm(&self.host_fb);
        self.predictor.expire_stale(Instant::now());

        if !self.predictor.should_show() {
            // Stock Adaptive: keep background preds; only clear residual overlay paint.
            if self.using_overlay_path {
                self.using_overlay_path = false;
                return self.render_host_only();
            }
            self.last_shown = Some(self.host_fb.clone());
            return hoststring.to_vec();
        }

        self.using_overlay_path = true;
        self.render_overlay_path()
    }

    /// Local keystroke: update predictor and emit Diff if overlay is active.
    /// Caller still forwards `keys` to the server via `Client::send_keys`.
    pub fn on_keystroke(&mut self, keys: &[u8]) -> Vec<u8> {
        if self.predictor.preference() == DisplayPreference::Never {
            return Vec::new();
        }
        // Ensure cursor tracks host before first prediction of a burst.
        if !self.predictor.active() {
            self.predictor
                .set_cursor(self.host_fb.cur_x, self.host_fb.cur_y);
        }
        // Bulk paste: stock resets if >100 bytes; mosh-go always predicts.
        // Prefer stock safety for huge pastes.
        if keys.len() > 100 {
            self.predictor.reset();
            if self.predictor.should_show() {
                self.using_overlay_path = true;
                return self.render_host_only();
            }
            return Vec::new();
        }
        self.predictor.keystroke(keys, &self.host_fb);
        // Background Adaptive: build pending without painting.
        if !self.predictor.should_show() {
            return Vec::new();
        }
        self.using_overlay_path = true;
        if self.last_shown.is_none() {
            self.last_shown = Some(self.host_fb.clone());
        }
        self.render_overlay_path()
    }

    fn render_overlay_path(&mut self) -> Vec<u8> {
        let mut display = self.host_fb.clone();
        self.predictor.overlay(&mut display);
        let paint = display.diff(self.last_shown.as_ref());
        self.last_shown = Some(display);
        paint
    }

    /// Diff host_fb (no overlay) against last_shown and update last_shown.
    fn render_host_only(&mut self) -> Vec<u8> {
        let paint = self.host_fb.diff(self.last_shown.as_ref());
        self.last_shown = Some(self.host_fb.clone());
        paint
    }

    #[cfg(test)]
    pub fn predictor_mut_for_test(&mut self) -> &mut Predictor {
        &mut self.predictor
    }

    /// Test helper: prove current prediction band (stock credited Correct).
    #[cfg(test)]
    pub fn prove_band_for_test(&mut self) {
        self.predictor.prove_band_for_test();
    }
}

#[cfg(test)]
#[path = "prediction_tests.rs"]
mod tests;
