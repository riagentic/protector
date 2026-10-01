//! Reading the Linux kernel's own view of the machine: `/proc` and
//! `/proc/pressure`. No external crates — this is the whole OS surface
//! Protector needs.

use std::collections::HashMap;
use std::fs;

#[derive(Clone, Debug)]
pub struct ProcInfo {
    pub pid: i32,
    pub ppid: i32,
    pub comm: String,
    pub state: char,
    pub uid: u32,
    pub rss: u64,
    /// Memory only this process holds and a kill gives back: anonymous RSS +
    /// swap. Shared and file-backed pages are left out — summed over a tree
    /// they would count one segment once per process.
    pub held: u64,
    /// The swapped-out part of `held`.
    pub swap: u64,
    /// Resident shared memory (tmpfs, memfd, shared anonymous maps).
    pub shmem: u64,
    pub ticks: u64,
    pub starttime: u64,
    pub threads: u64,
    pub oom_score: i64,
    pub cmd: String,
}

pub fn page_size() -> u64 {
    unsafe { libc::sysconf(libc::_SC_PAGESIZE) as u64 }
}

pub fn clock_ticks() -> u64 {
    unsafe { libc::sysconf(libc::_SC_CLK_TCK) as u64 }
}

pub fn num_cpus() -> usize {
    std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1)
}

fn read(path: &str) -> Option<String> {
    fs::read_to_string(path).ok()
}

/// Command lines by pid, valid while (starttime, comm) match. Reading
/// `/proc/<pid>/cmdline` takes that process's mmap lock, which a thrashing
/// process holds for seconds — the sampler must not wait on it every tick.
pub type CmdCache = HashMap<i32, (u64, String, String)>;

/// `pid -> ProcInfo` for every readable process.
pub fn read_procs(page: u64, cmds: &mut CmdCache) -> Vec<ProcInfo> {
    let mut out = Vec::with_capacity(512);
    let entries = match fs::read_dir("/proc") {
        Ok(e) => e,
        Err(_) => return out,
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        let pid: i32 = match name.parse() {
            Ok(p) => p,
            Err(_) => continue,
        };
        if let Some(p) = read_one(pid, page, cmds) {
            out.push(p);
        }
    }
    let live: std::collections::HashSet<i32> = out.iter().map(|p| p.pid).collect();
    cmds.retain(|pid, _| live.contains(pid));
    out
}

/// One `kB` field of `/proc/<pid>/status`, in bytes.
fn status_kb(status: &str, key: &str) -> u64 {
    status
        .lines()
        .find_map(|l| l.strip_prefix(key))
        .and_then(|v| v.split_whitespace().next()?.parse::<u64>().ok())
        .unwrap_or(0)
        * 1024
}

fn read_one(pid: i32, page: u64, cmds: &mut CmdCache) -> Option<ProcInfo> {
    let stat = read(&format!("/proc/{pid}/stat"))?;
    let open = stat.find('(')?;
    let close = stat.rfind(')')?;
    if close <= open {
        return None;
    }
    let comm = stat[open + 1..close].to_string();
    let rest: Vec<&str> = stat[close + 1..].split_whitespace().collect();
    if rest.len() < 22 {
        return None;
    }
    let state = rest[0].chars().next().unwrap_or('?');
    let ppid = rest[1].parse::<i32>().ok()?;
    let utime = rest[11].parse::<u64>().ok()?;
    let stime = rest[12].parse::<u64>().ok()?;
    let threads = rest[17].parse::<u64>().unwrap_or(1);
    let starttime = rest[19].parse::<u64>().ok()?;
    let rss_pages = rest[21].parse::<i64>().unwrap_or(0).max(0) as u64;

    let uid = fs::metadata(format!("/proc/{pid}"))
        .map(|m| {
            use std::os::unix::fs::MetadataExt;
            m.uid()
        })
        .unwrap_or(u32::MAX);

    let oom_score = read(&format!("/proc/{pid}/oom_score"))
        .and_then(|s| s.trim().parse::<i64>().ok())
        .unwrap_or(0);

    let (held, swap, shmem) = read(&format!("/proc/{pid}/status"))
        .map(|s| {
            let swap = status_kb(&s, "VmSwap:");
            (
                status_kb(&s, "RssAnon:") + swap,
                swap,
                status_kb(&s, "RssShmem:"),
            )
        })
        .unwrap_or((0, 0, 0));

    // cmdline is NUL-separated; kernel threads have none so fall back to comm.
    let cmd = match cmds.get(&pid) {
        Some((start, name, cmd)) if *start == starttime && *name == comm => cmd.clone(),
        _ => {
            let cmd = read(&format!("/proc/{pid}/cmdline"))
                .map(|s| {
                    let joined = s.replace('\0', " ");
                    let joined = joined.trim();
                    if joined.is_empty() {
                        format!("[{comm}]")
                    } else {
                        joined.chars().take(260).collect()
                    }
                })
                .unwrap_or_else(|| format!("[{comm}]"));
            cmds.insert(pid, (starttime, comm.clone(), cmd.clone()));
            cmd
        }
    };

    Some(ProcInfo {
        pid,
        ppid,
        comm,
        state,
        uid,
        rss: rss_pages * page,
        held,
        swap,
        shmem,
        ticks: utime + stime,
        starttime,
        threads,
        oom_score,
        cmd,
    })
}

#[derive(Clone, Debug, Default)]
pub struct MemInfo {
    pub total: u64,
    pub available: u64,
    pub swap_total: u64,
    pub swap_free: u64,
}

/// All zero when the file, or its MemAvailable line, cannot be read — the
/// kernel itself reports 0 available in deep exhaustion, so a missing value
/// must not look like that.
pub fn read_meminfo() -> MemInfo {
    let mut m = MemInfo::default();
    let mut seen = false;
    if let Some(text) = read("/proc/meminfo") {
        for line in text.lines() {
            let mut it = line.split_whitespace();
            let key = it.next().unwrap_or("");
            let val: u64 = match it.next().and_then(|v| v.parse().ok()) {
                Some(v) => v,
                None => continue,
            };
            let bytes = val * 1024;
            match key {
                "MemTotal:" => m.total = bytes,
                "MemAvailable:" => {
                    m.available = bytes;
                    seen = true;
                }
                "SwapTotal:" => m.swap_total = bytes,
                "SwapFree:" => m.swap_free = bytes,
                _ => {}
            }
        }
    }
    if seen {
        m
    } else {
        MemInfo::default()
    }
}

/// Times a task had to reclaim memory itself because the machine as a whole
/// had none free (`allocstall_*` in `/proc/vmstat`). A cgroup hitting its own
/// limit does not count. None if unreadable.
pub fn read_reclaim_stalls() -> Option<u64> {
    let text = read("/proc/vmstat")?;
    let mut hits = text
        .lines()
        .filter(|l| l.starts_with("allocstall"))
        .filter_map(|l| l.split_whitespace().nth(1)?.parse::<u64>().ok())
        .peekable();
    hits.peek()?;
    Some(hits.sum())
}

#[derive(Clone, Copy, Debug, Default)]
pub struct Pressure {
    pub some10: f64,
    pub some60: f64,
    pub full10: f64,
    pub full60: f64,
}

/// Parse one `/proc/pressure/<res>` file.
pub fn read_pressure(res: &str) -> Pressure {
    let mut p = Pressure::default();
    let text = match read(&format!("/proc/pressure/{res}")) {
        Some(t) => t,
        None => return p,
    };
    for line in text.lines() {
        let mut parts = line.split_whitespace();
        let kind = parts.next().unwrap_or("");
        let mut avg10 = 0.0;
        let mut avg60 = 0.0;
        for kv in parts {
            if let Some(v) = kv.strip_prefix("avg10=") {
                avg10 = v.parse().unwrap_or(0.0);
            } else if let Some(v) = kv.strip_prefix("avg60=") {
                avg60 = v.parse().unwrap_or(0.0);
            }
        }
        match kind {
            "some" => {
                p.some10 = avg10;
                p.some60 = avg60;
            }
            "full" => {
                p.full10 = avg10;
                p.full60 = avg60;
            }
            _ => {}
        }
    }
    p
}

/// Cumulative (total, idle) jiffies from `/proc/stat` line 1.
pub fn read_cpu_jiffies() -> (u64, u64) {
    if let Some(text) = read("/proc/stat") {
        if let Some(line) = text.lines().next() {
            let vals: Vec<u64> = line
                .split_whitespace()
                .skip(1)
                .filter_map(|v| v.parse().ok())
                .collect();
            if vals.len() >= 5 {
                let idle = vals[3] + vals[4]; // idle + iowait
                let total: u64 = vals.iter().sum();
                return (total, idle);
            }
        }
    }
    (0, 0)
}

pub fn read_loadavg() -> (f64, f64, f64) {
    if let Some(text) = read("/proc/loadavg") {
        let mut it = text.split_whitespace();
        let a = it.next().and_then(|v| v.parse().ok()).unwrap_or(0.0);
        let b = it.next().and_then(|v| v.parse().ok()).unwrap_or(0.0);
        let c = it.next().and_then(|v| v.parse().ok()).unwrap_or(0.0);
        return (a, b, c);
    }
    (0.0, 0.0, 0.0)
}

/// uid → login name, read once from `/etc/passwd`.
pub fn user_map() -> HashMap<u32, String> {
    let mut map = HashMap::new();
    if let Some(text) = read("/etc/passwd") {
        for line in text.lines() {
            let mut it = line.split(':');
            let name = it.next().unwrap_or("");
            let _ = it.next();
            if let Some(uid) = it.next().and_then(|v| v.parse::<u32>().ok()) {
                map.insert(uid, name.to_string());
            }
        }
    }
    map
}

/// PIDs from `pid` up to (and excluding) 1 — used to shield the service and
/// anything the app asks to shield.
pub fn ancestry(pid: i32) -> Vec<i32> {
    let mut chain = Vec::new();
    let mut cur = pid;
    let mut seen = 0;
    while cur > 1 && seen < 64 {
        chain.push(cur);
        let stat = match read(&format!("/proc/{cur}/stat")) {
            Some(s) => s,
            None => break,
        };
        let close = match stat.rfind(')') {
            Some(c) => c,
            None => break,
        };
        let rest: Vec<&str> = stat[close + 1..].split_whitespace().collect();
        let ppid = rest.get(1).and_then(|v| v.parse::<i32>().ok());
        match ppid {
            Some(p) => cur = p,
            None => break,
        }
        seen += 1;
    }
    chain.push(1);
    chain
}

pub fn epoch_secs() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

/// Seconds since boot, from `/proc/uptime`.
pub fn read_uptime() -> f64 {
    read("/proc/uptime")
        .and_then(|s| {
            s.split_whitespace()
                .next()
                .and_then(|v| v.parse::<f64>().ok())
        })
        .unwrap_or(0.0)
}

/// Send a signal to one pid; ignore "no such process" and permission errors.
pub fn signal(pid: i32, sig: i32) -> bool {
    unsafe { libc::kill(pid, sig) == 0 }
}

pub fn alive(pid: i32) -> bool {
    unsafe { libc::kill(pid, 0) == 0 }
}
