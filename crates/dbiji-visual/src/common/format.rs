//! Number, duration and size formatting for labels and tooltips.

/// Compact number: `1284` -> `1.3K`, `4_200_000` -> `4.2M`. Values below
/// 1000 keep up to `decimals` fraction digits (trailing zeros removed).
pub(crate) fn compact(v: f64, decimals: usize) -> String {
    if !v.is_finite() {
        return if v.is_nan() { "NaN".into() } else if v > 0.0 { "∞".into() } else { "-∞".into() };
    }
    let a = v.abs();
    let (scaled, suffix) = if a >= 1e12 {
        (v / 1e12, "T")
    } else if a >= 1e9 {
        (v / 1e9, "B")
    } else if a >= 1e6 {
        (v / 1e6, "M")
    } else if a >= 1e3 {
        (v / 1e3, "K")
    } else {
        return trim_decimals(v, decimals);
    };
    let digits = if scaled.abs() >= 100.0 { 0 } else { 1 };
    format!("{}{suffix}", trim_decimals(scaled, digits))
}

/// Fixed decimals with trailing zeros (and a trailing dot) removed.
pub(crate) fn trim_decimals(v: f64, decimals: usize) -> String {
    if !v.is_finite() {
        return compact(v, decimals);
    }
    let s = format!("{v:.decimals$}");
    let s = if s.contains('.') { s.trim_end_matches('0').trim_end_matches('.').to_string() } else { s };
    if s == "-0" { "0".into() } else { s }
}

/// Integer-ish count with thousands separators: `1234567` -> `1,234,567`.
pub(crate) fn grouped(v: f64) -> String {
    if !v.is_finite() {
        return compact(v, 0);
    }
    if v.fract().abs() > 1e-9 && v.abs() < 1e15 {
        let s = trim_decimals(v, 2);
        let (int, frac) = s.split_once('.').map(|(a, b)| (a.to_string(), format!(".{b}"))).unwrap_or((s.clone(), String::new()));
        return format!("{}{frac}", group_int(&int));
    }
    group_int(&format!("{:.0}", v))
}

fn group_int(int: &str) -> String {
    let (sign, digits) = match int.strip_prefix('-') {
        Some(d) => ("-", d),
        None => ("", int),
    };
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i) % 3 == 0 {
            out.push(',');
        }
        out.push(c);
    }
    format!("{sign}{out}")
}

/// Duration given in milliseconds: `0.042 ms`, `12.3 ms`, `1.25 s`, `2 min 5 s`.
pub(crate) fn duration_ms(ms: f64) -> String {
    if !ms.is_finite() {
        return "-".into();
    }
    let a = ms.abs();
    if a < 1.0 {
        format!("{} ms", trim_decimals(ms, 3))
    } else if a < 1000.0 {
        format!("{} ms", trim_decimals(ms, 1))
    } else if a < 60_000.0 {
        format!("{} s", trim_decimals(ms / 1000.0, 2))
    } else if a < 3_600_000.0 {
        let total = (ms / 1000.0).round() as i64;
        format!("{} min {} s", total / 60, total % 60)
    } else {
        let total = (ms / 60_000.0).round() as i64;
        format!("{} h {} min", total / 60, total % 60)
    }
}

/// Byte size with binary prefixes.
pub(crate) fn bytes(v: f64) -> String {
    if !v.is_finite() {
        return compact(v, 0);
    }
    const UNITS: [&str; 6] = ["B", "KiB", "MiB", "GiB", "TiB", "PiB"];
    let mut value = v;
    let mut unit = 0;
    while value.abs() >= 1024.0 && unit + 1 < UNITS.len() {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{} {}", trim_decimals(value, 0), UNITS[unit])
    } else {
        format!("{} {}", trim_decimals(value, if value.abs() >= 100.0 { 0 } else { 1 }), UNITS[unit])
    }
}

/// Value with a display unit as used by server metrics ("", "%", "bytes",
/// "ms", "rows" ...). `rate` appends "/s".
pub(crate) fn with_unit(v: f64, unit: &str, rate: bool) -> String {
    let per = if rate { "/s" } else { "" };
    match unit {
        "bytes" | "B" | "byte" => format!("{}{per}", bytes(v)),
        "%" => format!("{}%", trim_decimals(v, 1)),
        "ms" => {
            if rate {
                format!("{} ms{per}", compact(v, 1))
            } else {
                duration_ms(v)
            }
        }
        "s" | "sec" | "seconds" => {
            if rate {
                format!("{} s{per}", compact(v, 2))
            } else {
                duration_ms(v * 1000.0)
            }
        }
        "" => format!("{}{per}", compact(v, 2)),
        other => format!("{} {other}{per}", compact(v, 2)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compact_numbers() {
        assert_eq!(compact(0.0, 2), "0");
        assert_eq!(compact(12.345, 2), "12.35");
        assert_eq!(compact(1284.0, 2), "1.3K");
        assert_eq!(compact(12_900.0, 2), "12.9K");
        assert_eq!(compact(4_200_000.0, 2), "4.2M");
        assert_eq!(compact(-150_000.0, 0), "-150K");
        assert_eq!(compact(f64::NAN, 0), "NaN");
    }

    #[test]
    fn grouping_and_units() {
        assert_eq!(grouped(1_234_567.0), "1,234,567");
        assert_eq!(grouped(-1234.5), "-1,234.5");
        assert_eq!(grouped(12.0), "12");
        assert_eq!(bytes(512.0), "512 B");
        assert_eq!(bytes(1536.0), "1.5 KiB");
        assert_eq!(bytes(5.0 * 1024.0 * 1024.0 * 1024.0), "5 GiB");
        assert_eq!(duration_ms(0.0421), "0.042 ms");
        assert_eq!(duration_ms(12.34), "12.3 ms");
        assert_eq!(duration_ms(1250.0), "1.25 s");
        assert_eq!(duration_ms(125_000.0), "2 min 5 s");
        assert_eq!(with_unit(42.0, "%", false), "42%");
        assert_eq!(with_unit(2048.0, "bytes", true), "2 KiB/s");
        assert_eq!(with_unit(3.0, "rows", true), "3 rows/s");
        assert_eq!(with_unit(3.0, "", false), "3");
    }
}
