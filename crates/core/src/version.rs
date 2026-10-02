//! Comparing Bonegrader versions (`1.10.0` > `1.9.3`), used for the client
//! update hints published in the manifest.

use std::cmp::Ordering;

/// Compare dotted numeric versions. Missing components count as `0`
/// (`1.2` == `1.2.0`); a pre-release suffix (`1.2.0-beta`) sorts before the
/// plain release. Non-numeric components compare as `0`.
pub fn compare(a: &str, b: &str) -> Ordering {
    let (a_core, a_pre) = split_pre(a);
    let (b_core, b_pre) = split_pre(b);
    let a_nums: Vec<u64> = a_core.split('.').map(leading_number).collect();
    let b_nums: Vec<u64> = b_core.split('.').map(leading_number).collect();
    for i in 0..a_nums.len().max(b_nums.len()) {
        let x = a_nums.get(i).copied().unwrap_or(0);
        let y = b_nums.get(i).copied().unwrap_or(0);
        match x.cmp(&y) {
            Ordering::Equal => {}
            other => return other,
        }
    }
    match (a_pre, b_pre) {
        (None, None) => Ordering::Equal,
        (Some(_), None) => Ordering::Less,
        (None, Some(_)) => Ordering::Greater,
        (Some(x), Some(y)) => x.cmp(y),
    }
}

/// True if `current` is strictly older than `other`.
pub fn is_older(current: &str, other: &str) -> bool {
    compare(current, other) == Ordering::Less
}

fn split_pre(v: &str) -> (&str, Option<&str>) {
    let v = v.trim().trim_start_matches(['v', 'V']);
    // Build metadata (`+abc`) never affects precedence.
    let v = v.split('+').next().unwrap_or(v);
    match v.split_once('-') {
        Some((core, pre)) => (core, Some(pre)),
        None => (v, None),
    }
}

fn leading_number(part: &str) -> u64 {
    let digits: String = part.chars().take_while(char::is_ascii_digit).collect();
    digits.parse().unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn numeric_components_compare_numerically() {
        assert_eq!(compare("1.10.0", "1.9.3"), Ordering::Greater);
        assert_eq!(compare("1.2.0", "1.2.0"), Ordering::Equal);
        assert_eq!(compare("1.2", "1.2.0"), Ordering::Equal);
        assert_eq!(compare("2.0.0", "10.0.0"), Ordering::Less);
        assert_eq!(compare("v1.3.0", "1.3.0"), Ordering::Equal);
    }

    #[test]
    fn pre_releases_sort_before_releases() {
        assert!(is_older("1.2.0-beta", "1.2.0"));
        assert!(is_older("1.2.0-alpha", "1.2.0-beta"));
        assert!(!is_older("1.2.0", "1.2.0-beta"));
        assert_eq!(compare("1.2.0+build5", "1.2.0"), Ordering::Equal);
    }

    #[test]
    fn is_older_is_strict() {
        assert!(is_older("1.1.0", "1.2.0"));
        assert!(!is_older("1.2.0", "1.2.0"));
        assert!(!is_older("1.3.0", "1.2.0"));
    }
}
