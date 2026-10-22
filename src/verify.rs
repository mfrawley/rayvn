//! `rvn test` and `rvn verify`: compile `src/test/java`, then run JUnit 5 through the JUnit Platform Console
//! Launcher. Which classes run follows the surefire (unit tests) and failsafe (integration tests) `includes`,
//! `excludes` and skip settings in the POM; the plugins themselves are never executed.

use crate::build::{compile, copy_resources, glob, java_sources};
use crate::log::{self, say};
use crate::model::{Effective, interpolate};
use crate::pom::TestCfg;
use crate::reports;
use crate::repos::Repos;
use crate::resolve::Resolved;
use crate::settings::TestSettings;
use crate::testcache::{self, KeyInputs, Outcome, Verdict, parse_summary};
use anyhow::{Context, Result, bail};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Mutex;

const SUREFIRE_INCLUDES: [&str; 4] = [
    "**/Test*.java",
    "**/*Test.java",
    "**/*Tests.java",
    "**/*TestCase.java",
];
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
    let includes: Vec<&str> = if cfg.includes.is_empty() {
        defaults.to_vec()
    } else {
        cfg.includes.iter().map(String::as_str).collect()
    };
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
    cfg.skips
        .iter()
        .any(|s| interpolate(s, &root.props) == "true")
}

fn relative_sources(dir: &Path) -> Result<Vec<String>> {
    Ok(java_sources(dir)?
        .iter()
        .filter_map(|p| p.strip_prefix(dir).ok())
        .map(|p| p.to_string_lossy().replace('\\', "/"))
        .collect())
}

/// Whether the module has test sources; without them there is nothing to compile or run.
pub fn has_tests(dir: &Path) -> Result<bool> {
    Ok(!java_sources(&dir.join("src/test/java"))?.is_empty())
}

/// Compiles the tests into `target/test-classes` and returns that directory.
pub fn compile_tests(
    root: &Effective,
    dir: &Path,
    main_classes: &Path,
    resolved: &[Resolved],
) -> Result<PathBuf> {
    let out = dir.join("target/test-classes");
    std::fs::create_dir_all(&out)?;
    let mut cp = vec![main_classes.display().to_string()];
    cp.extend(
        resolved
            .iter()
            .filter_map(|r| r.jar.as_ref().map(|p| p.display().to_string())),
    );
    say!("  compiling tests");
    compile(root, &dir.join("src/test/java"), &out, &cp)?;
    copy_resources(&dir.join("src/test/resources"), &out)?;
    Ok(out)
}

/// The JUnit Platform Console Launcher, at the same version as the `junit-platform-engine` already resolved.
fn console_launcher(repos: &Repos, resolved: &[Resolved]) -> Result<PathBuf> {
    let engine = resolved
        .iter()
        .find(|r| r.gav.group == "org.junit.platform" && r.gav.artifact == "junit-platform-engine")
        .context(
            "no junit-platform-engine in the dependencies: add junit-jupiter as a test dependency",
        )?;
    repos
        .get(
            "org.junit.platform",
            "junit-platform-console-standalone",
            &engine.gav.version,
            None,
            "jar",
        )?
        .map(|l| l.path)
        .with_context(|| {
            format!(
                "junit-platform-console-standalone {} not found in any repository",
                engine.gav.version
            )
        })
}

/// The test JVM runs in the module's directory, so every entry must be absolute: with a relative POM path
/// (`rvn test` at a workspace root) `./mod/target/classes` would otherwise be looked up under `./mod/` again.
fn absolute_classpath(entries: &[PathBuf]) -> Result<Vec<String>> {
    entries
        .iter()
        .map(|p| Ok(std::path::absolute(p)?.display().to_string()))
        .collect()
}

fn java_command(
    classpath: &[String],
    jvm_args: &[String],
    classes: &[String],
    reports: &Path,
) -> Command {
    let mut cmd = Command::new("java");
    cmd.args(jvm_args);
    cmd.arg("-cp")
        .arg(classpath.join(":"))
        .arg("org.junit.platform.console.ConsoleLauncher");
    cmd.args([
        "execute",
        "--disable-banner",
        "--details=summary",
        "--disable-ansi-colors",
    ]);
    cmd.arg("--reports-dir").arg(reports);
    for c in classes {
        cmd.args(["--select-class", c]);
    }
    cmd
}

/// What to do for one phase, decided from the POM and `rayvn.toml` alone.
#[derive(Debug, PartialEq, Eq)]
pub enum Selection {
    /// The POM skips the phase (`skipTests`, `skipITs`, ...).
    Skipped,
    /// No test class matched the includes.
    Empty,
    Run(Plan),
}

#[derive(Debug, PartialEq, Eq)]
pub struct Plan {
    pub classes: Vec<String>,
    pub jvm_args: Vec<String>,
    /// May a stored pass stand in for a run?
    pub use_cache: bool,
    pub shards: usize,
}

/// Pure planning step: which classes run, with which JVM arguments, cached or not, in how many JVMs.
/// `sources` are the paths below `src/test/java`; `rerun` (`--rerun`) turns the cache off.
pub fn plan(
    phase: Phase,
    cfg: &TestCfg,
    root: &Effective,
    sources: &[String],
    settings: &TestSettings,
    rerun: bool,
) -> Selection {
    if skipped(cfg, root) {
        return Selection::Skipped;
    }
    let defaults: &[&str] = if phase == Phase::Unit {
        &SUREFIRE_INCLUDES
    } else {
        &FAILSAFE_INCLUDES
    };
    let classes: Vec<String> = select_tests(sources, cfg, defaults)
        .iter()
        .map(|s| class_name(s))
        .collect();
    if classes.is_empty() {
        return Selection::Empty;
    }
    let cacheable = match phase {
        Phase::Unit => settings.cache.unwrap_or(true),
        Phase::Integration => settings.cache_integration.unwrap_or(false),
    };
    let jvm_args = cfg
        .arg_line
        .iter()
        .flat_map(|a| {
            interpolate(a, &root.props)
                .split_whitespace()
                .map(String::from)
                .collect::<Vec<_>>()
        })
        .collect();
    Selection::Run(Plan {
        classes,
        jvm_args,
        use_cache: cacheable && !rerun,
        shards: settings.shards.unwrap_or(1).max(1),
    })
}

/// `(coordinates, sha256)` of every resolved jar. A workspace module is identified by the hash of its class tree,
/// a jar without a checksum by its path.
fn jar_inputs(resolved: &[Resolved]) -> Result<Vec<(String, String)>> {
    resolved
        .iter()
        .filter_map(|r| Some((r, r.jar.as_ref()?)))
        .map(|(r, jar)| {
            let coords = format!("{}:{}", r.gav, r.classifier.as_deref().unwrap_or(""));
            let id = match (&r.sha256, r.workspace) {
                (Some(h), _) => h.clone(),
                (None, true) => tree_hash(jar)?,
                (None, false) => jar.display().to_string(),
            };
            Ok((coords, id))
        })
        .collect()
}

/// One hash for everything below `dir`, by relative path and content.
fn tree_hash(dir: &Path) -> Result<String> {
    let lines: Vec<String> = testcache::hash_tree(dir)?
        .into_iter()
        .map(|(p, h)| format!("{p} {h}"))
        .collect();
    Ok(crate::lock::hash_bytes(lines.join("\n").as_bytes()))
}

fn prefixed(prefix: &str, files: Vec<(String, String)>) -> impl Iterator<Item = (String, String)> {
    files
        .into_iter()
        .map(move |(p, h)| (format!("{prefix}/{p}"), h))
}

/// Where test results are stored: `--test-cache`, else `RAYVN_TEST_CACHE`, else `[test] cache_dir` (relative to the
/// project), else `target/rayvn-test-cache`. Empty values count as unset.
pub fn cache_dir(
    flag: Option<&str>,
    env: Option<&str>,
    configured: Option<&str>,
    project: &Path,
) -> PathBuf {
    fn set(v: Option<&str>) -> Option<&str> {
        v.filter(|s| !s.is_empty())
    }
    match (set(flag), set(env), set(configured)) {
        (Some(p), _, _) | (None, Some(p), _) => PathBuf::from(p),
        (None, None, Some(p)) => project.join(p),
        _ => project.join("target/rayvn-test-cache"),
    }
}

/// Record name for one project and phase, so that projects can share a cache directory.
fn record_name(root: &Effective, phase: Phase) -> String {
    format!("{}.{}.{}", root.gav.group, root.gav.artifact, phase.name())
}

/// The version banner of the `java` on PATH (`java -version` prints to stderr).
fn jdk_version() -> Result<String> {
    let out = Command::new("java")
        .arg("-version")
        .output()
        .context("running java -version")?;
    Ok(String::from_utf8_lossy(&out.stderr).trim().to_string())
}

/// Copies `reader` to `out` line by line, each line behind `prefix`, and returns everything read. Pass an unlocked
/// `Stdout`: each line is written atomically, and parallel shards must not hold the lock between lines.
fn pump(reader: impl std::io::BufRead, prefix: &str, out: &mut impl Write) -> Result<String> {
    let mut all = String::new();
    for line in reader.lines() {
        let line = line?;
        writeln!(out, "{prefix}{line}")?;
        all.push_str(&line);
        all.push('\n');
    }
    Ok(all)
}

/// Runs `cmd`, streaming its output behind `prefix` (stdout to stdout, stderr to stderr), and reports success and the
/// launcher's summary.
fn run_command(cmd: Command, prefix: &str) -> Result<Outcome> {
    run_command_writers(cmd, prefix, &mut std::io::stdout(), &mut std::io::stderr())
}

fn run_command_writers(
    mut cmd: Command,
    prefix: &str,
    stdout_writer: &mut impl Write,
    stderr_writer: &mut (impl Write + Send),
) -> Result<Outcome> {
    let mut child = cmd
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("running java")?;
    let stdout = child.stdout.take().context("no stdout from java")?;
    let stderr = child.stderr.take().context("no stderr from java")?;
    let output = std::thread::scope(|scope| -> Result<String> {
        let errors = scope.spawn(|| pump(std::io::BufReader::new(stderr), prefix, stderr_writer));
        let output = pump(std::io::BufReader::new(stdout), prefix, stdout_writer)?;
        errors
            .join()
            .map_err(|_| anyhow::anyhow!("reading the test JVM's stderr panicked"))??;
        Ok(output)
    })?;
    let status = child.wait()?;
    Ok(Outcome {
        success: status.success(),
        summary: parse_summary(&output),
    })
}

/// Everything `run` needs to know about the build; keeps the parameter list short.
pub struct TestRun<'a> {
    pub root: &'a Effective,
    pub dir: &'a Path,
    pub main_classes: &'a Path,
    pub test_classes: &'a Path,
    pub resolved: &'a [Resolved],
    pub repos: &'a Repos,
    pub settings: &'a TestSettings,
    pub rerun: bool,
    /// Value of `--test-cache`, if given.
    pub cache_flag: Option<&'a str>,
    /// Value of `--reports-dir`, if given.
    pub reports_flag: Option<&'a str>,
}

/// Where the JUnit XML of `phase` goes: `<reports_flag>/<artifact>/<surefire|failsafe>-reports` (a directory per
/// module, since every run clears its own), else `target/<surefire|failsafe>-reports` in the module. Absolute,
/// because the test JVM runs in the module's directory.
fn reports_dir(phase: Phase, flag: Option<&str>, module: &Path, artifact: &str) -> Result<PathBuf> {
    let base = match flag.filter(|f| !f.is_empty()) {
        Some(f) => PathBuf::from(f).join(artifact),
        None => module.join("target"),
    };
    Ok(std::path::absolute(base.join(phase.reports_name()))?)
}

/// Where the reports of the run a cache record stands for are kept: beside the record, named after it.
fn stored_reports(store: &Path, record: &str) -> PathBuf {
    store.join(format!("{record}.reports"))
}

impl Phase {
    /// Directory name for this phase's JUnit XML, as Maven names it.
    fn reports_name(self) -> &'static str {
        match self {
            Phase::Unit => "surefire-reports",
            Phase::Integration => "failsafe-reports",
        }
    }

    fn name(self) -> &'static str {
        match self {
            Phase::Unit => "unit",
            Phase::Integration => "integration",
        }
    }
}

/// What a test phase did for one module.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TestOutcome {
    /// The POM skips the phase, or no test class was selected.
    NoTests,
    /// The tests ran and passed (count of passed tests when JUnit reported it).
    Passed(usize),
    /// Served from the test cache: the inputs are unchanged since the last pass.
    Cached(usize),
}

/// Integration tests of different modules do not run at the same time: they tend to share what the unit tests do
/// not (ports, databases, files). Bazel's `exclusive` tag does the same.
static INTEGRATION: Mutex<()> = Mutex::new(());

/// Runs (or serves from the cache) the tests of `phase`. A failure is an error.
pub fn run(phase: Phase, cx: &TestRun) -> Result<TestOutcome> {
    let _alone = (phase == Phase::Integration)
        .then(|| INTEGRATION.lock().unwrap_or_else(|e| e.into_inner()));
    let cfg = match phase {
        Phase::Unit => cx.root.surefire.clone(),
        Phase::Integration => cx.root.failsafe.clone(),
    }
    .unwrap_or_default();
    for i in &cfg.ignored {
        say!("  warning: test option not supported by rayvn, ignored: {i}");
    }
    let sources = relative_sources(&cx.dir.join("src/test/java"))?;
    let plan = match plan(phase, &cfg, cx.root, &sources, cx.settings, cx.rerun) {
        Selection::Skipped => {
            say!("  {} skipped by the POM", phase.label());
            return Ok(TestOutcome::NoTests);
        }
        Selection::Empty => {
            say!("  no {} selected", phase.label());
            return Ok(TestOutcome::NoTests);
        }
        Selection::Run(p) => p,
    };
    let key = testcache::key(&KeyInputs {
        jdk: jdk_version()?,
        jvm_args: plan.jvm_args.clone(),
        classes: plan.classes.clone(),
        jars: jar_inputs(cx.resolved)?,
        files: prefixed("main", testcache::hash_tree(cx.main_classes)?)
            .chain(prefixed("test", testcache::hash_tree(cx.test_classes)?))
            .chain(prefixed(
                "input",
                testcache::hash_declared(cx.dir, &cx.settings.inputs)?,
            ))
            .collect(),
    });
    let launcher = console_launcher(cx.repos, cx.resolved)?;
    let mut classpath = vec![cx.test_classes.to_path_buf(), cx.main_classes.to_path_buf()];
    classpath.extend(cx.resolved.iter().filter_map(|r| r.jar.clone()));
    // Last, so the project's own JUnit jars win; both are the same version.
    classpath.push(launcher);
    let classpath = absolute_classpath(&classpath)?;

    let shards = testcache::split_shards(&plan.classes, plan.shards);
    let store = cache_dir(
        cx.cache_flag,
        std::env::var("RAYVN_TEST_CACHE").ok().as_deref(),
        cx.settings.cache_dir.as_deref(),
        cx.dir,
    );
    let record = record_name(cx.root, phase);
    let reports = reports_dir(phase, cx.reports_flag, cx.dir, &cx.root.gav.artifact)?;
    let stored = stored_reports(&store, &record);
    // A record without stored XML (written before reports existed) would leave CI with nothing to read.
    let use_cache = plan.use_cache && !reports::files(&stored)?.is_empty();
    // The shard threads are not this thread, so they get the module's prefix from here.
    let module_prefix = log::prefix().unwrap_or_default();
    let verdict = testcache::run_cached(&store, &record, &key, use_cache, || {
        say!(
            "  running {} {} with JUnit in {} JVM(s)",
            plan.classes.len(),
            phase.label(),
            shards.len()
        );
        reports::clear(&reports)?;
        testcache::run_shards(&shards, |i, classes| {
            let prefix = if shards.len() > 1 {
                format!("{module_prefix}[{}] ", i + 1)
            } else {
                module_prefix.clone()
            };
            let private = reports::shard_dir(&reports, i);
            std::fs::create_dir_all(&private)?;
            let mut cmd = java_command(&classpath, &plan.jvm_args, classes, &private);
            cmd.current_dir(cx.dir);
            let outcome = run_command(cmd, &prefix);
            reports::collect(&reports, i, shards.len())?;
            outcome
        })
    })?;
    match verdict {
        Verdict::Cached(r) => {
            let n = reports::copy_set(&stored, &reports)?;
            say!(
                "  {} up to date: {} passed (cached, inputs unchanged; --rerun to force)",
                phase.label(),
                r.summary.successful
            );
            say!("  {}", reports_line(n, &reports));
            Ok(TestOutcome::Cached(r.summary.successful))
        }
        Verdict::Ran(o) => {
            let n = reports::files(&reports)?.len();
            say!("  {}", reports_line(n, &reports));
            if !o.success {
                bail!("{} failed", phase.label());
            }
            if o.summary.is_some() {
                reports::copy_set(&reports, &stored)?;
            }
            let passed = o.summary.map(|s| s.successful);
            say!(
                "  {} passed{}",
                phase.label(),
                passed.map(|n| format!(": {n}")).unwrap_or_default()
            );
            Ok(TestOutcome::Passed(passed.unwrap_or(0)))
        }
    }
}

fn reports_line(count: usize, dir: &Path) -> String {
    match count {
        0 => format!("no JUnit XML written to {}", dir.display()),
        1 => format!("JUnit XML: {}", dir.display()),
        n => format!("JUnit XML: {n} files in {}", dir.display()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn files() -> Vec<String> {
        [
            "a/FooTest.java",
            "a/TestBar.java",
            "a/BazIT.java",
            "a/ITQux.java",
            "a/Helper.java",
            "FooTests.java",
        ]
        .map(String::from)
        .to_vec()
    }

    #[test]
    fn surefire_defaults_pick_test_classes_and_not_integration_tests() {
        assert_eq!(
            select_tests(&files(), &TestCfg::default(), &SUREFIRE_INCLUDES),
            ["a/FooTest.java", "a/TestBar.java", "FooTests.java"]
        );
    }

    #[test]
    fn failsafe_defaults_pick_integration_tests() {
        assert_eq!(
            select_tests(&files(), &TestCfg::default(), &FAILSAFE_INCLUDES),
            ["a/BazIT.java", "a/ITQux.java"]
        );
    }

    #[test]
    fn configured_includes_replace_the_defaults_and_excludes_remove() {
        let cfg = TestCfg {
            includes: vec!["**/*Test.java".into(), "**/*IT.java".into()],
            excludes: vec!["**/*IT.java".into()],
            ..Default::default()
        };
        assert_eq!(
            select_tests(&files(), &cfg, &SUREFIRE_INCLUDES),
            ["a/FooTest.java"]
        );
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
            compiler: None,
            unrun: vec![],
            surefire: None,
            failsafe: None,
            relocation: None,
        };
        assert!(skipped(
            &TestCfg {
                skips: vec!["${skipITs}".into()],
                ..Default::default()
            },
            &root
        ));
        assert!(!skipped(
            &TestCfg {
                skips: vec!["${unset}".into(), "false".into()],
                ..Default::default()
            },
            &root
        ));
    }

    #[test]
    fn classpath_entries_are_made_absolute_so_they_survive_a_change_of_directory() {
        let cp = absolute_classpath(&[
            PathBuf::from("./mod/target/classes"),
            PathBuf::from("/abs/a.jar"),
        ])
        .unwrap();
        assert!(
            Path::new(&cp[0]).is_absolute() && cp[0].ends_with("mod/target/classes"),
            "{}",
            cp[0]
        );
        assert_eq!(cp[1], "/abs/a.jar");
    }

    #[test]
    fn java_command_selects_each_class_after_the_jvm_args_and_classpath() {
        let cmd = java_command(
            &["a".into(), "b".into()],
            &["-Xmx1g".into()],
            &["x.A".into(), "x.B".into()],
            Path::new("/r"),
        );
        let args: Vec<_> = cmd
            .get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        assert_eq!(
            &args[..4],
            [
                "-Xmx1g",
                "-cp",
                "a:b",
                "org.junit.platform.console.ConsoleLauncher"
            ]
        );
        assert_eq!(
            &args[args.len() - 4..],
            ["--select-class", "x.A", "--select-class", "x.B"]
        );
        let at = args.iter().position(|a| a == "--reports-dir").unwrap();
        assert_eq!(args[at + 1], "/r");
    }

    #[test]
    fn reports_go_where_maven_puts_them_unless_redirected() {
        let module = Path::new("/w/mod");
        assert_eq!(
            reports_dir(Phase::Unit, None, module, "mod").unwrap(),
            Path::new("/w/mod/target/surefire-reports")
        );
        assert_eq!(
            reports_dir(Phase::Integration, None, module, "mod").unwrap(),
            Path::new("/w/mod/target/failsafe-reports")
        );
        assert_eq!(
            reports_dir(Phase::Unit, Some("/ci/out"), module, "mod").unwrap(),
            Path::new("/ci/out/mod/surefire-reports"),
            "one directory per module, so that modules do not clear each other's reports"
        );
        assert_eq!(
            reports_dir(Phase::Unit, Some(""), module, "mod").unwrap(),
            Path::new("/w/mod/target/surefire-reports"),
            "an empty value counts as unset"
        );
    }

    #[test]
    fn stored_reports_sit_beside_the_record_they_belong_to() {
        assert_eq!(
            stored_reports(Path::new("/c"), "g.a.unit"),
            Path::new("/c/g.a.unit.reports")
        );
    }

    #[test]
    fn the_reports_line_says_how_many_files_and_where() {
        let d = Path::new("/r");
        assert_eq!(reports_line(0, d), "no JUnit XML written to /r");
        assert_eq!(reports_line(1, d), "JUnit XML: /r");
        assert_eq!(reports_line(3, d), "JUnit XML: 3 files in /r");
    }

    fn root_with(props: &[(&str, &str)]) -> Effective {
        Effective {
            gav: crate::pom::Gav::new("g", "a", "1"),
            packaging: "jar".into(),
            props: props
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
            managed: Default::default(),
            deps: vec![],
            repos: vec![],
            shade: None,
            compiler: None,
            unrun: vec![],
            surefire: None,
            failsafe: None,
            relocation: None,
        }
    }

    fn planned(phase: Phase, cfg: &TestCfg, settings: &TestSettings, rerun: bool) -> Selection {
        plan(
            phase,
            cfg,
            &root_with(&[("skipITs", "false")]),
            &files(),
            settings,
            rerun,
        )
    }

    fn run_plan(sel: Selection) -> Plan {
        match sel {
            Selection::Run(p) => p,
            other => panic!("expected a run, got {other:?}"),
        }
    }

    #[test]
    fn plan_selects_classes_and_caches_unit_tests_by_default() {
        let p = run_plan(planned(
            Phase::Unit,
            &TestCfg::default(),
            &TestSettings::default(),
            false,
        ));
        assert_eq!(p.classes, ["a.FooTest", "a.TestBar", "FooTests"]);
        assert_eq!((p.use_cache, p.shards), (true, 1));
    }

    #[test]
    fn integration_tests_are_cached_only_on_request() {
        let off = run_plan(planned(
            Phase::Integration,
            &TestCfg::default(),
            &TestSettings::default(),
            false,
        ));
        assert!(!off.use_cache);
        let on = TestSettings {
            cache_integration: Some(true),
            ..Default::default()
        };
        assert!(run_plan(planned(Phase::Integration, &TestCfg::default(), &on, false)).use_cache);
    }

    #[test]
    fn rerun_and_cache_false_both_turn_the_cache_off() {
        assert!(
            !run_plan(planned(
                Phase::Unit,
                &TestCfg::default(),
                &TestSettings::default(),
                true
            ))
            .use_cache
        );
        let off = TestSettings {
            cache: Some(false),
            ..Default::default()
        };
        assert!(!run_plan(planned(Phase::Unit, &TestCfg::default(), &off, false)).use_cache);
    }

    #[test]
    fn shards_default_to_one_and_never_to_zero() {
        let two = TestSettings {
            shards: Some(2),
            ..Default::default()
        };
        assert_eq!(
            run_plan(planned(Phase::Unit, &TestCfg::default(), &two, false)).shards,
            2
        );
        let zero = TestSettings {
            shards: Some(0),
            ..Default::default()
        };
        assert_eq!(
            run_plan(planned(Phase::Unit, &TestCfg::default(), &zero, false)).shards,
            1
        );
    }

    #[test]
    fn plan_reports_skipped_and_empty_phases() {
        let skip = TestCfg {
            skips: vec!["true".into()],
            ..Default::default()
        };
        assert_eq!(
            planned(Phase::Unit, &skip, &TestSettings::default(), false),
            Selection::Skipped
        );
        let none = TestCfg {
            includes: vec!["**/Nothing.java".into()],
            ..Default::default()
        };
        assert_eq!(
            planned(Phase::Unit, &none, &TestSettings::default(), false),
            Selection::Empty
        );
    }

    #[test]
    fn plan_splits_and_interpolates_the_arg_line() {
        let cfg = TestCfg {
            arg_line: Some("-Xmx${mem}  -ea".into()),
            ..Default::default()
        };
        let sel = plan(
            Phase::Unit,
            &cfg,
            &root_with(&[("mem", "2g")]),
            &files(),
            &TestSettings::default(),
            false,
        );
        assert_eq!(run_plan(sel).jvm_args, ["-Xmx2g", "-ea"]);
    }

    fn resolved_jar(
        sha: Option<&str>,
        jar: Option<&Path>,
        classifier: Option<&str>,
        workspace: bool,
    ) -> Resolved {
        Resolved {
            gav: crate::pom::Gav::new("g", "a", "1"),
            scope: "compile".into(),
            classifier: classifier.map(Into::into),
            depth: 1,
            via: None,
            jar: jar.map(Path::to_path_buf),
            typ: "jar".into(),
            repo: None,
            sha256: sha.map(Into::into),
            workspace,
        }
    }

    #[test]
    fn jar_inputs_use_checksums_and_fall_back_to_paths() {
        let r = |sha, jar, classifier| resolved_jar(sha, jar, classifier, false);
        let got = jar_inputs(&[
            r(Some("h"), Some(Path::new("/x.jar")), None),
            r(None, Some(Path::new("/y.jar")), Some("tests")),
            r(None, None, None),
        ])
        .unwrap();
        assert_eq!(
            got,
            [
                ("g:a:1:".to_string(), "h".to_string()),
                ("g:a:1:tests".to_string(), "/y.jar".to_string())
            ]
        );
    }

    #[test]
    fn a_workspace_module_is_identified_by_its_class_tree() {
        let d = std::env::temp_dir().join(format!("rvn-wsjar-{}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(d.join("A.class"), "one").unwrap();
        let ws = [resolved_jar(None, Some(&d), None, true)];
        let before = jar_inputs(&ws).unwrap();
        assert_eq!(
            jar_inputs(&ws).unwrap(),
            before,
            "same classes, same identity"
        );
        std::fs::write(d.join("A.class"), "two").unwrap();
        assert_ne!(
            jar_inputs(&ws).unwrap(),
            before,
            "changed classes, new identity"
        );
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn pump_prefixes_each_line_and_returns_the_text() {
        let mut out = Vec::new();
        let all = pump(std::io::Cursor::new("one\ntwo\n"), "[1] ", &mut out).unwrap();
        assert_eq!(String::from_utf8(out).unwrap(), "[1] one\n[1] two\n");
        assert_eq!(all, "one\ntwo\n");
    }

    #[test]
    fn run_command_reports_exit_status_and_summary() {
        let mut ok = Command::new("sh");
        ok.args([
            "-c",
            "echo '[ 2 tests found ]'; echo '[ 2 tests successful ]'; echo '[ 0 tests failed ]'",
        ]);
        assert_eq!(
            run_command_writers(ok, "", &mut std::io::sink(), &mut std::io::sink()).unwrap(),
            Outcome {
                success: true,
                summary: Some(testcache::Summary {
                    found: 2,
                    successful: 2,
                    failed: 0
                })
            }
        );
        let mut bad = Command::new("sh");
        bad.args(["-c", "exit 3"]);
        assert_eq!(
            run_command_writers(bad, "", &mut std::io::sink(), &mut std::io::sink()).unwrap(),
            Outcome {
                success: false,
                summary: None
            }
        );
    }

    #[test]
    fn run_command_drains_stderr_too() {
        let mut noisy = Command::new("sh");
        noisy.args([
            "-c",
            "yes 'a line of stderr that is long enough to fill the pipe quickly, well past the 64 KB a pipe holds' | head -n 2000 >&2; echo '[ 1 tests found ]'; echo '[ 1 tests successful ]'; echo '[ 0 tests failed ]'",
        ]);
        let o = run_command_writers(noisy, "", &mut std::io::sink(), &mut std::io::sink()).unwrap();
        assert!(o.success, "a full stderr pipe must not stall the JVM");
        assert_eq!(o.summary.map(|s| s.successful), Some(1));
    }

    #[test]
    fn cache_dir_prefers_flag_then_env_then_config_then_default() {
        let p = Path::new("/proj");
        assert_eq!(
            cache_dir(Some("/f"), Some("/e"), Some("c"), p),
            PathBuf::from("/f")
        );
        assert_eq!(
            cache_dir(None, Some("/e"), Some("c"), p),
            PathBuf::from("/e")
        );
        assert_eq!(
            cache_dir(None, None, Some("c"), p),
            PathBuf::from("/proj/c")
        );
        assert_eq!(
            cache_dir(None, None, None, p),
            PathBuf::from("/proj/target/rayvn-test-cache")
        );
    }

    #[test]
    fn cache_dir_treats_empty_values_as_unset() {
        assert_eq!(
            cache_dir(Some(""), Some(""), Some(""), Path::new("/p")),
            PathBuf::from("/p/target/rayvn-test-cache")
        );
    }

    #[test]
    fn cache_dir_keeps_an_absolute_configured_path() {
        assert_eq!(
            cache_dir(None, None, Some("/ci/cache"), Path::new("/p")),
            PathBuf::from("/ci/cache")
        );
    }

    #[test]
    fn record_names_differ_by_project_and_phase() {
        let a = root_with(&[]);
        let mut b = root_with(&[]);
        b.gav = crate::pom::Gav::new("g", "other", "1");
        assert_eq!(record_name(&a, Phase::Unit), "g.a.unit");
        assert_ne!(record_name(&a, Phase::Unit), record_name(&b, Phase::Unit));
        assert_ne!(
            record_name(&a, Phase::Unit),
            record_name(&a, Phase::Integration)
        );
    }

    #[test]
    fn a_module_has_tests_only_if_it_has_java_test_sources() {
        let d = std::env::temp_dir().join(format!("rvn-hastests-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        assert!(!has_tests(&d).unwrap(), "no src/test/java at all");
        std::fs::create_dir_all(d.join("src/test/java")).unwrap();
        std::fs::create_dir_all(d.join("src/test/resources")).unwrap();
        std::fs::write(d.join("src/test/resources/x.txt"), "").unwrap();
        assert!(!has_tests(&d).unwrap(), "resources alone are not tests");
        std::fs::write(d.join("src/test/java/ATest.java"), "class ATest {}").unwrap();
        assert!(has_tests(&d).unwrap());
        let _ = std::fs::remove_dir_all(&d);
    }
}
