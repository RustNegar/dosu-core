//! Logical -> visual reordering for a single terminal row.
//!
//! We only do UAX #9 reordering, not letter-shaping: CoreText (Terminal.app,
//! iTerm2) already handles Arabic/Persian contextual joining on its own once
//! codepoints are in correct visual order. `Shaper` stays as a hook in case a
//! future renderer needs manual shaping.

use crate::grid::Cell;
use unicode_bidi::BidiInfo;

/// Post-reorder shaping hook. `NoopShaper` (the default) is correct for any
/// CoreText/HarfBuzz-backed terminal.
pub trait Shaper {
    fn shape(&self, logical_run: &[char]) -> Vec<char>;
}

pub struct NoopShaper;
impl Shaper for NoopShaper {
    fn shape(&self, logical_run: &[char]) -> Vec<char> {
        logical_run.to_vec()
    }
}

/// Gaps of this many blank cells or more mark a hard boundary between
/// independent text fields on the same row (list pane vs. preview pane,
/// `ls -l` columns, etc). Ordinary prose word-spacing is 1-2 spaces.
const MIN_FIELD_GAP: usize = 3;

/// UI chrome (icons, bullets, borders, spinners) that must never be treated
/// as part of a bidi paragraph, since it's not a "strong" bidi character and
/// would otherwise let an adjacent RTL letter drag it into the reorder.
/// Deliberately a narrow Unicode-block allowlist, not "any punctuation" --
/// ordinary punctuation must stay eligible for paragraph-direction detection.
fn is_structural_marker(c: char) -> bool {
    matches!(c as u32,
        0x2500..=0x257F // Box Drawing (─ │ ┌ ╭ ...)
        | 0x2580..=0x259F // Block Elements (▌ ▐ ░ ▓ ...)
        | 0x25A0..=0x25FF // Geometric Shapes (■ ● ▲ ...)
        | 0x2600..=0x26FF // Miscellaneous Symbols
        | 0x2700..=0x27BF // Dingbats (❯ ✓ ✗ ...)
        | 0x2800..=0x28FF // Braille Patterns (⠋ ⠙ ... -- spinners)
        | 0xE000..=0xF8FF // Private Use Area (nerd-font icons)
        | 0xF0000..=0xFFFFD
        | 0x100000..=0x10FFFD
    )
}

/// Splits off a leading run of `is_structural_marker` characters (plus a
/// separating blank) from `field`, returning how many cells to leave
/// untouched (no bidi). Keeps peeling (marker-run, space) pairs since a
/// prompt can chain several marker segments (icon, space, `❯`, space, ...).
fn split_marker_prefix(field: &[Cell]) -> usize {
    let mut i = 0;
    loop {
        let segment_start = i;
        while i < field.len() && is_structural_marker(field[i].ch) {
            i += 1;
        }
        if i == segment_start {
            break; // no marker characters here; nothing more to peel
        }
        if i >= field.len() || field[i].ch != ' ' {
            break; // marker ran straight into non-space content, or EOF
        }
        // A single space follows the marker run just consumed. Whether
        // it's a *separator between two marker segments* (keep peeling)
        // or the *final* space before real content (consume it and
        // stop) depends on what comes right after it.
        if i + 1 < field.len() && is_structural_marker(field[i + 1].ch) {
            i += 1; // separator space -- another marker segment follows
            continue;
        }
        i += 1; // final trailing space before real content
        break;
    }
    i.min(field.len())
}

/// Splits off a leading line-number "gutter" (e.g. `bat --number`'s
/// `12 │ `) on top of `split_marker_prefix`. A leading digit run only
/// counts as a gutter when followed by a real structural marker or a
/// space -- plain digits alone aren't structural, since prose can
/// legitimately start with a number.
fn split_gutter_prefix(field: &[Cell]) -> usize {
    let mut digits_end = 0;
    while digits_end < field.len() && field[digits_end].ch.is_ascii_digit() {
        digits_end += 1;
    }
    let gutter_end = if digits_end > 0 && digits_end <= 4 {
        let mut after_spaces = digits_end;
        while after_spaces < field.len() && field[after_spaces].ch == ' ' {
            after_spaces += 1;
        }
        let has_space = after_spaces > digits_end;
        let has_marker_next =
            after_spaces < field.len() && is_structural_marker(field[after_spaces].ch);
        if has_space || has_marker_next {
            // Covers both plain (`bat --style=numbers`, "1 ") and
            // bar-decorated ("1│") gutter styles.
            after_spaces
        } else {
            0
        }
    } else {
        0
    };
    gutter_end + split_marker_prefix(&field[gutter_end..])
}

/// Mirror of `split_marker_prefix` for the trailing edge (e.g. a preview
/// pane's fixed right-side border character). Returns the fixed suffix length.
fn split_marker_suffix(field: &[Cell]) -> usize {
    let mut i = field.len();
    while i > 0 && is_structural_marker(field[i - 1].ch) {
        i -= 1;
    }
    let marker_len = field.len() - i;
    if marker_len == 0 {
        return 0;
    }
    if i > 0 && field[i - 1].ch == ' ' {
        i -= 1;
    }
    field.len() - i
}

/// Splits `cells` into maximal ranges of content separated by gaps of
/// `MIN_FIELD_GAP`+ consecutive blank cells. Each returned range is fed to
/// bidi independently.
fn split_fields(cells: &[Cell]) -> Vec<std::ops::Range<usize>> {
    let mut fields = Vec::new();
    let mut i = 0;
    let n = cells.len();
    while i < n {
        if cells[i].ch == ' ' {
            i += 1;
            continue;
        }
        let start = i;
        // `field_end` tracks the real content end separately from the scan
        // cursor `i`, since `i` moves past a gap before we know if it's a
        // hard boundary.
        let mut field_end = i;
        while i < n {
            if cells[i].ch != ' ' {
                i += 1;
                field_end = i;
                continue;
            }
            let gap_start = i;
            while i < n && cells[i].ch == ' ' {
                i += 1;
            }
            if i - gap_start >= MIN_FIELD_GAP || i >= n {
                break; // hard boundary: field ends at gap_start, not i
            }
            field_end = i; // short (prose) gap: absorbed into the field
        }
        fields.push(start..field_end);
    }
    fields
}

/// Reorders one row of cells from logical to visual order (UAX #9). Cell
/// metadata travels with its character. Returns a same-length Vec (no
/// reflow — a row is a fixed-width viewport) plus a `logical_to_visual`
/// map so callers can translate the cursor's logical column into its
/// on-screen column: for a pure-RTL row the last-typed character (where
/// the cursor sits) reorders to the front, so the mapping isn't identity.
///
/// Bidi runs per-field (`split_fields`), not over the whole physical row:
/// a columnar TUI row (e.g. fzf's list + preview panes) is really two
/// unrelated text fields sharing a row, and UAX #9's first-strong-char
/// rule would otherwise let a Persian entry in the list pane classify the
/// *entire row* as RTL and fling it to the far right edge.
pub fn reorder_row(cells: &[Cell], shaper: &dyn Shaper) -> (Vec<Cell>, Vec<usize>) {
    if cells.is_empty() {
        return (Vec::new(), Vec::new());
    }

    // Trim to real content first: trailing blank padding must not be fed
    // into bidi, or a Persian-only line's RTL paragraph would swallow the
    // padding too and shove the real text to the row's far right edge.
    let content_len = cells
        .iter()
        .rposition(|c| *c != Cell::default())
        .map_or(0, |i| i + 1);
    if content_len == 0 {
        return (cells.to_vec(), (0..cells.len()).collect());
    }
    let content = &cells[..content_len];

    // No RTL characters: reorders to itself, skip the bidi pass.
    if !content.iter().any(|c| is_rtl_candidate(c.ch)) {
        return (cells.to_vec(), (0..cells.len()).collect());
    }

    let mut result: Vec<Cell> = content.to_vec();
    let mut logical_to_visual: Vec<usize> = (0..cells.len()).collect();

    for field in split_fields(content) {
        if field.is_empty()
            || !content[field.clone()]
                .iter()
                .any(|c| is_rtl_candidate(c.ch))
        {
            continue; // pure-LTR (or empty) field: nothing to reorder
        }
        // Peel off leading marker/gutter prefix and trailing marker suffix
        // before either can hijack the field's bidi paragraph direction.
        let marker_len = split_gutter_prefix(&content[field.clone()]);
        let text_start = field.start + marker_len;
        let suffix_len = split_marker_suffix(&content[text_start..field.end]);
        let text_field = text_start..(field.end - suffix_len);
        if text_field.is_empty()
            || !content[text_field.clone()]
                .iter()
                .any(|c| is_rtl_candidate(c.ch))
        {
            continue; // only markers (or nothing) left: no bidi needed
        }
        let (field_result, field_map) = reorder_field(&content[text_field.clone()], shaper);
        result[text_field.clone()].copy_from_slice(&field_result);
        for (local_i, &visual_local) in field_map.iter().enumerate() {
            logical_to_visual[text_field.start + local_i] = text_field.start + visual_local;
        }
    }

    let mut result_full = result;
    result_full.extend_from_slice(&cells[content_len..]);

    // Cursor immediately after the last typed character, on an RTL field:
    // the next character typed appears at the visual front of the field,
    // not the untranslated logical column. Only applies when the trailing
    // field itself is RTL.
    if content_len < cells.len() {
        if let Some(last_field) = split_fields(content).last() {
            if last_field.end == content_len {
                // Land after any marker prefix, not on top of it.
                let marker_len = split_gutter_prefix(&content[last_field.clone()]);
                let text_start = last_field.start + marker_len;
                let text_field = text_start..last_field.end;
                if !text_field.is_empty()
                    && content[text_field].iter().any(|c| is_rtl_candidate(c.ch))
                {
                    logical_to_visual[content_len] = text_start;
                }
            }
        }
    }

    (result_full, logical_to_visual)
}

/// Runs UAX #9 reordering on a single field (no internal wide gutters).
fn reorder_field(content: &[Cell], shaper: &dyn Shaper) -> (Vec<Cell>, Vec<usize>) {
    // `unicode-bidi` indexes by UTF-8 byte offset, not char index, so we
    // build an explicit cell<->byte-offset map rather than assume they match
    // (Persian/Arabic codepoints are 2 bytes each).
    let text: String = content.iter().map(|c| c.ch).collect();
    let mut cell_byte_start: Vec<usize> = Vec::with_capacity(content.len());
    let mut offset = 0usize;
    for cell in content {
        cell_byte_start.push(offset);
        offset += cell.ch.len_utf8();
    }

    let bidi_info = BidiInfo::new(&text, None);
    if bidi_info.paragraphs.is_empty() {
        return (content.to_vec(), (0..content.len()).collect());
    }
    let para = &bidi_info.paragraphs[0];
    let line = para.range.clone();
    reorder_field_with_bidi_info(content, &cell_byte_start, &bidi_info, para, line, shaper)
}

/// UAX#9 Rule L4: mirror bracket-like characters (`(` <-> `)`, etc.) when
/// they land inside an RTL run, so they still visually "open"/"close" in
/// the reading direction. Covers the common ASCII/guillemet pairs, not the
/// full Unicode BidiMirroring table.
fn mirror_char(c: char) -> char {
    match c {
        '(' => ')',
        ')' => '(',
        '[' => ']',
        ']' => '[',
        '{' => '}',
        '}' => '{',
        '<' => '>',
        '>' => '<',
        '\u{2039}' => '\u{203A}', // ‹ ›
        '\u{203A}' => '\u{2039}',
        '\u{00AB}' => '\u{00BB}', // « »
        '\u{00BB}' => '\u{00AB}',
        '\u{FF08}' => '\u{FF09}', // fullwidth ( )
        '\u{FF09}' => '\u{FF08}',
        '\u{3008}' => '\u{3009}', // 〈 〉
        '\u{3009}' => '\u{3008}',
        _ => c,
    }
}

/// Does the visual-run walk + shaping for one field, given an already-built
/// `BidiInfo`/paragraph and the byte range within it that corresponds to
/// `content`. Factored out so `reorder_logical_line` can run `BidiInfo::new`
/// once over several joined rows (levels resolved from the whole logical
/// line, per UAX #9) while still requesting visual runs scoped to one row's
/// slice at a time via `visual_runs(para, line_range)`.
///
/// `cell_byte_start[i]` is `content[i]`'s byte offset within the
/// *paragraph's* text, not relative to `content` -- these differ once
/// `content` is one row's slice of a joined multi-row paragraph.
fn reorder_field_with_bidi_info(
    content: &[Cell],
    cell_byte_start: &[usize],
    bidi_info: &BidiInfo,
    para: &unicode_bidi::ParagraphInfo,
    byte_range: std::ops::Range<usize>,
    shaper: &dyn Shaper,
) -> (Vec<Cell>, Vec<usize>) {
    let (levels, visual_runs) = bidi_info.visual_runs(para, byte_range);

    let mut result: Vec<Cell> = Vec::with_capacity(content.len());
    let mut logical_to_visual: Vec<usize> = (0..content.len()).collect();

    for run in visual_runs {
        let run_level = levels[run.start];
        let mut run_cell_indices: Vec<usize> = (0..content.len())
            .filter(|&i| cell_byte_start[i] >= run.start && cell_byte_start[i] < run.end)
            .collect();
        if run_level.is_rtl() {
            run_cell_indices.reverse();
        }

        let run_chars: Vec<char> = run_cell_indices.iter().map(|&i| content[i].ch).collect();
        let run_chars = if run_level.is_rtl() {
            run_chars.into_iter().map(mirror_char).collect()
        } else {
            run_chars
        };
        let shaped = if run_level.is_rtl() {
            shaper.shape(&run_chars)
        } else {
            run_chars
        };

        for (idx, ch) in run_cell_indices.into_iter().zip(shaped) {
            logical_to_visual[idx] = result.len();
            result.push(Cell { ch, ..content[idx] });
        }
    }

    (result, logical_to_visual)
}

/// Multi-row counterpart of `reorder_row`/`reorder_field`. `rows` are
/// consecutive physical rows forming one logical line (row 0 plus its
/// DECAWM auto-wrap continuations, see `Grid::wrapped_rows`). Runs UAX #9
/// once over the joined text so word order stays correct across wrap
/// points, then maps visual runs back onto each physical row (row width
/// is fixed, so only order within a row can change, never which row).
///
/// Callers must confirm every row has at most one `split_fields` field
/// first (see `reorder_grid`) -- joining rows with multiple columnar
/// fields isn't meaningful, since "wrap" is a single-column prose concept.
fn reorder_logical_line(rows: &[&[Cell]], shaper: &dyn Shaper) -> Vec<(Vec<Cell>, Vec<usize>)> {
    struct RowInfo {
        content_len: usize,
        /// The row's sole `split_fields` field, or empty if content is empty.
        field: std::ops::Range<usize>,
        /// `field` with any leading marker prefix peeled off; only this
        /// sub-range joins the cross-row bidi paragraph.
        text_field: std::ops::Range<usize>,
    }

    let infos: Vec<RowInfo> = rows
        .iter()
        .map(|cells| {
            let content_len = cells
                .iter()
                .rposition(|c| *c != Cell::default())
                .map_or(0, |i| i + 1);
            let content = &cells[..content_len];
            let field = split_fields(content).into_iter().next().unwrap_or(0..0);
            let marker_len = if field.is_empty() {
                0
            } else {
                split_gutter_prefix(&content[field.clone()])
            };
            let text_start = field.start + marker_len;
            let suffix_len = if text_start >= field.end {
                0
            } else {
                split_marker_suffix(&content[text_start..field.end])
            };
            let text_field = text_start..(field.end - suffix_len);
            RowInfo {
                content_len,
                field,
                text_field,
            }
        })
        .collect();

    let any_rtl = rows.iter().zip(&infos).any(|(cells, info)| {
        cells[..info.content_len]
            .iter()
            .any(|c| is_rtl_candidate(c.ch))
    });
    if !any_rtl {
        return rows
            .iter()
            .map(|c| (c.to_vec(), (0..c.len()).collect()))
            .collect();
    }

    // Join every row's post-marker text into one string, recording each
    // cell's byte offset (for mapping visual runs back to cells) and each
    // row's own byte sub-range (for querying `visual_runs` per row).
    let mut combined = String::new();
    let mut row_cell_bytes: Vec<Vec<usize>> = Vec::with_capacity(rows.len());
    let mut row_byte_range: Vec<std::ops::Range<usize>> = Vec::with_capacity(rows.len());
    for (cells, info) in rows.iter().zip(&infos) {
        let text_content = &cells[..info.content_len][info.text_field.clone()];
        let start_byte = combined.len();
        let mut bytes_this_row = Vec::with_capacity(text_content.len());
        for cell in text_content {
            bytes_this_row.push(combined.len());
            combined.push(cell.ch);
        }
        row_cell_bytes.push(bytes_this_row);
        row_byte_range.push(start_byte..combined.len());
    }

    let bidi_info = BidiInfo::new(&combined, None);
    let para = match bidi_info.paragraphs.first() {
        Some(p) => p,
        None => {
            return rows
                .iter()
                .map(|c| (c.to_vec(), (0..c.len()).collect()))
                .collect()
        }
    };

    let mut out = Vec::with_capacity(rows.len());
    for (i, (cells, info)) in rows.iter().zip(&infos).enumerate() {
        let content = &cells[..info.content_len];
        let mut result: Vec<Cell> = content.to_vec();
        let mut logical_to_visual: Vec<usize> = (0..cells.len()).collect();

        if !row_byte_range[i].is_empty() {
            let text_content = &content[info.text_field.clone()];
            let (field_result, field_map) = reorder_field_with_bidi_info(
                text_content,
                &row_cell_bytes[i],
                &bidi_info,
                para,
                row_byte_range[i].clone(),
                shaper,
            );
            result[info.text_field.clone()].copy_from_slice(&field_result);
            for (local_i, &visual_local) in field_map.iter().enumerate() {
                logical_to_visual[info.text_field.start + local_i] =
                    info.text_field.start + visual_local;
            }
        }

        // Only the last row in the group can have a cursor with trailing
        // blank padding (interior rows are always fully packed by DECAWM).
        // Same RTL cursor special case as `reorder_row`.
        if i == rows.len() - 1
            && info.content_len < cells.len()
            && info.field.end == info.content_len
            && content[info.text_field.clone()]
                .iter()
                .any(|c| is_rtl_candidate(c.ch))
        {
            logical_to_visual[info.content_len] = info.text_field.start;
        }

        let mut result_full = result;
        result_full.extend_from_slice(&cells[info.content_len..]);
        out.push((result_full, logical_to_visual));
    }

    out
}

fn is_rtl_candidate(c: char) -> bool {
    use unicode_bidi::BidiClass::*;
    matches!(unicode_bidi::bidi_class(c), AL | R | RLE | RLO | RLI | AN)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::grid::{feed, Grid};

    #[test]
    fn cursor_lands_at_visual_start_after_persian_only_line() {
        // Simulates: user types only Persian ("سلام") and the cursor sits
        // right after the last typed character, at logical column 4.
        let mut grid = Grid::new(20, 3);
        let mut parser = vte::Parser::new();
        feed(&mut parser, &mut grid, "سلام".as_bytes());
        assert_eq!(grid.cursor.row, 0);
        assert_eq!(grid.cursor.col, 4);

        let visual = reorder_grid(&grid, &NoopShaper);
        // Pure-RTL row: the last logical char (cursor position) reorders to
        // the front, so the on-screen column must be 0, not logical 4.
        assert_eq!(visual.cursor.col, 0);
    }
    #[test]
    fn parentheses_mirror_correctly_in_rtl_context() {
        // UAX#9 Rule L4: brackets must mirror when moved into an RTL run,
        // or "(Dosu)" renders visually flipped as ")Dosu(".
        let mut grid = Grid::new(30, 3);
        let mut parser = vte::Parser::new();
        feed(&mut parser, &mut grid, "# دوسو (Dosu)".as_bytes());
        let visual = reorder_grid(&grid, &NoopShaper);
        let row: String = visual.row(0).iter().map(|c| c.ch).collect();
        let row = row.trim_end();
        // The paren before "Dosu" in the final layout must be the opening
        // one, and the one after must be the closing one.
        let open_idx = row.find('(');
        let close_idx = row.find(')');
        let dosu_idx = row.find("Dosu");
        assert!(open_idx.is_some() && close_idx.is_some() && dosu_idx.is_some());
        assert!(
            open_idx.unwrap() < dosu_idx.unwrap(),
            "an opening paren must sit before Dosu, got {row:?}"
        );
        assert!(
            dosu_idx.unwrap() < close_idx.unwrap(),
            "a closing paren must sit after Dosu, got {row:?}"
        );
    }

    #[test]
    fn trailing_marker_suffix_stays_fixed_at_right_edge() {
        // e.g. a preview pane's fixed right-edge border char must not get
        // swept into the RTL reorder along with the content before it.
        let mut grid = Grid::new(30, 3);
        let mut parser = vte::Parser::new();
        feed(&mut parser, &mut grid, "سلام دنیا \u{2502}".as_bytes());
        let visual = reorder_grid(&grid, &NoopShaper);
        let row: String = visual.row(0).iter().map(|c| c.ch).collect();
        assert!(
            row.trim_end().ends_with('\u{2502}'),
            "trailing border must stay fixed, got {row:?}"
        );
    }

    #[test]
    fn bat_plain_number_gutter_with_no_marker_char_stays_fixed() {
        // bat's plain `--style=numbers` gutter has no separator char at
        // all -- just digits then a space -- unlike the bar-decorated style.
        let mut grid = Grid::new(30, 3);
        let mut parser = vte::Parser::new();
        feed(&mut parser, &mut grid, "   1 سلام".as_bytes());
        let visual = reorder_grid(&grid, &NoopShaper);
        let row: String = visual.row(0).iter().map(|c| c.ch).collect();
        assert!(
            row.starts_with("   1 "),
            "gutter must stay fixed at the start, got {row:?}"
        );
    }

    #[test]
    fn bar_style_gutter_directly_after_digits_still_works() {
        // Regression guard for the bar-decorated style ("  12│text").
        let mut grid = Grid::new(30, 3);
        let mut parser = vte::Parser::new();
        feed(&mut parser, &mut grid, "  12\u{2502}سلام دنیا".as_bytes());
        let visual = reorder_grid(&grid, &NoopShaper);
        let row: String = visual.row(0).iter().map(|c| c.ch).collect();
        assert!(
            row.starts_with("  12\u{2502}"),
            "bar-style gutter must stay fixed, got {row:?}"
        );
    }

    #[test]
    fn ui_marker_prefix_stays_fixed_while_persian_text_after_it_reorders() {
        // `▌` isn't a "strong" bidi char, so without marker protection the
        // Persian text after it would decide the field is RTL and drag it along.
        let mut grid = Grid::new(20, 3);
        let mut parser = vte::Parser::new();
        feed(&mut parser, &mut grid, "▌ تست/".as_bytes());

        let visual = reorder_grid(&grid, &NoopShaper);
        let row: String = visual.row(0).iter().map(|c| c.ch).collect();
        // The marker must stay at column 0 -- untouched by reordering.
        assert_eq!(row.chars().next(), Some('▌'));
    }

    #[test]
    fn persian_entry_in_narrow_field_stays_put_despite_distant_unrelated_content() {
        // A Persian entry in a narrow list pane, with unrelated Latin
        // content far to the right (preview pane), separated by a wide gutter.
        // Without field-splitting, the leftmost strong char (Persian) would
        // classify the whole row RTL and drag "سلام/" to the far right edge.
        let mut grid = Grid::new(100, 3);
        let mut parser = vte::Parser::new();
        // CUP to each field's column, matching how fzf draws it.
        feed(&mut parser, &mut grid, b"\x1b[1;2H");
        feed(&mut parser, &mut grid, "سلام/".as_bytes());
        feed(&mut parser, &mut grid, b"\x1b[1;60H");
        feed(&mut parser, &mut grid, b"checksum = \"43d5b281e737544\"");

        let visual = reorder_grid(&grid, &NoopShaper);
        let visual_row: String = visual.row(0).iter().map(|c| c.ch).collect();
        let persian_pos = visual_row
            .find('م')
            .expect("Persian text must still be present");
        assert!(persian_pos < 20, "Persian entry must stay near its original column (~2), got position {persian_pos} (row: {visual_row:?})");
        assert!(visual_row.contains("checksum = \"43d5b281e737544\""));
    }

    #[test]
    fn wrapped_line_reorders_as_one_paragraph_not_per_row() {
        // A Latin word ("ABC") wraps via DECAWM so its tail ("BC") lands
        // on the same row as the next Persian word ("شش"). Reordered per-row
        // in isolation, that row would misdetect itself as LTR; jointly, it
        // correctly uses the logical line's RTL direction.
        // 4-column grid: row0 = "پنجA" (fills exactly), row1 = "BCشش".
        let mut grid = Grid::new(4, 3);
        let mut parser = vte::Parser::new();
        feed(&mut parser, &mut grid, "پنجABCشش".as_bytes());
        assert!(
            grid.is_row_wrapped(1),
            "row1 must be recorded as an auto-wrap continuation of row0"
        );

        let visual = reorder_grid(&grid, &NoopShaper);
        let row1: String = visual.row(1).iter().map(|c| c.ch).collect();
        assert_eq!(
            row1, "ششBC",
            "row1 must reorder using the whole logical line's (RTL) direction, not its own -- got {row1:?}"
        );
    }

    #[test]
    fn wrapped_group_with_a_multi_field_row_falls_back_to_per_row() {
        // v1 scope limit: a wrapped group with a multi-field row (columnar
        // TUI, not prose) falls back to independent per-row reordering.
        let mut grid = Grid::new(20, 3);
        let mut parser = vte::Parser::new();
        // Fill row0, then print one more char to trigger the auto-wrap
        // (DECAWM only wraps once something is printed past the last column).
        let row0_fill = format!("سلام{}", "ی".repeat(16));
        assert_eq!(row0_fill.chars().count(), 20);
        feed(&mut parser, &mut grid, row0_fill.as_bytes());
        feed(&mut parser, &mut grid, "ی".as_bytes());
        assert_eq!(grid.cursor.row, 1);
        // Two widely separated fields on row1, a pattern prose wrap can't produce.
        feed(&mut parser, &mut grid, b"\x1b[2;1H");
        feed(&mut parser, &mut grid, "فارسی".as_bytes());
        feed(&mut parser, &mut grid, b"\x1b[2;15H");
        feed(&mut parser, &mut grid, b"latin text");
        assert!(grid.is_row_wrapped(1));

        // Reference: plain per-row reordering for row1 in isolation.
        let (expected_row1, _) = reorder_row(grid.row(1), &NoopShaper);

        let visual = reorder_grid(&grid, &NoopShaper);
        assert_eq!(
            visual.row(1),
            expected_row1.as_slice(),
            "a wrapped row with >1 field must fall back to independent per-row reordering"
        );
    }

    #[test]
    fn long_persian_sentence_wraps_across_rows_with_all_text_preserved() {
        // Content-integrity smoke test: every typed character must survive
        // reordering somewhere in the grid, however many rows it wrapped across.
        let sentence = "این یک جمله‌ی بسیار بسیار بسیار بسیار طولانیِ فارسی است که قطعاً از عرض یک ترمینال معمولی رد می‌شود و باید روی حداقل دو ردیف نمایش داده شود";
        let cols = 20;
        let rows = 10;
        let mut grid = Grid::new(cols, rows);
        let mut parser = vte::Parser::new();
        feed(&mut parser, &mut grid, sentence.as_bytes());

        assert!(
            grid.cursor.row >= 1,
            "sentence must be long enough to wrap past row 0"
        );
        assert!(
            grid.is_row_wrapped(1),
            "row1 must be a recorded auto-wrap continuation"
        );

        let visual = reorder_grid(&grid, &NoopShaper);
        let mut all_visual_chars: Vec<char> = Vec::new();
        for r in 0..=grid.cursor.row {
            all_visual_chars.extend(visual.row(r).iter().map(|c| c.ch));
        }
        let mut expected: Vec<char> = sentence.chars().collect();
        let mut actual: Vec<char> = all_visual_chars
            .into_iter()
            .filter(|&c| c != '\0' && c != ' ')
            .collect();
        expected.retain(|&c| c != ' ');
        expected.sort_unstable();
        actual.sort_unstable();
        assert_eq!(
            actual, expected,
            "reordering must not drop, duplicate, or corrupt any character"
        );
    }

    #[test]
    fn cursor_lands_after_marker_not_on_top_of_it_while_typing_rtl() {
        // The cursor must land just past the marker, not on top of it
        // (column 0), while typing RTL text after a "❯ " prompt.
        let mut grid = Grid::new(40, 3);
        let mut parser = vte::Parser::new();
        feed(&mut parser, &mut grid, "❯ ".as_bytes());
        feed(&mut parser, &mut grid, "این یک متن تست است".as_bytes());

        let visual = reorder_grid(&grid, &NoopShaper);
        let row: String = visual.row(0).iter().map(|c| c.ch).collect();
        assert_eq!(row.chars().next(), Some('❯'), "marker must still be first");
        assert_eq!(
            visual.cursor.col, 2,
            "cursor must land right after the marker+space (col 2), not on the marker itself"
        );
    }

    #[test]
    fn cursor_lands_after_marker_on_a_wrapped_continuation_row_too() {
        // Same case, but the marker's row is also part of a wrapped logical line.
        let mut grid = Grid::new(20, 3);
        let mut parser = vte::Parser::new();
        feed(&mut parser, &mut grid, "❯ ".as_bytes());
        // Long enough to wrap onto a 2nd row.
        feed(
            &mut parser,
            &mut grid,
            "این یک متن تست است که خیلی طولانی است".as_bytes(),
        );
        assert!(grid.is_row_wrapped(1));

        let visual = reorder_grid(&grid, &NoopShaper);
        assert_eq!(visual.cursor.row, grid.cursor.row);
        // Cursor sits on the last (continuation) row -- must not be
        // pulled back to the marker's row/column at all.
        assert_ne!((visual.cursor.row, visual.cursor.col), (0, 0));
    }

    #[test]
    fn chained_marker_segments_all_stay_fixed_not_just_the_first() {
        // An oh-my-posh-style template chains multiple marker segments
        // (icon, then separately a `❯` glyph); all must stay fixed, not just
        // the first segment peeled off.
        let mut grid = Grid::new(40, 3);
        let mut parser = vte::Parser::new();
        feed(&mut parser, &mut grid, " \u{eeed} \u{276f} ".as_bytes());
        feed(&mut parser, &mut grid, "این یک متن تست است".as_bytes());

        let visual = reorder_grid(&grid, &NoopShaper);
        let row: String = visual.row(0).iter().map(|c| c.ch).collect();
        // Both marker segments, in their original order, must still
        // lead the line -- untouched by reordering.
        assert!(
            row.starts_with(" \u{eeed} \u{276f} "),
            "both marker segments must stay fixed at the front, got {row:?}"
        );
        // And the cursor must land right after them, not dragged into
        // the reordered Persian text.
        assert_eq!(visual.cursor.col, 5);
    }
}

/// Reorders an entire grid, row by row, into a new visual-order grid, and
/// translates the cursor's logical column into the matching visual column.
pub fn reorder_grid(grid: &crate::grid::Grid, shaper: &dyn Shaper) -> crate::grid::Grid {
    let mut out = crate::grid::Grid::new(grid.cols, grid.rows);
    let mut visual_cursor_col = grid.cursor.col;

    // Applies one row's reorder result to `out`, shared by every branch below.
    let apply_row = |out: &mut crate::grid::Grid,
                     visual_cursor_col: &mut usize,
                     row_idx: usize,
                     visual: Vec<Cell>,
                     logical_to_visual: &[usize]| {
        if row_idx == grid.cursor.row {
            *visual_cursor_col = logical_to_visual
                .get(grid.cursor.col)
                .copied()
                .unwrap_or(grid.cursor.col);
        }
        for (c, cell) in visual.into_iter().enumerate() {
            out.set_cell(row_idx, c, cell);
        }
    };

    let mut r = 0;
    while r < grid.rows {
        // A row written under a restricted DECSTBM scroll region belongs to
        // an inline TUI widget's layout, not prose -- copy it through
        // unchanged. See `Grid::structured_rows`.
        if grid.is_row_structured(r) {
            for (c, cell) in grid.row(r).iter().copied().enumerate() {
                out.set_cell(r, c, cell);
            }
            if r == grid.cursor.row {
                visual_cursor_col = grid.cursor.col;
            }
            r += 1;
            continue;
        }

        // Group row `r` with its DECAWM auto-wrap continuations
        // (`Grid::wrapped_rows`) into one logical prose line, reordered as
        // a single UAX #9 paragraph (see `reorder_logical_line`). A
        // structured row ends a group.
        let group_start = r;
        let mut group_end = r + 1;
        while group_end < grid.rows
            && grid.is_row_wrapped(group_end)
            && !grid.is_row_structured(group_end)
        {
            group_end += 1;
        }

        if group_end - group_start == 1 {
            let (visual, logical_to_visual) = reorder_row(grid.row(r), shaper);
            apply_row(
                &mut out,
                &mut visual_cursor_col,
                r,
                visual,
                &logical_to_visual,
            );
            r += 1;
            continue;
        }

        let group_rows: Vec<&[Cell]> = (group_start..group_end).map(|i| grid.row(i)).collect();

        // v1 scope limit: joining rows only makes sense for single-
        // column prose. If any row in the group actually has more than
        // one `split_fields` field (a columnar TUI row, e.g. fzf's list
        // + preview panes, happening to also be DECAWM-wrapped), fall
        // back to reordering every row in the group independently --
        // the existing, already-correct per-row behavior.
        let has_multi_field_row = group_rows.iter().any(|cells| {
            let content_len = cells
                .iter()
                .rposition(|c| *c != Cell::default())
                .map_or(0, |i| i + 1);
            split_fields(&cells[..content_len]).len() > 1
        });

        if has_multi_field_row {
            for row_idx in group_start..group_end {
                let (visual, logical_to_visual) = reorder_row(grid.row(row_idx), shaper);
                apply_row(
                    &mut out,
                    &mut visual_cursor_col,
                    row_idx,
                    visual,
                    &logical_to_visual,
                );
            }
            r = group_end;
            continue;
        }

        for (offset, (visual, logical_to_visual)) in reorder_logical_line(&group_rows, shaper)
            .into_iter()
            .enumerate()
        {
            apply_row(
                &mut out,
                &mut visual_cursor_col,
                group_start + offset,
                visual,
                &logical_to_visual,
            );
        }
        r = group_end;
    }

    out.cursor = grid.cursor;
    out.cursor.col = visual_cursor_col;
    out
}
