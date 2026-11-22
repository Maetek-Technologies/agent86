//! Formatting for the basic catalog's functions, in the `en-US` locale every
//! reference SDK defaults to: numbers and currency as `Intl.NumberFormat`
//! writes them, dates by TR35 pattern, plural categories, and the string forms
//! JavaScript gives values (`String(value)`), so output is the same on every
//! renderer.

use serde_json::Value;

/// A number as JavaScript's `String(number)` writes it: shortest round-trip
/// digits, fixed notation between 1e-7 and 1e21, exponential outside.
pub fn number_string(value: f64) -> String {
    if value.is_nan() {
        return "NaN".into();
    }
    if value.is_infinite() {
        return if value > 0.0 { "Infinity" } else { "-Infinity" }.into();
    }
    if value == 0.0 {
        return "0".into();
    }
    let (negative, digits, exponent) = decompose(value);
    let sign = if negative { "-" } else { "" };
    let n = exponent + 1; // position of the decimal point after the first digit
    let k = digits.len() as i32;
    let body = if (-5..=21).contains(&n) && n <= 21 && n > -6 {
        if k <= n {
            format!("{digits}{}", "0".repeat((n - k) as usize))
        } else if n > 0 {
            format!("{}.{}", &digits[..n as usize], &digits[n as usize..])
        } else {
            format!("0.{}{digits}", "0".repeat((-n) as usize))
        }
    } else {
        let mantissa = if k == 1 {
            digits.clone()
        } else {
            format!("{}.{}", &digits[..1], &digits[1..])
        };
        let e = n - 1;
        format!("{mantissa}e{}{}", if e < 0 { "-" } else { "+" }, e.abs())
    };
    format!("{sign}{body}")
}

/// A finite, nonzero float's sign, shortest decimal digits, and decimal
/// exponent of its first digit (`1234.5` is `("12345", 3)`).
fn decompose(value: f64) -> (bool, String, i32) {
    let text = format!("{:e}", value.abs());
    let (mantissa, exponent) = text.split_once('e').unwrap_or((&text, "0"));
    let digits: String = mantissa.chars().filter(char::is_ascii_digit).collect();
    let digits = digits.trim_end_matches('0');
    let digits = if digits.is_empty() { "0" } else { digits };
    (
        value < 0.0,
        digits.to_string(),
        exponent.parse().unwrap_or(0),
    )
}

/// A value as JavaScript's `String(value)` writes it, with objects and arrays
/// as JSON and `null` as `""` (the specification's type conversion rule).
pub fn to_string(value: &Value) -> String {
    match value {
        Value::Null => String::new(),
        Value::String(s) => s.clone(),
        Value::Bool(b) => b.to_string(),
        Value::Number(n) => n.as_f64().map(number_string).unwrap_or_default(),
        Value::Array(_) | Value::Object(_) => value.to_string(),
    }
}

/// A value as JavaScript's `Number(value)` reads it (`NaN` when it is not a
/// number).
pub fn to_number(value: &Value) -> f64 {
    match value {
        Value::Null => 0.0,
        Value::Bool(b) => f64::from(u8::from(*b)),
        Value::Number(n) => n.as_f64().unwrap_or(f64::NAN),
        Value::String(s) => {
            let s = s.trim();
            if s.is_empty() {
                return 0.0;
            }
            match s {
                "Infinity" | "+Infinity" => f64::INFINITY,
                "-Infinity" => f64::NEG_INFINITY,
                _ => {
                    let decimal = s
                        .bytes()
                        .all(|b| b.is_ascii_digit() || b"+-.eE".contains(&b));
                    if decimal {
                        s.parse().unwrap_or(f64::NAN)
                    } else if let Some(hex) = s.strip_prefix("0x").or_else(|| s.strip_prefix("0X"))
                    {
                        u64::from_str_radix(hex, 16).map_or(f64::NAN, |n| n as f64)
                    } else {
                        f64::NAN
                    }
                }
            }
        }
        Value::Array(items) => match items.as_slice() {
            [] => 0.0,
            [one] => to_number(&Value::String(to_string(one))),
            _ => f64::NAN,
        },
        Value::Object(_) => f64::NAN,
    }
}

/// Round a decimal digit string half away from zero (as `Intl` does) to
/// `places` fraction digits, from a float's shortest representation.
fn fixed(value: f64, places: usize) -> String {
    let (_, digits, exponent) = decompose(value);
    // The digits as an integer and the position of the decimal point.
    let point = exponent + 1;
    let mut int: Vec<u8> = Vec::new();
    let mut frac: Vec<u8> = Vec::new();
    for (i, d) in digits.bytes().enumerate() {
        if (i as i32) < point {
            int.push(d - b'0');
        } else {
            frac.push(d - b'0');
        }
    }
    if point > digits.len() as i32 {
        int.extend(std::iter::repeat_n(
            0,
            (point - digits.len() as i32) as usize,
        ));
    }
    if point < 0 {
        let mut padded = vec![0u8; (-point) as usize];
        padded.extend(frac);
        frac = padded;
    }
    if int.is_empty() {
        int.push(0);
    }
    let round_up = frac.get(places).is_some_and(|&d| d >= 5);
    frac.resize(places.max(frac.len()), 0);
    frac.truncate(places);
    if round_up {
        let mut carry = true;
        for d in frac.iter_mut().rev().chain(int.iter_mut().rev()) {
            if !carry {
                break;
            }
            if *d == 9 {
                *d = 0;
            } else {
                *d += 1;
                carry = false;
            }
        }
        if carry {
            int.insert(0, 1);
        }
    }
    let int: String = int.iter().map(|d| char::from(b'0' + d)).collect();
    let int = int.trim_start_matches('0');
    let int = if int.is_empty() { "0" } else { int };
    let frac: String = frac.iter().map(|d| char::from(b'0' + d)).collect();
    if frac.is_empty() {
        int.to_string()
    } else {
        format!("{int}.{frac}")
    }
}

fn group(int: &str) -> String {
    let mut out = String::new();
    for (i, c) in int.chars().enumerate() {
        if i > 0 && (int.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

/// The digits of a number as `Intl.NumberFormat("en-US")` writes them, without
/// sign: `decimals` fixes the fraction digits (else up to 3, trailing zeros
/// dropped); grouping uses `,`.
fn digits(value: f64, decimals: Option<usize>, grouping: bool) -> String {
    let text = match decimals {
        Some(places) => fixed(value.abs(), places),
        None => {
            let text = fixed(value.abs(), 3);
            match text.split_once('.') {
                Some((int, frac)) => {
                    let frac = frac.trim_end_matches('0');
                    if frac.is_empty() {
                        int.to_string()
                    } else {
                        format!("{int}.{frac}")
                    }
                }
                None => text,
            }
        }
    };
    let (int, frac) = text
        .split_once('.')
        .map_or((text.as_str(), None), |(i, f)| (i, Some(f)));
    let int = if grouping {
        group(int)
    } else {
        int.to_string()
    };
    match frac {
        Some(frac) => format!("{int}.{frac}"),
        None => int,
    }
}

fn is_negative_nonzero(value: f64, decimals: Option<usize>) -> bool {
    value < 0.0
        && digits(value, decimals, false)
            .bytes()
            .any(|b| b.is_ascii_digit() && b != b'0')
}

/// `formatNumber`: `en-US` digits with grouping unless `grouping` is false.
pub fn format_number(value: f64, decimals: Option<usize>, grouping: bool) -> String {
    if value.is_nan() {
        return String::new();
    }
    if value.is_infinite() {
        return if value > 0.0 { "∞" } else { "-∞" }.into();
    }
    let sign = if is_negative_nonzero(value, decimals) {
        "-"
    } else {
        ""
    };
    format!("{sign}{}", digits(value, decimals, grouping))
}

/// The `en-US` symbol `Intl` uses for a currency, when it has one.
fn currency_symbol(code: &str) -> Option<&'static str> {
    Some(match code {
        "USD" => "$",
        "EUR" => "€",
        "GBP" => "£",
        "JPY" => "¥",
        "CNY" => "CN¥",
        "INR" => "₹",
        "KRW" => "₩",
        "CAD" => "CA$",
        "AUD" => "A$",
        "NZD" => "NZ$",
        "MXN" => "MX$",
        "BRL" => "R$",
        "HKD" => "HK$",
        "TWD" => "NT$",
        "ILS" => "₪",
        "VND" => "₫",
        "PHP" => "₱",
        "XAF" => "FCFA",
        "XCD" => "EC$",
        "XPF" => "CFPF",
        _ => return None,
    })
}

/// `formatCurrency`: the amount with the currency's `en-US` symbol, or its
/// code and a no-break space when it has none; 2 fraction digits unless
/// `decimals` says otherwise.
pub fn format_currency(
    value: f64,
    currency: &str,
    decimals: Option<usize>,
    grouping: bool,
) -> String {
    if value.is_nan() {
        return String::new();
    }
    let code = currency.to_uppercase();
    let places = Some(decimals.unwrap_or(2));
    let sign = if is_negative_nonzero(value, places) {
        "-"
    } else {
        ""
    };
    let amount = digits(value, places, grouping);
    match currency_symbol(&code) {
        Some(symbol) => format!("{sign}{symbol}{amount}"),
        None => format!("{sign}{code}\u{a0}{amount}"),
    }
}

/// The plural category of a number in `en`: `one` for 1, `other` otherwise,
/// with the explicit forms every SDK checks first (`zero` for 0, `one` for 1,
/// `two` for 2) when given.
pub fn plural_category(value: f64, has: impl Fn(&str) -> bool) -> &'static str {
    if value == 0.0 && has("zero") {
        "zero"
    } else if value == 1.0 && has("one") {
        "one"
    } else if value == 2.0 && has("two") {
        "two"
    } else if value == 1.0 {
        "one"
    } else {
        "other"
    }
}

const MONTHS: [&str; 12] = [
    "January",
    "February",
    "March",
    "April",
    "May",
    "June",
    "July",
    "August",
    "September",
    "October",
    "November",
    "December",
];
const WEEKDAYS: [&str; 7] = [
    "Monday",
    "Tuesday",
    "Wednesday",
    "Thursday",
    "Friday",
    "Saturday",
    "Sunday",
];

struct Timestamp {
    /// The date and time as written (its own offset kept).
    wall: jiff::civil::DateTime,
    /// The instant, in UTC.
    instant: jiff::Timestamp,
}

/// Parse an ISO 8601 date or date-time. A value with no offset is read as
/// UTC; one with an offset keeps its wall-clock fields for formatting. Fields
/// that do not name a real date or time (`2026-02-30`) fail.
fn parse_timestamp(text: &str) -> Option<Timestamp> {
    static SHAPE: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    let shape = SHAPE.get_or_init(|| {
        regex::Regex::new(
            r"^([+-]?\d{4,6})-(\d{2})-(\d{2})(?:[T ](\d{2})(?::(\d{2})(?::(\d{2})(?:\.(\d+))?)?)?)?(Z|[+-]\d{2}:?\d{2})?$",
        )
        .expect("timestamp pattern")
    });
    let caps = shape.captures(text.trim())?;
    let number = |i: usize| {
        caps.get(i)
            .map(|m| m.as_str().parse::<i32>().ok())
            .unwrap_or(Some(0))
    };
    let year = i16::try_from(number(1)?).ok()?;
    let month = i8::try_from(number(2)?).ok()?;
    let day = i8::try_from(number(3)?).ok()?;
    let hour = i8::try_from(number(4)?).ok()?;
    let minute = i8::try_from(number(5)?).ok()?;
    let second = i8::try_from(number(6)?).ok()?;
    let nanos = caps.get(7).map_or(0, |m| {
        let digits: String = m.as_str().chars().take(9).collect();
        format!("{digits:0<9}").parse::<i32>().unwrap_or(0)
    });
    let wall = jiff::civil::DateTime::new(year, month, day, hour, minute, second, nanos).ok()?;
    let offset_seconds = match caps.get(8).map(|m| m.as_str()) {
        None | Some("Z") => 0,
        Some(offset) => {
            let sign = if offset.starts_with('-') { -1 } else { 1 };
            let digits: String = offset.chars().filter(char::is_ascii_digit).collect();
            let hours: i32 = digits.get(..2)?.parse().ok()?;
            let minutes: i32 = digits.get(2..)?.parse().ok()?;
            sign * (hours * 3600 + minutes * 60)
        }
    };
    let offset = jiff::tz::Offset::from_seconds(offset_seconds).ok()?;
    let instant = offset.to_timestamp(wall).ok()?;
    Some(Timestamp { wall, instant })
}

/// `formatDate`: a TR35 pattern applied to the date as written
/// (`yyyy yy MMMM MMM MM M EEEE E dd d HH H hh h mm ss a`), or `ISO` for the
/// instant in UTC with milliseconds. A value that does not parse gives `""`.
pub fn format_date(value: &Value, pattern: &str) -> String {
    let text = match value {
        Value::String(s) if !s.is_empty() => s.clone(),
        _ => return String::new(),
    };
    let Some(parsed) = parse_timestamp(&text) else {
        return String::new();
    };
    let pattern = if pattern.is_empty() {
        "yyyy-MM-dd"
    } else {
        pattern
    };
    if pattern == "ISO" {
        let utc = jiff::tz::Offset::UTC.to_datetime(parsed.instant);
        return format!(
            "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}.{:03}Z",
            utc.year(),
            utc.month(),
            utc.day(),
            utc.hour(),
            utc.minute(),
            utc.second(),
            utc.subsec_nanosecond() / 1_000_000
        );
    }
    let wall = parsed.wall;
    let hour = wall.hour();
    let hour12 = if hour % 12 == 0 { 12 } else { hour % 12 };
    let month = usize::try_from(wall.month() - 1).unwrap_or(0);
    let weekday = usize::try_from(wall.weekday().to_monday_zero_offset()).unwrap_or(0);
    const TOKENS: [&str; 17] = [
        "yyyy", "yy", "MMMM", "MMM", "MM", "M", "EEEE", "E", "dd", "d", "HH", "H", "hh", "h", "mm",
        "ss", "a",
    ];
    let mut out = String::new();
    let mut rest = pattern;
    'scan: while !rest.is_empty() {
        for token in TOKENS {
            if let Some(after) = rest.strip_prefix(token) {
                let piece = match token {
                    "yyyy" => wall.year().to_string(),
                    "yy" => format!("{:02}", wall.year().rem_euclid(100)),
                    "MMMM" => MONTHS[month].to_string(),
                    "MMM" => MONTHS[month][..3].to_string(),
                    "MM" => format!("{:02}", wall.month()),
                    "M" => wall.month().to_string(),
                    "EEEE" => WEEKDAYS[weekday].to_string(),
                    "E" => WEEKDAYS[weekday][..3].to_string(),
                    "dd" => format!("{:02}", wall.day()),
                    "d" => wall.day().to_string(),
                    "HH" => format!("{hour:02}"),
                    "H" => hour.to_string(),
                    "hh" => format!("{hour12:02}"),
                    "h" => hour12.to_string(),
                    "mm" => format!("{:02}", wall.minute()),
                    "ss" => format!("{:02}", wall.second()),
                    _ => if hour < 12 { "AM" } else { "PM" }.to_string(),
                };
                out.push_str(&piece);
                rest = after;
                continue 'scan;
            }
        }
        let mut chars = rest.chars();
        if let Some(c) = chars.next() {
            out.push(c);
        }
        rest = chars.as_str();
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn numbers_print_as_javascript_prints_them() {
        for (value, text) in [
            (1.0, "1"),
            (-0.0, "0"),
            (0.1 + 0.2, "0.30000000000000004"),
            (1234.5, "1234.5"),
            (1e21, "1e+21"),
            (1.5e-7, "1.5e-7"),
            (0.000001, "0.000001"),
            (123456789012345680000.0, "123456789012345680000"),
            (-42.25, "-42.25"),
        ] {
            assert_eq!(number_string(value), text, "{value}");
        }
    }

    #[test]
    fn number_formats_match_intl_en_us() {
        assert_eq!(format_number(1234.567, Some(2), true), "1,234.57");
        assert_eq!(format_number(1234.567, Some(0), false), "1235");
        assert_eq!(format_number(1234.5678, None, true), "1,234.568");
        assert_eq!(format_number(1.005, Some(2), true), "1.01");
        assert_eq!(format_number(0.125, Some(2), true), "0.13");
        assert_eq!(format_number(-0.001, Some(2), true), "0.00");
        assert_eq!(format_number(-1234.5, Some(1), true), "-1,234.5");
        assert_eq!(format_number(999.999, Some(2), true), "1,000.00");
        assert_eq!(
            format_number(1e21, Some(0), true),
            "1,000,000,000,000,000,000,000"
        );
        assert_eq!(format_currency(99.99, "usd", Some(2), true), "$99.99");
        assert_eq!(format_currency(1234.5, "USD", None, true), "$1,234.50");
        assert_eq!(format_currency(-5.0, "EUR", None, true), "-€5.00");
        assert_eq!(format_currency(5.0, "CHF", None, true), "CHF\u{a0}5.00");
    }

    #[test]
    fn dates_format_as_written() {
        let date = |value: &str, pattern: &str| format_date(&json!(value), pattern);
        assert_eq!(date("2026-08-26T12:00:00Z", "yyyy-MM-dd"), "2026-08-26");
        assert_eq!(
            date("2026-09-04T23:30:00-05:00", "yyyy-MM-dd HH:mm"),
            "2026-09-04 23:30"
        );
        assert_eq!(
            date("2026-01-16T14:30:00Z", "EEEE, d MMMM"),
            "Friday, 16 January"
        );
        assert_eq!(date("2026-01-16T14:30:00Z", "h:mm a"), "2:30 PM");
        assert_eq!(
            date("2026-01-16T00:05:00Z", "hh:mm a, MMM dd, yy"),
            "12:05 AM, Jan 16, 26"
        );
        assert_eq!(
            date("2026-09-04T23:30:00-05:00", "ISO"),
            "2026-09-05T04:30:00.000Z"
        );
        assert_eq!(date("2026-02-30", "yyyy-MM-dd"), "");
        assert_eq!(date("not a date", "yyyy"), "");
    }

    #[test]
    fn javascript_number_reads() {
        assert_eq!(to_number(&json!("42.5")), 42.5);
        assert_eq!(to_number(&json!(" 7 ")), 7.0);
        assert!(to_number(&json!("not_a_number")).is_nan());
        assert_eq!(to_number(&json!(null)), 0.0);
        assert_eq!(to_number(&json!(true)), 1.0);
        assert!(to_number(&json!({})).is_nan());
    }
}
