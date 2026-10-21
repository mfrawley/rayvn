//! Test result cache: skip a test run whose inputs are unchanged and which passed last time.
//!
//! The key is a hash of everything that can change a test's outcome *and that rayvn can see*: the compiled
//! main and test classes (resources included), the SHA-256 of every locked jar, the JDK, the JVM arguments, the
//! selected classes and any files the project declares in `rayvn.toml` (`[test] inputs`). Unlike Bazel there
//! is no sandbox, so a test that reads an undeclared file can be served a stale pass; that is why integration
//! tests are cached only on request. Failed runs are never stored.
//!
//! Everything here is pure or takes the work to do as a closure, so the policy can be tested without a JVM.

use crate::lock::hash_bytes;
use anyhow::{Context, Result};
use rayon::prelude::*;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// Everything that goes into a cache key. Order of the vectors does not matter.
#[derive(Debug, Default, Clone)]
pub struct KeyInputs {
    pub jdk: String,
    pub jvm_args: Vec<String>,
    pub classes: Vec<String>,
    /// `(coordinates, sha256)` of each locked jar.
    pub jars: Vec<(String, String)>,
    /// `(path relative to the hashed root, content hash)` of class files, resources and declared inputs.
    pub files: Vec<(String, String)>,
}

/// Deterministic hash of `inputs`: the same inputs in any order give the same key, any change gives another.
pub fn key(inputs: &KeyInputs) -> String {
    let mut lines = vec![format!("jdk={}", inputs.jdk)];
    lines.extend(inputs.jvm_args.iter().map(|a| format!("arg={a}")));
    lines.extend(inputs.classes.iter().map(|c| format!("class={c}")));
    lines.extend(inputs.jars.iter().map(|(g, h)| format!("jar={g} {h}")));
    lines.extend(inputs.files.iter().map(|(p, h)| format!("file={p} {h}")));
    // JVM arguments are ordered, everything else is a set.
    let (args, mut rest): (Vec<_>, Vec<_>) = lines.into_iter().partition(|l| l.starts_with("arg="));
    rest.sort();
    rest.extend(args);
    hash_bytes(rest.join("\n").as_bytes())
}

/// `(relative path, content hash)` of every file below `dir`, in parallel. A missing directory has no files.
pub fn hash_tree(dir: &Path) -> Result<Vec<(String, String)>> {
    let mut files = vec![];
    crate::build::walk(dir, &mut files)?;
    files
        .par_iter()
        .map(|f| {
            let rel = f.strip_prefix(dir)?.to_string_lossy().replace('\\', "/");
            Ok((
                rel,
                hash_bytes(&std::fs::read(f).with_context(|| f.display().to_string())?),
            ))
        })
        .collect()
}

/// Files below `root` (outside `target` and `.git`) that match one of the ant-style `globs`, hashed.
pub fn hash_declared(root: &Path, globs: &[String]) -> Result<Vec<(String, String)>> {
    if globs.is_empty() {
        return Ok(vec![]);
    }
    let mut hashed = hash_tree_skipping(root, &["target", ".git"])?;
    hashed.retain(|(rel, _)| globs.iter().any(|g| crate::build::glob(g, rel)));
    Ok(hashed)
}

fn hash_tree_skipping(root: &Path, skip: &[&str]) -> Result<Vec<(String, String)>> {
    let mut files = vec![];
    crate::build::walk_filtered(
        root,
        &|p| p.file_name().is_some_and(|n| skip.iter().any(|s| n == *s)),
        &mut files,
    )?;
    files
        .par_iter()
        .map(|f| {
            Ok((
                f.strip_prefix(root)?.to_string_lossy().replace('\\', "/"),
                hash_bytes(&std::fs::read(f)?),
            ))
        })
        .collect()
}

/// Test counts from the JUnit Console Launcher's summary table.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Summary {
    pub found: usize,
    pub successful: usize,
    pub failed: usize,
}

impl std::ops::Add for Summary {
    type Output = Summary;

    fn add(self, o: Summary) -> Summary {
        Summary {
            found: self.found + o.found,
            successful: self.successful + o.successful,
            failed: self.failed + o.failed,
        }
    }
}

/// Reads `[   15 tests found   ]`-style lines out of the launcher's output; `None` if there is no summary.
pub fn parse_summary(output: &str) -> Option<Summary> {
    let count = |label: &str| -> Option<usize> {
        let line = output
            .lines()
            .find(|l| l.trim_start().starts_with('[') && l.contains(label))?;
        line.trim_start_matches(|c: char| c == '[' || c.is_whitespace())
            .split_whitespace()
            .next()?
            .parse()
            .ok()
    };
    Some(Summary {
        found: count("tests found")?,
        successful: count("tests successful")?,
        failed: count("tests failed")?,
    })
}

/// Splits `classes` into at most `n` shards, dealing them out in sorted order so that shards are stable
/// from run to run. Empty shards are dropped.
pub fn split_shards(classes: &[String], n: usize) -> Vec<Vec<String>> {
    let n = n.max(1);
    let mut sorted = classes.to_vec();
    sorted.sort();
    let mut shards = vec![vec![]; n];
    for (i, c) in sorted.into_iter().enumerate() {
        shards[i % n].push(c);
    }
    shards.retain(|s: &Vec<String>| !s.is_empty());
    shards
}

/// What one test JVM (or all of them together) did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Outcome {
    pub success: bool,
    pub summary: Option<Summary>,
}

/// Runs every shard through `exec` in parallel and combines the results: success only if all succeeded;
/// the summary is the sum, or `None` if any shard printed none. The first error aborts.
pub fn run_shards(
    shards: &[Vec<String>],
    exec: impl Fn(usize, &[String]) -> Result<Outcome> + Sync,
) -> Result<Outcome> {
    let outcomes: Vec<Outcome> = shards
        .par_iter()
        .enumerate()
        .map(|(i, s)| exec(i, s))
        .collect::<Result<_>>()?;
    let summary = outcomes
        .iter()
        .map(|o| o.summary)
        .try_fold(Summary::default(), |acc, s| s.map(|s| acc + s));
    Ok(Outcome {
        success: outcomes.iter().all(|o| o.success),
        summary,
    })
}

/// A stored pass.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Record {
    pub key: String,
    pub summary: Summary,
}

fn record_path(store: &Path, name: &str) -> PathBuf {
    store.join(format!("{name}.toml"))
}

/// The record stored under `name` if its key is `key`.
pub fn lookup(store: &Path, name: &str, key: &str) -> Option<Record> {
    let rec: Record =
        toml::from_str(&std::fs::read_to_string(record_path(store, name)).ok()?).ok()?;
    (rec.key == key).then_some(rec)
}

pub fn save(store: &Path, name: &str, rec: &Record) -> Result<()> {
    std::fs::create_dir_all(store)?;
    // Write-then-rename, so that jobs sharing a cache directory never read a half-written record.
    let tmp = store.join(format!("{name}.toml.{}.tmp", std::process::id()));
    let path = record_path(store, name);
    (|| -> Result<()> {
        std::fs::write(&tmp, toml::to_string(rec)?)?;
        std::fs::rename(&tmp, &path)?;
        Ok(())
    })()
    .with_context(|| format!("writing test cache in {}", store.display()))
}

/// Removes a stale record so an outdated pass cannot outlive a failing run.
pub fn forget(store: &Path, name: &str) {
    let _ = std::fs::remove_file(record_path(store, name));
}

/// How a run ended.
#[derive(Debug, PartialEq, Eq)]
pub enum Verdict {
    /// Inputs unchanged since a pass: nothing was run.
    Cached(Record),
    Ran(Outcome),
}

/// Runs `exec` unless `use_cache` and the stored record matches `key`. A passing run with a summary is stored;
/// a failing run (or one without a summary to report) removes the record.
pub fn run_cached(
    store: &Path,
    name: &str,
    key: &str,
    use_cache: bool,
    exec: impl FnOnce() -> Result<Outcome>,
) -> Result<Verdict> {
    if use_cache && let Some(rec) = lookup(store, name, key) {
        return Ok(Verdict::Cached(rec));
    }
    let outcome = exec()?;
    match (outcome.success, outcome.summary) {
        (true, Some(summary)) => save(
            store,
            name,
            &Record {
                key: key.into(),
                summary,
            },
        )?,
        _ => forget(store, name),
    }
    Ok(Verdict::Ran(outcome))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;

    fn dir(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("rvn-testcache-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn pair(a: &str, b: &str) -> (String, String) {
        (a.into(), b.into())
    }

    fn inputs() -> KeyInputs {
        KeyInputs {
            jdk: "25".into(),
            jvm_args: vec!["-Xmx1g".into(), "-ea".into()],
            classes: vec!["a.B".into(), "a.C".into()],
            jars: vec![pair("g:a:1", "h1"), pair("g:b:1", "h2")],
            files: vec![pair("a/B.class", "x"), pair("in.csv", "y")],
        }
    }

    fn pass(n: usize) -> Outcome {
        Outcome {
            success: true,
            summary: Some(Summary {
                found: n,
                successful: n,
                failed: 0,
            }),
        }
    }

    #[test]
    fn key_ignores_the_order_of_sets_but_not_of_jvm_arguments() {
        let mut shuffled = inputs();
        shuffled.classes.reverse();
        shuffled.jars.reverse();
        shuffled.files.reverse();
        assert_eq!(key(&inputs()), key(&shuffled));
        let mut reordered = inputs();
        reordered.jvm_args.reverse();
        assert_ne!(key(&inputs()), key(&reordered));
    }

    #[test]
    fn key_changes_when_any_input_changes() {
        let base = key(&inputs());
        let edits: [fn(&mut KeyInputs); 5] = [
            |i| i.jdk = "21".into(),
            |i| i.jvm_args.push("-Dx=1".into()),
            |i| i.classes.push("a.D".into()),
            |i| i.jars[0].1 = "h9".into(),
            |i| i.files[1].1 = "z".into(),
        ];
        for edit in edits {
            let mut i = inputs();
            edit(&mut i);
            assert_ne!(key(&i), base);
        }
    }

    #[test]
    fn key_does_not_confuse_a_jar_with_a_file_of_the_same_text() {
        let a = KeyInputs {
            jars: vec![pair("p", "h")],
            ..Default::default()
        };
        let b = KeyInputs {
            files: vec![pair("p", "h")],
            ..Default::default()
        };
        assert_ne!(key(&a), key(&b));
    }

    #[test]
    fn summary_is_read_from_the_launcher_table() {
        let out = "noise\n[        12 containers found      ]\n[        15 tests found           ]\n[        14 tests successful      ]\n[         1 tests failed          ]\n";
        assert_eq!(
            parse_summary(out),
            Some(Summary {
                found: 15,
                successful: 14,
                failed: 1
            })
        );
        assert_eq!(parse_summary("no table here"), None);
        assert_eq!(
            parse_summary("[ 3 tests found ]"),
            None,
            "a partial table is not a summary"
        );
    }

    #[test]
    fn summaries_add_up() {
        let s = Summary {
            found: 3,
            successful: 2,
            failed: 1,
        };
        assert_eq!(
            s + s,
            Summary {
                found: 6,
                successful: 4,
                failed: 2
            }
        );
    }

    #[test]
    fn shards_are_stable_balanced_and_never_empty() {
        let classes: Vec<String> = ["d", "a", "c", "b", "e"].map(String::from).to_vec();
        assert_eq!(
            split_shards(&classes, 2),
            [vec!["a", "c", "e"], vec!["b", "d"]]
        );
        assert_eq!(split_shards(&classes, 1), [vec!["a", "b", "c", "d", "e"]]);
        assert_eq!(split_shards(&classes, 0).len(), 1, "zero shards means one");
        assert_eq!(
            split_shards(&classes[..2], 8).len(),
            2,
            "no more shards than classes"
        );
        assert!(split_shards(&[], 4).is_empty());
    }

    #[test]
    fn shard_outcomes_are_combined() {
        let shards = split_shards(&["a".into(), "b".into(), "c".into()], 2);
        let all = run_shards(&shards, |_, s| Ok(pass(s.len()))).unwrap();
        assert_eq!(all, pass(3));
        let one_fails = run_shards(&shards, |i, _| {
            Ok(if i == 1 {
                Outcome {
                    success: false,
                    summary: None,
                }
            } else {
                pass(2)
            })
        })
        .unwrap();
        assert_eq!(
            one_fails,
            Outcome {
                success: false,
                summary: None
            }
        );
        assert!(run_shards(&shards, |_, _| anyhow::bail!("jvm missing")).is_err());
    }

    #[test]
    fn a_pass_is_stored_and_served_for_the_same_key() {
        let d = dir("hit");
        let runs = Cell::new(0);
        let go = |key: &str, use_cache| {
            run_cached(&d, "unit", key, use_cache, || {
                runs.set(runs.get() + 1);
                Ok(pass(4))
            })
            .unwrap()
        };
        assert_eq!(go("k1", true), Verdict::Ran(pass(4)));
        assert_eq!(
            go("k1", true),
            Verdict::Cached(Record {
                key: "k1".into(),
                summary: Summary {
                    found: 4,
                    successful: 4,
                    failed: 0
                }
            })
        );
        assert_eq!(runs.get(), 1);
        assert_eq!(
            go("k2", true),
            Verdict::Ran(pass(4)),
            "another key runs again"
        );
        assert_eq!(
            go("k2", false),
            Verdict::Ran(pass(4)),
            "rerun ignores the cache"
        );
        assert_eq!(runs.get(), 3);
    }

    #[test]
    fn a_failure_is_never_stored_and_removes_the_old_pass() {
        let d = dir("fail");
        run_cached(&d, "unit", "k", true, || Ok(pass(1))).unwrap();
        let fail = || {
            Ok(Outcome {
                success: false,
                summary: Some(Summary {
                    found: 1,
                    successful: 0,
                    failed: 1,
                }),
            })
        };
        assert!(
            matches!(run_cached(&d, "unit", "k", false, fail).unwrap(), Verdict::Ran(o) if !o.success)
        );
        assert_eq!(lookup(&d, "unit", "k"), None);
    }

    #[test]
    fn a_pass_without_a_summary_is_not_cached() {
        let d = dir("nosummary");
        run_cached(&d, "unit", "k", true, || {
            Ok(Outcome {
                success: true,
                summary: None,
            })
        })
        .unwrap();
        assert_eq!(lookup(&d, "unit", "k"), None);
    }

    #[test]
    fn an_error_leaves_the_stored_pass_alone() {
        let d = dir("error");
        run_cached(&d, "unit", "k", true, || Ok(pass(1))).unwrap();
        assert!(run_cached(&d, "unit", "other", true, || anyhow::bail!("boom")).is_err());
        assert!(lookup(&d, "unit", "k").is_some());
    }

    #[test]
    fn saving_leaves_no_temporary_file_and_replaces_the_old_record() {
        let d = dir("atomic");
        let rec = |k: &str| Record {
            key: k.into(),
            summary: Summary::default(),
        };
        save(&d, "unit", &rec("one")).unwrap();
        save(&d, "unit", &rec("two")).unwrap();
        let names: Vec<_> = std::fs::read_dir(&d)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names, ["unit.toml"]);
        assert_eq!(lookup(&d, "unit", "two"), Some(rec("two")));
        assert_eq!(lookup(&d, "unit", "one"), None);
    }

    #[test]
    fn phases_have_separate_records() {
        let d = dir("phases");
        run_cached(&d, "unit", "k", true, || Ok(pass(1))).unwrap();
        assert_eq!(lookup(&d, "integration", "k"), None);
    }

    #[test]
    fn trees_are_hashed_by_relative_path_and_content() {
        let d = dir("tree");
        std::fs::create_dir_all(d.join("a")).unwrap();
        std::fs::write(d.join("a/B.class"), "one").unwrap();
        let before = hash_tree(&d).unwrap();
        assert_eq!(before[0].0, "a/B.class");
        std::fs::write(d.join("a/B.class"), "two").unwrap();
        assert_ne!(hash_tree(&d).unwrap(), before);
        assert!(hash_tree(&d.join("missing")).unwrap().is_empty());
    }

    #[test]
    fn declared_inputs_match_globs_and_skip_target_and_git() {
        let d = dir("declared");
        for f in [
            "test/in/a.csv",
            "test/in/b.txt",
            "target/x.csv",
            ".git/y.csv",
        ] {
            std::fs::create_dir_all(d.join(f).parent().unwrap()).unwrap();
            std::fs::write(d.join(f), f).unwrap();
        }
        let names: Vec<String> = hash_declared(&d, &["**/*.csv".into()])
            .unwrap()
            .into_iter()
            .map(|(p, _)| p)
            .collect();
        assert_eq!(names, ["test/in/a.csv"]);
        assert!(hash_declared(&d, &[]).unwrap().is_empty());
    }
}
