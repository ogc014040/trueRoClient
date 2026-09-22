//! Parser for plain-text (uncompiled) Lua data chunks, the hand-maintained
//! counterpart to the compiled `.lub` files [`crate::lub`] reads. Covers only
//! the literal subset these data dumps use: `name = value` assignments where
//! `value` is a string, number, boolean or `{ ... }` table (array-style,
//! `[N] = ...`, or `ident = ...` entries, freely mixed). No expressions,
//! operators or function calls.
//!
//! String bytes are decoded as Big5 as they're captured, so downstream code
//! sees the same UTF-8 [`Value::Str`] contents regardless of whether they
//! came from this parser or a compiled chunk.

use std::rc::Rc;

use crate::lub::{Key, LubError, LuaState, Value};

/// Rewrites a plain-text Lua data chunk so every string literal (and `--`
/// comment) is UTF-8, keeping everything else — structure, whitespace,
/// even malformed bits this reader can't parse — byte-for-byte as written.
/// Encoding is field-aware the same way [`parse_source_chunk`] is:
/// `identifiedResourceName`/`unidentifiedResourceName` values are read as
/// EUC-KR (these files keep the original Korean GRF texture name there,
/// never localized), everything else as Big5.
///
/// Unlike `parse_source_chunk`, this never fails: a string or comment that
/// runs off the end of the data is copied through as-is rather than
/// dropped, since the point is a lossless re-encoding, not extraction.
pub fn transcode_to_utf8(data: &[u8]) -> Vec<u8> {
    if std::str::from_utf8(data).is_ok() {
        // Already UTF-8 (e.g. re-run on a file this already converted) —
        // nothing to do, and re-scanning would risk the field-aware
        // Big5/EUC-KR decode below mangling text that's already correct.
        return data.to_vec();
    }
    let mut out = Vec::with_capacity(data.len());
    let mut pos = 0usize;
    let mut last_ident: &[u8] = b"";
    while pos < data.len() {
        let b = data[pos];
        if b == b'"' {
            let mut parser = Parser {
                data,
                pos,
                is_utf8: false,
            };
            match parser.scan_string_raw() {
                Ok(raw) => {
                    let (decoded, _, _) = StrEncoding::for_field(last_ident).decoder().decode(&raw);
                    out.push(b'"');
                    out.extend_from_slice(decoded.as_bytes());
                    out.push(b'"');
                    pos = parser.pos;
                }
                Err(_) => {
                    out.extend_from_slice(&data[pos..]);
                    return out;
                }
            }
        } else if data[pos..].starts_with(b"--") {
            let start = pos;
            while pos < data.len() && data[pos] != b'\n' {
                pos += 1;
            }
            let (decoded, _, _) = encoding_rs::BIG5.decode(&data[start..pos]);
            out.extend_from_slice(decoded.as_bytes());
        } else if b.is_ascii_alphabetic() || b == b'_' {
            let start = pos;
            while pos < data.len() && (data[pos].is_ascii_alphanumeric() || data[pos] == b'_') {
                pos += 1;
            }
            last_ident = &data[start..pos];
            out.extend_from_slice(&data[start..pos]);
        } else {
            out.push(b);
            pos += 1;
        }
    }
    out
}

pub fn parse_source_chunk(data: &[u8], state: &mut LuaState) -> Result<(), LubError> {
    let mut parser = Parser {
        data,
        pos: 0,
        is_utf8: std::str::from_utf8(data).is_ok(),
    };
    parser.skip_trivia();
    while parser.pos < parser.data.len() {
        // These files sometimes end with a real `main = function() ... end`
        // that calls `AddItem` for every `tbl` entry — a construct this
        // reader has no use for and can't parse (no function/control-flow
        // support). By this point `tbl` itself is already fully built, so
        // stop rather than erroring out over it, same as the compiled-chunk
        // reader does for a trailing `setmetatable(...)` closure.
        let Some(name) = parser.try_parse_ident() else {
            break;
        };
        parser.skip_trivia();
        if parser.expect(b'=').is_err() {
            break;
        }
        parser.skip_trivia();
        let Ok(value) = parser.parse_value(state) else {
            break;
        };
        state.set_global(&name, value);
        parser.skip_trivia();
    }
    Ok(())
}

#[derive(Clone, Copy)]
enum StrEncoding {
    Big5,
    EucKr,
}

impl StrEncoding {
    fn decoder(self) -> &'static encoding_rs::Encoding {
        match self {
            StrEncoding::Big5 => encoding_rs::BIG5,
            StrEncoding::EucKr => encoding_rs::EUC_KR,
        }
    }

    /// Resource names are the one field these files keep in their original
    /// Korean (EUC-KR) instead of localizing — everything else is Big5.
    fn for_field(name: &[u8]) -> Self {
        if name == b"identifiedResourceName" || name == b"unidentifiedResourceName" {
            StrEncoding::EucKr
        } else {
            StrEncoding::Big5
        }
    }
}

struct Parser<'a> {
    data: &'a [u8],
    pos: usize,
    /// Whether `data` as a whole is valid UTF-8 — checked once per file,
    /// not guessed per string. When true, string content is used verbatim
    /// (no Big5/EUC-KR decode) and high bytes are scanned one at a time
    /// instead of paired: UTF-8 continuation bytes (`0x80..=0xBF`) can
    /// never collide with an ASCII delimiter, unlike Big5/EUC-KR trail
    /// bytes, so the pairing [`Self::scan_string_raw`] otherwise needs
    /// would only risk mis-consuming a real `"`/`,`/`}` that happens to sit
    /// right after a 3- or 4-byte character.
    is_utf8: bool,
}

impl<'a> Parser<'a> {
    fn peek(&self) -> Option<u8> {
        self.data.get(self.pos).copied()
    }

    /// Whitespace and `-- line comments`; the block comment form (`--[[`)
    /// isn't used by these data dumps and isn't handled.
    fn skip_trivia(&mut self) {
        loop {
            while matches!(self.peek(), Some(b' ' | b'\t' | b'\r' | b'\n')) {
                self.pos += 1;
            }
            if self.data[self.pos..].starts_with(b"--") {
                while !matches!(self.peek(), None | Some(b'\n')) {
                    self.pos += 1;
                }
                continue;
            }
            break;
        }
    }

    fn expect(&mut self, byte: u8) -> Result<(), LubError> {
        if self.peek() == Some(byte) {
            self.pos += 1;
            Ok(())
        } else {
            Err(LubError::SyntaxErrorAt(self.pos))
        }
    }

    fn try_parse_ident(&mut self) -> Option<String> {
        let start = self.pos;
        while matches!(self.peek(), Some(b) if b.is_ascii_alphanumeric() || b == b'_') {
            self.pos += 1;
        }
        if self.pos == start {
            return None;
        }
        Some(String::from_utf8_lossy(&self.data[start..self.pos]).into_owned())
    }

    fn parse_value(&mut self, state: &mut LuaState) -> Result<Value, LubError> {
        self.parse_value_enc(state, StrEncoding::Big5)
    }

    /// Like [`Self::parse_value`], but a direct string value is decoded with
    /// `enc` instead of always assuming Big5 — used for the
    /// `identifiedResourceName`/`unidentifiedResourceName` fields, which
    /// these files keep in EUC-KR (they're the original Korean GRF texture
    /// name, never localized). The encoding only applies to this value
    /// itself, not to nested tables (e.g. a description array is always
    /// Big5 regardless of a sibling field's encoding).
    fn parse_value_enc(&mut self, state: &mut LuaState, enc: StrEncoding) -> Result<Value, LubError> {
        match self.peek() {
            Some(b'"') => self.parse_string(enc),
            Some(b'{') => self.parse_table(state),
            Some(b'-' | b'0'..=b'9') => Ok(Value::Number(self.parse_number()?)),
            _ if self.data[self.pos..].starts_with(b"true") => {
                self.pos += 4;
                Ok(Value::Bool(true))
            }
            _ if self.data[self.pos..].starts_with(b"false") => {
                self.pos += 5;
                Ok(Value::Bool(false))
            }
            _ if self.data[self.pos..].starts_with(b"nil") => {
                self.pos += 3;
                Ok(Value::Nil)
            }
            _ => Err(LubError::SyntaxErrorAt(self.pos)),
        }
    }

    fn parse_string(&mut self, enc: StrEncoding) -> Result<Value, LubError> {
        let raw = self.scan_string_raw()?;
        if self.is_utf8 {
            return Ok(Value::Str(Rc::from(raw)));
        }
        let (decoded, _, _) = enc.decoder().decode(&raw);
        Ok(Value::Str(Rc::from(decoded.into_owned().into_bytes())))
    }

    /// A double-quote can never appear as a Big5 trail byte (those fall in
    /// `0x40..=0x7E` or `0xA1..=0xFE`, `"` is `0x22`), so a bare `"` always
    /// safely marks the end of the string. `\"` is ambiguous, though: some
    /// descriptions use it for a literal embedded quote (string continues),
    /// but some display names just end in a literal `\` immediately
    /// followed by the real terminator — `0x5C` is a valid Big5 trail byte,
    /// not always an escape introducer. Disambiguate by peeking past the
    /// candidate quote: if a value separator (`,`/`}`/`;`) follows, this is
    /// the real end and the `\` was ordinary content; otherwise more string
    /// content follows and `\"` is an escaped literal quote. `\\` is
    /// unambiguous (an escaped backslash) and always treated as one.
    ///
    /// Consumes the surrounding quotes and returns the raw (still encoded)
    /// content bytes — decoding is the caller's job, since which encoding
    /// applies depends on which field this string is the value of.
    fn scan_string_raw(&mut self) -> Result<Vec<u8>, LubError> {
        self.expect(b'"')?;
        let mut raw: Vec<u8> = Vec::new();
        loop {
            match self.peek() {
                None => return Err(LubError::UnexpectedEof),
                Some(b'"') => break,
                Some(b'\\') if self.data.get(self.pos + 1) == Some(&b'\\') => {
                    raw.push(b'\\');
                    self.pos += 2;
                }
                Some(b'\\') if self.data.get(self.pos + 1) == Some(&b'"') => {
                    if self.looks_like_value_boundary(self.pos + 2) {
                        raw.push(b'\\');
                        self.pos += 1;
                    } else {
                        raw.push(b'"');
                        self.pos += 2;
                    }
                }
                Some(lead) if lead >= 0x81 && !self.is_utf8 => {
                    raw.push(lead);
                    self.pos += 1;
                    if let Some(trail) = self.peek() {
                        raw.push(trail);
                        self.pos += 1;
                    }
                }
                Some(b) => {
                    raw.push(b);
                    self.pos += 1;
                }
            }
        }
        self.pos += 1;
        Ok(raw)
    }

    /// Whether `pos` sits on a table-entry separator — i.e. whether a `"`
    /// right before it would plausibly be closing a value rather than
    /// sitting mid-string. `}` (or end of data) settles it immediately; a
    /// `,` only counts if a newline follows before any other content, since
    /// this file puts one array element/field per line — a `,` followed by
    /// more same-line text (e.g. `OLv.2\",",`) is just a comma inside the
    /// string, not the real separator.
    fn looks_like_value_boundary(&self, pos: usize) -> bool {
        match self.data.get(pos) {
            None | Some(b'}') => return true,
            Some(b',') => {}
            Some(_) => return false,
        }
        let mut p = pos + 1;
        loop {
            match self.data.get(p) {
                None | Some(b'\n') => return true,
                Some(b' ' | b'\t' | b'\r') => p += 1,
                Some(_) => return false,
            }
        }
    }

    fn parse_number(&mut self) -> Result<f64, LubError> {
        let start = self.pos;
        if self.peek() == Some(b'-') {
            self.pos += 1;
        }
        while matches!(self.peek(), Some(b'0'..=b'9' | b'.')) {
            self.pos += 1;
        }
        std::str::from_utf8(&self.data[start..self.pos])
            .ok()
            .and_then(|s| s.parse().ok())
            .ok_or(LubError::SyntaxErrorAt(start))
    }

    /// Malformed entries (typos like `costume = fals}` turn up in these
    /// large hand-maintained files) are skipped rather than failing the
    /// whole chunk: on error, rewind to the entry's start and resynchronize
    /// at the next separator via [`Self::recover_to_next_entry`], instead
    /// of losing every other entry in the table.
    fn parse_table(&mut self, state: &mut LuaState) -> Result<Value, LubError> {
        self.expect(b'{')?;
        let index = state.new_table();
        let mut next_index: u64 = 1;
        loop {
            self.skip_trivia();
            if self.peek() == Some(b'}') {
                self.pos += 1;
                break;
            }
            let entry_start = self.pos;
            let needs_recovery = match self.parse_entry(state, index, &mut next_index) {
                Ok(()) => {
                    self.skip_trivia();
                    match self.peek() {
                        Some(b',' | b';') => {
                            self.pos += 1;
                            false
                        }
                        Some(b'}') | None => false,
                        // A successfully-parsed value followed by neither a
                        // separator nor the table's own `}` (e.g. a stray
                        // duplicated `"` right after a description line's
                        // real closing quote) means the file has drifted
                        // out of sync here too — resync the same as a
                        // parse failure rather than trying `parse_entry`
                        // again from this nonsensical position.
                        _ => true,
                    }
                }
                Err(_) => {
                    self.pos = entry_start;
                    true
                }
            };
            if needs_recovery && !self.recover_to_next_entry() {
                return Err(LubError::SyntaxErrorAt(entry_start));
            }
        }
        Ok(Value::Table(index))
    }

    fn parse_entry(
        &mut self,
        state: &mut LuaState,
        table_index: usize,
        next_index: &mut u64,
    ) -> Result<(), LubError> {
        let key = if self.peek() == Some(b'[') {
            self.pos += 1;
            self.skip_trivia();
            let n = self.parse_number()?;
            self.skip_trivia();
            self.expect(b']')?;
            self.skip_trivia();
            self.expect(b'=')?;
            self.skip_trivia();
            Key::Number(n.to_bits())
        } else {
            let checkpoint = self.pos;
            match self.try_parse_ident() {
                Some(name) => {
                    self.skip_trivia();
                    if self.peek() == Some(b'=') {
                        self.pos += 1;
                        self.skip_trivia();
                        Key::Str(Rc::from(name.into_bytes()))
                    } else {
                        self.pos = checkpoint;
                        let key = Key::Number((*next_index as f64).to_bits());
                        *next_index += 1;
                        key
                    }
                }
                None => {
                    let key = Key::Number((*next_index as f64).to_bits());
                    *next_index += 1;
                    key
                }
            }
        };
        let enc = match &key {
            Key::Str(name) => StrEncoding::for_field(name),
            Key::Number(_) => StrEncoding::Big5,
        };
        let value = self.parse_value_enc(state, enc)?;
        state
            .table_mut(table_index)
            .ok_or(LubError::TypeError)?
            .insert(key, value);
        Ok(())
    }

    /// Scans forward from a malformed entry, tracking brace depth and
    /// skipping over string literals (approximately — this only needs to
    /// avoid miscounting braces/quotes inside them, not decode them), to the
    /// next `,` at this table's own depth (consumed) or a `}` that closes
    /// it (left in place, for the caller's own end-of-table check). Returns
    /// `false` if data runs out first.
    /// Deliberately does *not* track string literals specially — an earlier
    /// version treated `"` as entering/leaving string mode, but a
    /// mismatched quote in the data (e.g. text opened with `"` and closed
    /// with `'` by mistake) could desync that tracking so badly recovery
    /// ran to the end of the file without ever resyncing, losing
    /// everything parsed so far along with it. Item descriptions in
    /// practice never contain a literal `{`/`}`, so pure brace counting —
    /// still Big5-pair-safe, since a trail byte can equal `{`/`}` — is both
    /// simpler and far more robust here; getting a description's internal
    /// quoting wrong just means [`Self::parse_string`] mis-scoping *that*
    /// value, which is what error recovery exists to route around anyway.
    fn recover_to_next_entry(&mut self) -> bool {
        let mut depth: i32 = 0;
        loop {
            match self.peek() {
                None => return false,
                Some(b'{') => {
                    depth += 1;
                    self.pos += 1;
                }
                Some(b'}') if depth > 0 => {
                    depth -= 1;
                    self.pos += 1;
                }
                Some(b'}') => return true,
                Some(b',') if depth == 0 => {
                    self.pos += 1;
                    return true;
                }
                Some(lead) if lead >= 0x81 && !self.is_utf8 => {
                    self.pos += 1;
                    if self.peek().is_some() {
                        self.pos += 1;
                    }
                }
                Some(_) => self.pos += 1,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn transcode_reencodes_fields_by_name_and_preserves_structure() {
        let (chinese, _, _) = encoding_rs::BIG5.encode("紅色藥水");
        let (korean, _, _) = encoding_rs::EUC_KR.encode("빨간포션");
        let mut src = Vec::new();
        src.extend_from_slice(b"-- header\ntbl = {\n\t[501] = {\n\t\tidentifiedDisplayName = \"");
        src.extend_from_slice(&chinese);
        src.extend_from_slice(b"\",\n\t\tidentifiedResourceName = \"");
        src.extend_from_slice(&korean);
        src.extend_from_slice(b"\",\n\t},\n}\n");

        let out = transcode_to_utf8(&src);
        let text = std::str::from_utf8(&out).expect("output must be valid utf-8");
        assert!(text.contains("identifiedDisplayName = \"紅色藥水\""));
        assert!(text.contains("identifiedResourceName = \"빨간포션\""));
        // structure/whitespace preserved verbatim
        assert!(text.starts_with("-- header\ntbl = {\n\t[501] = {\n"));
        assert!(text.ends_with("\t},\n}\n"));
    }

    #[test]
    fn parses_top_level_assignment_and_nested_table() {
        let src = b"-- comment\ntbl = {\n\t[501] = {\n\t\tname = \"Red Potion\",\n\t\tcosts = { 10, 20, },\n\t\tcostume = false,\n\t},\n}\n";
        let mut state = LuaState::new();
        parse_source_chunk(src, &mut state).unwrap();
        let tbl = state.global_table("tbl").unwrap();
        let item = tbl.get(&Key::Number((501.0f64).to_bits())).unwrap();
        let Value::Table(idx) = item else { panic!() };
        let item = state.table(*idx).unwrap();

        let Value::Str(name) = item.get(&Key::Str(Rc::from(*b"name"))).unwrap() else {
            panic!()
        };
        assert_eq!(&**name, b"Red Potion");

        let Value::Table(costs_idx) = item.get(&Key::Str(Rc::from(*b"costs"))).unwrap() else {
            panic!()
        };
        let costs = state.table(*costs_idx).unwrap();
        assert_eq!(
            costs.get(&Key::Number((1.0f64).to_bits())).unwrap().as_number(),
            Some(10.0)
        );
        assert_eq!(
            costs.get(&Key::Number((2.0f64).to_bits())).unwrap().as_number(),
            Some(20.0)
        );

        assert!(matches!(
            item.get(&Key::Str(Rc::from(*b"costume"))),
            Some(Value::Bool(false))
        ));
    }

    #[test]
    fn handles_escaped_quote_inside_string() {
        let src = br#"tbl = { desc = "say \"hi\" then stop" }"#;
        let mut state = LuaState::new();
        parse_source_chunk(src, &mut state).unwrap();
        let tbl = state.global_table("tbl").unwrap();
        let Value::Str(desc) = tbl.get(&Key::Str(Rc::from(*b"desc"))).unwrap() else {
            panic!()
        };
        assert_eq!(std::str::from_utf8(desc).unwrap(), "say \"hi\" then stop");
    }

    #[test]
    fn escaped_quote_next_to_big5_text_does_not_end_string_early() {
        let (chinese, _, _) = encoding_rs::BIG5.encode("棉襯衫");
        let mut src = b"tbl = { desc = \"".to_vec();
        src.extend_from_slice(&chinese);
        src.extend_from_slice(b" \\\"quoted\\\" ");
        src.extend_from_slice(&chinese);
        src.extend_from_slice(b"\" }");
        let mut state = LuaState::new();
        parse_source_chunk(&src, &mut state).unwrap();
        let tbl = state.global_table("tbl").unwrap();
        let Value::Str(desc) = tbl.get(&Key::Str(Rc::from(*b"desc"))).unwrap() else {
            panic!()
        };
        assert_eq!(
            std::str::from_utf8(desc).unwrap(),
            "棉襯衫 \"quoted\" 棉襯衫"
        );
    }

    #[test]
    fn lone_backslash_before_big5_lead_byte_is_not_an_escape() {
        // "穀\物": 0xbd5c is 穀, a bare 0x5c, then 0xaaab is 物. The lone `\`
        // must not swallow the following Big5 lead byte as a fake escape
        // target — that byte still needs its own trail byte consumed.
        let mut src = b"tbl = { name = \"".to_vec();
        src.extend_from_slice(&[0xbd, 0x5c, 0x5c, 0xaa, 0xab]);
        src.extend_from_slice(b"\" }");
        let mut state = LuaState::new();
        parse_source_chunk(&src, &mut state).unwrap();
        let tbl = state.global_table("tbl").unwrap();
        let Value::Str(name) = tbl.get(&Key::Str(Rc::from(*b"name"))).unwrap() else {
            panic!()
        };
        assert_eq!(std::str::from_utf8(name).unwrap(), "穀\\物");
    }

    #[test]
    fn trailing_backslash_before_real_terminator_is_not_swallowed_as_escape() {
        // A `\"` followed by `,` then a newline (this file's one-field-per-
        // line convention marking a real separator, see
        // `looks_like_value_boundary`) means the backslash was ordinary
        // content and this quote really is the end — as opposed to `\""`
        // (see handles_escaped_quote_inside_string) where a second quote
        // follows on the same line.
        let mut src = Vec::new();
        src.extend_from_slice(b"tbl = { name = \"ends with backslash\\");
        src.push(b'"');
        src.extend_from_slice(b",\n\tnext = 1 }");
        let mut state = LuaState::new();
        parse_source_chunk(&src, &mut state).unwrap();
        let tbl = state.global_table("tbl").unwrap();
        let Value::Str(name) = tbl.get(&Key::Str(Rc::from(*b"name"))).unwrap() else {
            panic!()
        };
        assert_eq!(std::str::from_utf8(name).unwrap(), "ends with backslash\\");
        assert_eq!(
            tbl.get(&Key::Str(Rc::from(*b"next"))).unwrap().as_number(),
            Some(1.0)
        );
    }

    #[test]
    fn comma_right_after_escaped_quote_without_newline_is_still_content() {
        // `..."防禦力提昇OLv.2",",` — the closing `\"` is followed by a comma
        // that's part of the string's own text (no newline before the next
        // `"`), so it must NOT be mistaken for the array separator; the
        // bare `"` right after that comma is the real terminator.
        let src = b"tbl = { d = { \"can use \\\"Buff\\\",\", \"next line\" } }";
        let mut state = LuaState::new();
        parse_source_chunk(src, &mut state).unwrap();
        let tbl = state.global_table("tbl").unwrap();
        let Value::Table(d_idx) = tbl.get(&Key::Str(Rc::from(*b"d"))).unwrap() else {
            panic!()
        };
        let d = state.table(*d_idx).unwrap();
        let Value::Str(first) = d.get(&Key::Number((1.0f64).to_bits())).unwrap() else {
            panic!()
        };
        assert_eq!(std::str::from_utf8(first).unwrap(), "can use \"Buff\",");
        let Value::Str(second) = d.get(&Key::Number((2.0f64).to_bits())).unwrap() else {
            panic!()
        };
        assert_eq!(std::str::from_utf8(second).unwrap(), "next line");
    }

    #[test]
    fn malformed_field_is_skipped_not_fatal() {
        // Recovery kicks in at whichever level the error actually occurs —
        // here that's the field inside item [2], not item [2] itself, so
        // just `name` is dropped from it rather than losing the whole item.
        let src = b"tbl = {\n\t[1] = { name = \"one\" },\n\t[2] = { name = fals },\n\t[3] = { name = \"three\" },\n}";
        let mut state = LuaState::new();
        parse_source_chunk(src, &mut state).unwrap();
        let tbl = state.global_table("tbl").unwrap();
        assert_eq!(tbl.len(), 3, "every item id should still be present");

        let name_of = |id: u16| -> Option<String> {
            let Value::Table(idx) = tbl.get(&Key::Number((id as f64).to_bits())).unwrap() else {
                panic!()
            };
            let inner = state.table(*idx).unwrap();
            inner.get(&Key::Str(Rc::from(*b"name"))).map(|v| {
                let Value::Str(s) = v else { panic!() };
                std::str::from_utf8(s).unwrap().to_string()
            })
        };
        assert_eq!(name_of(1), Some("one".to_string()));
        assert_eq!(name_of(2), None, "the malformed `fals` field should be dropped");
        assert_eq!(name_of(3), Some("three".to_string()));
    }

    #[test]
    fn malformed_item_is_skipped_when_the_whole_value_is_unparseable() {
        // Here the error happens while parsing item [2]'s own value (`fals`
        // where a `{...}` table was expected), one level up from the
        // previous test, so this time the whole item is dropped.
        let src = b"tbl = {\n\t[1] = { name = \"one\" },\n\t[2] = fals,\n\t[3] = { name = \"three\" },\n}";
        let mut state = LuaState::new();
        parse_source_chunk(src, &mut state).unwrap();
        let tbl = state.global_table("tbl").unwrap();
        assert_eq!(tbl.len(), 2);
        assert!(tbl.get(&Key::Number((2.0f64).to_bits())).is_none());
        assert!(tbl.get(&Key::Number((1.0f64).to_bits())).is_some());
        assert!(tbl.get(&Key::Number((3.0f64).to_bits())).is_some());
    }

    #[test]
    fn decodes_big5_string_content() {
        let (encoded, _, _) = encoding_rs::BIG5.encode("棉襯衫");
        let mut src = b"tbl = { name = \"".to_vec();
        src.extend_from_slice(&encoded);
        src.extend_from_slice(b"\" }\n");
        let mut state = LuaState::new();
        parse_source_chunk(&src, &mut state).unwrap();
        let tbl = state.global_table("tbl").unwrap();
        let Value::Str(name) = tbl.get(&Key::Str(Rc::from(*b"name"))).unwrap() else {
            panic!()
        };
        assert_eq!(std::str::from_utf8(name).unwrap(), "棉襯衫");
    }
}
