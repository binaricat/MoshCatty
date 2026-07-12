//! Speculative local echo: mosh-go pending-list core + stock fidelity extras.
//!
//! Base API matches [mosh-go `predict.go`](https://github.com/unixshells/mosh-go):
//! pending `(rune, x, y)`, Confirm, Overlay, single Diff paint path.
//!
//! Stock extras (mobile-shell/mosh `terminaloverlay.cc`) for Termius-like feel:
//! - Backspace undoes own predictions / shifts pending; host-row insert BS
//!   when safe under frame-ack Pending semantics
//! - Left/right arrow cursor prediction (CSI C/D, SS3)
//! - Underline **flagging** hysteresis (80/50 ms), separate from show
//! - Glitch triggers: long-pending preds force show / underline
//! - **True tentative epochs**: become_tentative bumps epoch without wipe;
//!   overlay hides preds with epoch > confirmed_epoch
//! - **Frame-ack expiry**: preds stay Pending until local_frame_acked reaches
//!   expiration_sent (stock late_ack vs expiration_frame)
//!
//! Never dual-write raw glyphs beside HostBytes.

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
    /// Stock local_frame_sent / local_frame_acked (SSP state numbers).
    local_frame_sent: u64,
    local_frame_acked: u64,
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
            // Start equal so the first keystrokes of a session are visible
            // (stock starts 1/0 which hides until first prove; Termius-like
            // UX prefers immediate echo — we still bump on become_tentative).
            prediction_epoch: 1,
            confirmed_epoch: 1,
            active: false,
            confirmed: 0,
            preference,
            show: matches!(preference, DisplayPreference::Always),
            flagging: matches!(preference, DisplayPreference::Always),
            glitch_trigger: 0,
            esc_buf: Vec::new(),
            local_frame_sent: 0,
            local_frame_acked: 0,
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

    /// Update SSP frame watermarks (stock set_local_frame_sent / acked).
    pub fn set_frames(&mut self, sent: u64, acked: u64) {
        if sent > self.local_frame_sent {
            self.local_frame_sent = sent;
        }
        if acked > self.local_frame_acked {
            self.local_frame_acked = acked;
        }
    }

    /// Stock hysteresis for show + flagging + glitch sampling.
    pub fn set_srtt(&mut self, srtt: Option<Duration>) {
        match self.preference {
            DisplayPreference::Always => {
                self.show = true;
                self.flagging = true;
            }
            DisplayPreference::Never => {
                self.show = false;
                self.flagging = false;
            }
            DisplayPreference::Adaptive => {
                let Some(d) = srtt else {
                    return;
                };
                // Show trigger (stock SRTT_TRIGGER_*)
                if d > SRTT_TRIGGER_HIGH {
                    self.show = true;
                } else if d <= SRTT_TRIGGER_LOW {
                    // Hold show while cell predictions exist. Cursor-only
                    // (empty pending + cursor_exp_sent) must not latch show
                    // forever, but also must not clobber glass-cursor active.
                    if self.pending.is_empty() {
                        self.show = false;
                        if self.cursor_exp_sent.is_none() {
                            self.active = false;
                        }
                    }
                }
                // Underline flagging (stock FLAG_TRIGGER_*)
                if d > FLAG_TRIGGER_HIGH {
                    self.flagging = true;
                } else if d <= FLAG_TRIGGER_LOW {
                    self.flagging = false;
                }
                // Glitch only applies while predictions are still outstanding.
                if !self.pending.is_empty() {
                    if self.glitch_trigger > GLITCH_REPAIR_COUNT {
                        self.flagging = true;
                    }
                    if self.glitch_trigger >= GLITCH_REPAIR_COUNT {
                        self.show = true;
                    }
                }
            }
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
        if ch == '\u{08}' || ch == '\u{7f}' {
            self.predict_backspace(fb);
            return;
        }
        if (ch as u32) < 0x20 {
            self.become_tentative();
            if ch == '\r' {
                // Stock newline_carriage_return (no scroll prediction at bottom).
                self.cur_x = 0;
                if self.cur_y + 1 < fb.rows {
                    self.cur_y += 1;
                }
                self.active = true;
                self.cursor_exp_sent = Some(self.local_frame_sent.saturating_add(1));
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
        // Last column: stock still places, becomes tentative, then wraps.
        let at_last_col = cx + 1 >= fb.cols;
        if at_last_col {
            self.become_tentative();
        }
        // Insert-mode: shift ALL same-row pending at/after cursor (any epoch).
        if !self.overwrite && !at_last_col {
            for p in &mut self.pending {
                if p.y == cy && p.x >= cx {
                    p.x = p.x.saturating_add(1);
                }
            }
            self.pending.retain(|p| p.x < fb.cols);
            self.predict_host_insert(fb, cx, cy, ch);
        } else if self.overwrite {
            let orig = fb.cell_at(cx, cy).map(|c| c.ch).unwrap_or(' ');
            // Last-col overwrite is unknown (shell/emacs disagree).
            self.pending.push(self.make_pred(ch, cx, cy, orig, at_last_col));
            if at_last_col {
                self.cur_x = 0;
                if self.cur_y + 1 < fb.rows {
                    self.cur_y += 1;
                }
                self.cursor_exp_sent = Some(self.local_frame_sent.saturating_add(1));
            } else {
                self.cur_x = cx.saturating_add(1);
            }
            self.active = true;
            self.sort_pending();
        } else {
            // Insert at last col: place as unknown, wrap cursor (no bottom scroll).
            let orig = fb.cell_at(cx, cy).map(|c| c.ch).unwrap_or(' ');
            self.pending.push(self.make_pred(ch, cx, cy, orig, true));
            self.cur_x = 0;
            if self.cur_y + 1 < fb.rows {
                self.cur_y += 1;
            }
            self.cursor_exp_sent = Some(self.local_frame_sent.saturating_add(1));
            self.active = true;
            self.sort_pending();
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
            // ESC + non-CSI: control ESC only, reprocess second byte
            return ArrowParse::NotArrow(1);
        }
        // CSI [params] final — L/R arrows accept optional count (CSI n C / CSI n D).
        let mut j = 2;
        let mut param: u32 = 0;
        let mut saw_digit = false;
        while j < bytes.len() {
            let c = bytes[j];
            if (b'0'..=b'9').contains(&c) {
                saw_digit = true;
                param = param
                    .saturating_mul(10)
                    .saturating_add(u32::from(c - b'0'));
                j += 1;
                continue;
            }
            if c == b';' {
                // Extra params: skip to final (only first count used for C/D).
                j += 1;
                while j < bytes.len()
                    && ((b'0'..=b'9').contains(&bytes[j]) || bytes[j] == b';')
                {
                    j += 1;
                }
                continue;
            }
            if (b'@'..=b'~').contains(&c) {
                j += 1;
                let n = if saw_digit { param.max(1) as usize } else { 1 };
                return match c {
                    b'C' => {
                        self.move_cursor_right_n(n, fb);
                        ArrowParse::Handled(j)
                    }
                    b'D' => {
                        self.move_cursor_left_n(n);
                        ArrowParse::Handled(j)
                    }
                    _ => ArrowParse::NotArrow(j),
                };
            }
            if (b' '..=b'/').contains(&c) {
                return ArrowParse::NotArrow(j + 1);
            }
            // Unexpected intermediate — treat as incomplete only if more may come
            // with valid digits; otherwise skip.
            j += 1;
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

    /// Undo own last pending glyph, shift own pending, or host-row insert BS.
    /// Host-row shifts use frame-ack Pending so Confirm will not diverge early.
    /// Stock insert-mode printable into host line: shift host tail right, place ch.
    fn predict_host_insert(&mut self, fb: &Framebuffer, cx: usize, cy: usize, ch: char) {
        let exp = self.local_frame_sent.saturating_add(1);
        let ep = self.prediction_epoch;
        let now = Instant::now();
        let mut last_content = cx;
        for x in (cx..fb.cols).rev() {
            if let Some(c) = fb.cell_at(x, cy) {
                if c.ch != ' ' && c.ch != '\0' {
                    last_content = x;
                    break;
                }
            }
        }
        if last_content > cx {
            for x in (cx + 1..=last_content).rev() {
                let src = fb.cell_at(x - 1, cy);
                let sch = src
                    .map(|c| if c.ch == '\0' { ' ' } else { c.ch })
                    .unwrap_or(' ');
                let orig = fb.cell_at(x, cy).map(|c| c.ch).unwrap_or(' ');
                self.pending.push(Prediction {
                    ch: sch,
                    x,
                    y: cy,
                    epoch: ep,
                    at: now,
                    expiration_sent: exp,
                    original_ch: orig,
                    unknown: x + 1 >= fb.cols,
                    overlay_attr: None,
                });
            }
        }
        let orig = fb.cell_at(cx, cy).map(|c| c.ch).unwrap_or(' ');
        self.pending.push(Prediction {
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
        self.cur_x = cx.saturating_add(1);
        self.active = true;
        self.sort_pending();
    }

    fn predict_backspace(&mut self, fb: &Framebuffer) {
        if self.cur_x == 0 {
            return;
        }
        let cx = self.cur_x - 1;
        let cy = self.cur_y;
        if let Some(last) = self.pending.last() {
            if last.epoch == self.prediction_epoch && last.x == cx && last.y == cy {
                self.pending.pop();
                self.cur_x = cx;
                self.active = true;
                return;
            }
        }
        // Overwrite-mode BS: stock clears cell to space (no row shift).
        if self.overwrite {
            let orig = fb.cell_at(cx, cy).map(|c| c.ch).unwrap_or(' ');
            // Remove any pending at this cell, then place space.
            self.pending.retain(|p| !(p.y == cy && p.x == cx));
            self.pending.push(self.make_pred(' ', cx, cy, orig, false));
            self.cur_x = cx;
            self.active = true;
            self.sort_pending();
            return;
        }
        // Shift own pending on this row (any epoch; insert BS among local preds).
        let mut next = Vec::with_capacity(self.pending.len().max(fb.cols));
        let mut had_own = false;
        for p in self.pending.drain(..) {
            if p.y != cy {
                next.push(p);
                continue;
            }
            if p.x < cx {
                next.push(p);
            } else if p.x > cx {
                next.push(Prediction { x: p.x - 1, ..p });
                had_own = true;
            } else {
                had_own = true; // deleted at cx
            }
        }
        self.cur_x = cx;
        if had_own {
            self.pending = next;
            self.sort_pending();
            self.active = true;
            return;
        }
        // Host-row insert BS: predict shifted host tail until last non-blank
        // (stock shifts whole row; we stop at trailing blanks to avoid O(cols)
        // space preds that stall Confirm forever).
        self.pending = next;
        let exp = self.local_frame_sent.saturating_add(1);
        let ep = self.prediction_epoch;
        let now = Instant::now();
        let mut last_content = cx;
        for x in (cx..fb.cols).rev() {
            if let Some(c) = fb.cell_at(x, cy) {
                if c.ch != ' ' && c.ch != '\0' {
                    last_content = x;
                    break;
                }
            }
        }
        // Shift content from cx..last_content; write space at the old last content cell.
        if last_content >= cx {
            for x in cx..last_content {
                let src = fb.cell_at(x + 1, cy);
                let ch = src
                    .map(|c| if c.ch == '\0' { ' ' } else { c.ch })
                    .unwrap_or(' ');
                let orig = fb.cell_at(x, cy).map(|c| c.ch).unwrap_or(' ');
                self.pending.push(Prediction {
                    ch,
                    x,
                    y: cy,
                    epoch: ep,
                    at: now,
                    expiration_sent: exp,
                    original_ch: orig,
                    unknown: x + 1 >= fb.cols.saturating_sub(1),
                    overlay_attr: None,
                });
            }
            self.pending.push(Prediction {
                ch: ' ',
                x: last_content,
                y: cy,
                epoch: ep,
                at: now,
                expiration_sent: exp,
                original_ch: fb.cell_at(last_content, cy).map(|c| c.ch).unwrap_or(' '),
                unknown: true,
                overlay_attr: None,
            });
        }
        self.sort_pending();
        self.active = true;
    }

    /// Stock become_tentative: bump prediction_epoch only.
    /// Existing pending stay; those with epoch > confirmed_epoch are hidden.
    pub fn become_tentative(&mut self) {
        self.prediction_epoch = self.prediction_epoch.wrapping_add(1);
    }

    /// Kill all pending in a failed tentative epoch (stock kill_epoch).
    fn kill_epoch(&mut self, epoch: u64, fb: &Framebuffer) {
        self.pending.retain(|p| p.epoch != epoch);
        if self.prediction_epoch == epoch {
            self.prediction_epoch = self.prediction_epoch.wrapping_add(1);
        }
        if self.pending.is_empty() {
            self.active = false;
            self.glitch_trigger = 0;
            self.cursor_exp_sent = None;
            self.cur_x = fb.cur_x;
            self.cur_y = fb.cur_y;
        } else {
            // Stock re-tentatives remaining work after a failed band.
            self.become_tentative();
        }
    }

    /// Full reset (resize, demote, huge paste, screen clear).
    pub fn reset(&mut self) {
        self.pending.clear();
        self.prediction_epoch = self.prediction_epoch.wrapping_add(1);
        // Re-align confidence so typing after reset is immediately visible
        // (unlike become_tentative, which intentionally re-proves).
        self.confirmed_epoch = self.prediction_epoch;
        self.active = false;
        self.confirmed = 0;
        self.glitch_trigger = 0;
        self.esc_buf.clear();
        self.cursor_exp_sent = None;
        self.last_quick_confirmation = None;
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

    fn make_pred(&self, ch: char, x: usize, y: usize, original_ch: char, unknown: bool) -> Prediction {
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
        let acked = self.local_frame_acked;
        let before = self.pending.len();
        self.pending.retain(|p| {
            if p.at >= cutoff {
                return true;
            }
            // Keep frame-Pending preds until acked (stock late_ack / expiration).
            if sent > 0 && acked < p.expiration_sent {
                return true;
            }
            false
        });
        if self.pending.len() != before && self.pending.is_empty() {
            // Cell preds gone — drop cursor-only state too (no cells to hold).
            if self.cursor_exp_sent.is_none() {
                self.active = false;
            } else {
                // Keep glass cursor until confirm resolves cursor_exp.
            }
            // Do not leave glitch latch on after preds are gone.
            self.glitch_trigger = 0;
        }
    }

    /// Test helper: backdate the oldest pending prediction.
    #[cfg(test)]
    pub fn backdate_oldest_for_test(&mut self, ago: Duration) {
        if let Some(p) = self.pending.first_mut() {
            p.at = Instant::now().checked_sub(ago).unwrap_or_else(Instant::now);
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
            self.glitch_trigger = 0;
            // Cursor-only predictions (arrows / CR) must survive until frame ack.
            if let Some(exp) = self.cursor_exp_sent {
                if self.local_frame_sent == 0 || self.local_frame_acked >= exp {
                    if fb.cur_x == self.cur_x && fb.cur_y == self.cur_y {
                        self.active = false;
                        self.cursor_exp_sent = None;
                        self.cur_x = fb.cur_x;
                        self.cur_y = fb.cur_y;
                    } else if self.local_frame_sent > 0 {
                        // Host cursor disagrees after ack — stock IncorrectOrExpired.
                        self.reset();
                        self.cur_x = fb.cur_x;
                        self.cur_y = fb.cur_y;
                    } else {
                        self.active = false;
                        self.cursor_exp_sent = None;
                        self.cur_x = fb.cur_x;
                        self.cur_y = fb.cur_y;
                    }
                }
                // else still Pending: keep predicted cursor + active
                return;
            }
            self.active = false;
            self.cur_x = fb.cur_x;
            self.cur_y = fb.cur_y;
            return;
        }
        if !self.active {
            self.cur_x = fb.cur_x;
            self.cur_y = fb.cur_y;
            return;
        }

        let mut confirmed = 0usize;
        let mut quick = false;
        while confirmed < self.pending.len() {
            let pred = &self.pending[confirmed];
            // Frame not yet acked — stock Pending: stop, do not diverge.
            // When frame watermarks were never set (unit tests / early session),
            // skip Pending and allow content confirm.
            if self.local_frame_sent > 0 && self.local_frame_acked < pred.expiration_sent {
                break;
            }
            let pred_epoch = pred.epoch;
            let pred_x = pred.x;
            let pred_y = pred.y;
            let pred_ch = pred.ch;
            let pred_at = pred.at;
            let pred_original = pred.original_ch;
            let pred_unknown = pred.unknown;
            let _ = pred.expiration_sent;

            let Some(cell) = fb.cell_at(pred_x, pred_y) else {
                if confirmed > 0 {
                    self.pending.drain(..confirmed);
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
            if cell.ch == pred_ch {
                if pred_ch == ' ' && is_default_blank(cell) && fb.cur_x <= pred_x {
                    break;
                }
                if Instant::now().saturating_duration_since(pred_at) < GLITCH_THRESHOLD {
                    quick = true;
                }
                // Stock CorrectNoCredit: no-op / blank / unknown do not prove a band.
                let no_credit = pred_unknown
                    || pred_ch == ' '
                    || is_default_blank(cell)
                    || pred_ch == pred_original;
                if !no_credit && pred_epoch > self.confirmed_epoch {
                    self.confirmed_epoch = pred_epoch;
                }
                // Stock Correct: copy host cell renditions onto remaining same-row preds.
                if !no_credit {
                    let host_attr = cell.attr;
                    for p in self.pending.iter_mut().skip(confirmed) {
                        if p.y == pred_y {
                            p.overlay_attr = Some(host_attr);
                        }
                    }
                }
                confirmed += 1;
            } else if pred_unknown {
                // Stock unknown → CorrectNoCredit: drop without diverge.
                confirmed += 1;
            } else if (cell.ch == ' ' || cell.ch == '\0') && pred_ch != ' ' {
                break;
            } else if pred_epoch > self.confirmed_epoch {
                // Drain already-matched prefix, then kill failed tentative epoch.
                if confirmed > 0 {
                    self.pending.drain(..confirmed);
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

        if confirmed > 0 {
            self.pending.drain(..confirmed);
            self.confirmed = self.confirmed.saturating_add(confirmed);
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
            self.glitch_trigger = 0;
            // Cursor-only: if expired and host disagrees, reset; if matches, clear active.
            if let Some(exp) = self.cursor_exp_sent {
                if self.local_frame_sent == 0 || self.local_frame_acked >= exp {
                    if fb.cur_x == self.cur_x && fb.cur_y == self.cur_y {
                        self.active = false;
                        self.cursor_exp_sent = None;
                        self.cur_x = fb.cur_x;
                        self.cur_y = fb.cur_y;
                    } else if self.local_frame_sent > 0 {
                        // Host cursor disagrees after ack — stock IncorrectOrExpired.
                        self.reset();
                        self.cur_x = fb.cur_x;
                        self.cur_y = fb.cur_y;
                    } else {
                        self.active = false;
                        self.cur_x = fb.cur_x;
                        self.cur_y = fb.cur_y;
                    }
                }
            } else {
                self.active = false;
                self.cur_x = fb.cur_x;
                self.cur_y = fb.cur_y;
            }
        }
    }

    /// Overlay predictions; underline only when flagging (stock).
    /// Tentative preds (epoch > confirmed_epoch) are hidden until proven.
    pub fn overlay(&self, fb: &mut Framebuffer) {
        if !self.active || !self.show {
            return;
        }
        for pred in &self.pending {
            // Stock: if tentative(confirmed_epoch) skip
            if pred.epoch > self.confirmed_epoch {
                continue;
            }
            let left_attr = if pred.x > 0 {
                fb.cell_at(pred.x - 1, pred.y).map(|c| c.attr)
            } else {
                None
            };
            // Stock unknown: underline only, never replace the host glyph.
            if pred.unknown {
                if self.flagging {
                    if let Some(cell) = fb.cell_at_mut(pred.x, pred.y) {
                        cell.attr.under = true;
                    }
                }
                continue;
            }
            if let Some(cell) = fb.cell_at_mut(pred.x, pred.y) {
                cell.ch = pred.ch;
                cell.width = 1;
                // Prefer Correct-cascaded host renditions; else inherit left cell.
                if let Some(attr) = pred.overlay_attr {
                    cell.attr = attr;
                } else if let Some(attr) = left_attr {
                    cell.attr = attr;
                }
                // Flagging underlines predictions (overrides inherited under).
                cell.attr.under = self.flagging;
            }
        }
        if !self.pending.is_empty() || self.active {
            fb.cur_x = self.cur_x.min(fb.cols.saturating_sub(1));
            fb.cur_y = self.cur_y.min(fb.rows.saturating_sub(1));
        }
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

fn is_default_blank(cell: &crate::framebuffer::Cell) -> bool {
    (cell.ch == ' ' || cell.ch == '\0') && cell.attr == crate::framebuffer::Attr::default()
}

/// True if stream contains geometry-breaking ops that invalidate pending
/// cell coordinates (erase, insert/delete chars/lines, ECH).
fn hoststring_is_destructive_clear(data: &[u8]) -> bool {
    // CSI J/K (ED/EL), @/P (ICH/DCH), L/M (IL/DL), X (ECH).
    let mut i = 0;
    while i + 1 < data.len() {
        if data[i] == 0x1b && data[i + 1] == b'[' {
            let mut j = i + 2;
            while j < data.len() {
                let c = data[j];
                j += 1;
                if matches!(c, b'J' | b'K' | b'@' | b'P' | b'L' | b'M' | b'X') {
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

    /// Feed SSP sent/acked watermarks into the predictor.
    pub fn set_frames(&mut self, sent: u64, acked: u64) {
        self.predictor.set_frames(sent, acked);
    }

    #[cfg(test)]
    pub fn set_frames_for_test(&mut self, sent: u64, acked: u64) {
        self.set_frames(sent, acked);
    }

    /// Resize local model; returns a full redraw for the PTY when size changes.
    pub fn resize(&mut self, cols: usize, rows: usize) -> Vec<u8> {
        if cols == self.host_fb.cols && rows == self.host_fb.rows {
            return Vec::new();
        }
        self.host_fb.resize(cols, rows);
        self.predictor.reset();
        self.predictor.set_cursor(self.host_fb.cur_x, self.host_fb.cur_y);
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
        // Bottom-row LF scrolls the host model without remapping pending y.
        let may_scroll = hoststring.contains(&b'\n')
            && self.host_fb.cur_y + 1 >= self.host_fb.rows
            && self.predictor.pending_len() > 0;
        // Structural scan must see sticky carry + this chunk (same reassembly
        // apply_ansi uses); otherwise split CSI like "\x1b[2" + "@" misses ICH.
        let structural_scan: Vec<u8> = if self.pen.carry.is_empty() {
            hoststring.to_vec()
        } else {
            let mut v = self.pen.carry.clone();
            v.extend_from_slice(hoststring);
            v
        };
        let structural = hoststring_is_destructive_clear(&structural_scan);
        crate::ansi_apply::apply_ansi_with_pen(&mut self.host_fb, &mut self.pen, hoststring);
        // Geometry-breaking ops wipe pending (coords no longer match cells).
        if structural || may_scroll {
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
}

#[cfg(test)]
#[path = "prediction_tests.rs"]
mod tests;
