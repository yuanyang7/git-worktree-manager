use std::fmt::Write;

const MAX_JSON_DEPTH: usize = 128;

#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    Null,
    Bool(bool),
    Number(i64),
    String(String),
    Array(Vec<Value>),
    Object(Vec<(String, Value)>),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParseError {
    pub message: String,
    pub position: usize,
}

impl std::fmt::Display for ParseError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "invalid JSON at byte {}: {}",
            self.position, self.message
        )
    }
}

impl std::error::Error for ParseError {}

impl Value {
    pub fn to_json(&self) -> String {
        match self {
            Self::Null => "null".to_owned(),
            Self::Bool(value) => value.to_string(),
            Self::Number(value) => value.to_string(),
            Self::String(value) => string(value),
            Self::Array(values) => array(values.iter().map(Self::to_json)),
            Self::Object(fields) => Object {
                fields: fields
                    .iter()
                    .map(|(key, value)| format!("{}:{}", string(key), value.to_json()))
                    .collect(),
            }
            .finish(),
        }
    }

    pub fn object_field(&self, key: &str) -> Option<&Value> {
        match self {
            Self::Object(fields) => fields
                .iter()
                .find_map(|(field, value)| (field == key).then_some(value)),
            _ => None,
        }
    }

    pub fn required_string(&self, key: &str) -> Result<&str, String> {
        match self.object_field(key) {
            Some(Self::String(value)) => Ok(value),
            Some(_) => Err(format!("JSON field {key:?} must be a string")),
            None => Err(format!("JSON field {key:?} is required")),
        }
    }

    pub fn optional_string(&self, key: &str) -> Result<Option<&str>, String> {
        match self.object_field(key) {
            None | Some(Self::Null) => Ok(None),
            Some(Self::String(value)) => Ok(Some(value)),
            Some(_) => Err(format!("JSON field {key:?} must be a string or null")),
        }
    }

    pub fn optional_i64(&self, key: &str) -> Result<Option<i64>, String> {
        match self.object_field(key) {
            None | Some(Self::Null) => Ok(None),
            Some(Self::Number(value)) => Ok(Some(*value)),
            Some(_) => Err(format!("JSON field {key:?} must be an integer or null")),
        }
    }

    pub fn optional_bool(&self, key: &str) -> Result<Option<bool>, String> {
        match self.object_field(key) {
            None | Some(Self::Null) => Ok(None),
            Some(Self::Bool(value)) => Ok(Some(*value)),
            Some(_) => Err(format!("JSON field {key:?} must be a boolean or null")),
        }
    }

    pub fn as_object(&self) -> Result<&[(String, Value)], String> {
        match self {
            Self::Object(fields) => Ok(fields),
            _ => Err("JSON request must be an object".to_owned()),
        }
    }
}

pub fn parse(input: &str) -> Result<Value, ParseError> {
    let mut parser = Parser {
        input: input.as_bytes(),
        position: 0,
    };
    let value = parser.value()?;
    parser.whitespace();
    if parser.position != parser.input.len() {
        return Err(parser.error("trailing characters"));
    }
    Ok(value)
}

struct Parser<'input> {
    input: &'input [u8],
    position: usize,
}

impl Parser<'_> {
    fn value(&mut self) -> Result<Value, ParseError> {
        self.value_at_depth(0)
    }

    fn value_at_depth(&mut self, depth: usize) -> Result<Value, ParseError> {
        if depth >= MAX_JSON_DEPTH {
            return Err(self.error("JSON nesting depth exceeds the supported limit"));
        }
        self.whitespace();
        match self.peek() {
            Some(b'n') => {
                self.literal(b"null")?;
                Ok(Value::Null)
            }
            Some(b't') => {
                self.literal(b"true")?;
                Ok(Value::Bool(true))
            }
            Some(b'f') => {
                self.literal(b"false")?;
                Ok(Value::Bool(false))
            }
            Some(b'"') => self.string().map(Value::String),
            Some(b'[') => self.array(depth),
            Some(b'{') => self.object(depth),
            Some(byte) if byte == b'-' || byte.is_ascii_digit() => self.number(),
            Some(_) => Err(self.error("unexpected value")),
            None => Err(self.error("expected a value")),
        }
    }

    fn object(&mut self, depth: usize) -> Result<Value, ParseError> {
        self.expect(b'{')?;
        self.whitespace();
        let mut fields = Vec::new();
        if self.consume(b'}') {
            return Ok(Value::Object(fields));
        }
        loop {
            self.whitespace();
            let key = self.string()?;
            self.whitespace();
            self.expect(b':')?;
            let value = self.value_at_depth(depth + 1)?;
            fields.push((key, value));
            self.whitespace();
            if self.consume(b'}') {
                return Ok(Value::Object(fields));
            }
            self.expect(b',')?;
        }
    }

    fn array(&mut self, depth: usize) -> Result<Value, ParseError> {
        self.expect(b'[')?;
        self.whitespace();
        let mut values = Vec::new();
        if self.consume(b']') {
            return Ok(Value::Array(values));
        }
        loop {
            values.push(self.value_at_depth(depth + 1)?);
            self.whitespace();
            if self.consume(b']') {
                return Ok(Value::Array(values));
            }
            self.expect(b',')?;
        }
    }

    fn number(&mut self) -> Result<Value, ParseError> {
        let start = self.position;
        self.consume(b'-');
        if self.consume(b'0') {
            if self.peek().is_some_and(|byte| byte.is_ascii_digit()) {
                return Err(self.error("leading zeroes are not allowed"));
            }
        } else {
            let digits_start = self.position;
            while self.peek().is_some_and(|byte| byte.is_ascii_digit()) {
                self.position += 1;
            }
            if self.position == digits_start {
                return Err(self.error("expected digits in number"));
            }
        }
        if self
            .peek()
            .is_some_and(|byte| matches!(byte, b'.' | b'e' | b'E'))
        {
            return Err(self.error("only integer JSON numbers are supported"));
        }
        let value = std::str::from_utf8(&self.input[start..self.position])
            .expect("JSON number is ASCII")
            .parse::<i64>()
            .map_err(|_| self.error("integer is out of range"))?;
        Ok(Value::Number(value))
    }

    fn string(&mut self) -> Result<String, ParseError> {
        self.expect(b'"')?;
        let mut value = String::new();
        loop {
            let byte = self
                .next()
                .ok_or_else(|| self.error("unterminated string"))?;
            match byte {
                b'"' => return Ok(value),
                b'\\' => {
                    let escaped = self
                        .next()
                        .ok_or_else(|| self.error("unterminated escape"))?;
                    match escaped {
                        b'"' => value.push('"'),
                        b'\\' => value.push('\\'),
                        b'/' => value.push('/'),
                        b'b' => value.push('\u{08}'),
                        b'f' => value.push('\u{0c}'),
                        b'n' => value.push('\n'),
                        b'r' => value.push('\r'),
                        b't' => value.push('\t'),
                        b'u' => value.push(self.unicode_escape()?),
                        _ => return Err(self.error("unknown string escape")),
                    }
                }
                byte if byte < 0x20 => return Err(self.error("control character in string")),
                byte => {
                    let start = self.position - 1;
                    while self
                        .peek()
                        .is_some_and(|next| next >= 0x20 && next != b'\\' && next != b'"')
                    {
                        self.position += 1;
                    }
                    let bytes = &self.input[start..self.position];
                    let chunk = std::str::from_utf8(bytes)
                        .map_err(|_| self.error("string is not valid UTF-8"))?;
                    value.push_str(chunk);
                    let _ = byte;
                }
            }
        }
    }

    fn unicode_escape(&mut self) -> Result<char, ParseError> {
        let first = self.hex4()?;
        if (0xd800..=0xdbff).contains(&first) {
            if !self.consume(b'\\') || !self.consume(b'u') {
                return Err(self.error("high surrogate is not followed by a low surrogate"));
            }
            let second = self.hex4()?;
            if !(0xdc00..=0xdfff).contains(&second) {
                return Err(self.error("invalid low surrogate"));
            }
            let codepoint =
                0x1_0000 + ((u32::from(first) - 0xd800) << 10) + (u32::from(second) - 0xdc00);
            return char::from_u32(codepoint)
                .ok_or_else(|| self.error("invalid unicode codepoint"));
        }
        if (0xdc00..=0xdfff).contains(&first) {
            return Err(self.error("unexpected low surrogate"));
        }
        char::from_u32(u32::from(first)).ok_or_else(|| self.error("invalid unicode codepoint"))
    }

    fn hex4(&mut self) -> Result<u16, ParseError> {
        let start = self.position;
        for _ in 0..4 {
            if !self.peek().is_some_and(|byte| byte.is_ascii_hexdigit()) {
                return Err(self.error("invalid unicode escape"));
            }
            self.position += 1;
        }
        let digits = std::str::from_utf8(&self.input[start..self.position])
            .expect("unicode escape is ASCII");
        u16::from_str_radix(digits, 16).map_err(|_| self.error("invalid unicode escape"))
    }

    fn literal(&mut self, expected: &[u8]) -> Result<(), ParseError> {
        if self
            .input
            .get(self.position..self.position + expected.len())
            == Some(expected)
        {
            self.position += expected.len();
            Ok(())
        } else {
            Err(self.error("invalid literal"))
        }
    }

    fn expect(&mut self, expected: u8) -> Result<(), ParseError> {
        if self.consume(expected) {
            Ok(())
        } else {
            Err(self.error(&format!("expected {:?}", expected as char)))
        }
    }

    fn consume(&mut self, expected: u8) -> bool {
        if self.peek() == Some(expected) {
            self.position += 1;
            true
        } else {
            false
        }
    }

    fn whitespace(&mut self) {
        while self
            .peek()
            .is_some_and(|byte| matches!(byte, b' ' | b'\n' | b'\r' | b'\t'))
        {
            self.position += 1;
        }
    }

    fn peek(&self) -> Option<u8> {
        self.input.get(self.position).copied()
    }

    fn next(&mut self) -> Option<u8> {
        let byte = self.peek()?;
        self.position += 1;
        Some(byte)
    }

    fn error(&self, message: &str) -> ParseError {
        ParseError {
            message: message.to_owned(),
            position: self.position,
        }
    }
}

pub fn string(value: &str) -> String {
    let mut output = String::with_capacity(value.len() + 2);
    output.push('"');

    for character in value.chars() {
        match character {
            '"' => output.push_str("\\\""),
            '\\' => output.push_str("\\\\"),
            '\u{08}' => output.push_str("\\b"),
            '\u{0c}' => output.push_str("\\f"),
            '\n' => output.push_str("\\n"),
            '\r' => output.push_str("\\r"),
            '\t' => output.push_str("\\t"),
            character if character.is_control() => write!(output, "\\u{:04x}", character as u32)
                .expect("writing to String cannot fail"),
            character => output.push(character),
        }
    }

    output.push('"');
    output
}

pub fn optional_string(value: Option<&str>) -> String {
    value.map(string).unwrap_or_else(|| "null".to_owned())
}

pub fn optional_bool(value: Option<bool>) -> String {
    value
        .map(|value| value.to_string())
        .unwrap_or_else(|| "null".to_owned())
}

pub fn optional_number(value: Option<u64>) -> String {
    value
        .map(|value| value.to_string())
        .unwrap_or_else(|| "null".to_owned())
}

pub struct Object {
    fields: Vec<String>,
}

impl Object {
    pub fn new() -> Self {
        Self { fields: Vec::new() }
    }

    pub fn string(mut self, key: &str, value: &str) -> Self {
        self.fields
            .push(format!("{}:{}", string(key), string(value)));
        self
    }

    pub fn optional_string(mut self, key: &str, value: Option<&str>) -> Self {
        self.fields
            .push(format!("{}:{}", string(key), optional_string(value)));
        self
    }

    pub fn bool(mut self, key: &str, value: bool) -> Self {
        self.fields.push(format!("{}:{}", string(key), value));
        self
    }

    pub fn optional_bool(mut self, key: &str, value: Option<bool>) -> Self {
        self.fields
            .push(format!("{}:{}", string(key), optional_bool(value)));
        self
    }

    pub fn number(mut self, key: &str, value: u64) -> Self {
        self.fields.push(format!("{}:{}", string(key), value));
        self
    }

    pub fn signed_number(mut self, key: &str, value: i64) -> Self {
        self.fields.push(format!("{}:{}", string(key), value));
        self
    }

    pub fn optional_number(mut self, key: &str, value: Option<u64>) -> Self {
        self.fields
            .push(format!("{}:{}", string(key), optional_number(value)));
        self
    }

    pub fn optional_signed_number(mut self, key: &str, value: Option<i64>) -> Self {
        let serialized = value
            .map(|value| value.to_string())
            .unwrap_or_else(|| "null".to_owned());
        self.fields.push(format!("{}:{serialized}", string(key)));
        self
    }

    pub fn null(mut self, key: &str) -> Self {
        self.fields.push(format!("{}:null", string(key)));
        self
    }

    pub fn raw(mut self, key: &str, value: String) -> Self {
        self.fields.push(format!("{}:{}", string(key), value));
        self
    }

    pub fn finish(self) -> String {
        format!("{{{}}}", self.fields.join(","))
    }
}

impl Default for Object {
    fn default() -> Self {
        Self::new()
    }
}

pub fn array(values: impl IntoIterator<Item = String>) -> String {
    format!("[{}]", values.into_iter().collect::<Vec<_>>().join(","))
}

#[cfg(test)]
mod tests {
    use super::{Value, parse, string};

    #[test]
    fn escapes_json_control_characters() {
        assert_eq!(string("a\"b\\c\n\t"), r#""a\"b\\c\n\t""#);
    }

    #[test]
    fn parses_protocol_objects_and_unicode_escapes() {
        let value = parse(r#"{"request_id":"one","params":{"path":"caf\u00e9 \ud83d\ude80"}}"#)
            .expect("JSON should parse");
        assert_eq!(value.required_string("request_id"), Ok("one"));
        assert_eq!(
            value
                .object_field("params")
                .expect("params should exist")
                .required_string("path"),
            Ok("café 🚀")
        );
    }

    #[test]
    fn rejects_fractional_numbers_and_trailing_data() {
        assert!(parse("{\"value\":1.5}").is_err());
        assert!(parse("true false").is_err());
        assert_eq!(parse("null"), Ok(Value::Null));
    }

    #[test]
    fn rejects_excessively_nested_json() {
        let nested = format!("{}0{}", "[".repeat(128), "]".repeat(128));
        assert!(parse(&nested).is_err());
    }
}
