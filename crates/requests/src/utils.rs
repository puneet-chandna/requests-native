#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum HeaderValidationError {
    InvalidName,
    InvalidValue,
}

pub fn validate_header(name: &str, value: &str) -> Result<(), HeaderValidationError> {
    validate_header_name(name)?;
    validate_header_value(value)
}

pub fn validate_header_bytes(name: &[u8], value: &[u8]) -> Result<(), HeaderValidationError> {
    validate_header_name_bytes(name)?;
    validate_header_value_bytes(value)
}

pub fn validate_header_name(name: &str) -> Result<(), HeaderValidationError> {
    let mut chars = name.chars();
    let Some(first) = chars.next() else {
        return Err(HeaderValidationError::InvalidName);
    };
    if first == ':'
        || is_python_whitespace(first)
        || chars.any(|ch| matches!(ch, ':' | '\r' | '\n'))
    {
        return Err(HeaderValidationError::InvalidName);
    }
    Ok(())
}

pub fn validate_header_value(value: &str) -> Result<(), HeaderValidationError> {
    let mut chars = value.chars();
    let Some(first) = chars.next() else {
        return Ok(());
    };
    if is_python_whitespace(first) || first == '\r' || first == '\n' {
        return Err(HeaderValidationError::InvalidValue);
    }
    if chars.any(|ch| matches!(ch, '\r' | '\n')) {
        return Err(HeaderValidationError::InvalidValue);
    }
    Ok(())
}

pub fn trim_python_whitespace_start(value: &str) -> &str {
    value.trim_start_matches(is_python_whitespace)
}

fn is_python_whitespace(value: char) -> bool {
    value.is_whitespace() || matches!(value, '\u{1c}'..='\u{1f}')
}

pub fn validate_header_name_bytes(name: &[u8]) -> Result<(), HeaderValidationError> {
    let Some((&first, rest)) = name.split_first() else {
        return Err(HeaderValidationError::InvalidName);
    };
    if first == b':'
        || is_python_bytes_whitespace(first)
        || rest.contains(&b':')
        || has_return(rest)
    {
        return Err(HeaderValidationError::InvalidName);
    }
    Ok(())
}

pub fn validate_header_value_bytes(value: &[u8]) -> Result<(), HeaderValidationError> {
    let Some((&first, rest)) = value.split_first() else {
        return Ok(());
    };
    if is_python_bytes_whitespace(first) || has_return(rest) {
        return Err(HeaderValidationError::InvalidValue);
    }
    Ok(())
}

fn has_return(value: &[u8]) -> bool {
    value.contains(&b'\r') || value.contains(&b'\n')
}

fn is_python_bytes_whitespace(value: u8) -> bool {
    value.is_ascii_whitespace() || value == b'\x0b'
}

pub fn encode_query_pairs(pairs: &[(Vec<u8>, Vec<u8>)]) -> String {
    let mut encoded = String::new();
    for (index, (name, value)) in pairs.iter().enumerate() {
        if index != 0 {
            encoded.push('&');
        }
        encoded.push_str(&quote_form_component(name));
        encoded.push('=');
        encoded.push_str(&quote_form_component(value));
    }
    encoded
}

fn quote_form_component(value: &[u8]) -> String {
    quote_bytes(value, b"", true)
}

pub fn normalize_percent_escape_hex(uri: &str) -> String {
    let bytes = uri.as_bytes();
    let mut normalized = String::with_capacity(uri.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%'
            && index + 2 < bytes.len()
            && bytes[index + 1].is_ascii_hexdigit()
            && bytes[index + 2].is_ascii_hexdigit()
        {
            normalized.push('%');
            normalized.push(char::from(bytes[index + 1].to_ascii_uppercase()));
            normalized.push(char::from(bytes[index + 2].to_ascii_uppercase()));
            index += 3;
        } else {
            normalized.push(char::from(bytes[index]));
            index += 1;
        }
    }
    normalized
}

pub fn requote_uri(uri: &str) -> String {
    const SAFE_WITH_PERCENT: &[u8] = b"!#$%&'()*+,/:;=?@[]~";
    const SAFE_WITHOUT_PERCENT: &[u8] = b"!#$&'()*+,/:;=?@[]~";

    match unquote_unreserved(uri) {
        Ok(unquoted) => quote_bytes(unquoted.as_bytes(), SAFE_WITH_PERCENT, false),
        Err(()) => quote_bytes(uri.as_bytes(), SAFE_WITHOUT_PERCENT, false),
    }
}

fn unquote_unreserved(uri: &str) -> Result<String, ()> {
    let bytes = uri.as_bytes();
    let mut output = String::with_capacity(uri.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] != b'%' {
            output.push(char::from(bytes[index]));
            index += 1;
            continue;
        }

        if index + 2 < bytes.len()
            && bytes[index + 1].is_ascii_alphanumeric()
            && bytes[index + 2].is_ascii_alphanumeric()
        {
            let value = hex_value(bytes[index + 1])
                .and_then(|high| hex_value(bytes[index + 2]).map(|low| high * 16 + low))
                .ok_or(())?;
            if value.is_ascii_alphanumeric() || matches!(value, b'-' | b'.' | b'_' | b'~') {
                output.push(char::from(value));
            } else {
                output.push('%');
                output.push(char::from(bytes[index + 1]));
                output.push(char::from(bytes[index + 2]));
            }
            index += 3;
        } else {
            output.push('%');
            index += 1;
        }
    }
    Ok(output)
}

fn hex_value(value: u8) -> Option<u8> {
    match value {
        b'0'..=b'9' => Some(value - b'0'),
        b'a'..=b'f' => Some(value - b'a' + 10),
        b'A'..=b'F' => Some(value - b'A' + 10),
        _ => None,
    }
}

fn quote_bytes(value: &[u8], safe: &[u8], plus_for_space: bool) -> String {
    const HEX: &[u8; 16] = b"0123456789ABCDEF";
    let mut quoted = String::with_capacity(value.len());
    for &byte in value {
        if byte.is_ascii_alphanumeric()
            || matches!(byte, b'-' | b'.' | b'_' | b'~')
            || safe.contains(&byte)
        {
            quoted.push(char::from(byte));
        } else if plus_for_space && byte == b' ' {
            quoted.push('+');
        } else {
            quoted.push('%');
            quoted.push(char::from(HEX[usize::from(byte >> 4)]));
            quoted.push(char::from(HEX[usize::from(byte & 0x0f)]));
        }
    }
    quoted
}

#[cfg(test)]
mod tests {
    use super::{
        HeaderValidationError, encode_query_pairs, normalize_percent_escape_hex, requote_uri,
        trim_python_whitespace_start, validate_header, validate_header_bytes,
    };

    #[test]
    fn uri_requoting_decodes_unreserved_and_quotes_invalid_percents() {
        assert_eq!(
            requote_uri("http://example.com/a%7E/%2F?q=%"),
            "http://example.com/a~/%2F?q=%"
        );
        assert_eq!(
            requote_uri("http://example.com/%zz?q=%"),
            "http://example.com/%25zz?q=%25"
        );
        assert_eq!(
            normalize_percent_escape_hex("http://x/%7e/%2f"),
            "http://x/%7E/%2F"
        );
    }

    #[test]
    fn query_encoding_matches_urlencode_for_bytes_and_repeated_rows() {
        let pairs = vec![
            (b"repeat".to_vec(), b"one".to_vec()),
            (b"repeat".to_vec(), b"two words".to_vec()),
            (b"raw".to_vec(), vec![0xff]),
        ];
        assert_eq!(
            encode_query_pairs(&pairs),
            "repeat=one&repeat=two+words&raw=%FF"
        );
    }

    #[test]
    fn header_validation_preserves_text_and_byte_rules() {
        assert_eq!(validate_header("Name", ""), Ok(()));
        assert_eq!(validate_header_bytes(b"Name", b"value"), Ok(()));
        assert_eq!(
            validate_header(" Name", "value"),
            Err(HeaderValidationError::InvalidName)
        );
        assert_eq!(
            validate_header_bytes(b"Name", b"\tvalue"),
            Err(HeaderValidationError::InvalidValue)
        );
        assert_eq!(
            validate_header_bytes(b"\x0bName", b"value"),
            Err(HeaderValidationError::InvalidName)
        );
        assert_eq!(
            validate_header_bytes(b"Name", b"\x0bvalue"),
            Err(HeaderValidationError::InvalidValue)
        );
        for separator in ['\u{1c}', '\u{1d}', '\u{1e}', '\u{1f}'] {
            assert_eq!(
                validate_header(&format!("{separator}Name"), "value"),
                Err(HeaderValidationError::InvalidName)
            );
            assert_eq!(
                validate_header("Name", &format!("{separator}value")),
                Err(HeaderValidationError::InvalidValue)
            );
        }
    }

    #[test]
    fn leading_whitespace_matches_python_lstrip() {
        assert_eq!(
            trim_python_whitespace_start("\u{1c}\u{1f} mailto:user@example.org"),
            "mailto:user@example.org"
        );
    }
}
