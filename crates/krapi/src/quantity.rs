//! THE KUBERNETES QUANTITY — the `quantity()` CEL library's value, a
//! compact port of `k8s.io/apimachinery/pkg/api/resource`'s parse and
//! arithmetic surface. A quantity is `mantissa × 10^exponent` in i128;
//! the binary suffixes (Ki..Ei) fold into the same decimal form exactly
//! (1Ki = 1024 = 1024×10⁰), so comparisons and adds stay exact until an
//! i128 would overflow — at which point the ops refuse rather than lie.

/// One parsed quantity: `mantissa × 10^exponent`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Quantity {
    mantissa: i128,
    exponent: i32,
}

/// The suffix table: (spelling, mantissa multiplier, exponent delta).
/// Two-character suffixes sort first so `Ei` wins over `E`.
const SUFFIXES: &[(&str, u128, i32)] = &[
    ("Ki", 1_024, 0),
    ("Mi", 1_048_576, 0),
    ("Gi", 1_073_741_824, 0),
    ("Ti", 1_099_511_627_776, 0),
    ("Pi", 1_125_899_906_842_624, 0),
    ("Ei", 1_152_921_504_606_846_976, 0),
    ("k", 1, 3),
    ("M", 1, 6),
    ("G", 1, 9),
    ("T", 1, 12),
    ("P", 1, 15),
    ("E", 1, 18),
    ("m", 1, -3),
    ("u", 1, -6),
    ("n", 1, -9),
];

const MAX_EXP: i32 = 64;
const MAX_SCALE: i32 = 39;

impl Quantity {
    /// Parse a Kubernetes quantity:
    /// `[+-]?digits[.digits][e[+-]?digits | Ki|Mi|Gi|Ti|Pi|Ei|k|M|G|T|P|E|m|u|n]`.
    /// An `e`/`E` followed by a sign or digits is scientific; a bare
    /// `E` is the exa suffix.
    ///
    /// # Errors
    /// Empty input, a malformed number, an unknown suffix, or a value
    /// too large for the i128 mantissa.
    pub fn parse(s: &str) -> Result<Self, String> {
        let bad = || format!("quantity {s:?} is not a valid quantity");
        let s = s.trim();
        if s.is_empty() {
            return Err(bad());
        }

        // The scientific tail: an e/E whose remainder is a signed
        // integer. A bare E (or Ei) stays for the suffix split.
        let mut work = s;
        let mut sci_exp: i64 = 0;
        if let Some(p) = work.rfind(['e', 'E']) {
            let tail = &work[p + 1..];
            let (neg, digits) = match tail.strip_prefix(['+', '-']) {
                Some(rest) => (tail.starts_with('-'), rest),
                None => (false, tail),
            };
            if !digits.is_empty() && digits.chars().all(|c| c.is_ascii_digit()) {
                sci_exp = digits.parse::<i64>().map_err(|_| bad())?;
                if neg {
                    sci_exp = -sci_exp;
                }
                work = &work[..p];
            }
        }

        let (num, suffix) = split_suffix(work).ok_or_else(bad)?;
        let (neg, digits) = match num.strip_prefix('-') {
            Some(rest) => (true, rest),
            None => (false, num.strip_prefix('+').unwrap_or(num)),
        };
        if digits.is_empty() {
            return Err(bad());
        }
        let (int_part, frac_part) = match digits.split_once('.') {
            Some((i, f)) => (i, f),
            None => (digits, ""),
        };
        if int_part.is_empty() && frac_part.is_empty() {
            return Err(bad());
        }
        if !int_part.chars().all(|c| c.is_ascii_digit())
            || !frac_part.chars().all(|c| c.is_ascii_digit())
        {
            return Err(bad());
        }
        let mut mantissa: i128 = 0;
        for c in int_part.chars().chain(frac_part.chars()) {
            mantissa = mantissa
                .checked_mul(10)
                .and_then(|m| m.checked_add(i128::from(c as u8 - b'0')))
                .ok_or_else(|| format!("quantity {s:?} overflows"))?;
        }
        if neg {
            mantissa = -mantissa;
        }
        let exponent = i64::from(-i32::try_from(frac_part.len()).map_err(|_| bad())?) + sci_exp;
        let (mul, exp_delta) = parse_suffix_pow(suffix)?;
        let mantissa = mantissa
            .checked_mul(i128::try_from(mul).map_err(|_| bad())?)
            .ok_or_else(|| format!("quantity {s:?} overflows"))?;
        let exponent = i32::try_from(exponent + i64::from(exp_delta)).map_err(|_| bad())?;
        if exponent.abs() > MAX_EXP {
            return Err(format!("quantity {s:?} overflows"));
        }
        Ok(Self { mantissa, exponent })
    }

    /// Exact i64 integer value, when representable.
    #[must_use]
    pub fn as_integer(&self) -> Option<i64> {
        let value = if self.exponent >= 0 {
            let pow = u32::try_from(self.exponent).ok()?;
            self.mantissa.checked_mul(10_i128.checked_pow(pow)?)?
        } else {
            let div = 10_i128.checked_pow(u32::try_from(-self.exponent).ok()?)?;
            if self.mantissa.checked_rem(div)? != 0 {
                return None;
            }
            self.mantissa.checked_div(div)?
        };
        i64::try_from(value).ok()
    }

    /// Approximate f64 value.
    #[must_use]
    pub fn as_float(&self) -> f64 {
        // i128 → f64 rounds past 2^53; quantities that large are already
        // outside exact-float land, which upstream also documents.
        format!("{:e}", self.mantissa)
            .parse::<f64>()
            .map_or(f64::INFINITY, |m| m * 10_f64.powi(self.exponent))
    }

    /// The sign: -1, 0, or 1.
    #[must_use]
    pub fn sign(&self) -> i32 {
        match self.mantissa {
            m if m < 0 => -1,
            0 => 0,
            _ => 1,
        }
    }

    /// Compare two quantities exactly (refusing when the exponent gap
    /// exceeds what an i128 can scale).
    ///
    /// # Errors
    /// The scale between the two exponents overflows i128.
    pub fn compare(&self, other: &Self) -> Result<std::cmp::Ordering, String> {
        match self.exponent.cmp(&other.exponent) {
            std::cmp::Ordering::Equal => Ok(self.mantissa.cmp(&other.mantissa)),
            std::cmp::Ordering::Greater => {
                let scaled = scale(self.mantissa, self.exponent - other.exponent)?;
                Ok(scaled.cmp(&other.mantissa))
            }
            std::cmp::Ordering::Less => {
                let scaled = scale(other.mantissa, other.exponent - self.exponent)?;
                Ok(self.mantissa.cmp(&scaled))
            }
        }
    }

    /// Add or subtract, aligned to the coarser exponent.
    ///
    /// # Errors
    /// The alignment scale or the result overflows i128.
    pub fn binop(&self, other: &Self, op: fn(i128, i128) -> Option<i128>) -> Result<Self, String> {
        let (m, e) = if self.exponent >= other.exponent {
            let scaled = scale(self.mantissa, self.exponent - other.exponent)?;
            (
                op(scaled, other.mantissa).ok_or("quantity arithmetic overflows")?,
                other.exponent,
            )
        } else {
            let scaled = scale(other.mantissa, other.exponent - self.exponent)?;
            (
                op(self.mantissa, scaled).ok_or("quantity arithmetic overflows")?,
                self.exponent,
            )
        };
        Ok(Self {
            mantissa: m,
            exponent: e,
        })
    }
}

/// Scale a mantissa up by `10^diff`, refusing past the i128 range.
fn scale(mantissa: i128, diff: i32) -> Result<i128, String> {
    if !(0..=MAX_SCALE).contains(&diff) {
        return Err("quantity comparison overflows".to_owned());
    }
    let pow = u32::try_from(diff).map_err(|_| "quantity comparison overflows".to_owned())?;
    let factor = 10_i128
        .checked_pow(pow)
        .ok_or_else(|| "quantity comparison overflows".to_owned())?;
    mantissa
        .checked_mul(factor)
        .ok_or_else(|| "quantity comparison overflows".to_owned())
}

/// Split a trailing quantity suffix off; `None` when the tail is not a
/// known suffix and not a bare number.
fn split_suffix(s: &str) -> Option<(&str, &str)> {
    for (name, _, _) in SUFFIXES {
        if let Some(num) = s.strip_suffix(name)
            && num.ends_with(|c: char| c.is_ascii_digit() || c == '.')
            && num.chars().any(|c| c.is_ascii_digit())
        {
            return Some((num, name));
        }
    }
    if s.chars()
        .last()
        .is_some_and(|c| c.is_ascii_digit() || c == '.')
    {
        return Some((s, ""));
    }
    None
}

/// The suffix's (mantissa multiplier, exponent delta).
fn parse_suffix_pow(suffix: &str) -> Result<(u128, i32), String> {
    if suffix.is_empty() {
        return Ok((1, 0));
    }
    for (name, mul, delta) in SUFFIXES {
        if *name == suffix {
            return Ok((*mul, *delta));
        }
    }
    Err(format!("unknown suffix {suffix:?}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(s: &str) -> Result<Quantity, String> {
        Quantity::parse(s)
    }

    #[test]
    fn parse_scalars_suffixes_and_scientific() {
        assert_eq!(parse("5").unwrap().as_integer(), Some(5));
        assert_eq!(parse("100m").unwrap().as_float(), 0.1);
        assert_eq!(parse("1Ki").unwrap().as_integer(), Some(1_024));
        assert_eq!(parse("1Gi").unwrap().as_integer(), Some(1_073_741_824));
        assert_eq!(parse("1.5G").unwrap().as_integer(), Some(1_500_000_000));
        assert_eq!(parse("-3").unwrap().as_integer(), Some(-3));
        assert_eq!(parse("1e3").unwrap().as_integer(), Some(1_000));
        assert_eq!(parse("1.5").unwrap().as_float(), 1.5);
        assert_eq!(parse(".5").unwrap().as_float(), 0.5);
        assert_eq!(
            parse("2E").unwrap().as_integer(),
            Some(2_000_000_000_000_000_000)
        );
        assert!(parse("").is_err());
        assert!(parse("bogus").is_err());
        assert!(parse("12Q").is_err());
        assert!(parse("1.2.3").is_err());
    }

    #[test]
    fn compare_across_suffixes() {
        let l = parse("512Mi").unwrap().compare(&parse("1Gi").unwrap());
        assert_eq!(l, Ok(std::cmp::Ordering::Less));
        let g = parse("2Gi").unwrap().compare(&parse("1Gi").unwrap());
        assert_eq!(g, Ok(std::cmp::Ordering::Greater));
        let e = parse("1000m").unwrap().compare(&parse("1").unwrap());
        assert_eq!(e, Ok(std::cmp::Ordering::Equal));
        let n = parse("-1").unwrap().compare(&parse("1").unwrap());
        assert_eq!(n, Ok(std::cmp::Ordering::Less));
    }

    #[test]
    fn binop_aligns_exponents() {
        let one_gi = parse("1Gi").unwrap();
        let out = one_gi.binop(&one_gi, i128::checked_add).unwrap();
        assert_eq!(out.as_integer(), Some(2 * 1_073_741_824));
        let out = parse("2Gi")
            .unwrap()
            .binop(&one_gi, i128::checked_sub)
            .unwrap();
        assert_eq!(out.as_integer(), Some(1_073_741_824));
        let out = parse("100m")
            .unwrap()
            .binop(&parse("500m").unwrap(), i128::checked_add)
            .unwrap();
        assert_eq!(out.as_float(), 0.6);
    }
}
