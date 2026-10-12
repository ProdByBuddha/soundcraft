//! Opt-in automatic-discovery policy, fixed before the process starts.

use std::ffi::OsStr;

/// None is the OS wildcard; an explicit policy never yields that wildcard.
pub(crate) fn manufacturers(value: Option<&OsStr>) -> Vec<Option<u32>> {
    let Some(value) = value else { return vec![None] };
    let Some(value) = value.to_str() else { return Vec::new() };
    let mut out = Vec::new();
    for field in value.split(',').take(64) {
        if let Some(code) = crate::parse_fourcc(field.trim())
            && code != 0
            && !out.contains(&Some(code))
        {
            out.push(Some(code));
        }
    }
    out
}

/// Empty policy performs no OS enumeration, rather than filtering returned components.
pub(crate) fn enumerate<T>(policy: &[Option<u32>], mut discover: impl FnMut(Option<u32>) -> Vec<T>) -> Vec<T> {
    let mut out = Vec::new();
    for &manufacturer in policy {
        out.extend(discover(manufacturer));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn absent_policy_keeps_os_wildcard_but_explicit_policy_does_not() {
        assert_eq!(manufacturers(None), vec![None]);
        assert_eq!(manufacturers(Some(OsStr::new("appl, #6170706C, other, #00000000"))), vec![Some(u32::from_be_bytes(*b"appl"))]);
        assert_eq!(manufacturers(Some(OsStr::new("appl, Acme"))), vec![Some(u32::from_be_bytes(*b"appl")), Some(u32::from_be_bytes(*b"Acme"))]);
    }

    #[test]
    fn empty_or_invalid_policy_never_calls_discovery() {
        for value in ["", "invalid", "#00000000", ",,"] {
            let policy = manufacturers(Some(OsStr::new(value)));
            let out: Vec<()> = enumerate(&policy, |_| panic!("discovery must not run"));
            assert!(out.is_empty());
        }
    }

    #[test]
    fn manufacturer_filter_reaches_discovery_before_components_are_returned() {
        let policy = manufacturers(Some(OsStr::new("appl")));
        let mut queries = Vec::new();
        let result = enumerate(&policy, |manufacturer| {
            queries.push(manufacturer);
            vec!["Apple test component"]
        });
        assert_eq!(queries, vec![Some(u32::from_be_bytes(*b"appl"))]);
        assert_eq!(result, vec!["Apple test component"]);
    }
}
