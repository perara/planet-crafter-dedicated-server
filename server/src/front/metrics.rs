//! What the front measures about itself and the banks: the process's CPU and memory (asked of the
//! kernel when the page asks, no sampling in the loop), the traffic to and from clients, and the
//! cgroup's memory against which a flip is allowed or refused.

use std::sync::atomic::{AtomicU64, Ordering};

use serde_json::{json, Value};

/// Batch bytes received from and sent to clients (the transports' own framing not counted).
pub(super) static BYTES_IN: AtomicU64 = AtomicU64::new(0);
pub(super) static BYTES_OUT: AtomicU64 = AtomicU64::new(0);

/// What the front counts over its life for GET /api/metrics (a counter is what Prometheus rates).
#[derive(Default)]
pub(super) struct Totals {
    /// Game processes that ended outside a hand-over (CrashWindow keeps only the last ten minutes).
    pub crashes: u64,
    /// Games the watchdog killed for saying nothing.
    pub watchdog_kills: u64,
    /// Hand-overs that failed and left the old bank running.
    pub flips_failed: u64,
    /// How long the world stood in the last hand-over, in milliseconds.
    pub last_stand_ms: Option<f64>,
    /// Players dropped for too much waiting for a bank, and the bytes that waited.
    pub dropped_clients: u64,
    pub dropped_bytes: u64,
}

pub(super) fn traffic() -> Value {
    json!({ "bytesIn": BYTES_IN.load(Ordering::Relaxed), "bytesOut": BYTES_OUT.load(Ordering::Relaxed) })
}

/// CPU time used so far and memory held now, asked of the kernel when the page asks (no
/// sampling in the loop).
pub(super) fn process() -> Value {
    // SAFETY: getrusage only fills the struct it is given.
    let mut usage: libc::rusage = unsafe { std::mem::zeroed() };
    let seconds = |t: libc::timeval| t.tv_sec as f64 + t.tv_usec as f64 / 1e6;
    let cpu = (unsafe { libc::getrusage(libc::RUSAGE_SELF, &mut usage) } == 0).then(|| seconds(usage.ru_utime) + seconds(usage.ru_stime));
    let rss = std::fs::read_to_string("/proc/self/status").ok().and_then(|s| s.lines().find_map(|l| l.strip_prefix("VmRSS:")?.trim().strip_suffix("kB")?.trim().parse::<u64>().ok()));
    json!({ "cpuSeconds": cpu, "memoryBytes": rss.map(|kb| kb * 1024) })
}

/// The CPU time and memory of another process of this user (a bank), from /proc.
pub fn process_of(pid: u32) -> (f64, u64) {
    // SAFETY: sysconf only reads.
    let ticks = unsafe { libc::sysconf(libc::_SC_CLK_TCK) }.max(1) as f64;
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).unwrap_or_default();
    // After the command's closing parenthesis: state is field 3, utime and stime 14 and 15.
    let fields: Vec<&str> = stat.rsplit_once(')').map(|(_, rest)| rest.split_whitespace().collect()).unwrap_or_default();
    let cpu = [11, 12].iter().filter_map(|i| fields.get(*i)?.parse::<f64>().ok()).sum::<f64>() / ticks;
    let rss = std::fs::read_to_string(format!("/proc/{pid}/status")).ok().and_then(|s| s.lines().find_map(|l| l.strip_prefix("VmRSS:")?.trim().strip_suffix("kB")?.trim().parse::<u64>().ok()));
    (cpu, rss.unwrap_or(0) * 1024)
}

/// The memory limit of the cgroup this process runs in (cgroup v2: a container's `--memory`, a
/// Kubernetes memory limit) and what counts against it now (anonymous memory and shared
/// memory, the snapshot's memfd among it; page cache is reclaimable and left out). None
/// without a limit or without cgroup v2 (a host: nothing is checked).
pub fn cgroup_memory() -> Option<(u64, u64)> {
    let limit = std::fs::read_to_string("/sys/fs/cgroup/memory.max").ok()?.trim().parse().ok()?;
    let stat = std::fs::read_to_string("/sys/fs/cgroup/memory.stat").ok()?;
    let field = |name: &str| stat.lines().find_map(|l| l.strip_prefix(name)?.trim().parse::<u64>().ok());
    Some((limit, field("anon ")? + field("shmem ")?))
}

/// Whether a flip fits in the memory the cgroup leaves. The new bank and the world's snapshot
/// are in memory while the old bank still holds the world: measured on a world of 100000
/// objects (bank 321 MB, snapshot 166 MB, the new bank peaking at 488 MB as it restores),
/// about twice the live bank's memory beyond what is in use. A flip that does not fit gets a
/// bank killed by the kernel (a crash restart, players dropped), so it is refused first.
pub fn flip_memory(live_bank: u64, cgroup: Option<(u64, u64)>) -> Result<(), String> {
    let Some((limit, used)) = cgroup else { return Ok(()) };
    let need = live_bank * 2;
    let free = limit.saturating_sub(used);
    if free >= need {
        return Ok(());
    }
    let mb = |b: u64| b.div_ceil(1 << 20);
    Err(format!(
        "a hot update needs about {} MB of memory beyond what runs (the new bank and the world's snapshot are in memory while the old bank still holds the world); this container's limit is {} MB with {} MB in use, {} MB free. Raise the memory limit to at least {} MB (docker run --memory, Compose mem_limit, a Kubernetes memory limit), or restart instead (pcds upgrade --restart VERSION), which needs no extra memory and disconnects every player",
        mb(need),
        mb(limit),
        mb(used),
        mb(free),
        mb(used + need)
    ))
}

/// The CPU time of the children this process has reaped (banks that ended).
pub(super) fn reaped_children_cpu() -> f64 {
    // SAFETY: getrusage only fills the struct it is given.
    let mut usage: libc::rusage = unsafe { std::mem::zeroed() };
    let seconds = |t: libc::timeval| t.tv_sec as f64 + t.tv_usec as f64 / 1e6;
    if unsafe { libc::getrusage(libc::RUSAGE_CHILDREN, &mut usage) } != 0 {
        return 0.0;
    }
    seconds(usage.ru_utime) + seconds(usage.ru_stime)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_process_is_measured_from_the_kernel() {
        assert!(process()["cpuSeconds"].as_f64().is_some() && process()["memoryBytes"].as_u64().unwrap() > 0);
        let (cpu, memory) = process_of(std::process::id());
        assert!(cpu >= 0.0 && memory > 0);
    }

    #[test]
    fn a_flip_must_fit_in_the_memory_the_container_leaves() {
        const MB: u64 = 1 << 20;
        // No limit (a host): nothing is checked.
        assert!(flip_memory(10_000 * MB, None).is_ok());
        // The measured world: bank 321 MB; 1 GB with 355 MB in use fits, with the last
        // snapshot (166 MB) also held it does not, nor does 512 MB.
        assert!(flip_memory(321 * MB, Some((1024 * MB, 355 * MB))).is_ok());
        let e = flip_memory(321 * MB, Some((1024 * MB, 521 * MB))).unwrap_err();
        assert!(e.contains("about 642 MB") && e.contains("limit is 1024 MB") && e.contains("at least 1163 MB") && e.contains("pcds upgrade --restart"), "{e}");
        assert!(flip_memory(321 * MB, Some((512 * MB, 355 * MB))).is_err());
        // Already over the limit counts as no room.
        assert!(flip_memory(MB, Some((MB, 2 * MB))).is_err());
        // This test's own cgroup, whatever it is: reads or says nothing, never fails.
        let _ = cgroup_memory();
    }
}
