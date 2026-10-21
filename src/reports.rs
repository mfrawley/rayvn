//! JUnit XML reports of a test run, in the layout CI systems already look for: `target/surefire-reports/` for unit
//! tests and `target/failsafe-reports/` for integration tests, one `TEST-*.xml` per test JVM.
//!
//! The Console Launcher writes `TEST-junit-jupiter.xml` into the directory it is given. Each shard gets a private
//! directory so that parallel JVMs cannot overwrite each other, and [`collect`] moves the file into the reports
//! directory under a per-shard name. A result served from the test cache starts no JVM, so the reports of the
//! run it stands for are kept beside the cache record and put back with [`copy_set`].

use anyhow::{Context, Result};
use std::path::{Path, PathBuf};

const LAUNCHER_FILE: &str = "TEST-junit-jupiter.xml";

fn is_report(name: &str) -> bool {
    name.starts_with("TEST-") && name.ends_with(".xml")
}

/// The `TEST-*.xml` files directly inside `dir`, sorted; none if the directory does not exist.
pub fn files(dir: &Path) -> Result<Vec<PathBuf>> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Ok(vec![]);
    };
    let mut found: Vec<PathBuf> = entries
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(is_report)
        })
        .collect();
    found.sort();
    Ok(found)
}

/// Removes the `TEST-*.xml` files of an earlier run, so that a report never describes tests that no longer exist.
pub fn clear(dir: &Path) -> Result<()> {
    for f in files(dir)? {
        std::fs::remove_file(&f).with_context(|| format!("removing {}", f.display()))?;
    }
    Ok(())
}

/// The private directory the launcher of shard `index` (from 0) writes into.
pub fn shard_dir(reports: &Path, index: usize) -> PathBuf {
    reports.join(format!(".shard-{}", index + 1))
}

/// `TEST-junit-jupiter.xml` for a single JVM, `TEST-junit-jupiter-2.xml` and so on when the run is sharded.
fn shard_file(index: usize, shards: usize) -> String {
    if shards > 1 {
        format!("TEST-junit-jupiter-{}.xml", index + 1)
    } else {
        LAUNCHER_FILE.to_string()
    }
}

/// Moves the launcher's report out of shard `index`'s private directory into `reports`, then removes that
/// directory. Returns whether the launcher wrote a report (it does not when the JVM fails to start).
pub fn collect(reports: &Path, index: usize, shards: usize) -> Result<bool> {
    let from = shard_dir(reports, index);
    let written = from.join(LAUNCHER_FILE);
    let found = written.is_file();
    if found {
        std::fs::rename(&written, reports.join(shard_file(index, shards)))
            .with_context(|| format!("moving {}", written.display()))?;
    }
    let _ = std::fs::remove_dir_all(&from);
    Ok(found)
}

/// Makes `to` hold exactly the reports in `from`: the old ones in `to` go, copies of the new ones arrive.
/// Returns how many files were copied.
pub fn copy_set(from: &Path, to: &Path) -> Result<usize> {
    std::fs::create_dir_all(to).with_context(|| format!("creating {}", to.display()))?;
    clear(to)?;
    let sources = files(from)?;
    for f in &sources {
        let name = f.file_name().context("report without a file name")?;
        std::fs::copy(f, to.join(name)).with_context(|| format!("copying {}", f.display()))?;
    }
    Ok(sources.len())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dir(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("rvn-reports-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn write(dir: &Path, name: &str, body: &str) {
        std::fs::create_dir_all(dir).unwrap();
        std::fs::write(dir.join(name), body).unwrap();
    }

    fn names(dir: &Path) -> Vec<String> {
        files(dir)
            .unwrap()
            .iter()
            .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
            .collect()
    }

    #[test]
    fn only_test_xml_files_count_as_reports() {
        let d = dir("only");
        write(&d, "TEST-a.xml", "");
        write(&d, "TEST-b.txt", "");
        write(&d, "other.xml", "");
        assert_eq!(names(&d), ["TEST-a.xml"]);
        assert!(files(&d.join("missing")).unwrap().is_empty());
    }

    #[test]
    fn clearing_removes_old_reports_and_nothing_else() {
        let d = dir("clear");
        write(&d, "TEST-old.xml", "");
        write(&d, "notes.txt", "");
        clear(&d).unwrap();
        assert!(names(&d).is_empty());
        assert!(d.join("notes.txt").exists());
        clear(&d.join("missing")).unwrap();
    }

    #[test]
    fn a_single_jvm_keeps_the_launcher_file_name() {
        let d = dir("one");
        write(&shard_dir(&d, 0), LAUNCHER_FILE, "<testsuite/>");
        assert!(collect(&d, 0, 1).unwrap());
        assert_eq!(names(&d), [LAUNCHER_FILE]);
        assert!(
            !shard_dir(&d, 0).exists(),
            "the private directory is removed"
        );
    }

    #[test]
    fn shards_get_one_report_each_and_none_overwrites_another() {
        let d = dir("shards");
        for i in 0..3 {
            write(
                &shard_dir(&d, i),
                LAUNCHER_FILE,
                &format!("<testsuite n='{i}'/>"),
            );
        }
        for i in 0..3 {
            assert!(collect(&d, i, 3).unwrap());
        }
        assert_eq!(
            names(&d),
            [
                "TEST-junit-jupiter-1.xml",
                "TEST-junit-jupiter-2.xml",
                "TEST-junit-jupiter-3.xml"
            ]
        );
        assert_eq!(
            std::fs::read_to_string(d.join("TEST-junit-jupiter-2.xml")).unwrap(),
            "<testsuite n='1'/>"
        );
    }

    #[test]
    fn a_jvm_that_wrote_no_report_is_reported_as_such() {
        let d = dir("none");
        std::fs::create_dir_all(shard_dir(&d, 0)).unwrap();
        assert!(!collect(&d, 0, 1).unwrap());
        assert!(names(&d).is_empty());
    }

    #[test]
    fn copying_a_set_replaces_what_was_there() {
        let (from, to) = (dir("from"), dir("to"));
        write(&from, "TEST-new.xml", "new");
        write(&to, "TEST-stale.xml", "stale");
        assert_eq!(copy_set(&from, &to).unwrap(), 1);
        assert_eq!(names(&to), ["TEST-new.xml"]);
        assert_eq!(
            std::fs::read_to_string(to.join("TEST-new.xml")).unwrap(),
            "new"
        );
    }

    #[test]
    fn copying_from_nothing_empties_the_destination() {
        let to = dir("empty-to");
        write(&to, "TEST-stale.xml", "");
        assert_eq!(copy_set(&to.join("missing"), &to).unwrap(), 0);
        assert!(names(&to).is_empty());
    }
}
