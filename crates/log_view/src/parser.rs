//! Bytes of a CI log to lines with style spans.

use std::collections::HashMap;

use vte::{Params, Perform};

use crate::ansi::{Attrs, Color, Span, Style, StyleId, apply_sgr};
use crate::buffer::{Batch, GroupEvent, LineKind, LineMeta};
use crate::timestamp;

/// Most input bytes of one line that are parsed; the rest of a longer line is
/// skipped up to its newline and replaced by a marker. Bounds what an
/// unterminated escape sequence can accumulate as well as the line's text.
pub const MAX_LINE_BYTES: usize = 64 * 1024;

/// How a truncated line shows that it was cut.
const TRUNCATED_STYLE: Style = Style {
    fg: Color::Indexed(3),
    bg: Color::Default,
    attrs: Attrs::ITALIC,
};

const GROUP: &str = "##[group]";
const END_GROUP: &str = "##[endgroup]";
const ERROR: &str = "##[error]";
const WARNING: &str = "##[warning]";

/// Turns chunks of log bytes into lines. A chunk may end anywhere: inside a
/// line, an escape sequence or a UTF-8 character. A carriage return makes the
/// next character start the line again, as progress output expects, and
/// `\r\n` ends a line without losing it. A newline always ends the line, even
/// inside an unterminated escape sequence: CI logs can contain any bytes.
pub struct LogParser {
    vte: vte::Parser,
    /// Whether `vte` may be inside a sequence or a character. It is then
    /// reset at the end of the line, so no state outlives a newline.
    dirty: bool,
    lines: Lines,
    /// Bytes of the line in progress passed to `vte`, and bytes seen of it;
    /// they differ once the line is past [`MAX_LINE_BYTES`].
    fed: usize,
    seen: usize,
    /// The start of a character a chunk ended in the middle of. vte keeps
    /// such bytes itself but loses the character after them, so they are
    /// held back here instead.
    cut: Vec<u8>,
}

impl Default for LogParser {
    fn default() -> Self {
        Self::new()
    }
}

impl LogParser {
    #[must_use]
    pub fn new() -> Self {
        Self {
            vte: vte::Parser::new(),
            dirty: false,
            lines: Lines::default(),
            fed: 0,
            seen: 0,
            cut: Vec::new(),
        }
    }

    /// Parses `bytes`; complete lines wait for [`LogParser::take`].
    pub fn feed(&mut self, bytes: &[u8]) {
        if self.cut.is_empty() {
            self.advance(bytes);
        } else {
            let mut joined = std::mem::take(&mut self.cut);
            joined.extend_from_slice(bytes);
            self.advance(&joined);
        }
    }

    fn advance(&mut self, bytes: &[u8]) {
        let (whole, cut) = bytes.split_at(bytes.len() - incomplete_tail(bytes));
        let mut rest = whole;
        while let Some(at) = memchr::memchr(b'\n', rest) {
            self.segment(&rest[..at]);
            self.end_line(true);
            rest = &rest[at + 1..];
        }
        self.segment(rest);
        self.cut.extend_from_slice(cut);
    }

    /// Feeds part of a line to `vte`, up to [`MAX_LINE_BYTES`] for the line.
    fn segment(&mut self, bytes: &[u8]) {
        self.seen += bytes.len();
        let room = MAX_LINE_BYTES.saturating_sub(self.fed);
        let (take, skipped) = bytes.split_at(bytes.len().min(room));
        if !take.is_empty() {
            self.fed += take.len();
            self.dirty |= memchr::memchr(0x1b, take).is_some() || incomplete_tail(take) > 0;
            self.vte.advance(&mut self.lines, take);
        }
        if !skipped.is_empty() {
            self.reset_vte();
        }
    }

    fn reset_vte(&mut self) {
        if self.dirty {
            self.vte = vte::Parser::new();
            self.dirty = false;
        }
    }

    /// Completes the line in progress, marking it when it was cut.
    fn end_line(&mut self, newline: bool) {
        self.reset_vte();
        if self.seen > self.fed {
            self.lines.mark_truncated(self.seen);
        }
        if newline {
            self.lines.overwrite = false;
        }
        self.lines.finish_line();
        self.fed = 0;
        self.seen = 0;
    }

    /// Ends the line in progress, if it has any text: the log is over.
    pub fn finish(&mut self) {
        if !self.lines.text.is_empty() || self.seen > self.fed {
            self.end_line(false);
        }
    }

    /// The lines completed since the last call.
    pub fn take(&mut self) -> Batch {
        std::mem::take(&mut self.lines.out)
    }
}

/// How many bytes at the end of `bytes` are the start of a UTF-8 character
/// that the rest of it would complete.
fn incomplete_tail(bytes: &[u8]) -> usize {
    for back in 1..=bytes.len().min(3) {
        let byte = bytes[bytes.len() - back];
        let needed = match byte {
            0x80..=0xBF => continue,
            0xC0..=0xDF => 2,
            0xE0..=0xEF => 3,
            0xF0..=0xF7 => 4,
            _ => return 0,
        };
        return if needed > back { back } else { 0 };
    }
    0
}

#[derive(Default)]
struct Interner {
    ids: HashMap<Style, StyleId>,
    count: usize,
}

impl Interner {
    /// The id of `style`, adding it to `new` the first time. A log with more
    /// than 65,535 distinct styles gets the default for the rest.
    fn id(&mut self, style: Style, new: &mut Vec<Style>) -> StyleId {
        if style == Style::default() {
            return StyleId::DEFAULT;
        }
        if let Some(&id) = self.ids.get(&style) {
            return id;
        }
        let Ok(next) = u16::try_from(self.count + 1) else {
            return StyleId::DEFAULT;
        };
        self.count += 1;
        let id = StyleId(next);
        self.ids.insert(style, id);
        new.push(style);
        id
    }
}

#[derive(Default)]
struct Lines {
    style: Style,
    interner: Interner,
    /// The style of the text printed since the last span was closed.
    run: StyleId,
    text: String,
    spans: Vec<Span>,
    columns: u32,
    overwrite: bool,
    /// The timestamp of a line that a carriage return then overwrote: it
    /// belongs to the line that replaces it.
    overwritten_time: Option<i64>,
    /// Id of the next line to complete, for group boundaries.
    next_line: u64,
    out: Batch,
}

impl Lines {
    fn close_run(&mut self) {
        let end = self.text.len() as u32;
        let start = self.spans.last().map_or(0, |s| s.end);
        if end > start {
            self.spans.push(Span {
                end,
                style: self.run,
            });
        }
    }

    fn restyle(&mut self, apply: impl FnOnce(&mut Style)) {
        self.close_run();
        apply(&mut self.style);
        self.run = self.interner.id(self.style, &mut self.out.styles);
    }

    fn clear_line(&mut self) {
        self.text.clear();
        self.spans.clear();
        self.columns = 0;
    }

    /// Appends the marker for a line of `bytes` bytes that was cut.
    fn mark_truncated(&mut self, bytes: usize) {
        self.close_run();
        let before = self.text.len();
        if !self.text.is_empty() {
            self.text.push(' ');
        }
        let marker = format!("[line truncated ({bytes} bytes)]");
        self.columns += (self.text.len() - before + marker.chars().count()) as u32;
        self.text.push_str(&marker);
        let style = self.interner.id(TRUNCATED_STYLE, &mut self.out.styles);
        self.spans.push(Span {
            end: self.text.len() as u32,
            style,
        });
    }

    /// Completes the line in progress.
    fn finish_line(&mut self) {
        self.close_run();
        while self
            .spans
            .last()
            .is_some_and(|s| s.style == StyleId::DEFAULT)
        {
            self.spans.pop();
        }
        let carried = self.overwritten_time.take();
        let (time_ms, mut cut) = match timestamp::parse_prefix(&self.text) {
            Some((time, len)) => (Some(time), len),
            None => (carried, 0),
        };
        let rest = &self.text[cut..];
        let mut kind = LineKind::Text;
        if let Some(rest) = rest.strip_prefix(GROUP) {
            kind = LineKind::GroupStart;
            cut = self.text.len() - rest.len();
        } else if let Some(rest) = rest.strip_prefix(ERROR) {
            kind = LineKind::Error;
            cut = self.text.len() - rest.len();
        } else if let Some(rest) = rest.strip_prefix(WARNING) {
            kind = LineKind::Warning;
            cut = self.text.len() - rest.len();
        } else if rest.trim_end() == END_GROUP {
            self.out.groups.push(GroupEvent::End(self.next_line));
            self.clear_line();
            return;
        }
        if cut > 0 {
            self.text.drain(..cut);
            self.spans
                .iter_mut()
                .for_each(|s| s.end = s.end.saturating_sub(cut as u32));
            self.spans.retain(|s| s.end > 0);
        }
        if kind == LineKind::GroupStart {
            self.out.groups.push(GroupEvent::Start(self.next_line));
        }
        self.next_line += 1;
        self.out.lines.push(LineMeta::new(
            self.out.text.len(),
            self.out.spans.len(),
            time_ms,
            kind,
        ));
        self.out.text.push_str(&self.text);
        self.out.spans.extend_from_slice(&self.spans);
        self.out.max_columns = self.out.max_columns.max(self.columns);
        self.clear_line();
    }
}

impl Perform for Lines {
    fn print(&mut self, c: char) {
        if self.overwrite {
            if self.overwritten_time.is_none() {
                self.overwritten_time = timestamp::parse_prefix(&self.text).map(|(time, _)| time);
            }
            self.clear_line();
            self.overwrite = false;
        }
        self.text.push(c);
        self.columns += 1;
    }

    fn execute(&mut self, byte: u8) {
        match byte {
            b'\r' => self.overwrite = true,
            b'\t' => self.print('\t'),
            _ => {}
        }
    }

    fn csi_dispatch(&mut self, params: &Params, intermediates: &[u8], ignore: bool, action: char) {
        if action == 'm' && intermediates.is_empty() && !ignore {
            self.restyle(|style| apply_sgr(style, params));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ansi::{Attrs, Color};
    use crate::buffer::LogBuffer;

    /// Parses `chunks` in turn and returns the buffer.
    fn parse(chunks: &[&[u8]]) -> LogBuffer {
        let mut buffer = LogBuffer::new();
        let mut parser = LogParser::new();
        for chunk in chunks {
            buffer.append(&mut parser, chunk);
        }
        parser.finish();
        buffer.push(&mut parser.take());
        buffer
    }

    fn texts(buffer: &LogBuffer) -> Vec<String> {
        buffer
            .lines(0..buffer.total())
            .map(|l| l.text.to_owned())
            .collect()
    }

    /// (text, style) of each run of line `id`.
    fn runs(buffer: &LogBuffer, id: u64) -> Vec<(String, Style)> {
        let line = buffer.line(id).expect("line exists");
        let mut start = 0;
        line.spans
            .iter()
            .map(|span| {
                let text = line.text[start..span.end as usize].to_owned();
                start = span.end as usize;
                (text, buffer.style(span.style))
            })
            .collect()
    }

    #[test]
    fn plain_lines_split_on_newlines_and_crlf() {
        let buffer = parse(&[b"one\ntwo\r\nthree\n"]);
        assert_eq!(texts(&buffer), ["one", "two", "three"]);
        assert!(buffer.line(0).is_some_and(|l| l.spans.is_empty()));
    }

    #[test]
    fn a_line_split_across_chunks_is_joined() {
        let buffer = parse(&[b"hel", b"lo wor", b"ld\nnext\n"]);
        assert_eq!(texts(&buffer), ["hello world", "next"]);
    }

    #[test]
    fn an_escape_split_across_chunks_still_colors() {
        for cut in 1..b"\x1b[31;1mred\x1b[0m done\n".len() {
            let all = b"\x1b[31;1mred\x1b[0m done\n";
            let buffer = parse(&[&all[..cut], &all[cut..]]);
            assert_eq!(texts(&buffer), ["red done"], "cut at {cut}");
            let runs = runs(&buffer, 0);
            assert_eq!(runs.len(), 1, "cut at {cut}");
            assert_eq!(runs[0].0, "red");
            assert_eq!(runs[0].1.fg, Color::Indexed(1));
            assert!(runs[0].1.attrs.contains(Attrs::BOLD));
        }
    }

    #[test]
    fn a_utf8_character_split_across_chunks_survives() {
        let bytes = "caf\u{e9} \u{2713}\n".as_bytes();
        for cut in 1..bytes.len() {
            let buffer = parse(&[&bytes[..cut], &bytes[cut..]]);
            assert_eq!(texts(&buffer), ["caf\u{e9} \u{2713}"], "cut at {cut}");
        }
    }

    #[test]
    fn sgr_covers_16_256_and_true_color() {
        let buffer = parse(&[
            b"\x1b[92ma\x1b[0m\x1b[38;5;208mb\x1b[48;2;1;2;3mc\x1b[38:2::9:8:7md\x1b[mz\n",
        ]);
        let runs = runs(&buffer, 0);
        assert_eq!(runs[0].1.fg, Color::Indexed(10));
        assert_eq!(runs[1].1.fg, Color::Indexed(208));
        assert_eq!(runs[2].1.bg, Color::Rgb(1, 2, 3));
        assert_eq!(runs[2].1.fg, Color::Indexed(208));
        assert_eq!(runs[3].1.fg, Color::Rgb(9, 8, 7));
        // `z` follows a bare reset, so it is unstyled and has no span.
        assert_eq!(runs.len(), 4);
        assert_eq!(buffer.line(0).map(|l| l.text), Some("abcdz"));
    }

    #[test]
    fn attributes_set_and_clear() {
        let buffer = parse(&[b"\x1b[1;3;4;7mx\x1b[22;23;24;27my\x1b[2mz\n"]);
        let runs = runs(&buffer, 0);
        let all = Attrs::BOLD;
        assert!(runs[0].1.attrs.contains(all));
        assert!(runs[0].1.attrs.contains(Attrs::ITALIC));
        assert!(runs[0].1.attrs.contains(Attrs::UNDERLINE));
        assert!(runs[0].1.attrs.contains(Attrs::INVERSE));
        assert_eq!(runs.len(), 3);
        assert_eq!(runs[1].1, Style::default());
        assert!(runs[2].1.attrs.contains(Attrs::DIM));
    }

    #[test]
    fn style_carries_over_to_the_next_line() {
        let buffer = parse(&[b"\x1b[33mone\ntwo\x1b[0m\n"]);
        assert_eq!(runs(&buffer, 0)[0].1.fg, Color::Indexed(3));
        assert_eq!(runs(&buffer, 1)[0].1.fg, Color::Indexed(3));
    }

    #[test]
    fn other_escapes_are_stripped() {
        let buffer = parse(&[b"a\x1b[2Kb\x1b[1;1Hc\x1b]0;title\x07d\x1b(Be\n"]);
        assert_eq!(texts(&buffer), ["abcde"]);
    }

    #[test]
    fn a_carriage_return_overwrites_the_line() {
        let buffer = parse(&[b"progress 10%\rprogress 55%\rdone\n", b"kept\r\n"]);
        assert_eq!(texts(&buffer), ["done", "kept"]);
    }

    #[test]
    fn an_overwritten_line_keeps_its_timestamp() {
        let buffer = parse(&[b"2024-05-01T12:00:00Z 10%\r55%\r100%\n"]);
        assert_eq!(texts(&buffer), ["100%"]);
        assert_eq!(
            buffer.line(0).and_then(|l| l.time_ms),
            Some(1_714_564_800_000)
        );
    }

    #[test]
    fn an_overwrite_across_chunks_and_with_color() {
        let buffer = parse(&[b"old text\r", b"\x1b[32mnew\x1b[0m\n"]);
        assert_eq!(texts(&buffer), ["new"]);
        assert_eq!(runs(&buffer, 0)[0].1.fg, Color::Indexed(2));
    }

    #[test]
    fn the_last_line_without_a_newline_is_flushed_by_finish() {
        let buffer = parse(&[b"a\nb"]);
        assert_eq!(texts(&buffer), ["a", "b"]);
        let mut buffer = LogBuffer::new();
        let mut parser = LogParser::new();
        buffer.append(&mut parser, b"a\nb");
        assert_eq!(buffer.len(), 1);
    }

    #[test]
    fn timestamps_move_to_their_own_column() {
        let buffer = parse(&[
            b"2024-05-01T12:34:56.5Z \x1b[36;1mrun\x1b[0m it\n2024-05-01T12:34:57Z plain\nno stamp\n",
        ]);
        assert_eq!(texts(&buffer), ["run it", "plain", "no stamp"]);
        assert_eq!(
            buffer.line(0).and_then(|l| l.time_ms),
            Some(1_714_566_896_500)
        );
        assert_eq!(buffer.line(2).and_then(|l| l.time_ms), None);
        let runs = runs(&buffer, 0);
        assert_eq!(runs[0].0, "run");
        assert_eq!(runs[0].1.fg, Color::Indexed(6));
    }

    #[test]
    fn groups_open_and_close() {
        let buffer = parse(&[
            b"before\n##[group]Run tests\none\ntwo\n##[endgroup]\nafter\n",
            b"2024-05-01T12:00:00Z ##[group]Second\nx\n",
        ]);
        assert_eq!(
            texts(&buffer),
            ["before", "Run tests", "one", "two", "after", "Second", "x"]
        );
        let groups = buffer.groups();
        assert_eq!(groups.len(), 2);
        assert_eq!((groups[0].start, groups[0].end), (1, Some(4)));
        assert_eq!((groups[1].start, groups[1].end), (5, None));
        assert_eq!(buffer.line(1).map(|l| l.kind), Some(LineKind::GroupStart));
        assert_eq!(buffer.group_containing(3).map(|g| g.start), Some(1));
        assert_eq!(buffer.group_containing(4), None);
        assert_eq!(buffer.group_containing(6).map(|g| g.start), Some(5));
        assert_eq!(groups[0].body(buffer.total()), 2..4);
    }

    #[test]
    fn a_new_group_closes_the_open_one() {
        let buffer = parse(&[b"##[group]a\nx\n##[group]b\ny\n"]);
        let groups = buffer.groups();
        assert_eq!((groups[0].start, groups[0].end), (0, Some(2)));
        assert_eq!((groups[1].start, groups[1].end), (2, None));
    }

    #[test]
    fn errors_and_warnings_lose_their_marker() {
        let buffer = parse(&[b"##[error]boom\n##[warning]careful\n"]);
        assert_eq!(texts(&buffer), ["boom", "careful"]);
        assert_eq!(buffer.line(0).map(|l| l.kind), Some(LineKind::Error));
        assert_eq!(buffer.line(1).map(|l| l.kind), Some(LineKind::Warning));
    }

    const TEN: [&str; 10] = ["l0", "l1", "l2", "l3", "l4", "l5", "l6", "l7", "l8", "l9"];

    fn ten_lines() -> Vec<u8> {
        TEN.iter()
            .flat_map(|l| format!("{l}\n").into_bytes())
            .collect()
    }

    #[test]
    fn an_unterminated_escape_does_not_hide_later_lines() {
        let openers: [&[u8]; 10] = [
            b"\x1b]0;title",
            b"\x1b]8;;http://example.com",
            b"\x1bPq data",
            b"\x1b_apc",
            b"\x1b^pm",
            b"\x1bXsos",
            b"\x1b[31;",
            b"\x1b[",
            b"\x1b",
            b"\x1b(",
        ];
        for opener in openers {
            let mut input = opener.to_vec();
            input.push(b'\n');
            input.extend(ten_lines());
            let got = texts(&parse(&[&input]));
            let name = String::from_utf8_lossy(opener);
            assert_eq!(got.len(), 11, "{name}: {got:?}");
            assert_eq!(got[1..], TEN, "{name}");
            assert_eq!(got[0], "", "{name}");
        }
    }

    #[test]
    fn an_unterminated_escape_keeps_the_text_before_it() {
        let buffer = parse(&[b"before \x1b]0;title\nnext\n"]);
        assert_eq!(texts(&buffer), ["before ", "next"]);
    }

    #[test]
    fn an_unterminated_escape_split_across_chunks_does_not_hide_lines() {
        let mut input = b"\x1b]0;title\n".to_vec();
        input.extend(ten_lines());
        for cut in 1..input.len() {
            let buffer = parse(&[&input[..cut], &input[cut..]]);
            assert_eq!(texts(&buffer).len(), 11, "cut at {cut}");
        }
    }

    #[test]
    fn a_sequence_does_not_continue_into_the_next_line() {
        let buffer = parse(&[b"a\x1b]0;t\nb\x07c\n"]);
        assert_eq!(texts(&buffer), ["a", "bc"]);
    }

    #[test]
    fn escape_strings_are_never_shown_as_text() {
        let buffer = parse(&[b"a\x1b]8;;http://x\x07link\x1b]8;;\x07\x1bPsecret\x1b\\b\n"]);
        assert_eq!(texts(&buffer), ["alinkb"]);
    }

    #[test]
    fn an_incomplete_character_at_a_newline_does_not_leak_into_the_next_line() {
        let buffer = parse(&[b"caf\xc3\nnext\n"]);
        assert_eq!(texts(&buffer)[1], "next");
    }

    #[test]
    fn overlong_lines_are_cut_with_a_marker() {
        let long = "x".repeat(MAX_LINE_BYTES + 100);
        let buffer = parse(&[long.as_bytes(), b"\nshort\n"]);
        let first = buffer.line(0).expect("first line");
        let marker = format!("[line truncated ({} bytes)]", MAX_LINE_BYTES + 100);
        assert_eq!(
            first.text,
            format!("{} {marker}", "x".repeat(MAX_LINE_BYTES))
        );
        let last = first.spans.last().expect("marker span");
        assert_eq!(last.end as usize, first.text.len());
        assert_ne!(last.style, StyleId::DEFAULT);
        assert_eq!(buffer.line(1).map(|l| l.text), Some("short"));
    }

    #[test]
    fn a_ten_megabyte_line_without_a_newline_is_cut_and_later_lines_appear() {
        let chunk = vec![b'y'; 1 << 20];
        let mut chunks: Vec<&[u8]> = vec![&chunk; 10];
        chunks.push(b"\nafter 1\nafter 2\n");
        let got = texts(&parse(&chunks));
        assert_eq!(got.len(), 3);
        assert!(got[0].starts_with(&"y".repeat(MAX_LINE_BYTES)));
        assert!(got[0].ends_with(&format!("[line truncated ({} bytes)]", 10 << 20)));
        assert!(got[0].len() < MAX_LINE_BYTES + 64);
        assert_eq!(got[1..], ["after 1", "after 2"]);
    }

    #[test]
    fn an_unterminated_escape_past_the_cap_is_cut_too() {
        let mut input = b"\x1b]0;".to_vec();
        input.resize(3 * MAX_LINE_BYTES, b'z');
        input.extend_from_slice(b"\nafter\n");
        let got = texts(&parse(&[&input]));
        assert_eq!(got.len(), 2);
        assert!(got[0].starts_with("[line truncated"));
        assert_eq!(got[1], "after");
    }

    #[test]
    fn a_truncated_last_line_is_marked_by_finish() {
        let long = "x".repeat(MAX_LINE_BYTES + 1);
        let got = texts(&parse(&[long.as_bytes()]));
        assert!(got[0].ends_with("bytes)]"));
    }
}
