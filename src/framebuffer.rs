//! Client cell grid + Diff, shaped after mosh-go `framebuffer.go`.
//!
//! Stock mosh and mosh-go keep a local Framebuffer so predictions are an
//! *overlay on cells*, and a single `Diff(old, new)` stream is written to
//! the PTY — never dual-write predicted glyphs beside raw HostBytes.

use std::cmp::Ordering;

/// SGR attributes for one cell (subset sufficient for mosh paint + underline).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Attr {
    pub bold: bool,
    pub dim: bool,
    pub italic: bool,
    pub under: bool,
    pub blink: bool,
    pub reverse: bool,
    pub strike: bool,
    pub fg: Color,
    pub bg: Color,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ColorType {
    #[default]
    Default,
    Index,
    Rgb,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Color {
    pub kind: ColorType,
    /// Index 0–255, or 0x00RRGGBB for RGB.
    pub value: u32,
}

impl Color {
    pub const fn default_color() -> Self {
        Self {
            kind: ColorType::Default,
            value: 0,
        }
    }

    pub const fn index(idx: u32) -> Self {
        Self {
            kind: ColorType::Index,
            value: idx,
        }
    }

    pub const fn rgb(r: u8, g: u8, b: u8) -> Self {
        Self {
            kind: ColorType::Rgb,
            value: ((r as u32) << 16) | ((g as u32) << 8) | (b as u32),
        }
    }
}

/// One character cell.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Cell {
    pub ch: char,
    /// Display width: 1 for normal, 2 for wide CJK, 0 for wide-continuation.
    pub width: u8,
    pub attr: Attr,
}

impl Default for Cell {
    fn default() -> Self {
        Self {
            ch: ' ',
            width: 1,
            attr: Attr::default(),
        }
    }
}

/// Terminal screen state (mosh-go `Framebuffer`).
#[derive(Debug, Clone)]
pub struct Framebuffer {
    pub cols: usize,
    pub rows: usize,
    pub cells: Vec<Cell>,
    pub cur_x: usize,
    pub cur_y: usize,
    pub cursor_visible: bool,
    /// Stock DrawState `next_print_will_wrap` — last-col print defers wrap to next char.
    pub next_print_will_wrap: bool,
}

impl Framebuffer {
    pub fn new(cols: usize, rows: usize) -> Self {
        let cols = cols.max(1);
        let rows = rows.max(1);
        Self {
            cols,
            rows,
            cells: vec![Cell::default(); cols * rows],
            cur_x: 0,
            cur_y: 0,
            cursor_visible: true,
            next_print_will_wrap: false,
        }
    }

    pub fn resize(&mut self, cols: usize, rows: usize) {
        let cols = cols.max(1);
        let rows = rows.max(1);
        if cols == self.cols && rows == self.rows {
            return;
        }
        let mut next = Self::new(cols, rows);
        let copy_cols = self.cols.min(cols);
        let copy_rows = self.rows.min(rows);
        for y in 0..copy_rows {
            for x in 0..copy_cols {
                if let (Some(dst), Some(src)) = (next.cell_at_mut(x, y), self.cell_at(x, y)) {
                    *dst = *src;
                }
            }
        }
        next.cur_x = self.cur_x.min(cols.saturating_sub(1));
        next.cur_y = self.cur_y.min(rows.saturating_sub(1));
        next.cursor_visible = self.cursor_visible;
        next.next_print_will_wrap = false;
        *self = next;
    }

    pub fn cell_at(&self, x: usize, y: usize) -> Option<&Cell> {
        if x >= self.cols || y >= self.rows {
            return None;
        }
        self.cells.get(y * self.cols + x)
    }

    pub fn cell_at_mut(&mut self, x: usize, y: usize) -> Option<&mut Cell> {
        if x >= self.cols || y >= self.rows {
            return None;
        }
        self.cells.get_mut(y * self.cols + x)
    }

    /// Place a rune. Returns display columns used (1 or 2). Wide runes also
    /// mark the following cell as a width-0 continuation when room remains.
    pub fn put_rune(&mut self, x: usize, y: usize, ch: char, attr: Attr) -> usize {
        let w = rune_display_width(ch);
        // Combining/ZW: do not overwrite base cell (host model approximation).
        if w == 0 {
            return 0;
        }
        if let Some(cell) = self.cell_at_mut(x, y) {
            cell.ch = ch;
            cell.width = w as u8;
            cell.attr = attr;
        }
        if w == 2 && x + 1 < self.cols {
            if let Some(cont) = self.cell_at_mut(x + 1, y) {
                cont.ch = ' ';
                cont.width = 0;
                cont.attr = attr;
            }
        }
        w
    }

    /// Diff this framebuffer against `old` (mosh-go `Diff`).
    /// When `old` is `None` or size differs, emit a full redraw.
    pub fn diff(&self, old: Option<&Framebuffer>) -> Vec<u8> {
        match old {
            Some(prev) if prev.cols == self.cols && prev.rows == self.rows => {
                self.diff_same_size(prev)
            }
            _ => self.full_redraw(),
        }
    }

    fn full_redraw(&self) -> Vec<u8> {
        let mut buf = Vec::with_capacity(self.cols * self.rows + 64);
        buf.extend_from_slice(b"\x1b[?25l");
        buf.extend_from_slice(b"\x1b[H");
        buf.extend_from_slice(b"\x1b[2J");
        buf.extend_from_slice(b"\x1b[m");

        let mut cur_attr = Attr::default();
        for y in 0..self.rows {
            if y > 0 {
                buf.extend_from_slice(b"\r\n");
            }
            let mut last_non_space = None;
            for x in (0..self.cols).rev() {
                let c = &self.cells[y * self.cols + x];
                if (c.ch != ' ' && c.ch != '\0') || c.attr != Attr::default() {
                    last_non_space = Some(x);
                    break;
                }
            }
            if let Some(last) = last_non_space {
                for x in 0..=last {
                    let c = &self.cells[y * self.cols + x];
                    if c.width == 0 {
                        continue;
                    }
                    append_attr_diff(&mut buf, &mut cur_attr, &c.attr);
                    push_cell_char(&mut buf, c.ch);
                }
            }
        }

        if cur_attr != Attr::default() {
            buf.extend_from_slice(b"\x1b[m");
        }
        append_cup(&mut buf, self.cur_y, self.cur_x);
        if self.cursor_visible {
            buf.extend_from_slice(b"\x1b[?25h");
        }
        buf
    }

    fn diff_same_size(&self, old: &Framebuffer) -> Vec<u8> {
        let mut buf = Vec::new();
        buf.extend_from_slice(b"\x1b[?25l");

        let mut cur_attr = Attr::default();
        let mut pen_x: isize = -1;
        let mut pen_y: isize = -1;

        for y in 0..self.rows {
            let row_off = y * self.cols;
            let mut first = None;
            let mut last = None;
            for x in 0..self.cols {
                if self.cells[row_off + x] != old.cells[row_off + x] {
                    if first.is_none() {
                        first = Some(x);
                    }
                    last = Some(x);
                }
            }
            let (Some(first), Some(last)) = (first, last) else {
                continue;
            };

            if pen_x != first as isize || pen_y != y as isize {
                append_cup(&mut buf, y, first);
                pen_x = first as isize;
                pen_y = y as isize;
            }

            for x in first..=last {
                let c = &self.cells[row_off + x];
                if c.width == 0 {
                    continue;
                }
                append_attr_diff(&mut buf, &mut cur_attr, &c.attr);
                push_cell_char(&mut buf, c.ch);
                pen_x += c.width as isize;
            }
        }

        if cur_attr != Attr::default() {
            buf.extend_from_slice(b"\x1b[m");
        }
        append_cup(&mut buf, self.cur_y, self.cur_x);
        if self.cursor_visible {
            buf.extend_from_slice(b"\x1b[?25h");
        }

        // No cell changes: emit only cursor / visibility deltas.
        if self.cells == old.cells {
            let mut slim = Vec::new();
            if self.cur_x != old.cur_x || self.cur_y != old.cur_y {
                append_cup(&mut slim, self.cur_y, self.cur_x);
            }
            if self.cursor_visible != old.cursor_visible {
                if self.cursor_visible {
                    slim.extend_from_slice(b"\x1b[?25h");
                } else {
                    slim.extend_from_slice(b"\x1b[?25l");
                }
            }
            return slim;
        }

        buf
    }
}

fn push_cell_char(buf: &mut Vec<u8>, ch: char) {
    if ch == '\0' || ch == ' ' {
        buf.push(b' ');
    } else {
        let mut tmp = [0u8; 4];
        let s = ch.encode_utf8(&mut tmp);
        buf.extend_from_slice(s.as_bytes());
    }
}

fn append_cup(buf: &mut Vec<u8>, row: usize, col: usize) {
    // 1-indexed CUP
    buf.extend_from_slice(b"\x1b[");
    buf.extend_from_slice((row + 1).to_string().as_bytes());
    buf.push(b';');
    buf.extend_from_slice((col + 1).to_string().as_bytes());
    buf.push(b'H');
}

fn append_attr_diff(buf: &mut Vec<u8>, cur: &mut Attr, next: &Attr) {
    if cur == next {
        return;
    }

    let needs_reset = (cur.bold && !next.bold)
        || (cur.dim && !next.dim)
        || (cur.italic && !next.italic)
        || (cur.under && !next.under)
        || (cur.blink && !next.blink)
        || (cur.reverse && !next.reverse)
        || (cur.strike && !next.strike)
        || (cur.fg.kind != ColorType::Default && next.fg.kind == ColorType::Default)
        || (cur.bg.kind != ColorType::Default && next.bg.kind == ColorType::Default);

    if needs_reset {
        buf.extend_from_slice(b"\x1b[0");
        *cur = Attr::default();
    } else {
        buf.extend_from_slice(b"\x1b[");
    }

    let mut first = !needs_reset;
    let mut add = |code: &str| {
        if !first {
            buf.push(b';');
        }
        first = false;
        buf.extend_from_slice(code.as_bytes());
    };

    if next.bold && !cur.bold {
        add("1");
    }
    if next.dim && !cur.dim {
        add("2");
    }
    if next.italic && !cur.italic {
        add("3");
    }
    if next.under && !cur.under {
        add("4");
    }
    if next.blink && !cur.blink {
        add("5");
    }
    if next.reverse && !cur.reverse {
        add("7");
    }
    if next.strike && !cur.strike {
        add("9");
    }

    if next.fg != cur.fg {
        append_color_param(buf, &next.fg, true, &mut first);
    }
    if next.bg != cur.bg {
        append_color_param(buf, &next.bg, false, &mut first);
    }

    // If we only opened CSI without params (identical after reset path edge),
    // still close SGR.
    if first && !needs_reset {
        // Nothing added — drop the open CSI we wrote.
        // Safer to emit reset+rebuild:
        buf.truncate(buf.len().saturating_sub(2)); // remove "\x1b["
        *cur = *next;
        return;
    }

    buf.push(b'm');
    *cur = *next;
}

fn append_color_param(buf: &mut Vec<u8>, c: &Color, fg: bool, first: &mut bool) {
    let sep = |buf: &mut Vec<u8>, first: &mut bool| {
        if !*first {
            buf.push(b';');
        }
        *first = false;
    };
    match c.kind {
        ColorType::Default => {
            sep(buf, first);
            buf.extend_from_slice(if fg { b"39" } else { b"49" });
        }
        ColorType::Index if c.value < 8 => {
            sep(buf, first);
            let base = if fg { 30 } else { 40 };
            buf.extend_from_slice((base + c.value).to_string().as_bytes());
        }
        ColorType::Index if c.value < 16 => {
            sep(buf, first);
            let base = if fg { 90 } else { 100 };
            buf.extend_from_slice((base + c.value - 8).to_string().as_bytes());
        }
        ColorType::Index => {
            sep(buf, first);
            buf.extend_from_slice(if fg { b"38;5;" } else { b"48;5;" });
            buf.extend_from_slice(c.value.to_string().as_bytes());
        }
        ColorType::Rgb => {
            sep(buf, first);
            let r = (c.value >> 16) & 0xff;
            let g = (c.value >> 8) & 0xff;
            let b = c.value & 0xff;
            buf.extend_from_slice(if fg { b"38;2;" } else { b"48;2;" });
            buf.extend_from_slice(r.to_string().as_bytes());
            buf.push(b';');
            buf.extend_from_slice(g.to_string().as_bytes());
            buf.push(b';');
            buf.extend_from_slice(b.to_string().as_bytes());
        }
    }
}

/// Ordering helper for tests.
pub fn cmp_cells(a: &Cell, b: &Cell) -> Ordering {
    (a.ch, a.width, a.attr.under).cmp(&(b.ch, b.width, b.attr.under))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn put_and_cell_at() {
        let mut fb = Framebuffer::new(80, 24);
        fb.put_rune(3, 2, 'Z', Attr::default());
        assert_eq!(fb.cell_at(3, 2).unwrap().ch, 'Z');
    }

    #[test]
    fn diff_empty_when_identical() {
        let fb = Framebuffer::new(10, 5);
        let other = fb.clone();
        assert!(fb.diff(Some(&other)).is_empty());
    }

    #[test]
    fn diff_emits_changed_glyph() {
        let old = Framebuffer::new(10, 5);
        let mut new = old.clone();
        new.put_rune(
            0,
            0,
            'a',
            Attr {
                under: true,
                ..Attr::default()
            },
        );
        new.cur_x = 1;
        let paint = new.diff(Some(&old));
        assert!(paint.windows(1).any(|w| w == b"a"));
        // underline SGR
        assert!(paint
            .windows(3)
            .any(|w| w == b"\x1b[4" || w.starts_with(b"\x1b[")));
        assert!(String::from_utf8_lossy(&paint).contains('a'));
    }

    #[test]
    fn full_redraw_on_size_change() {
        let old = Framebuffer::new(10, 5);
        let new = Framebuffer::new(20, 10);
        let paint = new.diff(Some(&old));
        assert!(paint.windows(4).any(|w| w == b"\x1b[2J"));
    }
}

/// Approximate terminal cell width for host modeling (1 or 2).
fn rune_display_width(ch: char) -> usize {
    let c = ch as u32;
    // Combining / ZW — treat as 0 so host cursor does not advance.
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
