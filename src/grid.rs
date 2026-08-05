//! A full, authoritative model of the terminal screen.
//!
//! Unlike the original C bicon's "patch the stream as it flies by" approach
//! (the root cause of its overwrite/leftover-character bugs), every byte the
//! child emits goes through a real VT parser (`vte`, same crate Alacritty
//! uses) into a 2-D cell grid -- always a complete, correct snapshot, no
//! partial/implicit state to get out of sync.

use std::mem;
use vte::{Params, Perform};

/// A terminal foreground/background color, as set via SGR (`\x1b[...m`).
/// Covers 16-color, 256-color (`38;5;n`/`48;5;n`), and 24-bit truecolor
/// (`38;2;r;g;b`/`48;2;r;g;b` -- used by Powerlevel10k/oh-my-posh segments).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Color {
    #[default]
    Default,
    Indexed(u8),
    Rgb(u8, u8, u8),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Cell {
    /// The logical (as-typed / as-received) character occupying this cell.
    pub ch: char,
    pub bold: bool,
    pub dim: bool,
    pub italic: bool,
    pub underline: bool,
    pub reverse: bool,
    pub strikethrough: bool,
    pub fg: Color,
    pub bg: Color,
    /// True for the second half of a wide (e.g. double-width) glyph.
    pub wide_continuation: bool,
}

impl Default for Cell {
    fn default() -> Self {
        Cell {
            ch: ' ',
            bold: false,
            dim: false,
            italic: false,
            underline: false,
            reverse: false,
            strikethrough: false,
            fg: Color::Default,
            bg: Color::Default,
            wide_continuation: false,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Cursor {
    pub row: usize,
    pub col: usize,
    pub visible: bool,
}

/// A complete snapshot of the terminal screen, in *logical* (as-received)
/// order. Bidi reordering is applied later, per-row, to produce a
/// *visual* grid — this struct never mixes the two.
pub struct Grid {
    pub cols: usize,
    pub rows: usize,
    cells: Vec<Cell>,
    pub cursor: Cursor,
    /// SGR attribute/color state to apply to the *next* printed character.
    pending_bold: bool,
    pending_dim: bool,
    pending_italic: bool,
    pending_underline: bool,
    pending_reverse: bool,
    pending_strikethrough: bool,
    pending_fg: Color,
    pending_bg: Color,
    /// Last character written by `print()`, for CSI `b` (REP). `None`
    /// until something has actually been printed.
    last_printed: Option<char>,
    /// Lines pushed off the top by `scroll_up()` since the last
    /// `take_scroll_lines()` call, so the render pipeline can replay the
    /// same number of real linefeeds on the actual tty (pushing content
    /// into the real terminal/tmux's native scrollback). `scroll_up` only
    /// simulates scrolling in-memory; it never touches the real terminal.
    scroll_lines: usize,
    /// Cursor stashed by DECSC (`ESC 7`)/CSI `s`, restored by DECRC
    /// (`ESC 8`)/CSI `u`. Multi-line zsh-autosuggestions rely on this to
    /// snap the cursor back to the real typing position after temporarily
    /// showing suggestion text below the input line.
    saved_cursor: Option<Cursor>,
    /// DECAWM (auto-wrap mode). Defaults on, matching real terminals.
    auto_wrap: bool,
    /// DECSTBM (`CSI r`) scroll region, 0-indexed inclusive, default full
    /// screen. Lets zsh/ZLE restrict scrolling to a sub-region (e.g. just
    /// a multi-line autosuggestion) via IND/RI at the region's edge --
    /// tmux never needs this since it redraws panes wholesale.
    top_margin: usize,
    bottom_margin: usize,
    /// Marks rows written while a restricted DECSTBM region was active --
    /// the signal that a row belongs to an inline TUI widget's own layout
    /// (fzf popup, etc.) rather than shell prose, so bidi reordering must
    /// skip it (reordering a box-drawing layout as if it were prose would
    /// shuffle its columns/borders). Cleared once a row is rewritten under
    /// a full-screen (unrestricted) region.
    structured_rows: Vec<bool>,
    /// `true` for row `r` iff it's a DECAWM auto-wrap continuation of row
    /// `r-1` (not a real `\n`/IND/NEL). Tells `bidi::reorder_grid` which
    /// consecutive rows form one logical prose line, so UAX #9 runs over
    /// the whole line instead of scrambling word order at each wrap point.
    /// Row 0 is never a continuation.
    wrapped_rows: Vec<bool>,
}

impl Grid {
    pub fn new(cols: usize, rows: usize) -> Self {
        Grid {
            cols,
            rows,
            cells: vec![Cell::default(); cols * rows],
            cursor: Cursor {
                row: 0,
                col: 0,
                visible: true,
            },
            pending_bold: false,
            pending_dim: false,
            pending_italic: false,
            pending_underline: false,
            pending_reverse: false,
            pending_strikethrough: false,
            pending_fg: Color::Default,
            pending_bg: Color::Default,
            last_printed: None,
            scroll_lines: 0,
            saved_cursor: None,
            auto_wrap: true,
            top_margin: 0,
            bottom_margin: rows.saturating_sub(1),
            structured_rows: vec![false; rows],
            wrapped_rows: vec![false; rows],
        }
    }

    /// Reset all pending SGR attributes/colors to their defaults (SGR 0).
    fn reset_pending_attrs(&mut self) {
        self.pending_bold = false;
        self.pending_dim = false;
        self.pending_italic = false;
        self.pending_underline = false;
        self.pending_reverse = false;
        self.pending_strikethrough = false;
        self.pending_fg = Color::Default;
        self.pending_bg = Color::Default;
    }

    pub fn resize(&mut self, cols: usize, rows: usize) {
        if cols == self.cols && rows == self.rows {
            // No-op: unchanged dimensions must not reset DECSTBM margins /
            // wrapped_rows / structured_rows (spurious SIGWINCH shouldn't
            // wipe an in-progress restricted scroll region).
            return;
        }
        let mut new_cells = vec![Cell::default(); cols * rows];
        for r in 0..self.rows.min(rows) {
            for c in 0..self.cols.min(cols) {
                new_cells[r * cols + c] = self.cells[r * self.cols + c];
            }
        }
        self.cells = new_cells;
        self.cols = cols;
        self.rows = rows;
        self.cursor.row = self.cursor.row.min(rows.saturating_sub(1));
        self.cursor.col = self.cursor.col.min(cols.saturating_sub(1));
        // A resize invalidates any scroll region a previous DECSTBM set up
        // for the old screen size; real terminals reset it to full-screen
        // on resize too.
        self.top_margin = 0;
        self.bottom_margin = rows.saturating_sub(1);
        self.structured_rows = vec![false; rows];
        // No reflow on resize (content just gets clipped/padded), so any
        // previously recorded wrap relationship is no longer trustworthy --
        // forget it all; the next real auto-wrap re-marks rows correctly.
        self.wrapped_rows = vec![false; rows];
    }

    pub fn row(&self, r: usize) -> &[Cell] {
        &self.cells[r * self.cols..(r + 1) * self.cols]
    }

    /// Writes a single cell directly, bypassing cursor logic. Used by the
    /// bidi module to build a visual-order grid from a logical one.
    pub fn set_cell(&mut self, r: usize, c: usize, cell: Cell) {
        if r < self.rows && c < self.cols {
            self.cells[r * self.cols + c] = cell;
        }
    }

    fn row_mut(&mut self, r: usize) -> &mut [Cell] {
        &mut self.cells[r * self.cols..(r + 1) * self.cols]
    }

    fn put_char(&mut self, ch: char) {
        if self.cols == 0 || self.rows == 0 {
            return;
        }
        if self.cursor.col >= self.cols {
            if self.auto_wrap {
                self.newline(true);
            } else {
                self.cursor.col = self.cols - 1;
            }
        }
        let (row, col) = (self.cursor.row, self.cursor.col);
        self.row_mut(row)[col] = Cell {
            ch,
            bold: self.pending_bold,
            dim: self.pending_dim,
            italic: self.pending_italic,
            underline: self.pending_underline,
            reverse: self.pending_reverse,
            strikethrough: self.pending_strikethrough,
            fg: self.pending_fg,
            bg: self.pending_bg,
            wide_continuation: false,
        };
        self.cursor.col += 1;
        self.last_printed = Some(ch);
        if self.top_margin != 0 || self.bottom_margin != self.rows.saturating_sub(1) {
            self.structured_rows[row] = true;
        }
    }

    /// Moves to the start of the next line (CR + IND). `wrapped` records
    /// whether this is a DECAWM auto-wrap continuation (`true`, from
    /// `put_char`) or a genuine new logical line (`false`, a real
    /// `\n`/`\r\n`), stored in `wrapped_rows` for the resulting row.
    fn newline(&mut self, wrapped: bool) {
        self.cursor.col = 0;
        self.index();
        let row = self.cursor.row;
        if row < self.wrapped_rows.len() {
            self.wrapped_rows[row] = wrapped;
        }
    }

    /// IND (`ESC D`): move the cursor down one line, honoring the DECSTBM
    /// scroll region -- if the cursor is on the region's bottom margin,
    /// scroll just that region up instead of falling off the bottom of
    /// the whole screen. Plain `LF`/`newline()` is CR + this.
    fn index(&mut self) {
        if self.cursor.row == self.bottom_margin {
            self.scroll_region_up(self.top_margin, self.bottom_margin, 1);
        } else if self.cursor.row + 1 < self.rows {
            self.cursor.row += 1;
        }
    }

    /// Reverse Index (`ESC M`): move the cursor up one line; if it's on
    /// the scroll region's top margin, scroll that region down instead
    /// (inserting a blank line) rather than just clamping at row 0. This,
    /// combined with `index()`'s bottom-margin handling, is what real zsh
    /// (ZLE/zsh-autosuggestions) relies on for confining a redraw to a
    /// restricted scroll region -- something tmux never exercises since it
    /// redraws its pane wholesale with absolute cursor addressing.
    fn reverse_index(&mut self) {
        if self.cursor.row == self.top_margin {
            self.scroll_region_down(self.top_margin, self.bottom_margin, 1);
        } else if self.cursor.row > 0 {
            self.cursor.row -= 1;
        }
    }

    /// DECSTBM (`CSI top;bottom r`): restrict scrolling to `top..=bottom`
    /// (1-indexed on the wire, converted to 0-indexed inclusive here).
    /// An invalid or degenerate range resets to the full screen, matching
    /// real terminal behavior. Per spec, the cursor also homes to (0,0).
    fn set_scroll_region(&mut self, top: usize, bottom: Option<usize>) {
        let top0 = top.saturating_sub(1);
        let bottom0 = bottom
            .unwrap_or(self.rows)
            .saturating_sub(1)
            .min(self.rows.saturating_sub(1));
        if top0 < bottom0 {
            self.top_margin = top0;
            self.bottom_margin = bottom0;
        } else {
            self.top_margin = 0;
            self.bottom_margin = self.rows.saturating_sub(1);
        }
        // Full-screen region: whatever inline widget was using a
        // restricted region is done -- forget structured_rows so prose
        // there gets bidi-reordered again.
        if self.top_margin == 0 && self.bottom_margin == self.rows.saturating_sub(1) {
            self.structured_rows.iter_mut().for_each(|s| *s = false);
        }
        self.cursor.row = 0;
        self.cursor.col = 0;
    }

    /// Whether row `r` was written while a restricted DECSTBM region was
    /// active; the bidi pass uses this to skip inline-widget layout rows.
    pub fn is_row_structured(&self, r: usize) -> bool {
        self.structured_rows.get(r).copied().unwrap_or(false)
    }

    /// Whether row `r` is an auto-wrap continuation of row `r-1`; the bidi
    /// pass uses this to group rows into one logical line before reordering.
    pub fn is_row_wrapped(&self, r: usize) -> bool {
        self.wrapped_rows.get(r).copied().unwrap_or(false)
    }

    /// Clears the wrap-continuation flag on the cursor's row. Called after
    /// an explicit (non-auto-wrap) cursor-down move, so a stale flag from
    /// the row's previous content doesn't wrongly glue it to the row above.
    fn clear_wrapped_flag_at_cursor(&mut self) {
        let row = self.cursor.row;
        if let Some(flag) = self.wrapped_rows.get_mut(row) {
            *flag = false;
        }
    }

    /// Shifts rows `top..=bottom` up by `n`, blanking the `n` rows exposed
    /// at the bottom; rows outside the range are untouched. `scroll_lines`
    /// only increments for a full-screen scroll -- a DECSTBM-confined
    /// scroll doesn't push content into the terminal's real scrollback.
    fn scroll_region_up(&mut self, top: usize, bottom: usize, n: usize) {
        if bottom < top || bottom >= self.rows {
            return;
        }
        let region_rows = bottom - top + 1;
        let n = n.min(region_rows);
        if n == 0 {
            return;
        }
        let start = top * self.cols;
        let end = (bottom + 1) * self.cols;
        self.cells.copy_within(start + n * self.cols..end, start);
        for c in &mut self.cells[end - n * self.cols..end] {
            *c = Cell::default();
        }
        self.structured_rows.copy_within(top + n..=bottom, top);
        for flag in &mut self.structured_rows[bottom + 1 - n..=bottom] {
            *flag = false;
        }
        // Wrap-continuation flags travel with the content, like
        // structured_rows above. Newly exposed blank rows have no wrap
        // relationship.
        self.wrapped_rows.copy_within(top + n..=bottom, top);
        for flag in &mut self.wrapped_rows[bottom + 1 - n..=bottom] {
            *flag = false;
        }
        if top == 0 && bottom == self.rows - 1 {
            self.scroll_lines += n;
        }
        // A cursor saved before this scroll no longer points at the same
        // content if it falls inside the scrolled region.
        if let Some(saved) = &mut self.saved_cursor {
            if saved.row >= top && saved.row <= bottom {
                match saved.row.checked_sub(n) {
                    Some(row) if row >= top => saved.row = row,
                    _ => self.saved_cursor = None,
                }
            }
        }
    }

    /// Mirror of `scroll_region_up`: shifts rows `top..=bottom` down by
    /// `n`, blanking the `n` rows that land at the top of that range.
    fn scroll_region_down(&mut self, top: usize, bottom: usize, n: usize) {
        if bottom < top || bottom >= self.rows {
            return;
        }
        let region_rows = bottom - top + 1;
        let n = n.min(region_rows);
        if n == 0 {
            return;
        }
        let start = top * self.cols;
        let end = (bottom + 1) * self.cols;
        self.cells
            .copy_within(start..end - n * self.cols, start + n * self.cols);
        for c in &mut self.cells[start..start + n * self.cols] {
            *c = Cell::default();
        }
        self.structured_rows.copy_within(top..=bottom - n, top + n);
        for flag in &mut self.structured_rows[top..top + n] {
            *flag = false;
        }
        self.wrapped_rows.copy_within(top..=bottom - n, top + n);
        for flag in &mut self.wrapped_rows[top..top + n] {
            *flag = false;
        }
        if let Some(saved) = &mut self.saved_cursor {
            if saved.row >= top && saved.row <= bottom {
                let new_row = saved.row + n;
                if new_row <= bottom {
                    saved.row = new_row;
                } else {
                    self.saved_cursor = None;
                }
            }
        }
    }

    /// Returns lines scrolled off the top since the last call, resetting
    /// the counter to zero.
    pub fn take_scroll_lines(&mut self) -> usize {
        mem::replace(&mut self.scroll_lines, 0)
    }

    fn erase_line(&mut self) {
        let row = self.cursor.row;
        for c in self.row_mut(row) {
            *c = Cell::default();
        }
        // The row is now blank, so any wrap relationship it recorded no
        // longer describes real content.
        if let Some(flag) = self.wrapped_rows.get_mut(row) {
            *flag = false;
        }
    }

    fn erase_line_from_cursor(&mut self) {
        let (row, col) = (self.cursor.row, self.cursor.col);
        for c in &mut self.row_mut(row)[col..] {
            *c = Cell::default();
        }
    }

    /// DCH: deletes `n` characters starting at the cursor, shifting
    /// everything after them on the same row left by `n`, and filling
    /// the newly-vacated cells at the end of the row with blanks.
    fn delete_chars(&mut self, n: usize) {
        let (row, col) = (self.cursor.row, self.cursor.col);
        let cols = self.cols;
        let n = n.min(cols.saturating_sub(col));
        let line = self.row_mut(row);
        line.copy_within(col + n..cols, col);
        for c in &mut line[cols - n..] {
            *c = Cell::default();
        }
    }

    /// ICH: inserts `n` blank characters at the cursor, shifting existing
    /// content on the same row right by `n`; content pushed past the
    /// last column is discarded.
    fn insert_chars(&mut self, n: usize) {
        let (row, col) = (self.cursor.row, self.cursor.col);
        let cols = self.cols;
        let n = n.min(cols.saturating_sub(col));
        let line = self.row_mut(row);
        line.copy_within(col..cols - n, col + n);
        for c in &mut line[col..col + n] {
            *c = Cell::default();
        }
    }

    fn erase_line_to_cursor(&mut self) {
        let (row, col) = (self.cursor.row, self.cursor.col);
        let end = col.min(self.cols - 1);
        for c in &mut self.row_mut(row)[..=end] {
            *c = Cell::default();
        }
    }

    /// ED (`\x1b[0J` / bare `\x1b[J`): erase from the cursor to the end of
    /// the screen -- the current row from the cursor onward, plus every
    /// row below it.
    fn erase_screen_from_cursor(&mut self) {
        self.erase_line_from_cursor();
        let row = self.cursor.row;
        for c in &mut self.cells[(row + 1) * self.cols..] {
            *c = Cell::default();
        }
        // Every row strictly below the cursor is now fully blank.
        for flag in &mut self.wrapped_rows[row + 1..] {
            *flag = false;
        }
    }

    /// ED (`\x1b[1J`): erase from the start of the screen to the cursor --
    /// every row above it, plus the current row up to and including the
    /// cursor.
    fn erase_screen_to_cursor(&mut self) {
        let row = self.cursor.row;
        for c in &mut self.cells[..row * self.cols] {
            *c = Cell::default();
        }
        // Every row strictly above the cursor is now fully blank.
        for flag in &mut self.wrapped_rows[..row] {
            *flag = false;
        }
        self.erase_line_to_cursor();
    }

    fn erase_screen(&mut self) {
        // Push whatever's currently meaningful into real scrollback before
        // wiping it, via the scroll_lines -> real-newline-replay machinery
        // in render.rs. Only counting up to the last non-blank row (not
        // full screen height) avoids flooding scrollback with empty lines.
        if let Some(last_meaningful) = (0..self.rows)
            .rev()
            .find(|&r| self.row(r).iter().any(|c| !c.ch.is_whitespace()))
        {
            self.scroll_lines += last_meaningful + 1;
        }
        self.cells = vec![Cell::default(); self.cols * self.rows];
        self.structured_rows.iter_mut().for_each(|s| *s = false);
        self.wrapped_rows.iter_mut().for_each(|s| *s = false);
    }
}

/// Feeds `vte::Parser` output into a `Grid`. A pragmatic subset of
/// ECMA-48/xterm: enough control sequences to run a real shell correctly,
/// expanded incrementally as gaps are found by the test suite.
pub struct GridPerformer<'a> {
    pub grid: &'a mut Grid,
    /// Bytes to write back to the child (not our own stdout) in reply to a
    /// terminal query, e.g. DSR (`\x1b[6n`). A plain buffer so `GridPerformer`
    /// stays agnostic of how a reply is actually delivered -- the caller of
    /// `feed()` decides that.
    pub responses: Vec<u8>,
}

impl<'a> Perform for GridPerformer<'a> {
    fn print(&mut self, c: char) {
        self.grid.put_char(c);
    }

    fn execute(&mut self, byte: u8) {
        match byte {
            b'\n' => self.grid.newline(false),
            b'\r' => self.grid.cursor.col = 0,
            // Backspace: move left only, do NOT erase -- erasure is a
            // separate, explicit step.
            0x08 if self.grid.cursor.col > 0 => {
                self.grid.cursor.col -= 1;
            }
            _ => {}
        }
    }

    fn esc_dispatch(&mut self, _intermediates: &[u8], _ignore: bool, byte: u8) {
        match byte {
            // RIS - full terminal reset. Some `clear`/`reset` wrappers emit
            // this instead of (or alongside) CSI 2J.
            b'c' => {
                self.grid.erase_screen();
                self.grid.cursor.row = 0;
                self.grid.cursor.col = 0;
                self.grid.reset_pending_attrs();
                self.grid.top_margin = 0;
                self.grid.bottom_margin = self.grid.rows.saturating_sub(1);
                self.grid.auto_wrap = true;
            }
            // DECSC / DECRC (save/restore cursor). Used by
            // zsh-autosuggestions and similar line editors to park the
            // cursor, draw extra text elsewhere, then snap back.
            b'7' => self.grid.saved_cursor = Some(self.grid.cursor),
            b'8' => {
                if let Some(saved) = self.grid.saved_cursor {
                    self.grid.cursor = saved;
                }
            }
            // RI - Reverse Index (see `Grid::reverse_index`).
            b'M' => self.grid.reverse_index(),
            // IND - Index: cursor down one line honoring the DECSTBM
            // region (unlike LF, no carriage return). Explicit cursor
            // control, not an auto-wrap continuation.
            b'D' => {
                self.grid.index();
                self.grid.clear_wrapped_flag_at_cursor();
            }
            // NEL - Next Line: carriage return + IND.
            b'E' => {
                self.grid.cursor.col = 0;
                self.grid.index();
                self.grid.clear_wrapped_flag_at_cursor();
            }
            _ => {}
        }
    }

    fn csi_dispatch(&mut self, params: &Params, intermediates: &[u8], _ignore: bool, action: char) {
        // `vte` represents an absent numeric parameter as a literal `0`
        // (e.g. bare `ESC[A` yields params == [[0]]), not an empty list.
        // Per ECMA-48, `0` and "absent" are equivalent for cursor-movement
        // params (A/B/C/D, H/f, r, S/T) -- both mean "use the default".
        let p = |i: usize, default: usize| -> usize {
            match params
                .iter()
                .nth(i)
                .and_then(|p| p.first().copied())
                .map(|v| v as usize)
            {
                None | Some(0) => default,
                Some(v) => v,
            }
        };
        match action {
            'A' => self.grid.cursor.row = self.grid.cursor.row.saturating_sub(p(0, 1)),
            'B' => self.grid.cursor.row = (self.grid.cursor.row + p(0, 1)).min(self.grid.rows - 1),
            'C' => self.grid.cursor.col = (self.grid.cursor.col + p(0, 1)).min(self.grid.cols - 1),
            'D' => self.grid.cursor.col = self.grid.cursor.col.saturating_sub(p(0, 1)),
            'H' | 'f' => {
                self.grid.cursor.row = p(0, 1).saturating_sub(1).min(self.grid.rows - 1);
                self.grid.cursor.col = p(1, 1).saturating_sub(1).min(self.grid.cols - 1);
            }
            // CHA / HPA - Cursor Horizontal Absolute: jump to column n on
            // the current row, row unchanged. fzf relies on this to jump
            // between its list/preview panes on every redraw.
            'G' | '`' => {
                self.grid.cursor.col = p(0, 1).saturating_sub(1).min(self.grid.cols - 1);
            }
            // VPA - Vertical Position Absolute: same as CHA/HPA but for the row.
            'd' => {
                self.grid.cursor.row = p(0, 1).saturating_sub(1).min(self.grid.rows - 1);
            }
            's' => self.grid.saved_cursor = Some(self.grid.cursor),
            'u' => {
                if let Some(saved) = self.grid.saved_cursor {
                    self.grid.cursor = saved;
                }
            }
            // DECSTBM - set scroll region (see `Grid::set_scroll_region`).
            'r' => {
                let top = p(0, 1);
                let bottom = match params
                    .iter()
                    .nth(1)
                    .and_then(|g| g.first().copied())
                    .map(|v| v as usize)
                {
                    None | Some(0) => None,
                    Some(v) => Some(v),
                };
                self.grid.set_scroll_region(top, bottom);
            }
            // SU / SD - scroll the (possibly restricted) region up/down
            // by n lines directly, independent of cursor position.
            'S' => {
                let n = p(0, 1);
                self.grid
                    .scroll_region_up(self.grid.top_margin, self.grid.bottom_margin, n);
            }
            'T' => {
                let n = p(0, 1);
                self.grid
                    .scroll_region_down(self.grid.top_margin, self.grid.bottom_margin, n);
            }
            'J' => match p(0, 0) {
                2 | 3 => self.grid.erase_screen(),
                0 => self.grid.erase_screen_from_cursor(),
                1 => self.grid.erase_screen_to_cursor(),
                _ => {}
            },
            'K' => match p(0, 0) {
                0 => self.grid.erase_line_from_cursor(),
                1 => self.grid.erase_line_to_cursor(),
                2 => self.grid.erase_line(),
                _ => {}
            },
            // DCH (Delete Character): removes Ps characters at the cursor,
            // shifting the rest of the line left. Used by zsh's vi-mode
            // for in-place word edits (`dw`, `cw`).
            'P' => self.grid.delete_chars(p(0, 1)),
            // ICH (Insert Character): mirror of DCH -- inserts Ps blanks at
            // the cursor, shifting content right; overflow is discarded.
            '@' => self.grid.insert_chars(p(0, 1)),
            'm' => {
                // Params are grouped by semicolon; colon-separated
                // sub-params (rarer truecolor emitters) share a group.
                // Compound sequences (`38;5;196`, `38;2;255;0;0`) use
                // semicolons in virtually every shell/prompt, so flatten
                // every group into one stream and consume extra operands
                // by hand.
                let flat: Vec<u16> = params.iter().flat_map(|g| g.iter().copied()).collect();
                if flat.is_empty() {
                    self.grid.reset_pending_attrs();
                }
                let mut i = 0;
                while i < flat.len() {
                    match flat[i] {
                        0 => self.grid.reset_pending_attrs(),
                        1 => self.grid.pending_bold = true,
                        2 => self.grid.pending_dim = true,
                        3 => self.grid.pending_italic = true,
                        4 => self.grid.pending_underline = true,
                        7 => self.grid.pending_reverse = true,
                        9 => self.grid.pending_strikethrough = true,
                        22 => {
                            self.grid.pending_bold = false;
                            self.grid.pending_dim = false;
                        }
                        23 => self.grid.pending_italic = false,
                        24 => self.grid.pending_underline = false,
                        27 => self.grid.pending_reverse = false,
                        29 => self.grid.pending_strikethrough = false,
                        v @ 30..=37 => self.grid.pending_fg = Color::Indexed((v - 30) as u8),
                        38 => match flat.get(i + 1) {
                            Some(5) => {
                                if let Some(&n) = flat.get(i + 2) {
                                    self.grid.pending_fg = Color::Indexed(n as u8);
                                }
                                i += 2;
                            }
                            Some(2) => {
                                if let (Some(&r), Some(&g), Some(&b)) =
                                    (flat.get(i + 2), flat.get(i + 3), flat.get(i + 4))
                                {
                                    self.grid.pending_fg = Color::Rgb(r as u8, g as u8, b as u8);
                                }
                                i += 4;
                            }
                            _ => {}
                        },
                        39 => self.grid.pending_fg = Color::Default,
                        v @ 40..=47 => self.grid.pending_bg = Color::Indexed((v - 40) as u8),
                        48 => match flat.get(i + 1) {
                            Some(5) => {
                                if let Some(&n) = flat.get(i + 2) {
                                    self.grid.pending_bg = Color::Indexed(n as u8);
                                }
                                i += 2;
                            }
                            Some(2) => {
                                if let (Some(&r), Some(&g), Some(&b)) =
                                    (flat.get(i + 2), flat.get(i + 3), flat.get(i + 4))
                                {
                                    self.grid.pending_bg = Color::Rgb(r as u8, g as u8, b as u8);
                                }
                                i += 4;
                            }
                            _ => {}
                        },
                        49 => self.grid.pending_bg = Color::Default,
                        v @ 90..=97 => self.grid.pending_fg = Color::Indexed((v - 90 + 8) as u8),
                        v @ 100..=107 => self.grid.pending_bg = Color::Indexed((v - 100 + 8) as u8),
                        _ => {}
                    }
                    i += 1;
                }
            }
            // REP - repeat the preceding graphic character `p(0, 1)` times.
            // zsh's ZLE uses this to redraw runs of repeated characters efficiently.
            'b' => {
                if let Some(ch) = self.grid.last_printed {
                    for _ in 0..p(0, 1) {
                        self.grid.put_char(ch);
                    }
                }
            }
            // DSR (`\x1b[6n`) - Device Status Report, cursor position
            // DSR (`\x1b[6n`) - Device Status Report, cursor position
            // variant. The child shell/line editor sends this and blocks
            // reading its own stdin until it sees a matching
            // `\x1b[{row};{col}R` reply.
            'n' => {
                if p(0, 0) == 6 {
                    self.responses.extend_from_slice(
                        format!(
                            "\x1b[{};{}R",
                            self.grid.cursor.row + 1,
                            self.grid.cursor.col + 1
                        )
                        .as_bytes(),
                    );
                }
            }
            // DECAWM (auto-wrap mode), `\x1b[?7h` / `\x1b[?7l`. fzf and
            // similar TUIs disable this to print a full-width line and
            // leave the cursor pinned at the last column instead of
            // auto-advancing, since their relative cursor moves assume no
            // wrap happened.
            'h' if intermediates.contains(&b'?') && p(0, 0) == 7 => {
                self.grid.auto_wrap = true;
            }
            'l' if intermediates.contains(&b'?') && p(0, 0) == 7 => {
                self.grid.auto_wrap = false;
            }
            _ => {}
        }
    }
}

/// Drives a `vte::Parser` over raw child-process output, mutating `grid`.
/// Returns any bytes that must be written back to the *child* in reply to
/// a terminal query it made (currently just DSR cursor-position replies);
/// callers that don't care can ignore the return value, but interactive
/// callers (the CLI's PTY loop) must forward it to the child's stdin, not
/// to our own stdout.
pub fn feed(parser: &mut vte::Parser, grid: &mut Grid, bytes: &[u8]) -> Vec<u8> {
    let mut performer = GridPerformer {
        grid,
        responses: Vec::new(),
    };
    for &b in bytes {
        parser.advance(&mut performer, b);
    }
    performer.responses
}

#[cfg(test)]
mod tests {
    use super::*;

    fn feed_str(grid: &mut Grid, s: &str) {
        let mut parser = vte::Parser::new();
        feed(&mut parser, grid, s.as_bytes());
    }

    #[test]
    fn decawm_off_pins_cursor_at_last_column_instead_of_wrapping() {
        // Printing past the last column with DECAWM off must overwrite
        // that column repeatedly, not advance to a new row.
        let mut grid = Grid::new(5, 3);
        feed_str(&mut grid, "\x1b[?7l"); // disable auto-wrap
        feed_str(&mut grid, "abcdeXYZ"); // 8 chars into a 5-col row
        assert_eq!(grid.cursor.row, 0, "must stay on row 0, not wrap to row 1");
        assert_eq!(
            grid.row(0).iter().map(|c| c.ch).collect::<String>(),
            "abcdZ"
        );
    }

    #[test]
    fn decawm_on_by_default_still_wraps_normally() {
        let mut grid = Grid::new(5, 3);
        feed_str(&mut grid, "abcdeXYZ");
        assert_eq!(grid.cursor.row, 1, "default (auto-wrap on) must still wrap");
    }

    #[test]
    fn dch_deletes_and_shifts_line_left() {
        // zsh vi-mode does exactly this to remove a character in place:
        // move cursor to it, then CSI P.
        let mut grid = Grid::new(10, 2);
        feed_str(&mut grid, "abcdefgh");
        feed_str(&mut grid, "\x1b[1;3H"); // cursor to 'c' (row 1, col 3, 1-indexed)
        feed_str(&mut grid, "\x1b[2P"); // delete 'c' and 'd'
        assert_eq!(
            grid.row(0).iter().map(|c| c.ch).collect::<String>(),
            "abefgh    "
        );
    }

    #[test]
    fn dch_default_count_is_one() {
        let mut grid = Grid::new(10, 2);
        feed_str(&mut grid, "abcdef");
        feed_str(&mut grid, "\x1b[1;2H\x1b[P"); // bare CSI P == delete 1
        assert_eq!(
            grid.row(0).iter().map(|c| c.ch).collect::<String>(),
            "acdef     "
        );
    }

    #[test]
    fn ich_inserts_blanks_and_shifts_line_right() {
        let mut grid = Grid::new(10, 2);
        feed_str(&mut grid, "abcdef");
        feed_str(&mut grid, "\x1b[1;2H\x1b[2@"); // insert 2 blanks after 'a'
        assert_eq!(
            grid.row(0).iter().map(|c| c.ch).collect::<String>(),
            "a  bcdef  "
        );
    }

    #[test]
    fn ich_discards_content_pushed_past_last_column() {
        let mut grid = Grid::new(5, 2);
        feed_str(&mut grid, "abcde");
        feed_str(&mut grid, "\x1b[1;1H\x1b[2@");
        assert_eq!(
            grid.row(0).iter().map(|c| c.ch).collect::<String>(),
            "  abc"
        );
    }

    #[test]
    fn decawm_re_enabled_after_off_resumes_wrapping() {
        let mut grid = Grid::new(5, 3);
        feed_str(&mut grid, "\x1b[?7l");
        feed_str(&mut grid, "abcde"); // fills row 0 exactly, no overflow yet
        feed_str(&mut grid, "\x1b[?7h"); // re-enable
        feed_str(&mut grid, "X");
        assert_eq!(
            grid.cursor.row, 1,
            "wrap must resume once DECAWM is back on"
        );
    }

    #[test]
    fn ris_resets_decawm_to_default_on() {
        let mut grid = Grid::new(5, 3);
        feed_str(&mut grid, "\x1b[?7l");
        feed_str(&mut grid, "\x1bc"); // RIS
        feed_str(&mut grid, "abcdeXYZ");
        assert_eq!(grid.cursor.row, 1, "RIS must restore auto-wrap");
    }

    #[test]
    fn clear_pushes_only_meaningful_rows_to_scrollback_not_the_whole_screen() {
        // `clear` on a short command's output must push only the
        // meaningful rows into real scrollback, not the whole (mostly
        // blank) screen.
        let mut grid = Grid::new(20, 50); // tall "screen", short output
        feed_str(&mut grid, "line one\r\nline two\r\nline three\r\n");
        feed_str(&mut grid, "\x1b[2J"); // clear
        assert_eq!(
            grid.take_scroll_lines(),
            3,
            "must count only up to the last non-blank row, not all 50"
        );
    }

    #[test]
    fn clear_ignores_colored_but_visually_blank_cells() {
        // A colored blank cell (e.g. a selection-bar background over empty
        // space) isn't `Cell::default()`, but it's not visually meaningful
        // either -- it must not count as content when deciding scrollback push.
        let mut grid = Grid::new(20, 10);
        feed_str(&mut grid, "\x1b[48;5;236m                    \x1b[0m"); // a colored blank row, no text at all
        feed_str(&mut grid, "\x1b[2J");
        assert_eq!(
            grid.take_scroll_lines(),
            0,
            "a row with only colored whitespace must not count as meaningful content"
        );
    }

    #[test]
    fn clear_on_an_already_blank_screen_pushes_nothing() {
        let mut grid = Grid::new(20, 10);
        feed_str(&mut grid, "\x1b[2J");
        assert_eq!(grid.take_scroll_lines(), 0);
    }

    #[test]
    fn ed_0_clears_current_row_and_everything_below() {
        let mut grid = Grid::new(5, 3);
        feed_str(&mut grid, "AAAAA");
        feed_str(&mut grid, "\r\nBBBBB");
        feed_str(&mut grid, "\r\nCCCCC");
        // Cursor now at end of row 2 (0-indexed), col 5. Move to row 1 col 2
        // and erase from there to end of screen.
        feed_str(&mut grid, "\x1b[2;3H\x1b[0J");
        assert_eq!(
            grid.row(0).iter().map(|c| c.ch).collect::<String>(),
            "AAAAA"
        );
        assert_eq!(
            grid.row(1).iter().map(|c| c.ch).collect::<String>(),
            "BB   "
        );
        assert_eq!(
            grid.row(2).iter().map(|c| c.ch).collect::<String>(),
            "     "
        );
    }

    #[test]
    fn saved_cursor_stays_correct_across_an_intervening_scroll() {
        // DECRC after a scroll must land at the shifted row matching where
        // that logical position now sits, not the stale pre-scroll row.
        let mut grid = Grid::new(20, 3);
        feed_str(&mut grid, "a\r\n"); // row0 done, cursor at row1 col0
        feed_str(&mut grid, "\x1b7"); // save (row1, col0)
        feed_str(&mut grid, "b\r\nc\r\n"); // forces exactly one scroll
        feed_str(&mut grid, "\x1b8"); // restore
        assert_eq!(grid.cursor.row, 0);
    }

    #[test]
    fn saved_cursor_is_dropped_if_it_scrolls_off_entirely() {
        let mut grid = Grid::new(20, 2);
        feed_str(&mut grid, "a\r\n"); // cursor at row1
        feed_str(&mut grid, "\x1b7"); // save row1
        feed_str(&mut grid, "b\r\nc\r\nd\r\n"); // scrolls row1's content off
        let before = (grid.cursor.row, grid.cursor.col);
        feed_str(&mut grid, "\x1b8"); // nothing sensible to restore to
        assert_eq!((grid.cursor.row, grid.cursor.col), before);
    }

    #[test]
    fn ris_full_reset_clears_screen() {
        let mut grid = Grid::new(5, 2);
        feed_str(&mut grid, "\x1b[31mhello");
        feed_str(&mut grid, "\x1bc"); // ESC c = RIS
        assert_eq!(
            grid.row(0).iter().map(|c| c.ch).collect::<String>(),
            "     "
        );
        assert_eq!(grid.cursor.row, 0);
        assert_eq!(grid.cursor.col, 0);
    }

    #[test]
    fn decsc_decrc_save_and_restore_cursor() {
        // Simulates zsh-autosuggestions: save cursor, wander off to draw
        // suggestion text, then restore to the saved typing position.
        let mut grid = Grid::new(20, 5);
        feed_str(&mut grid, "abc"); // cursor at row0, col3
        feed_str(&mut grid, "\x1b7"); // DECSC: save (0,3)
        feed_str(&mut grid, "\r\nmore suggestion text"); // wander off
        assert_ne!((grid.cursor.row, grid.cursor.col), (0, 3));
        feed_str(&mut grid, "\x1b8"); // DECRC: restore
        assert_eq!((grid.cursor.row, grid.cursor.col), (0, 3));
    }

    #[test]
    fn csi_s_u_save_and_restore_cursor() {
        let mut grid = Grid::new(20, 5);
        feed_str(&mut grid, "abc");
        feed_str(&mut grid, "\x1b[s"); // CSI s: save
        feed_str(&mut grid, "\r\nxyz");
        assert_ne!((grid.cursor.row, grid.cursor.col), (0, 3));
        feed_str(&mut grid, "\x1b[u"); // CSI u: restore
        assert_eq!((grid.cursor.row, grid.cursor.col), (0, 3));
    }

    #[test]
    fn restore_with_no_prior_save_is_a_no_op() {
        let mut grid = Grid::new(20, 5);
        feed_str(&mut grid, "abc");
        let before = (grid.cursor.row, grid.cursor.col);
        feed_str(&mut grid, "\x1b8");
        assert_eq!((grid.cursor.row, grid.cursor.col), before);
    }

    #[test]
    fn truecolor_and_256_color_survive_sgr() {
        let mut grid = Grid::new(20, 3);
        // Powerlevel10k-style truecolor bg + fg segment, e.g. a directory
        // segment: bold white text on a blue truecolor background.
        feed_str(
            &mut grid,
            "\x1b[1;38;2;255;255;255;48;2;30;60;200m~/code\x1b[0m",
        );
        let cell = grid.row(0)[0];
        assert!(cell.bold);
        assert_eq!(cell.fg, Color::Rgb(255, 255, 255));
        assert_eq!(cell.bg, Color::Rgb(30, 60, 200));

        // 256-color palette form, e.g. `ls --color` or oh-my-posh.
        let mut grid2 = Grid::new(20, 3);
        feed_str(&mut grid2, "\x1b[38;5;196mred-ish\x1b[0m");
        assert_eq!(grid2.row(0)[0].fg, Color::Indexed(196));

        // Plain reset must clear color, not just bold/reverse.
        let mut grid3 = Grid::new(20, 3);
        feed_str(&mut grid3, "\x1b[31mred\x1b[0mplain");
        assert_eq!(grid3.row(0)[0].fg, Color::Indexed(1));
        assert_eq!(grid3.row(0)[3].fg, Color::Default);
    }

    #[test]
    fn dsr_replies_with_cursor_position() {
        let mut grid = Grid::new(10, 5);
        let mut parser = vte::Parser::new();
        // Move to row 2, col 4 (1-indexed on the wire), then ask for it.
        let responses = feed(&mut parser, &mut grid, b"\x1b[2;4H\x1b[6n");
        assert_eq!(responses, b"\x1b[2;4R");
    }

    #[test]
    fn dsr_with_no_prior_query_produces_no_response() {
        let mut grid = Grid::new(10, 5);
        let mut parser = vte::Parser::new();
        let responses = feed(&mut parser, &mut grid, b"hello");
        assert!(responses.is_empty());
    }

    #[test]
    fn rep_repeats_last_printed_character() {
        let mut grid = Grid::new(10, 3);
        let mut parser = vte::Parser::new();
        // Print 'x', then CSI 4b => repeat it 4 more times => "xxxxx".
        feed(&mut parser, &mut grid, b"x\x1b[4b");
        assert_eq!(
            grid.row(0).iter().map(|c| c.ch).collect::<String>(),
            "xxxxx     "
        );
    }

    #[test]
    fn rep_with_nothing_printed_yet_is_a_no_op() {
        let mut grid = Grid::new(10, 3);
        let mut parser = vte::Parser::new();
        feed(&mut parser, &mut grid, b"\x1b[3b");
        assert_eq!(grid.cursor.col, 0);
    }

    #[test]
    fn scroll_up_is_tracked_and_drained_by_take_scroll_lines() {
        let mut grid = Grid::new(5, 2);
        let mut parser = vte::Parser::new();
        assert_eq!(grid.take_scroll_lines(), 0);
        // Three newlines on a 2-row grid: row 0->1 (no scroll), then two
        // more that each push one line off the top.
        feed(&mut parser, &mut grid, b"a\r\nb\r\nc\r\nd");
        assert_eq!(grid.take_scroll_lines(), 2);
        // Counter resets after being read.
        assert_eq!(grid.take_scroll_lines(), 0);
    }

    #[test]
    fn auto_wrap_marks_continuation_row_but_real_newline_does_not() {
        let mut grid = Grid::new(5, 4);
        let mut parser = vte::Parser::new();
        // "abcdefghij" on a 5-col grid auto-wraps: row0="abcde",
        // row1="fghij" -- row1 must be flagged as a continuation.
        feed(&mut parser, &mut grid, b"abcdefghij");
        assert!(!grid.is_row_wrapped(0), "first row is never a continuation");
        assert!(
            grid.is_row_wrapped(1),
            "row filled purely by auto-wrap must be flagged"
        );
        // A real CRLF starts a fresh logical line -- must NOT be flagged.
        feed_str(&mut grid, "\r\nklmno");
        assert!(
            !grid.is_row_wrapped(2),
            "a real \\r\\n starts a new logical line, not a continuation"
        );
    }

    #[test]
    fn erasing_a_wrapped_row_clears_its_flag() {
        let mut grid = Grid::new(5, 4);
        feed_str(&mut grid, "abcdefghij"); // row1 wrapped from row0
        assert!(grid.is_row_wrapped(1));
        grid.cursor.row = 1;
        grid.cursor.col = 0;
        feed_str(&mut grid, "\x1b[2K"); // EL 2: erase entire current line
        assert!(
            !grid.is_row_wrapped(1),
            "a fully-erased row's wrap flag must be cleared"
        );
    }

    #[test]
    fn scrolling_moves_the_wrap_flag_with_its_content() {
        // 3-row grid: fill it so a further line forces a full-screen
        // scroll, and confirm the wrapped flag travels with the row's
        // actual content rather than staying pinned to a row index.
        let mut grid = Grid::new(5, 3);
        feed_str(&mut grid, "abcdefghij"); // row0="abcde", row1="fghij" (wrapped)
        feed_str(&mut grid, "\r\nzzzzz"); // row2: real newline, not wrapped
        assert!(grid.is_row_wrapped(1));
        assert!(!grid.is_row_wrapped(2));
        // One more real newline scrolls everything up by one: old row1
        // (wrapped) becomes row0, old row2 (not wrapped) becomes row1.
        feed_str(&mut grid, "\r\nwwwww");
        assert!(
            grid.is_row_wrapped(0),
            "the wrapped row's flag must scroll up with its content"
        );
        assert!(!grid.is_row_wrapped(1));
    }

    #[test]
    fn resize_forgets_stale_wrap_flags() {
        let mut grid = Grid::new(5, 4);
        feed_str(&mut grid, "abcdefghij");
        assert!(grid.is_row_wrapped(1));
        grid.resize(20, 4);
        assert!(
            !grid.is_row_wrapped(1),
            "resize has no reflow, so old wrap flags no longer describe the clipped/padded content"
        );
    }

    #[test]
    fn resize_with_unchanged_dimensions_is_a_true_no_op() {
        // A spurious SIGWINCH (fires without an actual size change, e.g.
        // from tmux status-bar redraws) must not wipe an active DECSTBM
        // region or wrap-tracking state.
        let mut grid = Grid::new(20, 10);
        feed_str(&mut grid, "\x1b[3;7r"); // DECSTBM: restrict to rows 3-7
        feed_str(&mut grid, "abcdefghijklmnopqrstuvwxyz"); // forces a wrap
        assert!(grid.is_row_wrapped(1));

        grid.resize(20, 10); // identical dimensions -- must be a no-op

        assert!(
            grid.is_row_wrapped(1),
            "wrap flags must survive a same-size resize"
        );
        // Confirm the DECSTBM region is still restricted (not reset to
        // full-screen) by checking that a scroll at the bottom margin
        // does NOT count as a full-page scroll.
        feed_str(&mut grid, "\x1b[7;1H\r\nX"); // move to bottom margin, force a scroll within it
        assert_eq!(
            grid.take_scroll_lines(),
            0,
            "a scroll confined to the DECSTBM region must not count as a full-page scroll"
        );
    }
}

/// Swap in a freshly sized grid, e.g. after a terminal resize (SIGWINCH),
/// preserving as much on-screen content as possible.
pub fn resized(old: &mut Grid, cols: usize, rows: usize) -> Grid {
    let mut new_grid = Grid::new(cols, rows);
    mem::swap(old, &mut new_grid);
    old.resize(cols, rows);
    new_grid
}

#[cfg(test)]
mod repro_tests {
    use super::*;

    fn feed_str(grid: &mut Grid, s: &str) {
        let mut parser = vte::Parser::new();
        crate::grid::feed(&mut parser, grid, s.as_bytes());
    }

    #[test]
    fn reverse_index_moves_cursor_up_like_real_zsh_autosuggestions() {
        // zsh-autosuggestions redraw pattern: move down to draw the
        // suggestion, ESC M to go back up, then reposition the column.
        let mut grid = Grid::new(40, 10);
        feed_str(&mut grid, "abc"); // cursor at row0, col3
        feed_str(&mut grid, "\x1b[1B"); // CUD: down 1 -> row1, col3
        assert_eq!((grid.cursor.row, grid.cursor.col), (1, 3));
        feed_str(&mut grid, "\x1bM"); // RI: should go back up -> row0
        assert_eq!(
            (grid.cursor.row, grid.cursor.col),
            (0, 3),
            "Reverse Index (ESC M) must move the cursor back up"
        );
    }

    #[test]
    fn decstbm_restricts_index_and_reverse_index_to_the_region() {
        // zsh sets a scroll region (rows 2..=5) to redraw a multi-line
        // suggestion without disturbing the prompt above it.
        let mut grid = Grid::new(10, 6);
        feed_str(&mut grid, "PROMPT>\r\n"); // row0
        feed_str(&mut grid, "typed cmd\r\n"); // row1
        feed_str(&mut grid, "\x1b[3;6r"); // DECSTBM: region = rows 2..=5 (0-indexed)
                                          // DECSTBM also homes the cursor to (0,0) per spec.
        assert_eq!((grid.cursor.row, grid.cursor.col), (0, 0));
        grid.cursor.row = 5; // move to the region's bottom margin
        feed_str(&mut grid, "\x1bD"); // IND at bottom margin -> scroll region up
        assert_eq!(
            grid.cursor.row, 5,
            "cursor should stay pinned at the bottom margin"
        );
        // Rows above the region (the real prompt) must be untouched.
        assert_eq!(
            grid.row(0).iter().map(|c| c.ch).collect::<String>().trim(),
            "PROMPT>"
        );
        assert_eq!(
            grid.row(1).iter().map(|c| c.ch).collect::<String>().trim(),
            "typed cmd"
        );
        grid.cursor.row = 2; // top margin
        feed_str(&mut grid, "\x1bM"); // RI at top margin -> scroll region down
        assert_eq!(
            grid.cursor.row, 2,
            "cursor should stay pinned at the top margin"
        );
        assert_eq!(
            grid.row(0).iter().map(|c| c.ch).collect::<String>().trim(),
            "PROMPT>"
        );
        assert_eq!(
            grid.row(1).iter().map(|c| c.ch).collect::<String>().trim(),
            "typed cmd"
        );
    }

    #[test]
    fn csi_g_backtick_d_move_to_absolute_position() {
        // CHA/HPA (`G`/backtick) and VPA (`d`).
        let mut grid = Grid::new(200, 24);
        grid.cursor.row = 5;
        grid.cursor.col = 10;
        feed_str(&mut grid, "\x1b[70G");
        assert_eq!((grid.cursor.row, grid.cursor.col), (5, 69));
        feed_str(&mut grid, "\x1b[136`");
        assert_eq!((grid.cursor.row, grid.cursor.col), (5, 135));
        feed_str(&mut grid, "\x1b[3d");
        assert_eq!(
            (grid.cursor.row, grid.cursor.col),
            (2, 135),
            "VPA must move the row and leave the column alone"
        );
    }

    #[test]
    fn real_captured_fzf_preview_border_lands_in_the_right_columns() {
        // Byte-for-byte from a real fzf Ctrl-T session with a preview
        // pane: draw the top border, then jump to columns 70/136 (pane
        // edges), moving down one row each time.
        let mut grid = Grid::new(200, 24);
        grid.cursor.row = 0;
        grid.cursor.col = 0;
        feed_str(
            &mut grid,
            "\x1b[1B\x1b[70G\u{2502} \x1b[136G \u{2502}\x1b[1B\x1b[70G\u{2502} \x1b[136G \u{2502}",
        );
        // Row 1 (0-indexed) should have the left border at column 69 and
        // the right border at column 136 (0-indexed 135/end).
        let row1: String = grid.row(1).iter().map(|c| c.ch).collect();
        assert_eq!(
            row1.chars().nth(69),
            Some('│'),
            "left preview border must land at column 70"
        );
        let row2: String = grid.row(2).iter().map(|c| c.ch).collect();
        assert_eq!(
            row2.chars().nth(69),
            Some('│'),
            "left preview border must land at column 70 on the next row too"
        );
    }

    #[test]
    fn decstbm_invalid_range_resets_to_full_screen() {
        let mut grid = Grid::new(10, 6);
        feed_str(&mut grid, "\x1b[5;2r"); // bottom < top: invalid
                                          // Should fall back to full-screen scrolling: IND at the last row
                                          // scrolls the whole screen, not nothing.
        grid.cursor.row = 5;
        feed_str(&mut grid, "x");
        feed_str(&mut grid, "\x1bD");
        assert_eq!(grid.cursor.row, 5);
    }

    #[test]
    fn bare_csi_cursor_moves_default_to_one_not_zero() {
        // `vte` represents an omitted CSI parameter as a literal `0`
        // (bare `ESC[A` yields params == [[0]]), not "no parameter". Per
        // ECMA-48, `0` and "absent" both mean "use the default".
        let mut grid = Grid::new(20, 10);
        grid.cursor.row = 5;
        grid.cursor.col = 5;
        feed_str(&mut grid, "\x1b[A"); // bare CUU
        assert_eq!(grid.cursor.row, 4, "bare ESC[A must move up by 1, not 0");
        feed_str(&mut grid, "\x1b[B"); // bare CUD
        assert_eq!(grid.cursor.row, 5, "bare ESC[B must move down by 1, not 0");
        feed_str(&mut grid, "\x1b[C"); // bare CUF
        assert_eq!(grid.cursor.col, 6, "bare ESC[C must move right by 1, not 0");
        feed_str(&mut grid, "\x1b[D"); // bare CUB
        assert_eq!(grid.cursor.col, 5, "bare ESC[D must move left by 1, not 0");
    }

    #[test]
    fn real_captured_autosuggestion_redraw_returns_to_typing_position() {
        // Byte-for-byte from a real Ghostty zsh-autosuggestions redraw of
        // a 3-line suggestion: write it, then bare CUU twice to climb back
        // to the typing line, then CUB to the right column.
        let mut grid = Grid::new(80, 24);
        grid.cursor.row = 1; // the "export " line
        grid.cursor.col = 12;
        feed_str(&mut grid, "\x1b[90mall_proxy=\"socks5://127.0.0.1:10808\"\x1b[39m\r\r\n\x1b[90mexport http_proxy=\"http://127.0.0.1:10808\"\x1b[39m\x1b[K\r\r\n\x1b[90mexport https_proxy=\"http://127.0.0.1:10808\"\x1b[39m\x1b[K\x1b[A\x1b[A\x1b[31D");
        assert_eq!((grid.cursor.row, grid.cursor.col), (1, 12));
    }

    #[test]
    fn restricted_scroll_region_marks_rows_structured_for_bidi_bypass() {
        // fzf's Ctrl-T/Ctrl-R inline popup claims the bottom few lines via
        // DECSTBM rather than the alternate screen; rows written there
        // must be flagged so the bidi pass leaves them alone.
        let mut grid = Grid::new(40, 10);
        feed_str(&mut grid, "prompt$ normal text\r\n"); // row0: ordinary prose
        assert!(!grid.is_row_structured(0));
        feed_str(&mut grid, "\x1b[6;10r"); // restrict to rows 5..=9 (0-indexed)
        grid.cursor.row = 5;
        grid.cursor.col = 0;
        feed_str(&mut grid, "│ سلام │"); // fzf-style row containing Persian text
        assert!(
            grid.is_row_structured(5),
            "row written under a restricted region must be marked structured"
        );
        // Resetting to a full-screen region and writing ordinary prose
        // again must clear the flag.
        feed_str(&mut grid, "\x1b[r"); // CSI r with no params = full screen
        grid.cursor.row = 5;
        grid.cursor.col = 0;
        feed_str(&mut grid, "more prose");
        assert!(
            !grid.is_row_structured(5),
            "flag must clear once control returns to normal rendering"
        );
    }

    #[test]
    fn structured_rows_are_not_bidi_reordered() {
        let mut grid = Grid::new(40, 10);
        feed_str(&mut grid, "\x1b[6;10r"); // restrict rows 5..=9
        grid.cursor.row = 5;
        grid.cursor.col = 0;
        // A box-drawing row with a Persian filename in the middle -- the
        // exact shape of an fzf preview-pane row.
        feed_str(&mut grid, "│ سلام │ preview │");
        let visual = crate::bidi::reorder_grid(&grid, &crate::bidi::NoopShaper);
        let logical: String = grid.row(5).iter().map(|c| c.ch).collect();
        let visual_row: String = visual.row(5).iter().map(|c| c.ch).collect();
        assert_eq!(
            logical.trim_end(),
            visual_row.trim_end(),
            "a structured row must pass through bidi unchanged"
        );
    }
}
