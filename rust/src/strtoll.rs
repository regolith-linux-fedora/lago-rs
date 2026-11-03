//! `rm` operand conversion: C-locale `strtoll` with base 0 (ISO C23
//! prefixes), ignoring anything after the number.
//!
//! Leading C whitespace is skipped, one sign is accepted, `0x`/`0X` selects
//! hexadecimal, `0b`/`0B` binary, another leading `0` octal, anything else
//! decimal. Conversion stops at the first character that is not a digit of
//! the base. Out-of-range values saturate to `i64::MIN`/`i64::MAX`. A string
//! with no digits converts to 0.

fn is_c_space(c: u8) -> bool {
    matches!(c, b' ' | b'\t' | b'\n' | 0x0b | 0x0c | b'\r')
}

fn digit_value(c: u8) -> Option<u32> {
    match c {
        b'0'..=b'9' => Some(u32::from(c - b'0')),
        b'a'..=b'z' => Some(u32::from(c - b'a') + 10),
        b'A'..=b'Z' => Some(u32::from(c - b'A') + 10),
        _ => None,
    }
}

pub fn parse_int64(text: &[u8]) -> i64 {
    let mut s = text;
    while let Some((&c, rest)) = s.split_first() {
        if !is_c_space(c) {
            break;
        }
        s = rest;
    }

    let mut negative = false;
    match s.first() {
        Some(b'-') => {
            negative = true;
            s = &s[1..];
        }
        Some(b'+') => s = &s[1..],
        _ => {}
    }

    let base: u32;
    if s.first() == Some(&b'0') {
        let prefix = s.get(1).map(u8::to_ascii_uppercase);
        let digit_after = |radix: u32| {
            s.get(2)
                .and_then(|&c| digit_value(c))
                .is_some_and(|d| d < radix)
        };
        if prefix == Some(b'X') && digit_after(16) {
            base = 16;
            s = &s[2..];
        } else if prefix == Some(b'B') && digit_after(2) {
            base = 2;
            s = &s[2..];
        } else {
            base = 8;
        }
    } else {
        base = 10;
    }

    let mut value: u64 = 0;
    let mut overflow = false;
    for &c in s {
        let digit = match digit_value(c) {
            Some(d) if d < base => u64::from(d),
            _ => break,
        };
        match value
            .checked_mul(u64::from(base))
            .and_then(|v| v.checked_add(digit))
        {
            Some(v) => value = v,
            None => overflow = true,
        }
    }

    let limit = if negative {
        1u64 << 63
    } else {
        i64::MAX as u64
    };
    if overflow || value > limit {
        return if negative { i64::MIN } else { i64::MAX };
    }
    if negative {
        (value as i64).wrapping_neg()
    } else {
        value as i64
    }
}

#[cfg(test)]
mod tests {
    use super::parse_int64;

    #[test]
    fn prefixes() {
        assert_eq!(parse_int64(b"0x10"), 16);
        assert_eq!(parse_int64(b"0X1F"), 31);
        assert_eq!(parse_int64(b"010"), 8);
        assert_eq!(parse_int64(b"0b101"), 5);
        assert_eq!(parse_int64(b"-0b11"), -3);
        assert_eq!(parse_int64(b"0b"), 0);
        assert_eq!(parse_int64(b"0x"), 0);
        assert_eq!(parse_int64(b"0b2"), 0);
        assert_eq!(parse_int64(b"08"), 0);
    }

    #[test]
    fn whitespace_signs_and_tails() {
        assert_eq!(parse_int64(b"  7  "), 7);
        assert_eq!(parse_int64(b"\x0b6"), 6);
        assert_eq!(parse_int64(b"\xc2\xa06"), 0);
        assert_eq!(parse_int64(b"4abc"), 4);
        assert_eq!(parse_int64(b"1Password"), 1);
        assert_eq!(parse_int64(b"- 5"), 0);
        assert_eq!(parse_int64(b"+-5"), 0);
        assert_eq!(parse_int64(b""), 0);
    }

    #[test]
    fn saturation() {
        assert_eq!(parse_int64(b"99999999999999999999999"), i64::MAX);
        assert_eq!(parse_int64(b"-99999999999999999999999"), i64::MIN);
        assert_eq!(parse_int64(b"-0x8000000000000000"), i64::MIN);
        assert_eq!(parse_int64(b"0x8000000000000000"), i64::MAX);
        assert_eq!(parse_int64(b"-0x8000000000000001"), i64::MIN);
    }
}
