//! Speculative local echo shaped after mosh-go `predict.go`.
//!
//! Predictions live as pending `(rune, x, y)` records and are applied via
//! [`Predictor::overlay`] onto a **copy** of the host Framebuffer. Confirm
//! matches server cells. Never write predicted glyphs as a second PTY stream
//! beside HostBytes (that caused `ls` → `lls` dual-write bugs).
//!
//! Display modes (`MOSH_PREDICTION_DISPLAY`) follow stock naming:
//! - `always` — always overlay when pending
//! - `never` — disable
//! - `adaptive` (default) — overlay when SRTT ≥ 20 ms (stock `SRTT_TRIGGER_LOW`)

use std::time::{Duration, Instant};

use crate::framebuffer::Framebuffer;

/// Stock adaptive threshold: show predictions when SRTT is at least this.
const SRTT_TRIGGER_LOW: Duration = Duration::from_millis(20);

/// mosh-go `predictionTimeout`.
const PREDICTION_TIMEOUT: Duration = Duration::from_millis(500);

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
    epoch: u64,
    at: Instant,
}

/// mosh-go style predictor.
#[derive(Debug)]
pub struct Predictor {
    pending: Vec<Prediction>,
    cur_x: usize,
    cur_y: usize,
    epoch: u64,
    active: bool,
    confirmed: usize,
    preference: DisplayPreference,
    /// Whether adaptive/always should overlay right now.
    show: bool,
}

impl Predictor {
    pub fn new(preference: DisplayPreference) -> Self {
        Self {
            pending: Vec::new(),
            cur_x: 0,
            cur_y: 0,
            epoch: 0,
            active: false,
            confirmed: 0,
            preference,
            show: matches!(preference, DisplayPreference::Always),
        }
    }

    pub fn preference(&self) -> DisplayPreference {
        self.preference
    }

    pub fn set_srtt(&mut self, srtt: Option<Duration>) {
        self.show = match self.preference {
            DisplayPreference::Always => true,
            DisplayPreference::Never => false,
            DisplayPreference::Adaptive => srtt.map(|d| d >= SRTT_TRIGGER_LOW).unwrap_or(false),
        };
    }

    /// Whether overlays should be applied (preference + adaptive trigger).
    pub fn should_show(&self) -> bool {
        self.show
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
        self.active && !self.pending.is_empty()
    }

    /// mosh-go `Keystroke`: printable → pending; control/escape → Reset.
    pub fn keystroke(&mut self, input: &[u8]) {
        if !self.show {
            self.reset();
            return;
        }
        let mut i = 0;
        while i < input.len() {
            let (ch, len) = decode_utf8_char(input, i);
            i += len;
            if ch == '\u{FFFD}' && len == 1 {
                self.reset();
                return;
            }
            let code = ch as u32;
            if code < 0x20 || code == 0x7f {
                // Control — mosh-go resets (including backspace).
                self.reset();
                return;
            }
            if is_print(ch) {
                self.pending.push(Prediction {
                    ch,
                    x: self.cur_x,
                    y: self.cur_y,
                    epoch: self.epoch,
                    at: Instant::now(),
                });
                // mosh-go advances one column per printable (width=1 model).
                self.cur_x = self.cur_x.saturating_add(1);
                self.active = true;
            }
        }
    }

    /// mosh-go `Reset`.
    pub fn reset(&mut self) {
        self.pending.clear();
        self.epoch = self.epoch.wrapping_add(1);
        self.active = false;
        self.confirmed = 0;
    }

    /// mosh-go `SetCursor` — only tracks server cursor when inactive.
    pub fn set_cursor(&mut self, x: usize, y: usize) {
        if !self.active {
            self.cur_x = x;
            self.cur_y = y;
        }
    }

    /// mosh-go `ExpireStale`.
    pub fn expire_stale(&mut self, now: Instant) {
        let cutoff = now.checked_sub(PREDICTION_TIMEOUT).unwrap_or(now);
        let mut changed = false;
        while self
            .pending
            .first()
            .map(|p| p.at < cutoff)
            .unwrap_or(false)
        {
            self.pending.remove(0);
            changed = true;
        }
        if changed && self.pending.is_empty() {
            self.active = false;
        }
    }

    /// mosh-go `Confirm`.
    pub fn confirm(&mut self, fb: &Framebuffer) {
        if !self.active || self.pending.is_empty() {
            self.cur_x = fb.cur_x;
            self.cur_y = fb.cur_y;
            return;
        }

        let mut confirmed = 0usize;
        while confirmed < self.pending.len() {
            let pred = &self.pending[confirmed];
            if pred.epoch != self.epoch {
                confirmed += 1;
                continue;
            }
            let Some(cell) = fb.cell_at(pred.x, pred.y) else {
                self.reset();
                self.cur_x = fb.cur_x;
                self.cur_y = fb.cur_y;
                return;
            };
            if cell.ch == pred.ch {
                confirmed += 1;
            } else if (cell.ch == ' ' || cell.ch == '\0') && pred.ch != ' ' {
                // Server not caught up yet.
                break;
            } else {
                // Divergence.
                self.reset();
                self.cur_x = fb.cur_x;
                self.cur_y = fb.cur_y;
                return;
            }
        }

        if confirmed > 0 {
            self.pending.drain(..confirmed);
            self.confirmed = self.confirmed.saturating_add(confirmed);
        }

        if self.pending.is_empty() {
            self.active = false;
            self.cur_x = fb.cur_x;
            self.cur_y = fb.cur_y;
        }
    }

    /// mosh-go `Overlay` — mutate `fb` in place with underlined predictions.
    pub fn overlay(&self, fb: &mut Framebuffer) {
        if !self.active || !self.show {
            return;
        }
        for pred in &self.pending {
            if pred.epoch != self.epoch {
                continue;
            }
            if let Some(cell) = fb.cell_at_mut(pred.x, pred.y) {
                cell.ch = pred.ch;
                cell.width = 1;
                cell.attr.under = true;
            }
        }
        if !self.pending.is_empty() {
            fb.cur_x = self.cur_x.min(fb.cols.saturating_sub(1));
            fb.cur_y = self.cur_y.min(fb.rows.saturating_sub(1));
        }
    }
}

fn is_print(ch: char) -> bool {
    !ch.is_control()
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
            using_overlay_path: matches!(preference, DisplayPreference::Always),
        }
    }

    pub fn predictor(&self) -> &Predictor {
        &self.predictor
    }

    pub fn host_fb(&self) -> &Framebuffer {
        &self.host_fb
    }

    pub fn resize(&mut self, cols: usize, rows: usize) {
        self.host_fb.resize(cols, rows);
        if let Some(shown) = self.last_shown.as_mut() {
            shown.resize(cols, rows);
        }
        self.predictor.reset();
        self.predictor.set_cursor(self.host_fb.cur_x, self.host_fb.cur_y);
    }

    pub fn set_srtt(&mut self, srtt: Option<Duration>) {
        let was = self.predictor.should_show();
        self.predictor.set_srtt(srtt);
        let now = self.predictor.should_show();
        if was && !now {
            self.predictor.reset();
            // Fall back to host-only shown state.
            self.last_shown = Some(self.host_fb.clone());
            self.using_overlay_path = false;
        } else if !was && now {
            // Enter overlay path with last_shown = pure host.
            self.last_shown = Some(self.host_fb.clone());
            self.using_overlay_path = true;
        }
    }

    /// HostBytes (or raw hoststring) arrived from mosh-server.
    pub fn on_host_bytes(&mut self, hoststring: &[u8]) -> Vec<u8> {
        crate::ansi_apply::apply_ansi(&mut self.host_fb, hoststring);
        self.predictor
            .set_cursor(self.host_fb.cur_x, self.host_fb.cur_y);
        self.predictor.confirm(&self.host_fb);
        self.predictor.expire_stale(Instant::now());

        if !self.predictor.should_show() {
            self.using_overlay_path = false;
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
        self.predictor.keystroke(keys);
        self.render_overlay_path()
    }

    fn render_overlay_path(&mut self) -> Vec<u8> {
        let mut display = self.host_fb.clone();
        self.predictor.overlay(&mut display);
        let paint = display.diff(self.last_shown.as_ref());
        self.last_shown = Some(display);
        paint
    }
}

// ---------------------------------------------------------------------------
// Tests (ported from mosh-go predict_test.go + double-paint regression)
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ansi_apply::apply_ansi;
    use crate::framebuffer::Attr;

    #[test]
    fn env_preference_parsing() {
        assert_eq!(
            DisplayPreference::from_env_value("always"),
            DisplayPreference::Always
        );
        assert_eq!(
            DisplayPreference::from_env_value("NEVER"),
            DisplayPreference::Never
        );
        assert_eq!(
            DisplayPreference::from_env_value("adaptive"),
            DisplayPreference::Adaptive
        );
    }

    #[test]
    fn basic_echo_pending_positions() {
        // TestPredictorBasicEcho
        let mut p = Predictor::new(DisplayPreference::Always);
        p.set_srtt(None);
        p.set_cursor(0, 0);
        p.keystroke(b"abc");
        assert!(p.active());
        assert_eq!(p.pending_len(), 3);
        assert_eq!(p.pending_char(0), Some('a'));
        assert_eq!(p.pending_pos(0), Some((0, 0)));
        assert_eq!(p.pending_char(1), Some('b'));
        assert_eq!(p.pending_pos(1), Some((1, 0)));
        assert_eq!(p.pending_char(2), Some('c'));
        assert_eq!(p.pending_pos(2), Some((2, 0)));
    }

    #[test]
    fn overlay_underlines() {
        let mut p = Predictor::new(DisplayPreference::Always);
        p.set_cursor(0, 0);
        p.keystroke(b"hi");
        let mut fb = Framebuffer::new(80, 24);
        p.overlay(&mut fb);
        assert_eq!(fb.cell_at(0, 0).unwrap().ch, 'h');
        assert!(fb.cell_at(0, 0).unwrap().attr.under);
        assert_eq!(fb.cell_at(1, 0).unwrap().ch, 'i');
        assert!(fb.cell_at(1, 0).unwrap().attr.under);
        assert_eq!(fb.cur_x, 2);
        assert_eq!(fb.cur_y, 0);
    }

    #[test]
    fn confirm_all() {
        let mut p = Predictor::new(DisplayPreference::Always);
        p.set_cursor(0, 0);
        p.keystroke(b"ab");
        let mut fb = Framebuffer::new(80, 24);
        fb.put_rune(0, 0, 'a', Attr::default());
        fb.put_rune(1, 0, 'b', Attr::default());
        fb.cur_x = 2;
        p.confirm(&fb);
        assert!(!p.active());
        assert_eq!(p.pending_len(), 0);
    }

    #[test]
    fn partial_confirm() {
        let mut p = Predictor::new(DisplayPreference::Always);
        p.set_cursor(0, 0);
        p.keystroke(b"abc");
        let mut fb = Framebuffer::new(80, 24);
        fb.put_rune(0, 0, 'a', Attr::default());
        fb.cur_x = 1;
        p.confirm(&fb);
        assert!(p.active());
        assert_eq!(p.pending_len(), 2);
        assert_eq!(p.pending_char(0), Some('b'));
    }

    #[test]
    fn divergence_resets() {
        let mut p = Predictor::new(DisplayPreference::Always);
        p.set_cursor(0, 0);
        p.keystroke(b"abc");
        let mut fb = Framebuffer::new(80, 24);
        fb.put_rune(0, 0, 'x', Attr::default());
        fb.cur_x = 5;
        p.confirm(&fb);
        assert!(!p.active());
        assert_eq!(p.cur_x(), 5);
    }

    #[test]
    fn control_char_resets() {
        let mut p = Predictor::new(DisplayPreference::Always);
        p.set_cursor(0, 0);
        p.keystroke(b"ab");
        assert!(p.active());
        p.keystroke(b"\n");
        assert!(!p.active());
    }

    #[test]
    fn escape_resets() {
        let mut p = Predictor::new(DisplayPreference::Always);
        p.set_cursor(0, 0);
        p.keystroke(b"ab");
        p.keystroke(&[0x1b]);
        assert!(!p.active());
    }

    #[test]
    fn space_confirm() {
        let mut p = Predictor::new(DisplayPreference::Always);
        p.set_cursor(0, 0);
        p.keystroke(b"hi there");
        assert_eq!(p.pending_len(), 8);
        let mut fb = Framebuffer::new(80, 24);
        fb.put_rune(0, 0, 'h', Attr::default());
        fb.put_rune(1, 0, 'i', Attr::default());
        fb.put_rune(2, 0, ' ', Attr::default());
        fb.cur_x = 3;
        p.confirm(&fb);
        assert_eq!(p.pending_len(), 5);
        assert_eq!(p.pending_char(0), Some('t'));
    }

    #[test]
    fn set_cursor_not_overridden_while_active() {
        let mut p = Predictor::new(DisplayPreference::Always);
        p.set_cursor(10, 5);
        p.keystroke(b"x");
        p.set_cursor(0, 0);
        assert_eq!(p.cur_x(), 11);
    }

    #[test]
    fn overlay_does_not_touch_unpredicted() {
        let mut p = Predictor::new(DisplayPreference::Always);
        p.set_cursor(5, 0);
        p.keystroke(b"x");
        let mut fb = Framebuffer::new(80, 24);
        fb.put_rune(0, 0, 'A', Attr::default());
        fb.put_rune(1, 0, 'B', Attr::default());
        p.overlay(&mut fb);
        assert_eq!(fb.cell_at(0, 0).unwrap().ch, 'A');
        assert_eq!(fb.cell_at(1, 0).unwrap().ch, 'B');
        assert_eq!(fb.cell_at(5, 0).unwrap().ch, 'x');
    }

    /// Regression: dual-write would produce "ll"; Diff path must show single "l".
    #[test]
    fn no_double_paint_after_host_confirms_echo() {
        let mut pipe = DisplayPipeline::new(80, 24, DisplayPreference::Always);
        // Prompt
        let _ = pipe.on_host_bytes(b"\x1b[H\x1b[2J$ ");
        // User types "ls"
        let local = pipe.on_keystroke(b"ls");
        assert!(!local.is_empty(), "local overlay paint expected");
        // Server echoes with absolute CUP (relative path is also applied into host_fb)
        // Simulate server hoststring placing l,s at columns after "$ "
        // "$ " is cols 0,1 → echo at 2,3
        let host = b"\x1b[1;3Hl\x1b[1;4Hs\x1b[1;5H";
        let after = pipe.on_host_bytes(host);
        // Final host_fb should have one l and one s
        assert_eq!(pipe.host_fb().cell_at(2, 0).unwrap().ch, 'l');
        assert_eq!(pipe.host_fb().cell_at(3, 0).unwrap().ch, 's');
        // Shown screen via last_shown: no double l at 2 and 3 from prediction leftover
        let shown = pipe.last_shown.as_ref().unwrap();
        assert_eq!(shown.cell_at(2, 0).unwrap().ch, 'l');
        assert_eq!(shown.cell_at(3, 0).unwrap().ch, 's');
        // Confirmed cells should not stay underlined once fully confirmed
        assert!(!pipe.predictor().active());
        assert!(!shown.cell_at(2, 0).unwrap().attr.under);
        let _ = after;
    }

    #[test]
    fn apply_ansi_then_confirm_pipeline() {
        let mut pipe = DisplayPipeline::new(80, 24, DisplayPreference::Always);
        let _ = pipe.on_host_bytes(b"\x1b[1;1H");
        let _ = pipe.on_keystroke(b"ab");
        assert!(pipe.predictor().active());
        // Confirm via host
        let mut fb = Framebuffer::new(80, 24);
        apply_ansi(&mut fb, b"\x1b[1;1Hab");
        // Directly use confirm path through host bytes
        let _ = pipe.on_host_bytes(b"\x1b[1;1Hab");
        assert!(!pipe.predictor().active());
    }

    #[test]
    fn never_mode_passthrough() {
        let mut pipe = DisplayPipeline::new(80, 24, DisplayPreference::Never);
        let out = pipe.on_host_bytes(b"\x1b[Hhello");
        assert_eq!(out, b"\x1b[Hhello");
        assert!(pipe.on_keystroke(b"x").is_empty());
    }
}
