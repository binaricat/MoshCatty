//! Speculative local echo (prediction) for high-latency links.
//!
//! Stock mosh paints predicted keystrokes immediately and underlines them
//! until the server's HostBytes frame confirms the cell. MoshCatty historically
//! only forwarded HostBytes, so typing felt like plain SSH and never showed
//! prediction underlines (Netcatty #2121).
//!
//! This module implements a *minimal* predictor:
//! - Printable ASCII / UTF-8 scalar grapheme starts (byte < 0x80 printable,
//!   or leading UTF-8 multibyte) are echoed with SGR underline when active.
//! - Backspace erases one outstanding prediction cell.
//! - Any other control / escape sequence clears outstanding predictions
//!   (we stop guessing after navigation or control keys).
//! - Host frames clear outstanding predictions (server paint is authoritative).
//!
//! Display modes match stock `MOSH_PREDICTION_DISPLAY`:
//! - `always` — always show predictions
//! - `never` — disable
//! - `adaptive` (default) — show when SRTT ≥ 20 ms (stock `SRTT_TRIGGER_LOW`)

use std::time::Duration;

/// Stock mosh: predictions are shown on adaptive mode once RTT is at least this.
const SRTT_TRIGGER_LOW: Duration = Duration::from_millis(20);

/// SGR underline on / off around each predicted cell (stock flagging look).
const SGR_UNDERLINE_ON: &[u8] = b"\x1b[4m";
const SGR_UNDERLINE_OFF: &[u8] = b"\x1b[24m";
/// Erase one cell to the left (backspace over a predicted char).
const ERASE_ONE: &[u8] = b"\x08 \x08";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DisplayPreference {
    Always,
    Never,
    Adaptive,
}

impl DisplayPreference {
    /// Parse stock-compatible `MOSH_PREDICTION_DISPLAY` values.
    pub fn from_env_value(raw: &str) -> Self {
        match raw.trim().to_ascii_lowercase().as_str() {
            "always" | "yes" | "1" | "true" | "on" => Self::Always,
            "never" | "no" | "0" | "false" | "off" => Self::Never,
            _ => Self::Adaptive,
        }
    }

    pub fn from_env() -> Self {
        match std::env::var("MOSH_PREDICTION_DISPLAY") {
            Ok(v) => Self::from_env_value(&v),
            Err(_) => Self::Adaptive,
        }
    }
}

/// Tracks speculative local echo for one mosh-client session.
#[derive(Debug)]
pub struct LocalPredictor {
    preference: DisplayPreference,
    /// Number of predicted cells still outstanding (awaiting host confirm).
    outstanding: usize,
    /// Cached whether predictions should paint right now.
    active: bool,
}

impl LocalPredictor {
    pub fn new(preference: DisplayPreference) -> Self {
        Self {
            preference,
            outstanding: 0,
            active: matches!(preference, DisplayPreference::Always),
        }
    }

    pub fn preference(&self) -> DisplayPreference {
        self.preference
    }

    pub fn outstanding(&self) -> usize {
        self.outstanding
    }

    /// Update adaptive trigger from the transport SRTT sample.
    pub fn set_srtt(&mut self, srtt: Option<Duration>) {
        self.active = match self.preference {
            DisplayPreference::Always => true,
            DisplayPreference::Never => false,
            DisplayPreference::Adaptive => srtt.map(|d| d >= SRTT_TRIGGER_LOW).unwrap_or(false),
        };
    }

    /// Server HostBytes arrived — authoritative screen state replaces guesses.
    pub fn on_host_paint(&mut self) {
        self.outstanding = 0;
    }

    /// Produce local paint for `keys` (also forwarded to the server by the caller).
    pub fn predict(&mut self, keys: &[u8]) -> Vec<u8> {
        if keys.is_empty() || !self.active {
            // Still track bookkeeping for backspace after a mode flip? Clear.
            if !self.active {
                self.outstanding = 0;
            }
            return Vec::new();
        }

        let mut out = Vec::new();
        let mut i = 0;
        while i < keys.len() {
            let b = keys[i];
            // ESC sequences (CSI/SS3/etc.): stop predicting this burst.
            if b == 0x1b {
                self.outstanding = 0;
                break;
            }
            // Backspace / DEL
            if b == 0x08 || b == 0x7f {
                if self.outstanding > 0 {
                    out.extend_from_slice(ERASE_ONE);
                    self.outstanding -= 1;
                }
                i += 1;
                continue;
            }
            // Other C0 controls (CR, LF, Tab, Ctrl-C, …): drop confidence.
            if b < 0x20 {
                self.outstanding = 0;
                i += 1;
                continue;
            }
            // Printable ASCII
            if b < 0x80 {
                out.extend_from_slice(SGR_UNDERLINE_ON);
                out.push(b);
                out.extend_from_slice(SGR_UNDERLINE_OFF);
                self.outstanding = self.outstanding.saturating_add(1);
                i += 1;
                continue;
            }
            // UTF-8 multibyte: echo the whole codepoint as one predicted cell.
            // Wide East-Asian glyphs may be wrong width; host paint corrects.
            let width = utf8_char_width(b);
            if width == 0 || i + width > keys.len() {
                self.outstanding = 0;
                break;
            }
            out.extend_from_slice(SGR_UNDERLINE_ON);
            out.extend_from_slice(&keys[i..i + width]);
            out.extend_from_slice(SGR_UNDERLINE_OFF);
            self.outstanding = self.outstanding.saturating_add(1);
            i += width;
        }
        out
    }
}

fn utf8_char_width(first: u8) -> usize {
    match first {
        0x00..=0x7F => 1,
        0xC2..=0xDF => 2,
        0xE0..=0xEF => 3,
        0xF0..=0xF4 => 4,
        _ => 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn env_preference_parsing() {
        assert_eq!(DisplayPreference::from_env_value("always"), DisplayPreference::Always);
        assert_eq!(DisplayPreference::from_env_value("NEVER"), DisplayPreference::Never);
        assert_eq!(DisplayPreference::from_env_value("adaptive"), DisplayPreference::Adaptive);
        assert_eq!(DisplayPreference::from_env_value("bogus"), DisplayPreference::Adaptive);
    }

    #[test]
    fn adaptive_stays_quiet_on_low_rtt() {
        let mut p = LocalPredictor::new(DisplayPreference::Adaptive);
        p.set_srtt(Some(Duration::from_millis(5)));
        assert!(p.predict(b"ls").is_empty());
        assert_eq!(p.outstanding(), 0);
    }

    #[test]
    fn adaptive_underlines_on_high_rtt() {
        let mut p = LocalPredictor::new(DisplayPreference::Adaptive);
        p.set_srtt(Some(Duration::from_millis(80)));
        let paint = p.predict(b"ls");
        // underline + l + off + underline + s + off
        assert!(paint.windows(SGR_UNDERLINE_ON.len()).any(|w| w == SGR_UNDERLINE_ON));
        assert!(paint.contains(&b'l'));
        assert!(paint.contains(&b's'));
        assert_eq!(p.outstanding(), 2);
    }

    #[test]
    fn always_mode_predicts_without_rtt() {
        let mut p = LocalPredictor::new(DisplayPreference::Always);
        let paint = p.predict(b"a");
        assert!(paint.contains(&b'a'));
        assert_eq!(p.outstanding(), 1);
    }

    #[test]
    fn never_mode_is_silent() {
        let mut p = LocalPredictor::new(DisplayPreference::Never);
        p.set_srtt(Some(Duration::from_millis(500)));
        assert!(p.predict(b"abc").is_empty());
    }

    #[test]
    fn backspace_erases_one_prediction() {
        let mut p = LocalPredictor::new(DisplayPreference::Always);
        let _ = p.predict(b"ab");
        assert_eq!(p.outstanding(), 2);
        let paint = p.predict(&[0x7f]);
        assert_eq!(paint, ERASE_ONE);
        assert_eq!(p.outstanding(), 1);
    }

    #[test]
    fn host_paint_clears_outstanding() {
        let mut p = LocalPredictor::new(DisplayPreference::Always);
        let _ = p.predict(b"xy");
        p.on_host_paint();
        assert_eq!(p.outstanding(), 0);
    }

    #[test]
    fn control_char_clears_outstanding() {
        let mut p = LocalPredictor::new(DisplayPreference::Always);
        let _ = p.predict(b"ab");
        let paint = p.predict(b"\r");
        assert!(paint.is_empty());
        assert_eq!(p.outstanding(), 0);
    }

    #[test]
    fn utf8_multibyte_counts_as_one_cell() {
        let mut p = LocalPredictor::new(DisplayPreference::Always);
        // "你" = E4 BD A0
        let paint = p.predict(&[0xE4, 0xBD, 0xA0]);
        assert!(paint.windows(3).any(|w| w == [0xE4, 0xBD, 0xA0]));
        assert_eq!(p.outstanding(), 1);
    }
}
