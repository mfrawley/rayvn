//! `rvn test` and `rvn verify`: compile `src/test/java`, then run JUnit 5 through the JUnit Platform Console
//! Launcher. Which classes run follows the surefire (unit tests) and failsafe (integration tests) `includes`,
//! `excludes` and skip settings in the POM; the plugins themselves are never executed.

use crate::build::{compile, copy_resources, glob, java_sources};
use crate::model::{Effective, interpolate};
use crate::pom::TestCfg;
use crate::repos::Repos;
use crate::resolve::Resolved;
use anyhow::{Context, Result, bail};
use std::path::{Path, PathBuf};
use std::process::Command;

const SUREFIRE_INCLUDES: [&str; 4] = ["**/Test*.java", "**/*Test.java", "**/*Tests.java", "**/*TestCase.java"];
const FAILSAFE_INCLUDES: [&str; 3] = ["**/IT*.java", "**/*IT.java", "**/*ITCase.java"];

/// Which of the two Maven test phases to run.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Phase {
    Unit,
    Integration,
}

impl Phase {
    fn label(self) -> &'static str {
        match self {
            Phase::Unit => "unit tests",
            Phase::Integration => "integration tests",
        }
    }
}

/// Test classes below `src/test/java`, as source paths relative to it, chosen by the plugin's globs
/// (or the plugin's defaults when none are configured).
fn select_tests(files: &[String], cfg: &TestCfg, defaults: &[&str]) -> Vec<String> {
    let includes: Vec<&str> = if cfg.includes.is_empty() { defaults.to_vec() } else { cfg.includes.iter().map(String::as_str).collect() };
    files
        .iter()
        .filter(|f| includes.iter().any(|g| glob(g, f)) && !cfg.excludes.iter().any(|g| glob(g, f)))
        .cloned()
        .collect()
}

/// `io/moia/FooTest.java` -> `io.moia.FooTest`.
fn class_name(source: &str) -> String {
    source.trim_end_matches(".java").replace('/', ".")
}

/// True when a skip setting (`skip`, `skipTests`, `skipITs`) interpolates to `true`.
fn skipped(cfg: &TestCfg, root: &Effective) -> bool {
    cfg.skips.iter().any(|s| interpolate(s, &root.props) == "true")
}

fn relative_sources(dir: &Path) -> Result<Vec<String>> {
    Ok(java_sources(dir)?.iter().filter_map(|p| p.strip_prefix(dir).ok()).map(|p| p.to_string_lossy().replace('\\', "/")).collect())
}

/// Compiles the tests into `target/test-classes` and returns that directory.
pub fn compile_tests(root: &Effective, dir: &Path, main_classes: &Path, resolved: &[Resolved]) -> Result<PathBuf> {
    let out = dir.join("target/test-classes");
    std::fs::create_dir_all(&out)?;
    let mut cp = vec![main_classes.display().to_string()];
    cp.extend(resolved.iter().filter_map(|r| r.jar.as_ref().map(|p| p.display().to_string())));
    eprintln!("  compiling tests");
    compile(root, &dir.join("src/test/java"), &out, &cp)?;
    copy_resources(&dir.join("src/test/resources"), &out)?;
    Ok(out)
}

/// The JUnit Platform Console Launcher, at the same version as the `junit-platform-engine` already resolved.
fn console_launcher(repos: &Repos, resolved: &[Resolved]) -> Result<PathBuf> {
    let engine = resolved
        .iter()
        .find(|r| r.gav.group == "org.junit.platform" && r.gav.artifact == "junit-platform-engine")
        .context("no junit-platform-engine in the dependencies: add junit-jupiter as a test dependency")?;
    repos
        .get("org.junit.platform", "junit-platform-console-standalone", &engine.gav.version, None, "jar")?
        .map(|l| l.path)
        .with_context(|| format!("junit-platform-console-standalone {} not found in any repository", engine.gav.version))
}

fn java_command(classpath: &[String], jvm_args: &[String], classes: &[String]) -> Command {
    let mut cmd = Command::new("java");
    cmd.args(jvm_args);
    cmd.arg("-cp").arg(classpath.join(":")).arg("org.junit.platform.console.ConsoleLauncher");
    cmd.args(["execute", "--disable-banner", "--details=summary", "--disable-ansi-colors"]);
    for c in classes {
        cmd.args(["--select-class", c]);
    }
    cmd
}

/// Runs the selected tests of `phase`; returns false when none were selected or the phase is skipped.
pub fn run(phase: Phase, root: &Effective, dir: &Path, test_classes: &Path, main_classes: &Path, resolved: &[Resolved], repos: &Repos) -> Result<bool> {
    let cfg = match phase {
        Phase::Unit => root.surefire.clone().unwrap_or_default(),
        Phase::Integration => root.failsafe.clone().unwrap_or_default(),
    };
    if skipped(&cfg, root) {
        eprintln!("  {} skipped by the POM", phase.label());
        return Ok(false);
    }
    for i in &cfg.ignored {
        eprintln!("  warning: test option not supported by rayvn, ignored: {i}");
    }
    let defaults: &[&str] = if phase == Phase::Unit { &SUREFIRE_INCLUDES } else { &FAILSAFE_INCLUDES };
    let classes: Vec<String> = select_tests(&relative_sources(&dir.join("src/test/java"))?, &cfg, defaults).iter().map(|s| class_name(s)).collect();
    if classes.is_empty() {
        eprintln!("  no {} selected", phase.label());
        return Ok(false);
    }
    let launcher = console_launcher(repos, resolved)?;
    let mut classpath = vec![test_classes.display().to_string(), main_classes.display().to_string()];
    classpath.extend(resolved.iter().filter_map(|r| r.jar.as_ref().map(|p| p.display().to_string())));
    // Last, so the project's own JUnit jars win; both are the same version.
    classpath.push(launcher.display().to_string());
    let jvm_args: Vec<String> = cfg.arg_line.iter().flat_map(|a| interpolate(a, &root.props).split_whitespace().map(String::from).collect::<Vec<_>>()).collect();
    eprintln!("  running {} {} with JUnit", classes.len(), phase.label());
    let status = java_command(&classpath, &jvm_args, &classes).current_dir(dir).status().context("running java")?;
    if !status.success() {
        bail!("{} failed", phase.label());
    }
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn files() -> Vec<String> {
        ["a/FooTest.java", "a/TestBar.java", "a/BazIT.java", "a/ITQux.java", "a/Helper.java", "FooTests.java"].map(String::from).to_vec()
    }

    #[test]
    fn surefire_defaults_pick_test_classes_and_not_integration_tests() {
        assert_eq!(select_tests(&files(), &TestCfg::default(), &SUREFIRE_INCLUDES), ["a/FooTest.java", "a/TestBar.java", "FooTests.java"]);
    }

    #[test]
    fn failsafe_defaults_pick_integration_tests() {
        assert_eq!(select_tests(&files(), &TestCfg::default(), &FAILSAFE_INCLUDES), ["a/BazIT.java", "a/ITQux.java"]);
    }

    #[test]
    fn configured_includes_replace_the_defaults_and_excludes_remove() {
        let cfg = TestCfg { includes: vec!["**/*Test.java".into(), "**/*IT.java".into()], excludes: vec!["**/*IT.java".into()], ..Default::default() };
        assert_eq!(select_tests(&files(), &cfg, &SUREFIRE_INCLUDES), ["a/FooTest.java"]);
    }

    #[test]
    fn class_names_come_from_source_paths() {
        assert_eq!(class_name("io/moia/FooTest.java"), "io.moia.FooTest");
        assert_eq!(class_name("FooTest.java"), "FooTest");
    }

    #[test]
    fn skip_settings_are_interpolated() {
        let root = Effective {
            gav: crate::pom::Gav::new("g", "a", "1"),
            packaging: "jar".into(),
            props: [("skipITs".to_string(), "true".to_string())].into(),
            managed: Default::default(),
            deps: vec![],
            repos: vec![],
            shade: None,
            surefire: None,
            failsafe: None,
        };
        assert!(skipped(&TestCfg { skips: vec!["${skipITs}".into()], ..Default::default() }, &root));
        assert!(!skipped(&TestCfg { skips: vec!["${unset}".into(), "false".into()], ..Default::default() }, &root));
    }

    #[test]
    fn java_command_selects_each_class_after_the_jvm_args_and_classpath() {
        let cmd = java_command(&["a".into(), "b".into()], &["-Xmx1g".into()], &["x.A".into(), "x.B".into()]);
        let args: Vec<_> = cmd.get_args().map(|a| a.to_string_lossy().into_owned()).collect();
        assert_eq!(&args[..4], ["-Xmx1g", "-cp", "a:b", "org.junit.platform.console.ConsoleLauncher"]);
        assert_eq!(&args[args.len() - 4..], ["--select-class", "x.A", "--select-class", "x.B"]);
    }
}
