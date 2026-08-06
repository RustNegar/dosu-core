//! Fuzz target: feed arbitrary *valid* UTF-8 text through the real
//! VTE -> Grid -> reorder_grid pipeline and make sure it never panics
//! or hangs.
//!
//! Bytes that don't decode as valid UTF-8 are skipped (see
//! `reorder_grid_lossy` for the "garbage came down the pty" case, which
//! deliberately does NOT require valid UTF-8).
//!
//! This target intentionally makes no assertions about the *output* --
//! correctness for known shapes is covered by the unit tests in
//! `src/bidi.rs`. This is purely a crash/hang oracle.

#![no_main]

use dosu_core::bidi::{reorder_grid, NoopShaper};
use dosu_core::grid::{feed, Grid};
use libfuzzer_sys::fuzz_target;

// A grid wide enough to exercise both single-row and (with long input)
// auto-wrap into multiple rows, but small enough that fuzzing stays fast.
const COLS: usize = 40;
const ROWS: usize = 24;

fuzz_target!(|data: &[u8]| {
    let Ok(text) = std::str::from_utf8(data) else {
        return;
    };

    let mut grid = Grid::new(COLS, ROWS);
    let mut parser = vte::Parser::new();

    // Real pty output never arrives as one giant write; feed it in a
    // handful of chunks to also exercise the parser's cross-call state
    // (e.g. a multi-byte UTF-8 sequence or an escape sequence split
    // across two reads).
    for chunk in text.as_bytes().chunks(37.max(text.len() / 4 + 1)) {
        let _ = feed(&mut parser, &mut grid, chunk);
    }

    // Must not panic or hang for any valid-UTF-8 input.
    let _visual = reorder_grid(&grid, &NoopShaper);
});
