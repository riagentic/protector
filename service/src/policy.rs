//! The decision layer: group processes into trees, score how much each tree
//! threatens stability, and decide whether the machine is approaching a freeze
//! and which tree (if any) to remove.
//!
//! Philosophy (inherited from the Python watchdog): high CPU is not a problem
//! and never triggers a kill. The only thing that freezes a machine is memory
//! exhaustion and swap thrashing — measured directly by PSI and available RAM.

use std::collections::{HashMap, HashSet};

use crate::config::Config;
use crate::kernel::Episode;
use crate::model::{ProcView, Risk, Signal, System, Trigger};
use crate::proc::ProcInfo;

/// Rolling per-tree state (by root pid) used for growth and CPU averages.
#[derive(Clone, Copy, Debug, Default)]
pub struct EmaState {
    pub start: u64,
    pub rss: f64,
    pub grow: f64,
    pub cpu: f64,
}

#[derive(Clone)]
pub struct TreeStats {
    pub root: i32,
    pub name: String,
    pub cmd: String,
    pub uid: u32,
    pub protected: bool,
    pub killable: bool,
    pub members: usize,
    pub kill_members: Vec<i32>,
    /// What an automatic kill removes instead of the whole tree: the lowest
    /// subtree holding at least half of the tree's memory / of its CPU use.
    pub mem_job: Job,
    pub cpu_job: Job,
    /// The most RAM one member Protector may not signal (protected, another
    /// user's) holds resident — what a kill elsewhere would not cure.
    pub untouchable: u64,
    pub rss: u64,
    pub threads: u64,
    pub d_state: usize,
    pub oom_score: i64,
    pub cpu_core: f64,
    pub cpu_machine: f64,
    pub age_s: f64,
    pub rss_pct: f64,
    pub grow_bps: f64,
    pub harm: u32,
    pub signals: Vec<Signal>,
}

/// One killable subtree inside a tree.
#[derive(Clone, Debug, Default)]
pub struct Job {
    pub pid: i32,
    pub name: String,
    pub cmd: String,
    pub members: Vec<i32>,
    pub rss: u64,
    pub rss_pct: f64,
}

pub fn human_bytes(b: u64) -> String {
    const KB: f64 = 1024.0;
    let b = b as f64;
    if b >= KB * KB * KB {
        format!("{:.1} GB", b / (KB * KB * KB))
    } else if b >= KB * KB {
        format!("{:.0} MB", b / (KB * KB))
    } else if b >= KB {
        format!("{:.0} KB", b / KB)
    } else {
        format!("{b:.0} B")
    }
}

fn clampf(v: f64, lo: f64, hi: f64) -> f64 {
    v.max(lo).min(hi)
}

/// Map a "higher is worse" value to 0–100 risk: 0 at zero, 50 at `warn`, 100 at
/// `kill`, and above `kill` stays 100.
fn ramp(v: f64, warn: f64, kill: f64) -> f64 {
    if kill <= warn {
        return if v >= kill { 100.0 } else { 0.0 };
    }
    if v >= kill {
        100.0
    } else if v >= warn {
        50.0 + 50.0 * (v - warn) / (kill - warn)
    } else {
        50.0 * (v / warn).max(0.0)
    }
}

fn is_protected(pid: i32, comm: &str, prefixes: &[String], protected_pids: &HashSet<i32>) -> bool {
    pid == 1
        || protected_pids.contains(&pid)
        || prefixes.iter().any(|p| comm.starts_with(p.as_str()))
}

/// The one job that is the problem, not the shell, terminal or agents around
/// it: walk down from the root into the heaviest child for as long as that
/// child's subtree outweighs the process we stand on and still weighs at least
/// `floor` (so the job stays big enough to matter — a fan-out of small workers
/// is one job at their parent). A `wrapper` (a shell, or whatever hosts a
/// protected process) is never where the walk ends while it has a child with
/// any weight. Returns (pid, its subtree).
pub fn focus(
    root: i32,
    kids: &HashMap<i32, Vec<i32>>,
    wrappers: &HashSet<i32>,
    floor: u64,
    weigh: &dyn Fn(&[i32]) -> u64,
) -> (i32, Vec<i32>) {
    let subtree = |top: i32| -> Vec<i32> {
        let mut seen = HashSet::from([top]);
        let mut out = vec![top];
        let mut i = 0;
        while i < out.len() {
            for k in kids.get(&out[i]).into_iter().flatten() {
                if seen.insert(*k) {
                    out.push(*k);
                }
            }
            i += 1;
        }
        out
    };
    let mut cur = root;
    for _ in 0..64 {
        let own = weigh(&[cur]);
        let heaviest = kids
            .get(&cur)
            .into_iter()
            .flatten()
            .map(|k| (weigh(&subtree(*k)), *k))
            .max();
        match heaviest {
            Some((w, kid)) if w > 0 && (wrappers.contains(&cur) || (w >= floor && w >= own)) => {
                cur = kid
            }
            _ => break,
        }
    }
    (cur, subtree(cur))
}

/// Group every process into a tree keyed by its top-most ancestor that is the
/// same user and not a protected (system/desktop) process. This is what makes a
/// job's whole sub-process fan-out count — and die — together.
#[allow(clippy::too_many_arguments)]
pub fn analyze(
    procs: &[ProcInfo],
    cfg: &Config,
    prefixes: &[String],
    protected_pids: &HashSet<i32>,
    me_uid: u32,
    nproc: usize,
    total_ram: u64,
    now: f64,
    dt: f64,
    clk: u64,
    prev_ticks: &HashMap<i32, u64>,
    ema: &mut HashMap<i32, EmaState>,
    psi_mem_some60: f64,
) -> Vec<TreeStats> {
    let index: HashMap<i32, &ProcInfo> = procs.iter().map(|p| (p.pid, p)).collect();

    // root pid -> member pids
    let mut groups: HashMap<i32, Vec<i32>> = HashMap::new();
    for p in procs {
        let mut cur = p.pid;
        let mut hops = 0;
        while hops < 64 {
            let cp = match index.get(&cur) {
                Some(c) => *c,
                None => break,
            };
            if cp.ppid <= 1 {
                break;
            }
            let par = match index.get(&cp.ppid) {
                Some(x) => *x,
                None => break,
            };
            if par.uid != cp.uid {
                break;
            }
            if is_protected(par.pid, &par.comm, prefixes, protected_pids) {
                break;
            }
            cur = par.pid;
            hops += 1;
        }
        groups.entry(cur).or_default().push(p.pid);
    }

    let uptime = crate::proc::read_uptime();
    let mut trees: Vec<TreeStats> = Vec::with_capacity(groups.len());
    for (root_pid, member_pids) in groups.iter() {
        let root = match index.get(root_pid) {
            Some(r) => *r,
            None => continue,
        };
        let mut rss = 0u64;
        let mut hosts: HashSet<i32> = HashSet::new();
        let mut tick_delta = 0u64;
        let mut threads = 0u64;
        let mut d_state = 0usize;
        let mut oom_score = 0i64;
        let mut kill_members = Vec::new();
        let mut any_killable = false;
        let mut krss: HashMap<i32, u64> = HashMap::new();
        let mut kshm: HashMap<i32, u64> = HashMap::new();
        let mut untouchable = 0u64;
        let mut kticks: HashMap<i32, u64> = HashMap::new();
        let mut kids: HashMap<i32, Vec<i32>> = HashMap::new();

        for pid in member_pids {
            let p = match index.get(pid) {
                Some(p) => *p,
                None => continue,
            };
            rss += p.rss;
            let burnt = prev_ticks
                .get(pid)
                .map_or(0, |old| p.ticks.saturating_sub(*old));
            tick_delta += burnt;
            threads += p.threads;
            if p.state == 'D' {
                d_state += 1;
            }
            oom_score = oom_score.max(p.oom_score);
            let pk = p.uid == me_uid && !is_protected(p.pid, &p.comm, prefixes, protected_pids);
            if pk {
                any_killable = true;
                kill_members.push(p.pid);
                krss.insert(p.pid, p.held);
                kshm.insert(p.pid, p.shmem);
                kticks.insert(p.pid, burnt);
            } else {
                untouchable = untouchable.max(p.held - p.swap + p.shmem);
                // Whatever sits above a protected process hosts it: killing the
                // shell above an agent or a terminal hangs the session up.
                let mut up = p.ppid;
                while let Some(par) = index.get(&up).filter(|_| hosts.insert(up)) {
                    if up == *root_pid {
                        break;
                    }
                    up = par.ppid;
                }
            }
            if pid != root_pid {
                kids.entry(p.ppid).or_default().push(p.pid);
            }
        }
        for host in &hosts {
            krss.remove(host);
            kticks.remove(host);
        }
        kill_members.retain(|p| !hosts.contains(p));
        let wrappers: HashSet<i32> = member_pids
            .iter()
            .filter(|pid| {
                index
                    .get(pid)
                    .is_some_and(|p| crate::config::WRAPPERS.contains(&p.comm.as_str()))
            })
            .chain(&hosts)
            .copied()
            .collect();
        // Private memory adds up; a shared segment is held once however many
        // members map it.
        let memory = |pids: &[i32]| -> u64 {
            let own = pids.iter().filter(|p| krss.contains_key(p));
            own.clone().filter_map(|p| krss.get(p)).sum::<u64>()
                + own.filter_map(|p| kshm.get(p)).max().copied().unwrap_or(0)
        };
        let burn = |pids: &[i32]| -> u64 { pids.iter().filter_map(|p| kticks.get(p)).sum() };
        let job = |weigh: &dyn Fn(&[i32]) -> u64, floor: u64| {
            let (pid, subtree) = focus(*root_pid, &kids, &wrappers, floor, weigh);
            let members: Vec<i32> = subtree
                .into_iter()
                .filter(|p| krss.contains_key(p))
                .collect();
            let top = index.get(&pid).copied().unwrap_or(root);
            let rss = memory(&members);
            Job {
                pid,
                name: top.comm.clone(),
                cmd: top.cmd.clone(),
                members,
                rss,
                rss_pct: if total_ram > 0 {
                    100.0 * rss as f64 / total_ram as f64
                } else {
                    0.0
                },
            }
        };
        let mem_floor = (cfg.min_victim_rss_pct / 100.0 * total_ram as f64) as u64;
        let cpu_floor = kticks.values().sum::<u64>().div_ceil(2);
        let (mem_job, cpu_job) = (job(&memory, mem_floor), job(&burn, cpu_floor));

        let root_protected = is_protected(root.pid, &root.comm, prefixes, protected_pids);
        let root_killable = !root_protected && root.uid == me_uid;
        let killable = root_killable && any_killable && !(cfg.only_user && root.uid != me_uid);

        let cpu_core = if dt > 0.0 && clk > 0 {
            100.0 * tick_delta as f64 / clk as f64 / dt
        } else {
            0.0
        };

        // Growth / CPU averages over ~30 s.
        let alpha = if dt > 0.0 {
            1.0 - (-dt / 30.0).exp()
        } else {
            0.0
        };
        let st = ema.entry(root.pid).or_insert(EmaState {
            start: root.starttime,
            rss: rss as f64,
            grow: 0.0,
            cpu: cpu_core,
        });
        if st.start != root.starttime {
            // pid reuse: start over.
            *st = EmaState {
                start: root.starttime,
                rss: rss as f64,
                grow: 0.0,
                cpu: cpu_core,
            };
        }
        let instant_grow = if dt > 0.0 {
            ((rss as f64 - st.rss) / dt).max(0.0)
        } else {
            0.0
        };
        st.grow = st.grow * (1.0 - alpha) + instant_grow * alpha;
        st.rss = rss as f64;
        st.cpu = st.cpu * (1.0 - alpha) + cpu_core * alpha;
        let grow_bps = st.grow;
        let cpu_avg = st.cpu;

        let rss_pct = if total_ram > 0 {
            100.0 * rss as f64 / total_ram as f64
        } else {
            0.0
        };
        let age_s = if clk > 0 && now > 0.0 {
            // starttime is in ticks since boot; convert to age using uptime.
            (uptime - root.starttime as f64 / clk as f64).max(0.0)
        } else {
            0.0
        };

        trees.push(TreeStats {
            root: root.pid,
            name: root.comm.clone(),
            cmd: root.cmd.clone(),
            uid: root.uid,
            protected: root_protected,
            killable,
            members: member_pids.len(),
            kill_members,
            mem_job,
            cpu_job,
            untouchable,
            rss,
            threads,
            d_state,
            oom_score,
            cpu_core,
            cpu_machine: cpu_avg / nproc as f64,
            age_s,
            rss_pct,
            grow_bps,
            harm: 0,
            signals: Vec::new(),
        });
    }

    // Score in a second pass so "relative hog" is measured against the biggest
    // tree on the machine, not an absolute number that is meaningless on a box
    // with a lot of RAM.
    let largest_rss = trees.iter().map(|t| t.rss).max().unwrap_or(0);
    for t in trees.iter_mut() {
        score_tree(t, cfg, psi_mem_some60, largest_rss);
    }

    ema.retain(|root, _| groups.contains_key(root));
    trees.sort_by(|a, b| b.harm.cmp(&a.harm).then_with(|| b.rss.cmp(&a.rss)));
    trees
}

/// Fill `harm` and `signals` for one tree.
///
/// Harm is the worst of several normalized components: absolute memory danger
/// (how close to the kill threshold), relative hogging (share of the largest
/// tree), memory growth, CPU, attributed memory pressure, OOM score and
/// D-state tasks. On a machine with plenty of RAM the biggest app therefore
/// scores around 45 — "the top hog, not yet dangerous" — and only real pressure
/// pushes it toward 100.
fn score_tree(t: &mut TreeStats, cfg: &Config, psi_mem_some60: f64, largest_rss: u64) {
    let rss_pct = t.rss_pct;
    let grow_mb_s = t.grow_bps / 1e6;
    let rel = if largest_rss > 0 {
        t.rss as f64 / largest_rss as f64
    } else {
        0.0
    };

    let mem_abs = clampf(rss_pct / cfg.rss_tree_kill.max(1.0), 0.0, 1.0);
    let mem_rel = clampf(rel, 0.0, 1.0);
    let mem = 100.0 * (0.55 * mem_abs + 0.45 * mem_rel);
    let grow = 100.0 * clampf(grow_mb_s / cfg.growth_kill_mbps.max(0.01), 0.0, 1.0);
    let cpu = 100.0 * clampf(t.cpu_machine / cfg.cpu_sys_kill.max(1.0), 0.0, 1.0);
    let oom = 12.0 * clampf(t.oom_score.clamp(0, 1000) as f64 / 1000.0, 0.0, 1.0);
    let pressure = (100.0 * psi_mem_some60 / cfg.psi_mem_some_kill.max(1.0))
        * clampf(rss_pct / cfg.rss_tree_warn.max(1.0), 0.0, 1.0);
    let dpen = if t.d_state > 0 {
        (t.d_state as f64 * 10.0).min(30.0)
    } else {
        0.0
    };

    t.harm = clampf(
        mem.max(grow).max(cpu).max(oom).max(pressure).max(dpen),
        0.0,
        100.0,
    ) as u32;

    // ── signals ──────────────────────────────────────────────────────
    let mut signals: Vec<Signal> = Vec::new();
    if rss_pct >= cfg.rss_tree_kill {
        signals.push(sig(
            "crit",
            "mem",
            format!("RSS {rss_pct:.0}% of RAM ({})", human_bytes(t.rss)),
        ));
    } else if rss_pct >= cfg.rss_tree_warn {
        signals.push(sig(
            "danger",
            "mem",
            format!("RSS {rss_pct:.0}% of RAM ({})", human_bytes(t.rss)),
        ));
    }
    if grow_mb_s >= cfg.growth_danger_mbps {
        signals.push(sig(
            "danger",
            "grow",
            format!("growing {grow_mb_s:.0} MB/s"),
        ));
    } else if grow_mb_s >= cfg.growth_warn_mbps {
        signals.push(sig("warn", "grow", format!("growing {grow_mb_s:.0} MB/s")));
    }
    if t.cpu_core >= 400.0 {
        signals.push(sig(
            "danger",
            "cpu",
            format!("CPU {:.0}% ({:.1} cores)", t.cpu_core, t.cpu_core / 100.0),
        ));
    } else if t.cpu_core >= 100.0 {
        signals.push(sig(
            "warn",
            "cpu",
            format!("CPU {:.0}% ({:.1} cores)", t.cpu_core, t.cpu_core / 100.0),
        ));
    }
    if t.oom_score >= 900 {
        signals.push(sig("warn", "oom", format!("OOM score {}", t.oom_score)));
    }
    if t.d_state > 0 {
        signals.push(sig(
            if t.d_state >= cfg.d_state_crit {
                "danger"
            } else {
                "warn"
            },
            "dstate",
            format!("{} task(s) stuck in D-state", t.d_state),
        ));
    }
    if t.members >= 40 {
        signals.push(sig(
            "info",
            "procs",
            format!("{} processes in tree", t.members),
        ));
    }
    if t.protected {
        signals.push(sig(
            "info",
            "protected",
            "protected (system process)".to_string(),
        ));
    } else if !t.killable {
        signals.push(sig(
            "info",
            "other-user",
            "other user — never signalled".to_string(),
        ));
    }
    t.signals = signals;
}

fn sig(severity: &str, key: &str, text: String) -> Signal {
    Signal {
        severity: severity.to_string(),
        key: key.to_string(),
        text,
    }
}

/// Public projection of a tree for the wire.
pub fn to_view(t: &TreeStats, nproc: usize) -> ProcView {
    ProcView {
        pid: t.root,
        name: t.name.clone(),
        cmd: t.cmd.clone(),
        uid: t.uid,
        user: String::new(),
        protected: t.protected,
        killable: t.killable,
        rss: t.rss,
        rss_pct: t.rss_pct,
        cpu_core: t.cpu_core,
        cpu_machine: t.cpu_machine,
        grow_bps: t.grow_bps,
        threads: t.threads,
        d_state: t.d_state,
        oom_score: t.oom_score,
        age_s: t.age_s,
        nproc,
        members: t.members,
        harm: t.harm,
        signals: t.signals.clone(),
    }
}

/// Sustained-condition tracker: true only after `cond` has held continuously for
/// `secs`, then consumed so it must re-arm.
pub fn sustained(
    hold: &mut HashMap<String, f64>,
    key: &str,
    cond: bool,
    now: f64,
    secs: f64,
) -> bool {
    if !cond {
        hold.remove(key);
        return false;
    }
    let since = *hold.entry(key.to_string()).or_insert(now);
    if now - since >= secs {
        hold.remove(key);
        true
    } else {
        false
    }
}

pub struct Evaluated {
    pub risk: Risk,
    pub signals: Vec<Signal>,
    pub trigger: Trigger,
}

/// What a desktop alert should say: every signal serious enough to interrupt
/// the user. A busy CPU is only ever "warn", so load alone never alerts.
pub fn alert(signals: &[Signal]) -> Option<String> {
    let v: Vec<&str> = signals
        .iter()
        .filter(|s| s.severity == "danger" || s.severity == "crit")
        .map(|s| s.text.as_str())
        .collect();
    (!v.is_empty()).then(|| v.join("\n"))
}

/// Last resort for a stuck kernel: true once the episode is `stall_reboot_s`
/// old, the kernel reported a stall within the window, and tasks are still
/// stuck right now. A stall that cleared by itself never reboots.
pub fn should_reboot(k: &Episode, d_state: usize, now: f64, cfg: &Config) -> bool {
    cfg.stall_reboot_s > 0.0
        && now - k.start >= cfg.stall_reboot_s
        && k.last_stall.is_some_and(|t| now - t <= cfg.kernel_window_s)
        && d_state >= 3
}

/// Decide the system's state and whether a kill is warranted right now.
#[allow(clippy::too_many_arguments)]
pub fn evaluate(
    s: &System,
    trees: &[TreeStats],
    k: Option<&Episode>,
    cfg: &Config,
    now: f64,
    hold: &mut HashMap<String, f64>,
    cooldown_left: f64,
    mode_armed: bool,
) -> Evaluated {
    let mut signals: Vec<Signal> = Vec::new();
    let k_kind = k.map(|k| {
        if k.last_stall.is_some() {
            "stall"
        } else {
            "fault"
        }
    });

    // Real danger is no way out, not a big number. Tasks must be waiting on
    // memory right now, for the whole machine (`stalling`), and either RAM and
    // swap are both spent (`exhausted`) or RAM is scarce and everything has
    // been at a standstill a third of the time (`gridlock`). A full swap with
    // RAM to spare is idle pages parked on disk; a big load swapping out with
    // RAM or swap left is the kernel coping; pressure without machine-wide
    // reclaim is one cgroup at its own limit. None of those may kill.
    // An unreadable /proc/meminfo reads as all zero: never act on it.
    let blind = s.mem_total == 0;
    let scarce = s.mem_available_pct < cfg.mem_avail_warn;
    let stalling = !blind
        && s.direct_reclaim
        && (s.psi.mem_some10 > cfg.psi_mem_some_warn || s.psi.mem_full10 > cfg.psi_mem_full_warn);
    let thrash = scarce && stalling;
    let swap_spent = s.swap_total == 0 || s.swap_used_pct > cfg.swap_panic;
    let exhausted = stalling && swap_spent && s.mem_available_pct < cfg.mem_avail_kill;
    let gridlock = thrash && s.psi.mem_full10 > cfg.psi_mem_full_panic;

    // ── system WARN signals (informational) ──────────────────────────
    if s.mem_available_pct < cfg.mem_avail_warn {
        signals.push(sig(
            if s.mem_available_pct < cfg.mem_avail_kill {
                "crit"
            } else {
                "warn"
            },
            "memavail",
            format!("available RAM {:.0}%", s.mem_available_pct),
        ));
    }
    if s.swap_used_pct > cfg.swap_warn {
        let (severity, note) = match (thrash, s.swap_used_pct > cfg.swap_kill) {
            (true, true) => ("crit", ""),
            (true, false) => ("warn", ""),
            (false, _) => ("info", " — RAM to spare"),
        };
        signals.push(sig(
            severity,
            "swap",
            format!("swap {:.1}% used{note}", s.swap_used_pct),
        ));
    }
    // Pressure with RAM to spare is a stutter, not a freeze: shown, never a
    // desktop alert (2026-10-08…14: eight pop-ups at 29–44% available).
    let loud = |over_kill: bool| if over_kill && scarce { "crit" } else { "warn" };
    if s.psi.mem_some60 > cfg.psi_mem_some_warn {
        signals.push(sig(
            loud(s.psi.mem_some60 > cfg.psi_mem_some_kill),
            "psi_mem_some",
            format!("memory PSI some60 {:.0}", s.psi.mem_some60),
        ));
    }
    if s.psi.mem_full10 > cfg.psi_mem_full_warn {
        signals.push(sig(
            loud(s.psi.mem_full10 > cfg.psi_mem_full_kill),
            "psi_mem_full",
            format!("memory PSI full10 {:.0}", s.psi.mem_full10),
        ));
    }
    if s.cpu_busy_pct > cfg.cpu_sys_warn {
        signals.push(sig(
            "warn",
            "cpu",
            format!("CPU busy {:.0}%", s.cpu_busy_pct),
        ));
    }
    // The OS cannot breathe: this daemon, asleep between samples, is itself
    // woken seconds late while every core is busy. Fair sharing of a loaded
    // CPU (builds, tests, models) never does that — only a hostile hog does.
    let starved = !blind
        && s.sampler_lag_s > cfg.starve_lag_s
        && s.cpu_busy_pct >= cfg.cpu_sys_kill
        && s.psi.cpu_some60 > cfg.psi_cpu_some_kill;
    if starved {
        signals.push(sig(
            "danger",
            "starved",
            format!("CPU starved — samples {:.0}s late", s.sampler_lag_s),
        ));
    }
    if s.d_state >= 3 {
        signals.push(sig(
            if s.d_state >= cfg.d_state_crit {
                "danger"
            } else {
                "warn"
            },
            "dstate",
            format!("{} task(s) stuck in D-state", s.d_state),
        ));
    }
    if let (Some(k), Some(kind)) = (k, k_kind) {
        signals.push(sig(
            "crit",
            "kernel",
            format!("kernel {kind}: {} — no process to kill", k.text),
        ));
    }
    if s.psi.io_full10 > 10.0 {
        signals.push(sig(
            "warn",
            "psi_io",
            format!("io PSI full10 {:.0}", s.psi.io_full10),
        ));
    }
    if s.psi.cpu_some60 > 25.0 {
        signals.push(sig(
            "info",
            "psi_cpu",
            format!("CPU PSI some60 {:.0}", s.psi.cpu_some60),
        ));
    }

    // ── risk score: how close the machine is to a kill ──────────────
    // Pressure without scarcity tops out at WATCH; swap counts only while RAM
    // is scarce and tasks are stalling.
    let cap = |r: f64| if scarce { r } else { r.min(60.0) };
    let r_mem = ramp(
        100.0 - s.mem_available_pct,
        100.0 - cfg.mem_avail_warn,
        100.0 - cfg.mem_avail_kill,
    );
    let r_swap = if thrash {
        ramp(s.swap_used_pct, cfg.swap_warn, cfg.swap_kill)
    } else {
        0.0
    };
    let r_ps = cap(ramp(
        s.psi.mem_some60,
        cfg.psi_mem_some_warn,
        cfg.psi_mem_some_kill,
    ));
    let r_pf = cap(ramp(
        s.psi.mem_full10,
        cfg.psi_mem_full_warn,
        cfg.psi_mem_full_kill,
    ));
    let r_cpu = ramp(s.cpu_busy_pct, cfg.cpu_sys_warn, cfg.cpu_sys_kill) * 0.6;
    let r_d = ramp(s.d_state as f64, 3.0, cfg.d_state_crit.max(4) as f64) * 0.6;
    let r_k = if k.is_some() { 90.0 } else { 0.0 };
    let score = clampf(
        r_mem
            .max(r_swap)
            .max(r_ps)
            .max(r_pf)
            .max(r_cpu)
            .max(r_d)
            .max(r_k),
        0.0,
        100.0,
    ) as u32;

    // ── KILL stage ───────────────────────────────────────────────────
    // Every window is fed every tick, so it restarts the moment its condition
    // clears.
    let rules = [
        (
            "panic",
            exhausted && s.mem_available_pct < cfg.mem_avail_panic,
            cfg.panic_sustain,
            "PANIC: RAM and swap spent",
        ),
        (
            "exhausted",
            exhausted && s.psi.mem_full10 > cfg.psi_mem_full_warn,
            cfg.pressure_kill_sustain,
            "RAM and swap nearly spent",
        ),
        (
            "gridlock",
            gridlock,
            cfg.mem_avail_kill_sustain,
            "RAM scarce and everything stalled",
        ),
    ]
    .map(|(key, cond, secs, why)| (sustained(hold, key, cond, now, secs), secs, why));
    let choked = sustained(hold, "starved", starved, now, cfg.cpu_sys_kill_sustain);
    let panic = rules[0].0;
    let reason = rules.iter().find(|r| r.0).map(|(_, secs, why)| {
        format!(
            "{why} for {secs:.0}s (available RAM {:.1}%, memory PSI some10 {:.0} some60 {:.0} full10 {:.0}, swap {:.0}%)",
            s.mem_available_pct,
            s.psi.mem_some10,
            s.psi.mem_some60,
            s.psi.mem_full10,
            s.swap_used_pct
        )
    });
    let cpu_reason = (reason.is_none() && choked).then(|| {
        format!(
            "CPU starved for {:.0}s (busy {:.0}%, CPU PSI some60 {:.0}, samples {:.0}s late)",
            cfg.cpu_sys_kill_sustain, s.cpu_busy_pct, s.psi.cpu_some60, s.sampler_lag_s
        )
    });

    let status = if panic {
        "PANIC"
    } else if score >= 85 {
        "CRITICAL"
    } else if score >= 45 {
        "WATCH"
    } else {
        "OK"
    };

    let mut reasons: Vec<String> = Vec::new();
    if r_mem >= 45.0 {
        reasons.push(format!("available RAM {:.0}%", s.mem_available_pct));
    }
    if r_swap >= 45.0 {
        reasons.push(format!("swap {:.0}%", s.swap_used_pct));
    }
    if r_ps >= 45.0 {
        reasons.push(format!("memory PSI some60 {:.0}", s.psi.mem_some60));
    }
    if r_pf >= 45.0 {
        reasons.push(format!("memory PSI full10 {:.0}", s.psi.mem_full10));
    }
    if r_cpu >= 45.0 {
        reasons.push(format!("CPU busy {:.0}%", s.cpu_busy_pct));
    }
    if r_d >= 45.0 {
        reasons.push(format!("{} tasks in D-state", s.d_state));
    }
    if let Some(kind) = k_kind {
        reasons.push(format!("kernel {kind}"));
    }

    // ── victim ───────────────────────────────────────────────────────
    // Starved: the job burning at least half of every core. Memory: the job
    // holding the most — unless one process Protector may not touch holds more
    // RAM; then the cause is elsewhere and it holds rather than kill a
    // bystander. In a panic any big job goes. With RAM and swap spent by many
    // small jobs (twenty agents' test runs), a fifth of the size floor will do.
    let untouchable = trees.iter().map(|t| t.untouchable).max().unwrap_or(0);
    let min_pct = if exhausted {
        cfg.min_victim_rss_pct / 5.0
    } else {
        cfg.min_victim_rss_pct
    };
    let victim: Option<&Job> = if cpu_reason.is_some() {
        trees
            .iter()
            .filter(|t| t.killable && !t.cpu_job.members.is_empty())
            .max_by(|a, b| a.cpu_machine.total_cmp(&b.cpu_machine))
            .filter(|t| t.cpu_machine >= cfg.min_victim_cpu_pct)
            .map(|t| &t.cpu_job)
    } else {
        trees
            .iter()
            .filter(|t| t.killable && !t.mem_job.members.is_empty())
            .filter(|t| t.mem_job.rss_pct >= min_pct)
            .max_by_key(|t| t.mem_job.rss)
            .filter(|t| panic || t.mem_job.rss >= untouchable)
            .map(|t| &t.mem_job)
    };
    let reason = reason.or(cpu_reason);

    let mut trigger = Trigger::idle();
    if let Some(r) = reason {
        match victim {
            Some(v) => {
                trigger = Trigger {
                    active: true,
                    reason: Some(r.clone()),
                    panic,
                    victim_pid: Some(v.pid),
                    victim_name: Some(v.name.clone()),
                };
                signals.push(sig(
                    if panic { "crit" } else { "danger" },
                    "trigger",
                    format!("TRIGGER: {r} → {} ({})", v.name, v.pid),
                ));
            }
            None => {
                let txt = format!(
                    "freeze signal ({r}) but no job of ours is to blame and large enough — holding"
                );
                signals.push(sig("warn", "hold", txt));
                trigger = Trigger {
                    active: true,
                    reason: Some(r),
                    panic,
                    victim_pid: None,
                    victim_name: None,
                };
            }
        }
        // In observe mode a real trigger is still reported, but as a would-do.
        if !mode_armed && trigger.active {
            if let Some(t) = signals.last_mut() {
                t.text = format!("[observe] {}", t.text);
            }
        }
    }

    if !panic && cooldown_left > 0.0 && trigger.active {
        signals.push(sig(
            "info",
            "cooldown",
            format!("cooldown {:.0}s left", cooldown_left),
        ));
    }

    let risk = Risk {
        score,
        status: status.to_string(),
        reasons,
    };
    Evaluated {
        risk,
        signals,
        trigger,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Psi;

    fn sys(
        mem_available_pct: f64,
        swap_used_pct: f64,
        psi_some60: f64,
        psi_full10: f64,
        cpu_busy_pct: f64,
        largest_tree_pct: f64,
        d_state: usize,
    ) -> System {
        System {
            mem_total: 100,
            mem_avail: 100,
            mem_available_pct,
            mem_used_pct: 100.0 - mem_available_pct,
            swap_total: 100,
            swap_used: swap_used_pct as u64,
            swap_used_pct,
            cpu_busy_pct,
            nproc: 32,
            load1: 0.0,
            load5: 0.0,
            load15: 0.0,
            psi: Psi {
                mem_some10: psi_some60,
                mem_some60: psi_some60,
                mem_full10: psi_full10,
                ..Psi::default()
            },
            d_state,
            sampler_lag_s: 0.0,
            direct_reclaim: true,
            procs: 1,
            trees: 1,
            threads: 1,
            largest_tree_pct,
        }
    }

    fn tree(root: i32, rss: u64, rss_pct: f64, killable: bool) -> TreeStats {
        let job = Job {
            pid: root,
            name: format!("p{root}"),
            cmd: format!("p{root}"),
            members: if killable { vec![root] } else { vec![] },
            rss,
            rss_pct,
        };
        TreeStats {
            root,
            name: format!("p{root}"),
            cmd: format!("p{root}"),
            uid: 1000,
            protected: false,
            killable,
            members: 1,
            kill_members: if killable { vec![root] } else { vec![] },
            mem_job: job.clone(),
            cpu_job: job,
            untouchable: if killable { 0 } else { rss },
            rss,
            threads: 1,
            d_state: 0,
            oom_score: 0,
            cpu_core: 0.0,
            cpu_machine: 0.0,
            age_s: 1.0,
            rss_pct,
            grow_bps: 0.0,
            harm: 0,
            signals: Vec::new(),
        }
    }

    /// Hold one state for `secs`, sampling every second; the last trigger
    /// that fired, if any did.
    fn after(s: &System, trees: &[TreeStats], cfg: &Config, secs: u32) -> Trigger {
        let mut hold = HashMap::new();
        (0..=secs)
            .map(|t| evaluate(s, trees, None, cfg, t as f64, &mut hold, 0.0, true).trigger)
            .filter(|t| t.active)
            .last()
            .unwrap_or_else(Trigger::idle)
    }

    /// Hold one state for `secs`, sampling every second; true if it ever
    /// triggers.
    fn ever_triggers(s: &System, trees: &[TreeStats], cfg: &Config, secs: u32) -> bool {
        let mut hold = HashMap::new();
        (0..=secs).any(|t| {
            evaluate(s, trees, None, cfg, t as f64, &mut hold, 0.0, true)
                .trigger
                .active
        })
    }

    /// RAM and swap spent, everything stalling: a real freeze in progress.
    fn freezing() -> System {
        sys(1.0, 95.0, 50.0, 40.0, 10.0, 40.0, 0)
    }

    #[test]
    fn ramp_is_zero_at_zero_half_at_warn_full_at_kill() {
        assert_eq!(ramp(0.0, 10.0, 40.0), 0.0);
        assert_eq!(ramp(10.0, 10.0, 40.0), 50.0);
        assert_eq!(ramp(25.0, 10.0, 40.0), 75.0);
        assert_eq!(ramp(40.0, 10.0, 40.0), 100.0);
        assert_eq!(ramp(99.0, 10.0, 40.0), 100.0);
    }

    #[test]
    fn sustained_fires_only_after_the_window_and_then_rearms() {
        let mut hold = HashMap::new();
        assert!(!sustained(&mut hold, "k", true, 0.0, 30.0));
        assert!(!sustained(&mut hold, "k", true, 29.0, 30.0));
        assert!(sustained(&mut hold, "k", true, 30.0, 30.0));
        // consumed: must re-arm from scratch
        assert!(!sustained(&mut hold, "k", true, 31.0, 30.0));
        // condition clears
        assert!(!sustained(&mut hold, "k", false, 100.0, 30.0));
    }

    #[test]
    fn idle_machine_is_ok_with_no_trigger() {
        let cfg = Config::default();
        let trees = vec![tree(1, 100, 10.0, true)];
        let mut hold = HashMap::new();
        let ev = evaluate(
            &sys(95.0, 5.0, 0.0, 0.0, 10.0, 10.0, 0),
            &trees,
            None,
            &cfg,
            0.0,
            &mut hold,
            0.0,
            true,
        );
        assert_eq!(ev.risk.status, "OK");
        assert!(!ev.trigger.active);
        assert!(ev.risk.score < 45);
    }

    #[test]
    fn a_freeze_panics_after_a_few_samples_and_picks_the_largest_killable_job() {
        let cfg = Config::default();
        let trees = vec![
            tree(10, 5_000, 30.0, true),
            tree(20, 9_000, 60.0, true),
            tree(30, 8_000, 55.0, false), // protected: never a victim
        ];
        assert!(
            !after(&freezing(), &trees, &cfg, 0).active,
            "one bad sample is not a freeze"
        );
        let t = after(&freezing(), &trees, &cfg, cfg.panic_sustain as u32);
        assert!(t.active && t.panic);
        assert_eq!(t.victim_pid, Some(20));
    }

    #[test]
    fn a_bystander_is_spared_when_the_hog_is_not_ours_to_kill() {
        let cfg = Config::default();
        // Another user's 120 GB model server fills the machine; ours is stable.
        let ours = || tree(20, 45_000, 24.0, true);
        let theirs = tree(30, 120_000, 63.0, false);
        let exhausted = sys(4.0, 90.0, 12.0, 6.0, 10.0, 40.0, 0);
        let t = after(&exhausted, &[ours(), theirs], &cfg, 60);
        assert!(t.active);
        assert_eq!(t.victim_pid, None, "killing our job would not be the cure");
    }

    #[test]
    fn a_freeze_spread_over_many_small_jobs_is_still_acted_on() {
        let cfg = Config::default();
        // Twenty agents each running a 4 GB test (2.1% of RAM): RAM and swap
        // spent. The biggest of them goes, though none reaches the 5% floor.
        let mut trees: Vec<TreeStats> = (0..20).map(|i| tree(100 + i, 4_000, 2.1, true)).collect();
        trees[7].mem_job.rss = 4_100;
        let s = sys(4.0, 90.0, 12.0, 6.0, 10.0, 2.1, 0);
        assert_eq!(
            after(&s, &trees, &cfg, cfg.pressure_kill_sustain as u32).victim_pid,
            Some(107)
        );
        // Under gridlock with swap left, the floor stays: too small to help.
        let s = sys(10.0, 5.0, 60.0, 35.0, 10.0, 2.1, 0);
        let t = after(&s, &trees, &cfg, cfg.mem_avail_kill_sustain as u32);
        assert!(t.active && t.victim_pid.is_none());
    }

    #[test]
    fn a_protected_hog_inside_our_tree_spares_the_bystanders_until_a_panic() {
        let cfg = Config::default();
        // A protected member (an agent leaking 40 GB) of an otherwise killable
        // tree outweighs our 16 GB VM: killing the VM would not cure it.
        let mut session = tree(10, 1_000, 0.5, true);
        session.untouchable = 40_000;
        let vm = tree(20, 16_000, 8.4, true);
        let s = sys(4.0, 90.0, 12.0, 6.0, 10.0, 8.4, 0);
        let t = after(
            &s,
            &[session.clone(), vm.clone()],
            &cfg,
            cfg.pressure_kill_sustain as u32,
        );
        assert!(t.active && t.victim_pid.is_none());
        // In a panic the machine comes first.
        let t = after(&freezing(), &[session, vm], &cfg, cfg.panic_sustain as u32);
        assert_eq!(t.victim_pid, Some(20));
    }

    #[test]
    fn guard_holds_when_no_job_is_large_enough_to_help() {
        let cfg = Config::default();
        // A freeze, but the only tree is 0.1% of RAM (< a fifth of the 5% floor).
        let trees = vec![tree(10, 100, 0.1, true)];
        let mut hold = HashMap::new();
        let mut ev = None;
        for t in 0..=10 {
            ev = Some(evaluate(
                &freezing(),
                &trees,
                None,
                &cfg,
                t as f64,
                &mut hold,
                0.0,
                true,
            ));
        }
        let ev = ev.unwrap();
        assert!(ev.trigger.active);
        assert_eq!(ev.trigger.victim_pid, None);
        assert!(ev.signals.iter().any(|s| s.key == "hold"));
    }

    #[test]
    fn low_ram_kills_only_with_swap_spent_and_tasks_waiting() {
        let cfg = Config::default();
        let trees = vec![tree(10, 9_000, 40.0, true)];
        // Nothing waiting on it: the kernel is coping, whatever the levels.
        assert!(!ever_triggers(
            &sys(1.0, 99.0, 0.0, 0.0, 10.0, 40.0, 0),
            &trees,
            &cfg,
            3600
        ));
        // Waiting, RAM nearly gone, but swap still has room to take it.
        assert!(!ever_triggers(
            &sys(1.0, 70.0, 12.0, 6.0, 10.0, 40.0, 0),
            &trees,
            &cfg,
            3600
        ));
        // RAM and swap both spent and tasks waiting: fires after the window.
        let s = sys(4.0, 90.0, 12.0, 6.0, 10.0, 40.0, 0);
        let window = cfg.pressure_kill_sustain as u32;
        assert!(!after(&s, &trees, &cfg, window - 1).active);
        let t = after(&s, &trees, &cfg, window);
        assert!(t.active && !t.panic);
        assert_eq!(t.victim_pid, Some(10));
        // Only `some` stalls, nothing fully stuck: a big load that may still fit.
        assert!(!ever_triggers(
            &sys(4.0, 90.0, 30.0, 4.0, 10.0, 40.0, 0),
            &trees,
            &cfg,
            3600
        ));
        // No swap configured counts as spent.
        let mut s = sys(4.0, 0.0, 12.0, 6.0, 10.0, 40.0, 0);
        s.swap_total = 0;
        assert!(after(&s, &trees, &cfg, window).active);
        // The last 2%: a panic, after a few samples.
        let s = sys(1.0, 90.0, 12.0, 6.0, 10.0, 40.0, 0);
        assert!(
            !after(&s, &trees, &cfg, 0).active,
            "one bad sample is not a freeze"
        );
        assert!(after(&s, &trees, &cfg, cfg.panic_sustain as u32).panic);
        // The kernel reports exactly 0 available in deep exhaustion: still act.
        let mut s = sys(0.0, 99.0, 50.0, 40.0, 10.0, 40.0, 0);
        s.mem_avail = 0;
        assert!(after(&s, &trees, &cfg, cfg.panic_sustain as u32).panic);
    }

    #[test]
    fn stalls_without_global_reclaim_never_kill() {
        // PSI also counts a container thrashing inside its own limit and a task
        // paging itself back in. Without the machine-wide counter moving, even
        // the worst numbers are somebody's private problem.
        let cfg = Config::default();
        let trees = vec![tree(10, 9_000, 40.0, true)];
        for avail in [0.5, 4.0, 14.0] {
            let mut s = sys(avail, 99.0, 100.0, 100.0, 10.0, 40.0, 20);
            s.direct_reclaim = false;
            assert!(!ever_triggers(&s, &trees, &cfg, 900), "avail={avail}");
        }
        // A stale 60 s average with nothing waiting now does not count either.
        let mut s = sys(10.0, 90.0, 0.0, 0.0, 10.0, 40.0, 0);
        s.psi.mem_some60 = 50.0;
        assert!(!ever_triggers(&s, &trees, &cfg, 900));
    }

    #[test]
    fn the_2026_10_08_kills_do_not_happen() {
        let cfg = Config::default();
        let trees = vec![tree(6921, 69_000, 36.0, true)];
        // 11:24:50 — a 70 GB model load: swap 66%, but 48% of RAM available and
        // pressure far under the kill line. It killed 79 processes.
        let first = sys(48.0, 66.0, 7.0, 6.0, 51.0, 36.0, 3);
        assert!(!ever_triggers(&first, &trees, &cfg, 3600));
        let ev = evaluate(
            &first,
            &trees,
            None,
            &cfg,
            0.0,
            &mut HashMap::new(),
            0.0,
            true,
        );
        assert_ne!(ev.risk.status, "CRITICAL");
        assert_eq!(alert(&ev.signals), None);
        // 11:25:56 — the cause already gone, 80% available, no pressure, swap
        // still 62% because swap does not drain. It killed the browser.
        let second = sys(80.0, 62.0, 2.0, 0.0, 15.0, 5.0, 0);
        assert!(!ever_triggers(&second, &trees, &cfg, 3600));
        let ev = evaluate(
            &second,
            &trees,
            None,
            &cfg,
            0.0,
            &mut HashMap::new(),
            0.0,
            true,
        );
        assert_eq!(ev.risk.status, "OK");
        assert!(
            ev.risk.score < 45,
            "idle swap is not freeze risk: {}",
            ev.risk.score
        );
        assert_eq!(alert(&ev.signals), None);
    }

    #[test]
    fn nothing_kills_while_ram_is_not_scarce() {
        let cfg = Config::default();
        let trees = vec![tree(10, 9_000, 80.0, true)];
        let steps = [0.0, 30.0, 61.0, 86.0, 100.0];
        for avail in [cfg.mem_avail_warn, 20.0, 50.0, 100.0] {
            for swap in steps {
                for some in steps {
                    for full in steps {
                        for cpu in [0.0, 99.0, 100.0] {
                            let mut s = sys(avail, swap, some, full, cpu, 80.0, 20);
                            s.psi.cpu_some60 = 100.0;
                            assert!(
                                !ever_triggers(&s, &trees, &cfg, 900),
                                "avail={avail} swap={swap} some={some} full={full} cpu={cpu}"
                            );
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn gridlock_kills_only_after_it_holds() {
        let cfg = Config::default();
        let trees = vec![tree(10, 9_000, 40.0, true)];
        // RAM scarce, swap with room, but everything stalled a third of the time.
        let s = sys(10.0, 5.0, 60.0, 35.0, 10.0, 40.0, 0);
        let window = cfg.mem_avail_kill_sustain as u32;
        assert!(!after(&s, &trees, &cfg, window - 1).active);
        let t = after(&s, &trees, &cfg, window);
        assert!(t.active && !t.panic);
        assert_eq!(t.victim_pid, Some(10));
        // Pressure at the old kill level (some60 45, full10 20) is not gridlock.
        assert!(!ever_triggers(
            &sys(10.0, 5.0, 45.0, 20.0, 10.0, 40.0, 0),
            &trees,
            &cfg,
            3600
        ));
    }

    #[test]
    fn a_big_load_swapping_out_is_not_a_freeze() {
        let cfg = Config::default();
        let trees = vec![tree(10, 76_000, 40.0, true)];
        // Measured 2026-10-08 11:53–11:58, a 76 GB model loading: swap 72%,
        // PSI some10 11 / full10 17, machine-wide reclaim running. Harmless at
        // the 40% available it had — and just as harmless had it started with
        // less and reached 14%.
        for avail in [40.0, 14.0, 6.0] {
            let s = sys(avail, 72.0, 11.0, 17.0, 60.0, 40.0, 3);
            assert!(!ever_triggers(&s, &trees, &cfg, 3600), "avail={avail}");
        }
        // Swap level alone, at any RAM level without the last 5% gone: never.
        assert!(!ever_triggers(
            &sys(6.0, 100.0, 12.0, 6.0, 10.0, 40.0, 0),
            &trees,
            &cfg,
            3600
        ));
    }

    #[test]
    fn a_sustain_window_restarts_when_its_condition_clears() {
        let cfg = Config::default();
        let trees = vec![tree(10, 9_000, 40.0, true)];
        let low = sys(4.0, 90.0, 12.0, 6.0, 10.0, 40.0, 0);
        let fine = sys(90.0, 5.0, 0.0, 0.0, 10.0, 40.0, 0);
        let mut hold = HashMap::new();
        let mut at =
            |s: &System, t: f64| evaluate(s, &trees, None, &cfg, t, &mut hold, 0.0, true).trigger;
        assert!(!at(&low, 0.0).active);
        assert!(!at(&fine, 2.0).active);
        // Low again long after: the old window must not carry over.
        assert!(!at(&low, 500.0).active);
        assert!(at(&low, 500.0 + cfg.pressure_kill_sustain).active);
    }

    #[test]
    fn unreadable_meminfo_never_kills() {
        let cfg = Config::default();
        let trees = vec![tree(10, 9_000, 40.0, true)];
        let mut s = sys(0.0, 0.0, 50.0, 40.0, 10.0, 40.0, 0);
        (s.mem_total, s.mem_avail, s.swap_total) = (0, 0, 0);
        assert!(!ever_triggers(&s, &trees, &cfg, 900));
    }

    #[test]
    fn focus_is_the_lowest_job_never_the_session_above_it() {
        let gb = |n: u64| n << 30;
        let floor = gb(9);
        let land = |kids: &HashMap<i32, Vec<i32>>, w: &HashMap<i32, u64>, shells: &[i32]| {
            let weigh = |pids: &[i32]| pids.iter().filter_map(|p| w.get(p)).sum::<u64>();
            let (pid, mut members) =
                focus(1, kids, &shells.iter().copied().collect(), floor, &weigh);
            members.sort();
            (pid, members)
        };
        // bash(1) → agent shell(2) → tools 3,4,5; tool 3 → bash(6) → python(7) → worker(8)
        let kids: HashMap<i32, Vec<i32>> = HashMap::from([
            (1, vec![2]),
            (2, vec![3, 4, 5]),
            (3, vec![6]),
            (6, vec![7]),
            (7, vec![8]),
        ]);
        let w: HashMap<i32, u64> = HashMap::from([
            (1, 1 << 20),
            (2, 1 << 20),
            (3, gb(1)),
            (4, gb(1)),
            (5, gb(1)),
            (6, 1 << 20),
            (7, gb(70)),
            (8, 1 << 20),
        ]);
        assert_eq!(
            land(&kids, &w, &[1, 6]),
            (7, vec![7, 8]),
            "only the model server dies"
        );

        // Two servers of 40 GB under one shell: one of them, not the shell.
        let two: HashMap<i32, Vec<i32>> = HashMap::from([(1, vec![2, 3, 4])]);
        let w: HashMap<i32, u64> =
            HashMap::from([(1, 1 << 20), (2, gb(40)), (3, gb(39)), (4, gb(1))]);
        assert_eq!(land(&two, &w, &[1]), (2, vec![2]));

        // Three small background jobs under a terminal's shell: the shell is
        // never the job, so it is the heaviest of them (too small to kill).
        let w: HashMap<i32, u64> =
            HashMap::from([(1, 1 << 20), (2, gb(4)), (3, gb(3)), (4, gb(3))]);
        assert_eq!(land(&two, &w, &[1]), (2, vec![2]));

        // A fan-out of small workers is one job at their parent: make(2) -j.
        let make: HashMap<i32, Vec<i32>> = HashMap::from([(1, vec![2]), (2, (10..74).collect())]);
        let mut w: HashMap<i32, u64> = (10..74).map(|p| (p, gb(1))).collect();
        w.extend([(1, 1 << 20), (2, 1 << 20)]);
        let (pid, members) = land(&make, &w, &[1]);
        assert_eq!((pid, members.len()), (2, 65));

        // A ppid cycle from a racy /proc read must not hang the daemon.
        let cyclic: HashMap<i32, Vec<i32>> = HashMap::from([(1, vec![2]), (2, vec![1])]);
        land(&cyclic, &HashMap::from([(1, 1), (2, 9)]), &[]);
    }

    fn proc(pid: i32, ppid: i32, comm: &str, gb: u64) -> ProcInfo {
        ProcInfo {
            pid,
            ppid,
            comm: comm.to_string(),
            state: 'S',
            uid: 1000,
            rss: gb << 30,
            held: gb << 30,
            swap: 0,
            shmem: 0,
            ticks: 0,
            starttime: 1,
            threads: 1,
            oom_score: 0,
            cmd: comm.to_string(),
        }
    }

    fn trees_of(procs: &[ProcInfo]) -> Vec<TreeStats> {
        let cfg = Config::default();
        analyze(
            procs,
            &cfg,
            &cfg.protected_prefixes(),
            &HashSet::new(),
            1000,
            32,
            190 << 30,
            0.0,
            1.0,
            100,
            &HashMap::new(),
            &mut HashMap::new(),
            0.0,
        )
    }

    #[test]
    fn the_2026_10_08_session_loses_only_the_model_server() {
        // gnome-terminal → bash(10) → ricon(11) → claude(12) ×2 → bash → python
        let trees = trees_of(&[
            proc(10, 1, "bash", 0),
            proc(11, 10, "ricon", 0),
            proc(12, 11, "claude", 1),
            proc(13, 11, "claude", 1),
            proc(20, 12, "bash", 0),
            proc(21, 20, "python", 70),
            proc(22, 21, "python", 0),
            proc(30, 13, "bash", 0),
            proc(31, 30, "deno", 2),
        ]);
        let job = |root: i32| {
            trees
                .iter()
                .find(|t| t.root == root)
                .map(|t| t.mem_job.clone())
        };
        let model = job(20).expect("each tool call is its own tree");
        assert_eq!((model.pid, model.members.len()), (21, 2));
        // The agents and ricon are never signalled; the shell hosting ricon
        // has nothing left to kill.
        assert!(trees.iter().filter(|t| t.killable).all(|t| {
            [&t.mem_job, &t.cpu_job]
                .iter()
                .all(|j| j.members.iter().all(|p| ![10, 11, 12, 13].contains(p)))
        }));
        // And the largest job on the machine is the model server alone.
        let ev = after(&freezing(), &trees, &Config::default(), 60);
        assert_eq!(ev.victim_pid, Some(21));
    }

    #[test]
    fn a_shell_is_never_the_job_and_shared_memory_counts_once() {
        // bash(10) hosts an agent and twenty 1 GB background jobs: neither the
        // shell nor "all its children" is a job — only the heaviest child is,
        // and at 1 GB it is too small for the daemon to act on.
        let mut procs = vec![proc(10, 1, "bash", 0), proc(11, 10, "claude", 1)];
        procs.extend((100..120).map(|p| proc(p, 10, "worker", 1)));
        let trees = trees_of(&procs);
        let t = trees.iter().find(|t| t.root == 10).unwrap();
        assert_eq!(t.mem_job.members.len(), 1);
        assert!(t.mem_job.rss_pct < Config::default().min_victim_rss_pct);

        // Sixteen workers mapping one 8 GB segment hold 8 GB, not 128 GB.
        let mut procs = vec![proc(10, 1, "bash", 0), proc(20, 10, "ray", 0)];
        procs.extend((100..116).map(|p| ProcInfo {
            shmem: 8 << 30,
            ..proc(p, 20, "worker", 0)
        }));
        let trees = trees_of(&procs);
        let t = trees.iter().find(|t| t.root == 10).unwrap();
        assert_eq!(t.mem_job.rss, 8 << 30);
    }

    #[test]
    fn the_victim_is_the_job_not_the_tree() {
        let cfg = Config::default();
        let mut session = tree(6921, 75_000, 39.0, true);
        session.mem_job = Job {
            pid: 777,
            name: "python".to_string(),
            cmd: "python serve/server.py".to_string(),
            members: vec![777],
            rss: 70_000,
            rss_pct: 37.0,
        };
        let t = after(&freezing(), &[session], &cfg, 10);
        assert_eq!(t.victim_pid, Some(777));
        assert_eq!(t.victim_name.as_deref(), Some("python"));
    }

    fn starved(lag: f64, cpu: f64, psi: f64) -> System {
        let mut s = sys(90.0, 5.0, 0.0, 0.0, cpu, 40.0, 0);
        s.sampler_lag_s = lag;
        s.psi.cpu_some60 = psi;
        s
    }

    #[test]
    fn a_busy_cpu_never_kills_but_a_starved_os_does() {
        let cfg = Config::default();
        let mut hog = tree(10, 100, 1.0, true);
        hog.cpu_machine = 95.0;
        let trees = vec![hog, tree(20, 9_000, 40.0, true)];
        // Every core pinned and queued for an hour, samples on time: heavy work.
        assert!(!ever_triggers(
            &starved(0.0, 100.0, 100.0),
            &trees,
            &cfg,
            3600
        ));
        // Samples late but the CPU is not pinned (a slow disk, a paused VM).
        assert!(!ever_triggers(
            &starved(9.0, 60.0, 100.0),
            &trees,
            &cfg,
            3600
        ));
        // Pinned, queued and the daemon itself cannot run: after the window, the
        // tree burning the CPU goes — not the big idle one.
        let s = starved(5.0, 100.0, 90.0);
        let mut hold = HashMap::new();
        assert!(
            !evaluate(&s, &trees, None, &cfg, 0.0, &mut hold, 0.0, true)
                .trigger
                .active
        );
        let ev = evaluate(
            &s,
            &trees,
            None,
            &cfg,
            cfg.cpu_sys_kill_sustain,
            &mut hold,
            0.0,
            true,
        );
        assert!(ev.trigger.active && !ev.trigger.panic);
        assert_eq!(ev.trigger.victim_pid, Some(10));
    }

    #[test]
    fn starvation_holds_when_no_single_tree_is_the_hog() {
        let cfg = Config::default();
        let mut a = tree(10, 100, 1.0, true);
        a.cpu_machine = 30.0;
        let mut b = tree(20, 100, 1.0, true);
        b.cpu_machine = 30.0;
        let s = starved(5.0, 100.0, 90.0);
        let mut hold = HashMap::new();
        evaluate(&s, &[], None, &cfg, 0.0, &mut hold, 0.0, true);
        let ev = evaluate(
            &s,
            &[a, b],
            None,
            &cfg,
            cfg.cpu_sys_kill_sustain,
            &mut hold,
            0.0,
            true,
        );
        assert!(ev.trigger.active);
        assert_eq!(ev.trigger.victim_pid, None);
    }

    /// `PROTECTOR_REPLAY=~/.protector/protector.log cargo test replay -- --nocapture`
    /// feeds every heartbeat the daemon ever logged back through the policy.
    #[test]
    fn replay_logged_heartbeats() {
        let Ok(path) = std::env::var("PROTECTOR_REPLAY") else {
            return;
        };
        let cfg = Config::default();
        let trees = vec![tree(10, 9_000, 40.0, true)];
        let num = |line: &str, key: &str| -> f64 {
            line.split(key)
                .nth(1)
                .map(|r| {
                    r.chars()
                        .take_while(|c| c.is_ascii_digit() || *c == '.')
                        .collect::<String>()
                })
                .and_then(|v| v.parse().ok())
                .unwrap_or(0.0)
        };
        let text = std::fs::read_to_string(&path).expect("read log");
        let mut hold = HashMap::new();
        let (mut n, mut triggers, mut worst) = (0u32, Vec::new(), 0u32);
        for line in text.lines().filter(|l| l.contains(" OK cpu=")) {
            let mut s = sys(
                num(line, "ram_avail="),
                num(line, "swap="),
                num(line, "psi_mem[s60="),
                num(line, " f10="),
                num(line, "cpu="),
                40.0,
                0,
            );
            s.psi.cpu_some60 = num(line, "psi_cpu[s60=");
            n += 1;
            // Heartbeats are 30 s apart: treat each as held for the whole gap.
            let ev = evaluate(
                &s,
                &trees,
                None,
                &cfg,
                n as f64 * 30.0,
                &mut hold,
                0.0,
                true,
            );
            worst = worst.max(ev.risk.score);
            if ev.trigger.active {
                triggers.push(line.to_string());
            }
        }
        println!(
            "replayed {n} heartbeats: {} trigger(s), worst risk {worst}",
            triggers.len()
        );
        assert!(n > 0 && triggers.is_empty(), "{triggers:#?}");
    }

    #[test]
    fn harm_ranks_the_biggest_tree_above_small_ones_and_reaches_100_at_kill_threshold() {
        let cfg = Config::default();
        let mut big = tree(1, 10_000, 2.0, true);
        let mut small = tree(2, 100, 0.02, true);
        score_tree(&mut big, &cfg, 0.0, 10_000);
        score_tree(&mut small, &cfg, 0.0, 10_000);
        assert!(big.harm > small.harm);
        assert!(
            big.harm >= 40,
            "the top hog should be clearly visible: {}",
            big.harm
        );
        assert!(
            small.harm < 15,
            "a tiny tree should not look dangerous: {}",
            small.harm
        );

        let mut killer = tree(3, 100_000, cfg.rss_tree_kill, true);
        score_tree(&mut killer, &cfg, 0.0, 100_000);
        assert_eq!(killer.harm, 100);
    }

    #[test]
    fn growth_signal_fires_and_scores() {
        let cfg = Config::default();
        let mut t = tree(1, 1_000, 0.5, true);
        t.grow_bps = 200.0 * 1e6; // 200 MB/s — beyond growth_kill_mbps (150)
        score_tree(&mut t, &cfg, 0.0, 1_000);
        assert!(t.signals.iter().any(|s| s.key == "grow"));
        assert_eq!(t.harm, 100);

        // A moderate, flagged-but-not-fatal rate stays well below 100.
        let mut m = tree(2, 1, 0.0005, true);
        m.grow_bps = 30.0 * 1e6; // 30 MB/s → danger chip, harm ≈ 20
        score_tree(&mut m, &cfg, 0.0, 1_000);
        assert!(m.signals.iter().any(|s| s.key == "grow"));
        assert!(m.harm < 40, "30 MB/s should not read as fatal: {}", m.harm);
    }

    fn episode(start: f64, last_stall: Option<f64>) -> Episode {
        Episode {
            start,
            last: last_stall.unwrap_or(start),
            last_stall,
            text: "task kworker blocked for more than 122 seconds".to_string(),
        }
    }

    #[test]
    fn kernel_stall_is_critical_alerts_and_never_picks_a_victim() {
        let cfg = Config::default();
        // The 2026-10-02 freeze: 67% RAM free, a big killable tree, kernel stuck.
        let trees = vec![tree(10, 9_000, 40.0, true)];
        let k = episode(0.0, Some(190.0));
        let ev = evaluate(
            &sys(67.0, 1.7, 0.0, 0.0, 46.0, 40.0, 11),
            &trees,
            Some(&k),
            &cfg,
            200.0,
            &mut HashMap::new(),
            0.0,
            true,
        );
        assert_eq!(ev.risk.status, "CRITICAL");
        assert!(
            !ev.trigger.active,
            "killing a process cannot unstick a kernel"
        );
        let text = alert(&ev.signals).expect("must alert");
        assert!(text.contains("kernel stall") && text.contains("D-state"));
    }

    #[test]
    fn pressure_with_ram_to_spare_never_alerts() {
        let cfg = Config::default();
        // The eight alerts of 2026-10-08…14, e.g. 37% available, swap 97%,
        // full10 25: stutters, not freezes.
        for (avail, swap, full) in [(41.0, 50.0, 18.0), (30.0, 57.0, 28.0), (37.0, 97.0, 25.0)] {
            let ev = evaluate(
                &sys(avail, swap, 7.0, full, 20.0, 10.0, 0),
                &[],
                None,
                &cfg,
                0.0,
                &mut HashMap::new(),
                0.0,
                true,
            );
            assert_eq!(alert(&ev.signals), None, "avail={avail} full10={full}");
        }
        // Scarce RAM and the same pressure: the user must know.
        let ev = evaluate(
            &sys(8.0, 97.0, 7.0, 25.0, 20.0, 10.0, 0),
            &[],
            None,
            &cfg,
            0.0,
            &mut HashMap::new(),
            0.0,
            true,
        );
        assert!(alert(&ev.signals).is_some());
    }

    #[test]
    fn busy_cpu_alone_never_alerts() {
        let cfg = Config::default();
        let ev = evaluate(
            &sys(90.0, 5.0, 0.0, 0.0, 99.0, 10.0, 0),
            &[],
            None,
            &cfg,
            0.0,
            &mut HashMap::new(),
            0.0,
            true,
        );
        assert!(ev.signals.iter().any(|s| s.key == "cpu"));
        assert_eq!(alert(&ev.signals), None);
    }

    #[test]
    fn reboot_is_opt_in_and_needs_an_old_fresh_still_stuck_stall() {
        let mut cfg = Config::default();
        let k = episode(0.0, Some(190.0));
        assert!(!should_reboot(&k, 11, 200.0, &cfg), "off by default");
        cfg.stall_reboot_s = 180.0;
        assert!(should_reboot(&k, 11, 200.0, &cfg));
        assert!(!should_reboot(&k, 11, 100.0, &cfg), "too young");
        assert!(!should_reboot(&k, 0, 200.0, &cfg), "nothing stuck now");
        assert!(
            !should_reboot(&episode(0.0, None), 11, 200.0, &cfg),
            "fault, not a stall"
        );
        assert!(!should_reboot(&k, 11, 600.0, &cfg), "stall report too old");
    }
}
