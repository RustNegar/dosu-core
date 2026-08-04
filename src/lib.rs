//! dosu-core
//!
//! The engine behind Dosu: a modern, cross-platform bidirectional
//! (Persian/Arabic) terminal wrapper.
//!
//! Pipeline:
//!
//!   [child shell] --stdout--> [vte::Parser] --> [Grid: logical cell state]
//!                                                        |
//!                                                  [bidi module]
//!                                          (reorders RTL runs + Arabic/
//!                                           Persian letter joining)
//!                                                        |
//!                                                  [visual Grid]
//!                                                        |
//!                                                [render::Renderer]
//!                                        (diffs against last-drawn frame,
//!                                         emits minimal ANSI to the real tty)
//!
//! Keystrokes go straight from the real tty to the child's PTY, unmodified —
//! macOS already produces correct UTF-8 for Persian/Arabic keyboard layouts,
//! so unlike the original bicon, we do not reimplement keymaps at all.

pub mod altscreen;
pub mod bidi;
pub mod config;
pub mod grid;
pub mod pty;
pub mod render;

pub use config::Config;
pub use grid::Grid;
pub use pty::{PtySession, PtyWriterHandle};
pub use render::Renderer;
