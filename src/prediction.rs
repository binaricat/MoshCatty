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
                    // Hold show while cell predictions exist (not cursor-only
                    // active with empty pending — that would latch forever).
                    if self.pending.is_empty() {
                        self.show = false;
                        self.active = false;
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
    pub fn keystroke(&mut self, input: &[u8], fb: &Framebuffer) {
        if !self.show {
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

            let (ch, len) = decode_utf8_char(&data, i);
            i += len;

            if ch == '\u{FFFD}' && len == 1 {
                self.become_tentative();
                continue;
            }
            if ch == '\u{08}' || ch == '\u{7f}' {
                self.predict_backspace(fb);
                continue;
            }
            if (ch as u32) < 0x20 {
                self.become_tentative();
                if ch == '\r' {
                    self.cur_x = 0;
                }
                continue;
            }
            if is_print(ch) {
                if unicode_width_approx(ch) != 1 || self.cur_x + 1 >= fb.cols {
                    self.become_tentative();
                    continue;
                }
                // Insert-mode: shift same-row pending at/after cursor right
                // (mirrors stock row insert; avoids duplicate cells after arrows).
                let cx = self.cur_x;
                let cy = self.cur_y;
                for p in &mut self.pending {
                    if p.epoch == self.prediction_epoch && p.y == cy && p.x >= cx {
                        p.x = p.x.saturating_add(1);
                    }
                }
                self.pending.push(Prediction {
                    ch,
                    x: cx,
                    y: cy,
                    epoch: self.prediction_epoch,
                    at: Instant::now(),
                    expiration_sent: self.local_frame_sent.saturating_add(1),
                });
                self.cur_x = cx.saturating_add(1);
                self.active = true;
                self.sort_pending();
            }
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
                // ESC O X — consume ESC only; leave X for normal processing
                _ => ArrowParse::NotArrow(1),
            };
        }
        if kind != b'[' {
            // ESC + non-CSI: control ESC only, reprocess second byte
            return ArrowParse::NotArrow(1);
        }
        let mut j = 2;
        let mut saw_param = false;
        while j < bytes.len() {
            let c = bytes[j];
            if (b'0'..=b'9').contains(&c) || c == b';' {
                saw_param = true;
                j += 1;
                continue;
            }
            if (b'@'..=b'~').contains(&c) {
                j += 1;
                if saw_param {
                    return ArrowParse::NotArrow(j);
                }
                return match c {
                    b'C' => {
                        self.move_cursor_right(fb);
                        ArrowParse::Handled(j)
                    }
                    b'D' => {
                        self.move_cursor_left();
                        ArrowParse::Handled(j)
                    }
                    _ => ArrowParse::NotArrow(j),
                };
            }
            if (b' '..=b'/').contains(&c) {
                return ArrowParse::NotArrow(j + 1);
            }
            j += 1;
        }
        ArrowParse::NeedMore
    }

    fn move_cursor_left(&mut self) {
        if self.cur_x > 0 {
            self.cur_x -= 1;
            self.active = true;
        }
    }

    fn move_cursor_right(&mut self, fb: &Framebuffer) {
        if self.cur_x + 1 < fb.cols {
            self.cur_x += 1;
            self.active = true;
        }
    }

    /// Undo own last pending glyph, shift own pending, or host-row insert BS.
    /// Host-row shifts use frame-ack Pending so Confirm will not diverge early.
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
        // Shift own pending on this row (insert BS among local preds).
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
                self.pending.push(Prediction {
                    ch,
                    x,
                    y: cy,
                    epoch: ep,
                    at: now,
                    expiration_sent: exp,
                });
            }
            self.pending.push(Prediction {
                ch: ' ',
                x: last_content,
                y: cy,
                epoch: ep,
                at: now,
                expiration_sent: exp,
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
    fn kill_epoch(&mut self, epoch: u64) {
        self.pending.retain(|p| p.epoch != epoch);
        if self.prediction_epoch == epoch {
            self.prediction_epoch = self.prediction_epoch.wrapping_add(1);
        }
        if self.pending.is_empty() {
            self.active = false;
            self.glitch_trigger = 0;
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

    /// mosh-go ExpireStale + stock glitch sampling on oldest pending age.
    pub fn expire_stale(&mut self, now: Instant) {
        // Glitch: age of oldest pending
        if let Some(oldest) = self.pending.first() {
            let age = now.saturating_duration_since(oldest.at);
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
        let mut changed = false;
        while self.pending.first().map(|p| p.at < cutoff).unwrap_or(false) {
            let front = &self.pending[0];
            // Do not wall-clock-drop preds still in frame Pending (awaiting ack).
            if self.local_frame_sent > 0 && self.local_frame_acked < front.expiration_sent {
                break;
            }
            self.pending.remove(0);
            changed = true;
        }
        if changed && self.pending.is_empty() {
            self.active = false;
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
            self.active = false;
            self.glitch_trigger = 0;
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
            let pred_exp = pred.expiration_sent;
            let _ = pred_exp;

            let Some(cell) = fb.cell_at(pred_x, pred_y) else {
                if confirmed > 0 {
                    self.pending.drain(..confirmed);
                }
                if pred_epoch > self.confirmed_epoch {
                    self.kill_epoch(pred_epoch);
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
                // Stock CorrectNoCredit: blank/space matches do not prove a band.
                let no_credit = pred_ch == ' ' || is_default_blank(cell);
                if !no_credit && pred_epoch > self.confirmed_epoch {
                    self.confirmed_epoch = pred_epoch;
                }
                confirmed += 1;
            } else if (cell.ch == ' ' || cell.ch == '\0') && pred_ch != ' ' {
                break;
            } else if pred_epoch > self.confirmed_epoch {
                // Drain already-matched prefix, then kill failed tentative epoch.
                if confirmed > 0 {
                    self.pending.drain(..confirmed);
                }
                self.kill_epoch(pred_epoch);
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
                self.glitch_trigger -= 1;
            }
        }

        if self.pending.is_empty() {
            self.active = false;
            self.glitch_trigger = 0;
            self.cur_x = fb.cur_x;
            self.cur_y = fb.cur_y;
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
            if let Some(cell) = fb.cell_at_mut(pred.x, pred.y) {
                cell.ch = pred.ch;
                cell.width = 1;
                // Stock heuristic: match renditions of the cell to the left.
                if let Some(attr) = left_attr {
                    cell.attr = attr;
                }
                if self.flagging {
                    cell.attr.under = true;
                } else {
                    cell.attr.under = false;
                }
            }
        }
        if !self.pending.is_empty() || self.active {
            fb.cur_x = self.cur_x.min(fb.cols.saturating_sub(1));
            fb.cur_y = self.cur_y.min(fb.rows.saturating_sub(1));
        }
    }
}

/// Approximate terminal width: ASCII/Latin-1 = 1, most CJK = 2.
fn unicode_width_approx(ch: char) -> usize {
    let c = ch as u32;
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

/// True if hoststring contains a full-screen or full-line wipe that blanks
/// cells without replacing them with divergent glyphs.
fn hoststring_is_destructive_clear(data: &[u8]) -> bool {
    // CSI ... J (ED) or CSI ... K (EL) — stock Display erase ops.
    let mut i = 0;
    while i + 1 < data.len() {
        if data[i] == 0x1b && data[i + 1] == b'[' {
            let mut j = i + 2;
            while j < data.len() {
                let c = data[j];
                j += 1;
                if c == b'J' || c == b'K' {
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

fn decode_utf8_char(data: &[u8], i: usize) -> (char, usize) {
    let b0 = data[i];
    if b0 < 0x80 {
        return (b0 as char, 1);
    }
    let width = if b0 & 0xE0 == 0xC0 {
        2
    } else if b0 & 0xF0 == 0xE0 {
        3
    } else if b0 & 0xF8 == 0xF0 {
        4
    } else {
        return ('\u{FFFD}', 1);
    };
    if i + width > data.len() {
        return ('\u{FFFD}', 1);
    }
    match std::str::from_utf8(&data[i..i + width]) {
        Ok(s) => (s.chars().next().unwrap_or('\u{FFFD}'), width),
        Err(_) => ('\u{FFFD}', 1),
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
            self.predictor.reset();
            self.using_overlay_path = false;
            return self.render_host_only();
        }
        if !was_show && now_show {
            if self.last_shown.is_none() {
                self.last_shown = Some(self.host_fb.clone());
            }
            self.using_overlay_path = true;
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
        crate::ansi_apply::apply_ansi_with_pen(&mut self.host_fb, &mut self.pen, hoststring);
        // Destructive clears wipe pending (blank cells would stall Confirm and
        // ghost underlines; stronger than become_tentative).
        if hoststring_is_destructive_clear(hoststring) {
            self.predictor.reset();
        }
        self.predictor
            .set_cursor(self.host_fb.cur_x, self.host_fb.cur_y);
        self.predictor.confirm(&self.host_fb);
        self.predictor.expire_stale(Instant::now());

        if !self.predictor.should_show() {
            // Still Diff from last_shown if we were in overlay mode so any
            // residual underline cells are cleared; otherwise pass-through.
            if self.using_overlay_path || self.predictor.active() {
                self.predictor.reset();
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
        if !self.predictor.should_show() {
            self.predictor.reset();
            return Vec::new();
        }
        // Ensure cursor tracks host before first prediction of a burst.
        if !self.predictor.active() {
            self.predictor
                .set_cursor(self.host_fb.cur_x, self.host_fb.cur_y);
        }
        self.using_overlay_path = true;
        if self.last_shown.is_none() {
            self.last_shown = Some(self.host_fb.clone());
        }
        // Bulk paste: stock resets if >100 bytes; mosh-go always predicts.
        // Prefer stock safety for huge pastes.
        if keys.len() > 100 {
            self.predictor.reset();
            return self.render_host_only();
        }
        self.predictor.keystroke(keys, &self.host_fb);
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
