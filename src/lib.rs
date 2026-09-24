//! mdterm_bridge
//!
//! A streaming markdown -> ANSI converter, exposed as a `staticlib` with a
//! plain C ABI so it can be linked straight into xterm (see
//! `mdterm_bridge.h` and `xterm-411-mdterm.patch` next to this file).
//!
//! How it works
//! ------------
//! * xterm hands us whatever `read()` returned from the PTY. Bytes are held
//!   until a full '\n'-terminated line is available, so markdown that spans a
//!   read() boundary is still parsed correctly (`\n` never occurs inside a
//!   UTF-8 sequence).
//! * Programs like ollama word-wrap their own output ("step back over the
//!   last word, erase, newline, print the word again"). Those *soft* wraps are
//!   detected and the physical lines are joined again into one logical line,
//!   so `**bold**` or a table row is never torn apart. We then do our own
//!   word wrapping to the real terminal width.
//! * Block level: headings, fenced code (nested), block quotes, lists
//!   (bullet / numbered / task), horizontal rules and pipe tables (buffered
//!   until the table ends, then drawn with box characters).
//! * Inline level: `code`, **bold**, *italic*, ***both***, ~~strike~~,
//!   [links](url), ![images](url), <autolinks>, bare URLs, backslash escapes.
//! * Lines that move the cursor (progress bars, editors, prompts redrawing)
//!   are not prose and are passed through byte for byte.

use regex::{Captures, Regex};
use std::os::raw::c_void;
use std::sync::OnceLock;
use std::time::{Duration, Instant};
use std::{ptr, slice};
use unicode_width::UnicodeWidthChar;

// ---------------------------------------------------------------------
// Tunables
// ---------------------------------------------------------------------

/// A held partial line longer than this is not prose worth waiting for.
const MAX_PENDING: usize = 64 * 1024;
/// Safety valve: render a table that grows beyond this many rows.
const MAX_TABLE_ROWS: usize = 400;
/// Two '\n' arriving further apart than this count as "slow streaming".
const SLOW_LINE_GAP: Duration = Duration::from_millis(300);
/// This many slow lines in a row means an LLM-style stream is running.
const SESSION_LINES: u32 = 3;
/// ... and it stays "running" this long after the last '\n'.
const SESSION_TTL: Duration = Duration::from_secs(45);

// ---------------------------------------------------------------------
// ANSI SGR helpers
// ---------------------------------------------------------------------

const RESET: &str = "\x1b[0m";
const BOLD_ON: &str = "\x1b[1m";
const BOLD_OFF: &str = "\x1b[22m";
const ITALIC_ON: &str = "\x1b[3m";
const ITALIC_OFF: &str = "\x1b[23m";
const STRIKE_ON: &str = "\x1b[9m";
const STRIKE_OFF: &str = "\x1b[29m";
const DIM_ON: &str = "\x1b[2m";
const DIM_OFF: &str = "\x1b[22m";
const HEADING: &str = "\x1b[1;33m"; // h1: bold + yellow
const CODE_INLINE: &str = "\x1b[96m"; // bright cyan
const CODE_INLINE_OFF: &str = "\x1b[39m";
const CODE_BLOCK: &str = "\x1b[0;96m"; // fenced code: normal weight, bright cyan
const BULLET_COLOR: &str = "\x1b[32m"; // green
const QUOTE_STYLE: &str = "\x1b[2;3m"; // dim + italic
const LINK_ON: &str = "\x1b[4;94m"; // underline + bright blue
const LINK_OFF: &str = "\x1b[24;39m";
const IMAGE_ON: &str = "\x1b[95m"; // bright magenta
const IMAGE_OFF: &str = "\x1b[39m";

/// SGR style for a heading of the given level (1..=6).
fn heading_style(level: usize) -> &'static str {
    match level {
        1 => HEADING,               // bold + yellow
        2 => "\x1b[1;35m",          // bold + pink
        3 => "\x1b[1;32m",          // bold + green
        4 => "\x1b[1;36m",          // bold + cyan
        5 => "\x1b[1;31m",          // bold + red
        _ => "\x1b[1;38;5;250m",    // h6: bold + light gray
    }
}

// ---------------------------------------------------------------------
// Lazily-compiled regexes
// ---------------------------------------------------------------------

macro_rules! lazy_re {
    ($name:ident, $pat:expr) => {
        fn $name() -> &'static Regex {
            static RE: OnceLock<Regex> = OnceLock::new();
            RE.get_or_init(|| Regex::new($pat).unwrap())
        }
    };
}

lazy_re!(re_fence_line, r"^\s*(`{3,}|~{3,})(.*)$");
lazy_re!(re_heading, r"^(\s*)(#{1,6})\s+(.*)$");
lazy_re!(re_hr, r"^\s*(-{3,}|\*{3,}|_{3,})\s*$");
lazy_re!(re_quote, r"^(\s*)((?:>[ \t]?)+)(.*)$");
lazy_re!(re_list, r"^(\s*)([-*+]|\d{1,9}[.)])\s+(.*)$");
lazy_re!(re_table_row, r"^\s*\|.*\|\s*$");
lazy_re!(re_sep_cell, r"^\s*:?-+:?\s*$");
lazy_re!(re_br, r"(?i)<br\s*/?>");
lazy_re!(re_bold3, r"\*\*\*(.+?)\*\*\*");
lazy_re!(re_bold, r"\*\*(.+?)\*\*|__(.+?)__");
lazy_re!(re_italic, r"\*([^*]+?)\*|_([^_]+?)_");
lazy_re!(re_strike, r"~~(.+?)~~");
lazy_re!(re_image, r#"!\[([^\]]*)\]\(\s*([^)\s]+)(?:\s+"[^"]*")?\s*\)"#);
lazy_re!(re_link, r#"\[([^\]]+)\]\(\s*([^)\s]+)(?:\s+"[^"]*")?\s*\)"#);
lazy_re!(re_autolink, r"<((?:https?|ftp)://[^>\s]+)>");
lazy_re!(re_bare_url, r#"(?:https?|ftp)://[^\s<>\[\]()"']+"#);
// ollama's own word wrap: "step back over the current word, clear the rest
// of the line, newline, print the word again".
lazy_re!(re_wrap_tail, r"\x1b\[(\d+)D\x1b\[0?K$");
lazy_re!(re_erase_tail, r"\x1b\[0?K$");

// ---------------------------------------------------------------------
// Escape sequences and terminal text measurement
// ---------------------------------------------------------------------

/// Private-use characters are used as placeholders while text is being
/// rewritten. Real text containing them (Nerd-font prompt glyphs) is never
/// touched.
fn is_pua(c: char) -> bool {
    ('\u{E000}'..='\u{F8FF}').contains(&c)
}

/// Parse one escape sequence at the start of `b` (which begins with ESC).
/// Returns (length, zero_width_safe). `None` = incomplete / malformed.
/// "Safe" = it changes only colours/modes or sets a title, never the cursor
/// position, so it can be moved around while markdown markers are rewritten.
fn parse_escape(b: &[u8]) -> Option<(usize, bool)> {
    match b.get(1)? {
        b'[' => {
            let mut j = 2;
            while j < b.len() && (0x20..=0x3f).contains(&b[j]) {
                j += 1;
            }
            let fin = *b.get(j)?;
            if !(0x40..=0x7e).contains(&fin) {
                return None;
            }
            let params = &b[2..j];
            let safe =
                fin == b'm' || (params.first() == Some(&b'?') && (fin == b'h' || fin == b'l'));
            Some((j + 1, safe))
        }
        b']' => {
            let mut j = 2;
            loop {
                match b.get(j)? {
                    0x07 => return Some((j + 1, true)),
                    0x1b if b.get(j + 1) == Some(&b'\\') => return Some((j + 2, true)),
                    _ => j += 1,
                }
            }
        }
        _ => None,
    }
}

/// Does `bytes` contain something that moves the cursor / rewrites the line
/// (CR, BS, cursor-movement or erase sequences, ...)? That is what prompts,
/// echoed keystrokes, editors and progress bars look like.
fn has_unsafe_control(bytes: &[u8]) -> bool {
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            0x1b => match parse_escape(&bytes[i..]) {
                Some((len, true)) => i += len,
                Some((_, false)) => return true,
                None => {
                    // incomplete sequence at the very end: still arriving
                    if bytes[i..].len() < 32 && !bytes[i + 1..].contains(&0x1b) {
                        i += 1;
                    } else {
                        return true;
                    }
                }
            },
            b'\r' | 0x08 | 0x00..=0x06 | 0x0b | 0x0c | 0x0e..=0x1a | 0x1c..=0x1f | 0x7f => {
                return true
            }
            _ => i += 1,
        }
    }
    false
}

/// Length of a leading run of terminal control noise: bare '\r' and complete
/// CSI / OSC sequences. Bash, for example, sends `ESC[?2004l` right before
/// the first line of every command's output, on the *same* line.
fn control_prefix_len(s: &str) -> usize {
    let b = s.as_bytes();
    let mut i = 0;
    loop {
        match b.get(i) {
            Some(b'\r') => i += 1,
            Some(0x1b) => match parse_escape(&b[i..]) {
                Some((len, _)) => i += len,
                None => return i,
            },
            _ => return i,
        }
    }
}

/// Replace every (safe) escape sequence by a private-use placeholder char so
/// the markdown regexes see plain text. `None` if the line holds anything
/// unsafe, or private-use characters that would collide with placeholders.
fn extract_escapes(body: &str) -> Option<(String, Vec<String>)> {
    if body.chars().any(is_pua) {
        return None;
    }
    let b = body.as_bytes();
    let mut plain = String::with_capacity(body.len());
    let mut escapes: Vec<String> = Vec::new();
    let mut i = 0;
    let mut text_start = 0;
    while i < b.len() {
        match b[i] {
            0x1b => {
                let (len, safe) = parse_escape(&b[i..])?;
                if !safe || escapes.len() >= 0x1000 {
                    return None;
                }
                plain.push_str(&body[text_start..i]);
                plain.push(char::from_u32(0xE000 + escapes.len() as u32)?);
                escapes.push(body[i..i + len].to_string());
                i += len;
                text_start = i;
            }
            b'\t' => i += 1,
            0x00..=0x1f | 0x7f => return None,
            _ => i += 1,
        }
    }
    plain.push_str(&body[text_start..]);
    Some((plain, escapes))
}

fn restore_escapes(text: &str, escapes: &[String]) -> String {
    if escapes.is_empty() {
        return text.to_string();
    }
    let mut out = String::with_capacity(text.len() + escapes.len() * 6);
    for c in text.chars() {
        let cp = c as u32;
        if (0xE000..0xE000 + escapes.len() as u32).contains(&cp) {
            out.push_str(&escapes[(cp - 0xE000) as usize]);
        } else {
            out.push(c);
        }
    }
    out
}

/// Escape sequences that do nothing visible when they come in a pair:
/// "hide cursor, show cursor". ollama emits one after every token.
fn drop_noise(body: &str) -> String {
    body.replace("\x1b[?25l\x1b[?25h", "")
}

#[derive(PartialEq, Clone, Copy)]
enum Tail {
    None,
    /// `ESC[nD ESC[K`: the last n columns are re-printed on the next line
    WrapBack(usize),
    /// bare `ESC[K` before the newline
    Erase,
}

fn split_tail(body: &str) -> (&str, Tail) {
    if let Some(caps) = re_wrap_tail().captures(body) {
        let start = caps.get(0).map_or(body.len(), |m| m.start());
        let n = caps[1].parse().unwrap_or(0);
        return (&body[..start], Tail::WrapBack(n));
    }
    if let Some(m) = re_erase_tail().find(body) {
        return (&body[..m.start()], Tail::Erase);
    }
    (body, Tail::None)
}

/// Display width of `s`, ignoring escape sequences and placeholders.
fn visible_width(s: &str) -> usize {
    let bytes = s.as_bytes();
    let mut w = 0;
    let mut skip_until = 0;
    for (i, c) in s.char_indices() {
        if i < skip_until {
            continue;
        }
        if c == '\x1b' {
            if let Some((len, _)) = parse_escape(&bytes[i..]) {
                skip_until = i + len;
                continue;
            }
        }
        if !is_pua(c) {
            w += UnicodeWidthChar::width(c).unwrap_or(0);
        }
    }
    w
}

/// Drop the last `cols` display columns of `plain` (text with escape
/// placeholders). Placeholders have zero width; the ones that fall into the
/// dropped region are kept (at the end) so colour state is not lost.
fn truncate_cols_keep_escapes(plain: &str, cols: usize) -> String {
    let mut chars: Vec<char> = plain.chars().collect();
    let mut dropped: Vec<char> = Vec::new();
    let mut left = cols;
    while left > 0 {
        match chars.pop() {
            None => break,
            Some(c) if is_pua(c) => dropped.push(c),
            Some(c) => left = left.saturating_sub(UnicodeWidthChar::width(c).unwrap_or(0)),
        }
    }
    dropped.reverse();
    chars.extend(dropped);
    chars.into_iter().collect()
}

/// Split one over-long word into pieces of at most `width` columns.
fn hard_split(word: &str, width: usize) -> Vec<String> {
    let bytes = word.as_bytes();
    let mut parts = Vec::new();
    let mut cur = String::new();
    let mut cw = 0;
    let mut i = 0;
    while i < word.len() {
        let c = word[i..].chars().next().unwrap();
        if c == '\x1b' {
            if let Some((len, _)) = parse_escape(&bytes[i..]) {
                cur.push_str(&word[i..i + len]);
                i += len;
                continue;
            }
        }
        let cwid = if is_pua(c) { 0 } else { UnicodeWidthChar::width(c).unwrap_or(0) };
        if cw + cwid > width && cw > 0 {
            parts.push(std::mem::take(&mut cur));
            cw = 0;
        }
        cur.push(c);
        cw += cwid;
        i += c.len_utf8();
    }
    if !cur.is_empty() {
        parts.push(cur);
    }
    parts
}

/// Greedy word wrap of `text` (which may contain escape sequences) to
/// `width` columns. With `hard`, words longer than the width are split too.
fn wrap_visible(text: &str, width: usize, hard: bool) -> Vec<String> {
    if width == 0 {
        return vec![text.to_string()];
    }
    let bytes = text.as_bytes();
    let mut words: Vec<(String, usize)> = Vec::new();
    let mut cur = String::new();
    let mut cur_w = 0;
    let mut i = 0;
    while i < text.len() {
        let c = text[i..].chars().next().unwrap();
        if c == '\x1b' {
            if let Some((len, _)) = parse_escape(&bytes[i..]) {
                cur.push_str(&text[i..i + len]);
                i += len;
                continue;
            }
        }
        i += c.len_utf8();
        if c == ' ' {
            if !cur.is_empty() {
                words.push((std::mem::take(&mut cur), cur_w));
                cur_w = 0;
            }
            continue;
        }
        cur.push(c);
        if !is_pua(c) {
            cur_w += UnicodeWidthChar::width(c).unwrap_or(0);
        }
    }
    if !cur.is_empty() {
        words.push((cur, cur_w));
    }

    let mut lines: Vec<String> = Vec::new();
    let mut line = String::new();
    let mut lw = 0;
    for (w, ww) in words {
        if hard && ww > width {
            if lw > 0 {
                lines.push(std::mem::take(&mut line));
                lw = 0;
            }
            let parts = hard_split(&w, width);
            let n = parts.len();
            for (k, p) in parts.into_iter().enumerate() {
                if k + 1 == n {
                    lw = visible_width(&p);
                    line = p;
                } else {
                    lines.push(p);
                }
            }
            continue;
        }
        if lw == 0 {
            line = w;
            lw = ww;
        } else if lw + 1 + ww <= width {
            line.push(' ');
            line.push_str(&w);
            lw += 1 + ww;
        } else {
            lines.push(std::mem::take(&mut line));
            line = w;
            lw = ww;
        }
    }
    if !line.is_empty() || lines.is_empty() {
        lines.push(line);
    }
    lines
}

/// Pad `s` with spaces to `w` visible columns according to `align`.
fn pad_cell(s: &str, w: usize, align: Align) -> String {
    let vw = visible_width(s);
    let gap = w.saturating_sub(vw);
    match align {
        Align::Left => format!("{s}{}", " ".repeat(gap)),
        Align::Right => format!("{}{s}", " ".repeat(gap)),
        Align::Center => {
            let l = gap / 2;
            format!("{}{s}{}", " ".repeat(l), " ".repeat(gap - l))
        }
    }
}

#[derive(Clone, Copy, PartialEq, Debug)]
enum Align {
    Left,
    Center,
    Right,
}

// ---------------------------------------------------------------------
// Inline markdown
// ---------------------------------------------------------------------
//
// Spans that must not be re-interpreted (code, link targets, ...) are
// replaced by private-use placeholders U+F000.. while emphasis is applied,
// and put back at the end. Backslash-escaped punctuation becomes
// U+F800 + (char - 0x21).

const SPAN_MAX: usize = 0x800;

fn push_span(spans: &mut Vec<String>, s: String) -> Option<char> {
    if spans.len() >= SPAN_MAX {
        return None;
    }
    spans.push(s);
    char::from_u32(0xF000 + spans.len() as u32 - 1)
}

/// Put every mask back. Replacements may themselves contain placeholders
/// (a link whose text holds code), so repeat a few times.
fn unmask(s: &str, spans: &[String]) -> String {
    let mut cur = s.to_string();
    for _ in 0..8 {
        if !cur.chars().any(|c| ('\u{F000}'..='\u{F8FF}').contains(&c)) {
            break;
        }
        let mut next = String::with_capacity(cur.len() + 16);
        for c in cur.chars() {
            let cp = c as u32;
            if (0xF000..0xF800).contains(&cp) {
                match spans.get((cp - 0xF000) as usize) {
                    Some(t) => next.push_str(t),
                    None => next.push(c),
                }
            } else if (0xF800..0xF800 + 94).contains(&cp) {
                next.push(char::from_u32(0x21 + cp - 0xF800).unwrap_or(c));
            } else {
                next.push(c);
            }
        }
        cur = next;
    }
    cur
}

/// Mask backslash escapes and code spans. Handles CommonMark's variable
/// length backtick fences (`` `a` ``, ``` `` a`b `` ```).
fn mask_code_and_escapes(text: &str, spans: &mut Vec<String>) -> String {
    let chars: Vec<char> = text.chars().collect();
    let len = chars.len();
    let mut out = String::with_capacity(text.len());
    let mut i = 0;
    while i < len {
        let c = chars[i];
        if c == '\\' && i + 1 < len && chars[i + 1].is_ascii_punctuation() {
            let p = chars[i + 1] as u32;
            out.push(char::from_u32(0xF800 + p - 0x21).unwrap());
            i += 2;
            continue;
        }
        if c != '`' {
            out.push(c);
            i += 1;
            continue;
        }
        let mut j = i;
        while j < len && chars[j] == '`' {
            j += 1;
        }
        let n = j - i;
        // the closing run must have exactly the same length
        let mut k = j;
        let mut close = None;
        while k < len {
            if chars[k] == '`' {
                let mut m = k;
                while m < len && chars[m] == '`' {
                    m += 1;
                }
                if m - k == n {
                    close = Some((k, m));
                    break;
                }
                k = m;
            } else {
                k += 1;
            }
        }
        let mut done = false;
        if let Some((k, m)) = close {
            let mut content: String = chars[j..k].iter().collect();
            if content.len() >= 2
                && content.starts_with(' ')
                && content.ends_with(' ')
                && !content.trim().is_empty()
            {
                content = content[1..content.len() - 1].to_string();
            }
            if let Some(ph) = push_span(spans, format!("{CODE_INLINE}{content}{CODE_INLINE_OFF}")) {
                out.push(ph);
                i = m;
                done = true;
            }
        }
        if !done {
            // unmatched backticks are literal text
            out.extend(chars[i..j].iter());
            i = j;
        }
    }
    out
}

fn dim_url(url: &str) -> String {
    format!("{DIM_ON}({url}){DIM_OFF}")
}

/// Images, links, autolinks and bare URLs. The link *text* keeps being
/// processed (bold inside a link works); the target is frozen.
fn mask_links(text: &str, spans: &mut Vec<String>) -> String {
    // images first: ![alt](url)
    let s = re_image()
        .replace_all(text, |c: &Captures| {
            let alt = if c[1].trim().is_empty() { "image" } else { c[1].trim() };
            let rep = format!("{IMAGE_ON}[{alt}]{IMAGE_OFF} {}", dim_url(&c[2]));
            match push_span(spans, rep) {
                Some(ph) => ph.to_string(),
                None => c[0].to_string(),
            }
        })
        .into_owned();
    // [text](url)
    let s = re_link()
        .replace_all(&s, |c: &Captures| {
            let inner = inline_masked(&c[1], spans);
            let mut rep = format!("{LINK_ON}{inner}{LINK_OFF}");
            if c[1].trim() != &c[2] {
                rep.push(' ');
                rep.push_str(&dim_url(&c[2]));
            }
            match push_span(spans, rep) {
                Some(ph) => ph.to_string(),
                None => c[0].to_string(),
            }
        })
        .into_owned();
    // <https://...>
    let s = re_autolink()
        .replace_all(&s, |c: &Captures| {
            match push_span(spans, format!("{LINK_ON}{}{LINK_OFF}", &c[1])) {
                Some(ph) => ph.to_string(),
                None => c[0].to_string(),
            }
        })
        .into_owned();
    // bare URLs
    re_bare_url()
        .replace_all(&s, |c: &Captures| {
            let full = &c[0];
            let trimmed = full.trim_end_matches(|ch: char| ".,;:!?".contains(ch));
            let tail = &full[trimmed.len()..];
            match push_span(spans, format!("{LINK_ON}{trimmed}{LINK_OFF}")) {
                Some(ph) => format!("{ph}{tail}"),
                None => full.to_string(),
            }
        })
        .into_owned()
}

/// `_emphasis_` only counts at word boundaries (CommonMark rule), so that
/// `snake_case_names` and `my_file_name.txt` are left alone.
fn underscore_ok(hay: &str, m: &regex::Match) -> bool {
    let before = hay[..m.start()].chars().next_back();
    let after = hay[m.end()..].chars().next();
    let is_word = |c: char| c.is_alphanumeric() || c == '_';
    !before.map_or(false, is_word) && !after.map_or(false, is_word)
}

/// An emphasis run must not start or end with whitespace (`a * b * c` is
/// arithmetic, not italics).
fn flank_ok(inner: &str) -> bool {
    !inner.starts_with(char::is_whitespace) && !inner.ends_with(char::is_whitespace)
}

fn emphasis(masked: &str) -> String {
    let s1 = re_bold3()
        .replace_all(masked, |c: &Captures| {
            if !flank_ok(&c[1]) {
                return c[0].to_string();
            }
            format!("{BOLD_ON}{ITALIC_ON}{}{ITALIC_OFF}{BOLD_OFF}", &c[1])
        })
        .into_owned();

    let s2 = re_bold()
        .replace_all(&s1, |c: &Captures| {
            let m = c.get(0).unwrap();
            let inner = c.get(1).or_else(|| c.get(2)).unwrap().as_str();
            if !flank_ok(inner) || (c.get(2).is_some() && !underscore_ok(&s1, &m)) {
                return m.as_str().to_string();
            }
            format!("{BOLD_ON}{inner}{BOLD_OFF}")
        })
        .into_owned();

    let s3 = re_italic()
        .replace_all(&s2, |c: &Captures| {
            let m = c.get(0).unwrap();
            let inner = c.get(1).or_else(|| c.get(2)).unwrap().as_str();
            if !flank_ok(inner) || (c.get(2).is_some() && !underscore_ok(&s2, &m)) {
                return m.as_str().to_string();
            }
            format!("{ITALIC_ON}{inner}{ITALIC_OFF}")
        })
        .into_owned();

    re_strike()
        .replace_all(&s3, |c: &Captures| {
            if !flank_ok(&c[1]) {
                return c[0].to_string();
            }
            format!("{STRIKE_ON}{}{STRIKE_OFF}", &c[1])
        })
        .into_owned()
}

/// Links/emphasis on text whose code spans and escapes are already masked.
fn inline_masked(masked: &str, spans: &mut Vec<String>) -> String {
    let linked = mask_links(masked, spans);
    emphasis(&linked)
}

/// Full inline pass on plain text (no escape sequences).
fn inline(text: &str, spans: &mut Vec<String>) -> String {
    let masked = mask_code_and_escapes(text, spans);
    inline_masked(&masked, spans)
}

/// Apply inline-level markdown to a plain text fragment.
fn apply_inline(text: &str) -> String {
    let mut spans = Vec::new();
    let out = inline(text, &mut spans);
    unmask(&out, &spans)
}

// ---------------------------------------------------------------------
// Converter state
// ---------------------------------------------------------------------

struct Fence {
    ch: char,
    len: usize,
    /// > 1 while inside a nested fence (a ```python block inside a ```markdown one)
    depth: u32,
}

pub struct MdTermState {
    /// Bytes received but not yet part of a complete '\n'-terminated line.
    pending: Vec<u8>,
    /// Currently open fenced code block, if any.
    fence: Option<Fence>,
    /// Text of soft-wrapped physical lines, waiting for the rest of the line.
    carry: String,
    /// Buffered rows of a pipe table that is still being received.
    table: Vec<String>,
    /// Terminal width in columns (0 = unknown: no wrapping, no fitting).
    width: usize,
    /// How many '\n' in a row arrived slowly (LLM-style streaming).
    slow_lines: u32,
    last_nl: Option<Instant>,
}

impl MdTermState {
    fn new() -> Self {
        MdTermState {
            pending: Vec::with_capacity(256),
            fence: None,
            carry: String::new(),
            table: Vec::new(),
            width: 0,
            slow_lines: 0,
            last_nl: None,
        }
    }

    fn note_newline(&mut self) {
        let now = Instant::now();
        match self.last_nl {
            Some(t) => {
                let gap = now.duration_since(t);
                if gap > SESSION_TTL {
                    self.slow_lines = 0;
                } else if gap >= SLOW_LINE_GAP {
                    self.slow_lines = self.slow_lines.saturating_add(1);
                }
            }
            None => {}
        }
        self.last_nl = Some(now);
    }

    fn in_slow_session(&self) -> bool {
        self.slow_lines >= SESSION_LINES
            && self.last_nl.map_or(false, |t| t.elapsed() < SESSION_TTL)
    }

    /// Everything currently held back, in bytes.
    fn held_len(&self) -> usize {
        self.pending.len() + self.carry.len() + self.table.iter().map(|r| r.len() + 1).sum::<usize>()
    }

    /// 1 = looks interactive (prompt, echo, cursor movement): show quickly.
    /// 2 = a slow LLM-style stream is running: wait a long time for the rest.
    /// 0 = anything else.
    fn hold_class(&self) -> i32 {
        if !self.pending.is_empty()
            && (has_unsafe_control(&self.pending) || looks_like_prompt(&self.pending))
        {
            return 1;
        }
        if self.in_slow_session() {
            return 2;
        }
        if self.pending.is_empty() && self.carry.is_empty() && !self.table.is_empty() {
            return 1; // a table that is not being streamed: show it now
        }
        0
    }
}

/// A partial line that looks like a shell / REPL prompt: `$ `, `% `,
/// `user@host:~# `, `foo> `, `>>> `, `❯ `... A bare `# ` or `> ` is NOT a
/// prompt: that is the start of a markdown heading / quote being streamed.
fn looks_like_prompt(p: &[u8]) -> bool {
    if p == b">>> " || p == b"... " {
        return true;
    }
    let n = p.len();
    if n < 2 || p[n - 1] != b' ' {
        return false;
    }
    let head = &p[..n - 2];
    let has_text = head.iter().any(|b| b.is_ascii_alphanumeric() || *b >= 0x80);
    match p[n - 2] {
        b'$' | b'%' => true,
        b'#' | b'>' => has_text,
        _ => p.ends_with("❯ ".as_bytes()) || p.ends_with("» ".as_bytes()),
    }
}

// ---------------------------------------------------------------------
// Tables
// ---------------------------------------------------------------------

const BORDER_ON: &str = "\x1b[2m";
const BORDER_OFF: &str = "\x1b[22m";

struct TableCells {
    /// each cell: its display lines (real escapes, inline markdown applied)
    cells: Vec<Vec<String>>,
}

/// Split a table row into cells. Code spans and `\|` protect their pipes.
/// Returns the cells (still holding placeholders) and the row's escapes.
fn parse_row(row: &str, spans: &mut Vec<String>) -> (Vec<String>, Vec<String>) {
    let (plain, esc) = match extract_escapes(row) {
        Some(x) => x,
        None => (row.to_string(), Vec::new()),
    };
    let masked = mask_code_and_escapes(&plain, spans);
    let t = masked.trim();
    let t = t.strip_prefix('|').unwrap_or(t);
    let t = t.strip_suffix('|').unwrap_or(t);
    (t.split('|').map(|c| c.trim().to_string()).collect(), esc)
}

fn cell_lines(masked_cell: &str, esc: &[String], spans: &mut Vec<String>) -> Vec<String> {
    let with_br = re_br().replace_all(masked_cell, "\n").into_owned();
    with_br
        .split('\n')
        .map(|part| {
            let rendered = inline_masked(part.trim(), spans);
            restore_escapes(&unmask(&rendered, spans), esc)
        })
        .collect()
}

fn parse_align(sep: &str) -> Align {
    let t = sep.trim();
    match (t.starts_with(':'), t.ends_with(':')) {
        (true, true) => Align::Center,
        (false, true) => Align::Right,
        _ => Align::Left,
    }
}

/// Draw `rows` (raw markdown table rows) as a box table. `None` if it is not
/// a real table (no header separator), so the caller prints plain lines.
fn render_table(rows: &[String], width: usize, eol: &str) -> Option<String> {
    if rows.len() < 2 {
        return None;
    }
    let mut spans: Vec<String> = Vec::new();
    let (header, header_esc) = parse_row(&rows[0], &mut spans);
    let (sep, _) = parse_row(&rows[1], &mut spans);
    if sep.is_empty() || !sep.iter().all(|c| re_sep_cell().is_match(c)) {
        return None;
    }
    let n = header.len().max(1);
    let aligns: Vec<Align> = (0..n)
        .map(|i| sep.get(i).map_or(Align::Left, |c| parse_align(c)))
        .collect();

    let mut grid: Vec<TableCells> = Vec::new();
    let mut push_row = |cells: Vec<String>, esc: &[String], spans: &mut Vec<String>| {
        let mut cells = cells;
        if cells.len() > n {
            let extra = cells.split_off(n - 1).join(" | ");
            cells.push(extra);
        }
        while cells.len() < n {
            cells.push(String::new());
        }
        grid.push(TableCells {
            cells: cells.iter().map(|c| cell_lines(c, esc, spans)).collect(),
        });
    };
    push_row(header, &header_esc, &mut spans);
    for r in &rows[2..] {
        let (cells, esc) = parse_row(r, &mut spans);
        push_row(cells, &esc, &mut spans);
    }

    // natural column widths
    let mut natural = vec![1usize; n];
    for row in &grid {
        for (i, cell) in row.cells.iter().enumerate() {
            for l in cell {
                natural[i] = natural[i].max(visible_width(l));
            }
        }
    }
    let border = 3 * n + 1;
    let mut w = natural.clone();
    if width > 0 {
        let maxw = width.saturating_sub(1);
        if w.iter().sum::<usize>() + border > maxw {
            let avail = maxw.saturating_sub(border).max(n * 3);
            let minw: Vec<usize> = natural.iter().map(|&x| x.min(6)).collect();
            while w.iter().sum::<usize>() > avail {
                let mut best: Option<usize> = None;
                for i in 0..n {
                    if w[i] > minw[i] && best.map_or(true, |b| w[i] > w[b]) {
                        best = Some(i);
                    }
                }
                match best {
                    Some(i) => w[i] -= 1,
                    None => break,
                }
            }
        }
    }

    let hline = |l: &str, m: &str, r: &str| -> String {
        let segs: Vec<String> = w.iter().map(|&x| "─".repeat(x + 2)).collect();
        format!("{BORDER_ON}{l}{}{r}{BORDER_OFF}{eol}", segs.join(m))
    };

    let mut out = String::new();
    out.push_str(&hline("┌", "┬", "┐"));
    for (ri, row) in grid.iter().enumerate() {
        // wrap every cell to its column width
        let wrapped: Vec<Vec<String>> = row
            .cells
            .iter()
            .enumerate()
            .map(|(i, cell)| {
                cell.iter()
                    .flat_map(|l| wrap_visible(l, w[i], true))
                    .collect::<Vec<String>>()
            })
            .collect();
        let height = wrapped.iter().map(|c| c.len()).max().unwrap_or(1).max(1);
        for line_no in 0..height {
            let mut s = String::new();
            s.push_str(&format!("{BORDER_ON}│{BORDER_OFF}"));
            for (i, cell) in wrapped.iter().enumerate() {
                let text = cell.get(line_no).map(String::as_str).unwrap_or("");
                let padded = pad_cell(text, w[i], aligns[i]);
                if ri == 0 {
                    s.push_str(&format!(" {BOLD_ON}{padded}{BOLD_OFF} "));
                } else {
                    s.push_str(&format!(" {padded} "));
                }
                s.push_str(&format!("{BORDER_ON}│{BORDER_OFF}"));
            }
            s.push_str(eol);
            out.push_str(&s);
        }
        if ri == 0 {
            out.push_str(&hline("├", "┼", "┤"));
        }
    }
    out.push_str(&hline("└", "┴", "┘"));
    Some(out)
}

// ---------------------------------------------------------------------
// Block level
// ---------------------------------------------------------------------

enum FenceEv {
    /// a delimiter line: print nothing
    Hidden,
    /// a line inside an open block
    CodeLine,
    /// not part of any fence
    NotFence,
}

fn fence_event(fence: &mut Option<Fence>, plain: &str) -> FenceEv {
    let caps = re_fence_line().captures(plain);
    if fence.is_some() {
        if let Some(c) = caps {
            let marker = &c[1];
            let info = c[2].trim();
            let (ch, len) = {
                let f = fence.as_ref().unwrap();
                (f.ch, f.len)
            };
            if marker.starts_with(ch) && marker.chars().count() >= len {
                if info.is_empty() {
                    let f = fence.as_mut().unwrap();
                    f.depth -= 1;
                    if f.depth == 0 {
                        *fence = None;
                        return FenceEv::Hidden;
                    }
                    return FenceEv::CodeLine;
                }
                if !(ch == '`' && info.contains('`')) {
                    fence.as_mut().unwrap().depth += 1;
                    return FenceEv::CodeLine;
                }
            }
        }
        return FenceEv::CodeLine;
    }
    if let Some(c) = caps {
        let marker = &c[1];
        let info = c[2].trim();
        let ch = marker.chars().next().unwrap_or('`');
        if !(ch == '`' && info.contains('`')) {
            *fence = Some(Fence { ch, len: marker.chars().count(), depth: 1 });
            return FenceEv::Hidden;
        }
    }
    FenceEv::NotFence
}

/// Print `text` (already inline-converted) wrapped to `avail` columns.
fn push_wrapped(
    out: &mut String,
    first: &str,
    cont: &str,
    suffix: &str,
    text: &str,
    avail: usize,
    eol: &str,
) {
    let lines = if avail > 0 && visible_width(text) > avail {
        wrap_visible(text, avail, false)
    } else {
        vec![text.to_string()]
    };
    for (i, l) in lines.iter().enumerate() {
        out.push_str(if i == 0 { first } else { cont });
        out.push_str(l);
        out.push_str(suffix);
        out.push_str(eol);
    }
}

fn bullet_glyph(depth: usize) -> &'static str {
    match depth {
        0 => "\u{2022}", // •
        1 => "\u{25e6}", // ◦
        _ => "\u{25aa}", // ▪
    }
}

/// Render one non-table, non-fence block line (`plain` holds placeholders).
fn render_block(plain: &str, width: usize, eol: &str, out: &mut String) {
    let wrap_w = if width >= 20 { width - 1 } else { 0 };

    if re_hr().is_match(plain) {
        let n = if wrap_w > 0 { wrap_w.min(100) } else { 40 };
        out.push_str(&format!("{DIM_ON}{}{RESET}{eol}", "\u{2500}".repeat(n)));
        return;
    }

    if let Some(caps) = re_heading().captures(plain) {
        let indent = &caps[1];
        let style = heading_style(caps[2].len());
        let mut text = caps[3].trim_end().to_string();
        // optional closing hashes: "## Title ##"
        if let Some(pos) = text.rfind(' ') {
            if text[pos + 1..].chars().all(|c| c == '#') && text.len() > pos + 1 {
                text.truncate(pos);
            }
        }
        let text = apply_inline(&text);
        let avail = wrap_w.saturating_sub(indent.len());
        push_wrapped(out, &format!("{indent}{style}"), &format!("{indent}{style}"), RESET, &text, avail, eol);
        return;
    }

    if let Some(caps) = re_quote().captures(plain) {
        let indent = &caps[1];
        let depth = caps[2].matches('>').count().max(1);
        let bars = "\u{2503} ".repeat(depth);
        let text = apply_inline(caps[3].trim_end())
            .replace(CODE_INLINE, "\x1b[22;96m")
            .replace(CODE_INLINE_OFF, "\x1b[39;2m");
        let avail = wrap_w.saturating_sub(indent.len() + 2 * depth);
        let prefix = format!("{indent}{QUOTE_STYLE}{bars}");
        push_wrapped(out, &prefix, &prefix, RESET, &text, avail, eol);
        return;
    }

    if let Some(caps) = re_list().captures(plain) {
        let indent = caps[1].replace('\t', "    ");
        let marker = &caps[2];
        let mut rest = caps[3].to_string();
        let depth = indent.len() / 2;
        let ordered = marker.chars().next().map_or(false, |c| c.is_ascii_digit());
        let mut glyph = if ordered { marker.to_string() } else { bullet_glyph(depth).to_string() };
        let mut glyph_style = BULLET_COLOR;
        if let Some(r) = rest.strip_prefix("[ ] ") {
            glyph = "\u{2610}".to_string(); // ☐
            glyph_style = "\x1b[2m";
            rest = r.to_string();
        } else if let Some(r) = rest.strip_prefix("[x] ").or_else(|| rest.strip_prefix("[X] ")) {
            glyph = "\u{2611}".to_string(); // ☑
            rest = r.to_string();
        }
        let text = apply_inline(rest.trim_end());
        let gw = visible_width(&glyph);
        let first = format!("{indent}{glyph_style}{glyph}{RESET} ");
        let cont = " ".repeat(indent.len() + gw + 1);
        let avail = wrap_w.saturating_sub(indent.len() + gw + 1);
        push_wrapped(out, &first, &cont, "", &text, avail, eol);
        return;
    }

    // paragraph
    let trimmed = plain.trim_start();
    let indent = &plain[..plain.len() - trimmed.len()];
    let text = apply_inline(trimmed.trim_end());
    let avail = wrap_w.saturating_sub(indent.len());
    push_wrapped(out, indent, indent, "", &text, avail, eol);
}

impl MdTermState {
    /// Draw (or, if it is not a real table, print as plain lines) whatever
    /// table rows are buffered.
    fn release_table(&mut self, eol: &str, out: &mut String) {
        if self.table.is_empty() {
            return;
        }
        let rows = std::mem::take(&mut self.table);
        match render_table(&rows, self.width, eol) {
            Some(t) => out.push_str(&t),
            None => {
                for r in &rows {
                    self.emit_plain_row(r, eol, out);
                }
            }
        }
    }

    fn emit_plain_row(&mut self, row: &str, eol: &str, out: &mut String) {
        match extract_escapes(row) {
            Some((plain, esc)) => {
                let mut tmp = String::new();
                render_block(&plain, self.width, eol, &mut tmp);
                out.push_str(&restore_escapes(&tmp, &esc));
            }
            None => {
                out.push_str(row);
                out.push_str(eol);
            }
        }
    }

    /// Convert one *logical* line (soft wraps already joined, no line ending).
    /// `partial`: the line is not finished (idle flush): no wrapping, no
    /// table buffering, no line ending.
    fn emit_logical(&mut self, logical: &str, eol: &str, out: &mut String, partial: bool) {
        let (prefix, body) = logical.split_at(control_prefix_len(logical));
        let extracted = extract_escapes(body);
        let Some((plain, esc)) = extracted else {
            // cursor movement / private-use glyphs: not prose, hands off
            self.release_table(eol, out);
            out.push_str(logical);
            out.push_str(eol);
            return;
        };

        // 1. fenced code
        match fence_event(&mut self.fence, &plain) {
            FenceEv::Hidden => {
                self.release_table(eol, out);
                out.push_str(prefix);
                out.push_str(&esc.concat());
                return;
            }
            FenceEv::CodeLine => {
                self.release_table(eol, out);
                out.push_str(prefix);
                out.push_str(&restore_escapes(&format!("{CODE_BLOCK}{plain}{RESET}"), &esc));
                out.push_str(eol);
                return;
            }
            FenceEv::NotFence => {}
        }

        // 2. table rows are buffered until the table ends
        if !partial && re_table_row().is_match(&plain) {
            out.push_str(prefix);
            self.table.push(restore_escapes(&plain, &esc));
            if self.table.len() >= MAX_TABLE_ROWS {
                self.release_table(eol, out);
            }
            return;
        }
        self.release_table(eol, out);
        out.push_str(prefix);

        // 3. everything else
        let (width, eol) = if partial { (0, "") } else { (self.width, eol) };
        let mut tmp = String::new();
        render_block(&plain, width, eol, &mut tmp);
        out.push_str(&restore_escapes(&tmp, &esc));
    }
}

// ---------------------------------------------------------------------
// Feeding bytes
// ---------------------------------------------------------------------

impl MdTermState {
    /// Give back everything held (table, soft-wrapped text) without
    /// converting it, before a raw line is printed.
    fn release_held_raw(&mut self, eol: &str, out: &mut String) {
        self.release_table(eol, out);
        if !self.carry.is_empty() {
            out.push_str(&std::mem::take(&mut self.carry));
        }
    }
}

/// Handle one complete physical line (`line` ends with '\n').
fn process_physical(st: &mut MdTermState, line: &str, out: &mut String) {
    st.note_newline();

    let body = line.strip_suffix('\n').unwrap_or(line);
    let trimmed = body.trim_end_matches('\r');
    let cr = body.len() - trimmed.len();
    let eol = format!("{}\n", "\r".repeat(cr));

    let cleaned = drop_noise(trimmed);
    let (prefix, rest) = cleaned.split_at(control_prefix_len(&cleaned));
    let (head, tail) = split_tail(rest);

    let Some((mut plain, esc)) = extract_escapes(head) else {
        // Cursor movement, progress bars, private-use glyphs...: not prose.
        st.release_held_raw(&eol, out);
        out.push_str(line);
        return;
    };

    if let Tail::WrapBack(n) = tail {
        if n > 0 {
            plain = truncate_cols_keep_escapes(&plain, n);
        }
    }
    // A wrap is "soft": the same logical line continues on the next one.
    let soft = match tail {
        Tail::WrapBack(_) => true,
        // ollama wraps right after a space (the space is still on the line) or
        // when the line is full; other programs rarely end a line like that
        Tail::Erase => {
            plain.ends_with(' ') || (st.width > 0 && visible_width(&plain) + 8 >= st.width)
        }
        Tail::None => false,
    };

    let mut frag = String::new();
    if st.carry.is_empty() {
        frag.push_str(prefix);
    } else {
        frag.push_str(&prefix.replace('\r', ""));
    }
    frag.push_str(&restore_escapes(&plain, &esc));

    if soft {
        st.carry.push_str(&frag);
        return;
    }
    let logical = if st.carry.is_empty() {
        frag
    } else {
        let mut c = std::mem::take(&mut st.carry);
        c.push_str(&frag);
        c
    };
    st.emit_logical(&logical, &eol, out, false);
}

/// Feed new bytes into `state`, returning converted output for every
/// complete line now available, or `None` if we're still waiting on a
/// terminating '\n'.
fn feed_bytes(state: &mut MdTermState, input: &[u8]) -> Option<Vec<u8>> {
    state.pending.extend_from_slice(input);

    let mut out = String::new();
    let mut produced = false;

    while let Some(pos) = state.pending.iter().position(|&b| b == b'\n') {
        let line_bytes: Vec<u8> = state.pending.drain(..=pos).collect();
        // Lossy is deliberate: a genuinely malformed byte shouldn't wedge
        // the whole terminal, it just renders as U+FFFD like everywhere else.
        let line = String::from_utf8_lossy(&line_bytes);
        process_physical(state, &line, &mut out);
        produced = true;
    }

    if state.pending.len() > MAX_PENDING {
        state.release_held_raw("\r\n", &mut out);
        out.push_str(&String::from_utf8_lossy(&std::mem::take(&mut state.pending)));
        produced = true;
    }

    if produced {
        Some(out.into_bytes())
    } else {
        None
    }
}

/// The idle timeout fired: show what is being held. Interactive-looking
/// text (prompt, echo) goes out raw; prose is converted as far as it goes.
fn do_flush(st: &mut MdTermState) -> Vec<u8> {
    let class = st.hold_class();
    let mut out = String::new();
    st.release_table("\r\n", &mut out);

    let pend = String::from_utf8_lossy(&st.pending).into_owned();
    let text = format!("{}{}", st.carry, pend);
    st.carry.clear();
    st.pending.clear();

    if !text.is_empty() {
        if class == 1 {
            out.push_str(&text);
            // back at a prompt: whatever block was open is over
            if looks_like_prompt(pend.as_bytes()) {
                st.fence = None;
            }
        } else {
            st.emit_logical(&text, "", &mut out, true);
        }
    }
    out.into_bytes()
}

// ---------------------------------------------------------------------
// C ABI
// ---------------------------------------------------------------------

#[no_mangle]
pub extern "C" fn mdterm_new() -> *mut c_void {
    Box::into_raw(Box::new(MdTermState::new())) as *mut c_void
}

/// Tell the converter how many columns the terminal has (0 = unknown).
#[no_mangle]
pub extern "C" fn mdterm_set_width(state: *mut c_void, cols: usize) {
    if state.is_null() {
        return;
    }
    // SAFETY: `state` came from mdterm_new().
    unsafe { (*(state as *mut MdTermState)).width = cols };
}

fn into_c_buf(v: Vec<u8>, out_len: *mut usize) -> *mut u8 {
    if v.is_empty() {
        unsafe { *out_len = 0 };
        return ptr::null_mut();
    }
    let mut boxed = v.into_boxed_slice();
    let len = boxed.len();
    let data_ptr = boxed.as_mut_ptr();
    std::mem::forget(boxed);
    // SAFETY: caller checked out_len is non-null.
    unsafe { *out_len = len };
    data_ptr
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
        // SAFETY: caller guarantees `input` points at `input_len` valid bytes.
        unsafe { slice::from_raw_parts(input, input_len) }
    };
    match feed_bytes(state_ref, input_slice) {
        Some(output) => into_c_buf(output, out_len),
        None => {
            unsafe { *out_len = 0 };
            ptr::null_mut()
        }
    }
}

/// Number of bytes currently held back (partial line, soft-wrapped text,
/// buffered table rows).
#[no_mangle]
pub extern "C" fn mdterm_pending_len(state: *const c_void) -> usize {
    if state.is_null() {
        return 0;
    }
    // SAFETY: `state` came from mdterm_new().
    unsafe { (*(state as *const MdTermState)).held_len() }
}

/// How long the C side should let the held text sit idle before showing it:
/// 1 = looks interactive (prompt, echoed keys): a moment; 2 = a slow LLM-style
/// stream is running: a long time; 0 = the normal timeout.
#[no_mangle]
pub extern "C" fn mdterm_hold_class(state: *const c_void) -> i32 {
    if state.is_null() {
        return 0;
    }
    // SAFETY: `state` came from mdterm_new().
    unsafe { (*(state as *const MdTermState)).hold_class() }
}

/// The idle timeout fired: hand back whatever is held (tables are drawn,
/// prompts come out raw). Returns NULL / *out_len = 0 if nothing is held.
/// Release with mdterm_free_buf().
#[no_mangle]
pub extern "C" fn mdterm_flush(state: *mut c_void, out_len: *mut usize) -> *mut u8 {
    if state.is_null() || out_len.is_null() {
        return ptr::null_mut();
    }
    // SAFETY: `state` came from mdterm_new().
    let state_ref = unsafe { &mut *(state as *mut MdTermState) };
    into_c_buf(do_flush(state_ref), out_len)
}

#[no_mangle]
pub extern "C" fn mdterm_free_buf(ptr_in: *mut u8, len: usize) {
    if ptr_in.is_null() {
        return;
    }
    // SAFETY: only ever called with a (ptr, len) pair previously returned
    // from mdterm_feed()/mdterm_flush(), which built it the same way.
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
    // SAFETY: only ever called with a pointer previously returned from mdterm_new().
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

    /// Feed `s` and flush at the end, like a stream that stopped.
    fn conv_w(s: &str, width: usize) -> String {
        let mut st = MdTermState::new();
        st.width = width;
        let mut out = feed_bytes(&mut st, s.as_bytes()).unwrap_or_default();
        out.extend(do_flush(&mut st));
        String::from_utf8(out).unwrap()
    }
    fn conv(s: &str) -> String {
        conv_w(s, 0)
    }
    fn strip_sgr(s: &str) -> String {
        let re = Regex::new(r"\x1b\[[0-9;?]*[A-Za-z]").unwrap();
        re.replace_all(s, "").into_owned()
    }

    #[test]
    fn bold_and_italic() {
        let out = conv("this is **bold** and *italic*\n");
        assert!(out.contains(BOLD_ON) && out.contains(ITALIC_ON));
        assert!(!out.contains("**"));
    }

    #[test]
    fn heading_levels_have_distinct_styles() {
        let mut seen = std::collections::HashSet::new();
        for level in 1..=6 {
            let out = conv(&format!("{} Title\n", "#".repeat(level)));
            assert!(!out.contains('#'));
            assert!(seen.insert(heading_style(level)));
            assert!(out.contains(heading_style(level)));
        }
    }

    #[test]
    fn partial_line_buffers_until_newline() {
        let mut state = MdTermState::new();
        assert!(feed_bytes(&mut state, b"**bo").is_none());
        let out = feed_bytes(&mut state, b"ld**\n").unwrap();
        assert!(String::from_utf8(out).unwrap().contains(BOLD_ON));
    }

    #[test]
    fn crlf_and_double_cr_are_preserved() {
        assert_eq!(conv("plain\r\n"), "plain\r\n");
        assert!(conv("**x**\r\n").ends_with("\r\n"));
        assert!(conv("**x**\r\r\n").ends_with("\r\r\n"));
    }

    #[test]
    fn code_span_contents_are_not_reinterpreted() {
        let out = conv("a `**x** and _y_` b **z**\n");
        assert!(out.contains(&format!("{CODE_INLINE}**x** and _y_{CODE_INLINE_OFF}")));
        assert!(out.contains(&format!("{BOLD_ON}z{BOLD_OFF}")));
    }

    #[test]
    fn double_backtick_code_spans() {
        let out = conv("use `` `код` `` here\n");
        assert!(out.contains(&format!("{CODE_INLINE}`код`{CODE_INLINE_OFF}")));
        assert_eq!(conv("lonely ` backtick\n"), "lonely ` backtick\n");
    }

    #[test]
    fn intraword_underscores_untouched() {
        let out = conv("see my_file_name.txt and _real_ italic\n");
        assert!(out.contains("my_file_name.txt"));
        assert!(out.contains(&format!("{ITALIC_ON}real{ITALIC_OFF}")));
    }

    #[test]
    fn arithmetic_stars_are_not_italics() {
        assert_eq!(conv("2 * 3 * 4\n"), "2 * 3 * 4\n");
    }

    #[test]
    fn bold_italic_strike() {
        let out = conv("***both*** and ~~gone~~\n");
        assert!(out.contains(&format!("{BOLD_ON}{ITALIC_ON}both{ITALIC_OFF}{BOLD_OFF}")));
        assert!(out.contains(&format!("{STRIKE_ON}gone{STRIKE_OFF}")));
    }

    #[test]
    fn backslash_escapes() {
        assert_eq!(strip_sgr(&conv("\\*not italic\\* and \\`x\\`\n")), "*not italic* and `x`\n");
    }

    #[test]
    fn links_images_and_urls() {
        let out = conv("[Rust](https://rust-lang.org) ![logo](a/b.png) <https://x.io> see https://y.dev/a_b_c.\n");
        let plain = strip_sgr(&out);
        assert!(plain.contains("Rust (https://rust-lang.org)"));
        assert!(plain.contains("[logo] (a/b.png)"));
        assert!(plain.contains("https://x.io"));
        assert!(plain.contains("https://y.dev/a_b_c."));
        assert!(!plain.contains("]("));
        assert!(out.contains(LINK_ON));
    }

    #[test]
    fn bold_inside_link_text() {
        let out = conv("[**hot** link](https://a.b)\n");
        assert!(out.contains(BOLD_ON));
        assert!(!strip_sgr(&out).contains("**"));
    }

    #[test]
    fn bullets_numbered_and_tasks() {
        let out = strip_sgr(&conv("- one\n* two\n+ three\n  - nested\n1. first\n2) second\n- [ ] todo\n- [x] done\n"));
        assert!(out.contains("\u{2022} one") && out.contains("\u{2022} three"));
        assert!(out.contains("  \u{25e6} nested"));
        assert!(out.contains("1. first") && out.contains("2) second"));
        assert!(out.contains("\u{2610} todo") && out.contains("\u{2611} done"));
    }

    #[test]
    fn nested_quotes() {
        let out = strip_sgr(&conv("> a\n>> b\n"));
        assert!(out.contains("\u{2503} a"));
        assert!(out.contains("\u{2503} \u{2503} b"));
    }

    #[test]
    fn horizontal_rule_uses_terminal_width() {
        let out = strip_sgr(&conv_w("---\n", 30));
        assert_eq!(out.trim_end().chars().count(), 29);
        assert!(out.starts_with('\u{2500}'));
    }

    #[test]
    fn fence_hidden_and_content_verbatim() {
        let out = conv("```python\nprint(\"**x**\") # y\n```\n");
        assert!(!out.contains("```") && !out.contains("python"));
        assert!(out.contains(&format!("{CODE_BLOCK}print(\"**x**\") # y{RESET}")));
    }

    #[test]
    fn nested_fences_from_llms() {
        let out = strip_sgr(&conv(
            "```markdown\n# Hi\n```python\nprint(1)\n```\n```\nafter **b**\n",
        ));
        // the inner fences are content of the outer block
        assert!(out.contains("# Hi"));
        assert!(out.contains("```python"));
        assert!(out.contains("print(1)"));
        // and the outer block did close: the next line is converted
        let raw = conv("```markdown\n# Hi\n```python\nprint(1)\n```\n```\nafter **b**\n");
        assert!(raw.contains(&format!("{BOLD_ON}b{BOLD_OFF}")));
    }

    #[test]
    fn tilde_fences() {
        let out = conv("~~~\ncode\n~~~\n");
        assert!(!out.contains('~') && out.contains("code"));
    }

    #[test]
    fn sgr_inside_line_is_converted() {
        let out = conv("\x1b[90mHello **bold** and _it_\x1b[0m\r\n");
        assert!(out.starts_with("\x1b[90m"));
        assert!(out.contains(BOLD_ON) && out.contains(ITALIC_ON));
        assert!(out.ends_with("\x1b[0m\r\n"));
    }

    #[test]
    fn heading_after_bash_prefix_and_noise() {
        let out = conv("\x1b[?2004l\r#\x1b[?25l\x1b[?25h Title\r\n");
        assert!(out.starts_with("\x1b[?2004l\r"));
        assert!(out.contains(heading_style(1)) && !out.contains('#'));
    }

    #[test]
    fn cursor_movement_passes_through() {
        let line = "abc\x1b[2Ddef **x**\r\n";
        assert_eq!(conv(line), line);
        let line = "progress 50%\rprogress 60% **x**\n";
        assert_eq!(conv(line), line);
    }

    #[test]
    fn nerd_font_glyphs_are_left_alone() {
        let line = "\u{e0b0} main **x**\n";
        assert_eq!(conv(line), line);
    }

    #[test]
    fn soft_wrapped_lines_are_joined() {
        // ollama: "…придуманный *" + step back 1 + newline, then "**Джоном** …"
        let out = conv("Идея придуманный *\x1b[1D\x1b[K\r\n**Джоном** в 2004\r\n");
        let plain = strip_sgr(&out);
        assert_eq!(plain, "Идея придуманный **Джоном** в 2004\r\n".replace("**", ""));
        assert!(out.contains(BOLD_ON));
    }

    #[test]
    fn soft_wrap_with_plain_erase_needs_a_full_line() {
        // width known and line nearly full: joined
        let long = "a".repeat(28);
        let out = conv_w(&format!("{long}\x1b[K\r\nb **c**\r\n"), 33);
        assert!(strip_sgr(&out).starts_with(&format!("{long}b c")));
        // short line + ESC[K: a plain line ending
        let out = conv_w("hi\x1b[K\r\nyo\r\n", 80);
        assert_eq!(out, "hi\r\nyo\r\n");
    }

    #[test]
    fn bold_split_by_wrap_is_reassembled() {
        let out = conv("text **бол\x1b[3D\x1b[K\r\nбол ьшой** end\r\n");
        assert!(out.contains(BOLD_ON));
        assert!(!strip_sgr(&out).contains('*'));
    }

    #[test]
    fn paragraphs_are_word_wrapped_to_width() {
        let out = strip_sgr(&conv_w("one two three four five six seven eight nine\n", 24));
        for l in out.lines() {
            assert!(l.chars().count() <= 23, "{l:?}");
        }
        assert!(out.lines().count() >= 2);
    }

    #[test]
    fn list_items_wrap_with_hanging_indent() {
        let out = strip_sgr(&conv_w("- alpha beta gamma delta epsilon zeta\n", 24));
        let lines: Vec<&str> = out.lines().collect();
        assert!(lines[0].starts_with("\u{2022} alpha"));
        assert!(lines[1].starts_with("  "));
    }

    // ----- tables -----

    const TABLE: &str = "| Name | Qty | Price |\n|:---|:---:|---:|\n| Apple | 3 | 1.50 |\n| Kiwi **fruit** | 12 | 0.25 |\n\n";

    #[test]
    fn table_is_drawn_with_borders() {
        let out = conv(TABLE);
        let plain = strip_sgr(&out);
        assert!(plain.contains('┌') && plain.contains('┘') && plain.contains('├'));
        assert!(plain.contains("│ Apple"));
        assert!(!plain.contains("|:---"));
        assert!(!plain.contains("**"));
        // all rows are the same width
        let widths: Vec<usize> = plain.lines().filter(|l| !l.is_empty()).map(|l| l.chars().count()).collect();
        assert!(widths.windows(2).all(|w| w[0] == w[1]), "{widths:?}");
    }

    #[test]
    fn table_alignment() {
        let plain = strip_sgr(&conv(TABLE));
        let price_row = plain.lines().find(|l| l.contains("0.25")).unwrap();
        assert!(price_row.contains(" 0.25 │"));
        let apple = plain.lines().find(|l| l.contains("Apple")).unwrap();
        assert!(apple.contains("│ Apple "));
    }

    #[test]
    fn table_pipes_in_code_and_escapes() {
        let plain = strip_sgr(&conv("| a | b |\n|---|---|\n| `x | y` | c \\| d |\n"));
        assert!(plain.contains("x | y"));
        assert!(plain.contains("c | d"));
        assert_eq!(plain.lines().next().unwrap().matches('┬').count(), 1);
    }

    #[test]
    fn table_fits_terminal_width_by_wrapping_cells() {
        let src = "| Что | Описание |\n|---|---|\n| Длинная ячейка | очень длинный текст который не поместится в одну строку терминала |\n";
        let plain = strip_sgr(&conv_w(src, 40));
        for l in plain.lines() {
            assert!(l.chars().count() <= 40, "{l:?}");
        }
        assert!(plain.contains("поместится"));
        assert!(plain.lines().count() > 6);
    }

    #[test]
    fn table_wrapped_rows_from_ollama_are_reassembled() {
        // a row that ollama wrapped in the middle of a word ("сло" is 3 columns)
        let wrapped =
            "| A | B |\r\n|---|---|\r\n| длинное сло\x1b[3D\x1b[K\r\nслово | 2 |\r\n\r\n";
        let plain = strip_sgr(&conv(wrapped));
        assert!(plain.contains("│ длинное слово"), "{plain}");
    }

    #[test]
    fn table_ends_at_next_block_and_at_flush() {
        let out = strip_sgr(&conv("| a |\n|---|\n| 1 |\ntext after\n"));
        let table_end = out.find('┘').unwrap();
        assert!(out[table_end..].contains("text after"));
        // stream stopped right after the last row: flush draws it
        assert!(strip_sgr(&conv("| a |\n|---|\n| 1 |\n")).contains('└'));
    }

    #[test]
    fn pipe_lines_without_separator_are_plain_text() {
        let out = strip_sgr(&conv("| just | text |\nnext\n"));
        assert!(!out.contains('┌'));
        assert!(out.contains("| just | text |"));
    }

    #[test]
    fn table_inside_fence_is_code() {
        let out = strip_sgr(&conv("```\n| a |\n|---|\n```\n"));
        assert!(!out.contains('┌'));
        assert!(out.contains("|---|"));
    }

    #[test]
    fn table_br_makes_multiline_cells() {
        let plain = strip_sgr(&conv("| a |\n|---|\n| x<br>y |\n"));
        assert!(plain.contains("│ x "));
        assert!(plain.contains("│ y "));
    }

    // ----- streaming / flush behaviour -----

    #[test]
    fn prompt_detection() {
        assert!(looks_like_prompt(b"[iwa@komp xterm-411]$ "));
        assert!(looks_like_prompt(b"$ "));
        assert!(looks_like_prompt(b"root@host:~# "));
        assert!(looks_like_prompt(b">>> "));
        assert!(looks_like_prompt("~ ❯ ".as_bytes()));
        assert!(!looks_like_prompt(b"## "));
        assert!(!looks_like_prompt(b"> "));
        assert!(!looks_like_prompt(b"- "));
        assert!(!looks_like_prompt(b"Hello wor"));
        assert!(!looks_like_prompt("Это слово ".as_bytes()));
    }

    #[test]
    fn unsafe_control_detection() {
        assert!(!has_unsafe_control(b"\x1b[31mred **bold**"));
        assert!(!has_unsafe_control(b"\x1b[?25h text"));
        assert!(!has_unsafe_control(b"text \x1b[3"));
        assert!(has_unsafe_control(b"abc\x1b[2D"));
        assert!(has_unsafe_control(b"abc\x1b[K"));
        assert!(has_unsafe_control(b"abc\r"));
        assert!(has_unsafe_control(b"abc\x08"));
    }

    #[test]
    fn hold_classes() {
        let mut st = MdTermState::new();
        assert_eq!(st.hold_class(), 0);
        feed_bytes(&mut st, b"user@host:~$ ");
        assert_eq!(st.hold_class(), 1);
        st.pending.clear();
        feed_bytes(&mut st, b"some prose");
        assert_eq!(st.hold_class(), 0);
        // a slow stream is running: prose partials wait a long time
        st.slow_lines = 5;
        st.last_nl = Some(Instant::now());
        assert_eq!(st.hold_class(), 2);
        // ... but a prompt still shows quickly
        st.pending.clear();
        feed_bytes(&mut st, b"$ ");
        assert_eq!(st.hold_class(), 1);
    }

    #[test]
    fn slow_lines_are_counted() {
        let mut st = MdTermState::new();
        st.note_newline();
        st.last_nl = Some(Instant::now() - Duration::from_millis(500));
        st.note_newline();
        assert_eq!(st.slow_lines, 1);
        st.note_newline(); // immediately after: fast
        assert_eq!(st.slow_lines, 1);
    }

    #[test]
    fn buffered_table_counts_as_held_and_burst_tables_release_quickly() {
        let mut st = MdTermState::new();
        feed_bytes(&mut st, b"| a |\n|---|\n| 1 |\n");
        assert!(st.held_len() > 0);
        assert_eq!(st.hold_class(), 1);
        let flushed = String::from_utf8(do_flush(&mut st)).unwrap();
        assert!(flushed.contains('┌'));
        assert_eq!(st.held_len(), 0);
    }

    #[test]
    fn flush_converts_prose_but_prompts_stay_raw() {
        let mut st = MdTermState::new();
        feed_bytes(&mut st, b"**Minus:** no strict");
        let out = String::from_utf8(do_flush(&mut st)).unwrap();
        assert!(out.contains(BOLD_ON) && !out.contains("**"));
        feed_bytes(&mut st, b"[me@h ~]$ ");
        assert_eq!(String::from_utf8(do_flush(&mut st)).unwrap(), "[me@h ~]$ ");
    }

    #[test]
    fn prompt_flush_closes_a_dangling_fence() {
        let mut st = MdTermState::new();
        feed_bytes(&mut st, b"```\ncode\n");
        assert!(st.fence.is_some());
        feed_bytes(&mut st, b"$ ");
        do_flush(&mut st);
        assert!(st.fence.is_none());
    }

    #[test]
    fn huge_line_without_newline_is_flushed_raw() {
        let mut st = MdTermState::new();
        let big = vec![b'x'; MAX_PENDING + 10];
        let out = feed_bytes(&mut st, &big).unwrap();
        assert_eq!(out.len(), big.len());
        assert!(st.pending.is_empty());
    }
}
