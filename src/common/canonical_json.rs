//! RFC 8785 JSON Canonicalization Scheme (JCS) fingerprint helper.
//!
//! Event UID/payload comparison, ingest receipts, and HTTP idempotency all
//! call this module. Do not copy an equivalent implementation elsewhere.

use std::{collections::HashSet, fmt::Write as _};

use sha2::{Digest, Sha256};

use crate::common::errors::MegaError;

/// Matches `serde_json`'s default recursion limit so deep input returns `Err`
/// instead of exhausting the thread stack.
const MAX_NESTING: usize = 128;

/// SHA-256 hex of the RFC 8785 canonical form of `json_text`.
pub fn fingerprint(json_text: &str) -> Result<String, MegaError> {
    let canonical = canonicalize(json_text)?;
    let digest = Sha256::digest(canonical.as_bytes());
    Ok(hex::encode(digest))
}

/// RFC 8785 JCS text (no insignificant whitespace; object members sorted by
/// UTF-16 code units). Rejects duplicate keys, NaN, Infinity, and overflow.
pub fn canonicalize(json_text: &str) -> Result<String, MegaError> {
    let mut parser = Parser {
        input: json_text,
        index: 0,
        depth: 0,
    };
    let value = parser.parse_value()?;
    parser.skip_ws();
    if parser.index != parser.input.len() {
        return Err(reject("trailing data after JSON value"));
    }
    let mut out = String::new();
    value.write_jcs(&mut out)?;
    Ok(out)
}

fn reject(msg: &str) -> MegaError {
    MegaError::Other(format!("canonical JSON rejected input: {msg}"))
}

fn reject_duplicate(key: &str) -> MegaError {
    MegaError::Other(format!("canonical JSON rejected duplicate key: {key:?}"))
}

fn reject_nan() -> MegaError {
    MegaError::Other("canonical JSON rejected NaN".into())
}

fn reject_infinity() -> MegaError {
    MegaError::Other("canonical JSON rejected Infinity".into())
}

fn reject_overflow(lexeme: &str) -> MegaError {
    MegaError::Other(format!("canonical JSON rejected overflow number: {lexeme}"))
}

#[derive(Debug)]
enum Canonical {
    Null,
    Bool(bool),
    Number(String),
    String(String),
    Array(Vec<Canonical>),
    Object(Vec<(String, Canonical)>),
}

impl Canonical {
    fn write_jcs(&self, out: &mut String) -> Result<(), MegaError> {
        match self {
            Self::Null => out.push_str("null"),
            Self::Bool(true) => out.push_str("true"),
            Self::Bool(false) => out.push_str("false"),
            Self::Number(n) => out.push_str(n),
            Self::String(s) => write_json_string(s, out),
            Self::Array(items) => {
                out.push('[');
                for (i, item) in items.iter().enumerate() {
                    if i > 0 {
                        out.push(',');
                    }
                    item.write_jcs(out)?;
                }
                out.push(']');
            }
            Self::Object(members) => {
                out.push('{');
                for (i, (key, value)) in members.iter().enumerate() {
                    if i > 0 {
                        out.push(',');
                    }
                    write_json_string(key, out);
                    out.push(':');
                    value.write_jcs(out)?;
                }
                out.push('}');
            }
        }
        Ok(())
    }
}

fn write_json_string(s: &str, out: &mut String) {
    out.push('"');
    for ch in s.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\u{08}' => out.push_str("\\b"),
            '\t' => out.push_str("\\t"),
            '\n' => out.push_str("\\n"),
            '\u{0C}' => out.push_str("\\f"),
            '\r' => out.push_str("\\r"),
            ch if (ch as u32) < 0x20 => {
                let _ = write!(out, "\\u{:04x}", ch as u32);
            }
            ch => out.push(ch),
        }
    }
    out.push('"');
}

struct Parser<'a> {
    input: &'a str,
    index: usize,
    depth: usize,
}

impl Parser<'_> {
    fn rest(&self) -> &str {
        &self.input[self.index..]
    }

    fn peek(&self) -> Option<u8> {
        self.rest().as_bytes().first().copied()
    }

    fn bump(&mut self) {
        if !self.rest().is_empty() {
            let ch = self
                .rest()
                .chars()
                .next()
                .map(|c| c.len_utf8())
                .unwrap_or(1);
            self.index += ch;
        }
    }

    fn skip_ws(&mut self) {
        while matches!(self.peek(), Some(b' ' | b'\t' | b'\n' | b'\r')) {
            self.index += 1;
        }
    }

    fn parse_value(&mut self) -> Result<Canonical, MegaError> {
        self.skip_ws();
        match self.peek() {
            Some(b'n') => self.parse_literal("null", Canonical::Null),
            Some(b't') => self.parse_literal("true", Canonical::Bool(true)),
            Some(b'f') => self.parse_literal("false", Canonical::Bool(false)),
            Some(b'N') => {
                if self.rest().starts_with("NaN") {
                    Err(reject_nan())
                } else {
                    Err(reject("unexpected token"))
                }
            }
            Some(b'I') => {
                if self.rest().starts_with("Infinity") {
                    Err(reject_infinity())
                } else {
                    Err(reject("unexpected token"))
                }
            }
            Some(b'"') => Ok(Canonical::String(self.parse_string()?)),
            Some(b'[') => self.parse_array(),
            Some(b'{') => self.parse_object(),
            Some(b'-') if self.rest().starts_with("-Infinity") => Err(reject_infinity()),
            Some(b'-' | b'0'..=b'9') => self.parse_number(),
            Some(_) => Err(reject("unexpected token")),
            None => Err(reject("unexpected end of input")),
        }
    }

    fn parse_literal(&mut self, literal: &str, value: Canonical) -> Result<Canonical, MegaError> {
        if self.rest().starts_with(literal) {
            self.index += literal.len();
            Ok(value)
        } else {
            Err(reject("unexpected token"))
        }
    }

    fn enter_nesting(&mut self) -> Result<(), MegaError> {
        if self.depth >= MAX_NESTING {
            return Err(reject("JSON nesting exceeds 128"));
        }
        self.depth += 1;
        Ok(())
    }

    fn leave_nesting(&mut self) {
        self.depth = self.depth.saturating_sub(1);
    }

    fn parse_array(&mut self) -> Result<Canonical, MegaError> {
        self.enter_nesting()?;
        let result = self.parse_array_body();
        self.leave_nesting();
        result
    }

    fn parse_array_body(&mut self) -> Result<Canonical, MegaError> {
        self.bump();
        self.skip_ws();
        let mut items = Vec::new();
        if self.peek() == Some(b']') {
            self.bump();
            return Ok(Canonical::Array(items));
        }
        loop {
            items.push(self.parse_value()?);
            self.skip_ws();
            match self.peek() {
                Some(b',') => {
                    self.bump();
                }
                Some(b']') => {
                    self.bump();
                    break;
                }
                _ => return Err(reject("expected ',' or ']' in array")),
            }
        }
        Ok(Canonical::Array(items))
    }

    fn parse_object(&mut self) -> Result<Canonical, MegaError> {
        self.enter_nesting()?;
        let result = self.parse_object_body();
        self.leave_nesting();
        result
    }

    fn parse_object_body(&mut self) -> Result<Canonical, MegaError> {
        self.bump();
        self.skip_ws();
        let mut seen = HashSet::new();
        let mut members = Vec::new();
        if self.peek() == Some(b'}') {
            self.bump();
            return Ok(Canonical::Object(members));
        }
        loop {
            self.skip_ws();
            if self.peek() != Some(b'"') {
                return Err(reject("expected object key"));
            }
            let key = self.parse_string()?;
            if !seen.insert(key.clone()) {
                return Err(reject_duplicate(&key));
            }
            self.skip_ws();
            if self.peek() != Some(b':') {
                return Err(reject("expected ':' after object key"));
            }
            self.bump();
            let value = self.parse_value()?;
            members.push((key, value));
            self.skip_ws();
            match self.peek() {
                Some(b',') => {
                    self.bump();
                }
                Some(b'}') => {
                    self.bump();
                    break;
                }
                _ => return Err(reject("expected ',' or '}' in object")),
            }
        }
        members.sort_by(|a, b| a.0.encode_utf16().cmp(b.0.encode_utf16()));
        Ok(Canonical::Object(members))
    }

    fn parse_string(&mut self) -> Result<String, MegaError> {
        self.bump();
        let mut out = String::new();
        loop {
            let Some(ch) = self.rest().chars().next() else {
                return Err(reject("unterminated string"));
            };
            match ch {
                '"' => {
                    self.bump();
                    return Ok(out);
                }
                '\\' => {
                    self.bump();
                    out.push(self.parse_escape()?);
                }
                ch if (ch as u32) < 0x20 => {
                    return Err(reject("unescaped control character in string"));
                }
                ch => {
                    out.push(ch);
                    self.bump();
                }
            }
        }
    }

    fn parse_escape(&mut self) -> Result<char, MegaError> {
        let Some(ch) = self.rest().chars().next() else {
            return Err(reject("unterminated string escape"));
        };
        self.bump();
        match ch {
            '"' => Ok('"'),
            '\\' => Ok('\\'),
            '/' => Ok('/'),
            'b' => Ok('\u{08}'),
            'f' => Ok('\u{0C}'),
            'n' => Ok('\n'),
            'r' => Ok('\r'),
            't' => Ok('\t'),
            'u' => self.parse_hex_escape(),
            _ => Err(reject("invalid string escape")),
        }
    }

    fn parse_hex_escape(&mut self) -> Result<char, MegaError> {
        let unit = self.take_hex4()?;
        if (0xD800..=0xDBFF).contains(&unit) {
            if self.rest().starts_with("\\u") {
                self.index += 2;
                let low = self.take_hex4()?;
                if (0xDC00..=0xDFFF).contains(&low) {
                    let cp = 0x10000 + (((unit as u32) - 0xD800) << 10) + ((low as u32) - 0xDC00);
                    return char::from_u32(cp).ok_or_else(|| reject("invalid surrogate pair"));
                }
            }
            return Err(reject("unpaired UTF-16 surrogate"));
        }
        if (0xDC00..=0xDFFF).contains(&unit) {
            return Err(reject("unpaired UTF-16 surrogate"));
        }
        char::from_u32(unit as u32).ok_or_else(|| reject("invalid unicode escape"))
    }

    fn take_hex4(&mut self) -> Result<u16, MegaError> {
        let rest = self.rest().as_bytes();
        if rest.len() < 4 {
            return Err(reject("invalid unicode escape"));
        }
        let mut unit = 0u16;
        for &b in &rest[..4] {
            let digit = match b {
                b'0'..=b'9' => u16::from(b - b'0'),
                b'a'..=b'f' => u16::from(b - b'a') + 10,
                b'A'..=b'F' => u16::from(b - b'A') + 10,
                _ => return Err(reject("invalid unicode escape")),
            };
            unit = (unit << 4) | digit;
        }
        self.index += 4;
        Ok(unit)
    }

    fn parse_number(&mut self) -> Result<Canonical, MegaError> {
        let start = self.index;
        if self.peek() == Some(b'-') {
            self.index += 1;
        }
        match self.peek() {
            Some(b'0') => {
                self.index += 1;
                if matches!(self.peek(), Some(b'0'..=b'9')) {
                    return Err(reject("leading zeros are not allowed"));
                }
            }
            Some(b'1'..=b'9') => {
                self.index += 1;
                while matches!(self.peek(), Some(b'0'..=b'9')) {
                    self.index += 1;
                }
            }
            _ => return Err(reject("invalid number")),
        }
        if self.peek() == Some(b'.') {
            self.index += 1;
            if !matches!(self.peek(), Some(b'0'..=b'9')) {
                return Err(reject("invalid number fraction"));
            }
            while matches!(self.peek(), Some(b'0'..=b'9')) {
                self.index += 1;
            }
        }
        if matches!(self.peek(), Some(b'e' | b'E')) {
            self.index += 1;
            if matches!(self.peek(), Some(b'+' | b'-')) {
                self.index += 1;
            }
            if !matches!(self.peek(), Some(b'0'..=b'9')) {
                return Err(reject("invalid number exponent"));
            }
            while matches!(self.peek(), Some(b'0'..=b'9')) {
                self.index += 1;
            }
        }
        let lexeme = &self.input[start..self.index];
        let parsed: f64 = lexeme.parse().map_err(|_| reject_overflow(lexeme))?;
        number_from_f64(parsed, lexeme)
    }
}

fn number_from_f64(n: f64, lexeme: &str) -> Result<Canonical, MegaError> {
    if n.is_nan() {
        return Err(reject_nan());
    }
    if !n.is_finite() {
        return Err(reject_overflow(lexeme));
    }
    if n == 0.0 {
        return Ok(Canonical::Number("0".into()));
    }
    Ok(Canonical::Number(es6_number_to_string(n)))
}

/// ECMAScript `NumberToString` as required by RFC 8785 §3.2.2.3.
///
/// Every JSON number is normalized through IEEE-754 binary64 first (JCS),
/// then rendered with ES6 exponent cutoffs (`e < -6` or `e >= 21` →
/// scientific; otherwise decimal).
fn es6_number_to_string(n: f64) -> String {
    debug_assert!(n.is_finite() && n != 0.0);
    let mut buf = ryu::Buffer::new();
    let rendered = buf.format_finite(n);
    let (negative, digits, first_digit_exp) = parse_shortest_decimal(rendered);
    es6_format(negative, &digits, first_digit_exp)
}

/// Parse a ryu shortest decimal into digits (no decimal point) and the
/// exponent of the leading digit: `digits[0].digits[1..] × 10^first_digit_exp`.
fn parse_shortest_decimal(rendered: &str) -> (bool, String, i32) {
    let (negative, body) = match rendered.strip_prefix('-') {
        Some(rest) => (true, rest),
        None => (false, rendered),
    };
    let (coeff, exp_from_e) = if let Some(e_idx) = body.find(['e', 'E']) {
        let exp: i32 = body[e_idx + 1..]
            .trim_start_matches('+')
            .parse()
            .unwrap_or(0);
        (&body[..e_idx], exp)
    } else {
        (body, 0)
    };
    let (digits, coeff_exp) = coeff_to_digits(coeff);
    (negative, digits, coeff_exp + exp_from_e)
}

fn coeff_to_digits(coeff: &str) -> (String, i32) {
    if let Some(dot) = coeff.find('.') {
        let int_part = &coeff[..dot];
        let frac = &coeff[dot + 1..];
        if int_part.is_empty() || int_part == "0" {
            let leading_zeros = frac.bytes().take_while(|&b| b == b'0').count();
            let digits: String = frac[leading_zeros..].trim_end_matches('0').to_string();
            let digits = if digits.is_empty() {
                "0".to_string()
            } else {
                digits
            };
            (digits, -(leading_zeros as i32 + 1))
        } else {
            let mut digits = String::from(int_part);
            digits.push_str(frac);
            let digits = digits.trim_end_matches('0').to_string();
            (digits, int_part.len() as i32 - 1)
        }
    } else {
        let digits = coeff.trim_start_matches('0');
        let digits = if digits.is_empty() {
            "0".to_string()
        } else {
            digits.to_string()
        };
        let exp = digits.len() as i32 - 1;
        (digits, exp)
    }
}

fn es6_format(negative: bool, digits: &str, e: i32) -> String {
    let k = digits.len() as i32;
    let mut out = String::new();
    if negative {
        out.push('-');
    }
    if !(-6..21).contains(&e) {
        if let Some(first) = digits.chars().next() {
            out.push(first);
        }
        if k > 1 {
            out.push('.');
            out.push_str(&digits[1..]);
        }
        out.push('e');
        if e >= 0 {
            out.push('+');
        }
        out.push_str(&e.to_string());
        return out;
    }
    if e >= 0 {
        let int_digits = e + 1;
        if int_digits >= k {
            out.push_str(digits);
            for _ in 0..(int_digits - k) {
                out.push('0');
            }
        } else {
            let point = int_digits as usize;
            out.push_str(&digits[..point]);
            out.push('.');
            out.push_str(&digits[point..]);
        }
        return out;
    }
    out.push_str("0.");
    for _ in 0..(-e - 1) {
        out.push('0');
    }
    out.push_str(digits);
    out
}

#[cfg(test)]
mod tests {
    use super::{canonicalize, fingerprint};

    #[test]
    fn fingerprint_stable_under_key_reorder() {
        let left =
            fingerprint(r#"{"b":1,"a":2,"nested":{"z":true,"y":[3,1]}}"#).expect("canonical JSON");
        let right =
            fingerprint(r#"{"nested":{"y":[3,1],"z":true},"a":2,"b":1}"#).expect("canonical JSON");
        assert_eq!(left, right);
        assert_eq!(
            canonicalize(r#"{"b":1,"a":2}"#).expect("jcs"),
            r#"{"a":2,"b":1}"#
        );
    }

    #[test]
    fn rejects_duplicate_key() {
        let err = fingerprint(r#"{"a":1,"a":2}"#).expect_err("duplicate key");
        assert!(
            err.to_string().contains("duplicate key"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn rejects_nan() {
        let err = fingerprint("NaN").expect_err("NaN is not JSON");
        assert!(
            err.to_string().to_ascii_lowercase().contains("nan"),
            "{err}"
        );
        let nested = fingerprint(r#"{"x":NaN}"#).expect_err("NaN object member");
        assert!(
            nested.to_string().to_ascii_lowercase().contains("nan"),
            "{nested}"
        );
    }

    #[test]
    fn rejects_infinity() {
        fingerprint("Infinity").expect_err("Infinity is not JSON");
        fingerprint("-Infinity").expect_err("-Infinity is not JSON");
        fingerprint(r#"{"x":Infinity}"#).expect_err("Infinity object member");
    }

    #[test]
    fn rejects_overflow() {
        let err = fingerprint("1e1000").expect_err("overflow exponent");
        assert!(err.to_string().contains("overflow"), "{err}");
        fingerprint(&"9".repeat(400)).expect_err("overflow integer");
    }

    #[test]
    fn rfc8785_es6_number_vectors() {
        assert_eq!(canonicalize("1e-7").expect("jcs"), "1e-7");
        assert_eq!(canonicalize("1e-6").expect("jcs"), "0.000001");
        assert_eq!(canonicalize("1e+21").expect("jcs"), "1e+21");
        assert_eq!(canonicalize("1e20").expect("jcs"), "100000000000000000000");
        assert_eq!(canonicalize("1.5").expect("jcs"), "1.5");
        assert_eq!(canonicalize("0").expect("jcs"), "0");
        assert_eq!(canonicalize("-0").expect("jcs"), "0");
        let beyond_safe = canonicalize("9007199254740993").expect("jcs");
        let dotted = canonicalize("9007199254740993.0").expect("jcs");
        assert_eq!(beyond_safe, dotted);
        assert_eq!(beyond_safe, "9007199254740992");
    }

    #[test]
    fn rejects_malformed_unicode_escape() {
        fingerprint("\"\\u€€\"").expect_err("non-ascii unicode escape must not panic");
        fingerprint("\"\\u+041\"").expect_err("leading plus is not a hex digit");
        fingerprint("\"\\u12\"").expect_err("truncated unicode escape");
    }

    #[test]
    fn rejects_excessive_nesting() {
        let over_array = "[".repeat(129) + "0" + &"]".repeat(129);
        fingerprint(&over_array).expect_err("array nesting over 128");
        let over_object = "{\"a\":".repeat(129) + "0" + &"}".repeat(129);
        fingerprint(&over_object).expect_err("object nesting over 128");
        let over_mixed = "[{\"a\":".repeat(65) + "0" + &"}]".repeat(65);
        fingerprint(&over_mixed).expect_err("mixed nesting over 128");
        let at_limit = "[".repeat(128) + "0" + &"]".repeat(128);
        fingerprint(&at_limit).expect("128 nested arrays are accepted");
    }
}
