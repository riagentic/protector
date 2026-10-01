//! JSON shapes the daemon publishes. Field names are stable — the aio app reads
//! them by name, so renaming one is a wire change.

use serde::Serialize;

#[derive(Serialize, Clone)]
pub struct Snapshot {
    /// Epoch seconds when this sample was taken.
    pub t: f64,
    pub seq: u64,
    pub version: String,
    pub pid: i32,
    pub mode: String,
    pub armed: bool,
    pub uptime_s: f64,
    pub interval_ms: u64,
    pub config_path: String,
    pub system: System,
    pub risk: Risk,
    pub signals: Vec<Signal>,
    pub trigger: Trigger,
    /// Seconds left before a non-panic kill may fire again (0 = ready).
    pub cooldown_s: f64,
    pub last_kill: Option<KillInfo>,
    pub events: Vec<Event>,
    pub history: Vec<Sample>,
    pub processes: Vec<ProcView>,
    pub protected_prefixes: Vec<String>,
    pub thresholds: Thresholds,
}

#[derive(Serialize, Clone)]
pub struct System {
    pub mem_total: u64,
    pub mem_avail: u64,
    pub mem_available_pct: f64,
    pub mem_used_pct: f64,
    pub swap_total: u64,
    pub swap_used: u64,
    pub swap_used_pct: f64,
    pub cpu_busy_pct: f64,
    pub nproc: usize,
    pub load1: f64,
    pub load5: f64,
    pub load15: f64,
    pub psi: Psi,
    pub d_state: usize,
    /// Seconds the sampler overslept its interval — the OS starved of CPU.
    pub sampler_lag_s: f64,
    /// A task had to reclaim memory for the whole machine in the last 10 s.
    pub direct_reclaim: bool,
    pub procs: usize,
    pub trees: usize,
    pub threads: usize,
    pub largest_tree_pct: f64,
}

#[derive(Serialize, Clone, Default)]
pub struct Psi {
    pub mem_some10: f64,
    pub mem_some60: f64,
    pub mem_full10: f64,
    pub mem_full60: f64,
    pub cpu_some10: f64,
    pub cpu_some60: f64,
    pub io_some10: f64,
    pub io_some60: f64,
    pub io_full10: f64,
    pub io_full60: f64,
}

#[derive(Serialize, Clone)]
pub struct Risk {
    /// 0–100: how close the machine is to a freeze.
    pub score: u32,
    /// OK · WATCH · CRITICAL · PANIC
    pub status: String,
    pub reasons: Vec<String>,
}

#[derive(Serialize, Clone)]
pub struct Signal {
    /// info · warn · danger · crit
    pub severity: String,
    pub key: String,
    pub text: String,
}

#[derive(Serialize, Clone)]
pub struct Trigger {
    pub active: bool,
    pub reason: Option<String>,
    pub panic: bool,
    pub victim_pid: Option<i32>,
    pub victim_name: Option<String>,
}

impl Trigger {
    pub fn idle() -> Self {
        Trigger {
            active: false,
            reason: None,
            panic: false,
            victim_pid: None,
            victim_name: None,
        }
    }
}

#[derive(Serialize, Clone)]
pub struct KillInfo {
    pub t: f64,
    pub pid: i32,
    pub name: String,
    pub reason: String,
    pub dry_run: bool,
    pub members: usize,
    pub escalated: bool,
}

#[derive(Serialize, Clone)]
pub struct Event {
    pub seq: u64,
    pub t: f64,
    /// start · mode · kill · hold · warn · protect · panic · kernel · reboot · error
    pub kind: String,
    /// info · warn · crit
    pub severity: String,
    pub text: String,
    pub pid: Option<i32>,
    pub name: Option<String>,
}

#[derive(Serialize, Clone)]
pub struct Sample {
    pub t: f64,
    pub mem_available_pct: f64,
    pub swap_used_pct: f64,
    pub cpu_busy_pct: f64,
    pub risk: u32,
}

#[derive(Serialize, Clone)]
pub struct ProcView {
    pub pid: i32,
    pub name: String,
    pub cmd: String,
    pub uid: u32,
    pub user: String,
    pub protected: bool,
    pub killable: bool,
    pub rss: u64,
    pub rss_pct: f64,
    pub cpu_core: f64,
    pub cpu_machine: f64,
    pub grow_bps: f64,
    pub threads: u64,
    pub d_state: usize,
    pub oom_score: i64,
    pub age_s: f64,
    pub nproc: usize,
    pub members: usize,
    pub harm: u32,
    pub signals: Vec<Signal>,
}

/// The subset of thresholds the UI shows so a user can see why a number is red.
#[derive(Serialize, Clone)]
pub struct Thresholds {
    pub mem_avail_warn: f64,
    pub mem_avail_kill: f64,
    pub swap_warn: f64,
    pub swap_kill: f64,
    pub swap_panic: f64,
    pub psi_mem_some_kill: f64,
    pub psi_mem_full_kill: f64,
    pub rss_tree_warn: f64,
    pub rss_tree_kill: f64,
    pub cpu_sys_warn: f64,
    pub cpu_sys_kill: f64,
    pub growth_warn_mbps: f64,
    pub growth_danger_mbps: f64,
}
