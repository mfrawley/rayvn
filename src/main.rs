//! The `rvn` command line: parses arguments and sequences the steps. See the README for usage.
//!
//! # Architecture
//!
//! `rvn` is a thin command-line wrapper (`src/main.rs`) over the modules below. `cli` defines the commands, flags
//! and `--help` text (with `clap`). A run goes through these modules in order:
//!
//! 1. `settings`: read `settings.xml`, `~/.rayvn/config.toml` and `rayvn.toml` (repositories, mirrors,
//!    credentials, routes, hints).
//! 2. `pom` and `model`: parse `pom.xml`, then build the effective model (parents, BOM imports,
//!    properties, dependency management).
//! 3. `reactor`: for a POM with `<modules>`, find the modules, order them by their dependencies on each other and
//!    resolve a sibling to its build output; each module is then locked and built on its own.
//! 4. `resolve`: walk the dependency graph with Maven's mediation rules, fetching POMs through `repos`.
//! 5. `repos`: ask repositories (routed, hinted, cached, with a miss cache) and keep the local cache;
//!    bookkeeping lives in one file, see `index`.
//! 6. `lock`: write or read `rayvn.lock`, which pins versions, repositories and checksums.
//! 7. `schedule` and `log`: modules are worked on side by side, each starting when the modules it depends on are
//!    compiled; `log` prefixes the lines of each module so that the output stays readable.
//! 8. `build`: compile with `javac` and package the jar (shaded when the POM asks for it).
//! 9. `verify`, `testcache` and `reports`: compile the tests and run them with JUnit, selected as surefire and
//!    failsafe would, sharded across JVMs and skipped when nothing they depend on has changed; the results are
//!    written as JUnit XML where CI systems expect them.
//!
//! Credentials never appear in these types as plain config values; `secret` resolves them from the
//! environment or a helper command at the moment of use.

use anyhow::{Context, Result, bail};
use cli::{Args, Cmd};
use log::say;
use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::OnceLock;
use std::time::Instant;

mod build;
mod cli;
mod deploy;
mod index;
mod lock;
mod log;
mod model;
mod pom;
mod reactor;
mod reports;
mod repos;
mod resolve;
mod schedule;
mod secret;
mod settings;
mod stage;
mod testcache;
mod verify;
mod version;

fn main() {
    if let Err(e) = run() {
        eprintln!("error: {e:#}");
        std::process::exit(1);
    }
}

/// Printed to stdout, but only when stdout is a terminal, so piped output (`rvn classpath | ...`) stays clean.
fn print_banner() {
    use std::io::IsTerminal;
    if std::io::stdout().is_terminal() && std::env::var_os("RAYVN_NO_BANNER").is_none() {
        println!("{}", cli::BANNER);
        println!("  rvn {}\n", cli::VERSION);
    }
}

fn cold_cache_dir() -> PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    std::env::temp_dir().join(format!("rvn-cold-{}-{nanos}", std::process::id()))
}

/// The dependency set of `module` for `args`: from its fresh `rayvn.lock` when there is one, else resolved from the
/// POM (and the lock rewritten). `lock_hash` covers the module's POMs and those of the modules it depends on.
fn dependencies(
    args: &Args,
    module: &reactor::Module,
    lock_hash: &str,
    repos: &repos::Repos,
    loader: &model::Loader,
) -> Result<Vec<resolve::Resolved>> {
    let lock_path = module.lock_path();
    let fetch = args.cmd == Cmd::Fetch;
    let fresh = if args.cmd == Cmd::Lock || (args.refresh && !fetch) {
        None
    } else {
        lock::read_fresh(&lock_path, lock_hash)
    };
    if let Some(l) = fresh {
        say!("  using rayvn.lock ({} packages)", l.package.len());
        let resolved = lock::realize(&l, repos, loader.locals())?;
        if fetch {
            let (ok, skipped) = lock::verify(&l, &resolved)?;
            say!(
                "  verified {ok} files against rayvn.lock ({skipped} not checkable: no checksum recorded, SNAPSHOT, or a module of this build)"
            );
        }
        return Ok(resolved);
    }
    say!("  resolving dependencies");
    if fetch || args.locked {
        bail!(
            "{} is missing or out of date; run `rvn lock`",
            lock_path.display()
        );
    }
    let resolved = resolve::resolve(&module.effective, loader, repos, true)?;
    lock::write(&lock_path, lock_hash, &resolved, repos)?;
    repos.clear_progress();
    say!("  wrote {}", lock_path.display());
    Ok(resolved)
}

/// Colon-separated jar paths; test-scoped ones only if `include_test`.
fn classpath_line(resolved: &[resolve::Resolved], include_test: bool) -> String {
    let jars: Vec<String> = resolved
        .iter()
        .filter(|r| r.scope != "test" || include_test)
        .filter_map(|r| r.jar.as_ref().map(|p| p.display().to_string()))
        .collect();
    jars.join(":")
}

/// One line per artifact, indented by depth.
fn resolve_lines(resolved: &[resolve::Resolved]) -> Vec<String> {
    resolved
        .iter()
        .map(|r| {
            format!(
                "{}{}:{}  [{}] {}",
                "  ".repeat(r.depth - 1),
                r.gav.group,
                r.gav.artifact,
                r.scope,
                r.gav.version
            )
        })
        .collect()
}

/// Plugin executions that Maven would run during this build and rayvn does not.
fn unrun_warnings(root: &model::Effective) -> Vec<String> {
    root.unrun
        .iter()
        .map(|u| format!("warning: {}: Maven runs {u} during the build; rayvn does not, so the output may differ (docs/plugins.md)", root.gav.artifact))
        .collect()
}

fn warn_unrun(root: &model::Effective) {
    unrun_warnings(root).iter().for_each(|w| say!("{w}"));
}

/// What the first half of the work on a module leaves for the second.
#[derive(Default)]
struct Built {
    resolved: Vec<resolve::Resolved>,
    /// `None` when nothing was compiled: `lock`, `fetch`, `resolve`, `classpath`, or a module `rvn test` skips.
    main_classes: Option<PathBuf>,
    test_classes: Option<PathBuf>,
}

/// What the second half produced for a module.
#[derive(Default)]
struct Done {
    /// Result lines, printed on stdout once all modules are done so that their order does not depend on timing.
    stdout: Vec<String>,
    outcomes: Vec<verify::TestOutcome>,
}

/// Does `cmd` compile the tests of a module? `build` does where another module uses them (a `test-jar`
/// dependency); `test` and `verify` where the module has tests.
fn compiles_tests(cmd: Cmd, provides_tests: bool, has_tests: bool) -> bool {
    if cmd == Cmd::Build {
        provides_tests
    } else {
        cmd.runs_tests() && has_tests
    }
}

/// Everything the work on one module reads, shared by all modules being worked on.
struct Ctx<'a> {
    args: &'a Args,
    modules: &'a [reactor::Module],
    edges: &'a [Vec<usize>],
    repos: &'a repos::Repos,
    loader: &'a model::Loader<'a>,
    config: &'a settings::Config,
    /// Modules with at least one file under `src/test/java`.
    with_tests: HashSet<usize>,
    /// Modules whose test classes another module uses.
    test_providers: HashSet<usize>,
    /// Modules `rvn test` has to build: those with tests and what they need.
    needed: HashSet<usize>,
    /// Modules are worked on side by side, so their output is told apart by a prefix and the cumulative
    /// download summary is left to the end.
    overlapping: bool,
}

impl Ctx<'_> {
    /// First half: the module's dependencies, then its classes (and test classes where they are used). Other modules
    /// can start as soon as this is done.
    fn prepare(&self, i: usize) -> Result<Built> {
        let (args, module) = (self.args, &self.modules[i]);
        if module.is_aggregator() {
            return Ok(Built::default());
        }
        if args.cmd == Cmd::Test && !self.needed.contains(&i) {
            say!("  no tests here, and no module with tests depends on it: skipped");
            return Ok(Built::default());
        }
        let lock_hash = lock::fingerprint(&reactor::lock_inputs(self.modules, self.edges, i))?;
        let resolved = dependencies(args, module, &lock_hash, self.repos, self.loader)?;
        let direct = resolved.iter().filter(|r| r.depth == 1).count();
        if self.overlapping {
            say!("  {} artifacts ({direct} direct)", resolved.len());
        } else {
            say!(
                "  {} artifacts ({direct} direct): {}",
                resolved.len(),
                self.repos.summary()
            );
        }
        let mut built = Built {
            resolved,
            ..Built::default()
        };
        if matches!(
            args.cmd,
            Cmd::Build | Cmd::Test | Cmd::Verify | Cmd::Stage | Cmd::Deploy
        ) {
            let (root, dir) = (&module.effective, module.dir.as_path());
            if root.packaging == "pom" {
                return Ok(built);
            }
            if root.packaging != "jar" {
                bail!(
                    "packaging '{}' is not supported yet (only jar and pom)",
                    root.packaging
                );
            }
            warn_unrun(root);
            let main = build::compile_main(root, dir, &built.resolved)?;
            if compiles_tests(
                args.cmd,
                self.test_providers.contains(&i),
                self.with_tests.contains(&i),
            ) {
                built.test_classes =
                    Some(verify::compile_tests(root, dir, &main, &built.resolved)?);
            }
            built.main_classes = Some(main);
        }
        Ok(built)
    }

    /// Second half: package the jar and run the tests, as the command asks.
    fn finish(&self, i: usize, built: &Built) -> Result<Done> {
        let (args, module) = (self.args, &self.modules[i]);
        let (root, dir) = (&module.effective, module.dir.as_path());
        let mut done = Done::default();
        match args.cmd {
            Cmd::Lock | Cmd::Fetch => {}
            Cmd::Resolve => done.stdout = resolve_lines(&built.resolved),
            Cmd::Classpath => done.stdout = vec![classpath_line(&built.resolved, args.test)],
            Cmd::Build | Cmd::Test | Cmd::Verify | Cmd::Stage | Cmd::Deploy => {
                if module.is_aggregator() || root.packaging == "pom" {
                    if matches!(args.cmd, Cmd::Build | Cmd::Stage | Cmd::Deploy) {
                        let pom = build::package_pom(root, dir)?;
                        done.stdout.push(pom.display().to_string());
                    }
                    return Ok(done);
                }
                let Some(main) = &built.main_classes else {
                    return Ok(done);
                };
                let package = |done: &mut Done| -> Result<()> {
                    let jar = build::package(root, dir, main, &built.resolved)?;
                    done.stdout.push(jar.display().to_string());
                    Ok(())
                };
                if matches!(args.cmd, Cmd::Build | Cmd::Stage | Cmd::Deploy) {
                    package(&mut done)?;
                    return Ok(done);
                }
                let Some(test) = &built.test_classes else {
                    if args.cmd == Cmd::Verify {
                        package(&mut done)?;
                    }
                    return Ok(done);
                };
                let run = verify::TestRun {
                    root,
                    dir,
                    main_classes: main,
                    test_classes: test,
                    resolved: &built.resolved,
                    repos: self.repos,
                    settings: &self.config.test,
                    rerun: args.rerun,
                    cache_flag: args.test_cache.as_deref(),
                    reports_flag: args.reports_dir.as_deref(),
                };
                done.outcomes.push(verify::run(verify::Phase::Unit, &run)?);
                if args.cmd == Cmd::Verify {
                    package(&mut done)?;
                    done.outcomes
                        .push(verify::run(verify::Phase::Integration, &run)?);
                }
            }
        }
        Ok(done)
    }
}

/// `[name] ` padded to `width`, so that the lines of modules building side by side line up.
fn line_prefix(name: &str, width: usize) -> String {
    format!("[{name:<width$}] ")
}

/// The closing table of a multi-module `test` or `verify`: one line per module and a total.
fn test_report(rows: &[(String, Vec<verify::TestOutcome>)]) -> Vec<String> {
    use verify::TestOutcome::*;
    let (mut ran, mut cached, mut modules_with_tests) = (0, 0, 0);
    let mut lines = vec!["tests by module:".to_string()];
    for (name, outcomes) in rows {
        let (r, c) = outcomes.iter().fold((0, 0), |(r, c), o| match o {
            Passed(n) => (r + n, c),
            Cached(n) => (r, c + n),
            NoTests => (r, c),
        });
        if r + c == 0 {
            continue;
        }
        modules_with_tests += 1;
        ran += r;
        cached += c;
        lines.push(format!(
            "  {name}: {} passed{}",
            r + c,
            if c > 0 {
                format!(" ({c} cached)")
            } else {
                String::new()
            }
        ));
    }
    let silent = rows.len() - modules_with_tests;
    lines.push(format!("  total: {} passed in {modules_with_tests} modules ({cached} cached); {silent} modules have no tests", ran + cached));
    lines
}

fn stage_size(b: u64) -> String {
    if b >= 1 << 20 {
        format!("{:.1} MB", b as f64 / 1048576.0)
    } else {
        format!("{} KB", b / 1024)
    }
}

/// The modules `args` acts on, in build order, aggregators left out. Commands that only read dependencies
/// (`lock`, `fetch`, `resolve`, `classpath`) take just the module named with `-m`; the others also build what it needs.
fn pick(args: &Args, modules: &[reactor::Module]) -> Result<Vec<usize>> {
    let only_named = args.cmd.reads_dependencies_only() || args.only;
    let chosen = match (&args.module, only_named) {
        (Some(m), true) => vec![reactor::find(modules, m)?],
        (m, _) => reactor::select(modules, m.as_deref())?,
    };
    let builds_or_stages = matches!(args.cmd, Cmd::Build | Cmd::Stage | Cmd::Deploy);
    let chosen: Vec<usize> = chosen
        .into_iter()
        .filter(|&i| builds_or_stages || !modules[i].is_aggregator())
        .collect();
    if chosen.is_empty() {
        bail!(
            "{} lists modules but none of them builds anything",
            args.pom_path.display()
        );
    }
    Ok(chosen)
}

fn run() -> Result<()> {
    let threads = std::env::var("RAYON_NUM_THREADS")
        .or_else(|_| std::env::var("RAYVN_MAX_THREADS"))
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(32);
    rayon::ThreadPoolBuilder::new()
        .num_threads(threads)
        .build_global()
        .ok();
    let args = Args::from_env()?;
    print_banner();
    let project_dir = args.project_dir();
    let t0 = Instant::now();
    let (mut settings, _) = settings::load()?;
    let cold_dir = args.cold.then(cold_cache_dir);
    if let Some(d) = &cold_dir {
        settings.local_repo = Some(d.clone());
        eprintln!("rvn: cold run, cache {} (deleted afterwards)", d.display());
    }
    let (config, _) = settings::load_config(&project_dir)?;

    let scan = reactor::scan(&args.pom_path, &config.workspace.exclude)?;
    for g in &scan.unused {
        eprintln!("warning: [workspace] exclude '{g}' matches no module");
    }
    let found = scan.found;
    let declared: Vec<_> = found
        .iter()
        .flat_map(|f| f.poms.repos.iter().cloned())
        .collect();
    let repos = repos::Repos::new(&declared, &settings, &config, args.refresh, args.cold)?;
    let mut loader = model::Loader::new(&repos);
    let modules = reactor::load(found, &loader)?;
    for m in &modules {
        loader.register_local(m.effective.clone(), &m.dir);
    }
    let edges = reactor::dependency_edges(&modules);
    let selected: Vec<usize> = pick(&args, &modules)?;
    let many = selected.len() > 1;
    if many && args.cmd == Cmd::Classpath {
        bail!(
            "`classpath` prints one line; choose a module with -m ({})",
            selected
                .iter()
                .map(|&i| modules[i].name())
                .collect::<Vec<_>>()
                .join(", ")
        );
    }
    eprintln!(
        "rvn {}: {} ({} repositories, {} routes{})",
        args.cmd.name(),
        args.pom_path.display(),
        repos.remote_count(),
        repos.route_count(),
        if many {
            format!(", {} modules", selected.len())
        } else {
            String::new()
        }
    );
    let with_tests: HashSet<usize> = if args.cmd.runs_tests() {
        selected
            .iter()
            .copied()
            .filter_map(|i| match verify::has_tests(&modules[i].dir) {
                Ok(true) => Some(Ok(i)),
                Ok(false) => None,
                Err(e) => Some(Err(e)),
            })
            .collect::<Result<_>>()?
    } else {
        HashSet::new()
    };
    let cpus = std::thread::available_parallelism().map_or(1, |n| n.get());
    let jobs = schedule::job_count(args.jobs, cpus, args.cmd.overlaps_modules(), selected.len());
    let cx = Ctx {
        args: &args,
        modules: &modules,
        edges: &edges,
        repos: &repos,
        loader: &loader,
        config: &config,
        needed: reactor::needed_for_tests(&edges, &selected, &with_tests),
        with_tests,
        test_providers: reactor::test_class_providers(&modules, &selected),
        overlapping: jobs > 1,
    };
    if cx.overlapping {
        repos.stop_progress();
    }
    let position: std::collections::HashMap<usize, usize> =
        selected.iter().enumerate().map(|(p, &i)| (i, p)).collect();
    let depends_on: Vec<Vec<usize>> = selected
        .iter()
        .map(|&i| {
            edges[i]
                .iter()
                .filter_map(|d| position.get(d).copied())
                .collect()
        })
        .collect();
    let layout = schedule::layout(&depends_on, jobs);
    let width = selected
        .iter()
        .map(|&i| modules[i].name().len())
        .max()
        .unwrap_or(0);
    let built: Vec<OnceLock<Built>> = selected.iter().map(|_| OnceLock::new()).collect();
    let results = schedule::run(&layout.deps, &layout.order, jobs, |task| {
        let (p, compiling) = (task / 2, task % 2 == 0);
        let i = selected[p];
        let name = modules[i].name();
        let prefix = cx.overlapping.then(|| line_prefix(name, width));
        let outcome = log::with_prefix(prefix, || {
            if compiling {
                if many && !cx.overlapping {
                    say!("==> {name}");
                }
                let b = cx.prepare(i)?;
                let _ = built[p].set(b);
                Ok(Done::default())
            } else {
                cx.finish(i, built[p].get().context("module was not prepared")?)
            }
        });
        if many {
            outcome.with_context(|| format!("module {name}"))
        } else {
            outcome
        }
    })?;
    let mut finished: Vec<Done> = layout.order.iter().map(|_| Done::default()).collect();
    for (done, &task) in results.into_iter().zip(&layout.order) {
        finished[task] = done;
    }
    let mut test_rows = vec![];
    for (p, &i) in selected.iter().enumerate() {
        let done = std::mem::take(&mut finished[schedule::finish_task(p)]);
        done.stdout.iter().for_each(|l| println!("{l}"));
        test_rows.push((modules[i].name().to_string(), done.outcomes));
    }
    if cx.overlapping {
        say!("{}", repos.summary());
    }
    if many && args.cmd.runs_tests() {
        test_report(&test_rows)
            .iter()
            .for_each(|l| eprintln!("{l}"));
    }
    if matches!(args.cmd, Cmd::Stage | Cmd::Deploy) {
        let stage_dir = args
            .stage_dir
            .clone()
            .unwrap_or_else(|| project_dir.join("target/staging"));
        eprintln!("rvn: staging artifacts into {}", stage_dir.display());
        let mut all_staged = vec![];
        for &i in &selected {
            let m = &modules[i];
            let staged = stage::stage_module(&m.effective, &m.dir, &stage_dir)?;
            all_staged.extend(staged);
        }
        let total_size: u64 = all_staged.iter().map(|s| s.size).sum();
        eprintln!(
            "rvn: staged {} artifacts ({}) in {}",
            all_staged.len(),
            stage_size(total_size),
            stage_dir.display()
        );
        for s in &all_staged {
            let rel = s.file.strip_prefix(&stage_dir).unwrap_or(&s.file);
            eprintln!("  {}", rel.display());
        }
        if args.cmd == Cmd::Deploy && !args.dry_run {
            let root_effective = &modules[selected[0]].effective;
            let target = deploy::resolve_target_repo(&args, &config, root_effective)?;
            let auth = deploy::resolve_deploy_auth(&target, &config, &settings)?;

            let default_max_per_host = std::env::var("RAYVN_MAX_PER_HOST")
                .ok()
                .and_then(|s| s.parse().ok())
                .or(config.max_per_host)
                .unwrap_or(4);
            let mut custom_limits = std::collections::HashMap::new();
            for (h, mc) in &config.hosts {
                let key = repos::host_of(h);
                custom_limits
                    .entry(key)
                    .and_modify(|existing: &mut usize| *existing = (*existing).min(*mc))
                    .or_insert(*mc);
            }
            let limiter = repos::HostLimiter::new(default_max_per_host, custom_limits);

            deploy::upload_staged(&stage_dir, &target, auth.as_ref(), &limiter)?;
        }
    }
    eprintln!("done in {:?}", t0.elapsed());
    if let Some(d) = cold_dir {
        let _ = std::fs::remove_dir_all(d);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_test_report_lists_modules_with_tests_and_counts_the_rest() {
        use verify::TestOutcome::*;
        let rows = vec![
            ("a".to_string(), vec![Passed(3)]),
            ("b".to_string(), vec![NoTests]),
            ("c".to_string(), vec![Cached(4), Passed(1)]),
        ];
        assert_eq!(
            test_report(&rows),
            [
                "tests by module:",
                "  a: 3 passed",
                "  c: 5 passed (4 cached)",
                "  total: 8 passed in 2 modules (4 cached); 1 modules have no tests"
            ]
        );
    }

    #[test]
    fn tests_are_compiled_where_they_run_or_where_another_module_uses_them() {
        assert!(compiles_tests(Cmd::Build, true, false));
        assert!(
            !compiles_tests(Cmd::Build, false, true),
            "build runs nothing"
        );
        assert!(compiles_tests(Cmd::Test, false, true));
        assert!(compiles_tests(Cmd::Verify, true, true));
        assert!(
            !compiles_tests(Cmd::Test, true, false),
            "no sources, nothing to compile"
        );
        assert!(!compiles_tests(Cmd::Lock, true, true));
    }

    #[test]
    fn line_prefixes_are_padded_to_the_longest_module_name() {
        assert_eq!(line_prefix("core", 7), "[core   ] ");
        assert_eq!(line_prefix("invenium", 8), "[invenium] ");
    }

    #[test]
    fn unrun_plugins_produce_one_warning_each_naming_the_module() {
        let mut e = model::Effective {
            gav: pom::Gav::new("g", "mod", "1"),
            parent: None,
            packaging: "jar".into(),
            props: Default::default(),
            managed: Default::default(),
            deps: vec![],
            repos: vec![],
            shade: None,
            compiler: None,
            surefire: None,
            failsafe: None,
            relocation: None,
            distribution: None,
            unrun: vec!["exec-maven-plugin (exec at generate-sources)".into()],
        };
        let w = unrun_warnings(&e);
        assert_eq!(w.len(), 1);
        assert!(
            w[0].contains("mod")
                && w[0].contains("exec-maven-plugin (exec at generate-sources)")
                && w[0].contains("docs/plugins.md"),
            "{}",
            w[0]
        );
        e.unrun.clear();
        assert!(unrun_warnings(&e).is_empty());
    }
}
