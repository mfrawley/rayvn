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
    Lock(Common),
    /// Download exactly what rayvn.lock lists and verify its checksums (never reads POMs; for CI)
    #[command(after_help = "Example:\n  rvn fetch    restore the locked set on a CI machine")]
    Fetch(Common),
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
        after_help = "Example:\n  rvn build --locked    CI: fail rather than re-resolve\n\nPrints the jar path on stdout."
    )]
    Build(Common),
    /// Compile the tests and run the unit tests (surefire settings), skipping the run when nothing changed
    #[command(
        after_help = "Examples:\n  rvn test --rerun            run the tests even if their inputs are unchanged\n  rvn test --test-cache /ci   keep results in a shared directory\n  rvn test --reports-dir out  write JUnit XML to out/surefire-reports\n\nSee docs/testing.md."
    )]
    Test(TestOpts),
    /// Run the tests, package, then run the integration tests (failsafe settings)
    #[command(after_help = "See docs/testing.md.")]
    Verify(TestOpts),
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
    #[arg(short = 'm', long, value_name = "MODULE")]
    module: Option<String>,
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
    common: Common,
    /// Ignore the test result cache for this run
    #[arg(long)]
    rerun: bool,
    /// Store test results here (also RAYVN_TEST_CACHE, or `cache_dir` under `[test]` in rayvn.toml)
    #[arg(long, value_name = "DIR")]
    test_cache: Option<String>,
    /// Write JUnit XML to <DIR>/surefire-reports and <DIR>/failsafe-reports instead of target/
    #[arg(long, value_name = "DIR")]
    reports_dir: Option<String>,
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
        }
    }

    /// Commands that only read the dependency set act on the module named with `-m` and nothing it depends on.
    pub fn reads_dependencies_only(self) -> bool {
        matches!(self, Cmd::Lock | Cmd::Fetch | Cmd::Resolve | Cmd::Classpath)
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
}

impl Args {
    /// Parses the process arguments; `--help`, `--version` and usage errors print and exit here.
    pub fn from_env() -> Result<Args> {
        Args::from_cli(Cli::parse())
    }

    #[cfg(test)]
    fn parse(args: &[&str]) -> Result<Args> {
        let cli = Cli::try_parse_from(std::iter::once("rvn").chain(args.iter().copied()))?;
        Args::from_cli(cli)
    }

    fn from_cli(cli: Cli) -> Result<Args> {
        let (cmd, common, test, rerun, test_cache, reports_dir) = match cli.command {
            Command::Lock(c) => (Cmd::Lock, c, false, false, None, None),
            Command::Fetch(c) => (Cmd::Fetch, c, false, false, None, None),
            Command::Resolve(c) => (Cmd::Resolve, c, false, false, None, None),
            Command::Classpath(o) => (Cmd::Classpath, o.common, o.test, false, None, None),
            Command::Build(c) => (Cmd::Build, c, false, false, None, None),
            Command::Test(o) => (
                Cmd::Test,
                o.common,
                false,
                o.rerun,
                o.test_cache,
                o.reports_dir,
            ),
            Command::Verify(o) => (
                Cmd::Verify,
                o.common,
                false,
                o.rerun,
                o.test_cache,
                o.reports_dir,
            ),
        };
        if common.cold && cmd == Cmd::Classpath {
            bail!(
                "--cold deletes its cache afterwards, so `classpath` would print paths that no longer exist"
            );
        }
        Ok(Args {
            cmd,
            pom_path: common.pom,
            locked: common.locked,
            refresh: common.cold || common.refresh,
            cold: common.cold,
            test,
            rerun,
            test_cache,
            reports_dir,
            module: common.module,
        })
    }

    pub fn project_dir(&self) -> PathBuf {
        self.pom_path
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("."))
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
    fn module_flag_takes_a_name() {
        assert_eq!(
            parse(&["test", "-m", "core"]).module.as_deref(),
            Some("core")
        );
        assert_eq!(
            parse(&["test", "--module", "core"]).module.as_deref(),
            Some("core")
        );
        assert_eq!(parse(&["test"]).module, None);
        assert!(Args::parse(&["test", "-m"]).is_err());
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
