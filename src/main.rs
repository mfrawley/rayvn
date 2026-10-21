//! The `rvn` command line. All logic lives in the [`rayvn`] library; this file parses arguments and sequences
//! the steps.

use anyhow::{Context, Result, bail};
use rayvn::{build, lock, model, repos, resolve, settings, verify, version};
use std::path::{Path, PathBuf};
use std::time::Instant;

fn main() {
    if let Err(e) = run() {
        eprintln!("error: {e:#}");
        std::process::exit(1);
    }
}

const BANNER: &str = include_str!("../data/banner.txt");

/// Printed to stdout, but only when stdout is a terminal, so piped output (`rvn classpath | ...`) stays clean.
fn print_banner() {
    use std::io::IsTerminal;
    if std::io::stdout().is_terminal() && std::env::var_os("RAYVN_NO_BANNER").is_none() {
        println!("{BANNER}");
        println!("  {}\n", version());
    }
}

const COMMANDS: [&str; 7] = ["lock", "fetch", "resolve", "classpath", "build", "test", "verify"];
const USAGE: &str = "usage: rvn <lock|fetch|resolve|classpath|build|test|verify> [-f pom.xml] [--locked] [--refresh] [--cold] [--rerun] [--test-cache dir]";

#[derive(Debug, PartialEq)]
struct Args {
    cmd: String,
    pom_path: PathBuf,
    locked: bool,
    refresh: bool,
    cold: bool,
    test: bool,
    rerun: bool,
    test_cache: Option<String>,
}

impl Args {
    /// `None` for no or an unknown command (print usage).
    fn parse(args: &[String]) -> Result<Option<Args>> {
        let Some(cmd) = args.first().filter(|c| COMMANDS.contains(&c.as_str())) else { return Ok(None) };
        let flag = |name: &str| args.iter().any(|a| a == name);
        let pom_path = match args.iter().position(|a| a == "-f") {
            Some(i) => PathBuf::from(args.get(i + 1).context("-f needs a path")?),
            None => PathBuf::from("pom.xml"),
        };
        let test_cache = match args.iter().position(|a| a == "--test-cache") {
            Some(i) => Some(args.get(i + 1).context("--test-cache needs a directory")?.clone()),
            None => None,
        };
        let cold = flag("--cold");
        if cold && cmd == "classpath" {
            bail!("--cold deletes its cache afterwards, so `classpath` would print paths that no longer exist");
        }
        Ok(Some(Args { cmd: cmd.clone(), pom_path, locked: flag("--locked"), refresh: cold || flag("--refresh"), cold, test: flag("--test"), rerun: flag("--rerun"), test_cache }))
    }

    fn project_dir(&self) -> PathBuf {
        self.pom_path.parent().filter(|p| !p.as_os_str().is_empty()).map(PathBuf::from).unwrap_or_else(|| PathBuf::from("."))
    }
}

fn cold_cache_dir() -> PathBuf {
    let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_nanos());
    std::env::temp_dir().join(format!("rvn-cold-{}-{nanos}", std::process::id()))
}

/// The dependency set for `args`: from a fresh `rayvn.lock` when there is one, else resolved from the POM
/// (and the lock rewritten). The second value is the root project when it had to be loaded.
fn dependencies(
    args: &Args,
    pom_hash: &str,
    lock_path: &Path,
    repos: &repos::Repos,
    loader: &model::Loader,
) -> Result<(Vec<resolve::Resolved>, Option<model::Effective>)> {
    let fetch = args.cmd == "fetch";
    let fresh = if args.cmd == "lock" || (args.refresh && !fetch) { None } else { lock::read_fresh(lock_path, pom_hash) };
    if let Some(l) = fresh {
        eprintln!("  using rayvn.lock ({} packages)", l.package.len());
        let resolved = lock::realize(&l, repos)?;
        if fetch {
            let (ok, skipped) = lock::verify(&l, &resolved)?;
            eprintln!("  verified {ok} files against rayvn.lock ({skipped} not checkable: no checksum recorded, or SNAPSHOT)");
        }
        return Ok((resolved, None));
    }
    eprintln!("  resolving dependencies");
    if fetch || args.locked {
        bail!("rayvn.lock is missing or out of date; run `rvn lock`");
    }
    let root = loader.from_file(&args.pom_path)?;
    let resolved = resolve::resolve(&root, loader, repos, true)?;
    lock::write(lock_path, pom_hash, &resolved, repos)?;
    repos.clear_progress();
    eprintln!("  wrote {}", lock_path.display());
    Ok((resolved, Some(root)))
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
        .map(|r| format!("{}{}:{}  [{}] {}", "  ".repeat(r.depth - 1), r.gav.group, r.gav.artifact, r.scope, r.gav.version))
        .collect()
}

fn run() -> Result<()> {
    // Downloads block on the network, so allow far more in flight than there are cores.
    rayon::ThreadPoolBuilder::new().num_threads(32).build_global().ok();
    let raw: Vec<String> = std::env::args().skip(1).collect();
    if raw.first().is_some_and(|a| matches!(a.as_str(), "--version" | "-V" | "version")) {
        println!("{}", version());
        return Ok(());
    }
    let Some(args) = Args::parse(&raw)? else {
        println!("{BANNER}");
        println!("{}", version());
        println!("{USAGE}");
        return Ok(());
    };
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

    let pom_bytes = std::fs::read(&args.pom_path).with_context(|| format!("reading {}", args.pom_path.display()))?;
    let pom_hash = lock::hash_bytes(&pom_bytes);
    let repos = repos::Repos::new(&model::local_repos(&args.pom_path)?, &settings, &config, args.refresh, args.cold)?;
    let loader = model::Loader::new(&repos);
    eprintln!("rvn {}: {} ({} repositories, {} routes)", args.cmd, args.pom_path.display(), repos.remote_count(), repos.route_count());

    let (resolved, root) = dependencies(&args, &pom_hash, &project_dir.join("rayvn.lock"), &repos, &loader)?;
    let direct = resolved.iter().filter(|r| r.depth == 1).count();
    eprintln!("  {} artifacts ({direct} direct): {}", resolved.len(), repos.summary());

    match args.cmd.as_str() {
        "lock" | "fetch" => eprintln!("done in {:?}", t0.elapsed()),
        "resolve" => {
            resolve_lines(&resolved).iter().for_each(|l| println!("{l}"));
            eprintln!("done in {:?}", t0.elapsed());
        }
        "classpath" => println!("{}", classpath_line(&resolved, args.test)),
        "build" => {
            let root = match root {
                Some(r) => r,
                None => loader.from_file(&args.pom_path)?,
            };
            if root.packaging != "jar" {
                bail!("packaging '{}' is not supported yet (only jar)", root.packaging);
            }
            build::build(&root, &project_dir, &resolved)?;
            eprintln!("done in {:?}", t0.elapsed());
        }
        "test" | "verify" => {
            let root = match root {
                Some(r) => r,
                None => loader.from_file(&args.pom_path)?,
            };
            if root.packaging != "jar" {
                bail!("packaging '{}' is not supported yet (only jar)", root.packaging);
            }
            let main_classes = build::compile_main(&root, &project_dir, &resolved)?;
            let test_classes = verify::compile_tests(&root, &project_dir, &main_classes, &resolved)?;
            let cx = verify::TestRun {
                root: &root,
                dir: &project_dir,
                main_classes: &main_classes,
                test_classes: &test_classes,
                resolved: &resolved,
                repos: &repos,
                settings: &config.test,
                rerun: args.rerun,
                cache_flag: args.test_cache.as_deref(),
            };
            verify::run(verify::Phase::Unit, &cx)?;
            if args.cmd == "verify" {
                build::package(&root, &project_dir, &main_classes, &resolved)?;
                verify::run(verify::Phase::Integration, &cx)?;
            }
            eprintln!("done in {:?}", t0.elapsed());
        }
        _ => unreachable!(),
    }
    if let Some(d) = cold_dir {
        let _ = std::fs::remove_dir_all(d);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(args: &[&str]) -> Result<Option<Args>> {
        Args::parse(&args.iter().map(|a| a.to_string()).collect::<Vec<_>>())
    }

    #[test]
    fn no_or_unknown_command_means_usage() {
        assert_eq!(parse(&[]).unwrap(), None);
        assert_eq!(parse(&["frobnicate"]).unwrap(), None);
    }

    #[test]
    fn flags_and_pom_path_are_parsed() {
        let a = parse(&["build", "-f", "sub/pom.xml", "--locked", "--refresh"]).unwrap().unwrap();
        assert_eq!((a.cmd.as_str(), a.locked, a.refresh, a.cold, a.test), ("build", true, true, false, false));
        assert_eq!(a.project_dir(), PathBuf::from("sub"));
        assert_eq!(parse(&["lock"]).unwrap().unwrap().project_dir(), PathBuf::from("."));
    }

    #[test]
    fn cold_implies_refresh_and_is_refused_for_classpath() {
        let a = parse(&["lock", "--cold"]).unwrap().unwrap();
        assert!(a.cold && a.refresh);
        assert!(parse(&["classpath", "--cold"]).is_err());
    }

    #[test]
    fn rerun_flag_is_parsed() {
        assert!(parse(&["test", "--rerun"]).unwrap().unwrap().rerun);
        assert!(!parse(&["test"]).unwrap().unwrap().rerun);
    }

    #[test]
    fn test_cache_flag_takes_a_directory() {
        assert_eq!(parse(&["test", "--test-cache", "/ci"]).unwrap().unwrap().test_cache.as_deref(), Some("/ci"));
        assert_eq!(parse(&["test"]).unwrap().unwrap().test_cache, None);
        assert!(parse(&["test", "--test-cache"]).is_err());
    }

    #[test]
    fn dash_f_needs_a_path() {
        assert!(parse(&["lock", "-f"]).is_err());
    }
}
