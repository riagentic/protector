//! Kernel-log watcher. A stuck kernel (a device that will not wake, a hung
//! task, a CPU lockup) freezes the machine with plenty of free RAM and no
//! process to blame — invisible in `/proc`, but the kernel says so in its log.

use std::io::{BufRead, BufReader, ErrorKind};
use std::sync::mpsc::Sender;

/// Log fragments that mean the kernel is in trouble. `true` = it is stuck right
/// now (a stall); `false` = a fault that tends to come just before one.
const PATTERNS: &[(&str, bool)] = &[
    ("device inaccessible", false),
    ("blocked for more than", true),
    ("soft lockup", true),
    ("hard LOCKUP", true),
    ("detected stall", true),
];

#[derive(Clone, Debug, PartialEq)]
pub struct Fault {
    pub stall: bool,
    pub text: String,
}

/// One `/dev/kmsg` line → its fault, if any. A record reads
/// `4,1234,5678,-;message`; its continuation lines start with a space.
pub fn parse(line: &str) -> Option<Fault> {
    if line.starts_with(' ') {
        return None;
    }
    let msg = line.split_once(';').map_or(line, |(_, m)| m).trim();
    PATTERNS
        .iter()
        .find(|(p, _)| msg.contains(p))
        .map(|(_, stall)| Fault {
            stall: *stall,
            text: msg.to_string(),
        })
}

/// One run of kernel trouble: from its first fault until the log goes quiet.
#[derive(Clone, Debug, PartialEq)]
pub struct Episode {
    pub start: f64,
    pub last: f64,
    pub last_stall: Option<f64>,
    pub text: String,
}

/// Fold a new fault into the running episode (or open one).
pub fn step(ep: Option<Episode>, f: &Fault, now: f64) -> Episode {
    Episode {
        start: ep.as_ref().map_or(now, |e| e.start),
        last: now,
        last_stall: if f.stall {
            Some(now)
        } else {
            ep.and_then(|e| e.last_stall)
        },
        text: f.text.clone(),
    }
}

/// The episode, while its last fault is at most `window` seconds old.
pub fn live(ep: Option<Episode>, now: f64, window: f64) -> Option<Episode> {
    ep.filter(|e| now - e.last <= window)
}

/// Forward every fault the log reports until it ends or the receiver is gone.
pub fn watch(log: std::fs::File, tx: Sender<Fault>) {
    // One read returns one whole record; 8192 is the kernel's largest.
    let mut r = BufReader::with_capacity(8192, log);
    let mut line = Vec::new();
    loop {
        line.clear();
        match r.read_until(b'\n', &mut line) {
            Ok(0) => return,
            Ok(_) => {
                if let Some(f) = parse(&String::from_utf8_lossy(&line)) {
                    if tx.send(f).is_err() {
                        return;
                    }
                }
            }
            // EPIPE: we fell behind and records were dropped — keep reading.
            Err(e) if e.kind() == ErrorKind::BrokenPipe => {}
            // Anything else: never let stall detection die quietly.
            Err(_) => std::thread::sleep(std::time::Duration::from_secs(5)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Lines from the 2026-10-02 freeze, as /dev/kmsg delivers them.
    #[test]
    fn parses_the_faults_the_kernel_actually_logged() {
        let pm = parse("3,900,1,-;pcieport 0000:16:00.0: Unable to change power state from D3cold to D0, device inaccessible\n").unwrap();
        assert!(!pm.stall);
        assert!(pm.text.starts_with("pcieport 0000:16:00.0"));
        for l in [
            "3,901,2,-;INFO: task kworker/u129:5:2064019 blocked for more than 122 seconds.\n",
            "0,902,3,-;watchdog: BUG: soft lockup - CPU#13 stuck for 21s! [deno:2075883]\n",
            "3,903,4,-;rcu: INFO: rcu_preempt self-detected stall on CPU\n",
        ] {
            assert!(parse(l).unwrap().stall, "{l}");
        }
    }

    #[test]
    fn ignores_ordinary_lines_and_continuations() {
        assert_eq!(parse("6,1,1,-;usb 1-2: new full-speed USB device\n"), None);
        assert_eq!(parse(" SUBSYSTEM=pci soft lockup\n"), None);
    }

    #[test]
    fn episode_keeps_its_start_and_its_last_stall_then_expires() {
        let pm = Fault {
            stall: false,
            text: "a".into(),
        };
        let hung = Fault {
            stall: true,
            text: "b".into(),
        };
        let e = step(None, &pm, 10.0);
        assert_eq!((e.start, e.last_stall), (10.0, None));
        let e = step(Some(e), &hung, 200.0);
        let e = step(Some(e), &pm, 210.0);
        assert_eq!((e.start, e.last, e.last_stall), (10.0, 210.0, Some(200.0)));
        assert_eq!(e.text, "a");
        assert!(live(Some(e.clone()), 500.0, 300.0).is_some());
        assert!(live(Some(e), 511.0, 300.0).is_none());
    }
}
