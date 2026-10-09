//! Which releases are newer than this copy.

/// Whether tag `latest` is newer than version `current`. A `-labs.<date>.<commit>` suffix orders
/// builds of the same version by date; other suffixes are ignored. A version that doesn't parse
/// is never newer.
pub fn is_newer(latest: &str, current: &str) -> bool {
    matches!((parse(latest), parse(current)), (Some(l), Some(c)) if l > c)
}

fn parse(v: &str) -> Option<(u64, u64, u64, u64)> {
    let v = v.trim().trim_start_matches(['v', 'V']);
    let mut split = v.splitn(2, ['-', '+']);
    let core = split.next()?;
    let date = split
        .next()
        .and_then(|rest| rest.strip_prefix("labs."))
        .and_then(|rest| rest.split('.').next())
        .and_then(|d| if d.len() == 8 { d.parse::<u64>().ok() } else { None })
        .unwrap_or(0);
    let mut parts = core.split('.');
    let mut next = |required: bool| match parts.next() {
        Some(p) => p.parse::<u64>().ok(),
        None if required => None,
        None => Some(0),
    };
    let version = (next(true)?, next(false)?, next(false)?, date);
    parts.next().is_none().then_some(version)
}

/// The tags newer than `current`, newest first, from `tags` (newest first). When `current` is
/// listed, the ones before it are newer (two builds of a day share a date); otherwise numbers and
/// build dates decide.
pub fn newer<'a>(tags: &[&'a str], current: &str) -> Vec<&'a str> {
    let current = current.trim();
    if let Some(i) = tags.iter().position(|t| *t == current) {
        return tags[..i].to_vec();
    }
    tags.iter().copied().filter(|t| is_newer(t, current)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    const OLD: &str = "v0.2.1-labs.20261007.51eb7a4";
    const MID: &str = "v0.2.1-labs.20261009.845a253";
    const NEW: &str = "v0.3.0-labs.20261010.abc1234";

    #[test]
    fn versions_compare_by_number_then_build_date() {
        assert!(is_newer("v0.2.0", "0.1.1"));
        assert!(is_newer("v0.1.10", "0.1.9"));
        assert!(!is_newer("v0.1.1", "0.1.1"));
        assert!(!is_newer("v0.1.1-beta.2", "0.1.1"));
        assert!(!is_newer("nightly", "0.1.1"));
        assert!(!is_newer("v99999999999999999999.0.0", "0.1.1"));
        assert!(is_newer(MID, OLD));
        assert!(is_newer(NEW, MID));
        assert!(!is_newer(OLD, MID));
        assert!(is_newer(MID, "v0.2.1-labs.test.abc"), "a test build is older than the releases of its version");
    }

    #[test]
    fn newer_tags_are_the_ones_listed_before_ours() {
        let tags = [NEW, MID, OLD];
        assert_eq!(newer(&tags, OLD), vec![NEW, MID]);
        assert_eq!(newer(&tags, MID), vec![NEW]);
        assert!(newer(&tags, NEW).is_empty());
        assert_eq!(newer(&["v0.2.1-labs.20261009.bbbbbbb", "v0.2.1-labs.20261009.aaaaaaa"], "v0.2.1-labs.20261009.aaaaaaa").len(), 1);
        assert_eq!(newer(&["nightly", NEW], MID), vec![NEW], "an odd tag doesn't hide newer ones");
    }
}
