//! CPython `repr(float)`.
//!
//! Both CPython and Rust emit the *shortest* digit string that round-trips an
//! `f64`; only the notation differs. Rust's `{:e}` gives us those digits, and
//! this module lays them out the way Python does (`1e-05`, `0.0001`, `1.0`).

pub fn repr(value: f64) -> String {
    if value.is_nan() {
        return "nan".into();
    }
    if value.is_infinite() {
        return if value > 0.0 {
            "inf".into()
        } else {
            "-inf".into()
        };
    }
    let scientific = format!("{:e}", value);
    let (sign, body) = match scientific.strip_prefix('-') {
        Some(rest) => ("-", rest),
        None => ("", scientific.as_str()),
    };
    let (mantissa, exponent) = body.split_once('e').unwrap_or((body, "0"));
    let exponent: i32 = exponent.parse().unwrap_or(0);
    let digits: String = mantissa.chars().filter(|c| *c != '.').collect();
    // Position of the decimal point relative to the start of `digits`.
    let point = exponent + 1;
    let body = if !(-3..=16).contains(&point) {
        let mut out = digits[..1].to_string();
        if digits.len() > 1 {
            out.push('.');
            out.push_str(&digits[1..]);
        }
        let shown = point - 1;
        out.push_str(&format!(
            "e{}{:02}",
            if shown < 0 { '-' } else { '+' },
            shown.abs()
        ));
        out
    } else if point <= 0 {
        format!("0.{}{}", "0".repeat((-point) as usize), digits)
    } else if point as usize >= digits.len() {
        format!("{}{}.0", digits, "0".repeat(point as usize - digits.len()))
    } else {
        format!(
            "{}.{}",
            &digits[..point as usize],
            &digits[point as usize..]
        )
    };
    format!("{sign}{body}")
}

#[cfg(test)]
mod tests {
    use super::repr;

    #[test]
    fn matches_cpython() {
        for (value, expected) in [
            (0.0, "0.0"),
            (-0.0, "-0.0"),
            (1.0, "1.0"),
            (0.35, "0.35"),
            (0.1 + 0.2, "0.30000000000000004"),
            (1e-5, "1e-05"),
            (0.0001, "0.0001"),
            (1e16, "1e+16"),
            (1e15, "1000000000000000.0"),
            (123456.789, "123456.789"),
            (-2.5e-7, "-2.5e-07"),
            (1.7976931348623157e308, "1.7976931348623157e+308"),
            (5e-324, "5e-324"),
        ] {
            assert_eq!(repr(value), expected, "{value:e}");
        }
    }
}
