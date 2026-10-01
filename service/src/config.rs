//! Configuration: defaults, an optional JSON file, and the runtime token.
//!
//! Everything that decides when Protector acts lives here so the policy can be
//! tuned without touching code. WARN is broad; KILL is narrow and fires only
//! when the machine is about to lock up (see the README's Policy section).

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Shells and multiplexers: they wrap jobs, they are never the job. A kill
/// that would land on one moves on to its heaviest child instead.
pub const WRAPPERS: &[&str] = &["sh", "bash", "dash", "zsh", "fish", "tmux", "screen"];

/// Process-name prefixes that must never be signalled — PID 1, kernel threads,
/// the desktop session, audio, network, the package manager, and the agent
/// harness itself. Extra prefixes can be appended via config.
pub const DEFAULT_PROTECTED_PREFIXES: &[&str] = &[
    "systemd",
    "init",
    "kthreadd",
    "kworker",
    "ksoftirqd",
    "kthread",
    "migration",
    "rcu_",
    "watchdog/",
    "idle",
    "cpuhp",
    "kauditd",
    "Xorg",
    "Xwayland",
    "xwayland",
    "cinnamon",
    "gnome-shell",
    "gnome-session",
    "gnome-terminal",
    "vte-spawn",
    "xdg-desktop-portal",
    "gnome-keyring",
    "gsd-",
    "dbus-daemon",
    "dbus-broker",
    "systemd-logind",
    "systemd-journal",
    "systemd-udevd",
    "systemd-timesyn",
    "systemd-resolve",
    "systemd-network",
    "pulseaudio",
    "pipewire",
    "wireplumber",
    "rtkit-daemon",
    "NetworkManager",
    "ModemManager",
    "wpa_supplicant",
    "sshd",
    "gdm",
    "sudo",
    "polkitd",
    "upowerd",
    "colord",
    "cupsd",
    "thermald",
    "irqbalance",
    "unattended-upgr",
    "snapd",
    "containerd",
    "dockerd",
    "aptd",
    "packagekitd",
    "fwupd",
    "avahi-daemon",
    "nvidia-",
    "irq/",
    "acpi_",
    "scsi_",
    "kblockd",
    "kdmflush",
    "opencode",
    "claude",
    "ricon",
    "protectord",
    "electron",
];

#[derive(Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    /// "observe" (log what it *would* do) or "armed" (actually kill).
    pub mode: String,
    /// Sampling cadence.
    pub interval_ms: u64,
    /// Loopback port. 0 asks the OS; a taken fixed port falls back to 0.
    pub port: u16,
    /// Bearer token for the loopback API. Generated on first run.
    pub token: String,
    /// Only ever signal processes owned by this user.
    pub only_user: bool,
    /// Protect this service and its ancestry.
    pub protect_self: bool,
    /// Extra process-name prefixes that must never be signalled.
    pub extra_protected: Vec<String>,
    /// Seconds SIGTERM is given before SIGKILL.
    pub grace_s: f64,
    /// Minimum seconds between non-panic kills.
    pub cooldown_s: f64,
    /// A trigger only fires when the job it would kill holds at least this
    /// percentage of RAM — killing something smaller would not help.
    pub min_victim_rss_pct: f64,
    /// How many history samples / events to keep for the UI.
    pub history: usize,
    pub events: usize,
    /// How many trees the API returns (largest by harm first).
    pub top_limit: usize,

    // ── memory: the real freeze cause ────────────────────────────────
    pub mem_avail_warn: f64,
    pub mem_avail_kill: f64,
    /// Seconds the gridlock rule (RAM scarce, PSI full10 > psi_mem_full_panic)
    /// must hold. The name predates the rule; kept for config compatibility.
    pub mem_avail_kill_sustain: f64,
    pub mem_avail_panic: f64,
    pub swap_warn: f64,
    pub swap_kill: f64,
    pub swap_panic: f64,
    pub psi_mem_some_warn: f64,
    pub psi_mem_some_kill: f64,
    pub psi_mem_full_warn: f64,
    pub psi_mem_full_kill: f64,
    pub psi_mem_full_panic: f64,
    /// Seconds RAM and swap must both stay exhausted before a kill.
    pub pressure_kill_sustain: f64,
    /// Seconds a panic condition must hold: one bad sample is not a freeze.
    pub panic_sustain: f64,
    pub rss_tree_warn: f64,
    pub rss_tree_kill: f64,

    // ── cpu: a busy CPU never kills; only an OS starved of it does ───
    pub cpu_sys_warn: f64,
    pub cpu_sys_kill: f64,
    pub cpu_sys_kill_sustain: f64,
    pub psi_cpu_some_kill: f64,
    /// Seconds the sampler must oversleep before the OS counts as starved.
    pub starve_lag_s: f64,
    /// A starvation trigger only fires when one tree burns at least this
    /// percentage of the whole machine's CPU.
    pub min_victim_cpu_pct: f64,

    // ── memory-growth signal (per tree) ──────────────────────────────
    /// MB/s at which a tree's growth component reaches 100.
    pub growth_kill_mbps: f64,
    /// MB/s above which growth is worth flagging (warn / danger).
    pub growth_warn_mbps: f64,
    pub growth_danger_mbps: f64,
    /// Tasks in uninterruptible sleep before the system flag is danger.
    pub d_state_crit: usize,

    // ── kernel stalls: no process to kill, so alert (and optionally reboot) ──
    /// Seconds a kernel fault stays an active signal after its last log line.
    pub kernel_window_s: f64,
    /// Seconds a kernel stall must last before `sync` + a clean reboot (armed
    /// mode only). 0 = never reboot.
    pub stall_reboot_s: f64,
    /// Seconds a danger/crit signal must hold before a desktop alert.
    pub alert_sustain_s: f64,
    /// Minimum seconds between desktop alerts.
    pub alert_repeat_s: f64,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            mode: "observe".to_string(),
            interval_ms: 1000,
            port: 7801,
            token: String::new(),
            only_user: true,
            protect_self: true,
            extra_protected: Vec::new(),
            grace_s: 5.0,
            cooldown_s: 45.0,
            min_victim_rss_pct: 5.0,
            history: 180,
            events: 250,
            top_limit: 200,

            mem_avail_warn: 15.0,
            mem_avail_kill: 5.0,
            mem_avail_kill_sustain: 30.0,
            mem_avail_panic: 2.0,
            swap_warn: 25.0,
            swap_kill: 60.0,
            swap_panic: 85.0,
            psi_mem_some_warn: 10.0,
            psi_mem_some_kill: 40.0,
            psi_mem_full_warn: 5.0,
            psi_mem_full_kill: 15.0,
            psi_mem_full_panic: 30.0,
            pressure_kill_sustain: 10.0,
            panic_sustain: 3.0,
            rss_tree_warn: 45.0,
            rss_tree_kill: 65.0,

            cpu_sys_warn: 90.0,
            cpu_sys_kill: 98.0,
            cpu_sys_kill_sustain: 60.0,
            psi_cpu_some_kill: 40.0,
            starve_lag_s: 2.0,
            min_victim_cpu_pct: 50.0,

            growth_kill_mbps: 150.0,
            growth_warn_mbps: 5.0,
            growth_danger_mbps: 25.0,
            d_state_crit: 10,

            kernel_window_s: 300.0,
            stall_reboot_s: 0.0,
            alert_sustain_s: 10.0,
            alert_repeat_s: 300.0,
        }
    }
}

impl Config {
    pub fn armed(&self) -> bool {
        self.mode == "armed"
    }

    /// All protected prefixes: built-ins + config extras.
    pub fn protected_prefixes(&self) -> Vec<String> {
        let mut v: Vec<String> = DEFAULT_PROTECTED_PREFIXES
            .iter()
            .map(|s| s.to_string())
            .collect();
        v.extend(self.extra_protected.iter().cloned());
        v
    }
}

/// Resolve `PROTECTOR_HOME` or `~/.protector`.
pub fn home_dir() -> PathBuf {
    if let Ok(h) = std::env::var("PROTECTOR_HOME") {
        if !h.is_empty() {
            return PathBuf::from(h);
        }
    }
    if let Ok(h) = std::env::var("HOME") {
        if !h.is_empty() {
            return Path::new(&h).join(".protector");
        }
    }
    PathBuf::from(".protector")
}

pub fn config_path(home: &Path) -> PathBuf {
    home.join("config.json")
}

/// Set a file/dir mode explicitly (best effort; no-op off Unix). The daemon
/// holds a bearer token, so it must not rely on the umask — a stock `022` umask
/// would leave `runtime.json` world-readable.
pub fn set_private(path: &Path, mode: u32) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode));
    }
    #[cfg(not(unix))]
    {
        let _ = (path, mode);
    }
}

pub const DIR_MODE: u32 = 0o700;
pub const FILE_MODE: u32 = 0o600;

pub fn runtime_path(home: &Path) -> PathBuf {
    home.join("runtime.json")
}

pub fn log_path(home: &Path) -> PathBuf {
    home.join("protector.log")
}

/// Load config from `path`, creating it with defaults (and a fresh token) if it
/// does not exist. Unknown/missing fields fall back to defaults.
pub fn load(home: &Path, path: &Path) -> Result<Config, String> {
    std::fs::create_dir_all(home).map_err(|e| format!("create {}: {e}", home.display()))?;
    set_private(home, DIR_MODE);
    let mut cfg = if path.exists() {
        let text = std::fs::read_to_string(path).map_err(|e| format!("read config: {e}"))?;
        serde_json::from_str::<Config>(&text).map_err(|e| format!("parse config: {e}"))?
    } else {
        Config::default()
    };
    if cfg.token.is_empty() {
        cfg.token = random_token();
    }
    // Persist so the UI (and the next run) can read the token and any edits.
    if !path.exists() {
        save(path, &cfg)?;
    }
    Ok(cfg)
}

pub fn save(path: &Path, cfg: &Config) -> Result<(), String> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| format!("create {}: {e}", dir.display()))?;
    }
    // Write aside and rename: a reset mid-write must not leave a truncated
    // file the daemon then refuses to start on.
    let text = serde_json::to_string_pretty(cfg).map_err(|e| e.to_string())?;
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, text).map_err(|e| format!("write config: {e}"))?;
    set_private(&tmp, FILE_MODE);
    std::fs::rename(&tmp, path).map_err(|e| format!("write config: {e}"))
}

fn random_token() -> String {
    use std::io::Read;
    let mut buf = [0u8; 16];
    let ok = std::fs::File::open("/dev/urandom")
        .and_then(|mut f| f.read_exact(&mut buf))
        .is_ok();
    if !ok {
        // Extremely unlikely fallback: mix the pid and a clock reading.
        let t = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let p = std::process::id() as u128;
        let mix = t ^ (p << 64);
        buf = mix.to_le_bytes();
    }
    buf.iter().map(|b| format!("{b:02x}")).collect()
}
