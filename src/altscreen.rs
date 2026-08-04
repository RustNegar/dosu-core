//! Detects when the child switches into the "alternate screen" buffer
//! (`\x1b[?1049h` and friends), used by full-screen programs like
//! neovim, tmux, less, htop, man.
//!
//! Dosu's `Grid` model only implements a pragmatic ECMA-48/xterm subset
//! (no scroll regions, no alt-screen buffer, etc.), which breaks
//! full-screen programs that manage the real terminal directly. Instead
//! of reimplementing a full VT100 emulator, this scans the raw byte
//! stream (before `vte::Parser` sees it) for the alt-screen toggle
//! sequences and splits it into `Grid` segments (normal pipeline) and
//! `Raw` segments (forwarded straight to the real terminal, unmodified,
//! bidi skipped). Toggle sequences themselves are always `Raw`, since
//! the real terminal needs to see them to actually switch buffers. A
//! toggle sequence split across two PTY reads is carried over instead
//! of being misclassified.

/// Known enter/exit sequences for the alternate screen buffer. `?1049` is
/// the modern form (saves cursor too) used by neovim/tmux; `?47`/`?1047`
/// are older forms still used by e.g. some `less`/`vim` builds.
const TOGGLES: &[(&[u8], bool)] = &[
    (b"\x1b[?1049h", true),
    (b"\x1b[?1049l", false),
    (b"\x1b[?1047h", true),
    (b"\x1b[?1047l", false),
    (b"\x1b[?47h", true),
    (b"\x1b[?47l", false),
];

const MAX_TOGGLE_LEN: usize = 9; // len of "\x1b[?1049h"

/// GNU Screen/tmux's non-standard "set window title" escape:
/// `ESC k <title text> ESC \`. Not a standard ECMA-48 string introducer,
/// so `vte::Parser` can't treat it as one opaque unit -- a bare `ESC k`
/// dispatches immediately as a complete escape, then everything up to
/// `ESC \` gets fed to `print()` as ordinary text. zsh emits this after
/// every command via a precmd/preexec hook (e.g. `ESC k echo ESC \`);
/// without this check the command name was printed a second time onto
/// the grid, causing the "command shows up twice" bug.
const TITLE_START: u8 = b'k';
const TITLE_TERMINATOR: &[u8] = b"\x1b\\";
const MAX_TITLE_LEN: usize = 1024;

fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}

#[derive(Debug, PartialEq, Eq)]
pub enum Segment {
    /// Route through vte::Parser -> Grid -> bidi -> diff-render, as usual.
    Grid(Vec<u8>),
    /// Write straight to the real terminal, unmodified.
    Raw(Vec<u8>),
}

pub struct AltScreenScanner {
    in_alt: bool,
    /// An escape sequence seen at the end of the previous chunk that
    /// might still turn into a recognized toggle once more bytes arrive.
    carry: Vec<u8>,
}

impl AltScreenScanner {
    pub fn new() -> Self {
        AltScreenScanner { in_alt: false, carry: Vec::new() }
    }

    pub fn in_alt_screen(&self) -> bool {
        self.in_alt
    }

    /// Splits `input` into `Grid`/`Raw` segments, updating alt-screen
    /// state as toggle sequences are recognized.
    pub fn scan(&mut self, input: &[u8]) -> Vec<Segment> {
        let mut data = std::mem::take(&mut self.carry);
        data.extend_from_slice(input);

        let mut segments = Vec::new();
        let mut cur: Vec<u8> = Vec::new();
        let mut i = 0;

        macro_rules! flush_cur {
            () => {
                if !cur.is_empty() {
                    let bytes = std::mem::take(&mut cur);
                    segments.push(if self.in_alt {
                        Segment::Raw(bytes)
                    } else {
                        Segment::Grid(bytes)
                    });
                }
            };
        }

        while i < data.len() {
            if data[i] == 0x1b {
                let remaining = &data[i..];

                if let Some(&(pat, entering)) =
                    TOGGLES.iter().find(|(p, _)| remaining.starts_with(p))
                {
                    flush_cur!();
                    segments.push(Segment::Raw(pat.to_vec()));
                    self.in_alt = entering;
                    i += pat.len();
                    continue;
                }

                let could_still_match = remaining.len() < MAX_TOGGLE_LEN
                    && TOGGLES.iter().any(|(p, _)| p.starts_with(remaining));
                if could_still_match {
                    // Wait for the rest of this sequence on the next read.
                    flush_cur!();
                    self.carry = remaining.to_vec();
                    return segments;
                }

                if remaining.len() >= 2 && remaining[1] == TITLE_START {
                    if let Some(term_offset) = find_subslice(&remaining[2..], TITLE_TERMINATOR) {
                        let seq_len = 2 + term_offset + TITLE_TERMINATOR.len();
                        flush_cur!();
                        segments.push(Segment::Raw(remaining[..seq_len].to_vec()));
                        i += seq_len;
                        continue;
                    } else if remaining.len() < MAX_TITLE_LEN {
                        // Terminator not seen yet -- could be split
                        // across reads. Hold for the next call.
                        flush_cur!();
                        self.carry = remaining.to_vec();
                        return segments;
                    }
                    // No terminator within MAX_TITLE_LEN: give up rather
                    // than buffering forever; fall through below.
                }

                // Not a toggle/title sequence and can't become one:
                // treat this ESC as ordinary content for the current mode.
            }
            cur.push(data[i]);
            i += 1;
        }
        flush_cur!();
        segments
    }
}

impl Default for AltScreenScanner {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn grid_bytes(segs: &[Segment]) -> Vec<u8> {
        segs.iter()
            .filter_map(|s| match s {
                Segment::Grid(b) => Some(b.clone()),
                Segment::Raw(_) => None,
            })
            .flatten()
            .collect()
    }

    fn raw_bytes(segs: &[Segment]) -> Vec<u8> {
        segs.iter()
            .filter_map(|s| match s {
                Segment::Raw(b) => Some(b.clone()),
                Segment::Grid(_) => None,
            })
            .flatten()
            .collect()
    }

    #[test]
    fn plain_text_all_goes_to_grid() {
        let mut s = AltScreenScanner::new();
        let segs = s.scan(b"hello \x1b[31mworld\x1b[0m");
        assert!(!s.in_alt_screen());
        assert_eq!(grid_bytes(&segs), b"hello \x1b[31mworld\x1b[0m");
        assert!(raw_bytes(&segs).is_empty());
    }

    #[test]
    fn entering_and_leaving_alt_screen_routes_content_raw() {
        let mut s = AltScreenScanner::new();
        let segs = s.scan(b"before\x1b[?1049hNEOVIM CONTENT\x1b[?1049lafter");
        // Enter-toggle through exit-toggle must be raw; "before"/"after"
        // go through Grid.
        assert_eq!(grid_bytes(&segs), b"beforeafter");
        let raw = raw_bytes(&segs);
        assert!(raw.starts_with(b"\x1b[?1049h"));
        assert!(raw.windows(14).any(|w| w == b"NEOVIM CONTENT"));
        assert!(raw.ends_with(b"\x1b[?1049l"));
        assert!(!s.in_alt_screen());
    }

    #[test]
    fn toggle_split_across_two_reads_is_still_recognized() {
        let mut s = AltScreenScanner::new();
        let segs1 = s.scan(b"hi\x1b[?10");
        assert!(!s.in_alt_screen());
        assert_eq!(grid_bytes(&segs1), b"hi");
        let segs2 = s.scan(b"49hvim-stuff");
        assert!(s.in_alt_screen());
        assert_eq!(raw_bytes(&segs2), b"\x1b[?1049hvim-stuff");
    }

    #[test]
    fn screen_title_sequence_does_not_leak_into_grid_text() {
        // zsh sets the screen/tmux window title to the command name
        // after each run; without this fix "echo" would show up twice
        // in grid_bytes (the "command appears twice" bug).
        let mut s = AltScreenScanner::new();
        let segs = s.scan(b"echo hi\r\n\x1bkecho\x1b\\next-prompt");
        assert_eq!(grid_bytes(&segs), b"echo hi\r\nnext-prompt");
        assert_eq!(raw_bytes(&segs), b"\x1bkecho\x1b\\");
        assert!(!s.in_alt_screen());
    }

    #[test]
    fn screen_title_split_across_two_reads_is_still_recognized() {
        let mut s = AltScreenScanner::new();
        let segs1 = s.scan(b"before\x1bkexi");
        assert_eq!(grid_bytes(&segs1), b"before");
        assert!(raw_bytes(&segs1).is_empty());
        let segs2 = s.scan(b"t\x1b\\after");
        assert_eq!(raw_bytes(&segs2), b"\x1bkexit\x1b\\");
        assert_eq!(grid_bytes(&segs2), b"after");
    }
}
