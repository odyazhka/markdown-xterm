//! mdterm_bridge
//!
//! A small streaming markdown -> ANSI converter, exposed as a `staticlib`
//! with a plain C ABI so it can be linked straight into xterm (see
//! `mdterm_bridge.h` and `xterm-411-mdterm.patch` next to this file).
//!
//! Design notes:
//! - xterm hands us whatever `read()` returned from the PTY, which can
//!   split a markdown line (or even a UTF-8 line) across two calls.
//!   `MdTermState` buffers bytes until it has at least one complete
//!   `\n`-terminated line before converting anything, so markdown
//!   spanning a read() boundary still gets parsed correctly.
//! - Splitting on `\n` is UTF-8 safe: `\n` (0x0A) never appears as a
//!   continuation byte in a multi-byte UTF-8 sequence, so we never cut a
//!   character in half.
//! - Conversion is intentionally line-oriented and regex-based rather
//!   than a full CommonMark parser: LLM chat output is almost always
//!   headings / bold / italic / inline code / lists / fenced code
//!   blocks, and a full block-structure parser would fight the
//!   line-at-a-time streaming model this needs.

use regex::Regex;
use std::os::raw::c_void;
use std::sync::OnceLock;
use std::{ptr, slice};

// ---------------------------------------------------------------------
// Converter state
// ---------------------------------------------------------------------

pub struct MdTermState {
    /// Bytes received but not yet part of a complete '\n'-terminated line.
    pending: Vec<u8>,
    /// Whether we're currently inside a ``` fenced code block.
    in_code_fence: bool,
}

impl MdTermState {
    fn new() -> Self {
        MdTermState {
            pending: Vec::with_capacity(256),
            in_code_fence: false,
        }
    }
}

/// Upper bound for a held-back partial line. A "line" longer than this
/// without a '\n' is not prose we can usefully wait for: flush it raw.
const MAX_PENDING: usize = 64 * 1024;

// ---------------------------------------------------------------------
// ANSI SGR helpers
// ---------------------------------------------------------------------

const RESET: &str = "\x1b[0m";
const BOLD_ON: &str = "\x1b[1m";
const BOLD_OFF: &str = "\x1b[22m";
const ITALIC_ON: &str = "\x1b[3m";
const ITALIC_OFF: &str = "\x1b[23m";
const DIM_ON: &str = "\x1b[2m";
const HEADING: &str = "\x1b[1;33m"; // h1: bold + yellow

/// SGR style for a heading of the given level (1..=6).
fn heading_style(level: usize) -> &'static str {
    match level {
        1 => HEADING,          // bold + yellow
        2 => "\x1b[1;35m",     // bold + pink
        3 => "\x1b[1;32m",     // bold + green
        4 => "\x1b[1;36m",     // bold + cyan
        5 => "\x1b[1;31m",     // bold + red
        _ => "\x1b[1;38;5;250m", // h6: bold + light gray
    }
}
const CODE_INLINE: &str = "\x1b[96m"; // bright cyan
const CODE_BLOCK: &str = "\x1b[0;96m"; // fenced code: normal weight, bright cyan
const CODE_INLINE_OFF: &str = "\x1b[39m";
const BULLET_COLOR: &str = "\x1b[32m"; // green
const QUOTE_STYLE: &str = "\x1b[2;3m"; // dim + italic

// ---------------------------------------------------------------------
// Lazily-compiled regexes (compiled once, reused for every line)
// ---------------------------------------------------------------------

fn re_fence() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"^(\s*)```").unwrap())
}
fn re_heading() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"^(\s*)(#{1,6})\s+(.*)$").unwrap())
}
fn re_bullet() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"^(\s*)([-*+])\s+(.*)$").unwrap())
}
fn re_quote() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"^(\s*)>\s?(.*)$").unwrap())
}
fn re_hr() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"^\s*(-{3,}|\*{3,}|_{3,})\s*$").unwrap())
}
fn re_inline_code() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"`([^`]+?)`").unwrap())
}
fn re_bold() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"\*\*(.+?)\*\*|__(.+?)__").unwrap())
}
fn re_italic() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    // deliberately run *after* re_bold, so lone `*`/`_` left over from
    // bold matches don't get mis-paired.
    RE.get_or_init(|| Regex::new(r"\*([^*]+?)\*|_([^_]+?)_").unwrap())
}

/// `_emphasis_` only counts at word boundaries (CommonMark rule), so that
/// `snake_case_names` and `my_file_name.txt` are left alone.
fn underscore_ok(hay: &str, m: &regex::Match) -> bool {
    let before = hay[..m.start()].chars().next_back();
    let after = hay[m.end()..].chars().next();
    let is_word = |c: char| c.is_alphanumeric() || c == '_';
    !before.map_or(false, is_word) && !after.map_or(false, is_word)
}

/// Apply inline-level markdown (bold / italic / code) to a plain text
/// fragment. Order matters: code first (so its contents are frozen and
/// never re-interpreted), then bold, then italic.
fn apply_inline(text: &str) -> String {
    let with_code = re_inline_code()
        .replace_all(text, |caps: &regex::Captures| {
            format!("{CODE_INLINE}{}{CODE_INLINE_OFF}", &caps[1])
        })
        .into_owned();

    let with_bold = re_bold()
        .replace_all(&with_code, |caps: &regex::Captures| {
            let m = caps.get(0).unwrap();
            if caps.get(2).is_some() && !underscore_ok(&with_code, &m) {
                return m.as_str().to_string();
            }
            let inner = caps.get(1).or_else(|| caps.get(2)).unwrap().as_str();
            format!("{BOLD_ON}{inner}{BOLD_OFF}")
        })
        .into_owned();

    re_italic()
        .replace_all(&with_bold, |caps: &regex::Captures| {
            let m = caps.get(0).unwrap();
            if caps.get(2).is_some() && !underscore_ok(&with_bold, &m) {
                return m.as_str().to_string();
            }
            let inner = caps.get(1).or_else(|| caps.get(2)).unwrap().as_str();
            format!("{ITALIC_ON}{inner}{ITALIC_OFF}")
        })
        .into_owned()
}

/// Length of a leading run of terminal control noise: bare '\r' and complete
/// CSI (`ESC [ ... final`) / OSC (`ESC ] ... BEL|ST`) sequences. Bash, for
/// example, sends `ESC[?2004l` right before the first line of every command's
/// output, on the *same* line, so the markdown must be looked for after it.
fn control_prefix_len(s: &str) -> usize {
    let b = s.as_bytes();
    let mut i = 0;
    loop {
        match b.get(i) {
            Some(b'\r') => i += 1,
            Some(0x1b) => match b.get(i + 1) {
                Some(b'[') => {
                    let mut j = i + 2;
                    while j < b.len() && (0x20..=0x3f).contains(&b[j]) {
                        j += 1;
                    }
                    if j < b.len() && (0x40..=0x7e).contains(&b[j]) {
                        i = j + 1;
                    } else {
                        return i;
                    }
                }
                Some(b']') => {
                    let mut j = i + 2;
                    loop {
                        match b.get(j) {
                            Some(0x07) => {
                                j += 1;
                                break;
                            }
                            Some(0x1b) if b.get(j + 1) == Some(&b'\\') => {
                                j += 2;
                                break;
                            }
                            Some(_) => j += 1,
                            None => return i,
                        }
                    }
                    i = j;
                }
                _ => return i,
            },
            _ => return i,
        }
    }
}

/// Convert one line (may or may not end in '\n') to its ANSI-decorated
/// form. `in_code_fence` is threaded through and updated in place.
fn convert_line(line: &str, in_code_fence: &mut bool) -> String {
    let (body, had_newline) = match line.strip_suffix('\n') {
        Some(b) => (b, true),
        None => (line, false),
    };
    // The tty layer (ONLCR) turns every '\n' into "\r\n" on its way to
    // xterm. That '\r' must survive, otherwise every line starts where the
    // previous one ended ("staircase" output).
    let (body, had_cr) = match body.strip_suffix('\r') {
        Some(b) => (b, true),
        None => (body, false),
    };

    // Skip leading control noise (see control_prefix_len), then: a line that
    // still carries escape sequences (ls --color, prompts, TUI programs, ...)
    // is not markdown prose, so leave it byte-for-byte alone.
    let (prefix, body) = body.split_at(control_prefix_len(body));
    if body.contains('\x1b') {
        return line.to_string();
    }

    let rendered = if re_fence().is_match(body) {
        // Fence delimiter lines (``` / ```lang) are markup only: toggle the
        // state and print nothing for them (only any leading control noise).
        *in_code_fence = !*in_code_fence;
        return prefix.to_string();
    } else if *in_code_fence {
        // Inside a fenced block: text is passed through verbatim (no
        // markdown inside), plain weight, cyan.
        format!("{CODE_BLOCK}{body}{RESET}")
    } else if let Some(caps) = re_heading().captures(body) {
        let indent = &caps[1];
        let style = heading_style(caps[2].len());
        let text = apply_inline(&caps[3]);
        format!("{indent}{style}{text}{RESET}")
    } else if re_hr().is_match(body) {
        format!("{DIM_ON}{body}{RESET}")
    } else if let Some(caps) = re_quote().captures(body) {
        let indent = &caps[1];
        // The quote is dimmed; inline code inside it must stay bright cyan,
        // so switch dim off around it and back on after.
        let text = apply_inline(&caps[2])
            .replace(CODE_INLINE, "\x1b[22;96m")
            .replace(CODE_INLINE_OFF, "\x1b[39;2m");
        format!("{indent}{QUOTE_STYLE}\u{2503} {text}{RESET}")
    } else if let Some(caps) = re_bullet().captures(body) {
        let indent = &caps[1];
        let text = apply_inline(&caps[3]);
        format!("{indent}{BULLET_COLOR}\u{2022}{RESET} {text}")
    } else {
        apply_inline(body)
    };

    let mut out = format!("{prefix}{rendered}");
    if had_cr {
        out.push('\r');
    }
    if had_newline {
        out.push('\n');
    }
    out
}

/// Feed new bytes into `state`, returning converted output for every
/// complete line now available, or `None` if we're still waiting on a
/// terminating '\n'.
fn feed_bytes(state: &mut MdTermState, input: &[u8]) -> Option<Vec<u8>> {
    state.pending.extend_from_slice(input);

    let mut output: Vec<u8> = Vec::new();
    let mut produced_anything = false;

    loop {
        let newline_pos = state.pending.iter().position(|&b| b == b'\n');
        let Some(pos) = newline_pos else { break };

        let line_bytes: Vec<u8> = state.pending.drain(..=pos).collect();
        // Lossy is deliberate: a genuinely malformed byte shouldn't wedge
        // the whole terminal, it just renders as U+FFFD like everywhere else.
        let line_str = String::from_utf8_lossy(&line_bytes);
        let converted = convert_line(&line_str, &mut state.in_code_fence);
        output.extend_from_slice(converted.as_bytes());
        produced_anything = true;
    }

    if state.pending.len() > MAX_PENDING {
        output.extend(state.pending.drain(..));
        produced_anything = true;
    }

    if produced_anything {
        Some(output)
    } else {
        None
    }
}

// ---------------------------------------------------------------------
// C ABI
// ---------------------------------------------------------------------

#[no_mangle]
pub extern "C" fn mdterm_new() -> *mut c_void {
    let state = Box::new(MdTermState::new());
    Box::into_raw(state) as *mut c_void
}

#[no_mangle]
pub extern "C" fn mdterm_feed(
    state: *mut c_void,
    input: *const u8,
    input_len: usize,
    out_len: *mut usize,
) -> *mut u8 {
    if state.is_null() || out_len.is_null() {
        return ptr::null_mut();
    }

    // SAFETY: `state` was created by mdterm_new() and cast back to the
    // same type; caller (ptydata.c) never touches it directly.
    let state_ref = unsafe { &mut *(state as *mut MdTermState) };

    let input_slice: &[u8] = if input.is_null() || input_len == 0 {
        &[]
    } else {
        // SAFETY: caller guarantees `input` points at `input_len` valid
        // bytes for the duration of this call (it's xterm's own PTY
        // read buffer, not retained past the call).
        unsafe { slice::from_raw_parts(input, input_len) }
    };

    match feed_bytes(state_ref, input_slice) {
        Some(output) if !output.is_empty() => {
            let mut boxed = output.into_boxed_slice();
            let len = boxed.len();
            let data_ptr = boxed.as_mut_ptr();
            std::mem::forget(boxed);
            // SAFETY: out_len checked non-null above.
            unsafe {
                *out_len = len;
            }
            data_ptr
        }
        _ => {
            unsafe {
                *out_len = 0;
            }
            ptr::null_mut()
        }
    }
}

/// Number of bytes currently held back (partial line, no '\n' yet).
#[no_mangle]
pub extern "C" fn mdterm_pending_len(state: *const c_void) -> usize {
    if state.is_null() {
        return 0;
    }
    // SAFETY: `state` came from mdterm_new().
    unsafe { (*(state as *const MdTermState)).pending.len() }
}

/// Hand back the held-back partial line *unconverted* and clear it.
/// Used by the C side when no '\n' arrives within a short timeout
/// (shell prompts, echoed keystrokes, ...). Returns NULL / *out_len = 0
/// if nothing is pending. Release with mdterm_free_buf().
#[no_mangle]
pub extern "C" fn mdterm_flush(state: *mut c_void, out_len: *mut usize) -> *mut u8 {
    if state.is_null() || out_len.is_null() {
        return ptr::null_mut();
    }
    // SAFETY: `state` came from mdterm_new().
    let state_ref = unsafe { &mut *(state as *mut MdTermState) };
    if state_ref.pending.is_empty() {
        unsafe { *out_len = 0 };
        return ptr::null_mut();
    }
    let mut boxed = std::mem::take(&mut state_ref.pending).into_boxed_slice();
    let len = boxed.len();
    let data_ptr = boxed.as_mut_ptr();
    std::mem::forget(boxed);
    unsafe { *out_len = len };
    data_ptr
}

#[no_mangle]
pub extern "C" fn mdterm_free_buf(ptr_in: *mut u8, len: usize) {
    if ptr_in.is_null() {
        return;
    }
    // SAFETY: only ever called with a (ptr, len) pair previously
    // returned from mdterm_feed(), which built it the same way.
    unsafe {
        let slice_ptr: *mut [u8] = slice::from_raw_parts_mut(ptr_in, len);
        drop(Box::from_raw(slice_ptr));
    }
}

#[no_mangle]
pub extern "C" fn mdterm_free(state: *mut c_void) {
    if state.is_null() {
        return;
    }
    // SAFETY: only ever called with a pointer previously returned from
    // mdterm_new().
    unsafe {
        drop(Box::from_raw(state as *mut MdTermState));
    }
}

// ---------------------------------------------------------------------
// Tests (run on the host with `cargo test`, no xterm/C needed)
// ---------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bold_and_italic() {
        let mut fence = false;
        let out = convert_line("this is **bold** and *italic*\n", &mut fence);
        assert!(out.contains(BOLD_ON) && out.contains(ITALIC_ON));
    }

    #[test]
    fn heading() {
        let mut fence = false;
        let out = convert_line("# Title\n", &mut fence);
        assert!(out.contains(HEADING));
        assert!(!out.contains('#'));
    }

    #[test]
    fn partial_line_buffers_until_newline() {
        let mut state = MdTermState::new();
        assert!(feed_bytes(&mut state, b"**bo").is_none());
        let out = feed_bytes(&mut state, b"ld**\n").unwrap();
        let s = String::from_utf8(out).unwrap();
        assert!(s.contains(BOLD_ON));
    }

    #[test]
    fn code_fence_passes_through_unstyled_content() {
        let mut state = MdTermState::new();
        let a = feed_bytes(&mut state, b"```rust\n").unwrap();
        let b = feed_bytes(&mut state, b"let x = 1;\n").unwrap();
        let c = feed_bytes(&mut state, b"```\n").unwrap();
        let joined = [a, b, c].concat();
        let s = String::from_utf8(joined).unwrap();
        // code content itself must be untouched (no bold applied to `x`, etc.)
        assert!(s.contains("let x = 1;"));
        // the ``` delimiter lines are hidden
        assert!(!s.contains("```"));
        assert!(!s.contains("rust"));
    }

    #[test]
    fn list_bullet() {
        let mut fence = false;
        let out = convert_line("- item one\n", &mut fence);
        assert!(out.contains('\u{2022}'));
    }

    #[test]
    fn escape_lines_pass_through_untouched() {
        let mut fence = false;
        let line = "\x1b[32mmy_file_name*.c\x1b[0m\n";
        assert_eq!(convert_line(line, &mut fence), line);
    }

    #[test]
    fn flush_returns_raw_pending() {
        let mut state = MdTermState::new();
        assert!(feed_bytes(&mut state, b"user@host:~$ ").is_none());
        assert_eq!(state.pending, b"user@host:~$ ");
    }

    #[test]
    fn crlf_is_preserved() {
        let mut fence = false;
        let out = convert_line("**x**\r\n", &mut fence);
        assert!(out.ends_with("\r\n"));
        let out = convert_line("plain\r\n", &mut fence);
        assert_eq!(out, "plain\r\n");
    }

    #[test]
    fn intraword_underscores_untouched() {
        let mut fence = false;
        let out = convert_line("see my_file_name.txt and _real_ italic\n", &mut fence);
        assert!(out.contains("my_file_name.txt"));
        assert!(out.contains(&format!("{ITALIC_ON}real{ITALIC_OFF}")));
    }

    #[test]
    fn heading_after_bash_bracketed_paste_prefix() {
        let mut fence = false;
        let out = convert_line("\x1b[?2004l\r# Title\r\n", &mut fence);
        assert!(out.starts_with("\x1b[?2004l\r"));
        assert!(out.contains(HEADING));
        assert!(!out.contains('#'));
        assert!(out.ends_with("\r\n"));
    }

    #[test]
    fn heading_levels_have_distinct_styles() {
        let mut seen = std::collections::HashSet::new();
        for level in 1..=6 {
            let mut fence = false;
            let line = format!("{} Title\n", "#".repeat(level));
            let out = convert_line(&line, &mut fence);
            assert!(!out.contains('#'));
            assert!(seen.insert(heading_style(level)));
            assert!(out.contains(heading_style(level)));
        }
    }

    #[test]
    fn code_block_is_cyan_plain_and_verbatim() {
        let mut fence = false;
        assert_eq!(convert_line("```python\n", &mut fence), "");
        assert!(fence);
        let out = convert_line("print(\"**x**\") # y\n", &mut fence);
        assert_eq!(out, format!("{CODE_BLOCK}print(\"**x**\") # y{RESET}\n"));
        assert!(!out.contains("\x1b[1m"));
        assert_eq!(convert_line("```\r\n", &mut fence), "");
        assert!(!fence);
    }
}
