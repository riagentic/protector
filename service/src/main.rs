//! protectord — the daemon.
//!
//! One thread samples `/proc` and the pressure files, evaluates the policy,
//! performs (or refuses) kills, and publishes an immutable `Snapshot`. A second
//! thread serves the loopback API and forwards mutating requests back to the
//! sampler as `Command`s.

mod api;
mod config;
mod kernel;
mod model;
mod policy;
mod proc;

use std::collections::{HashMap, HashSet, VecDeque};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{channel, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

use api::{Command, Shared};
use config::{Config, VERSION};
use model::*;
use policy::EmaState;

static STOP: AtomicBool = AtomicBool::new(false);

/// Warning the user gets between "will reboot" and the reboot.
const REBOOT_WARN_S: f64 = 30.0;

/// A panic skips the cooldown, but still waits this long after a kill so the
/// freed memory shows up before a second job is chosen.
const PANIC_COOLDOWN_S: f64 = 10.0;

extern "C" fn on_signal(_sig: libc::c_int) {
    STOP.store(true, Ordering::SeqCst);
}

// ── logging ──────────────────────────────────────────────────────────

struct Logger {
    path: PathBuf,
    silent: bool,
}

impl Logger {
    fn new(path: PathBuf) -> Self {
        Logger {
            path,
            silent: false,
        }
    }
    fn silent() -> Self {
        Logger {
            path: PathBuf::from("/dev/null"),
            silent: true,
        }
    }
    fn log(&self, level: &str, msg: &str) {
        if self.silent {
            return;
        }
        let line = format!("{} {} {}\n", now_stamp(), level, msg);
        use std::io::Write;
        if let Ok(mut f) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)
        {
            let _ = f.write_all(line.as_bytes());
        }
        crate::config::set_private(&self.path, crate::config::FILE_MODE);
        print!("{line}");
        let _ = std::io::stdout().flush();
    }
}

fn now_stamp() -> String {
    let t = proc::epoch_secs() as libc::time_t;
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    unsafe { libc::localtime_r(&t, &mut tm) };
    format!(
        "{:04}-{:02}-{:02} {:02}:{:02}:{:02}",
        tm.tm_year + 1900,
        tm.tm_mon + 1,
        tm.tm_mday,
        tm.tm_hour,
        tm.tm_min,
        tm.tm_sec
    )
}

/// Desktop pop-up that stays until dismissed. Best effort; never blocks the
/// sampler.
fn notify(title: &str, body: &str) {
    let (title, body) = (format!("Protector: {title}"), body.to_string());
    std::thread::spawn(move || {
        let _ = std::process::Command::new("notify-send")
            .args(["--urgency=critical", "--app-name=Protector"])
            .args(["--icon=dialog-warning", "--", &title, &body])
            .status();
    });
}

/// Flush data to disk without waiting: on a stuck device `sync` may never
/// return, and a hard reset may be next.
fn sync_disks() -> Receiver<()> {
    let (tx, rx) = channel();
    std::thread::spawn(move || {
        unsafe { libc::sync() };
        let _ = tx.send(());
    });
    rx
}

/// `sync`, then ask logind for a clean reboot. logind refuses while something
/// blocks shutdown (a package install, say) — the error says so.
fn clean_reboot() -> Result<(), String> {
    let _ = sync_disks().recv_timeout(Duration::from_secs(10));
    let out = std::process::Command::new("systemctl")
        .arg("reboot")
        .output()
        .map_err(|e| e.to_string())?;
    if out.status.success() {
        Ok(())
    } else {
        Err(String::from_utf8_lossy(&out.stderr).trim().to_string())
    }
}

// ── sampler state ────────────────────────────────────────────────────

struct Inner {
    cfg: Config,
    config_path: PathBuf,
    started_epoch: f64,
    started: Instant,
    seq: u64,
    page: u64,
    clk: u64,
    nproc: usize,
    me_uid: u32,
    user_map: HashMap<u32, String>,
    events: VecDeque<Event>,
    history: VecDeque<Sample>,
    prev_ticks: HashMap<i32, u64>,
    prev_t: Option<f64>,
    prev_total: u64,
    prev_idle: u64,
    ema: HashMap<i32, EmaState>,
    hold: HashMap<String, f64>,
    last_kill_t: f64,
    last_kill: Option<KillInfo>,
    last_hold: Option<(String, f64)>,
    extra_pids: HashSet<i32>,
    last_trees: Vec<policy::TreeStats>,
    last_heartbeat: f64,
    faults: Receiver<kernel::Fault>,
    kernel: Option<kernel::Episode>,
    last_alert_t: f64,
    /// Seconds the last sleep between samples overran: CPU starvation.
    lag_s: f64,
    cmds: proc::CmdCache,
    reclaim_stalls: Option<u64>,
    last_reclaim_t: f64,
    /// When the pending clean reboot fires; infinity = decided, never again.
    reboot_at: Option<f64>,
}

impl Inner {
    fn event(
        &mut self,
        kind: &str,
        severity: &str,
        text: String,
        pid: Option<i32>,
        name: Option<String>,
    ) {
        self.seq += 1;
        let ev = Event {
            seq: self.seq,
            t: proc::epoch_secs(),
            kind: kind.to_string(),
            severity: severity.to_string(),
            text,
            pid,
            name,
        };
        self.events.push_back(ev);
        while self.events.len() > self.cfg.events {
            self.events.pop_front();
        }
    }

    /// Seconds since start on the monotonic clock.
    fn mono(&self) -> f64 {
        self.started.elapsed().as_secs_f64()
    }

    fn protected_pids(&self) -> HashSet<i32> {
        let mut set = HashSet::new();
        if self.cfg.protect_self {
            for p in proc::ancestry(std::process::id() as i32) {
                set.insert(p);
            }
        }
        for p in &self.extra_pids {
            set.insert(*p);
        }
        set
    }

    fn sample(&mut self, logger: &Logger) -> Arc<Snapshot> {
        // `now` stamps what the UI shows; every policy clock (sustain windows,
        // cooldown) runs on `mono`, which a clock step cannot move and which
        // stands still while suspended.
        let now = proc::epoch_secs();
        let mono = self.mono();
        let dt = match self.prev_t {
            Some(p) if mono > p => mono - p,
            _ => 0.0,
        };
        self.prev_t = Some(mono);

        let stalls = proc::read_reclaim_stalls();
        if matches!((self.reclaim_stalls, stalls), (Some(a), Some(b)) if b > a) {
            self.last_reclaim_t = mono;
        }
        self.reclaim_stalls = stalls;
        // Unreadable: do not let a missing counter veto every kill.
        let direct_reclaim = stalls.is_none() || mono - self.last_reclaim_t <= 10.0;

        let mem = proc::read_meminfo();
        let psi_mem = proc::read_pressure("memory");
        let psi_cpu = proc::read_pressure("cpu");
        let psi_io = proc::read_pressure("io");
        let (total, idle) = proc::read_cpu_jiffies();
        let cpu_busy = if self.prev_total > 0 && total > self.prev_total {
            let dtotal = (total - self.prev_total) as f64;
            let didle = idle.saturating_sub(self.prev_idle) as f64;
            (100.0 * (dtotal - didle) / dtotal).clamp(0.0, 100.0)
        } else {
            0.0
        };
        self.prev_total = total;
        self.prev_idle = idle;
        let (load1, load5, load15) = proc::read_loadavg();

        let procs = proc::read_procs(self.page, &mut self.cmds);
        let prefixes = self.cfg.protected_prefixes();
        let protected = self.protected_pids();

        let trees = policy::analyze(
            &procs,
            &self.cfg,
            &prefixes,
            &protected,
            self.me_uid,
            self.nproc,
            mem.total,
            now,
            dt,
            self.clk,
            &self.prev_ticks,
            &mut self.ema,
            psi_mem.some60,
        );
        self.prev_ticks = procs.iter().map(|p| (p.pid, p.ticks)).collect();

        let d_state = procs.iter().filter(|p| p.state == 'D').count();
        let threads: u64 = procs.iter().map(|p| p.threads).sum();
        let largest_tree_pct = trees
            .iter()
            .filter(|t| t.killable)
            .map(|t| t.rss_pct)
            .fold(0.0_f64, f64::max);

        let mem_available_pct = if mem.total > 0 {
            100.0 * mem.available as f64 / mem.total as f64
        } else {
            0.0
        };
        let swap_used = mem.swap_total.saturating_sub(mem.swap_free);
        let swap_used_pct = if mem.swap_total > 0 {
            100.0 * swap_used as f64 / mem.swap_total as f64
        } else {
            0.0
        };

        let system = System {
            mem_total: mem.total,
            mem_avail: mem.available,
            mem_available_pct,
            mem_used_pct: 100.0 - mem_available_pct,
            swap_total: mem.swap_total,
            swap_used,
            swap_used_pct,
            cpu_busy_pct: cpu_busy,
            nproc: self.nproc,
            load1,
            load5,
            load15,
            psi: Psi {
                mem_some10: psi_mem.some10,
                mem_some60: psi_mem.some60,
                mem_full10: psi_mem.full10,
                mem_full60: psi_mem.full60,
                cpu_some10: psi_cpu.some10,
                cpu_some60: psi_cpu.some60,
                io_some10: psi_io.some10,
                io_some60: psi_io.some60,
                io_full10: psi_io.full10,
                io_full60: psi_io.full60,
            },
            d_state,
            sampler_lag_s: self.lag_s,
            direct_reclaim,
            procs: procs.len(),
            trees: trees.len(),
            threads: threads as usize,
            largest_tree_pct,
        };

        let cooldown_left = (self.cfg.cooldown_s - (mono - self.last_kill_t)).max(0.0);

        // ── kernel log ───────────────────────────────────────────────
        for f in self.faults.try_iter().collect::<Vec<_>>() {
            let kind = if f.stall { "stall" } else { "fault" };
            let first = self.kernel.is_none();
            let escalated = f.stall && self.kernel.as_ref().is_none_or(|k| k.last_stall.is_none());
            self.kernel = Some(kernel::step(self.kernel.take(), &f, now));
            if first || escalated {
                logger.log("KERNEL", &format!("{kind}: {}", f.text));
                self.event(
                    "kernel",
                    "crit",
                    format!("kernel {kind}: {}", f.text),
                    None,
                    None,
                );
            }
            if escalated {
                let _ = sync_disks();
            }
        }
        self.kernel = kernel::live(self.kernel.take(), now, self.cfg.kernel_window_s);

        let ev = policy::evaluate(
            &system,
            &trees,
            self.kernel.as_ref(),
            &self.cfg,
            mono,
            &mut self.hold,
            cooldown_left,
            self.cfg.armed(),
        );

        // ── act on the trigger ───────────────────────────────────────
        if ev.trigger.active {
            let reason = ev.trigger.reason.clone().unwrap_or_default();
            match ev.trigger.victim_pid {
                Some(vpid) => {
                    let name = ev.trigger.victim_name.clone().unwrap_or_default();
                    let can_act = cooldown_left <= 0.0
                        || (ev.trigger.panic && mono - self.last_kill_t >= PANIC_COOLDOWN_S);
                    if !can_act {
                        self.event(
                            "hold",
                            "warn",
                            format!(
                                "freeze signal ({reason}) but in cooldown {:.0}s — holding",
                                cooldown_left
                            ),
                            Some(vpid),
                            Some(name),
                        );
                    } else if self.cfg.armed() {
                        let info = self.kill_job(vpid, &reason, ev.trigger.panic, logger, &trees);
                        let members = info.members;
                        self.last_kill_t = self.mono();
                        self.last_kill = Some(info);
                        self.event(
                            if ev.trigger.panic { "panic" } else { "kill" },
                            "crit",
                            format!("killed {name} ({vpid}) — {reason} — {members} process(es) signalled"),
                            Some(vpid),
                            Some(name),
                        );
                    } else {
                        // observe: report what would happen, rate-limited.
                        if cooldown_left <= 0.0 {
                            self.last_kill_t = self.mono();
                            let info =
                                self.kill_job(vpid, &reason, ev.trigger.panic, logger, &trees);
                            self.last_kill = Some(info);
                            self.event(
                                "hold",
                                if ev.trigger.panic { "crit" } else { "danger" },
                                format!("[observe] would kill {name} ({vpid}) — {reason}"),
                                Some(vpid),
                                Some(name),
                            );
                        }
                    }
                }
                None => {
                    // Signal but no victim large enough — report, rate-limited.
                    let fresh = match &self.last_hold {
                        Some((r, t)) => r != &reason || now - t > 30.0,
                        None => true,
                    };
                    if fresh {
                        self.last_hold = Some((reason.clone(), now));
                        self.event(
                            "hold",
                            if ev.trigger.panic { "crit" } else { "warn" },
                            format!("freeze signal ({reason}) but no tree large enough to help — holding"),
                            None,
                            None,
                        );
                    }
                }
            }
        }

        // ── alert: what the user must know now ───────────────────────
        let alert = policy::alert(&ev.signals);
        if policy::sustained(
            &mut self.hold,
            "alert",
            alert.is_some(),
            mono,
            self.cfg.alert_sustain_s,
        ) && now - self.last_alert_t >= self.cfg.alert_repeat_s
        {
            self.last_alert_t = now;
            let body = alert.unwrap_or_default();
            logger.log("ALERT", &body.replace('\n', "; "));
            notify("freeze risk", &body);
        }

        // ── last resort: a stuck kernel has no victim — reboot cleanly ──
        let stuck = self
            .kernel
            .as_ref()
            .is_some_and(|k| policy::should_reboot(k, d_state, now, &self.cfg));
        match (stuck, self.reboot_at) {
            (false, _) => self.reboot_at = None,
            (true, None) if self.cfg.armed() => {
                self.reboot_at = Some(now + REBOOT_WARN_S);
                let text = format!(
                    "kernel stall for {:.0}s — clean reboot in {REBOOT_WARN_S:.0}s",
                    self.cfg.stall_reboot_s
                );
                logger.log("REBOOT", &text);
                notify("rebooting", &text);
                self.event("reboot", "crit", text, None, None);
            }
            (true, None) => {
                self.reboot_at = Some(f64::INFINITY);
                let text = "[observe] would sync and reboot — kernel stall".to_string();
                logger.log("OBSERVE", &text);
                self.event("reboot", "crit", text, None, None);
            }
            (true, Some(t)) if t.is_finite() && !self.cfg.armed() => {
                self.reboot_at = Some(f64::INFINITY);
                logger.log("REBOOT", "cancelled — switched to observe");
                self.event(
                    "reboot",
                    "info",
                    "reboot cancelled — observe mode".to_string(),
                    None,
                    None,
                );
            }
            (true, Some(t)) if now >= t => {
                self.reboot_at = Some(f64::INFINITY);
                logger.log("REBOOT", "sync + systemctl reboot");
                if let Err(e) = clean_reboot() {
                    let text = format!("clean reboot refused: {e}");
                    logger.log("REBOOT", &text);
                    notify("cannot reboot", &text);
                    self.event("reboot", "crit", text, None, None);
                }
            }
            _ => {}
        }

        self.last_trees = trees;

        // ── project processes for the wire ───────────────────────────
        let mut processes: Vec<ProcView> = self
            .last_trees
            .iter()
            .take(self.cfg.top_limit)
            .map(|t| {
                let mut v = policy::to_view(t, self.nproc);
                v.user = self
                    .user_map
                    .get(&t.uid)
                    .cloned()
                    .unwrap_or_else(|| t.uid.to_string());
                v
            })
            .collect();
        // Protected / other-user trees may not be in the killable top; make sure
        // the very largest ones are visible for context.
        processes.sort_by(|a, b| b.harm.cmp(&a.harm).then_with(|| b.rss.cmp(&a.rss)));

        // ── history ──────────────────────────────────────────────────
        self.history.push_back(Sample {
            t: now,
            mem_available_pct,
            swap_used_pct,
            cpu_busy_pct: cpu_busy,
            risk: ev.risk.score,
        });
        while self.history.len() > self.cfg.history {
            self.history.pop_front();
        }

        let thresholds = Thresholds {
            mem_avail_warn: self.cfg.mem_avail_warn,
            mem_avail_kill: self.cfg.mem_avail_kill,
            swap_warn: self.cfg.swap_warn,
            swap_kill: self.cfg.swap_kill,
            swap_panic: self.cfg.swap_panic,
            psi_mem_some_kill: self.cfg.psi_mem_some_kill,
            psi_mem_full_kill: self.cfg.psi_mem_full_kill,
            rss_tree_warn: self.cfg.rss_tree_warn,
            rss_tree_kill: self.cfg.rss_tree_kill,
            cpu_sys_warn: self.cfg.cpu_sys_warn,
            cpu_sys_kill: self.cfg.cpu_sys_kill,
            growth_warn_mbps: self.cfg.growth_warn_mbps,
            growth_danger_mbps: self.cfg.growth_danger_mbps,
        };

        let snap = Arc::new(Snapshot {
            t: now,
            seq: self.seq,
            version: VERSION.to_string(),
            pid: std::process::id() as i32,
            mode: self.cfg.mode.clone(),
            armed: self.cfg.armed(),
            uptime_s: self.started.elapsed().as_secs_f64(),
            interval_ms: self.cfg.interval_ms,
            config_path: self.config_path.display().to_string(),
            system,
            risk: ev.risk.clone(),
            signals: ev.signals.clone(),
            trigger: ev.trigger.clone(),
            cooldown_s: cooldown_left,
            last_kill: self.last_kill.clone(),
            events: self.events.iter().cloned().collect(),
            history: self.history.iter().cloned().collect(),
            processes,
            protected_prefixes: prefixes,
            thresholds,
        });

        // ── heartbeat ────────────────────────────────────────────────
        if now - self.last_heartbeat >= 30.0 {
            self.last_heartbeat = now;
            logger.log(
                "OK",
                &format!(
                    "cpu={:.0}% ram_avail={:.0}% swap={:.1}% risk={} {} psi_mem[s60={:.0} f10={:.0}] psi_cpu[s60={:.0}] trees={} procs={} mode={}{}",
                    snap.system.cpu_busy_pct,
                    snap.system.mem_available_pct,
                    snap.system.swap_used_pct,
                    snap.risk.score,
                    snap.risk.status,
                    snap.system.psi.mem_some60,
                    snap.system.psi.mem_full10,
                    snap.system.psi.cpu_some60,
                    snap.system.trees,
                    snap.system.procs,
                    snap.mode,
                    if snap.signals.is_empty() {
                        String::new()
                    } else {
                        format!(
                            " WARN[{}]",
                            snap.signals
                                .iter()
                                .map(|s| s.text.clone())
                                .collect::<Vec<_>>()
                                .join(", ")
                        )
                    }
                ),
            );
        }

        snap
    }

    /// Automatic kill: only the job the policy chose (a tree's focus), never
    /// the shell, terminal or sibling jobs above it.
    fn kill_job(
        &self,
        pid: i32,
        reason: &str,
        panic: bool,
        logger: &Logger,
        trees: &[policy::TreeStats],
    ) -> KillInfo {
        let job = trees.iter().filter(|t| t.killable).find_map(|t| {
            [&t.mem_job, &t.cpu_job]
                .into_iter()
                .find(|j| j.pid == pid)
                .map(|j| (t, j))
        });
        let (what, members) = match job {
            Some((t, j)) => (
                format!(
                    "{}({pid}) {} = {:.0}% of RAM, {:.0}% of CPU in tree {}({}) [{}]",
                    j.name,
                    policy::human_bytes(j.rss),
                    j.rss_pct,
                    t.cpu_machine,
                    t.name,
                    t.root,
                    j.cmd.chars().take(120).collect::<String>(),
                ),
                j.members.clone(),
            ),
            None => (format!("({pid})"), Vec::new()),
        };
        self.kill(pid, what, members, reason, panic, logger)
    }

    /// Manual kill from the UI: the whole tree the user pointed at.
    fn kill_tree(
        &self,
        root: i32,
        reason: &str,
        logger: &Logger,
        trees: &[policy::TreeStats],
    ) -> KillInfo {
        let (what, members) = match trees.iter().find(|t| t.root == root) {
            Some(t) => (format!("tree {}({root})", t.name), t.kill_members.clone()),
            None => (format!("tree ({root})"), Vec::new()),
        };
        self.kill(root, what, members, reason, false, logger)
    }

    /// SIGTERM every member, wait `grace_s`, SIGKILL survivors.
    fn kill(
        &self,
        root: i32,
        name: String,
        members: Vec<i32>,
        reason: &str,
        panic: bool,
        logger: &Logger,
    ) -> KillInfo {
        if members.is_empty() {
            return KillInfo {
                t: proc::epoch_secs(),
                pid: root,
                name,
                reason: reason.to_string(),
                dry_run: !self.cfg.armed(),
                members: 0,
                escalated: false,
            };
        }
        if !self.cfg.armed() {
            logger.log(
                "OBSERVE",
                &format!(
                    "would TERM+KILL {} process(es): {name} — {reason}",
                    members.len()
                ),
            );
            return KillInfo {
                t: proc::epoch_secs(),
                pid: root,
                name,
                reason: reason.to_string(),
                dry_run: true,
                members: members.len(),
                escalated: false,
            };
        }
        // Signal first: under thrash the log write itself can block.
        for pid in &members {
            proc::signal(*pid, libc::SIGTERM);
        }
        let label = if panic { "PANIC KILL" } else { "KILL" };
        logger.log(
            label,
            &format!("{} process(es): {name} — {reason}", members.len()),
        );
        let grace = self.cfg.grace_s.max(0.0);
        std::thread::sleep(Duration::from_secs_f64(if panic {
            grace.min(1.0)
        } else {
            grace
        }));
        let mut escalated = false;
        for pid in &members {
            if proc::alive(*pid) {
                proc::signal(*pid, libc::SIGKILL);
                escalated = true;
            }
        }
        logger.log(
            "KILL",
            &format!(
                "signalled {} member(s); escalated={escalated}",
                members.len()
            ),
        );
        KillInfo {
            t: proc::epoch_secs(),
            pid: root,
            name,
            reason: reason.to_string(),
            dry_run: false,
            members: members.len(),
            escalated,
        }
    }

    fn handle(
        &mut self,
        cmd: Command,
        cfg_shared: &Arc<RwLock<Config>>,
        shared: &Shared,
        logger: &Logger,
    ) {
        match cmd {
            Command::SetMode { mode, reply } => {
                self.cfg.mode = mode.clone();
                if let Ok(mut c) = cfg_shared.write() {
                    c.mode = mode.clone();
                }
                // Publish the new mode at once so the UI does not wait a whole
                // sampling interval to see the switch.
                if let Ok(mut w) = shared.write() {
                    let mut s = (**w).clone();
                    s.mode = mode.clone();
                    s.armed = mode == "armed";
                    *w = Arc::new(s);
                }
                let _ = config::save(&self.config_path, &self.cfg);
                self.event(
                    "mode",
                    "info",
                    format!(
                        "mode = {mode}{}",
                        if mode == "armed" {
                            " — Protector will kill when stability is in jeopardy"
                        } else {
                            " — observing only, nothing will be killed"
                        }
                    ),
                    None,
                    None,
                );
                logger.log("MODE", &format!("mode = {mode}"));
                let _ = reply.send(Ok(format!("mode = {mode}")));
            }
            Command::Kill { pid, force, reply } => {
                if !self.cfg.armed() && !force {
                    let _ = reply.send(Err(
                        "service is in observe mode — arm it first (or pass force)".to_string(),
                    ));
                    return;
                }
                let tree = self
                    .last_trees
                    .iter()
                    .find(|t| t.root == pid && t.killable)
                    .map(|t| (t.name.clone(), t.kill_members.clone(), t.rss_pct));
                match tree {
                    Some((name, members, pct)) => {
                        if members.is_empty() {
                            let _ = reply.send(Err(format!("tree {pid} has no killable members")));
                            return;
                        }
                        let reason = format!("manual kill by user ({name}, {pct:.1}% RAM)");
                        // Answer now: the TERM→KILL grace outlasts the API timeout.
                        let _ = reply.send(Ok(format!(
                            "killing {name} ({pid}), {} process(es)",
                            members.len()
                        )));
                        let info = self.kill_tree(pid, &reason, logger, &self.last_trees);
                        self.last_kill_t = self.mono();
                        self.last_kill = Some(info.clone());
                        self.event(
                            "kill",
                            "crit",
                            format!(
                                "manual kill: {name} ({pid}) — {} process(es)",
                                members.len()
                            ),
                            Some(pid),
                            Some(name.clone()),
                        );
                    }
                    None => {
                        let _ = reply.send(Err(format!(
                            "no killable tree with pid {pid} (protected, other user, or gone)"
                        )));
                    }
                }
            }
            Command::Protect { pid, prefix, reply } => {
                let mut what = Vec::new();
                if let Some(p) = pid {
                    self.extra_pids.insert(p);
                    what.push(format!("pid {p}"));
                }
                if let Some(pfx) = prefix {
                    if !pfx.is_empty() {
                        if !self.cfg.extra_protected.contains(&pfx) {
                            self.cfg.extra_protected.push(pfx.clone());
                        }
                        what.push(format!("prefix '{pfx}'"));
                    }
                }
                if let Ok(mut c) = cfg_shared.write() {
                    c.extra_protected = self.cfg.extra_protected.clone();
                }
                let _ = config::save(&self.config_path, &self.cfg);
                let text = format!("protected {}", what.join(", "));
                self.event("protect", "info", text.clone(), pid, None);
                let _ = reply.send(Ok(text));
            }
        }
    }
}

// ── args ─────────────────────────────────────────────────────────────

struct Args {
    home: Option<PathBuf>,
    config: Option<PathBuf>,
    mode: Option<String>,
    interval_ms: Option<u64>,
    port: Option<u16>,
    token: Option<String>,
    print: bool,
    no_log: bool,
    help: bool,
    version: bool,
}

fn parse_args() -> Args {
    let mut a = Args {
        home: None,
        config: None,
        mode: None,
        interval_ms: None,
        port: None,
        token: None,
        print: false,
        no_log: false,
        help: false,
        version: false,
    };
    let mut it = std::env::args().skip(1);
    while let Some(arg) = it.next() {
        let (key, inline) = match arg.split_once('=') {
            Some((k, v)) => (k.to_string(), Some(v.to_string())),
            None => (arg.clone(), None),
        };
        let mut val = || inline.clone().or_else(|| it.next());
        match key.as_str() {
            "--home" => a.home = val().map(PathBuf::from),
            "--config" => a.config = val().map(PathBuf::from),
            "--mode" => a.mode = val(),
            "--armed" => a.mode = Some("armed".to_string()),
            "--observe" | "--dry-run" => a.mode = Some("observe".to_string()),
            "--interval" | "--interval-ms" => a.interval_ms = val().and_then(|v| v.parse().ok()),
            "--port" => a.port = val().and_then(|v| v.parse().ok()),
            "--token" => a.token = val(),
            "--print" | "--once" => a.print = true,
            "--no-log" => a.no_log = true,
            "--version" | "-V" => a.version = true,
            "--help" | "-h" => a.help = true,
            _ => {}
        }
    }
    a
}

fn print_help() {
    println!(
        "protectord {VERSION} — PC stability protector\n\
\n\
Usage: protectord [options]\n\
\n\
  --mode=observe|armed   observe (default) logs what it would kill; armed kills\n\
  --armed / --observe    shorthand for --mode\n\
  --interval=MS          sampling cadence (default 1000)\n\
  --port=N               loopback API port (default 7801; 0 = auto)\n\
  --home=DIR             state dir (default $PROTECTOR_HOME or ~/.protector)\n\
  --config=PATH          config file (default <home>/config.json)\n\
  --token=TOK            override the API bearer token\n\
  --print                sample once, print the snapshot JSON, exit\n\
  --no-log               do not write the log file\n\
  -V, --version          print version\n\
  -h, --help             this help\n"
    );
}

// ── main ─────────────────────────────────────────────────────────────

fn main() {
    let args = parse_args();
    if args.version {
        println!("protectord {VERSION}");
        return;
    }
    if args.help {
        print_help();
        return;
    }

    let home = args
        .home
        .clone()
        .or_else(|| std::env::var("PROTECTOR_HOME").ok().map(PathBuf::from))
        .unwrap_or_else(config::home_dir);
    let config_path = args
        .config
        .clone()
        .unwrap_or_else(|| config::config_path(&home));

    let mut cfg = match config::load(&home, &config_path) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("protectord: {e}");
            std::process::exit(2);
        }
    };
    if let Some(m) = &args.mode {
        cfg.mode = m.clone();
    }
    if let Some(i) = args.interval_ms {
        cfg.interval_ms = i.max(100);
    }
    if let Some(p) = args.port {
        cfg.port = p;
    }
    // An empty token would switch the API's auth off.
    if let Some(t) = args.token.as_ref().filter(|t| !t.is_empty()) {
        cfg.token = t.clone();
    }
    let _ = config::save(&config_path, &cfg);

    let logger = if args.no_log {
        Logger::silent()
    } else {
        Logger::new(config::log_path(&home))
    };

    // Stay in RAM: the moment this daemon is needed is the moment its own
    // pages would be swapped out. ONFAULT locks what is used, not every
    // reserved stack and arena.
    let locked =
        unsafe { libc::mlockall(libc::MCL_CURRENT | libc::MCL_FUTURE | libc::MCL_ONFAULT) } == 0;

    let user_map = proc::user_map();
    let me_uid = unsafe { libc::getuid() };

    let (fault_tx, faults) = channel();

    let mut inner = Inner {
        cfg: cfg.clone(),
        config_path: config_path.clone(),
        started_epoch: proc::epoch_secs(),
        started: Instant::now(),
        seq: 0,
        page: proc::page_size(),
        clk: proc::clock_ticks(),
        nproc: proc::num_cpus(),
        me_uid,
        user_map,
        events: VecDeque::new(),
        history: VecDeque::new(),
        prev_ticks: HashMap::new(),
        prev_t: None,
        prev_total: 0,
        prev_idle: 0,
        ema: HashMap::new(),
        hold: HashMap::new(),
        last_kill_t: f64::NEG_INFINITY,
        last_kill: None,
        last_hold: None,
        extra_pids: HashSet::new(),
        last_trees: Vec::new(),
        last_heartbeat: 0.0,
        faults,
        kernel: None,
        last_alert_t: 0.0,
        lag_s: 0.0,
        cmds: HashMap::new(),
        reclaim_stalls: None,
        last_reclaim_t: f64::NEG_INFINITY,
        reboot_at: None,
    };

    if args.print {
        // One sample, no server, no runtime file, no log noise on stdout.
        inner.last_heartbeat = proc::epoch_secs();
        let snap = inner.sample(&Logger::silent());
        match serde_json::to_string_pretty(&*snap) {
            Ok(s) => println!("{s}"),
            Err(e) => eprintln!("protectord: {e}"),
        }
        return;
    }

    // ── bind the API first so the runtime file carries the real port ──
    let addr = format!("127.0.0.1:{}", cfg.port);
    let server = match tiny_http::Server::http(&addr) {
        Ok(s) => s,
        Err(_) => {
            // Fixed port taken (another instance?) — fall back to an OS port.
            match tiny_http::Server::http("127.0.0.1:0") {
                Ok(s) => s,
                Err(e) => {
                    eprintln!("protectord: cannot bind API: {e}");
                    std::process::exit(1);
                }
            }
        }
    };
    let port = server
        .server_addr()
        .to_ip()
        .map(|a| a.port())
        .unwrap_or(cfg.port);

    // ── runtime file ─────────────────────────────────────────────────
    let runtime = serde_json::json!({
        "pid": std::process::id(),
        "port": port,
        "token": cfg.token,
        "version": VERSION,
        "mode": cfg.mode,
        "started": inner.started_epoch,
        "config_path": config_path.display().to_string(),
    });
    let runtime_path = config::runtime_path(&home);
    if let Some(dir) = runtime_path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let _ = std::fs::write(&runtime_path, runtime.to_string());
    config::set_private(&runtime_path, config::FILE_MODE);
    config::set_private(&home, config::DIR_MODE);

    // ── signals ──────────────────────────────────────────────────────
    unsafe {
        libc::signal(libc::SIGINT, on_signal as *const () as libc::sighandler_t);
        libc::signal(libc::SIGTERM, on_signal as *const () as libc::sighandler_t);
        libc::signal(libc::SIGHUP, on_signal as *const () as libc::sighandler_t);
    }

    let shared: Shared = Arc::new(RwLock::new({
        inner.last_heartbeat = proc::epoch_secs();
        inner.sample(&logger)
    }));
    let cfg_shared = Arc::new(RwLock::new(cfg.clone()));
    let (tx, rx): (Sender<Command>, Receiver<Command>) = channel();

    // ── API thread ───────────────────────────────────────────────────
    {
        let shared = shared.clone();
        let cfg_shared = cfg_shared.clone();
        let config_path_str = config_path.display().to_string();
        std::thread::spawn(move || {
            api::run(server, shared, tx, cfg_shared, config_path_str);
        });
    }

    // ── kernel-log thread (only what happens from now on) ────────────
    let kmsg = std::env::var("PROTECTOR_KMSG").unwrap_or_else(|_| "/dev/kmsg".to_string());
    match std::fs::File::open(&kmsg) {
        Ok(mut f) => {
            use std::io::Seek;
            let _ = f.seek(std::io::SeekFrom::End(0));
            std::thread::spawn(move || kernel::watch(f, fault_tx));
        }
        Err(e) => logger.log(
            "WARN",
            &format!("kernel stalls will go unseen — {kmsg}: {e}"),
        ),
    }

    logger.log(
        "START",
        &format!(
            "protectord {VERSION} pid={} port={} mode={} interval={}ms nproc={} home={}",
            std::process::id(),
            port,
            cfg.mode,
            cfg.interval_ms,
            inner.nproc,
            home.display()
        ),
    );
    if !locked {
        logger.log(
            "WARN",
            "mlockall refused — the daemon may be swapped out under pressure",
        );
    }
    // The policy, stated once at boot so the log answers "what will it do?".
    logger.log(
        "START",
        &format!(
            "policy: kill one job, only when tasks are stalled on machine-wide reclaim and: mem_avail<{}% with swap>{}% ({}s; panic <{}% {}s) | mem_avail<{}% with PSI mem full10>{} ({}s); or OS starved of CPU {}s; a full swap, busy CPU, big process or low RAM alone never; cooldown {}s; kernel stall: alert{}; mode {}",
            inner.cfg.mem_avail_kill,
            inner.cfg.swap_panic,
            inner.cfg.pressure_kill_sustain as i64,
            inner.cfg.mem_avail_panic,
            inner.cfg.panic_sustain as i64,
            inner.cfg.mem_avail_warn,
            inner.cfg.psi_mem_full_panic,
            inner.cfg.mem_avail_kill_sustain as i64,
            inner.cfg.cpu_sys_kill_sustain as i64,
            inner.cfg.cooldown_s,
            match inner.cfg.stall_reboot_s {
                s if s > 0.0 => format!(" + clean reboot after {s:.0}s"),
                _ => String::new(),
            },
            inner.cfg.mode,
        ),
    );
    inner.event(
        "start",
        "info",
        format!(
            "protectord {VERSION} started on port {port} in {} mode",
            inner.cfg.mode
        ),
        None,
        None,
    );

    // ── sampler loop ─────────────────────────────────────────────────
    let interval = Duration::from_millis(inner.cfg.interval_ms.max(100));
    loop {
        if STOP.load(Ordering::SeqCst) {
            break;
        }
        // Apply any pending commands before sampling.
        while let Ok(cmd) = rx.try_recv() {
            inner.handle(cmd, &cfg_shared, &shared, &logger);
        }
        let snap = inner.sample(&logger);
        if let Ok(mut w) = shared.write() {
            *w = snap;
        }
        let slept = Instant::now();
        inner.lag_s = 0.0;
        match rx.recv_timeout(interval) {
            Ok(cmd) => inner.handle(cmd, &cfg_shared, &shared, &logger),
            Err(RecvTimeoutError::Timeout) => {
                inner.lag_s = (slept.elapsed().as_secs_f64() - interval.as_secs_f64()).max(0.0);
            }
            Err(RecvTimeoutError::Disconnected) => break,
        }
    }

    logger.log("STOP", "protectord stopping");
    // Remove the runtime file only if it is still ours.
    if let Ok(text) = std::fs::read_to_string(&runtime_path) {
        if let Ok(v) = serde_json::from_str::<serde_json::Value>(&text) {
            if v.get("pid").and_then(|p| p.as_u64()) == Some(std::process::id() as u64) {
                let _ = std::fs::remove_file(&runtime_path);
            }
        }
    }
}
