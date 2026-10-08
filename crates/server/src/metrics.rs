//! Counters and process sampling for the benchmark harness and `--stats`.

use std::{
    process::Command,
    sync::atomic::{AtomicU64, Ordering::Relaxed},
};

#[derive(Default)]
pub struct Metrics {
    pub refreshes: AtomicU64,
    pub refresh_micros: AtomicU64,
    pub frames_full: AtomicU64,
    pub frames_diff: AtomicU64,
    pub runs: AtomicU64,
    pub msgs: AtomicU64,
    /// Encoded bytes handed to the transport (set by the transport; local mode leaves it 0).
    pub bytes_out: AtomicU64,
    pub bytes_in: AtomicU64,
}

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct MetricsSnapshot {
    pub refreshes: u64,
    pub avg_refresh_ms: f64,
    pub frames_full: u64,
    pub frames_diff: u64,
    pub runs: u64,
    pub msgs: u64,
    pub bytes_out: u64,
    pub bytes_in: u64,
}

impl Metrics {
    pub fn snapshot(&self) -> MetricsSnapshot {
        let refreshes = self.refreshes.load(Relaxed);
        MetricsSnapshot {
            refreshes,
            avg_refresh_ms: if refreshes == 0 {
                0.0
            } else {
                self.refresh_micros.load(Relaxed) as f64 / refreshes as f64 / 1000.0
            },
            frames_full: self.frames_full.load(Relaxed),
            frames_diff: self.frames_diff.load(Relaxed),
            runs: self.runs.load(Relaxed),
            msgs: self.msgs.load(Relaxed),
            bytes_out: self.bytes_out.load(Relaxed),
            bytes_in: self.bytes_in.load(Relaxed),
        }
    }

    pub fn count_msg(&self, m: &glyph_proto::ServerMsg) {
        self.msgs.fetch_add(1, Relaxed);
        match m {
            glyph_proto::ServerMsg::FullFrame { runs, .. } => {
                self.frames_full.fetch_add(1, Relaxed);
                self.runs.fetch_add(runs.len() as u64, Relaxed);
            }
            glyph_proto::ServerMsg::Diff { runs, .. } => {
                self.frames_diff.fetch_add(1, Relaxed);
                self.runs.fetch_add(runs.len() as u64, Relaxed);
            }
            _ => {}
        }
    }
}

/// Memory and cumulative CPU of a process tree.
///
/// `rss_kb` is the plain sum of resident sizes, which counts pages shared between Chromium's
/// processes once per process and therefore overstates real use (often 3-5x). `mem_kb` is the
/// shared-aware figure (macOS `footprint`, Linux PSS) and is what benchmarks should quote; it is
/// `None` where the platform offers neither.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct ProcSample {
    pub rss_kb: u64,
    pub mem_kb: Option<u64>,
    pub cpu_secs: f64,
    pub procs: u32,
}

/// `[DD-][[HH:]MM:]SS[.frac]` as printed by `ps -o time` on Linux and macOS.
pub fn parse_cpu_time(s: &str) -> Option<f64> {
    let (days, rest) = match s.split_once('-') {
        Some((d, r)) => (d.parse::<f64>().ok()?, r),
        None => (0.0, s),
    };
    let mut secs = 0.0;
    for part in rest.split(':') {
        secs = secs * 60.0 + part.parse::<f64>().ok()?;
    }
    Some(days * 86400.0 + secs)
}

/// Sum RSS and CPU time over `root` and all of its descendants (Chromium's renderer, GPU and
/// utility processes). Uses `ps`, which exists on both macOS and Linux.
pub fn sample_tree(root: u32) -> Option<ProcSample> {
    sample(root, true)
}

/// Just `pid` itself, without descendants (our own process: Chromium is its child and must not be
/// counted twice).
pub fn sample_process(pid: u32) -> Option<ProcSample> {
    sample(pid, false)
}

fn sample(root: u32, descend: bool) -> Option<ProcSample> {
    let out = Command::new("ps")
        .args(["-axo", "pid=,ppid=,rss=,time="])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&out.stdout);
    let rows: Vec<(u32, u32, u64, f64)> = text
        .lines()
        .filter_map(|l| {
            let mut f = l.split_whitespace();
            Some((
                f.next()?.parse().ok()?,
                f.next()?.parse().ok()?,
                f.next()?.parse().ok()?,
                parse_cpu_time(f.next()?)?,
            ))
        })
        .collect();
    let mut in_tree = vec![root];
    let mut s = ProcSample::default();
    let mut i = 0;
    while i < in_tree.len() {
        let pid = in_tree[i];
        for &(p, pp, rss, cpu) in &rows {
            if p == pid {
                s.rss_kb += rss;
                s.cpu_secs += cpu;
                s.procs += 1;
            } else if descend && pp == pid && !in_tree.contains(&p) {
                in_tree.push(p);
            }
        }
        i += 1;
    }
    s.mem_kb = shared_aware_kb(&in_tree);
    (s.procs > 0).then_some(s)
}

/// Sum of per-process proportional/physical memory, if the platform can tell us.
fn shared_aware_kb(pids: &[u32]) -> Option<u64> {
    if cfg!(target_os = "linux") {
        let mut total = 0u64;
        for p in pids {
            // processes may exit between `ps` and now; they simply contribute nothing
            if let Ok(t) = std::fs::read_to_string(format!("/proc/{p}/smaps_rollup")) {
                total += t
                    .lines()
                    .find_map(|l| l.strip_prefix("Pss:"))
                    .and_then(|v| v.split_whitespace().next()?.parse::<u64>().ok())
                    .unwrap_or(0);
            }
        }
        return (total > 0).then_some(total);
    }
    if cfg!(target_os = "macos") {
        let mut cmd = Command::new("footprint");
        for p in pids {
            cmd.arg("-p").arg(p.to_string());
        }
        let out = cmd.output().ok()?;
        return parse_footprint(&String::from_utf8_lossy(&out.stdout));
    }
    None
}

/// Total of `footprint -p …` output in KB. With several processes the tool prints a
/// `Summary Footprint: N MB` line, which already accounts for shared pages; use it when present
/// (adding it to the per-process lines would double the answer), else sum the per-process headers.
pub fn parse_footprint(text: &str) -> Option<u64> {
    fn kb(rest: &str) -> Option<f64> {
        let mut it = rest.split_whitespace();
        let n = it.next()?.parse::<f64>().ok()?;
        Some(
            n * match it.next()? {
                "KB" => 1.0,
                "MB" => 1024.0,
                "GB" => 1024.0 * 1024.0,
                "B" | "bytes" => 1.0 / 1024.0,
                _ => return None,
            },
        )
    }
    if let Some(v) = text
        .lines()
        .find_map(|l| l.trim_start().strip_prefix("Summary Footprint: "))
        .and_then(kb)
    {
        return Some(v as u64);
    }
    let sum: Vec<f64> = text
        .lines()
        .filter_map(|l| kb(l.split_once("Footprint: ")?.1))
        .collect();
    (!sum.is_empty()).then(|| sum.iter().sum::<f64>() as u64)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cpu_time_formats() {
        assert_eq!(parse_cpu_time("0:01.50"), Some(1.5));
        assert_eq!(parse_cpu_time("01:02:03"), Some(3723.0));
        assert_eq!(parse_cpu_time("1-00:00:10"), Some(86410.0));
        assert_eq!(parse_cpu_time("12:34.56"), Some(754.56));
        assert_eq!(parse_cpu_time("nope"), None);
    }

    #[test]
    fn footprint_output_parses() {
        let t = "====\nGoogle Chrome [1]: 64-bit    Footprint: 75 MB (16384 bytes per page)\n====\nHelper [2]: 64-bit    Footprint: 512 KB (16384 bytes per page)\nGPU [3]: 64-bit    Footprint: 1.5 GB (16384 bytes per page)\n";
        assert_eq!(parse_footprint(t), Some(75 * 1024 + 512 + 1536 * 1024));
        assert_eq!(parse_footprint("nothing here"), None);
        // the multi-process summary replaces (not adds to) the per-process lines
        let multi = "A [1]: 64-bit    Footprint: 70 MB (x)\nB [2]: 64-bit    Footprint: 30 MB (x)\nSummary Footprint: 90 MB\n";
        assert_eq!(parse_footprint(multi), Some(90 * 1024));
    }

    #[test]
    fn a_single_process_sample_excludes_children() {
        let mut child = std::process::Command::new("sleep")
            .arg("5")
            .spawn()
            .unwrap();
        let me = std::process::id();
        let (tree, one) = (sample_tree(me).unwrap(), sample_process(me).unwrap());
        child.kill().ok();
        child.wait().ok();
        assert_eq!(one.procs, 1);
        assert!(tree.procs > one.procs, "{tree:?} vs {one:?}");
    }

    #[test]
    fn samples_our_own_tree() {
        let s = sample_tree(std::process::id()).expect("ps works");
        assert!(s.rss_kb > 1000 && s.procs >= 1, "{s:?}");
    }
}
