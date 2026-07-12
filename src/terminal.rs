//! Terminal state application for the Mosh client.
//!
//! Stock mosh-server / mosh-go send HostBytes as ANSI escape sequences that
//! transform the client's view of the screen (CUP + cell paints). The client
//! does not re-parse a full cell-grid SSP object for paint — it writes the
//! hoststring stream to the local terminal (stdio / PTY consumer).
//!
//! This module accumulates that stream, strips ANSI for tests, and provides
//! helpers used by the CLI and integration tests.

use crate::pb::HostInstruction;

/// Local view of applied host terminal updates.
#[derive(Debug, Default)]
pub struct TerminalView {
    /// Raw bytes received from HostBytes (ANSI screen diffs).
    paint: Vec<u8>,
    cols: u16,
    rows: u16,
    /// Max HostInstruction.echo_ack_num seen (stock late_ack for prediction).
    echo_ack: u64,
}

impl TerminalView {
    pub fn new(cols: u16, rows: u16) -> Self {
        Self {
            paint: Vec::new(),
            cols,
            rows,
            echo_ack: 0,
        }
    }

    pub fn cols(&self) -> u16 {
        self.cols
    }

    pub fn rows(&self) -> u16 {
        self.rows
    }

    /// Cap retained paint history for long sessions (live path only needs the
    /// returned chunk; history is for tests / diagnostics).
    const PAINT_HISTORY_CAP: usize = 64 * 1024;

    /// Highest echo_ack from the server (stock `local_frame_late_acked`).
    pub fn echo_ack(&self) -> u64 {
        self.echo_ack
    }

    /// Apply a HostMessage diff (list of host instructions).
    /// Returns the bytes that should be written to the local terminal.
    pub fn apply_host_diff(&mut self, diff: &[u8]) -> Vec<u8> {
        let instrs = match HostInstruction::decode_message(diff) {
            Ok(i) => i,
            Err(_) => return Vec::new(),
        };
        let mut out = Vec::new();
        for hi in instrs {
            if hi.width > 0 && hi.height > 0 {
                self.cols = hi.width as u16;
                self.rows = hi.height as u16;
            }
            // Stock stmclient: late_ack comes from EchoAck / echo_ack_num.
            if hi.echo_ack_num >= 0 {
                let n = hi.echo_ack_num as u64;
                if n > self.echo_ack {
                    self.echo_ack = n;
                }
            }
            if !hi.hoststring.is_empty() {
                out.extend_from_slice(&hi.hoststring);
                self.paint.extend_from_slice(&hi.hoststring);
            }
        }
        // Bound history so multi-hour sessions do not retain full scrollback.
        if self.paint.len() > Self::PAINT_HISTORY_CAP {
            let drop = self.paint.len() - Self::PAINT_HISTORY_CAP;
            self.paint.drain(..drop);
        }
        out
    }

    /// All paint bytes accumulated so far.
    pub fn paint_bytes(&self) -> &[u8] {
        &self.paint
    }

    /// Drain newly accumulated paint since last drain (tests / buffered consumers).
    pub fn take_paint(&mut self) -> Vec<u8> {
        std::mem::take(&mut self.paint)
    }

    /// Whether the view has any non-empty screen content applied.
    pub fn has_content(&self) -> bool {
        !self.paint.is_empty()
    }
}

/// Strip CSI / OSC ANSI sequences for token matching in tests.
pub fn strip_ansi(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut chars = input.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\u{1b}' {
            match chars.peek() {
                Some('[') => {
                    chars.next();
                    // CSI: params then final byte 0x40-0x7E
                    while let Some(&ch) = chars.peek() {
                        chars.next();
                        if ('@'..='~').contains(&ch) {
                            break;
                        }
                    }
                }
                Some(']') => {
                    // OSC ... BEL or ST
                    chars.next();
                    while let Some(&ch) = chars.peek() {
                        chars.next();
                        if ch == '\u{7}' {
                            break;
                        }
                        if ch == '\u{1b}' {
                            if chars.peek() == Some(&'\\') {
                                chars.next();
                            }
                            break;
                        }
                    }
                }
                Some(_) => {
                    // Other ESC sequences: skip next char if present
                    let _ = chars.next();
                }
                None => {}
            }
        } else {
            out.push(c);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pb::HostInstruction;

    #[test]
    fn apply_hoststring_produces_paint() {
        let mut view = TerminalView::new(80, 24);
        let msg = HostInstruction::encode_message(&[HostInstruction {
            hoststring: b"\x1b[H$ echo hi\r\nhi\r\n".to_vec(),
            width: 0,
            height: 0,
            echo_ack_num: -1,
        }]);
        let painted = view.apply_host_diff(&msg);
        assert!(!painted.is_empty());
        assert!(view.has_content());
        let plain = strip_ansi(std::str::from_utf8(view.paint_bytes()).unwrap());
        assert!(plain.contains("echo hi"));
        assert!(plain.contains("hi"));
    }

    #[test]
    fn strip_ansi_removes_csi() {
        assert_eq!(strip_ansi("\x1b[31mred\x1b[0m"), "red");
        assert_eq!(strip_ansi("\x1b[?25lhi\x1b[?25h"), "hi");
    }

    #[test]
    fn strip_ansi_osc_title() {
        // OSC 0 title sequences appear in real mosh HostBytes paint
        let s = "\x1b]0;root@host: ~\x07prompt$ ";
        assert_eq!(strip_ansi(s), "prompt$ ");
    }

    #[test]
    fn apply_resize_updates_dims() {
        let mut view = TerminalView::new(80, 24);
        let msg = HostInstruction::encode_message(&[HostInstruction {
            width: 132,
            height: 43,
            echo_ack_num: -1,
            ..Default::default()
        }]);
        let paint = view.apply_host_diff(&msg);
        assert!(paint.is_empty());
        assert_eq!(view.cols(), 132);
        assert_eq!(view.rows(), 43);
    }

    #[test]
    fn apply_malformed_diff_is_noop() {
        let mut view = TerminalView::new(80, 24);
        let paint = view.apply_host_diff(&[0x80, 0x80, 0x80]);
        assert!(paint.is_empty());
        assert!(!view.has_content());
    }

    #[test]
    fn take_paint_drains() {
        let mut view = TerminalView::new(80, 24);
        let msg = HostInstruction::encode_message(&[HostInstruction {
            hoststring: b"abc".to_vec(),
            echo_ack_num: -1,
            ..Default::default()
        }]);
        let _ = view.apply_host_diff(&msg);
        assert_eq!(view.take_paint(), b"abc");
        assert!(!view.has_content());
    }

    #[test]
    fn multi_instruction_host_message() {
        let mut view = TerminalView::new(80, 24);
        let msg = HostInstruction::encode_message(&[
            HostInstruction {
                hoststring: b"\x1b[H".to_vec(),
                echo_ack_num: -1,
                ..Default::default()
            },
            HostInstruction {
                hoststring: b"line1\r\n".to_vec(),
                echo_ack_num: -1,
                ..Default::default()
            },
            HostInstruction {
                hoststring: b"line2".to_vec(),
                echo_ack_num: 1,
                ..Default::default()
            },
        ]);
        let painted = view.apply_host_diff(&msg);
        let plain = strip_ansi(std::str::from_utf8(&painted).unwrap());
        assert!(plain.contains("line1"));
        assert!(plain.contains("line2"));
    }

    #[test]
    fn utf8_cjk_in_hoststring() {
        let mut view = TerminalView::new(80, 24);
        let msg = HostInstruction::encode_message(&[HostInstruction {
            hoststring: "你好世界".as_bytes().to_vec(),
            echo_ack_num: -1,
            ..Default::default()
        }]);
        let painted = view.apply_host_diff(&msg);
        assert_eq!(std::str::from_utf8(&painted).unwrap(), "你好世界");
    }

    #[test]
    fn echo_ack_tracks_max_from_host_instructions() {
        let mut view = TerminalView::new(80, 24);
        assert_eq!(view.echo_ack(), 0);
        let msg = HostInstruction::encode_message(&[
            HostInstruction {
                hoststring: b"a".to_vec(),
                width: 80,
                height: 24,
                echo_ack_num: 3,
            },
            HostInstruction {
                hoststring: b"b".to_vec(),
                width: 0,
                height: 0,
                echo_ack_num: 5,
            },
            HostInstruction {
                hoststring: Vec::new(),
                width: 0,
                height: 0,
                echo_ack_num: 4, // lower — must not regress
            },
        ]);
        let painted = view.apply_host_diff(&msg);
        assert_eq!(painted, b"ab");
        assert_eq!(view.echo_ack(), 5);
    }
}
