//! Apply HostBytes ANSI (stock `Display::new_frame` output) into a Framebuffer.
//!
//! Enough of the VT stream for prediction confirm: CUP, SGR, printable text,
//! CR/LF, erase-in-line/display, cursor show/hide. OSC is skipped.

use crate::framebuffer::{Attr, Color, ColorType, Framebuffer};

/// Sticky SGR pen + incomplete-sequence carry across HostBytes chunks.
#[derive(Debug, Clone, Default)]
pub struct AnsiPen {
    pub attr: Attr,
    /// Incomplete ESC/CSI/OSC/UTF-8 fragment carried to the next HostBytes chunk.
    pub carry: Vec<u8>,
}

/// Apply an ANSI/hoststring fragment into `fb` (mutates cells + cursor).
/// Resets pen to default (call [`apply_ansi_with_pen`] to keep sticky SGR).
pub fn apply_ansi(fb: &mut Framebuffer, data: &[u8]) {
    let mut pen = AnsiPen::default();
    apply_ansi_with_pen(fb, &mut pen, data);
}

/// Like [`apply_ansi`] but preserves `pen` (and incomplete carry) across calls.
pub fn apply_ansi_with_pen(fb: &mut Framebuffer, pen: &mut AnsiPen, data: &[u8]) {
    // Reassemble incomplete ESC/UTF-8 from the previous HostBytes chunk.
    let buf: Vec<u8> = if pen.carry.is_empty() {
        data.to_vec()
    } else {
        let mut v = std::mem::take(&mut pen.carry);
        v.extend_from_slice(data);
        v
    };
    let data = &buf[..];
    let mut i = 0;
    while i < data.len() {
        let b = data[i];
        if b == 0x1b {
            match consume_escape(fb, pen, data, i) {
                EscapeConsume::Done(next) => i = next,
                EscapeConsume::NeedMore(start) => {
                    pen.carry = data[start..].to_vec();
                    return;
                }
            }
            continue;
        }
        if b == b'\r' {
            fb.cur_x = 0;
            fb.next_print_will_wrap = false;
            i += 1;
            continue;
        }
        if b == b'\n' {
            // Scroll when at bottom (stock Display often uses CR+LF scroll).
            if fb.cur_y + 1 >= fb.rows {
                scroll_up(fb, 1);
                fb.cur_y = fb.rows.saturating_sub(1);
            } else {
                fb.cur_y += 1;
            }
            fb.next_print_will_wrap = false;
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
        // UTF-8 printable (may be incomplete at chunk end)
        match decode_utf8_at(data, i) {
            Utf8Decode::NeedMore => {
                pen.carry = data[i..].to_vec();
                return;
            }
            Utf8Decode::Char(ch, len) => {
                i += len;
                if ch == '\0' {
                    continue;
                }
                // Stock next_print_will_wrap: wrap *before* placing the next glyph.
                if fb.next_print_will_wrap {
                    fb.next_print_will_wrap = false;
                    if fb.cur_y + 1 < fb.rows {
                        fb.cur_y += 1;
                        fb.cur_x = 0;
                    } else {
                        scroll_up(fb, 1);
                        fb.cur_y = fb.rows.saturating_sub(1);
                        fb.cur_x = 0;
                    }
                }
                let x = fb.cur_x;
                let y = fb.cur_y;
                if x < fb.cols && y < fb.rows {
                    let w = fb.put_rune(x, y, ch, pen.attr);
                    if w == 0 {
                        // Combining / ZW: do not advance cursor.
                        continue;
                    }
                    // Stock DECAWM: stay on last col and set wrap flag (no immediate wrap).
                    if x + w >= fb.cols {
                        fb.cur_x = fb.cols.saturating_sub(1);
                        fb.next_print_will_wrap = true;
                    } else {
                        fb.cur_x = x + w;
                        fb.next_print_will_wrap = false;
                    }
                }
            }
        }
    }
}

fn scroll_up(fb: &mut Framebuffer, lines: usize) {
    let lines = lines.min(fb.rows);
    if lines == 0 {
        return;
    }
    let cols = fb.cols;
    let rows = fb.rows;
    if lines >= rows {
        for c in fb.cells.iter_mut() {
            *c = Default::default();
        }
        return;
    }
    fb.cells.rotate_left(lines * cols);
    let start = (rows - lines) * cols;
    for c in &mut fb.cells[start..] {
        *c = Default::default();
    }
}

enum Utf8Decode {
    Char(char, usize),
    NeedMore,
}

enum EscapeConsume {
    Done(usize),
    NeedMore(usize),
}

fn decode_utf8_at(data: &[u8], i: usize) -> Utf8Decode {
    let b0 = data[i];
    if b0 < 0x80 {
        return Utf8Decode::Char(b0 as char, 1);
    }
    let width = if b0 & 0xE0 == 0xC0 {
        2
    } else if b0 & 0xF0 == 0xE0 {
        3
    } else if b0 & 0xF8 == 0xF0 {
        4
    } else {
        return Utf8Decode::Char('\u{FFFD}', 1);
    };
    if i + width > data.len() {
        return Utf8Decode::NeedMore;
    }
    match std::str::from_utf8(&data[i..i + width]) {
        Ok(s) => Utf8Decode::Char(s.chars().next().unwrap_or('\u{FFFD}'), width),
        Err(_) => Utf8Decode::Char('\u{FFFD}', 1),
    }
}

fn consume_escape(
    fb: &mut Framebuffer,
    pen: &mut AnsiPen,
    data: &[u8],
    start: usize,
) -> EscapeConsume {
    let mut i = start + 1;
    if i >= data.len() {
        return EscapeConsume::NeedMore(start);
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
                return EscapeConsume::NeedMore(start);
            }
            let final_byte = data[i];
            let params = &data[param_start..i];
            i += 1;
            apply_csi(fb, pen, params, final_byte);
            EscapeConsume::Done(i)
        }
        b']' => {
            // OSC ... BEL or ST
            i += 1;
            while i < data.len() {
                if data[i] == 0x07 {
                    return EscapeConsume::Done(i + 1);
                }
                if data[i] == 0x1b && i + 1 < data.len() && data[i + 1] == b'\\' {
                    return EscapeConsume::Done(i + 2);
                }
                i += 1;
            }
            EscapeConsume::NeedMore(start)
        }
        _ => {
            // Other ESC X — need the following byte if missing
            if i >= data.len() {
                EscapeConsume::NeedMore(start)
            } else {
                EscapeConsume::Done(i + 1)
            }
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

fn apply_csi(fb: &mut Framebuffer, pen: &mut AnsiPen, params: &[u8], final_byte: u8) {
    let private = params.first() == Some(&b'?');
    let nums = parse_params(params);

    match final_byte {
        b'H' | b'f' => {
            // CUP — 1-indexed
            let row = nums.first().copied().unwrap_or(1).max(1) as usize;
            let col = nums.get(1).copied().unwrap_or(1).max(1) as usize;
            fb.cur_y = (row - 1).min(fb.rows.saturating_sub(1));
            fb.cur_x = (col - 1).min(fb.cols.saturating_sub(1));
            fb.next_print_will_wrap = false;
        }
        b'A' => {
            let n = nums.first().copied().unwrap_or(1).max(1) as usize;
            fb.cur_y = fb.cur_y.saturating_sub(n);
            fb.next_print_will_wrap = false;
        }
        b'B' => {
            let n = nums.first().copied().unwrap_or(1).max(1) as usize;
            fb.cur_y = (fb.cur_y + n).min(fb.rows.saturating_sub(1));
            fb.next_print_will_wrap = false;
        }
        b'C' => {
            let n = nums.first().copied().unwrap_or(1).max(1) as usize;
            fb.cur_x = (fb.cur_x + n).min(fb.cols.saturating_sub(1));
            fb.next_print_will_wrap = false;
        }
        b'D' => {
            let n = nums.first().copied().unwrap_or(1).max(1) as usize;
            fb.cur_x = fb.cur_x.saturating_sub(n);
            fb.next_print_will_wrap = false;
        }
        b'G' => {
            // CHA — column absolute 1-indexed
            let col = nums.first().copied().unwrap_or(1).max(1) as usize;
            fb.cur_x = (col - 1).min(fb.cols.saturating_sub(1));
            fb.next_print_will_wrap = false;
        }
        b'd' => {
            // VPA
            let row = nums.first().copied().unwrap_or(1).max(1) as usize;
            fb.cur_y = (row - 1).min(fb.rows.saturating_sub(1));
            fb.next_print_will_wrap = false;
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
        b'@' => {
            // ICH — insert n blank cells at cursor
            let n = nums.first().copied().unwrap_or(1).max(1) as usize;
            insert_chars(fb, n);
        }
        b'P' => {
            // DCH — delete n cells at cursor
            let n = nums.first().copied().unwrap_or(1).max(1) as usize;
            delete_chars(fb, n);
        }
        b'L' => {
            // IL — insert n blank lines
            let n = nums.first().copied().unwrap_or(1).max(1) as usize;
            insert_lines(fb, n);
        }
        b'M' => {
            // DL — delete n lines
            let n = nums.first().copied().unwrap_or(1).max(1) as usize;
            delete_lines(fb, n);
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

fn insert_chars(fb: &mut Framebuffer, n: usize) {
    let y = fb.cur_y;
    let x = fb.cur_x;
    if y >= fb.rows || x >= fb.cols || n == 0 {
        return;
    }
    let n = n.min(fb.cols - x);
    // Shift right from end
    for col in (x..fb.cols - n).rev() {
        if let (Some(src), Some(dst)) = (fb.cell_at(col, y).copied(), fb.cell_at_mut(col + n, y)) {
            *dst = src;
        }
    }
    for col in x..x + n {
        if let Some(c) = fb.cell_at_mut(col, y) {
            *c = Default::default();
        }
    }
}

fn delete_chars(fb: &mut Framebuffer, n: usize) {
    let y = fb.cur_y;
    let x = fb.cur_x;
    if y >= fb.rows || x >= fb.cols || n == 0 {
        return;
    }
    let n = n.min(fb.cols - x);
    for col in x..fb.cols - n {
        if let (Some(src), Some(dst)) = (fb.cell_at(col + n, y).copied(), fb.cell_at_mut(col, y)) {
            *dst = src;
        }
    }
    for col in (fb.cols - n)..fb.cols {
        if let Some(c) = fb.cell_at_mut(col, y) {
            *c = Default::default();
        }
    }
}

fn insert_lines(fb: &mut Framebuffer, n: usize) {
    let y = fb.cur_y;
    if y >= fb.rows || n == 0 {
        return;
    }
    let n = n.min(fb.rows - y);
    let cols = fb.cols;
    // Shift rows down
    for row in (y..fb.rows - n).rev() {
        for col in 0..cols {
            if let (Some(src), Some(dst)) =
                (fb.cell_at(col, row).copied(), fb.cell_at_mut(col, row + n))
            {
                *dst = src;
            }
        }
    }
    for row in y..y + n {
        for col in 0..cols {
            if let Some(c) = fb.cell_at_mut(col, row) {
                *c = Default::default();
            }
        }
    }
}

fn delete_lines(fb: &mut Framebuffer, n: usize) {
    let y = fb.cur_y;
    if y >= fb.rows || n == 0 {
        return;
    }
    let n = n.min(fb.rows - y);
    let cols = fb.cols;
    for row in y..fb.rows - n {
        for col in 0..cols {
            if let (Some(src), Some(dst)) =
                (fb.cell_at(col, row + n).copied(), fb.cell_at_mut(col, row))
            {
                *dst = src;
            }
        }
    }
    for row in (fb.rows - n)..fb.rows {
        for col in 0..cols {
            if let Some(c) = fb.cell_at_mut(col, row) {
                *c = Default::default();
            }
        }
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

fn apply_sgr(pen: &mut AnsiPen, nums: &[u32]) {
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
                    pen.attr.fg =
                        Color::rgb(nums[i + 2] as u8, nums[i + 3] as u8, nums[i + 4] as u8);
                    i += 4;
                }
            }
            48 => {
                if i + 1 < nums.len() && nums[i + 1] == 5 && i + 2 < nums.len() {
                    pen.attr.bg = Color::index(nums[i + 2]);
                    i += 2;
                } else if i + 1 < nums.len() && nums[i + 1] == 2 && i + 4 < nums.len() {
                    pen.attr.bg =
                        Color::rgb(nums[i + 2] as u8, nums[i + 3] as u8, nums[i + 4] as u8);
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

    #[test]
    fn ich_inserts_blanks() {
        let mut fb = Framebuffer::new(10, 3);
        apply_ansi(&mut fb, b"\x1b[Habc");
        assert_eq!(fb.cell_at(0, 0).unwrap().ch, 'a');
        apply_ansi(&mut fb, b"\x1b[1;1H\x1b[2@");
        assert_eq!(fb.cell_at(0, 0).unwrap().ch, ' ');
        assert_eq!(fb.cell_at(1, 0).unwrap().ch, ' ');
        assert_eq!(fb.cell_at(2, 0).unwrap().ch, 'a');
        assert_eq!(fb.cell_at(3, 0).unwrap().ch, 'b');
        assert_eq!(fb.cell_at(4, 0).unwrap().ch, 'c');
    }

    #[test]
    fn dch_deletes_chars() {
        let mut fb = Framebuffer::new(10, 3);
        apply_ansi(&mut fb, b"\x1b[Habcde");
        apply_ansi(&mut fb, b"\x1b[1;2H\x1b[2P"); // delete 2 at col 1
        assert_eq!(fb.cell_at(0, 0).unwrap().ch, 'a');
        assert_eq!(fb.cell_at(1, 0).unwrap().ch, 'd');
        assert_eq!(fb.cell_at(2, 0).unwrap().ch, 'e');
    }

    #[test]
    fn split_csi_cup_reassembled_across_chunks() {
        let mut fb = Framebuffer::new(80, 24);
        let mut pen = AnsiPen::default();
        apply_ansi_with_pen(&mut fb, &mut pen, b"\x1b[1;");
        assert!(!pen.carry.is_empty(), "incomplete CSI must be carried");
        apply_ansi_with_pen(&mut fb, &mut pen, b"5H");
        assert!(pen.carry.is_empty());
        assert_eq!(fb.cur_y, 0);
        assert_eq!(fb.cur_x, 4, "CUP 1;5 → col 4");
    }

    #[test]
    fn split_utf8_reassembled_across_chunks() {
        let mut fb = Framebuffer::new(10, 3);
        let mut pen = AnsiPen::default();
        // '€' = E2 82 AC
        apply_ansi_with_pen(&mut fb, &mut pen, &[0xE2, 0x82]);
        assert!(!pen.carry.is_empty());
        apply_ansi_with_pen(&mut fb, &mut pen, &[0xAC]);
        assert!(pen.carry.is_empty());
        assert_eq!(fb.cell_at(0, 0).unwrap().ch, '€');
    }

    #[test]
    fn printable_defers_wrap_until_next_char() {
        let mut fb = Framebuffer::new(4, 3);
        apply_ansi(&mut fb, b"\x1b[Habcd"); // fills row 0 cols 0..3
                                            // Stock: after last col, stay at col 3 with wrap flag set.
        assert_eq!(fb.cur_x, 3);
        assert_eq!(fb.cur_y, 0);
        assert!(fb.next_print_will_wrap);
        assert_eq!(fb.cell_at(3, 0).unwrap().ch, 'd');
        apply_ansi(&mut fb, b"X");
        assert_eq!(fb.cur_y, 1);
        assert_eq!(fb.cur_x, 1);
        assert_eq!(fb.cell_at(0, 1).unwrap().ch, 'X');
        assert!(!fb.next_print_will_wrap);
    }

    #[test]
    fn cup_clears_wrap_flag() {
        let mut fb = Framebuffer::new(4, 3);
        apply_ansi(&mut fb, b"\x1b[Habcd");
        assert!(fb.next_print_will_wrap);
        apply_ansi(&mut fb, b"\x1b[1;1H");
        assert!(!fb.next_print_will_wrap);
        assert_eq!(fb.cur_x, 0);
        assert_eq!(fb.cur_y, 0);
    }
}
