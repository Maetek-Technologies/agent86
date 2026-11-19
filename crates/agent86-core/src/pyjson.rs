//! JSON number formatting that matches Python's `json.dumps`.
//!
//! Python writes floats with `repr()`: the shortest round-tripping digits, in
//! fixed notation unless the decimal exponent is below -4 or at least 16, with
//! a two-digit minimum exponent (`1.44e-05`, `1e+16`) and a trailing `.0` on
//! integral values. `serde_json` picks the same digits but lays them out
//! differently (`0.0000144`, `1e16`), so fields on the `run --json` contract
//! serialize through [`python_float`].

use serde::Serializer;
use serde_json::value::RawValue;

/// Format `value` exactly as Python's `repr(float)` does.
///
/// Non-finite values use Python's spellings (`nan`, `inf`, `-inf`), which are
/// not valid JSON; [`python_float`] never emits them.
pub fn python_float_repr(value: f64) -> String {
    if value.is_nan() {
        return "nan".to_owned();
    }
    if value.is_infinite() {
        return if value > 0.0 { "inf" } else { "-inf" }.to_owned();
    }
    if value == 0.0 {
        return if value.is_sign_negative() {
            "-0.0"
        } else {
            "0.0"
        }
        .to_owned();
    }

    // Rust's `{:e}` yields the shortest round-tripping digits, e.g. `-1.44e-5`.
    let scientific = format!("{value:e}");
    let (mantissa, exponent) = scientific
        .split_once('e')
        .expect("LowerExp output always has an exponent");
    let exponent: i32 = exponent.parse().expect("LowerExp exponent is an integer");
    let (sign, mantissa) = match mantissa.strip_prefix('-') {
        Some(rest) => ("-", rest),
        None => ("", mantissa),
    };
    let digits: String = mantissa.chars().filter(char::is_ascii_digit).collect();

    // Python's `decpt`: the value is 0.<digits> x 10^decpt.
    let decpt = exponent + 1;
    let body = if (-3..=16).contains(&decpt) {
        fixed(&digits, decpt)
    } else {
        let (first, rest) = digits.split_at(1);
        let fraction = if rest.is_empty() {
            String::new()
        } else {
            format!(".{rest}")
        };
        let exponent_sign = if exponent < 0 { '-' } else { '+' };
        format!("{first}{fraction}e{exponent_sign}{:02}", exponent.abs())
    };
    format!("{sign}{body}")
}

fn fixed(digits: &str, decpt: i32) -> String {
    if decpt <= 0 {
        let zeros = "0".repeat(decpt.unsigned_abs() as usize);
        format!("0.{zeros}{digits}")
    } else {
        let split = decpt as usize;
        if digits.len() <= split {
            let zeros = "0".repeat(split - digits.len());
            format!("{digits}{zeros}.0")
        } else {
            format!("{}.{}", &digits[..split], &digits[split..])
        }
    }
}

/// Serialize an `f64` field the way Python's `json.dumps` writes it.
///
/// Non-finite values fall back to `serde_json`'s handling (`null`), since
/// Python's `NaN` and `Infinity` are not valid JSON.
pub fn python_float<S: Serializer>(value: &f64, serializer: S) -> Result<S::Ok, S::Error> {
    if !value.is_finite() {
        return serializer.serialize_f64(*value);
    }
    match RawValue::from_string(python_float_repr(*value)) {
        Ok(raw) => serde::Serialize::serialize(&raw, serializer),
        Err(_) => serializer.serialize_f64(*value),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn floats_match_python_repr() {
        // Expected strings were produced by CPython's repr().
        let cases = [
            (0.0, "0.0"),
            (-0.0, "-0.0"),
            (1.0, "1.0"),
            (0.1, "0.1"),
            (0.0123, "0.0123"),
            (1.048, "1.048"),
            (0.0001, "0.0001"),
            (0.00012, "0.00012"),
            (1.44e-5, "1.44e-05"),
            (1e-5, "1e-05"),
            (5.999999999999999e-6, "5.999999999999999e-06"),
            (-2.5e-7, "-2.5e-07"),
            (123456.789, "123456.789"),
            (1e15, "1000000000000000.0"),
            (1e16, "1e+16"),
            (1.5e300, "1.5e+300"),
            (5e-324, "5e-324"),
        ];
        for (value, expected) in cases {
            assert_eq!(python_float_repr(value), expected, "{value:e}");
        }
    }
}

/// Format `value` exactly as Python's `json.dumps(value, ensure_ascii=...)` does.
///
/// Python's defaults: `", "` and `": "` separators, keys in insertion order
/// (preserved by `serde_json`'s `preserve_order` feature), floats via `repr()`,
/// and with `ensure_ascii` every non-ASCII character escaped as `\uXXXX`
/// (UTF-16 surrogate pairs above the Basic Multilingual Plane).
pub fn python_json_dumps(value: &serde_json::Value, ensure_ascii: bool) -> String {
    let mut out = String::new();
    write_value(&mut out, value, ensure_ascii, None, 0);
    out
}

/// `json.dumps(value, ensure_ascii=..., indent=indent)`: one item per line, `", "`
/// becoming `","` at line ends, and empty containers kept as `[]` and `{}`.
pub fn python_json_dumps_indented(
    value: &serde_json::Value,
    ensure_ascii: bool,
    indent: usize,
) -> String {
    let mut out = String::new();
    write_value(&mut out, value, ensure_ascii, Some(indent), 0);
    out
}

/// The separator before item `index` of a container at `depth`.
fn separator(out: &mut String, index: usize, indent: Option<usize>, depth: usize) {
    match indent {
        Some(width) => {
            if index > 0 {
                out.push(',');
            }
            out.push('\n');
            out.push_str(&" ".repeat(width * (depth + 1)));
        }
        None if index > 0 => out.push_str(", "),
        None => {}
    }
}

fn close(out: &mut String, bracket: char, indent: Option<usize>, depth: usize) {
    if let Some(width) = indent {
        out.push('\n');
        out.push_str(&" ".repeat(width * depth));
    }
    out.push(bracket);
}

fn write_value(
    out: &mut String,
    value: &serde_json::Value,
    ensure_ascii: bool,
    indent: Option<usize>,
    depth: usize,
) {
    use serde_json::Value;
    match value {
        Value::Array(items) if items.is_empty() => out.push_str("[]"),
        Value::Object(map) if map.is_empty() => out.push_str("{}"),
        Value::Array(items) if indent.is_some() => {
            out.push('[');
            for (index, item) in items.iter().enumerate() {
                separator(out, index, indent, depth);
                write_value(out, item, ensure_ascii, indent, depth + 1);
            }
            close(out, ']', indent, depth);
        }
        Value::Object(map) if indent.is_some() => {
            out.push('{');
            for (index, (key, item)) in map.iter().enumerate() {
                separator(out, index, indent, depth);
                write_string(out, key, ensure_ascii);
                out.push_str(": ");
                write_value(out, item, ensure_ascii, indent, depth + 1);
            }
            close(out, '}', indent, depth);
        }
        _ => write_compact(out, value, ensure_ascii),
    }
}

fn write_compact(out: &mut String, value: &serde_json::Value, ensure_ascii: bool) {
    use serde_json::Value;
    match value {
        Value::Null => out.push_str("null"),
        Value::Bool(flag) => out.push_str(if *flag { "true" } else { "false" }),
        Value::Number(number) => match (number.as_i64(), number.as_u64(), number.as_f64()) {
            (Some(int), _, _) => out.push_str(&int.to_string()),
            (None, Some(int), _) => out.push_str(&int.to_string()),
            (None, None, Some(float)) if float.is_finite() => {
                out.push_str(&python_float_repr(float));
            }
            _ => out.push_str(&number.to_string()),
        },
        Value::String(text) => write_string(out, text, ensure_ascii),
        Value::Array(items) => {
            out.push('[');
            for (index, item) in items.iter().enumerate() {
                if index > 0 {
                    out.push_str(", ");
                }
                write_compact(out, item, ensure_ascii);
            }
            out.push(']');
        }
        Value::Object(map) => {
            out.push('{');
            for (index, (key, item)) in map.iter().enumerate() {
                if index > 0 {
                    out.push_str(", ");
                }
                write_string(out, key, ensure_ascii);
                out.push_str(": ");
                write_compact(out, item, ensure_ascii);
            }
            out.push('}');
        }
    }
}

fn write_string(out: &mut String, text: &str, ensure_ascii: bool) {
    const BACKSLASH: char = '\x5c';
    out.push('"');
    for ch in text.chars() {
        // Python's short escapes; everything else below 0x20 (and, with
        // ensure_ascii, everything non-ASCII) becomes a 4-digit hex escape.
        let short = match ch {
            '"' => Some('"'),
            BACKSLASH => Some(BACKSLASH),
            '\x0a' => Some('n'),
            '\x0d' => Some('r'),
            '\x09' => Some('t'),
            '\x08' => Some('b'),
            '\x0c' => Some('f'),
            _ => None,
        };
        if let Some(letter) = short {
            out.push(BACKSLASH);
            out.push(letter);
        } else if (ch as u32) < 0x20 || (ensure_ascii && !ch.is_ascii()) {
            let mut units = [0u16; 2];
            for unit in ch.encode_utf16(&mut units) {
                out.push(BACKSLASH);
                out.push_str(&format!("u{unit:04x}"));
            }
        } else {
            out.push(ch);
        }
    }
    out.push('"');
}

#[cfg(test)]
mod dumps_tests {
    use super::python_json_dumps;
    use serde_json::json;

    /// `|` stands for a backslash, keeping escape sequences out of the source.
    fn esc(text: &str) -> String {
        text.replace('|', "\x5c")
    }

    #[test]
    fn dumps_matches_python_json_dumps() {
        // Expected strings were produced by CPython's json.dumps.
        let value = json!({
            "path": "caf\u{e9}/\u{fc}.txt",
            "n": 1,
            "x": 1.5e-05,
            "list": [true, null],
            "tab": "a\tb\u{1}",
            "quote": "say \"hi\" \x5c",
        });
        assert_eq!(
            python_json_dumps(&value, false),
            esc(
                r#"{"path": "café/ü.txt", "n": 1, "x": 1.5e-05, "list": [true, null], "tab": "a|tb|u0001", "quote": "say |"hi|" ||"}"#
            )
        );
        assert_eq!(
            python_json_dumps(&value, true),
            esc(
                r#"{"path": "caf|u00e9/|u00fc.txt", "n": 1, "x": 1.5e-05, "list": [true, null], "tab": "a|tb|u0001", "quote": "say |"hi|" ||"}"#
            )
        );
        assert_eq!(
            python_json_dumps(&json!({"e": "\u{1f600}"}), true),
            esc(r#"{"e": "|ud83d|ude00"}"#)
        );
        assert_eq!(python_json_dumps(&json!({}), false), "{}");
    }

    #[test]
    fn indented_dumps_matches_python() {
        // CPython: json.dumps(value, indent=2, ensure_ascii=False)
        let value = json!({"a": [1, {"b": null, "c": []}], "d": {}, "e": 1.5e-05, "f": "x"});
        assert_eq!(
            super::python_json_dumps_indented(&value, false, 2),
            "{\n  \"a\": [\n    1,\n    {\n      \"b\": null,\n      \"c\": []\n    }\n  ],\n  \"d\": {},\n  \"e\": 1.5e-05,\n  \"f\": \"x\"\n}"
        );
    }
}
