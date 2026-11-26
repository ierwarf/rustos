//! Windows runtime strings written directly into their final owned buffers.
//!
//! Arguments/environment entries are already validated NUL-free request views.
//! The image builder owns aggregate byte admission before publishing mappings.

use alloc::string::String;
use alloc::vec::Vec;
use core::ops::Range;

pub fn command_line(argv: &[&str]) -> String {
    let mut line = String::new();
    for (index, arg) in argv.iter().enumerate() {
        if index != 0 {
            line.push(' ');
        }
        append_argument(&mut line, arg);
    }
    line
}

fn append_argument(line: &mut String, arg: &str) {
    if arg.is_empty() {
        line.push_str("\"\"");
        return;
    }
    if !arg.bytes().any(|byte| matches!(byte, b' ' | b'\t' | b'"')) {
        line.push_str(arg);
        return;
    }
    line.push('"');
    let mut backslashes = 0usize;
    for ch in arg.chars() {
        match ch {
            '\\' => backslashes += 1,
            '"' => {
                for _ in 0..(backslashes * 2 + 1) {
                    line.push('\\');
                }
                line.push('"');
                backslashes = 0;
            }
            _ => {
                for _ in 0..backslashes {
                    line.push('\\');
                }
                backslashes = 0;
                line.push(ch);
            }
        }
    }
    for _ in 0..(backslashes * 2) {
        line.push('\\');
    }
    line.push('"');
}

pub fn append_environment_ascii(bytes: &mut Vec<u8>, env: &[&str]) -> Range<usize> {
    let start = bytes.len();
    for item in env {
        bytes.extend_from_slice(item.as_bytes());
        bytes.push(0);
    }
    bytes.push(0);
    if env.is_empty() {
        bytes.push(0);
    }
    start..bytes.len()
}

pub fn append_environment_utf16(bytes: &mut Vec<u8>, env: &[&str]) -> Range<usize> {
    let start = bytes.len();
    for item in env {
        for code_unit in item.encode_utf16() {
            bytes.extend_from_slice(&code_unit.to_le_bytes());
        }
        bytes.extend_from_slice(&0_u16.to_le_bytes());
    }
    bytes.extend_from_slice(&0_u16.to_le_bytes());
    if env.is_empty() {
        bytes.extend_from_slice(&0_u16.to_le_bytes());
    }
    start..bytes.len()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn command_line_preserves_spaces_quotes_backslashes_and_unicode() {
        for (input, expected) in [
            ("", "\"\""),
            ("plain", "plain"),
            ("two words", "\"two words\""),
            ("a\\\"b", "\"a\\\\\\\"b\""),
            ("C:\\a b\\", "\"C:\\a b\\\\\""),
            ("한글", "한글"),
        ] {
            assert_eq!(command_line(&[input]), expected);
        }
        assert_eq!(command_line(&["app", "a b", ""]), "app \"a b\" \"\"");
        assert!(command_line(&[]).is_empty());
    }

    #[test]
    fn environment_blocks_keep_the_prefix_and_always_have_two_terminators() {
        let mut bytes = Vec::from(&b"prefix"[..]);
        let narrow_empty = append_environment_ascii(&mut bytes, &[]);
        let wide_empty = append_environment_utf16(&mut bytes, &[]);
        let narrow = append_environment_ascii(&mut bytes, &["A=1", "B=한글"]);
        let wide = append_environment_utf16(&mut bytes, &["A=1", "B=한\u{1f980}"]);
        assert_eq!(&bytes[..6], b"prefix");
        assert_eq!(&bytes[narrow_empty], &[0, 0]);
        assert_eq!(&bytes[wide_empty], &[0, 0, 0, 0]);
        assert_eq!(&bytes[narrow], "A=1\0B=한글\0\0".as_bytes());
        let expected: Vec<_> = "A=1\0B=한\u{1f980}\0\0"
            .encode_utf16().flat_map(u16::to_le_bytes).collect();
        assert_eq!(&bytes[wide], expected.as_slice());
    }
}
