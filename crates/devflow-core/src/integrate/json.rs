//! A small ordered JSON tree for editing host files additively.
//!
//! `serde_json::Value` sorts object keys, so reading and writing a person's
//! `t3.json` or `.claude/settings.json` through it would reorder their file.
//! This tree keeps the order of every object and the text of every number, so
//! what devflow did not touch comes back as it was (formatting aside: the file
//! is printed with two-space indentation). Comments are not JSON and are not
//! accepted; a file that has them is reported, never rewritten.
use std::fmt::Write as _;

#[derive(Debug, Clone, PartialEq)]
pub enum Json {
    Null,
    Bool(bool),
    /// The number as written.
    Number(String),
    String(String),
    Array(Vec<Json>),
    Object(Vec<(String, Json)>),
}

/// Why a document is not JSON, by position (never by content).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParseError {
    pub line: usize,
    pub column: usize,
    pub what: &'static str,
}

impl std::fmt::Display for ParseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{} at line {}, column {}",
            self.what, self.line, self.column
        )
    }
}

impl Json {
    pub fn object() -> Json {
        Json::Object(Vec::new())
    }

    pub fn string(text: impl Into<String>) -> Json {
        Json::String(text.into())
    }

    pub fn get(&self, key: &str) -> Option<&Json> {
        match self {
            Json::Object(members) => members.iter().find(|(k, _)| k == key).map(|(_, v)| v),
            _ => None,
        }
    }

    pub fn get_mut(&mut self, key: &str) -> Option<&mut Json> {
        match self {
            Json::Object(members) => members.iter_mut().find(|(k, _)| k == key).map(|(_, v)| v),
            _ => None,
        }
    }

    /// Set `key`, keeping its place when it exists and appending it when not.
    pub fn set(&mut self, key: &str, value: Json) {
        if let Json::Object(members) = self {
            match members.iter_mut().find(|(k, _)| k == key) {
                Some((_, slot)) => *slot = value,
                None => members.push((key.to_string(), value)),
            }
        }
    }

    pub fn remove(&mut self, key: &str) -> Option<Json> {
        match self {
            Json::Object(members) => members
                .iter()
                .position(|(k, _)| k == key)
                .map(|at| members.remove(at).1),
            _ => None,
        }
    }

    pub fn as_str(&self) -> Option<&str> {
        match self {
            Json::String(text) => Some(text),
            _ => None,
        }
    }

    pub fn as_bool(&self) -> Option<bool> {
        match self {
            Json::Bool(value) => Some(*value),
            _ => None,
        }
    }

    pub fn as_array(&self) -> Option<&Vec<Json>> {
        match self {
            Json::Array(items) => Some(items),
            _ => None,
        }
    }

    pub fn as_array_mut(&mut self) -> Option<&mut Vec<Json>> {
        match self {
            Json::Array(items) => Some(items),
            _ => None,
        }
    }

    pub fn is_object(&self) -> bool {
        matches!(self, Json::Object(_))
    }

    pub fn is_empty_container(&self) -> bool {
        match self {
            Json::Array(items) => items.is_empty(),
            Json::Object(members) => members.is_empty(),
            _ => false,
        }
    }

    pub fn parse(text: &str) -> Result<Json, ParseError> {
        let mut parser = Parser {
            text,
            bytes: text.as_bytes(),
            at: 0,
            depth: 0,
        };
        parser.space();
        let value = parser.value()?;
        parser.space();
        if parser.at != parser.bytes.len() {
            return Err(parser.error("unexpected text after the document"));
        }
        Ok(value)
    }

    /// Two-space indented, with a final newline.
    pub fn to_pretty(&self) -> String {
        let mut out = String::new();
        self.write(&mut out, 0);
        out.push('\n');
        out
    }

    fn write(&self, out: &mut String, indent: usize) {
        match self {
            Json::Null => out.push_str("null"),
            Json::Bool(true) => out.push_str("true"),
            Json::Bool(false) => out.push_str("false"),
            Json::Number(text) => out.push_str(text),
            Json::String(text) => write_string(out, text),
            Json::Array(items) if items.is_empty() => out.push_str("[]"),
            Json::Array(items) => {
                out.push_str("[\n");
                for (index, item) in items.iter().enumerate() {
                    pad(out, indent + 1);
                    item.write(out, indent + 1);
                    out.push_str(if index + 1 == items.len() {
                        "\n"
                    } else {
                        ",\n"
                    });
                }
                pad(out, indent);
                out.push(']');
            }
            Json::Object(members) if members.is_empty() => out.push_str("{}"),
            Json::Object(members) => {
                out.push_str("{\n");
                for (index, (key, value)) in members.iter().enumerate() {
                    pad(out, indent + 1);
                    write_string(out, key);
                    out.push_str(": ");
                    value.write(out, indent + 1);
                    out.push_str(if index + 1 == members.len() {
                        "\n"
                    } else {
                        ",\n"
                    });
                }
                pad(out, indent);
                out.push('}');
            }
        }
    }
}

fn pad(out: &mut String, level: usize) {
    for _ in 0..level {
        out.push_str("  ");
    }
}

fn write_string(out: &mut String, text: &str) {
    out.push('"');
    for c in text.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => {
                let _ = write!(out, "\\u{:04x}", c as u32);
            }
            c => out.push(c),
        }
    }
    out.push('"');
}

/// Deeper than any host file; a bound against a hostile document.
const MAX_DEPTH: usize = 64;

struct Parser<'a> {
    text: &'a str,
    bytes: &'a [u8],
    at: usize,
    depth: usize,
}

impl Parser<'_> {
    fn error(&self, what: &'static str) -> ParseError {
        let before = &self.text[..self.at.min(self.text.len())];
        let line = before.matches('\n').count() + 1;
        let column = before.rsplit('\n').next().map_or(0, |l| l.chars().count()) + 1;
        ParseError { line, column, what }
    }

    fn space(&mut self) {
        while matches!(self.bytes.get(self.at), Some(b' ' | b'\t' | b'\n' | b'\r')) {
            self.at += 1;
        }
    }

    fn peek(&self) -> Option<u8> {
        self.bytes.get(self.at).copied()
    }

    fn eat(&mut self, byte: u8) -> bool {
        if self.peek() == Some(byte) {
            self.at += 1;
            true
        } else {
            false
        }
    }

    fn word(&mut self, word: &str, value: Json) -> Result<Json, ParseError> {
        if self.bytes[self.at..].starts_with(word.as_bytes()) {
            self.at += word.len();
            Ok(value)
        } else {
            Err(self.error("unexpected text"))
        }
    }

    fn value(&mut self) -> Result<Json, ParseError> {
        match self.peek() {
            None => Err(self.error("the document ends early")),
            Some(b'{') => self.object(),
            Some(b'[') => self.array(),
            Some(b'"') => self.string().map(Json::String),
            Some(b't') => self.word("true", Json::Bool(true)),
            Some(b'f') => self.word("false", Json::Bool(false)),
            Some(b'n') => self.word("null", Json::Null),
            Some(b'-' | b'0'..=b'9') => self.number(),
            Some(b'/') => Err(self.error("comments are not JSON")),
            Some(_) => Err(self.error("unexpected text")),
        }
    }

    fn enter(&mut self) -> Result<(), ParseError> {
        self.depth += 1;
        if self.depth > MAX_DEPTH {
            return Err(self.error("nested too deeply"));
        }
        Ok(())
    }

    fn object(&mut self) -> Result<Json, ParseError> {
        self.enter()?;
        self.at += 1;
        let mut members: Vec<(String, Json)> = Vec::new();
        self.space();
        if self.eat(b'}') {
            self.depth -= 1;
            return Ok(Json::Object(members));
        }
        loop {
            self.space();
            if self.peek() != Some(b'"') {
                return Err(self.error("expected a key"));
            }
            let key = self.string()?;
            self.space();
            if !self.eat(b':') {
                return Err(self.error("expected ':'"));
            }
            self.space();
            let value = self.value()?;
            // A repeated key keeps its last value, as parsers do; the first
            // place is kept.
            match members.iter_mut().find(|(k, _)| *k == key) {
                Some((_, slot)) => *slot = value,
                None => members.push((key, value)),
            }
            self.space();
            if self.eat(b',') {
                continue;
            }
            if self.eat(b'}') {
                self.depth -= 1;
                return Ok(Json::Object(members));
            }
            return Err(self.error("expected ',' or '}'"));
        }
    }

    fn array(&mut self) -> Result<Json, ParseError> {
        self.enter()?;
        self.at += 1;
        let mut items = Vec::new();
        self.space();
        if self.eat(b']') {
            self.depth -= 1;
            return Ok(Json::Array(items));
        }
        loop {
            self.space();
            items.push(self.value()?);
            self.space();
            if self.eat(b',') {
                continue;
            }
            if self.eat(b']') {
                self.depth -= 1;
                return Ok(Json::Array(items));
            }
            return Err(self.error("expected ',' or ']'"));
        }
    }

    fn number(&mut self) -> Result<Json, ParseError> {
        let start = self.at;
        self.eat(b'-');
        let digits = |p: &mut Self| {
            let from = p.at;
            while matches!(p.peek(), Some(b'0'..=b'9')) {
                p.at += 1;
            }
            p.at > from
        };
        if !digits(self) {
            return Err(self.error("expected a digit"));
        }
        if self.eat(b'.') && !digits(self) {
            return Err(self.error("expected a digit"));
        }
        if matches!(self.peek(), Some(b'e' | b'E')) {
            self.at += 1;
            if matches!(self.peek(), Some(b'+' | b'-')) {
                self.at += 1;
            }
            if !digits(self) {
                return Err(self.error("expected a digit"));
            }
        }
        Ok(Json::Number(self.text[start..self.at].to_string()))
    }

    fn string(&mut self) -> Result<String, ParseError> {
        self.at += 1;
        let mut out = String::new();
        loop {
            let Some(byte) = self.peek() else {
                return Err(self.error("the string is not closed"));
            };
            match byte {
                b'"' => {
                    self.at += 1;
                    return Ok(out);
                }
                b'\\' => {
                    self.at += 1;
                    let Some(escape) = self.peek() else {
                        return Err(self.error("the string is not closed"));
                    };
                    self.at += 1;
                    match escape {
                        b'"' => out.push('"'),
                        b'\\' => out.push('\\'),
                        b'/' => out.push('/'),
                        b'b' => out.push('\u{8}'),
                        b'f' => out.push('\u{c}'),
                        b'n' => out.push('\n'),
                        b'r' => out.push('\r'),
                        b't' => out.push('\t'),
                        b'u' => out.push(self.unicode()?),
                        _ => return Err(self.error("unknown escape")),
                    }
                }
                0..=0x1f => return Err(self.error("a control character in a string")),
                _ => {
                    // A whole UTF-8 character.
                    let rest = &self.text[self.at..];
                    let c = rest.chars().next().unwrap_or('\u{fffd}');
                    out.push(c);
                    self.at += c.len_utf8();
                }
            }
        }
    }

    fn hex4(&mut self) -> Result<u32, ParseError> {
        let digits = self
            .text
            .get(self.at..self.at + 4)
            .ok_or_else(|| self.error("a short \\u escape"))?;
        let value = u32::from_str_radix(digits, 16).map_err(|_| self.error("a bad \\u escape"))?;
        self.at += 4;
        Ok(value)
    }

    fn unicode(&mut self) -> Result<char, ParseError> {
        let first = self.hex4()?;
        if (0xd800..0xdc00).contains(&first) {
            if self.bytes[self.at..].starts_with(b"\\u") {
                self.at += 2;
                let second = self.hex4()?;
                if (0xdc00..0xe000).contains(&second) {
                    let code = 0x10000 + ((first - 0xd800) << 10) + (second - 0xdc00);
                    return char::from_u32(code).ok_or_else(|| self.error("a bad \\u escape"));
                }
            }
            return Err(self.error("a lone surrogate"));
        }
        char::from_u32(first).ok_or_else(|| self.error("a lone surrogate"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keeps_key_order_numbers_and_unicode() {
        let text = r#"{ "b": 1.50e2, "a": [true, null, "xé😀\n"], "c": {} }"#;
        let value = Json::parse(text).unwrap();
        let printed = value.to_pretty();
        assert!(printed.find("\"b\"").unwrap() < printed.find("\"a\"").unwrap());
        assert!(printed.contains("1.50e2"));
        assert!(printed.contains("é😀\\n"));
        assert_eq!(Json::parse(&printed).unwrap(), value);
        assert!(printed.contains("\"c\": {}"));
    }

    #[test]
    fn sets_keep_places_and_removals_work() {
        let mut value = Json::parse(r#"{"a": 1, "b": 2}"#).unwrap();
        value.set("a", Json::string("x"));
        value.set("c", Json::Bool(true));
        assert_eq!(
            value.to_pretty(),
            "{\n  \"a\": \"x\",\n  \"b\": 2,\n  \"c\": true\n}\n"
        );
        assert_eq!(value.remove("b"), Some(Json::Number("2".into())));
        assert_eq!(value.remove("zz"), None);
    }

    #[test]
    fn refuses_what_is_not_json_by_position_only() {
        for (text, what) in [
            ("{\"a\": 1,}", "expected a key"),
            ("// c\n{}", "comments are not JSON"),
            ("{\"a\": 1} x", "unexpected text after the document"),
            ("[1 2]", "expected ',' or ']'"),
            ("\"abc", "the string is not closed"),
            ("", "the document ends early"),
        ] {
            let error = Json::parse(text).unwrap_err();
            assert_eq!(error.what, what, "{text}");
            assert!(!error.to_string().contains("abc"));
        }
        let error = Json::parse("{\n  \"a\": nope\n}").unwrap_err();
        assert_eq!((error.line, error.column), (2, 8));
        assert!(Json::parse(&"[".repeat(100)).is_err());
    }
}
