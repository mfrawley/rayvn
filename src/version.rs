//! Maven version ordering and version ranges.
//!
//! [`Version`] orders like Maven's `ComparableVersion`: `1.0` equals `1`, `1.0-alpha < 1.0-rc < 1.0-SNAPSHOT <
//! 1.0 < 1.0-sp < 1.0.1`, and `1.2 < 1.10`. [`Constraint`] is a dependency `<version>` written as a range such as
//! `[10.0.0,11.0.0)`; [`Constraint::best`] picks the highest listed version inside it.
//! [Version order](https://maven.apache.org/pom.html#version-order-specification),
//! [ranges](https://maven.apache.org/enforcer/enforcer-rules/versionRanges.html).

use anyhow::{Result, bail};
use std::cmp::Ordering;

const QUALIFIERS: [&str; 7] = ["alpha", "beta", "milestone", "rc", "snapshot", "", "sp"];

#[derive(Debug, Clone)]
enum Item {
    Int(u128),
    /// A qualifier, already mapped through the aliases (`ga`, `final`, `release` are `""`; `cr` is `rc`).
    Str(String),
    List(Vec<Item>),
}

impl Item {
    fn qualifier(value: &str, followed_by_digit: bool) -> Item {
        let value = match (followed_by_digit, value) {
            (true, "a") => "alpha",
            (true, "b") => "beta",
            (true, "m") => "milestone",
            (_, "ga" | "final" | "release") => "",
            (_, "cr") => "rc",
            (_, v) => v,
        };
        Item::Str(value.to_string())
    }

    fn number(digits: &str) -> Item {
        Item::Int(digits.parse().unwrap_or(u128::MAX))
    }

    /// Trailing zeros, empty qualifiers and empty lists carry no information.
    fn is_null(&self) -> bool {
        match self {
            Item::Int(n) => *n == 0,
            Item::Str(s) => s.is_empty(),
            Item::List(l) => l.is_empty(),
        }
    }
}

/// Qualifiers sort by their position in [`QUALIFIERS`]; an unknown one sorts after all of them, alphabetically.
fn qualifier_key(q: &str) -> String {
    match QUALIFIERS.iter().position(|k| *k == q) {
        Some(i) => i.to_string(),
        None => format!("{}-{q}", QUALIFIERS.len()),
    }
}

/// Maven's `Item.compareTo(item)`, with `None` standing for a missing item (a shorter version).
fn cmp_item(l: &Item, r: Option<&Item>) -> Ordering {
    match (l, r) {
        (Item::Int(n), None) => {
            if *n == 0 {
                Ordering::Equal
            } else {
                Ordering::Greater
            }
        }
        (Item::Int(a), Some(Item::Int(b))) => a.cmp(b),
        (Item::Int(_), Some(_)) => Ordering::Greater,
        (Item::Str(s), None) => qualifier_key(s).cmp(&qualifier_key("")),
        (Item::Str(a), Some(Item::Str(b))) => qualifier_key(a).cmp(&qualifier_key(b)),
        (Item::Str(_), Some(_)) => Ordering::Less,
        (Item::List(l), None) => l
            .first()
            .map_or(Ordering::Equal, |first| cmp_item(first, None)),
        (Item::List(_), Some(Item::Int(_))) => Ordering::Less,
        (Item::List(_), Some(Item::Str(_))) => Ordering::Greater,
        (Item::List(a), Some(Item::List(b))) => {
            for i in 0..a.len().max(b.len()) {
                let ord = match (a.get(i), b.get(i)) {
                    (Some(x), y) => cmp_item(x, y),
                    (None, Some(y)) => cmp_item(y, None).reverse(),
                    (None, None) => Ordering::Equal,
                };
                if ord != Ordering::Equal {
                    return ord;
                }
            }
            Ordering::Equal
        }
    }
}

fn normalize(list: &mut Vec<Item>) {
    while let Some(last) = list.last() {
        if last.is_null() {
            list.pop();
        } else if matches!(last, Item::List(_)) {
            // A non-empty sub-list is kept, but zeros before it may still go.
            let i = list.len() - 1;
            let mut j = i;
            while j > 0 && list[j - 1].is_null() {
                list.remove(j - 1);
                j -= 1;
            }
            return;
        } else {
            return;
        }
    }
}

/// A Maven version, ordered like `ComparableVersion`. Equality follows the order: `1.0 == 1`.
#[derive(Debug, Clone)]
pub struct Version {
    raw: String,
    items: Vec<Item>,
}

impl Version {
    pub fn parse(raw: &str) -> Version {
        let lower = raw.to_lowercase();
        let chars: Vec<char> = lower.chars().collect();
        let sub = |a: usize, b: usize| chars[a..b].iter().collect::<String>();
        let item = |is_digit: bool, s: &str| {
            if is_digit {
                Item::number(s)
            } else {
                Item::qualifier(s, false)
            }
        };

        // `lists[0]` is the root; every `-` or digit/letter change opens a nested list, as in Maven.
        let mut lists: Vec<Vec<Item>> = vec![vec![]];
        let (mut is_digit, mut start) = (false, 0);
        for (i, &c) in chars.iter().enumerate() {
            if c == '.' || c == '-' {
                let cur = lists.last_mut().unwrap();
                cur.push(if i == start {
                    Item::Int(0)
                } else {
                    item(is_digit, &sub(start, i))
                });
                start = i + 1;
                if c == '-' {
                    lists.push(vec![]);
                }
            } else if c.is_ascii_digit() {
                if !is_digit && i > start {
                    if !lists.last().unwrap().is_empty() {
                        lists.push(vec![]);
                    }
                    lists
                        .last_mut()
                        .unwrap()
                        .push(Item::qualifier(&sub(start, i), true));
                    start = i;
                    lists.push(vec![]);
                }
                is_digit = true;
            } else {
                if is_digit && i > start {
                    lists.last_mut().unwrap().push(Item::number(&sub(start, i)));
                    start = i;
                    lists.push(vec![]);
                }
                is_digit = false;
            }
        }
        if chars.len() > start {
            if !is_digit && !lists.last().unwrap().is_empty() {
                lists.push(vec![]);
            }
            lists
                .last_mut()
                .unwrap()
                .push(item(is_digit, &sub(start, chars.len())));
        }

        // Fold the nested lists back, innermost first, trimming each as it closes.
        while lists.len() > 1 {
            let mut inner = lists.pop().unwrap();
            normalize(&mut inner);
            lists.last_mut().unwrap().push(Item::List(inner));
        }
        let mut items = lists.pop().unwrap();
        normalize(&mut items);
        Version {
            raw: raw.to_string(),
            items,
        }
    }

    pub fn as_str(&self) -> &str {
        &self.raw
    }
}

impl Ord for Version {
    fn cmp(&self, other: &Self) -> Ordering {
        cmp_item(
            &Item::List(self.items.clone()),
            Some(&Item::List(other.items.clone())),
        )
    }
}

impl PartialOrd for Version {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl PartialEq for Version {
    fn eq(&self, other: &Self) -> bool {
        self.cmp(other) == Ordering::Equal
    }
}

impl Eq for Version {}

#[derive(Debug, Clone)]
struct Bound {
    version: Version,
    inclusive: bool,
}

#[derive(Debug, Clone)]
struct Interval {
    lower: Option<Bound>,
    upper: Option<Bound>,
}

impl Interval {
    fn contains(&self, v: &Version) -> bool {
        let above = self.lower.as_ref().is_none_or(|b| match v.cmp(&b.version) {
            Ordering::Greater => true,
            Ordering::Equal => b.inclusive,
            Ordering::Less => false,
        });
        let below = self.upper.as_ref().is_none_or(|b| match v.cmp(&b.version) {
            Ordering::Less => true,
            Ordering::Equal => b.inclusive,
            Ordering::Greater => false,
        });
        above && below
    }
}

/// A dependency version written as a range: one or more intervals, e.g. `[1.0,2.0)` or `(,1.0],[1.2,)`.
#[derive(Debug, Clone)]
pub struct Constraint {
    intervals: Vec<Interval>,
}

impl Constraint {
    /// `None` when `spec` is an ordinary (soft) version; an error when it looks like a range but is malformed.
    pub fn parse(spec: &str) -> Result<Option<Constraint>> {
        let mut rest = spec.trim();
        if !rest.starts_with(['[', '(']) {
            return Ok(None);
        }
        let mut intervals = vec![];
        while !rest.is_empty() {
            let (open, Some(end)) = (rest.chars().next().unwrap(), rest.find([']', ')'])) else {
                bail!(
                    "invalid version range '{spec}': every '[' or '(' needs a closing ']' or ')'"
                );
            };
            if !matches!(open, '[' | '(') {
                bail!("invalid version range '{spec}': expected '[' or '(' before '{rest}'");
            }
            let (lower_incl, upper_incl) = (open == '[', rest.as_bytes()[end] == b']');
            let body = &rest[1..end];
            let bound = |s: &str, inclusive| {
                (!s.trim().is_empty()).then(|| Bound {
                    version: Version::parse(s.trim()),
                    inclusive,
                })
            };
            let interval = match body.split_once(',') {
                None => {
                    if !(lower_incl && upper_incl) || body.trim().is_empty() {
                        bail!(
                            "invalid version range '{spec}': a single version must be written [1.0]"
                        );
                    }
                    Interval {
                        lower: bound(body, true),
                        upper: bound(body, true),
                    }
                }
                Some((l, u)) => {
                    if u.contains(',') {
                        bail!("invalid version range '{spec}': an interval has at most two bounds");
                    }
                    let (lower, upper) = (bound(l, lower_incl), bound(u, upper_incl));
                    if let (Some(lo), Some(hi)) = (&lower, &upper) {
                        match lo.version.cmp(&hi.version) {
                            Ordering::Greater => bail!(
                                "invalid version range '{spec}': lower bound {} is above upper bound {}",
                                lo.version.as_str(),
                                hi.version.as_str()
                            ),
                            Ordering::Equal if !(lo.inclusive && hi.inclusive) => {
                                bail!("invalid version range '{spec}': an empty interval")
                            }
                            _ => {}
                        }
                    }
                    Interval { lower, upper }
                }
            };
            intervals.push(interval);
            rest = rest[end + 1..].trim_start();
            if !rest.is_empty() {
                rest = rest
                    .strip_prefix(',')
                    .map(str::trim_start)
                    .filter(|r| !r.is_empty())
                    .ok_or_else(|| {
                        anyhow::anyhow!(
                            "invalid version range '{spec}': intervals are separated by ','"
                        )
                    })?;
            }
        }
        Ok(Some(Constraint { intervals }))
    }

    /// The one version a range such as `[1.2.3]` allows; no repository listing is needed to resolve it.
    pub fn exact(&self) -> Option<&str> {
        match self.intervals.as_slice() {
            [
                Interval {
                    lower: Some(lo),
                    upper: Some(hi),
                },
            ] if lo.inclusive && hi.inclusive && lo.version.as_str() == hi.version.as_str() => {
                Some(lo.version.as_str())
            }
            _ => None,
        }
    }

    pub fn contains(&self, v: &Version) -> bool {
        self.intervals.iter().any(|i| i.contains(v))
    }

    /// The highest of `available` inside the range. `-SNAPSHOT` versions are skipped: a range such as
    /// `[1.0,2.0)` would otherwise take `2.0-SNAPSHOT`, which sorts below `2.0`.
    pub fn best(&self, available: &[String]) -> Option<String> {
        available
            .iter()
            .filter(|v| !v.ends_with("-SNAPSHOT"))
            .map(|v| Version::parse(v))
            .filter(|v| self.contains(v))
            .max()
            .map(|v| v.as_str().to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(s: &str) -> Version {
        Version::parse(s)
    }

    fn ascending(chain: &[&str]) {
        for w in chain.windows(2) {
            assert!(v(w[0]) < v(w[1]), "expected {} < {}", w[0], w[1]);
            assert!(v(w[1]) > v(w[0]), "expected {} > {}", w[1], w[0]);
        }
    }

    fn equal(group: &[&str]) {
        for w in group.windows(2) {
            assert_eq!(v(w[0]), v(w[1]), "expected {} == {}", w[0], w[1]);
        }
    }

    #[test]
    fn numbers_compare_numerically_not_as_text() {
        ascending(&["1", "1.1", "1.2", "1.10", "2", "10"]);
    }

    #[test]
    fn trailing_zeros_and_release_aliases_are_ignored() {
        equal(&[
            "1",
            "1.0",
            "1.0.0",
            "1-ga",
            "1.0-final",
            "1-release",
            "1.0-0",
        ]);
    }

    #[test]
    fn qualifiers_order_as_in_the_maven_specification() {
        ascending(&[
            "1.0-alpha",
            "1.0-beta",
            "1.0-milestone",
            "1.0-rc",
            "1.0-snapshot",
            "1.0",
            "1.0-sp",
            "1.0.1",
        ]);
    }

    #[test]
    fn single_letter_qualifiers_before_a_digit_are_alpha_beta_milestone() {
        equal(&["1-a1", "1-alpha-1"]);
        equal(&["1-b2", "1-beta-2"]);
        equal(&["1-m3", "1-milestone-3"]);
        equal(&["1-cr", "1-rc"]);
        ascending(&["1-a1", "1-a2", "1-b1"]);
    }

    #[test]
    fn unknown_qualifiers_sort_after_the_known_ones_alphabetically() {
        ascending(&["1.0", "1.0-sp", "1.0-bar", "1.0-foo"]);
    }

    #[test]
    fn case_does_not_matter() {
        equal(&["1.0-ALPHA", "1.0-alpha"]);
        equal(&["1.0.Final", "1.0"]);
    }

    #[test]
    fn a_hyphenated_number_is_a_later_build_of_the_same_version() {
        ascending(&["1.0", "1.0-1", "1.0-2", "1.0.1"]);
    }

    #[test]
    fn real_world_versions_order_sensibly() {
        ascending(&[
            "2.9.10",
            "2.9.10.8",
            "2.10.0",
            "2.13.4.2",
            "2.14.0-rc1",
            "2.14.0",
        ]);
        ascending(&["10.0.0", "10.178.1", "10.999.0", "11.0.0"]);
        ascending(&["33.0.0-jre", "33.1.0-jre"]);
    }

    #[test]
    fn a_soft_version_is_not_a_range() {
        assert!(Constraint::parse("1.2.3").unwrap().is_none());
        assert!(Constraint::parse("").unwrap().is_none());
    }

    fn pick(range: &str, available: &[&str]) -> Option<String> {
        let available: Vec<String> = available.iter().map(|s| s.to_string()).collect();
        Constraint::parse(range).unwrap().unwrap().best(&available)
    }

    const LISTED: [&str; 7] = ["0.9", "1.0", "1.5", "1.10", "2.0", "2.1", "3.0"];

    #[test]
    fn half_open_range_excludes_the_upper_bound() {
        assert_eq!(pick("[1.0,2.0)", &LISTED).as_deref(), Some("1.10"));
        assert_eq!(pick("[1.0,2.0]", &LISTED).as_deref(), Some("2.0"));
        assert_eq!(pick("(1.0,2.0)", &LISTED).as_deref(), Some("1.10"));
        assert_eq!(pick("(1.10,2.0)", &LISTED), None);
    }

    #[test]
    fn open_ended_ranges() {
        assert_eq!(pick("[1.5,)", &LISTED).as_deref(), Some("3.0"));
        assert_eq!(pick("(,1.5]", &LISTED).as_deref(), Some("1.5"));
        assert_eq!(pick("(,1.5)", &LISTED).as_deref(), Some("1.0"));
    }

    #[test]
    fn an_exact_range_names_its_version() {
        assert_eq!(
            Constraint::parse("[1.2.3]").unwrap().unwrap().exact(),
            Some("1.2.3")
        );
        assert_eq!(
            Constraint::parse("[1.2,1.3]").unwrap().unwrap().exact(),
            None
        );
        assert_eq!(
            Constraint::parse("[1.2],[1.3]").unwrap().unwrap().exact(),
            None
        );
    }

    #[test]
    fn a_single_bracketed_version_pins_it() {
        assert_eq!(pick("[1.5]", &LISTED).as_deref(), Some("1.5"));
        assert_eq!(pick("[1.6]", &LISTED), None);
        assert_eq!(pick("[1.0]", &["1.0.0"]).as_deref(), Some("1.0.0"));
    }

    #[test]
    fn a_union_of_intervals_takes_the_highest_match() {
        assert_eq!(pick("(,1.0],[2.0,2.1)", &LISTED).as_deref(), Some("2.0"));
        assert_eq!(pick("(,1.0], [1.5,1.6]", &LISTED).as_deref(), Some("1.5"));
    }

    #[test]
    fn snapshots_are_not_picked() {
        assert_eq!(
            pick("[1.0,2.0)", &["1.5", "1.9-SNAPSHOT", "2.0-SNAPSHOT"]).as_deref(),
            Some("1.5")
        );
    }

    #[test]
    fn pre_releases_inside_the_range_are_eligible_but_lose_to_the_release() {
        assert_eq!(
            pick("[1.0,2.0)", &["1.5", "1.6-rc1"]).as_deref(),
            Some("1.6-rc1")
        );
        assert_eq!(
            pick("[1.0,2.0)", &["1.6", "1.6-rc1"]).as_deref(),
            Some("1.6")
        );
        assert_eq!(
            pick("[1.0,2.0)", &["2.0-rc1", "1.5"]).as_deref(),
            Some("2.0-rc1")
        );
    }

    #[test]
    fn malformed_ranges_are_rejected_with_the_range_in_the_message() {
        for bad in [
            "[1.0,2.0",
            "[1.0",
            "(1.0)",
            "[1.0,2.0],",
            "[2.0,1.0]",
            "[1.0,1.0)",
            "[1,2,3]",
            "[]",
            "[1.0,2.0] x",
        ] {
            let e = Constraint::parse(bad).unwrap_err().to_string();
            assert!(e.contains(bad), "{bad}: {e}");
        }
    }
}
