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

#[derive(Default, Debug, PartialEq)]
struct State {
    misses: HashMap<String, u64>,
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

fn clean(s: &str) -> String {
    s.replace(['\t', '\n', '\r'], " ")
}

fn parse(text: &str, now: u64, miss_ttl: u64, use_misses: bool) -> State {
    let mut st = State::default();
    for line in text.lines() {
        st.lines += 1;
        let f: Vec<&str> = line.split('\t').collect();
        match f.as_slice() {
            ["miss", ts, repo, auth, path] if use_misses => {
                if let Ok(ts) = ts.parse::<u64>() {
                    if now.saturating_sub(ts) < miss_ttl {
                        st.misses.insert(format!("{repo}\t{auth}\t{path}"), ts);
                    }
                }
            }
            ["snap", path, repo, value, checked] => {
                if let Ok(checked) = checked.parse() {
                    st.snaps.insert(path.to_string(), SnapshotStamp { repo: repo.to_string(), value: value.to_string(), checked });
                }
            }
            ["from", path, repo] => {
                st.origins.insert(path.to_string(), repo.to_string());
            }
            ["val", path, etag, last_modified] => {
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
    let mut out = String::new();
    for (k, ts) in &st.misses {
        out.push_str(&format!("miss\t{ts}\t{k}\n"));
    }
    for (p, s) in &st.snaps {
        out.push_str(&format!("snap\t{p}\t{}\t{}\t{}\n", s.repo, s.value, s.checked));
    }
    for (p, r) in &st.origins {
        out.push_str(&format!("from\t{p}\t{r}\n"));
    }
    for (p, v) in &st.vals {
        out.push_str(&format!("val\t{p}\t{}\t{}\n", v.etag.as_deref().unwrap_or(""), v.last_modified.as_deref().unwrap_or("")));
    }
    out
}

impl Index {
    /// `use_misses = false` ignores recorded 404s (still records new ones). Rewrites the file when most of it is stale.
    pub fn load(cache: &Path, now: u64, miss_ttl: u64, use_misses: bool) -> Self {
        let path = cache.join(".rayvn-index.tsv");
        let text = std::fs::read_to_string(&path).unwrap_or_default();
        let state = parse(&text, now, miss_ttl, use_misses);
        let live = state.misses.len() + state.snaps.len() + state.vals.len() + state.origins.len();
        let index = Index { path, miss_ttl, state: Mutex::new(state) };
        if text.lines().count() > 2 * live + 100 && use_misses {
            index.compact();
        }
        index
    }

    fn compact(&self) {
        let st = self.state.lock().unwrap();
        let tmp = self.path.with_extension(format!("tmp-{}", std::process::id()));
        if std::fs::write(&tmp, format_state(&st)).is_ok() {
            let _ = std::fs::rename(&tmp, &self.path);
        }
    }

    fn append(&self, line: &str) {
        if let Some(dir) = self.path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(&self.path) {
            let _ = writeln!(f, "{line}");
        }
    }

    pub fn has_miss(&self, key: &str) -> bool {
        self.state.lock().unwrap().misses.contains_key(key)
    }

    /// `key` is `<repo>\t<auth|anon>\t<path>`.
    pub fn add_miss(&self, key: &str, now: u64) {
        if self.miss_ttl == 0 || self.state.lock().unwrap().misses.insert(key.to_string(), now).is_some() {
            return;
        }
        let fields: Vec<String> = key.split('\t').map(clean).collect();
        self.append(&format!("miss\t{now}\t{}", fields.join("\t")));
    }

    pub fn snapshot(&self, path: &str) -> Option<SnapshotStamp> {
        self.state.lock().unwrap().snaps.get(path).cloned()
    }

    pub fn set_snapshot(&self, path: &str, s: SnapshotStamp) {
        self.append(&format!("snap\t{}\t{}\t{}\t{}", clean(path), clean(&s.repo), clean(&s.value), s.checked));
        self.state.lock().unwrap().snaps.insert(path.to_string(), s);
    }

    /// The repository `path` was downloaded from, if rayvn did the download.
    pub fn origin(&self, path: &str) -> Option<String> {
        self.state.lock().unwrap().origins.get(path).cloned()
    }

    pub fn set_origin(&self, path: &str, repo: &str) {
        if self.state.lock().unwrap().origins.insert(path.to_string(), repo.to_string()).as_deref() != Some(repo) {
            self.append(&format!("from\t{}\t{}", clean(path), clean(repo)));
        }
    }

    pub fn validators(&self, path: &str) -> Option<Validators> {
        self.state.lock().unwrap().vals.get(path).cloned()
    }

    pub fn set_validators(&self, path: &str, v: Validators) {
        self.append(&format!("val\t{}\t{}\t{}", clean(path), clean(v.etag.as_deref().unwrap_or("")), clean(v.last_modified.as_deref().unwrap_or(""))));
        self.state.lock().unwrap().vals.insert(path.to_string(), v);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stamp() -> SnapshotStamp {
        SnapshotStamp { repo: "r".into(), value: "1.0-20240101.000000-3".into(), checked: 42 }
    }

    #[test]
    fn expired_misses_are_dropped_and_later_lines_win() {
        let text = "miss\t100\tcentral\tanon\ta/b\nmiss\t990\tcentral\tanon\tc/d\nmiss\t100\tr\tanon\tp\nmiss\t990\tr\tanon\tp\n";
        let st = parse(text, 1000, 50, true);
        let mut keys: Vec<_> = st.misses.iter().map(|(k, v)| (k.as_str(), *v)).collect();
        keys.sort();
        assert_eq!(keys, [("central\tanon\tc/d", 990), ("r\tanon\tp", 990)]);
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
        st.misses.insert("r\tauth\tp".into(), 7);
        st.snaps.insert("a/x-1-SNAPSHOT.jar".into(), stamp());
        st.vals.insert("a/maven-metadata-r.xml".into(), Validators { etag: Some("\"e\"".into()), last_modified: None });
        st.origins.insert("a/x-1.jar".into(), "r".into());
        let again = parse(&format_state(&st), 8, 100, true);
        assert_eq!((again.misses, again.snaps, again.vals, again.origins), (st.misses, st.snaps, st.vals, st.origins));
    }

    #[test]
    fn empty_validators_and_garbage_lines_are_ignored() {
        let st = parse("val\tp\t\t\nnonsense\nsnap\tp\tr\tv\tnotanumber\n", 0, 1, true);
        assert!(st.vals.is_empty() && st.snaps.is_empty());
    }

    #[test]
    fn index_persists_between_loads() {
        let dir = std::env::temp_dir().join(format!("rvn-index-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let _ = std::fs::remove_file(dir.join(".rayvn-index.tsv"));
        let i = Index::load(&dir, 100, 50, true);
        i.add_miss("r\tanon\tp", 100);
        i.set_snapshot("s.jar", stamp());
        i.set_validators("m.xml", Validators { etag: Some("e".into()), last_modified: Some("lm".into()) });
        i.set_origin("a/x-1.jar", "r");
        i.set_origin("a/x-1.jar", "r");
        let j = Index::load(&dir, 120, 50, true);
        assert!(j.has_miss("r\tanon\tp"));
        assert_eq!(j.origin("a/x-1.jar").as_deref(), Some("r"));
        assert_eq!(std::fs::read_to_string(dir.join(".rayvn-index.tsv")).unwrap().matches("from\t").count(), 1, "unchanged origin is not re-appended");
        assert_eq!(j.snapshot("s.jar"), Some(stamp()));
        assert_eq!(j.validators("m.xml").unwrap().last_modified.as_deref(), Some("lm"));
        assert!(!Index::load(&dir, 200, 50, true).has_miss("r\tanon\tp"));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
