//! Runs the modules of a build side by side: a module starts once every module it depends on (among those being
//! built) has finished, and at most `jobs` run at a time. With `jobs = 1` the order is exactly the topological order
//! the reactor produced, so a sequential build behaves as it always did.
//!
//! The first failure stops new modules from starting; those already running finish, then the error is returned.

use anyhow::{Result, anyhow};
use std::collections::{BTreeSet, HashMap};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::{Condvar, Mutex};

/// Stack size of a worker; dependency resolution recurses along dependency chains.
const WORKER_STACK: usize = 16 * 1024 * 1024;

struct State<T> {
    /// Positions (in `nodes`) that can start now; the lowest goes first.
    ready: BTreeSet<usize>,
    waiting_on: Vec<usize>,
    results: Vec<Option<T>>,
    running: usize,
    finished: usize,
    error: Option<anyhow::Error>,
    /// Failures after the first, from modules that were already running.
    others: Vec<anyhow::Error>,
}

/// The tasks of a build with `modules` modules: task `2p` compiles the module at position `p` and task `2p + 1`
/// packages it and runs its tests. A module's compile waits for the compiles of the modules it depends on, not for
/// their tests, so tests of one module overlap with the compiling of the next.
pub struct Layout {
    pub deps: Vec<Vec<usize>>,
    /// The tasks in the order they are preferred: with one job a module at a time, in the reactor's order; with
    /// more, every compile first, because that is what keeps other modules from waiting.
    pub order: Vec<usize>,
}

pub const fn compile_task(position: usize) -> usize {
    2 * position
}

pub const fn finish_task(position: usize) -> usize {
    2 * position + 1
}

/// `depends_on[p]` lists the positions (all below `p`) of the modules that the module at `p` depends on.
pub fn layout(depends_on: &[Vec<usize>], jobs: usize) -> Layout {
    let n = depends_on.len();
    let mut deps = vec![vec![]; 2 * n];
    for (p, on) in depends_on.iter().enumerate() {
        deps[compile_task(p)] = on.iter().map(|&q| compile_task(q)).collect();
        deps[finish_task(p)] = vec![compile_task(p)];
    }
    let order = if jobs <= 1 {
        (0..2 * n).collect()
    } else {
        (0..n)
            .map(compile_task)
            .chain((0..n).map(finish_task))
            .collect()
    };
    Layout { deps, order }
}

/// How many modules to work on at once: `requested` (`-j`), else one per CPU, never more than there are modules.
/// Commands that print one module's result after another stay at one.
pub fn job_count(requested: Option<usize>, cpus: usize, overlaps: bool, modules: usize) -> usize {
    if !overlaps {
        return 1;
    }
    requested.unwrap_or(cpus).clamp(1, modules.max(1))
}

/// Runs `work` for every module in `nodes` and returns the results in the order of `nodes`. `deps[i]` lists the
/// modules `i` depends on; any that are not in `nodes` are ignored. `nodes` must already be in dependency order.
pub fn run<T: Send>(
    deps: &[Vec<usize>],
    nodes: &[usize],
    jobs: usize,
    work: impl Fn(usize) -> Result<T> + Sync,
) -> Result<Vec<T>> {
    let position: HashMap<usize, usize> = nodes.iter().enumerate().map(|(p, &n)| (n, p)).collect();
    let mut waiting_on = vec![0; nodes.len()];
    let mut dependents: Vec<Vec<usize>> = vec![vec![]; nodes.len()];
    for (p, &n) in nodes.iter().enumerate() {
        for d in deps[n].iter().filter_map(|d| position.get(d)) {
            waiting_on[p] += 1;
            dependents[*d].push(p);
        }
    }
    let ready = (0..nodes.len()).filter(|&p| waiting_on[p] == 0).collect();
    let state = Mutex::new(State {
        ready,
        waiting_on,
        results: (0..nodes.len()).map(|_| None).collect(),
        running: 0,
        finished: 0,
        error: None,
        others: vec![],
    });
    let changed = Condvar::new();
    let workers = jobs.clamp(1, nodes.len().max(1));
    let worker = || {
        loop {
            let task = {
                let mut s = state.lock().unwrap();
                loop {
                    if s.error.is_some() || s.finished == nodes.len() {
                        return;
                    }
                    if let Some(p) = s.ready.pop_first() {
                        s.running += 1;
                        break p;
                    }
                    if s.running == 0 {
                        s.error = Some(anyhow!("modules depend on each other in a cycle"));
                        changed.notify_all();
                        return;
                    }
                    s = changed.wait(s).unwrap();
                }
            };
            let outcome = catch_unwind(AssertUnwindSafe(|| work(nodes[task])))
                .unwrap_or_else(|_| Err(anyhow!("module task panicked")));
            let mut s = state.lock().unwrap();
            s.running -= 1;
            match outcome {
                Ok(v) => {
                    s.results[task] = Some(v);
                    s.finished += 1;
                    for &d in &dependents[task] {
                        s.waiting_on[d] -= 1;
                        if s.waiting_on[d] == 0 {
                            s.ready.insert(d);
                        }
                    }
                }
                Err(e) => match s.error {
                    None => s.error = Some(e),
                    Some(_) => s.others.push(e),
                },
            }
            changed.notify_all();
        }
    };
    std::thread::scope(|scope| -> Result<()> {
        for n in 0..workers {
            std::thread::Builder::new()
                .name(format!("rvn-module-{n}"))
                .stack_size(WORKER_STACK)
                .spawn_scoped(scope, worker)?;
        }
        Ok(())
    })?;
    let s = state.into_inner().unwrap();
    for e in &s.others {
        eprintln!("also failed: {e:#}");
    }
    if let Some(e) = s.error {
        return Err(e);
    }
    Ok(s.results.into_iter().flatten().collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;
    use std::time::Duration;

    /// app (0) needs lib (1) and util (2); lib needs util.
    fn chain() -> Vec<Vec<usize>> {
        vec![vec![1, 2], vec![2], vec![]]
    }

    #[test]
    fn one_job_follows_the_given_order() {
        let seen = Mutex::new(vec![]);
        let out = run(&chain(), &[2, 1, 0], 1, |n| {
            seen.lock().unwrap().push(n);
            Ok(n * 10)
        })
        .unwrap();
        assert_eq!(*seen.lock().unwrap(), [2, 1, 0]);
        assert_eq!(out, [20, 10, 0], "results come back in the order of nodes");
    }

    #[test]
    fn a_module_starts_only_after_the_modules_it_depends_on_have_finished() {
        let log = Mutex::new(vec![]);
        run(&chain(), &[2, 1, 0], 4, |n| {
            log.lock().unwrap().push(format!("start {n}"));
            std::thread::sleep(Duration::from_millis(20));
            log.lock().unwrap().push(format!("end {n}"));
            Ok(())
        })
        .unwrap();
        let log = log.into_inner().unwrap();
        let at = |s: &str| log.iter().position(|l| l == s).unwrap();
        assert!(at("end 2") < at("start 1"));
        assert!(at("end 1") < at("start 0"));
    }

    #[test]
    fn independent_modules_run_at_the_same_time() {
        let (tx_a, rx_a) = mpsc::channel();
        let (tx_b, rx_b) = mpsc::channel();
        let (rx_a, rx_b) = (Mutex::new(rx_a), Mutex::new(rx_b));
        let deps = vec![vec![], vec![]];
        let result = run(&deps, &[0, 1], 2, |n| {
            // Each side announces itself and waits for the other: only possible if both run at once.
            let (tx, rx) = if n == 0 {
                (&tx_b, &rx_a)
            } else {
                (&tx_a, &rx_b)
            };
            tx.send(()).unwrap();
            rx.lock()
                .unwrap()
                .recv_timeout(Duration::from_secs(5))
                .map_err(|_| anyhow!("the other module never started"))
        });
        assert!(result.is_ok(), "{result:?}");
    }

    #[test]
    fn no_more_than_jobs_modules_run_at_once() {
        let deps = vec![vec![]; 6];
        let nodes: Vec<usize> = (0..6).collect();
        let (now, peak) = (Mutex::new(0), Mutex::new(0));
        run(&deps, &nodes, 2, |_| {
            let n = {
                let mut now = now.lock().unwrap();
                *now += 1;
                *now
            };
            {
                let mut peak = peak.lock().unwrap();
                *peak = (*peak).max(n);
            }
            std::thread::sleep(Duration::from_millis(15));
            *now.lock().unwrap() -= 1;
            Ok(())
        })
        .unwrap();
        assert_eq!(*peak.lock().unwrap(), 2);
    }

    #[test]
    fn a_module_compiles_after_the_compiles_of_its_dependencies_and_finishes_after_its_own_compile()
    {
        // 0 <- 1 <- 2, and 2 also needs 0.
        let l = layout(&[vec![], vec![0], vec![0, 1]], 4);
        assert_eq!(l.deps[compile_task(0)], Vec::<usize>::new());
        assert_eq!(l.deps[compile_task(2)], [compile_task(0), compile_task(1)]);
        assert_eq!(l.deps[finish_task(1)], [compile_task(1)]);
    }

    #[test]
    fn one_job_goes_module_by_module_and_more_jobs_compile_first() {
        let on = [vec![], vec![0]];
        assert_eq!(layout(&on, 1).order, [0, 1, 2, 3]);
        assert_eq!(layout(&on, 2).order, [0, 2, 1, 3]);
    }

    #[test]
    fn the_tests_of_a_module_do_not_hold_up_the_compile_of_the_next() {
        let l = layout(&[vec![], vec![0]], 2);
        let log = Mutex::new(vec![]);
        run(&l.deps, &l.order, 2, |t| {
            log.lock().unwrap().push(format!("start {t}"));
            if t == finish_task(0) {
                std::thread::sleep(Duration::from_millis(100));
            }
            log.lock().unwrap().push(format!("end {t}"));
            Ok(())
        })
        .unwrap();
        let log = log.into_inner().unwrap();
        let at = |s: String| log.iter().position(|l| *l == s).unwrap();
        assert!(at(format!("end {}", compile_task(1))) < at(format!("end {}", finish_task(0))));
    }

    #[test]
    fn job_count_is_the_request_or_the_cpus_within_the_number_of_modules() {
        assert_eq!(job_count(Some(4), 8, true, 24), 4);
        assert_eq!(job_count(None, 8, true, 24), 8);
        assert_eq!(
            job_count(None, 8, true, 3),
            3,
            "no more workers than modules"
        );
        assert_eq!(job_count(Some(0), 8, true, 3), 1);
        assert_eq!(
            job_count(Some(4), 8, false, 24),
            1,
            "resolve and classpath stay in order"
        );
        assert_eq!(job_count(None, 8, true, 0), 1);
    }

    #[test]
    fn a_failure_stops_new_modules_and_is_returned() {
        let started = Mutex::new(vec![]);
        let err = run(&chain(), &[2, 1, 0], 2, |n| {
            started.lock().unwrap().push(n);
            if n == 1 {
                Err(anyhow!("lib failed"))
            } else {
                Ok(())
            }
        })
        .unwrap_err();
        assert_eq!(err.to_string(), "lib failed");
        assert!(
            !started.lock().unwrap().contains(&0),
            "app depends on the module that failed"
        );
    }

    #[test]
    fn dependencies_outside_the_selection_are_ignored() {
        let out = run(&chain(), &[0], 2, Ok).unwrap();
        assert_eq!(out, [0]);
    }

    #[test]
    fn a_panicking_module_is_an_error_not_a_hang() {
        let err = run(&chain(), &[2, 1, 0], 2, |n| -> Result<()> {
            if n == 2 {
                panic!("boom");
            }
            Ok(())
        })
        .unwrap_err();
        assert!(err.to_string().contains("panicked"), "{err}");
    }

    #[test]
    fn nothing_to_do_is_fine() {
        assert!(run(&[], &[], 4, |_| Ok(())).unwrap().is_empty());
    }

    #[test]
    fn a_cycle_is_an_error_not_a_hang() {
        let deps = vec![vec![1], vec![0]];
        let err = run(&deps, &[0, 1], 2, |_| Ok(())).unwrap_err();
        assert!(err.to_string().contains("cycle"), "{err}");
    }
}
