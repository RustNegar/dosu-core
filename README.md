# dosu-core

The core engine behind [Dosu](https://github.com/RustNegar/dosu): PTY handling, terminal-state tracking, bidirectional (RTL) text reordering, and diff-based rendering.

This crate contains no CLI and no keybindings — it is the library that Dosu (and any other bidirectional-aware terminal front end) is built on.

## Pipeline

```
child shell --stdout--> vte::Parser --> Grid (logical cell state)
                                             |
                                         bidi module
                                (UAX #9 reordering of RTL runs)
                                             |
                                       visual Grid
                                             |
                                     render::Renderer
                         (diffs against the last-drawn frame,
                          emits minimal ANSI to the real tty)
```

Keystrokes are forwarded from the real tty to the child's PTY unmodified. macOS and Linux already produce correct UTF-8 for Persian/Arabic/Hebrew keyboard layouts, so no keymap reimplementation is needed on the input side — only the _output_ requires reordering.

## Modules

| Module      | Responsibility                                                                                                                                                                                                                                                                        |
| ----------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `pty`       | Spawns the child shell inside a pseudo-terminal via `portable-pty`, abstracting macOS/Linux/BSD PTY differences. Exposes `PtySession` and a cloneable `PtyWriterHandle` for writing back to the child from outside the main read loop.                                                |
| `grid`      | Parses the child's byte stream with `vte` and maintains logical terminal state — cells, cursor, scrollback — independent of display order. `feed()` drives the parser; `Grid` holds the resulting cell matrix.                                                                        |
| `bidi`      | Reorders each row (or wrapped group of rows) from logical to visual order per UAX #9, using the `unicode-bidi` crate. Handles mixed RTL/LTR fields, UI chrome (icons, borders, spinners) that must stay fixed, line-number gutters, bracket mirroring, and cursor-column translation. |
| `altscreen` | Detects alternate-screen mode switches (`vim`, `less`, full-screen TUIs) so the caller can decide when to pass raw bytes through instead of reordering.                                                                                                                               |
| `render`    | Diffs the visual grid against the last-drawn frame and emits the minimal ANSI needed to update the real terminal, rather than redrawing the whole screen every frame.                                                                                                                 |
| `config`    | Loads `~/.config/dosu/config.toml` (shell override, session languages, log level), with sensible defaults if no config file is present.                                                                                                                                               |

## Usage

```rust
use dosu_core::{Grid, PtySession, Renderer};
use dosu_core::bidi::{reorder_grid, NoopShaper};
use dosu_core::grid::feed;

let mut grid = Grid::new(cols, rows);
let mut parser = vte::Parser::new();

// Feed raw bytes from the child PTY into the logical grid.
feed(&mut parser, &mut grid, bytes_from_child);

// Reorder RTL runs into visual order.
let visual = reorder_grid(&grid, &NoopShaper);

// Diff against the previous frame and write the minimal ANSI update.
let mut renderer = Renderer::new();
renderer.render(&mut stdout, &visual)?;
```

`NoopShaper` is correct for any CoreText- or HarfBuzz-backed terminal (iTerm2, Kitty, WezTerm, Ghostty, Terminal.app), since those already perform Arabic/Persian letter-joining once codepoints are in the correct visual order. The `Shaper` trait exists as a hook for renderers that need manual shaping.

## Supported scripts

The bidi engine is script-driven (UAX #9 via `unicode-bidi`), not locale-driven: any text whose Unicode bidi class is `AL`, `R`, `RLE`, `RLO`, `RLI`, or `AN` is treated as RTL. This covers Arabic, Persian, and Hebrew without script-specific logic.

## Fuzzing

`bidi::reorder_grid` is fuzzed with `cargo-fuzz` (libFuzzer). The targets in `fuzz/fuzz_targets/` drive real fuzzer bytes through the same `vte::Parser -> Grid -> reorder_grid` pipeline the actual pty pipeline uses (never a hand-constructed `Grid`), covering both valid-UTF-8 and raw/garbage-byte input, plus multi-row DECAWM auto-wrap grouping.

```sh
cargo install cargo-fuzz
cargo fuzz run reorder_grid            # valid-UTF-8 input
cargo fuzz run reorder_grid_lossy      # raw / possibly-invalid-UTF-8 input
cargo fuzz run reorder_grid_multiline  # multi-row / wrapped-line input
```

Fuzzing requires the nightly toolchain (`cargo-fuzz` installs and uses it automatically). CI runs a bounded 60-second smoke test per target on every push to `main`; it's a regression check, not continuous fuzzing, so run the commands above locally for longer sessions when working on `bidi.rs`.

## Requirements

- Rust 1.70 or higher
- A Unix-like operating system (Linux or macOS)

## License

MIT
