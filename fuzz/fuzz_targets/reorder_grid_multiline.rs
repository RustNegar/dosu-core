//! Fuzz target: multi-row grids, including DECAWM auto-wrap
//! continuations, feeding `reorder_grid`'s logical-line-grouping path
//! (`Grid::is_row_wrapped` / the `reorder_logical_line` branch in
//! `bidi::reorder_grid`).
//!
//! A narrow grid (16 cols) is used deliberately so that arbitrary fuzzer
//! text reliably wraps across 2+ rows, exercising the "join wrapped rows
//! into one logical paragraph before reordering" path -- the prompt's
//! specific worry that row-wrapping interacts badly with bidi.
//!
//! Real explicit newlines (`\r\n`) are also injected periodically so the
//! grid contains a mix of hard line breaks and auto-wrap continuations,
//! matching real shell output (wrapped prompt line, then a real `\n`,
//! then more wrapped prose).

#![no_main]

use dosu_core::bidi::{reorder_grid, NoopShaper};
use dosu_core::grid::{feed, Grid};
use libfuzzer_sys::fuzz_target;

// Narrow on purpose: forces wrapping with realistically short fuzzer
// inputs instead of needing huge inputs to fill a normal 80/120-col grid.
const COLS: usize = 16;
const ROWS: usize = 30;

fuzz_target!(|data: &[u8]| {
    let Ok(text) = std::str::from_utf8(data) else {
        return;
    };
    if text.is_empty() {
        return;
    }

    let mut grid = Grid::new(COLS, ROWS);
    let mut parser = vte::Parser::new();

    // Split the fuzzer input on a byte that's cheap for the fuzzer to
    // discover (ASCII '|') into "lines" separated by real \r\n, while
    // feeding each line itself unbroken (so long lines auto-wrap).
    for (i, line) in text.split('|').enumerate() {
        if i > 0 {
            let _ = feed(&mut parser, &mut grid, b"\r\n");
        }
        let _ = feed(&mut parser, &mut grid, line.as_bytes());
    }

    let _visual = reorder_grid(&grid, &NoopShaper);
});
