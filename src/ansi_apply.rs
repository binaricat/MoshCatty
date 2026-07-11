//! Apply HostBytes ANSI (stock `Display::new_frame` output) into a Framebuffer.
//!
//! Enough of the VT stream for prediction confirm: CUP, SGR, printable text,
//! CR/LF, erase-in-line/display, cursor show/hide. OSC is skipped.

use crate::framebuffer::{Attr, Color, ColorType, Framebuffer};

/// Parser pen state while consuming hoststring.
#[derive(Debug, Clone)]
struct Pen {
    attr: Attr,
}

/// Apply an ANSI/hoststring fragment into `fb` (mutates cells + cursor).
pub fn apply_ansi(fb: &mut Framebuffer, data: &[u8]) {
    let mut pen = Pen {
        attr: Attr::default(),
    };
    let mut i = 0;
    while i < data.len() {
        let b = data[i];
        if b == 0x1b {
            i = consume_escape(fb, &mut pen, data, i);
            continue;
        }
        if b == b'\r' {
            fb.cur_x = 0;
            i += 1;
            continue;
        }
        if b == b'\n' {
            fb.cur_y = (fb.cur_y + 1).min(fb.rows.saturating_sub(1));
            i += 1;
            continue;
        }
        if b == 0x08 {
            // BS
            if fb.cur_x > 0 {
                fb.cur_x -= 1;
            }
            i += 1;
            continue;
        }
        if b == 0x07 {
            // BEL
            i += 1;
            continue;
        }
        if b < 0x20 {
            i += 1;
            continue;
        }
        // UTF-8 printable
        let (ch, len) = decode_utf8_at(data, i);
        i += len;
        if ch == '\0' {
            continue;
        }
        let x = fb.cur_x;
        let y = fb.cur_y;
        if x < fb.cols && y < fb.rows {
            fb.put_rune(x, y, ch, pen.attr);
            // Advance one column; clamp to last col (no wrap).
            fb.cur_x = (x + 1).min(fb.cols.saturating_sub(1));
        }
    }
}

fn decode_utf8_at(data: &[u8], i: usize) -> (char, usize) {
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

fn consume_escape(fb: &mut Framebuffer, pen: &mut Pen, data: &[u8], start: usize) -> usize {
    let mut i = start + 1;
    if i >= data.len() {
        return data.len();
    }
    match data[i] {
        b'[' => {
            i += 1;
            // CSI params
            let param_start = i;
            while i < data.len() {
                let c = data[i];
                if (b'0'..=b'9').contains(&c) || c == b';' || c == b'?' || c == b':' || c == b' ' {
                    i += 1;
                    continue;
                }
                break;
            }
            if i >= data.len() {
                return data.len();
            }
            let final_byte = data[i];
            let params = &data[param_start..i];
            i += 1;
            apply_csi(fb, pen, params, final_byte);
            i
        }
        b']' => {
            // OSC ... BEL or ST
            i += 1;
            while i < data.len() {
                if data[i] == 0x07 {
                    return i + 1;
                }
                if data[i] == 0x1b && i + 1 < data.len() && data[i + 1] == b'\\' {
                    return i + 2;
                }
                i += 1;
            }
            data.len()
        }
        _ => {
            // Other ESC X — skip one more byte if present
            i + 1
        }
    }
}

fn parse_params(params: &[u8]) -> Vec<u32> {
    if params.is_empty() {
        return Vec::new();
    }
    // Strip leading '?' for private modes
    let raw = if params.first() == Some(&b'?') {
        &params[1..]
    } else {
        params
    };
    if raw.is_empty() {
        return Vec::new();
    }
    raw.split(|&b| b == b';')
        .map(|p| {
            if p.is_empty() {
                0
            } else {
                std::str::from_utf8(p)
                    .ok()
                    .and_then(|s| s.parse().ok())
                    .unwrap_or(0)
            }
        })
        .collect()
}

fn apply_csi(fb: &mut Framebuffer, pen: &mut Pen, params: &[u8], final_byte: u8) {
    let private = params.first() == Some(&b'?');
    let nums = parse_params(params);

    match final_byte {
        b'H' | b'f' => {
            // CUP — 1-indexed
            let row = nums.first().copied().unwrap_or(1).max(1) as usize;
            let col = nums.get(1).copied().unwrap_or(1).max(1) as usize;
            fb.cur_y = (row - 1).min(fb.rows.saturating_sub(1));
            fb.cur_x = (col - 1).min(fb.cols.saturating_sub(1));
        }
        b'A' => {
            let n = nums.first().copied().unwrap_or(1).max(1) as usize;
            fb.cur_y = fb.cur_y.saturating_sub(n);
        }
        b'B' => {
            let n = nums.first().copied().unwrap_or(1).max(1) as usize;
            fb.cur_y = (fb.cur_y + n).min(fb.rows.saturating_sub(1));
        }
        b'C' => {
            let n = nums.first().copied().unwrap_or(1).max(1) as usize;
            fb.cur_x = (fb.cur_x + n).min(fb.cols.saturating_sub(1));
        }
        b'D' => {
            let n = nums.first().copied().unwrap_or(1).max(1) as usize;
            fb.cur_x = fb.cur_x.saturating_sub(n);
        }
        b'G' => {
            // CHA — column absolute 1-indexed
            let col = nums.first().copied().unwrap_or(1).max(1) as usize;
            fb.cur_x = (col - 1).min(fb.cols.saturating_sub(1));
        }
        b'd' => {
            // VPA
            let row = nums.first().copied().unwrap_or(1).max(1) as usize;
            fb.cur_y = (row - 1).min(fb.rows.saturating_sub(1));
        }
        b'J' => {
            // ED
            let mode = nums.first().copied().unwrap_or(0);
            match mode {
                2 | 3 => {
                    for c in fb.cells.iter_mut() {
                        *c = Default::default();
                    }
                    // stock often homes separately
                }
                0 => {
                    // erase from cursor to end of screen
                    erase_from_cursor(fb);
                }
                1 => {
                    erase_to_cursor(fb);
                }
                _ => {}
            }
        }
        b'K' => {
            // EL
            let mode = nums.first().copied().unwrap_or(0);
            let y = fb.cur_y;
            match mode {
                0 => {
                    for x in fb.cur_x..fb.cols {
                        if let Some(c) = fb.cell_at_mut(x, y) {
                            *c = Default::default();
                        }
                    }
                }
                1 => {
                    for x in 0..=fb.cur_x.min(fb.cols.saturating_sub(1)) {
                        if let Some(c) = fb.cell_at_mut(x, y) {
                            *c = Default::default();
                        }
                    }
                }
                2 => {
                    for x in 0..fb.cols {
                        if let Some(c) = fb.cell_at_mut(x, y) {
                            *c = Default::default();
                        }
                    }
                }
                _ => {}
            }
        }
        b'X' => {
            // ECH
            let n = nums.first().copied().unwrap_or(1).max(1) as usize;
            let y = fb.cur_y;
            for x in fb.cur_x..(fb.cur_x + n).min(fb.cols) {
                if let Some(c) = fb.cell_at_mut(x, y) {
                    *c = Default::default();
                }
            }
        }
        b'm' => apply_sgr(pen, &nums),
        b'h' if private => {
            if nums.contains(&25) {
                fb.cursor_visible = true;
            }
        }
        b'l' if private => {
            if nums.contains(&25) {
                fb.cursor_visible = false;
            }
        }
        _ => {}
    }
}

fn erase_from_cursor(fb: &mut Framebuffer) {
    let (cx, cy) = (fb.cur_x, fb.cur_y);
    for y in cy..fb.rows {
        let start = if y == cy { cx } else { 0 };
        for x in start..fb.cols {
            if let Some(c) = fb.cell_at_mut(x, y) {
                *c = Default::default();
            }
        }
    }
}

fn erase_to_cursor(fb: &mut Framebuffer) {
    let (cx, cy) = (fb.cur_x, fb.cur_y);
    for y in 0..=cy {
        let end = if y == cy {
            cx.min(fb.cols.saturating_sub(1))
        } else {
            fb.cols.saturating_sub(1)
        };
        for x in 0..=end {
            if let Some(c) = fb.cell_at_mut(x, y) {
                *c = Default::default();
            }
        }
    }
}

fn apply_sgr(pen: &mut Pen, nums: &[u32]) {
    if nums.is_empty() {
        pen.attr = Attr::default();
        return;
    }
    let mut i = 0;
    while i < nums.len() {
        match nums[i] {
            0 => pen.attr = Attr::default(),
            1 => pen.attr.bold = true,
            2 => pen.attr.dim = true,
            3 => pen.attr.italic = true,
            4 => pen.attr.under = true,
            5 | 6 => pen.attr.blink = true,
            7 => pen.attr.reverse = true,
            9 => pen.attr.strike = true,
            22 => {
                pen.attr.bold = false;
                pen.attr.dim = false;
            }
            23 => pen.attr.italic = false,
            24 => pen.attr.under = false,
            25 => pen.attr.blink = false,
            27 => pen.attr.reverse = false,
            29 => pen.attr.strike = false,
            39 => pen.attr.fg = Color::default_color(),
            49 => pen.attr.bg = Color::default_color(),
            n @ 30..=37 => {
                pen.attr.fg = Color::index(n - 30);
            }
            n @ 40..=47 => {
                pen.attr.bg = Color::index(n - 40);
            }
            n @ 90..=97 => {
                pen.attr.fg = Color::index(n - 90 + 8);
            }
            n @ 100..=107 => {
                pen.attr.bg = Color::index(n - 100 + 8);
            }
            38 => {
                // 38;5;n or 38;2;r;g;b
                if i + 1 < nums.len() && nums[i + 1] == 5 && i + 2 < nums.len() {
                    pen.attr.fg = Color::index(nums[i + 2]);
                    i += 2;
                } else if i + 1 < nums.len() && nums[i + 1] == 2 && i + 4 < nums.len() {
                    pen.attr.fg = Color::rgb(
                        nums[i + 2] as u8,
                        nums[i + 3] as u8,
                        nums[i + 4] as u8,
                    );
                    i += 4;
                }
            }
            48 => {
                if i + 1 < nums.len() && nums[i + 1] == 5 && i + 2 < nums.len() {
                    pen.attr.bg = Color::index(nums[i + 2]);
                    i += 2;
                } else if i + 1 < nums.len() && nums[i + 1] == 2 && i + 4 < nums.len() {
                    pen.attr.bg = Color::rgb(
                        nums[i + 2] as u8,
                        nums[i + 3] as u8,
                        nums[i + 4] as u8,
                    );
                    i += 4;
                }
            }
            _ => {}
        }
        i += 1;
    }
    let _ = ColorType::Default; // keep import used for docs/readability
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cup_and_print() {
        let mut fb = Framebuffer::new(80, 24);
        apply_ansi(&mut fb, b"\x1b[1;1Hhi");
        assert_eq!(fb.cell_at(0, 0).unwrap().ch, 'h');
        assert_eq!(fb.cell_at(1, 0).unwrap().ch, 'i');
        assert_eq!(fb.cur_x, 2);
        assert_eq!(fb.cur_y, 0);
    }

    #[test]
    fn clear_and_prompt() {
        let mut fb = Framebuffer::new(80, 24);
        apply_ansi(&mut fb, b"\x1b[H\x1b[2J$ ");
        assert_eq!(fb.cell_at(0, 0).unwrap().ch, '$');
        assert_eq!(fb.cell_at(1, 0).unwrap().ch, ' ');
    }

    #[test]
    fn underline_sgr() {
        let mut fb = Framebuffer::new(80, 24);
        apply_ansi(&mut fb, b"\x1b[4ma\x1b[24m");
        assert!(fb.cell_at(0, 0).unwrap().attr.under);
    }
}
