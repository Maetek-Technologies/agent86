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
