use std::fmt::Write;

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
    use super::string;

    #[test]
    fn escapes_json_control_characters() {
        assert_eq!(string("a\"b\\c\n\t"), r#""a\"b\\c\n\t""#);
    }
}
