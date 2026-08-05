//! Diff-based renderer: keeps the last rendered grid, compares it
//! cell-by-cell against the new one, and writes only what changed.
//! Avoids overwrite/fingerprint drift since every frame is derived
//! from a complete grid rather than patched live.

use crate::grid::{Cell, Color, Grid};
use std::io::{self, Write};

pub struct Renderer {
    last: Option<Grid>,
    /// Real terminal rows above Grid's row 0. Set once at startup from
    /// the cursor's real position (via DSR), so writes land where the
    /// user's shell left off instead of assuming grid row 0 = top row.
    row_offset: usize,
    /// Real terminal's total row count, fixed at startup. Note:
    /// `visual.rows + row_offset` only equals this initially --
    /// `row_offset` shrinks on real scrolls, so use this field directly
    /// whenever the absolute bottom row is needed.
    real_terminal_rows: usize,
    /// True until the first `render()` call completes. The very first
    /// frame must never clear the screen (unlike later "no last frame"
    /// cases like resize/alt-screen exit, where clearing is safe since
    /// the on-screen content is dosu's own by then).
    is_first_render: bool,
}

impl Default for Renderer {
    fn default() -> Self {
        Self::new()
    }
}

impl Renderer {
    pub fn new() -> Self {
        Renderer {
            last: None,
            is_first_render: true,
            row_offset: 0,
            real_terminal_rows: 0,
        }
    }

    /// Sets `row_offset` and the fixed real terminal row count. Call once
    /// at startup with the cursor's real starting row (0-indexed) and
    /// `term_size()`.
    pub fn set_row_offset(&mut self, row_offset: usize, real_terminal_rows: usize) {
        self.row_offset = row_offset;
        self.real_terminal_rows = real_terminal_rows;
    }

    /// Header size: real terminal rows above dosu's drawable area.
    /// On a real resize, callers must subtract this before resizing the
    /// grid, or the grid will claim/give back rows that belong to the
    /// header instead of matching future `row + 1 + row_offset` writes.
    pub fn row_offset(&self) -> usize {
        self.row_offset
    }

    /// Forces the next `render` to do a full redraw (e.g. after resize,
    /// or once a child alt-screen app that scribbled on the terminal
    /// exits).
    pub fn invalidate(&mut self) {
        self.last = None;
    }

    /// `scroll_lines` is how many lines scrolled off Grid's top since the
    /// last frame (`Grid::take_scroll_lines`). Diffing alone repaints in
    /// place via absolute positioning and never touches the real
    /// terminal's scrollback, so we first replay `scroll_lines` real
    /// linefeeds to push the previous frame into scrollback, same as if
    /// dosu weren't intercepting output at all.
    pub fn render<W: Write>(
        &mut self,
        visual: &Grid,
        out: &mut W,
        scroll_lines: usize,
    ) -> io::Result<()> {
        let scroll_lines = scroll_lines.min(visual.rows);

        let last_is_usable = matches!(
            &self.last,
            Some(last) if last.cols == visual.cols && last.rows == visual.rows
        );

        if scroll_lines > 0 && last_is_usable {
            // Only replay real linefeeds when `last` is trustworthy
            // (matches the current screen). If `last` is missing/stale
            // (first frame, post-invalidate, or a dimension mismatch),
            // scrolling for real would push stale content into
            // scrollback right before the full-redraw clears it --
            // the full-redraw path below is sufficient on its own then.
            write!(out, "\x1b[{};1H", self.real_terminal_rows.max(visual.rows))?;
            for _ in 0..scroll_lines {
                out.write_all(b"\n")?;
            }
            // A real scroll shifts the whole physical terminal,
            // including the header space `row_offset` tracks. Up to
            // `row_offset` lines of the scroll are absorbed by that
            // header alone (grid row 0 stays grid row 0, just slid up
            // with everything else). Only the excess beyond
            // `row_offset` actually eats into dosu's own rows, so only
            // that excess needs mirroring onto `last`. (Reindexing by
            // the full `scroll_lines` instead double-shifts and was the
            // "clear only clears partway" bug: a scroll bigger than
            // `row_offset` left stale rows on the real terminal marked
            // as already-blank in `last`, so they never got recleared.)
            let last_reindex = scroll_lines.saturating_sub(self.row_offset);
            if let Some(last) = &mut self.last {
                scroll_grid_rows(last, last_reindex);
            }
            self.row_offset = self.row_offset.saturating_sub(scroll_lines);
        }

        let full_redraw = match &self.last {
            None => true,
            Some(last) => last.cols != visual.cols || last.rows != visual.rows,
        };

        if full_redraw && self.is_first_render {
            // First frame ever: never send `\x1b[2J`. Diff against an
            // implicit blank grid instead, so non-blank cells still
            // render (e.g. the shell's first prompt) without wiping
            // whatever the user's previous session left on screen.
            let blank = Grid::new(visual.cols, visual.rows);
            for r in 0..visual.rows {
                self.diff_row(out, r, blank.row(r), visual.row(r))?;
            }
        } else if full_redraw {
            write!(out, "\x1b[2J\x1b[H")?; // clear + home
            for r in 0..visual.rows {
                self.write_row(out, r, visual.row(r))?;
            }
        } else {
            let last = self.last.as_ref().unwrap();
            for r in 0..visual.rows {
                let old_row = last.row(r);
                let new_row = visual.row(r);
                self.diff_row(out, r, old_row, new_row)?;
            }
        }

        self.is_first_render = false;

        // Position the real cursor to match the (already visually-mapped)
        // cursor position last, so it doesn't jump mid-redraw.
        write!(
            out,
            "\x1b[{};{}H",
            visual.cursor.row + 1 + self.row_offset,
            visual.cursor.col + 1
        )?;
        out.flush()?;

        self.last = Some(clone_grid(visual));
        Ok(())
    }

    fn write_row<W: Write>(&self, out: &mut W, row: usize, cells: &[Cell]) -> io::Result<()> {
        write!(out, "\x1b[{};1H", row + 1 + self.row_offset)?;
        write_cells(out, cells)
    }

    fn diff_row<W: Write>(
        &self,
        out: &mut W,
        row: usize,
        old: &[Cell],
        new: &[Cell],
    ) -> io::Result<()> {
        let mut col = 0usize;
        while col < new.len() {
            if old.get(col) == new.get(col) {
                col += 1;
                continue;
            }
            // Found a run of differing cells; extend it as far as possible
            // so we can emit one contiguous write instead of many single
            // cell writes.
            let start = col;
            while col < new.len() && old.get(col) != new.get(col) {
                col += 1;
            }
            write!(out, "\x1b[{};{}H", row + 1 + self.row_offset, start + 1)?;
            write_cells(out, &new[start..col])?;
        }
        Ok(())
    }
}

/// Everything about a cell that affects its SGR rendering (i.e. everything
/// except the character itself and wide-glyph bookkeeping). Used to detect
/// when a fresh `\x1b[...m` needs to be emitted.
type AttrKey = (bool, bool, bool, bool, bool, bool, Color, Color);

fn attr_key(cell: &Cell) -> AttrKey {
    (
        cell.bold,
        cell.dim,
        cell.italic,
        cell.underline,
        cell.reverse,
        cell.strikethrough,
        cell.fg,
        cell.bg,
    )
}

/// Builds the SGR escape sequence that reproduces a cell's full attribute
/// state from scratch (always starts with a `0` reset so we never inherit
/// stray state from whatever the real terminal had before).
fn sgr_for(cell: &Cell) -> String {
    let mut codes: Vec<String> = vec!["0".to_string()];
    if cell.bold {
        codes.push("1".into());
    }
    if cell.dim {
        codes.push("2".into());
    }
    if cell.italic {
        codes.push("3".into());
    }
    if cell.underline {
        codes.push("4".into());
    }
    if cell.reverse {
        codes.push("7".into());
    }
    if cell.strikethrough {
        codes.push("9".into());
    }
    match cell.fg {
        Color::Default => {}
        Color::Indexed(n) if n < 8 => codes.push((30 + n).to_string()),
        Color::Indexed(n) if n < 16 => codes.push((90 + (n - 8)).to_string()),
        Color::Indexed(n) => codes.push(format!("38;5;{n}")),
        Color::Rgb(r, g, b) => codes.push(format!("38;2;{r};{g};{b}")),
    }
    match cell.bg {
        Color::Default => {}
        Color::Indexed(n) if n < 8 => codes.push((40 + n).to_string()),
        Color::Indexed(n) if n < 16 => codes.push((100 + (n - 8)).to_string()),
        Color::Indexed(n) => codes.push(format!("48;5;{n}")),
        Color::Rgb(r, g, b) => codes.push(format!("48;2;{r};{g};{b}")),
    }
    format!("\x1b[{}m", codes.join(";"))
}

fn write_cells<W: Write>(out: &mut W, cells: &[Cell]) -> io::Result<()> {
    let mut cur: Option<AttrKey> = None;
    for cell in cells {
        let key = attr_key(cell);
        if cur != Some(key) {
            write!(out, "{}", sgr_for(cell))?;
            cur = Some(key);
        }
        write!(out, "{}", cell.ch)?;
    }
    write!(out, "\x1b[0m")
}

fn clone_grid(g: &Grid) -> Grid {
    let mut out = Grid::new(g.cols, g.rows);
    for r in 0..g.rows {
        for (c, cell) in g.row(r).iter().enumerate() {
            out.set_cell(r, c, *cell);
        }
    }
    out.cursor = g.cursor;
    out
}

/// Shifts every row of `g` up by `n`, padding the bottom with blank
/// cells -- mirrors `Grid::scroll_up` so `last` stays in sync with a
/// real terminal scroll.
fn scroll_grid_rows(g: &mut Grid, n: usize) {
    let n = n.min(g.rows);
    for r in 0..g.rows {
        if r + n < g.rows {
            let moved: Vec<Cell> = g.row(r + n).to_vec();
            for (c, cell) in moved.into_iter().enumerate() {
                g.set_cell(r, c, cell);
            }
        } else {
            for c in 0..g.cols {
                g.set_cell(r, c, Cell::default());
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::grid::feed;

    fn line_text(g: &Grid, r: usize) -> String {
        g.row(r)
            .iter()
            .map(|c| c.ch)
            .collect::<String>()
            .trim_end()
            .to_string()
    }

    #[test]
    fn repeated_scrolls_dont_reemit_lines_still_on_screen() {
        // Small scrolls across multiple render() calls must not repaint
        // lines that stay visible and unchanged (tmux-scrollback-dup bug).
        let mut grid = Grid::new(20, 5);
        let mut renderer = Renderer::new();
        let mut parser = vte::Parser::new();
        let mut out: Vec<u8> = Vec::new();

        let lines = ["one", "two", "three", "four", "five", "six", "seven"];
        for line in &lines {
            feed(&mut parser, &mut grid, format!("{line}\r\n").as_bytes());
            let scroll_lines = grid.take_scroll_lines();
            renderer.render(&grid, &mut out, scroll_lines).unwrap();
        }

        let stream = String::from_utf8_lossy(&out);
        // "four".."six" stay visible across consecutive renders, so each
        // should be written at most once.
        for line in ["four", "five", "six"] {
            let count = stream.matches(line).count();
            assert!(
                count <= 1,
                "'{line}' was (re)written {count} times, expected at most 1"
            );
        }

        // Visible content must still end up as the last 5 surviving lines.
        let visible: Vec<String> = (0..5).map(|r| line_text(&grid, r)).collect();
        assert_eq!(visible, vec!["four", "five", "six", "seven", ""]);
    }

    #[test]
    fn scroll_after_invalidate_does_not_emit_real_newlines() {
        // Right after invalidate() (e.g. alt-screen exit or resize), a
        // scroll must NOT be replayed as real linefeeds -- we don't know
        // what's really on screen, so that would push stale content into
        // scrollback right before the full-redraw clears it.
        let mut grid = Grid::new(20, 5);
        let mut renderer = Renderer::new();
        let mut parser = vte::Parser::new();
        let mut out: Vec<u8> = Vec::new();

        // Get a normal frame rendered first (last_is_usable afterwards).
        feed(&mut parser, &mut grid, b"one\r\ntwo\r\n");
        let initial_scroll = grid.take_scroll_lines();
        renderer.render(&grid, &mut out, initial_scroll).unwrap();

        renderer.invalidate();
        out.clear();

        // Now cause more scrolling in the same "invalid last" state.
        feed(&mut parser, &mut grid, b"three\r\nfour\r\nfive\r\nsix\r\n");
        let scroll_lines = grid.take_scroll_lines();
        assert!(scroll_lines > 0, "test setup should have caused a scroll");
        renderer.render(&grid, &mut out, scroll_lines).unwrap();

        assert!(
            !out.contains(&b'\n'),
            "no real linefeed should be sent right after invalidate(); got: {:?}",
            String::from_utf8_lossy(&out)
        );
    }

    #[test]
    fn scroll_larger_than_row_offset_still_clears_every_visible_row() {
        // A scroll bigger than the current row_offset (e.g. `clear` on a
        // screen fuller than dosu's header space) must not leave stale
        // rows un-erased on the real terminal, even though the diff
        // machinery thinks they're already blank ("clear only clears
        // partway" bug).
        let cols = 10;
        let grid_rows = 6;
        let row_offset = 2usize;
        let term_rows = row_offset + grid_rows;

        let mut grid = Grid::new(cols, grid_rows);
        let mut renderer = Renderer::new();
        renderer.set_row_offset(row_offset, term_rows);
        let mut parser = vte::Parser::new();
        let mut out: Vec<u8> = Vec::new();

        // Simulated real terminal (header + grid) that only ever
        // receives what `renderer` sends -- what the user would see.
        let mut real_sim = Grid::new(cols, term_rows);
        let mut real_parser = vte::Parser::new();

        // Populate the grid, render once so `last` is trusted, then
        // mirror that output onto the simulated real terminal.
        feed(
            &mut parser,
            &mut grid,
            b"one\r\ntwo\r\nthree\r\nfour\r\nfive\r\nsix",
        );
        renderer.render(&grid, &mut out, 0).unwrap();
        feed(&mut real_parser, &mut real_sim, &out);

        // Simulate `clear`: scroll by more than row_offset (2), as
        // grid.rs's erase_screen() does before wiping everything.
        feed(&mut parser, &mut grid, b"\x1b[2J\x1b[H");
        let scroll_lines = 5usize; // > row_offset (2)
        out.clear();
        renderer.render(&grid, &mut out, scroll_lines).unwrap();
        feed(&mut real_parser, &mut real_sim, &out);

        // Every row dosu's grid can address must now be fully blank.
        for r in 0..term_rows {
            let line: String = real_sim.row(r).iter().map(|c| c.ch).collect();
            let line = line.trim_end();
            assert!(
                line.is_empty(),
                "real terminal row {} still shows stale content {:?} after clear",
                r + 1,
                line
            );
        }
    }

    #[test]
    fn row_offset_shifts_every_absolute_position_written() {
        // Fixes dosu assuming its row 0 was always the terminal's top
        // row, which silently overwrote content above the real cursor.
        let mut grid = Grid::new(10, 3);
        let mut renderer = Renderer::new();
        renderer.set_row_offset(5, 8); // pretend the cursor started at real row 6, terminal is 8 rows
        let mut parser = vte::Parser::new();
        let mut out: Vec<u8> = Vec::new();

        feed(&mut parser, &mut grid, b"hi");
        renderer.render(&grid, &mut out, 0).unwrap();

        let s = String::from_utf8_lossy(&out);
        assert!(
            s.contains("\x1b[6;1H"),
            "row 0 content must be written at real row 6 (offset 5 + 1), got: {s:?}"
        );
        assert!(
            !s.contains("\x1b[1;1H"),
            "must never address the terminal's absolute row 1 when offset, got: {s:?}"
        );
    }

    #[test]
    fn first_render_ever_never_sends_a_clear_screen() {
        // The very first frame must not wipe the prior shell session's
        // screen -- only later full-redraws (resize, alt-screen exit)
        // should send `\x1b[2J`.
        let mut grid = Grid::new(20, 5);
        let mut renderer = Renderer::new();
        let mut parser = vte::Parser::new();
        let mut out: Vec<u8> = Vec::new();

        feed(&mut parser, &mut grid, b"hello");
        renderer.render(&grid, &mut out, 0).unwrap();
        assert!(
            !String::from_utf8_lossy(&out).contains("\x1b[2J"),
            "first render must not clear the screen; got: {:?}",
            String::from_utf8_lossy(&out)
        );

        // A later full-redraw (simulated via invalidate()) legitimately
        // clears.
        renderer.invalidate();
        out.clear();
        feed(&mut parser, &mut grid, b" world");
        renderer.render(&grid, &mut out, 0).unwrap();
        assert!(
            String::from_utf8_lossy(&out).contains("\x1b[2J"),
            "a later forced full-redraw should still clear normally"
        );
    }
}
