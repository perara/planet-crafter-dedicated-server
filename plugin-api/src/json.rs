//! A small JSON reader and writer for what a plugin meets: the `args_json` of an RPC event
//! (`[-12345,[1.5,2.0,-3.25],true]`), the rules (`Event::RuleChange`, `Host::rules`: an object
//! of dotted rule names to values) and the small bodies a plugin sends with `net.http`.
//!
//! The reader is strict (RFC 8259): no comments, no trailing commas, no trailing bytes. It
//! never panics. It stops at nesting 32 and at 1 MiB of text, so a hostile document cannot eat
//! the plugin's memory. Numbers are `f64`, as in JavaScript.
//!
//! ```
//! use pcds_plugin_api::json::{self, Value};
//!
//! let args = json::parse("[-12345,[1.5,2.0,-3.25],true]").unwrap();
//! assert_eq!(args.index(0).and_then(Value::as_i64), Some(-12345));
//! assert_eq!(args.index(1).and_then(Value::vec3), Some([1.5, 2.0, -3.25]));
//!
//! let body = json::Object::new().str("content", "Ann joined").num("players", 3.0).finish();
//! assert_eq!(body, r#"{"content":"Ann joined","players":3}"#);
//! ```

use alloc::string::String;
use alloc::vec::Vec;
use core::fmt;

/// The deepest nesting of arrays and objects the reader takes.
pub const MAX_DEPTH: usize = 32;
/// The most text the reader takes, in bytes.
pub const MAX_BYTES: usize = 1 << 20;

/// A JSON value.
#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    /// `null`.
    Null,
    /// `true` or `false`.
    Bool(bool),
    /// Any number. JSON has one kind; this is the nearest `f64`.
    Number(f64),
    /// A string.
    String(String),
    /// An array.
    Array(Vec<Value>),
    /// An object, as its pairs in the order written. A repeated key stays; `get` finds the last.
    Object(Vec<(String, Value)>),
}

/// Why `parse` refused a text, and where (a byte offset).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ParseError {
    /// The byte offset in the text where the problem is.
    pub offset: usize,
    /// What is wrong, in a few words.
    pub reason: &'static str,
}

impl fmt::Display for ParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "bad JSON at byte {}: {}", self.offset, self.reason)
    }
}

/// Reads one JSON value. The whole text must be that value (spaces around it are fine).
pub fn parse(text: &str) -> Result<Value, ParseError> {
    if text.len() > MAX_BYTES {
        return Err(ParseError { offset: MAX_BYTES, reason: "the text is over 1 MiB" });
    }
    let mut p = Parser { text, at: 0 };
    p.space();
    let value = p.value(0)?;
    p.space();
    if p.at != text.len() {
        return Err(p.fail("bytes after the value"));
    }
    Ok(value)
}

struct Parser<'a> {
    text: &'a str,
    at: usize,
}

impl Parser<'_> {
    fn fail(&self, reason: &'static str) -> ParseError {
        ParseError { offset: self.at, reason }
    }

    fn peek(&self) -> Option<u8> {
        self.text.as_bytes().get(self.at).copied()
    }

    fn space(&mut self) {
        while matches!(self.peek(), Some(b' ' | b'\t' | b'\n' | b'\r')) {
            self.at += 1;
        }
    }

    fn eat(&mut self, byte: u8) -> bool {
        let hit = self.peek() == Some(byte);
        if hit {
            self.at += 1;
        }
        hit
    }

    fn word(&mut self, word: &str, value: Value) -> Result<Value, ParseError> {
        if self.text[self.at..].starts_with(word) {
            self.at += word.len();
            Ok(value)
        } else {
            Err(self.fail("not a JSON value"))
        }
    }

    fn value(&mut self, depth: usize) -> Result<Value, ParseError> {
        match self.peek() {
            None => Err(self.fail("the text ends too soon")),
            Some(b'n') => self.word("null", Value::Null),
            Some(b't') => self.word("true", Value::Bool(true)),
            Some(b'f') => self.word("false", Value::Bool(false)),
            Some(b'"') => self.string().map(Value::String),
            Some(b'[') => self.array(depth),
            Some(b'{') => self.object(depth),
            Some(b'-' | b'0'..=b'9') => self.number(),
            Some(_) => Err(self.fail("not a JSON value")),
        }
    }

    fn array(&mut self, depth: usize) -> Result<Value, ParseError> {
        if depth >= MAX_DEPTH {
            return Err(self.fail("nested too deep"));
        }
        self.at += 1;
        let mut items = Vec::new();
        self.space();
        if self.eat(b']') {
            return Ok(Value::Array(items));
        }
        loop {
            self.space();
            items.push(self.value(depth + 1)?);
            self.space();
            if self.eat(b',') {
                continue;
            }
            if self.eat(b']') {
                return Ok(Value::Array(items));
            }
            return Err(self.fail("expected , or ]"));
        }
    }

    fn object(&mut self, depth: usize) -> Result<Value, ParseError> {
        if depth >= MAX_DEPTH {
            return Err(self.fail("nested too deep"));
        }
        self.at += 1;
        let mut pairs = Vec::new();
        self.space();
        if self.eat(b'}') {
            return Ok(Value::Object(pairs));
        }
        loop {
            self.space();
            if self.peek() != Some(b'"') {
                return Err(self.fail("expected a string key"));
            }
            let key = self.string()?;
            self.space();
            if !self.eat(b':') {
                return Err(self.fail("expected :"));
            }
            self.space();
            pairs.push((key, self.value(depth + 1)?));
            self.space();
            if self.eat(b',') {
                continue;
            }
            if self.eat(b'}') {
                return Ok(Value::Object(pairs));
            }
            return Err(self.fail("expected , or }"));
        }
    }

    fn digits(&mut self) -> usize {
        let start = self.at;
        while matches!(self.peek(), Some(b'0'..=b'9')) {
            self.at += 1;
        }
        self.at - start
    }

    fn number(&mut self) -> Result<Value, ParseError> {
        let start = self.at;
        self.eat(b'-');
        // No leading zeros: 0 alone, or 1-9 and more digits.
        if self.eat(b'0') {
            if matches!(self.peek(), Some(b'0'..=b'9')) {
                return Err(self.fail("a number cannot start with 0 and more digits"));
            }
        } else if self.digits() == 0 {
            return Err(self.fail("a number needs digits"));
        }
        if self.eat(b'.') && self.digits() == 0 {
            return Err(self.fail("digits needed after the dot"));
        }
        if self.eat(b'e') || self.eat(b'E') {
            if !self.eat(b'+') {
                self.eat(b'-');
            }
            if self.digits() == 0 {
                return Err(self.fail("digits needed in the exponent"));
            }
        }
        match self.text[start..self.at].parse::<f64>() {
            Ok(n) if n.is_finite() => Ok(Value::Number(n)),
            _ => Err(ParseError { offset: start, reason: "the number is too big" }),
        }
    }

    fn hex4(&mut self) -> Result<u32, ParseError> {
        let mut n = 0;
        for _ in 0..4 {
            let digit = self.peek().and_then(|b| (b as char).to_digit(16)).ok_or_else(|| self.fail("expected 4 hex digits"))?;
            n = n * 16 + digit;
            self.at += 1;
        }
        Ok(n)
    }

    fn string(&mut self) -> Result<String, ParseError> {
        self.at += 1;
        let mut out = String::new();
        loop {
            // Copy the plain run up to the next quote, backslash or control byte. Those are
            // ASCII, so the run ends on a character boundary.
            let run = self.at;
            while matches!(self.peek(), Some(b) if b != b'"' && b != b'\\' && b >= 0x20) {
                self.at += 1;
            }
            out.push_str(&self.text[run..self.at]);
            match self.peek() {
                None => return Err(self.fail("the string is not closed")),
                Some(b'"') => {
                    self.at += 1;
                    return Ok(out);
                }
                Some(b'\\') => {
                    self.at += 1;
                    let c = match self.peek() {
                        Some(b'"') => '"',
                        Some(b'\\') => '\\',
                        Some(b'/') => '/',
                        Some(b'b') => '\u{8}',
                        Some(b'f') => '\u{c}',
                        Some(b'n') => '\n',
                        Some(b'r') => '\r',
                        Some(b't') => '\t',
                        Some(b'u') => {
                            self.at += 1;
                            out.push(self.unicode()?);
                            continue;
                        }
                        _ => return Err(self.fail("not an escape")),
                    };
                    self.at += 1;
                    out.push(c);
                }
                Some(_) => return Err(self.fail("a control character in a string")),
            }
        }
    }

    /// After `\u`: four hex digits, and for a high surrogate the `\uXXXX` of its low half.
    fn unicode(&mut self) -> Result<char, ParseError> {
        let first = self.hex4()?;
        let code = match first {
            0xD800..=0xDBFF => {
                if !(self.eat(b'\\') && self.eat(b'u')) {
                    return Err(self.fail("a high surrogate needs a low one"));
                }
                let second = self.hex4()?;
                if !(0xDC00..=0xDFFF).contains(&second) {
                    return Err(self.fail("a high surrogate needs a low one"));
                }
                0x10000 + ((first - 0xD800) << 10) + (second - 0xDC00)
            }
            0xDC00..=0xDFFF => return Err(self.fail("a low surrogate on its own")),
            n => n,
        };
        char::from_u32(code).ok_or_else(|| self.fail("not a character"))
    }
}

impl Value {
    /// True for `null`.
    pub fn is_null(&self) -> bool {
        matches!(self, Value::Null)
    }

    /// The number, if this is one.
    pub fn as_f64(&self) -> Option<f64> {
        match self {
            Value::Number(n) => Some(*n),
            _ => None,
        }
    }

    /// The number as an integer, if this is a number with no fraction that fits an `i64`.
    pub fn as_i64(&self) -> Option<i64> {
        let n = self.as_f64()?;
        // 2^63 is the first value past i64::MAX; a NaN fails both tests.
        if (-9_223_372_036_854_775_808.0..9_223_372_036_854_775_808.0).contains(&n) && n as i64 as f64 == n {
            Some(n as i64)
        } else {
            None
        }
    }

    /// The text, if this is a string.
    pub fn as_str(&self) -> Option<&str> {
        match self {
            Value::String(s) => Some(s),
            _ => None,
        }
    }

    /// The flag, if this is `true` or `false`.
    pub fn as_bool(&self) -> Option<bool> {
        match self {
            Value::Bool(b) => Some(*b),
            _ => None,
        }
    }

    /// The items, if this is an array.
    pub fn as_array(&self) -> Option<&[Value]> {
        match self {
            Value::Array(items) => Some(items),
            _ => None,
        }
    }

    /// The pairs, if this is an object.
    pub fn as_object(&self) -> Option<&[(String, Value)]> {
        match self {
            Value::Object(pairs) => Some(pairs),
            _ => None,
        }
    }

    /// The value of a key, if this is an object that has it (the last one, if it is repeated).
    pub fn get(&self, key: &str) -> Option<&Value> {
        self.as_object()?.iter().rev().find(|(k, _)| k == key).map(|(_, v)| v)
    }

    /// An item of an array, if this is an array that long.
    pub fn index(&self, i: usize) -> Option<&Value> {
        self.as_array()?.get(i)
    }

    /// The numbers, if this is an array of numbers and nothing else.
    pub fn numbers(&self) -> Option<Vec<f64>> {
        self.as_array()?.iter().map(Value::as_f64).collect()
    }

    /// A position, if this is an array of exactly three numbers.
    pub fn vec3(&self) -> Option<[f32; 3]> {
        match self.numbers()?[..] {
            [x, y, z] => Some([x as f32, y as f32, z as f32]),
            _ => None,
        }
    }
}

/// Compact JSON. A number with no fraction has no `.0`; NaN and infinity, which JSON cannot
/// say, are written as `null`.
impl fmt::Display for Value {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Value::Null => f.write_str("null"),
            Value::Bool(b) => write!(f, "{b}"),
            Value::Number(n) => write_number(*n, f),
            Value::String(s) => write_escaped(s, f),
            Value::Array(items) => {
                f.write_str("[")?;
                for (i, item) in items.iter().enumerate() {
                    if i > 0 {
                        f.write_str(",")?;
                    }
                    write!(f, "{item}")?;
                }
                f.write_str("]")
            }
            Value::Object(pairs) => {
                f.write_str("{")?;
                for (i, (key, value)) in pairs.iter().enumerate() {
                    if i > 0 {
                        f.write_str(",")?;
                    }
                    write_escaped(key, f)?;
                    write!(f, ":{value}")?;
                }
                f.write_str("}")
            }
        }
    }
}

fn write_number(n: f64, out: &mut dyn fmt::Write) -> fmt::Result {
    if n.is_finite() {
        // Rust's Display for f64 writes 2.0 as `2` and never uses an exponent.
        write!(out, "{n}")
    } else {
        out.write_str("null")
    }
}

fn write_escaped(text: &str, out: &mut dyn fmt::Write) -> fmt::Result {
    out.write_char('"')?;
    for c in text.chars() {
        match c {
            '"' => out.write_str("\\\"")?,
            '\\' => out.write_str("\\\\")?,
            '\n' => out.write_str("\\n")?,
            '\r' => out.write_str("\\r")?,
            '\t' => out.write_str("\\t")?,
            '\u{8}' => out.write_str("\\b")?,
            '\u{c}' => out.write_str("\\f")?,
            c if (c as u32) < 0x20 => write!(out, "\\u{:04x}", c as u32)?,
            c => out.write_char(c)?,
        }
    }
    out.write_char('"')
}

/// Appends `text` to `out` as a JSON string, quotes included, with everything that needs it escaped.
pub fn escape(text: &str, out: &mut String) {
    // Writing to a String cannot fail.
    let _ = write_escaped(text, out);
}

/// Builds a small JSON object, key by key, in the order given. Keys and strings are escaped.
///
/// ```
/// use pcds_plugin_api::json::Object;
///
/// let body = Object::new().str("content", "hi").raw("embeds", "[]").bool("tts", false).finish();
/// assert_eq!(body, r#"{"content":"hi","embeds":[],"tts":false}"#);
/// ```
#[derive(Debug, Clone)]
pub struct Object {
    text: String,
}

impl Default for Object {
    fn default() -> Self {
        Self::new()
    }
}

impl Object {
    /// An object with no keys yet.
    pub fn new() -> Object {
        Object { text: String::from("{") }
    }

    fn key(mut self, key: &str) -> Object {
        if self.text.len() > 1 {
            self.text.push(',');
        }
        escape(key, &mut self.text);
        self.text.push(':');
        self
    }

    /// Adds a string value.
    pub fn str(mut self, key: &str, value: &str) -> Object {
        self = self.key(key);
        escape(value, &mut self.text);
        self
    }

    /// Adds a number. NaN and infinity become `null`.
    pub fn num(mut self, key: &str, value: f64) -> Object {
        self = self.key(key);
        // Writing to a String cannot fail.
        let _ = write_number(value, &mut self.text);
        self
    }

    /// Adds `true` or `false`.
    pub fn bool(mut self, key: &str, value: bool) -> Object {
        self = self.key(key);
        self.text.push_str(if value { "true" } else { "false" });
        self
    }

    /// Adds JSON you already have (a nested object, an array). It is not checked: give it valid JSON.
    pub fn raw(mut self, key: &str, json: &str) -> Object {
        self = self.key(key);
        self.text.push_str(json);
        self
    }

    /// The finished text.
    pub fn finish(mut self) -> String {
        self.text.push('}');
        self.text
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::string::ToString;
    use alloc::vec;

    fn ok(text: &str) -> Value {
        parse(text).unwrap_or_else(|e| panic!("{text:?}: {e}"))
    }

    #[test]
    fn the_example_rpc_arguments_read_as_a_plugin_would_use_them() {
        let args = ok("[-12345,[1.5,2.0,-3.25],[0,0.5,0,1],true,false]");
        assert_eq!(args.as_array().unwrap().len(), 5);
        assert_eq!(args.index(0).unwrap().as_i64(), Some(-12345));
        assert_eq!(args.index(1).unwrap().vec3(), Some([1.5, 2.0, -3.25]));
        assert_eq!(args.index(2).unwrap().numbers(), Some(vec![0.0, 0.5, 0.0, 1.0]));
        assert_eq!(args.index(2).unwrap().vec3(), None);
        assert_eq!(args.index(3).unwrap().as_bool(), Some(true));
        assert_eq!(args.index(4).unwrap().as_bool(), Some(false));
        assert!(args.index(5).is_none());
    }

    #[test]
    fn rules_read_as_an_object_of_dotted_names() {
        let rules = ok(r#"{"speed.growth": 2, "world.pauseWhenEmpty": true, "name": "x"}"#);
        assert_eq!(rules.get("speed.growth").and_then(Value::as_f64), Some(2.0));
        assert_eq!(rules.get("world.pauseWhenEmpty").and_then(Value::as_bool), Some(true));
        assert_eq!(rules.get("name").and_then(Value::as_str), Some("x"));
        assert!(rules.get("missing").is_none());
        assert_eq!(rules.as_object().unwrap().len(), 3);
        // A repeated key: the last wins, and both stay in the pairs.
        let twice = ok(r#"{"a":1,"a":2}"#);
        assert_eq!(twice.get("a").and_then(Value::as_i64), Some(2));
        assert_eq!(twice.as_object().unwrap().len(), 2);
    }

    #[test]
    fn every_accessor_says_none_for_the_wrong_kind() {
        let kinds = [ok("null"), ok("true"), ok("1.5"), ok(r#""s""#), ok("[1]"), ok(r#"{"a":1}"#)];
        let answers = |v: &Value| (v.is_null(), v.as_bool().is_some(), v.as_f64().is_some(), v.as_i64().is_some(), v.as_str().is_some(), v.as_array().is_some(), v.as_object().is_some(), v.get("a").is_some(), v.index(0).is_some(), v.numbers().is_some(), v.vec3().is_some());
        assert_eq!(answers(&kinds[0]), (true, false, false, false, false, false, false, false, false, false, false));
        assert_eq!(answers(&kinds[1]), (false, true, false, false, false, false, false, false, false, false, false));
        assert_eq!(answers(&kinds[2]), (false, false, true, false, false, false, false, false, false, false, false));
        assert_eq!(answers(&kinds[3]), (false, false, false, false, true, false, false, false, false, false, false));
        assert_eq!(answers(&kinds[4]), (false, false, false, false, false, true, false, false, true, true, false));
        assert_eq!(answers(&kinds[5]), (false, false, false, false, false, false, true, true, false, false, false));
        assert_eq!(ok("[1,2,3]").vec3(), Some([1.0, 2.0, 3.0]));
        assert_eq!(ok("[1,2]").vec3(), None);
        assert_eq!(ok(r#"[1,"2",3]"#).numbers(), None);
        assert_eq!(ok("[]").numbers(), Some(vec![]));
    }

    #[test]
    fn as_i64_wants_a_whole_number_in_range() {
        assert_eq!(ok("42").as_i64(), Some(42));
        assert_eq!(ok("-0").as_i64(), Some(0));
        assert_eq!(ok("1e3").as_i64(), Some(1000));
        assert_eq!(ok("2.5").as_i64(), None);
        assert_eq!(ok("-9223372036854775808").as_i64(), Some(i64::MIN));
        assert_eq!(ok("9223372036854775808").as_i64(), None);
        assert_eq!(ok("1e300").as_i64(), None);
        assert_eq!(Value::Number(f64::NAN).as_i64(), None);
    }

    #[test]
    fn numbers_follow_the_grammar() {
        for good in ["0", "-0", "7", "-12", "0.5", "10.25", "1e2", "1E+2", "1e-2", "-1.5e10", "0e0"] {
            assert!(parse(good).is_ok(), "{good}");
        }
        for bad in ["", "-", "+1", "01", "-01", "1.", ".5", "1e", "1e+", "--1", "0x10", "NaN", "Infinity", "1_0", "1e999", "-1e999"] {
            assert!(parse(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn strings_decode_escapes_and_pairs() {
        assert_eq!(ok(r#""a\"b\\c\/d\b\f\n\r\t""#).as_str(), Some("a\"b\\c/d\u{8}\u{c}\n\r\t"));
        assert_eq!(ok(r#""Aå€""#).as_str(), Some("A\u{e5}\u{20ac}"));
        assert_eq!(ok(r#""🚀""#).as_str(), Some("\u{1F680}"));
        assert_eq!(ok("\"hej på deg \u{1F680}\"").as_str(), Some("hej på deg \u{1F680}"));
        for bad in [r#""\ud83d""#, r#""\ude80""#, r#""\ud83dx""#, r#""\ud83dA""#, r#""\u12""#, r#""\u12g4""#, r#""\x""#, r#""abc"#, r#""\"#, "\"a\nb\"", "\"a\u{1}b\"", "'a'"] {
            assert!(parse(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn structure_errors_are_errors() {
        for bad in ["[1,]", "[,1]", "[1 2]", "{,}", r#"{"a":1,}"#, r#"{"a" 1}"#, r#"{a:1}"#, r#"{"a":}"#, "[", "]", "{", "}", "tru", "nul", "True", "[1] x", "1 2", "{} {}", "\u{feff}1", "// c\n1", "[1,2", r#"{"a":1"#] {
            assert!(parse(bad).is_err(), "{bad:?}");
        }
        for good in [" 1 ", "\t\r\n[ 1 , 2 ]\n", r#" { "a" : [ ] , "b" : { } } "#, "[]", "{}", "null", "[[],[[]],{}]"] {
            assert!(parse(good).is_ok(), "{good:?}");
        }
        let e = parse("[1, x]").unwrap_err();
        assert_eq!(e.offset, 4);
        assert!(e.to_string().contains("byte 4"));
    }

    #[test]
    fn nesting_stops_at_32_and_size_at_1_mib() {
        let nested = |n: usize| "[".repeat(n) + &"]".repeat(n);
        assert!(parse(&nested(MAX_DEPTH)).is_ok());
        assert!(parse(&nested(MAX_DEPTH + 1)).is_err());
        let objects = |n: usize| r#"{"a":"#.repeat(n) + "1" + &"}".repeat(n);
        assert!(parse(&objects(MAX_DEPTH)).is_ok());
        assert!(parse(&objects(MAX_DEPTH + 1)).is_err());
        // A hostile text with a hundred thousand brackets fails fast, without deep recursion.
        assert!(parse(&"[".repeat(100_000)).is_err());
        let fits = alloc::format!("\"{}\"", "a".repeat(MAX_BYTES - 2));
        assert_eq!(fits.len(), MAX_BYTES);
        assert!(parse(&fits).is_ok());
        let too_big = alloc::format!("\"{}\"", "a".repeat(MAX_BYTES - 1));
        assert!(parse(&too_big).is_err());
    }

    #[test]
    fn display_is_compact_and_round_trips() {
        for text in [
            "null",
            "true",
            "[]",
            "{}",
            "[-12345,[1.5,2,-3.25],[0,0.5,0,1],true,false]",
            r#"{"speed.growth":2,"a":{"b":[1,2,{"c":null}]},"s":"x\"y\\z\n\u0001"}"#,
            "[0.1,1e21,-1.5e-7]",
            "\"\u{1F680} \u{e5}\"",
        ] {
            let value = ok(text);
            let written = value.to_string();
            assert_eq!(ok(&written), value, "{text}");
            // Writing is a fixed point: a second round gives the same text.
            assert_eq!(ok(&written).to_string(), written);
        }
        assert_eq!(ok("[1.0, 2.50, 1e2, -0]").to_string(), "[1,2.5,100,-0]");
        assert_eq!(ok(r#"{"a" : [ 1 , 2 ]}"#).to_string(), r#"{"a":[1,2]}"#);
        assert_eq!(Value::Array(vec![Value::Number(f64::NAN), Value::Number(f64::INFINITY), Value::Number(f64::NEG_INFINITY)]).to_string(), "[null,null,null]");
        assert_eq!(Value::Number(1e21).to_string(), "1000000000000000000000");
    }

    #[test]
    fn escape_writes_quotes_and_the_needed_escapes() {
        let mut out = String::from("x=");
        escape("a\"b\\c\nd\re\tf\u{8}\u{c}\u{1}\u{1f}\u{7f}g\u{e5}", &mut out);
        assert_eq!(out, "x=\"a\\\"b\\\\c\\nd\\re\\tf\\b\\f\\u0001\\u001f\u{7f}g\u{e5}\"");
        assert_eq!(ok(&out[2..]).as_str(), Some("a\"b\\c\nd\re\tf\u{8}\u{c}\u{1}\u{1f}\u{7f}g\u{e5}"));
    }

    #[test]
    fn the_object_builder_makes_valid_json() {
        assert_eq!(Object::new().finish(), "{}");
        let body = Object::new().str("content", "Ann: \"hi\"\n").num("n", 3.0).num("half", 0.5).num("bad", f64::NAN).bool("b", true).raw("nested", r#"{"x":[1]}"#).str("k\"ey", "v").finish();
        assert_eq!(body, r#"{"content":"Ann: \"hi\"\n","n":3,"half":0.5,"bad":null,"b":true,"nested":{"x":[1]},"k\"ey":"v"}"#);
        let value = ok(&body);
        assert_eq!(value.get("content").and_then(Value::as_str), Some("Ann: \"hi\"\n"));
        assert_eq!(value.get("nested").and_then(|n| n.get("x")).and_then(|x| x.index(0)).and_then(Value::as_i64), Some(1));
        assert_eq!(value.get("k\"ey").and_then(Value::as_str), Some("v"));
    }

    #[test]
    fn random_text_never_panics() {
        // A cheap xorshift fuzz: pieces of JSON in random order, and mutated good documents.
        let pieces = ["[", "]", "{", "}", ",", ":", "\"", "\\", "\\u", "d83d", "de80", "-", "+", ".", "e", "E", "0", "1", "9", "true", "false", "null", " ", "\n", "\u{e5}", "\u{1F680}", "\u{1}", "a", "\\n", "/"];
        let mut x = 0x9E3779B97F4A7C15u64;
        let mut next = move || {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            x
        };
        for _ in 0..30_000 {
            let n = (next() % 24) as usize;
            let text: String = (0..n).map(|_| pieces[(next() % pieces.len() as u64) as usize]).collect();
            if let Ok(v) = parse(&text) {
                assert_eq!(ok(&v.to_string()), v, "{text:?}");
            }
        }
        let good = r#"{"a":[1,2.5,-3e2,"xå🚀"],"b":{"c":null,"d":true}}"#;
        for _ in 0..30_000 {
            let mut bytes = good.as_bytes().to_vec();
            for _ in 0..1 + next() % 3 {
                let at = (next() % bytes.len() as u64) as usize;
                match next() % 3 {
                    0 => bytes[at] = (next() % 128) as u8,
                    1 => {
                        bytes.remove(at);
                    }
                    _ => bytes.insert(at, b"[]{},:\"\\ e-0"[(next() % 11) as usize]),
                }
            }
            if let Ok(text) = core::str::from_utf8(&bytes) {
                let _ = parse(text);
            }
        }
    }
}
