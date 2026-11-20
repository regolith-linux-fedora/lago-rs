//! JSON reader with the acceptance rules of the daemon client's original
//! parser (non-strict mode).
//!
//! Besides standard JSON it accepts, and the client therefore accepts:
//! a UTF-8 byte-order mark at the very start; `/* */` comments between
//! tokens; a `/` not followed by `*` starting a comment that runs to the
//! next `/`; single-quoted strings taken literally; octal (`\NNN`) and
//! unknown (`\x` -> `x`) escapes; several top-level values (a first value
//! followed by arrays or objects becomes an array of all of them); and a
//! leading `var name =` assignment with an optional `;`. It rejects input
//! that is not UTF-8, numbers with leading zeros, `+` signs or bare dots,
//! trailing commas, unescaped `\b \t \n \f \r` inside double-quoted
//! strings, malformed `\u` escapes and lone surrogates, and nesting deeper
//! than 1024 levels.
//!
//! Strings are C strings once parsed: a decoded NUL ends them.

/// Maximum container depth; the outermost container is level 0.
const MAX_DEPTH: usize = 1024;

#[derive(Debug, Clone, PartialEq)]
pub enum Node {
    Null,
    Bool(bool),
    Int(i64),
    Double(f64),
    Str(Vec<u8>),
    Array(Vec<Node>),
    Object(Vec<(Vec<u8>, Node)>),
}

impl Node {
    fn member(&self, name: &[u8]) -> Option<&Node> {
        match self {
            Node::Object(members) => members.iter().find(|(k, _)| k == name).map(|(_, v)| v),
            _ => None,
        }
    }

    /// Integer view of a member: integers as is, doubles truncated toward
    /// zero (values outside the 64-bit range read as `i64::MIN`, as the
    /// hardware conversion gives), booleans as 0/1, everything else 0.
    pub fn int_member(&self, name: &[u8]) -> i64 {
        match self.member(name) {
            Some(Node::Int(v)) => *v,
            Some(Node::Double(d)) => double_to_int64(*d),
            Some(Node::Bool(b)) => i64::from(*b),
            _ => 0,
        }
    }

    /// String view of a member: `None` unless the member holds a string.
    pub fn string_member(&self, name: &[u8]) -> Option<&[u8]> {
        match self.member(name) {
            Some(Node::Str(s)) => Some(s),
            _ => None,
        }
    }
}

/// C `(int64_t) double` on x86-64: truncation, with NaN and out-of-range
/// values giving the "integer indefinite" value `i64::MIN`.
fn double_to_int64(d: f64) -> i64 {
    const TWO_63: f64 = 9_223_372_036_854_775_808.0;
    if d.is_nan() || !(-TWO_63..TWO_63).contains(&d) {
        i64::MIN
    } else {
        d.trunc() as i64
    }
}

/// Cuts a decoded string at its first NUL.
fn c_string(mut bytes: Vec<u8>) -> Vec<u8> {
    if let Some(nul) = bytes.iter().position(|&b| b == 0) {
        bytes.truncate(nul);
    }
    bytes
}

// ---------------------------------------------------------------------------
// Tokenizer

#[derive(Debug, Clone, PartialEq)]
enum Token {
    Eof,
    /// Any single character that is not part of another token, including
    /// the structural characters and skipped whitespace.
    Char(u8),
    Error,
    Int(i64),
    Float(f64),
    Str(Vec<u8>),
    Identifier,
    CommentSingle,
    CommentMulti,
    True,
    False,
    Null,
    Var,
}

struct Scanner<'a> {
    text: &'a [u8],
    pos: usize,
    next: Option<Token>,
}

fn is_hex(c: u8) -> bool {
    c.is_ascii_hexdigit()
}

fn hex_value(c: u8) -> u32 {
    match c {
        b'0'..=b'9' => u32::from(c - b'0'),
        _ => u32::from((c & 7) + 9),
    }
}

fn is_lower(c: u8) -> bool {
    c.is_ascii_lowercase()
}

fn is_unicode_scalar(c: u32) -> bool {
    c < 0x11_0000 && (c & 0xFFFF_F800) != 0xD800
}

fn push_unichar(out: &mut Vec<u8>, c: u32) {
    // Every caller passes a scalar value or an octal escape (< 0o1000).
    let ch = char::from_u32(c).unwrap_or('\u{FFFD}');
    let mut buf = [0u8; 4];
    out.extend_from_slice(ch.encode_utf8(&mut buf).as_bytes());
}

impl<'a> Scanner<'a> {
    fn new(text: &'a [u8]) -> Scanner<'a> {
        Scanner {
            text,
            pos: 0,
            next: None,
        }
    }

    fn peek_char(&self) -> u8 {
        self.text.get(self.pos).copied().unwrap_or(0)
    }

    fn get_char(&mut self) -> u8 {
        match self.text.get(self.pos) {
            Some(&c) => {
                self.pos += 1;
                c
            }
            None => 0,
        }
    }

    fn peek_token(&mut self) -> &Token {
        if self.next.is_none() {
            let token = self.token_skipping_blanks();
            self.next = Some(token);
        }
        self.next.as_ref().unwrap_or(&Token::Eof)
    }

    fn next_token(&mut self) -> Token {
        match self.next.take() {
            Some(token) => token,
            None => self.token_skipping_blanks(),
        }
    }

    fn token_skipping_blanks(&mut self) -> Token {
        loop {
            match self.raw_token() {
                Token::Char(b' ' | b'\t' | b'\r' | b'\n')
                | Token::CommentSingle
                | Token::CommentMulti => {}
                token => return token,
            }
        }
    }

    /// Four hex digits; each character is consumed even when it is not one.
    fn unichar(&mut self) -> Option<u32> {
        let mut value = 0u32;
        for i in 0..4 {
            let c = self.get_char();
            if !is_hex(c) {
                return None;
            }
            value += hex_value(c) << ((3 - i) * 4);
        }
        Some(value)
    }

    fn raw_token(&mut self) -> Token {
        if self.pos >= self.text.len() {
            return Token::Eof;
        }
        let ch = self.get_char();
        if is_lower(ch) {
            return self.identifier(ch);
        }
        match ch {
            b'/' if self.peek_char() == b'*' => self.block_comment(),
            b'/' => self.slash_comment(),
            b'\'' => self.single_quoted(),
            b'"' => self.double_quoted(),
            b'-' if !self.peek_char().is_ascii_digit() => Token::Error,
            b'-' | b'0'..=b'9' => self.number(ch),
            _ => Token::Char(ch),
        }
    }

    /// Lowercase words: `true`, `false`, `null`, `var` or a bare identifier.
    /// A lone lowercase letter is a plain character, except as the last
    /// character of the input, where it still reads as a word.
    fn identifier(&mut self, first: u8) -> Token {
        let at_end = self.pos >= self.text.len();
        if !at_end && !is_lower(self.peek_char()) {
            return Token::Char(first);
        }
        let mut word = vec![first];
        while is_lower(self.peek_char()) {
            word.push(self.get_char());
        }
        match word.as_slice() {
            b"true" => Token::True,
            b"false" => Token::False,
            b"null" => Token::Null,
            b"var" => Token::Var,
            _ => Token::Identifier,
        }
    }

    fn block_comment(&mut self) -> Token {
        self.get_char(); // '*'
        loop {
            match self.get_char() {
                0 => return Token::Error,
                b'*' if self.peek_char() == b'/' => {
                    self.get_char();
                    return Token::CommentMulti;
                }
                _ => {}
            }
        }
    }

    /// A `/` not followed by `*` opens a comment closed by the next `/`.
    fn slash_comment(&mut self) -> Token {
        loop {
            match self.get_char() {
                0 => return Token::Error,
                b'/' => return Token::CommentSingle,
                _ => {}
            }
        }
    }

    fn single_quoted(&mut self) -> Token {
        let mut s = Vec::new();
        loop {
            match self.get_char() {
                0 => return Token::Error,
                b'\'' => return Token::Str(s),
                c => s.push(c),
            }
        }
    }

    fn double_quoted(&mut self) -> Token {
        let mut s = Vec::new();
        let mut failed = false;
        loop {
            let ch = self.get_char();
            if ch == 0 {
                // End of input inside the string.
                return Token::Error;
            }
            if ch == b'"' || failed {
                return if failed { Token::Error } else { Token::Str(s) };
            }
            if ch == b'\\' {
                if !self.escape(&mut s) {
                    failed = true;
                }
            } else if matches!(ch, b'\n' | b'\t' | b'\r' | 0x0c | 0x08) {
                return Token::Error;
            } else {
                s.push(ch);
            }
        }
    }

    /// Decodes one escape after a backslash; false on a malformed one.
    fn escape(&mut self, s: &mut Vec<u8>) -> bool {
        let ch = self.get_char();
        match ch {
            0 => {}
            b'"' => s.push(b'"'),
            b'\\' => s.push(b'\\'),
            b'/' => s.push(b'/'),
            b'n' => s.push(b'\n'),
            b't' => s.push(b'\t'),
            b'r' => s.push(b'\r'),
            b'b' => s.push(0x08),
            b'f' => s.push(0x0c),
            b'u' => {
                if !is_hex(self.peek_char()) {
                    return false;
                }
                let Some(first) = self.unichar() else {
                    return false;
                };
                let code = if (0xD800..=0xDFFF).contains(&first) {
                    if self.peek_char() != b'\\' {
                        return false;
                    }
                    self.get_char();
                    if self.peek_char() != b'u' {
                        return false;
                    }
                    self.get_char();
                    let Some(second) = self.unichar() else {
                        return false;
                    };
                    if !((0xDC00..=0xDFFF).contains(&second) && (0xD800..=0xDBFF).contains(&first))
                    {
                        return false;
                    }
                    0x1_0000 + ((first & 0x3FF) << 10) + (second & 0x3FF)
                } else {
                    if !is_unicode_scalar(first) {
                        return false;
                    }
                    first
                };
                push_unichar(s, code);
            }
            b'0'..=b'7' => {
                let mut code = u32::from(ch - b'0');
                for _ in 0..2 {
                    let next = self.peek_char();
                    if (b'0'..=b'7').contains(&next) {
                        code = code * 8 + u32::from(next - b'0');
                        self.get_char();
                    } else {
                        break;
                    }
                }
                push_unichar(s, code);
            }
            other => s.push(other),
        }
        true
    }

    fn number(&mut self, first: u8) -> Token {
        let mut is_float = false;
        let mut text = vec![first];
        let mut ch = first;
        let mut leading_zero = first == b'0';
        if first == b'-' {
            ch = self.get_char();
            leading_zero = ch == b'0';
            text.push(ch);
        }
        loop {
            let after_e = is_float && (ch == b'e' || ch == b'E');
            let next = self.peek_char();
            let continues = next.is_ascii_digit()
                || next == b'e'
                || next == b'E'
                || next == b'.'
                || (after_e && (next == b'+' || next == b'-'));
            if !continues {
                break;
            }
            ch = self.get_char();
            match ch {
                b'.' => {
                    if !self.peek_char().is_ascii_digit() {
                        return Token::Error;
                    }
                    is_float = true;
                    text.push(ch);
                }
                b'0'..=b'9' => {
                    if leading_zero && !is_float {
                        return Token::Error;
                    }
                    text.push(ch);
                }
                b'+' | b'-' => {
                    if !is_float {
                        return Token::Error;
                    }
                    text.push(ch);
                }
                _ => {
                    // 'e' or 'E'
                    is_float = true;
                    text.push(ch);
                }
            }
        }
        if is_float {
            match parse_float(&text) {
                Some(value) => Token::Float(value),
                None => Token::Error,
            }
        } else {
            Token::Int(parse_decimal(&text))
        }
    }
}

/// `-?digits`, saturating at the 64-bit limits.
fn parse_decimal(text: &[u8]) -> i64 {
    let (negative, digits) = match text.split_first() {
        Some((b'-', rest)) => (true, rest),
        _ => (false, text),
    };
    let mut value: i64 = 0;
    for &d in digits {
        let d = i64::from(d - b'0');
        value = if negative {
            value.saturating_mul(10).saturating_sub(d)
        } else {
            value.saturating_mul(10).saturating_add(d)
        };
    }
    value
}

/// A float token is usable only if all of it is one decimal floating
/// constant: `-?D+(.D+)?([eE][+-]?D+)?`.
fn parse_float(text: &[u8]) -> Option<f64> {
    let mut i = 0;
    let digits = |i: &mut usize| {
        let start = *i;
        while text.get(*i).is_some_and(u8::is_ascii_digit) {
            *i += 1;
        }
        *i > start
    };
    if text.first() == Some(&b'-') {
        i += 1;
    }
    if !digits(&mut i) {
        return None;
    }
    if text.get(i) == Some(&b'.') {
        i += 1;
        if !digits(&mut i) {
            return None;
        }
    }
    if matches!(text.get(i), Some(b'e' | b'E')) {
        i += 1;
        if matches!(text.get(i), Some(b'+' | b'-')) {
            i += 1;
        }
        if !digits(&mut i) {
            return None;
        }
    }
    if i != text.len() {
        return None;
    }
    std::str::from_utf8(text).ok()?.parse::<f64>().ok()
}

// ---------------------------------------------------------------------------
// Parser

/// What a parse step leaves behind: `Ok` or a failure.
type Step = Result<(), ()>;

struct Parser<'a> {
    scanner: Scanner<'a>,
    root: Option<Node>,
    multi_root: bool,
}

impl<'a> Parser<'a> {
    fn value(&mut self, token: Token) -> Result<Node, ()> {
        match token {
            Token::Int(v) => Ok(Node::Int(v)),
            Token::Float(v) => Ok(Node::Double(v)),
            Token::Str(s) => Ok(Node::Str(c_string(s))),
            Token::True => Ok(Node::Bool(true)),
            Token::False => Ok(Node::Bool(false)),
            Token::Null => Ok(Node::Null),
            _ => Err(()),
        }
    }

    /// Parses a container or scalar after peeking `{`, `[` or anything else.
    fn element(&mut self, depth: usize) -> Result<Node, ()> {
        match self.scanner.peek_token() {
            Token::Char(b'[') => self.array(depth),
            Token::Char(b'{') => self.object(depth),
            _ => {
                let token = self.scanner.next_token();
                self.value(token)
            }
        }
    }

    fn array(&mut self, depth: usize) -> Result<Node, ()> {
        if depth >= MAX_DEPTH {
            return Err(());
        }
        let open = self.scanner.next_token();
        debug_assert_eq!(open, Token::Char(b'['));
        let mut elements = Vec::new();
        loop {
            if *self.scanner.peek_token() == Token::Char(b']') {
                break;
            }
            let element = self.element(depth + 1)?;
            match self.scanner.peek_token() {
                Token::Char(b',') => {
                    self.scanner.next_token();
                    if *self.scanner.peek_token() == Token::Char(b']') {
                        return Err(()); // trailing comma
                    }
                }
                Token::Char(b']') => {}
                _ => return Err(()), // missing comma
            }
            elements.push(element);
        }
        self.scanner.next_token(); // ']'
        Ok(Node::Array(elements))
    }

    fn object(&mut self, depth: usize) -> Result<Node, ()> {
        if depth >= MAX_DEPTH {
            return Err(());
        }
        let open = self.scanner.next_token();
        debug_assert_eq!(open, Token::Char(b'{'));
        let mut members: Vec<(Vec<u8>, Node)> = Vec::new();
        loop {
            let name = match self.scanner.peek_token() {
                Token::Char(b'}') => break,
                Token::Str(_) => match self.scanner.next_token() {
                    Token::Str(s) => c_string(s),
                    _ => return Err(()),
                },
                _ => return Err(()), // member name must be a string
            };
            if *self.scanner.peek_token() != Token::Char(b':') {
                return Err(());
            }
            self.scanner.next_token();
            let member = self.element(depth + 1)?;
            match self.scanner.peek_token() {
                Token::Char(b',') => {
                    self.scanner.next_token();
                    if *self.scanner.peek_token() == Token::Char(b'}') {
                        return Err(()); // trailing comma
                    }
                }
                Token::Str(_) => return Err(()), // missing comma
                _ => {}
            }
            match members.iter_mut().find(|(k, _)| *k == name) {
                Some(slot) => slot.1 = member,
                None => members.push((name, member)),
            }
        }
        self.scanner.next_token(); // '}'
        Ok(Node::Object(members))
    }

    /// A value after an earlier root: the root becomes an array holding
    /// every top-level value. The wrap happens before the new value parses.
    fn additional_root(&mut self, is_object: bool) -> Step {
        if !self.multi_root {
            let old = self.root.take().unwrap_or(Node::Null);
            self.root = Some(Node::Array(vec![old]));
            self.multi_root = true;
        }
        let parsed = if is_object {
            self.object(0)
        } else {
            self.array(0)
        };
        let node = parsed?;
        if let Some(Node::Array(all)) = self.root.as_mut() {
            all.push(node);
        }
        Ok(())
    }

    fn statement(&mut self) -> Step {
        let peeked = self.scanner.peek_token().clone();
        match peeked {
            Token::Char(open @ (b'{' | b'[')) => {
                let is_object = open == b'{';
                if self.root.is_some() {
                    self.additional_root(is_object)
                } else {
                    let node = if is_object {
                        self.object(0)?
                    } else {
                        self.array(0)?
                    };
                    self.root = Some(node);
                    Ok(())
                }
            }
            Token::Var => {
                self.scanner.next_token();
                if self.scanner.next_token() != Token::Identifier {
                    return Err(());
                }
                if self.scanner.next_token() != Token::Char(b'=') {
                    return Err(());
                }
                let result = self.statement();
                // A trailing semicolon ends the assignment, whatever the
                // value's outcome.
                if *self.scanner.peek_token() == Token::Char(b';') {
                    self.scanner.next_token();
                    return Ok(());
                }
                result
            }
            Token::Null
            | Token::True
            | Token::False
            | Token::Char(b'-')
            | Token::Int(_)
            | Token::Float(_)
            | Token::Str(_)
            | Token::Identifier => {
                let token = self.scanner.next_token();
                if self.root.is_some() {
                    return Err(());
                }
                let node = self.value(token)?;
                self.root = Some(node);
                Ok(())
            }
            _ => {
                self.scanner.next_token();
                Err(())
            }
        }
    }
}

/// Parses a reply already cut at its first NUL. `None` when it is rejected;
/// `Some(Node::Null)` stands for "accepted but empty" (no root).
pub fn parse(input: &[u8]) -> Option<Node> {
    if std::str::from_utf8(input).is_err() {
        return None;
    }
    let data = input.strip_prefix(b"\xEF\xBB\xBF").unwrap_or(input);
    let mut parser = Parser {
        scanner: Scanner::new(data),
        root: None,
        multi_root: false,
    };
    loop {
        if *parser.scanner.peek_token() == Token::Eof {
            break;
        }
        if parser.statement().is_err() {
            return None;
        }
    }
    Some(parser.root.unwrap_or(Node::Null))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn accepts(text: &[u8]) -> bool {
        parse(text).is_some()
    }

    fn array_len(text: &[u8]) -> Option<usize> {
        match parse(text) {
            Some(Node::Array(v)) => Some(v.len()),
            _ => None,
        }
    }

    #[test]
    fn standard_documents() {
        assert_eq!(array_len(b"[]"), Some(0));
        assert_eq!(array_len(b" [ 1 , \"a\" , null , true , {} ] "), Some(5));
        assert_eq!(array_len(b"\xEF\xBB\xBF[]"), Some(0));
    }

    #[test]
    fn number_shapes() {
        for bad in [
            &b"[01]"[..],
            b"[-01]",
            b"[1e]",
            b"[1e+]",
            b"[.5]",
            b"[5.]",
            b"[+1]",
            b"[--1]",
            b"[0x10]",
        ] {
            assert!(!accepts(bad), "{:?}", String::from_utf8_lossy(bad));
        }
        for good in [
            &b"[0]"[..],
            b"[-0]",
            b"[0.5]",
            b"[1E5]",
            b"[1e+5]",
            b"[0e5]",
            b"[1e999999]",
        ] {
            assert!(accepts(good), "{:?}", String::from_utf8_lossy(good));
        }
    }

    #[test]
    fn comments_and_multiple_roots() {
        assert_eq!(array_len(b"/*c*/[1,/**/2]/***/"), Some(2));
        assert_eq!(array_len(b"//[1]"), Some(1));
        assert!(!accepts(b"//c\n[]"));
        assert!(!accepts(b"[1]/*"));
        assert_eq!(array_len(b"[] []"), Some(2));
        assert_eq!(array_len(b"3 [1,2]"), Some(2));
        assert!(!accepts(b"[1] 2"));
        assert!(matches!(parse(b"{\"a\":1}"), Some(Node::Object(_))));
    }

    #[test]
    fn strings() {
        let s = |text: &[u8]| match parse(text) {
            Some(Node::Array(v)) => match &v[0] {
                Node::Str(s) => s.clone(),
                _ => panic!("not a string"),
            },
            _ => panic!("rejected"),
        };
        assert_eq!(s(b"[\"a\\x41b\"]"), b"ax41b");
        assert_eq!(s(b"[\"\\123\\778\"]"), b"S?8");
        assert_eq!(s(b"[\"a\\0b\"]"), b"a");
        assert_eq!(s(b"[\"\\ud83d\\ude00\"]"), "\u{1F600}".as_bytes());
        assert_eq!(s(b"['it\\s']"), b"it\\s");
        assert!(!accepts(b"[\"\\ud800\"]"));
        assert!(!accepts(b"[\"a\tb\"]"));
        assert!(accepts(b"[\"a\x01b\"]"));
    }

    #[test]
    fn assignments() {
        assert_eq!(array_len(b"var ab = [1];"), Some(1));
        assert!(!accepts(b"var a = [1]"));
        assert_eq!(array_len(b"[] var ab = x;"), Some(0));
    }

    #[test]
    fn depth_limit() {
        let ok = [b"[".repeat(1024), b"]".repeat(1024)].concat();
        let deep = [b"[".repeat(1025), b"]".repeat(1025)].concat();
        assert!(accepts(&ok));
        assert!(!accepts(&deep));
    }
}
