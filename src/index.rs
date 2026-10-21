//! One append-only bookkeeping file per cache directory (`<cache>/.rayvn-index.tsv`).
//!
//! Line formats (tab separated, later lines win):
//!
//! ```text
//! miss  <unix secs>  <repo>  <auth|anon>  <path>
//! snap  <local path> <repo>  <timestamped version>  <checked unix secs>
//! val   <metadata path>  <etag>  <last-modified>
//! from  <artifact path>  <repo>        (repository an artifact was downloaded from)
//! ```
//!
//! Paths are relative to the cache directory.

use std::collections::HashMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

/// HTTP cache validators of a downloaded file.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Validators {
    pub etag: Option<String>,
    pub last_modified: Option<String>,
}

/// Which remote file a local `-SNAPSHOT` file was copied from, and when rayvn last checked it.
#[derive(Debug, Clone, PartialEq)]
pub struct SnapshotStamp {
    pub repo: String,
    pub value: String,
    pub checked: u64,
}

/// Whether a request carried credentials. Private repositories answer 404 to anonymous ones, so a recorded miss
/// only holds for the same state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Auth {
    Authenticated,
    Anonymous,
}

impl Auth {
    fn as_str(self) -> &'static str {
        match self {
            Auth::Authenticated => "auth",
            Auth::Anonymous => "anon",
        }
    }

    fn parse(s: &str) -> Option<Auth> {
        [Auth::Authenticated, Auth::Anonymous]
            .into_iter()
            .find(|a| a.as_str() == s)
    }
}

/// A file a repository answered 404 for.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct MissKey {
    pub repo: String,
    pub auth: Auth,
    /// Repository-relative path, e.g. `org/x/y/1.0/y-1.0.jar`.
    pub path: String,
}

impl MissKey {
    pub fn new(repo: &str, auth: Auth, path: &str) -> Self {
        MissKey {
            repo: repo.to_string(),
            auth,
            path: path.to_string(),
        }
    }
}

#[derive(Default, Debug, PartialEq)]
struct State {
    misses: HashMap<MissKey, u64>,
    snaps: HashMap<String, SnapshotStamp>,
    vals: HashMap<String, Validators>,
    origins: HashMap<String, String>,
    lines: usize,
}

pub struct Index {
    path: PathBuf,
    miss_ttl: u64,
    state: Mutex<State>,
}

/// The first field of an index line: what kind of record it is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Tag {
    Miss,
    Snap,
    From,
    Val,
}

impl Tag {
    fn as_str(self) -> &'static str {
        match self {
            Tag::Miss => "miss",
            Tag::Snap => "snap",
            Tag::From => "from",
            Tag::Val => "val",
        }
    }

    /// `None` for a tag this version does not know, so lines from a newer rayvn are skipped.
    fn parse(s: &str) -> Option<Tag> {
        [Tag::Miss, Tag::Snap, Tag::From, Tag::Val]
            .into_iter()
            .find(|t| t.as_str() == s)
    }

    /// A line of this kind: the tag, then the fields, tab separated.
    fn line(self, fields: &[&str]) -> String {
        std::iter::once(self.as_str())
            .chain(fields.iter().copied())
            .collect::<Vec<_>>()
            .join("\t")
    }
}

fn clean(s: &str) -> String {
    s.replace(['\t', '\n', '\r'], " ")
}

fn miss_line(k: &MissKey, ts: u64) -> String {
    Tag::Miss.line(&[
        &ts.to_string(),
        &clean(&k.repo),
        k.auth.as_str(),
        &clean(&k.path),
    ])
}

fn parse(text: &str, now: u64, miss_ttl: u64, use_misses: bool) -> State {
    let mut st = State::default();
    for line in text.lines() {
        st.lines += 1;
        let f: Vec<&str> = line.split('\t').collect();
        let Some(tag) = f.first().and_then(|t| Tag::parse(t)) else {
            continue;
        };
        match (tag, &f[1..]) {
            (Tag::Miss, [ts, repo, auth, path]) if use_misses => {
                if let (Ok(ts), Some(auth)) = (ts.parse::<u64>(), Auth::parse(auth))
                    && now.saturating_sub(ts) < miss_ttl
                {
                    st.misses.insert(MissKey::new(repo, auth, path), ts);
                }
            }
            (Tag::Snap, [path, repo, value, checked]) => {
                if let Ok(checked) = checked.parse() {
                    st.snaps.insert(
                        path.to_string(),
                        SnapshotStamp {
                            repo: repo.to_string(),
                            value: value.to_string(),
                            checked,
                        },
                    );
                }
            }
            (Tag::From, [path, repo]) => {
                st.origins.insert(path.to_string(), repo.to_string());
            }
            (Tag::Val, [path, etag, last_modified]) => {
                let v = Validators {
                    etag: Some(etag.to_string()).filter(|s| !s.is_empty()),
                    last_modified: Some(last_modified.to_string()).filter(|s| !s.is_empty()),
                };
                if v == Validators::default() {
                    st.vals.remove(*path);
                } else {
                    st.vals.insert(path.to_string(), v);
                }
            }
            _ => {}
        }
    }
    st
}

fn format_state(st: &State) -> String {
    let mut lines = vec![];
    for (k, ts) in &st.misses {
        lines.push(miss_line(k, *ts));
    }
    for (p, s) in &st.snaps {
        lines.push(Tag::Snap.line(&[p, &s.repo, &s.value, &s.checked.to_string()]));
    }
    for (p, r) in &st.origins {
        lines.push(Tag::From.line(&[p, r]));
    }
    for (p, v) in &st.vals {
        lines.push(Tag::Val.line(&[
            p,
            v.etag.as_deref().unwrap_or(""),
            v.last_modified.as_deref().unwrap_or(""),
        ]));
    }
    lines.iter().map(|l| format!("{l}\n")).collect()
}

impl Index {
    /// `use_misses = false` ignores recorded 404s (still records new ones). Rewrites the file when most of it is stale.
    pub fn load(cache: &Path, now: u64, miss_ttl: u64, use_misses: bool) -> Self {
        let path = cache.join(".rayvn-index.tsv");
        let text = std::fs::read_to_string(&path).unwrap_or_default();
        let state = parse(&text, now, miss_ttl, use_misses);
        let live = state.misses.len() + state.snaps.len() + state.vals.len() + state.origins.len();
        let index = Index {
            path,
            miss_ttl,
            state: Mutex::new(state),
        };
        if text.lines().count() > 2 * live + 100 && use_misses {
            index.compact();
        }
        index
    }

    fn compact(&self) {
        let st = self.state.lock().unwrap();
        let tmp = self
            .path
            .with_extension(format!("tmp-{}", std::process::id()));
        if std::fs::write(&tmp, format_state(&st)).is_ok() {
            let _ = std::fs::rename(&tmp, &self.path);
        }
    }

    fn append(&self, line: &str) {
        if let Some(dir) = self.path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        if let Ok(mut f) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)
        {
            let _ = writeln!(f, "{line}");
        }
    }

    pub fn has_miss(&self, key: &MissKey) -> bool {
        self.state.lock().unwrap().misses.contains_key(key)
    }

    pub fn add_miss(&self, key: &MissKey, now: u64) {
        if self.miss_ttl == 0
            || self
                .state
                .lock()
                .unwrap()
                .misses
                .insert(key.clone(), now)
                .is_some()
        {
            return;
        }
        self.append(&miss_line(key, now));
    }

    pub fn snapshot(&self, path: &str) -> Option<SnapshotStamp> {
        self.state.lock().unwrap().snaps.get(path).cloned()
    }

    pub fn set_snapshot(&self, path: &str, s: SnapshotStamp) {
        self.append(&Tag::Snap.line(&[
            &clean(path),
            &clean(&s.repo),
            &clean(&s.value),
            &s.checked.to_string(),
        ]));
        self.state.lock().unwrap().snaps.insert(path.to_string(), s);
    }

    /// The repository `path` was downloaded from, if rayvn did the download.
    pub fn origin(&self, path: &str) -> Option<String> {
        self.state.lock().unwrap().origins.get(path).cloned()
    }

    pub fn set_origin(&self, path: &str, repo: &str) {
        if self
            .state
            .lock()
            .unwrap()
            .origins
            .insert(path.to_string(), repo.to_string())
            .as_deref()
            != Some(repo)
        {
            self.append(&Tag::From.line(&[&clean(path), &clean(repo)]));
        }
    }

    pub fn validators(&self, path: &str) -> Option<Validators> {
        self.state.lock().unwrap().vals.get(path).cloned()
    }

    pub fn set_validators(&self, path: &str, v: Validators) {
        self.append(&Tag::Val.line(&[
            &clean(path),
            &clean(v.etag.as_deref().unwrap_or("")),
            &clean(v.last_modified.as_deref().unwrap_or("")),
        ]));
        self.state.lock().unwrap().vals.insert(path.to_string(), v);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stamp() -> SnapshotStamp {
        SnapshotStamp {
            repo: "r".into(),
            value: "1.0-20240101.000000-3".into(),
            checked: 42,
        }
    }

    fn miss(repo: &str, path: &str) -> MissKey {
        MissKey::new(repo, Auth::Anonymous, path)
    }

    #[test]
    fn a_miss_only_holds_for_the_auth_state_it_was_recorded_in() {
        let mut st = State::default();
        st.misses.insert(miss("r", "p"), 7);
        let again = parse(&format_state(&st), 8, 100, true);
        assert!(again.misses.contains_key(&miss("r", "p")));
        assert!(
            !again
                .misses
                .contains_key(&MissKey::new("r", Auth::Authenticated, "p"))
        );
    }

    #[test]
    fn a_miss_line_with_an_unknown_auth_state_is_skipped() {
        let st = parse("miss\t990\tr\tweird\tp\n", 1000, 50, true);
        assert!(st.misses.is_empty());
    }

    #[test]
    fn every_tag_round_trips_and_unknown_ones_are_skipped() {
        for t in [Tag::Miss, Tag::Snap, Tag::From, Tag::Val] {
            assert_eq!(Tag::parse(t.as_str()), Some(t));
        }
        assert_eq!(Tag::parse("future"), None);
        assert_eq!(Tag::From.line(&["a/b", "central"]), "from\ta/b\tcentral");
        assert_eq!(parse("future\tx\ny\n", 0, 1, true).lines, 2);
    }

    #[test]
    fn expired_misses_are_dropped_and_later_lines_win() {
        let text = "miss\t100\tcentral\tanon\ta/b\nmiss\t990\tcentral\tanon\tc/d\nmiss\t100\tr\tanon\tp\nmiss\t990\tr\tanon\tp\n";
        let st = parse(text, 1000, 50, true);
        let mut keys: Vec<_> = st.misses.iter().map(|(k, v)| (k.clone(), *v)).collect();
        keys.sort_by_key(|(k, _)| k.path.clone());
        assert_eq!(keys, [(miss("central", "c/d"), 990), (miss("r", "p"), 990)]);
    }

    #[test]
    fn misses_can_be_ignored_without_losing_other_records() {
        let text = "miss\t990\tr\tanon\tp\nsnap\tx.jar\tr\tv\t5\n";
        let st = parse(text, 1000, 50, false);
        assert!(st.misses.is_empty());
        assert_eq!(st.snaps["x.jar"].checked, 5);
    }

    #[test]
    fn state_round_trips_through_the_file_format() {
        let mut st = State::default();
        st.misses
            .insert(MissKey::new("r", Auth::Authenticated, "p"), 7);
        st.snaps.insert("a/x-1-SNAPSHOT.jar".into(), stamp());
        st.vals.insert(
            "a/maven-metadata-r.xml".into(),
            Validators {
                etag: Some("\"e\"".into()),
                last_modified: None,
            },
        );
        st.origins.insert("a/x-1.jar".into(), "r".into());
        let again = parse(&format_state(&st), 8, 100, true);
        assert_eq!(
            (again.misses, again.snaps, again.vals, again.origins),
            (st.misses, st.snaps, st.vals, st.origins)
        );
    }

    #[test]
    fn empty_validators_and_garbage_lines_are_ignored() {
        let st = parse(
            "val\tp\t\t\nnonsense\nsnap\tp\tr\tv\tnotanumber\n",
            0,
            1,
            true,
        );
        assert!(st.vals.is_empty() && st.snaps.is_empty());
    }

    #[test]
    fn index_persists_between_loads() {
        let dir = std::env::temp_dir().join(format!("rvn-index-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let _ = std::fs::remove_file(dir.join(".rayvn-index.tsv"));
        let i = Index::load(&dir, 100, 50, true);
        i.add_miss(&miss("r", "p"), 100);
        i.set_snapshot("s.jar", stamp());
        i.set_validators(
            "m.xml",
            Validators {
                etag: Some("e".into()),
                last_modified: Some("lm".into()),
            },
        );
        i.set_origin("a/x-1.jar", "r");
        i.set_origin("a/x-1.jar", "r");
        let j = Index::load(&dir, 120, 50, true);
        assert!(j.has_miss(&miss("r", "p")));
        assert_eq!(j.origin("a/x-1.jar").as_deref(), Some("r"));
        assert_eq!(
            std::fs::read_to_string(dir.join(".rayvn-index.tsv"))
                .unwrap()
                .matches("from\t")
                .count(),
            1,
            "unchanged origin is not re-appended"
        );
        assert_eq!(j.snapshot("s.jar"), Some(stamp()));
        assert_eq!(
            j.validators("m.xml").unwrap().last_modified.as_deref(),
            Some("lm")
        );
        assert!(!Index::load(&dir, 200, 50, true).has_miss(&miss("r", "p")));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
