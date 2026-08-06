//! Fuzz target: feed *raw, possibly-invalid-UTF-8* bytes through
//! `String::from_utf8_lossy` and then the same VTE -> Grid ->
//! reorder_grid pipeline.
//!
//! A real pty can deliver truncated multi-byte sequences (a child
//! process writing a partial write(), a killed subprocess mid-escape-
//! sequence, binary garbage piped through `cat`, etc). This target
//! covers that "who knows what came down the pty" path, as opposed to
//! `reorder_grid`, which only exercises guaranteed-valid UTF-8.

#![no_main]

use dosu_core::bidi::{reorder_grid, NoopShaper};
use dosu_core::grid::{feed, Grid};
use libfuzzer_sys::fuzz_target;

const COLS: usize = 40;
const ROWS: usize = 24;

fuzz_target!(|data: &[u8]| {
    if data.is_empty() {
        return;
    }

    let lossy = String::from_utf8_lossy(data);

    let mut grid = Grid::new(COLS, ROWS);
    let mut parser = vte::Parser::new();

    // Also try feeding the *raw* bytes directly (not the lossy-decoded
    // string) -- vte::Parser is a byte-level state machine and is
    // expected to tolerate arbitrary/invalid byte sequences on its own,
    // same as a real pty stream.
    let _ = feed(&mut parser, &mut grid, data);
    let _visual = reorder_grid(&grid, &NoopShaper);

    // And separately, the lossy-decoded (guaranteed valid UTF-8, but
    // with U+FFFD replacement characters at every invalid boundary)
    // version, on a fresh grid.
    let mut grid2 = Grid::new(COLS, ROWS);
    let mut parser2 = vte::Parser::new();
    let _ = feed(&mut parser2, &mut grid2, lossy.as_bytes());
    let _visual2 = reorder_grid(&grid2, &NoopShaper);
});
