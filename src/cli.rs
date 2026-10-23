//! The command line: what `rvn --help` and `rvn <command> --help` print, and the parsed form the rest of the
//! program consumes ([`Args`]). Parsing is done by `clap`; each command declares only the flags that mean something
//! for it, so `rvn classpath --rerun` is an error rather than a silently ignored flag.

use anyhow::{Result, bail};
use clap::{Args as ClapArgs, Parser, Subcommand};
use std::path::PathBuf;

pub const BANNER: &str = include_str!("../data/banner.txt");

/// Package version, git commit, build time and a name derived from the commit (stamped by `build.rs`).
pub const VERSION: &str = concat!(
    env!("CARGO_PKG_VERSION"),
    " (",
    env!("RAYVN_COMMIT"),
    ", ",
    env!("RAYVN_BUILT"),
    ") \"",
    env!("RAYVN_NAME"),
    "\""
);

const ENVIRONMENT: &str = "\
Environment:
  RAYVN_LOCAL_REPO   where artifacts are cached
  RAYVN_RO_REPO      read-only cache layers, searched before the network
  RAYVN_TEST_CACHE   where test results are stored (same as --test-cache)
  RAYVN_DEBUG_HTTP   set to 1 to print each request and its status
  RAYVN_NO_BANNER    set to hide the banner

Progress and summaries go to stderr and results to stdout, so output pipes cleanly.
Documentation: README.md, docs/ (routing, credentials, testing, workspace, version ranges, plugins).";

#[derive(Parser, Debug)]
#[command(
    name = "rvn",
    version = VERSION,
    about = "A fast, Maven-compatible build tool. Builds from the existing pom.xml and settings.xml.",
    before_help = BANNER,
    after_help = ENVIRONMENT,
    arg_required_else_help = true,
    subcommand_required = true
)]
pub struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// Resolve the dependencies and write rayvn.lock next to the POM
    #[command(
        after_help = "Example:\n  rvn lock -m api    lock one module of a multi-module build"
    )]
    Lock(Parallel),
    /// Download exactly what rayvn.lock lists and verify its checksums (never reads POMs; for CI)
    #[command(after_help = "Example:\n  rvn fetch    restore the locked set on a CI machine")]
    Fetch(Parallel),
    /// Print the dependency tree, one line per artifact, indented by depth
    #[command(
        after_help = "Example:\n  rvn resolve | grep guava    which version of guava did we get?"
    )]
    Resolve(Common),
    /// Print the classpath as colon-separated jar paths
    #[command(
        after_help = "Example:\n  java -cp \"$(rvn classpath):target/classes\" app.Main\n\nThe classpath is one line, so a multi-module build needs -m."
    )]
    Classpath(ClasspathOpts),
    /// Compile and package the jar into target/ (shaded if the POM configures maven-shade-plugin)
    #[command(
        after_help = "Examples:\n  rvn build --locked    CI: fail rather than re-resolve\n  rvn build -j 4        at most four modules at once\n\nPrints the jar path on stdout."
    )]
    Build(Parallel),
    /// Compile the tests and run the unit tests (surefire settings), skipping the run when nothing changed
    #[command(
        after_help = "Examples:\n  rvn test --rerun            run the tests even if their inputs are unchanged\n  rvn test --test-cache /ci   keep results in a shared directory\n  rvn test --reports-dir out  write JUnit XML to out/<module>/surefire-reports\n\nSee docs/testing.md."
    )]
    Test(TestOpts),
    /// Run the tests, package, then run the integration tests (failsafe settings)
    #[command(after_help = "See docs/testing.md.")]
    Verify(TestOpts),
    /// Stage built artifacts and checksums into a Maven repository layout in target/staging/
    #[command(
        after_help = "Example:\n  rvn stage    stages all built artifacts into target/staging/"
    )]
    Stage(StageOpts),
    /// Stage artifacts and publish them to a remote repository
    #[command(
        after_help = "Example:\n  rvn deploy --dry-run    build and stage all artifacts without uploading"
    )]
    Deploy(DeployOpts),
}

/// Flags every command takes.
#[derive(ClapArgs, Debug)]
struct Common {
    /// Use this POM instead of ./pom.xml; the lockfile goes beside it
    #[arg(
        short = 'f',
        long = "file",
        value_name = "POM",
        default_value = "pom.xml"
    )]
    pom: PathBuf,
    /// Multi-module: this module and what it depends on (just this one for lock, fetch, resolve, classpath)
    #[arg(short = 'm', long, alias = "projects", value_name = "MODULE")]
    module: Option<String>,
    /// Build only the selected module, without upstream dependencies
    #[arg(long)]
    only: bool,
    /// Override the version of the workspace modules (like versions:set -DnewVersion=...)
    #[arg(long = "set-version", value_name = "VERSION")]
    set_version: Option<String>,
    /// Also make upstream dependencies (Maven compatibility alias, enabled by default)
    #[arg(short = 'a', long = "also-make", alias = "am")]
    also_make: bool,
    /// Fail if rayvn.lock is missing or stale, instead of resolving
    #[arg(long)]
    locked: bool,
    /// Ignore rayvn.lock and the miss cache for this run; the lock is rewritten
    #[arg(long)]
    refresh: bool,
    /// Use a throwaway cache, deleted afterwards; implies --refresh
    #[arg(long)]
    cold: bool,
}

/// Common flags plus the number of modules to work on at once.
#[derive(ClapArgs, Debug)]
struct Parallel {
    #[command(flatten)]
    common: Common,
    /// Work on up to N modules at once (default: one per CPU); -j 1 goes one after another
    #[arg(short = 'j', long, value_name = "N", value_parser = clap::value_parser!(u16).range(1..))]
    jobs: Option<u16>,
}

#[derive(ClapArgs, Debug)]
struct ClasspathOpts {
    #[command(flatten)]
    common: Common,
    /// Include test-scope jars
    #[arg(long)]
    test: bool,
}

#[derive(ClapArgs, Debug)]
struct TestOpts {
    #[command(flatten)]
    parallel: Parallel,
    /// Ignore the test result cache for this run
    #[arg(long)]
    rerun: bool,
    /// Store test results here (also RAYVN_TEST_CACHE, or `cache_dir` under `[test]` in rayvn.toml)
    #[arg(long, value_name = "DIR")]
    test_cache: Option<String>,
    /// Write JUnit XML to `<DIR>/surefire-reports` and `<DIR>/failsafe-reports` instead of target/
    #[arg(long, value_name = "DIR")]
    reports_dir: Option<String>,
}

#[derive(ClapArgs, Debug)]
struct StageOpts {
    #[command(flatten)]
    parallel: Parallel,
    /// Directory to stage artifacts into (default: target/staging)
    #[arg(long, value_name = "DIR")]
    stage_dir: Option<PathBuf>,
}

#[derive(ClapArgs, Debug)]
struct DeployOpts {
    #[command(flatten)]
    parallel: Parallel,
    /// Directory to stage artifacts into (default: target/staging)
    #[arg(long, value_name = "DIR")]
    stage_dir: Option<PathBuf>,
    /// Build and stage all artifacts locally, but do not upload to the remote repository
    #[arg(long)]
    dry_run: bool,
    /// Target repository to deploy to, e.g. "github::https://maven.pkg.github.com/org/repo"
    #[arg(long, value_name = "REPO")]
    alt_repo: Option<String>,
}

/// The commands, as the rest of the program names them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Cmd {
    Lock,
    Fetch,
    Resolve,
    Classpath,
    Build,
    Test,
    Verify,
    Stage,
    Deploy,
}

impl Cmd {
    pub fn name(self) -> &'static str {
        match self {
            Cmd::Lock => "lock",
            Cmd::Fetch => "fetch",
            Cmd::Resolve => "resolve",
            Cmd::Classpath => "classpath",
            Cmd::Build => "build",
            Cmd::Test => "test",
            Cmd::Verify => "verify",
            Cmd::Stage => "stage",
            Cmd::Deploy => "deploy",
        }
    }

    /// Commands that only read the dependency set act on the module named with `-m` and nothing it depends on.
    pub fn reads_dependencies_only(self) -> bool {
        matches!(self, Cmd::Lock | Cmd::Fetch | Cmd::Resolve | Cmd::Classpath)
    }

    /// Commands whose modules can be worked on side by side: the others print one module's result after another.
    pub fn overlaps_modules(self) -> bool {
        !matches!(self, Cmd::Resolve | Cmd::Classpath)
    }

    pub fn runs_tests(self) -> bool {
        matches!(self, Cmd::Test | Cmd::Verify)
    }
}

#[derive(Debug, PartialEq)]
pub struct Args {
    pub cmd: Cmd,
    pub pom_path: PathBuf,
    pub locked: bool,
    pub refresh: bool,
    pub cold: bool,
    pub test: bool,
    pub rerun: bool,
    pub test_cache: Option<String>,
    pub reports_dir: Option<String>,
    pub module: Option<String>,
    pub only: bool,
    pub set_version: Option<String>,
    pub stage_dir: Option<PathBuf>,
    pub dry_run: bool,
    pub alt_repo: Option<String>,
    /// `-j`; `None` means one per CPU.
    pub jobs: Option<usize>,
}

impl Args {
    /// Parses the process arguments; `--help`, `--version` and usage errors print and exit here.
    pub fn from_env() -> Result<Args> {
        let args = normalize_args(std::env::args());
        Args::from_cli(Cli::parse_from(args))
    }

    #[cfg(test)]
    fn parse(args: &[&str]) -> Result<Args> {
        let raw = std::iter::once("rvn").chain(args.iter().copied());
        let normalized = normalize_args(raw.map(String::from));
        let cli = Cli::try_parse_from(normalized)?;
        Args::from_cli(cli)
    }

    fn from_cli(cli: Cli) -> Result<Args> {
        let base = |cmd: Cmd, c: Common| Args {
            cmd,
            pom_path: c.pom,
            locked: c.locked,
            refresh: c.cold || c.refresh,
            cold: c.cold,
            test: false,
            rerun: false,
            test_cache: None,
            reports_dir: None,
            module: c.module,
            only: c.only,
            set_version: c.set_version,
            stage_dir: None,
            dry_run: false,
            alt_repo: None,
            jobs: None,
        };
        let with_jobs = |cmd: Cmd, p: Parallel| Args {
            jobs: p.jobs.map(usize::from),
            ..base(cmd, p.common)
        };
        let args = match cli.command {
            Command::Lock(p) => with_jobs(Cmd::Lock, p),
            Command::Fetch(p) => with_jobs(Cmd::Fetch, p),
            Command::Resolve(c) => base(Cmd::Resolve, c),
            Command::Classpath(o) => Args {
                test: o.test,
                ..base(Cmd::Classpath, o.common)
            },
            Command::Build(p) => with_jobs(Cmd::Build, p),
            Command::Test(o) => test_args(Cmd::Test, o, with_jobs),
            Command::Verify(o) => test_args(Cmd::Verify, o, with_jobs),
            Command::Stage(s) => Args {
                stage_dir: s.stage_dir,
                ..with_jobs(Cmd::Stage, s.parallel)
            },
            Command::Deploy(d) => Args {
                stage_dir: d.stage_dir,
                dry_run: d.dry_run,
                alt_repo: d.alt_repo,
                ..with_jobs(Cmd::Deploy, d.parallel)
            },
        };
        if args.cold && args.cmd == Cmd::Classpath {
            bail!(
                "--cold deletes its cache afterwards, so `classpath` would print paths that no longer exist"
            );
        }
        Ok(args)
    }

    pub fn project_dir(&self) -> PathBuf {
        self.pom_path
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("."))
    }
}

fn normalize_args(iter: impl IntoIterator<Item = String>) -> Vec<String> {
    iter.into_iter()
        .map(|s| {
            if s == "-pl" {
                "--projects".to_string()
            } else if let Some(rest) = s.strip_prefix("-pl=") {
                format!("--projects={rest}")
            } else if s == "-am" || s == "-amd" {
                "--also-make".to_string()
            } else {
                s
            }
        })
        .collect()
}

fn test_args(cmd: Cmd, o: TestOpts, with_jobs: impl Fn(Cmd, Parallel) -> Args) -> Args {
    Args {
        rerun: o.rerun,
        test_cache: o.test_cache,
        reports_dir: o.reports_dir,
        ..with_jobs(cmd, o.parallel)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    fn parse(args: &[&str]) -> Args {
        Args::parse(args).unwrap()
    }

    fn help_of(args: &[&str]) -> String {
        let e = Cli::try_parse_from(std::iter::once("rvn").chain(args.iter().copied()))
            .expect_err("--help stops parsing");
        e.to_string()
    }

    #[test]
    fn the_command_definition_is_consistent() {
        Cli::command().debug_assert();
    }

    #[test]
    fn no_or_unknown_command_is_an_error() {
        assert!(Args::parse(&[]).is_err());
        assert!(Args::parse(&["frobnicate"]).is_err());
    }

    #[test]
    fn flags_and_pom_path_are_parsed() {
        let a = parse(&["build", "-f", "sub/pom.xml", "--locked", "--refresh"]);
        assert_eq!(
            (a.cmd, a.locked, a.refresh, a.cold, a.test),
            (Cmd::Build, true, true, false, false)
        );
        assert_eq!(a.project_dir(), PathBuf::from("sub"));
        assert_eq!(parse(&["lock"]).project_dir(), PathBuf::from("."));
        assert_eq!(
            parse(&["lock", "--file", "x/pom.xml"]).pom_path,
            PathBuf::from("x/pom.xml")
        );
    }

    #[test]
    fn cold_implies_refresh_and_is_refused_for_classpath() {
        let a = parse(&["lock", "--cold"]);
        assert!(a.cold && a.refresh);
        assert!(Args::parse(&["classpath", "--cold"]).is_err());
    }

    #[test]
    fn rerun_flag_is_parsed() {
        assert!(parse(&["test", "--rerun"]).rerun);
        assert!(!parse(&["test"]).rerun);
    }

    #[test]
    fn test_cache_flag_takes_a_directory() {
        assert_eq!(
            parse(&["test", "--test-cache", "/ci"])
                .test_cache
                .as_deref(),
            Some("/ci")
        );
        assert_eq!(parse(&["test"]).test_cache, None);
        assert!(Args::parse(&["test", "--test-cache"]).is_err());
    }

    #[test]
    fn reports_dir_flag_takes_a_directory_and_is_for_test_and_verify() {
        assert_eq!(
            parse(&["verify", "--reports-dir", "out"])
                .reports_dir
                .as_deref(),
            Some("out")
        );
        assert_eq!(parse(&["test"]).reports_dir, None);
        assert!(Args::parse(&["test", "--reports-dir"]).is_err());
        assert!(Args::parse(&["build", "--reports-dir", "x"]).is_err());
    }

    #[test]
    fn jobs_is_for_the_commands_that_work_on_many_modules() {
        assert_eq!(parse(&["build", "-j", "4"]).jobs, Some(4));
        assert_eq!(parse(&["test", "--jobs", "2"]).jobs, Some(2));
        assert_eq!(parse(&["lock", "-j", "1"]).jobs, Some(1));
        assert_eq!(parse(&["verify"]).jobs, None);
        assert!(Args::parse(&["build", "-j", "0"]).is_err());
        assert!(Args::parse(&["build", "-j"]).is_err());
        assert!(Args::parse(&["classpath", "-j", "2"]).is_err());
        assert!(Args::parse(&["resolve", "-j", "2"]).is_err());
    }

    #[test]
    fn only_commands_that_print_one_result_per_module_stay_in_order() {
        assert!(Cmd::Build.overlaps_modules() && Cmd::Test.overlaps_modules());
        assert!(Cmd::Lock.overlaps_modules() && Cmd::Fetch.overlaps_modules());
        assert!(!Cmd::Resolve.overlaps_modules() && !Cmd::Classpath.overlaps_modules());
    }

    #[test]
    fn module_flag_takes_a_name() {
        assert_eq!(
            parse(&["test", "-m", "core"]).module.as_deref(),
            Some("core")
        );
        assert_eq!(
            parse(&["test", "--module", "core"]).module.as_deref(),
            Some("core")
        );
        assert_eq!(
            parse(&["test", "-pl", "core"]).module.as_deref(),
            Some("core")
        );
        assert_eq!(
            parse(&["test", "--projects", "core"]).module.as_deref(),
            Some("core")
        );
        assert_eq!(parse(&["test"]).module, None);
        assert!(Args::parse(&["test", "-m"]).is_err());
    }

    #[test]
    fn projects_selection_flags_parse() {
        let a = parse(&[
            "build",
            "-pl",
            "core",
            "--only",
            "-am",
            "--set-version",
            "2.0.0",
        ]);
        assert_eq!(a.module.as_deref(), Some("core"));
        assert!(a.only);
        assert_eq!(a.set_version.as_deref(), Some("2.0.0"));
    }

    #[test]
    fn stage_and_deploy_options_parse() {
        let s = parse(&["stage", "--stage-dir", "out/staging"]);
        assert_eq!(s.cmd, Cmd::Stage);
        assert_eq!(s.stage_dir, Some(PathBuf::from("out/staging")));

        let d = parse(&[
            "deploy",
            "--dry-run",
            "--alt-repo",
            "gh::https://example.com/repo",
        ]);
        assert_eq!(d.cmd, Cmd::Deploy);
        assert!(d.dry_run);
        assert_eq!(d.alt_repo.as_deref(), Some("gh::https://example.com/repo"));
    }

    #[test]
    fn dash_f_needs_a_path() {
        assert!(Args::parse(&["lock", "-f"]).is_err());
    }

    #[test]
    fn the_test_flag_is_for_classpath_only() {
        assert!(parse(&["classpath", "--test"]).test);
        assert!(Args::parse(&["build", "--test"]).is_err());
        assert!(Args::parse(&["build", "--rerun"]).is_err());
    }

    #[test]
    fn a_flag_that_means_nothing_for_a_command_is_rejected() {
        assert!(Args::parse(&["classpath", "--rerun"]).is_err());
        assert!(Args::parse(&["lock", "--test-cache", "x"]).is_err());
    }

    #[test]
    fn top_level_help_lists_every_command_and_the_environment() {
        let h = help_of(&["--help"]);
        for c in [
            "lock",
            "fetch",
            "resolve",
            "classpath",
            "build",
            "test",
            "verify",
            "stage",
            "deploy",
        ] {
            assert!(h.contains(c), "help does not mention `{c}`:\n{h}");
        }
        assert!(h.contains("RAYVN_LOCAL_REPO"));
    }

    #[test]
    fn command_help_describes_that_commands_flags() {
        let h = help_of(&["test", "--help"]);
        assert!(h.contains("--rerun") && h.contains("--test-cache"), "{h}");
        assert!(h.contains("--locked"), "common flags are listed too");
        assert!(h.contains("--jobs"));
        assert!(!help_of(&["resolve", "--help"]).contains("--jobs"));
        assert!(!help_of(&["lock", "--help"]).contains("--rerun"));
    }

    #[test]
    fn version_prints_the_stamped_version_line() {
        let v = help_of(&["--version"]);
        assert!(v.starts_with("rvn "));
        assert!(v.contains(env!("CARGO_PKG_VERSION")));
    }

    #[test]
    fn a_mistyped_command_gets_a_suggestion() {
        let e = Cli::try_parse_from(["rvn", "bild"])
            .unwrap_err()
            .to_string();
        assert!(e.contains("build"), "{e}");
    }

    #[test]
    fn command_names_match_what_the_user_types() {
        for (word, cmd) in [("lock", Cmd::Lock), ("verify", Cmd::Verify)] {
            assert_eq!(parse(&[word]).cmd, cmd);
            assert_eq!(cmd.name(), word);
        }
    }
}
