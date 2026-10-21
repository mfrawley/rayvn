//! The order repositories are asked in: declared order, mirrors, and hints that move likely sources first.

use crate::pom::RepoDecl;

pub(super) const CENTRAL_ID: &str = "central";
pub(super) const CENTRAL_URL: &str = "https://repo1.maven.org/maven2";

#[derive(Debug, Clone)]
pub struct Remote {
    pub id: String,
    pub url: String,
}

impl Remote {
    /// Maven Central, whatever a POM or mirror calls it.
    fn is_central(&self) -> bool {
        self.id == CENTRAL_ID || self.url.trim_end_matches('/') == CENTRAL_URL
    }
}

/// Remotes in their original order, except that those named by a matching hint come first, most specific hint first
/// (earlier hints win ties, so user hints beat shipped ones), then Central if `central_second`.
pub(super) fn hinted_order(
    remotes: &[Remote],
    hints: &[crate::settings::Hint],
    group: &str,
    central_second: bool,
) -> Vec<Remote> {
    let mut matched: Vec<(usize, usize)> = hints
        .iter()
        .enumerate()
        .filter_map(|(i, h)| h.specificity(group).map(|s| (s, i)))
        .collect();
    matched.sort_by_key(|&(score, i)| (std::cmp::Reverse(score), i));
    let mut out: Vec<Remote> = vec![];
    for (_, i) in matched {
        for r in remotes.iter().filter(|r| hints[i].names(&r.id, &r.url)) {
            if !out.iter().any(|o| o.id == r.id) {
                out.push(r.clone());
            }
        }
    }
    if central_second
        && let Some(c) = remotes.iter().find(|r| r.is_central())
        && !out.iter().any(|o| o.id == c.id)
    {
        out.push(c.clone());
    }
    out.extend(
        remotes
            .iter()
            .filter(|r| !out.iter().any(|o| o.id == r.id))
            .cloned()
            .collect::<Vec<_>>(),
    );
    out
}

pub(super) fn apply_mirror(mirrors: &[crate::settings::Mirror], r: RepoDecl) -> (String, String) {
    let (id, url) = match mirrors.iter().find(|m| m.matches(&r.id, &r.url)) {
        Some(m) => (m.id.clone(), m.url.clone()),
        None => (r.id, r.url),
    };
    (id, url.trim_end_matches('/').to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::settings::Hint;

    fn remote(id: &str) -> Remote {
        Remote {
            id: id.into(),
            url: format!("https://{id}.example/repo"),
        }
    }

    #[test]
    fn central_is_recognised_by_id_or_by_url() {
        assert!(remote("central").is_central());
        let by_url = Remote {
            id: "mirror".into(),
            url: format!("{CENTRAL_URL}/"),
        };
        assert!(by_url.is_central());
        assert!(!remote("a").is_central());
    }

    fn hint(repo: &str, groups: &[&str]) -> Hint {
        Hint {
            repo: Some(repo.into()),
            url: None,
            groups: groups.iter().map(|g| g.to_string()).collect(),
        }
    }

    fn ids(v: Vec<Remote>) -> Vec<String> {
        v.into_iter().map(|r| r.id).collect()
    }

    #[test]
    fn unmatched_group_keeps_original_order() {
        let remotes = [remote("a"), remote("b"), remote("central")];
        assert_eq!(
            ids(hinted_order(
                &remotes,
                &[hint("b", &["org.x.*"])],
                "com.y",
                false
            )),
            ["a", "b", "central"]
        );
    }

    #[test]
    fn matching_hint_moves_repo_to_front() {
        let remotes = [remote("a"), remote("b"), remote("central")];
        assert_eq!(
            ids(hinted_order(
                &remotes,
                &[hint("b", &["org.x.*"])],
                "org.x.sub",
                false
            )),
            ["b", "a", "central"]
        );
    }

    #[test]
    fn more_specific_hint_goes_first_and_catch_all_follows() {
        let remotes = [remote("a"), remote("b"), remote("central")];
        let hints = [hint("central", &["*"]), hint("b", &["org.x.*"])];
        assert_eq!(
            ids(hinted_order(&remotes, &hints, "org.x", false)),
            ["b", "central", "a"]
        );
        assert_eq!(
            ids(hinted_order(&remotes, &hints, "other", false)),
            ["central", "a", "b"]
        );
    }

    #[test]
    fn earlier_hint_wins_a_tie() {
        let remotes = [remote("a"), remote("b")];
        let hints = [hint("b", &["org.x.*"]), hint("a", &["org.x.*"])];
        assert_eq!(
            ids(hinted_order(&remotes, &hints, "org.x", false)),
            ["b", "a"]
        );
    }

    #[test]
    fn url_hint_ignores_unknown_repositories_and_trailing_slash() {
        let remotes = [remote("a"), remote("b")];
        let by_url = |u: &str| Hint {
            repo: None,
            url: Some(u.into()),
            groups: vec!["org.x.*".into()],
        };
        assert_eq!(
            ids(hinted_order(
                &remotes,
                &[by_url("https://b.example/repo/")],
                "org.x",
                false
            )),
            ["b", "a"]
        );
        assert_eq!(
            ids(hinted_order(
                &remotes,
                &[by_url("https://nowhere.example")],
                "org.x",
                false
            )),
            ["a", "b"]
        );
    }

    #[test]
    fn central_second_follows_hints_and_precedes_other_repos() {
        let remotes = [remote("a"), remote("b"), remote("central")];
        assert_eq!(
            ids(hinted_order(
                &remotes,
                &[hint("b", &["org.x.*"])],
                "org.x",
                true
            )),
            ["b", "central", "a"]
        );
        assert_eq!(
            ids(hinted_order(&remotes, &[], "org.y", true)),
            ["central", "a", "b"]
        );
    }

    #[test]
    fn shipped_hint_list_parses() {
        assert!(!crate::settings::default_hints().is_empty());
    }

    #[test]
    fn sibling_prefix_does_not_match() {
        assert_eq!(
            hint("a", &["org.example.*"]).specificity("org.examplefoo"),
            None
        );
    }
}
