//! Process memory numbers for the build report.

/// `(current, peak)` resident bytes of this process, or zeros if unavailable.
#[cfg(windows)]
pub fn process_bytes() -> (usize, usize) {
    #[repr(C)]
    struct Counters {
        cb: u32,
        page_faults: u32,
        peak_working_set: usize,
        working_set: usize,
        rest: [usize; 6],
    }
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn GetCurrentProcess() -> isize;
        fn K32GetProcessMemoryInfo(process: isize, counters: *mut Counters, cb: u32) -> i32;
    }
    let mut c = Counters { cb: std::mem::size_of::<Counters>() as u32, page_faults: 0, peak_working_set: 0, working_set: 0, rest: [0; 6] };
    // SAFETY: `c` is a correctly sized, writable PROCESS_MEMORY_COUNTERS and the
    // pseudo-handle from GetCurrentProcess is always valid.
    let ok = unsafe { K32GetProcessMemoryInfo(GetCurrentProcess(), &mut c, c.cb) };
    if ok == 0 { (0, 0) } else { (c.working_set, c.peak_working_set) }
}

#[cfg(not(windows))]
pub fn process_bytes() -> (usize, usize) {
    let rss = std::fs::read_to_string("/proc/self/statm")
        .ok()
        .and_then(|s| s.split_whitespace().nth(1).and_then(|n| n.parse::<usize>().ok()))
        .map_or(0, |pages| pages * 4096);
    let peak = std::fs::read_to_string("/proc/self/status")
        .ok()
        .and_then(|s| s.lines().find_map(|l| l.strip_prefix("VmHWM:")?.split_whitespace().next()?.parse::<usize>().ok()))
        .map_or(rss, |kb| kb * 1024);
    (rss, peak)
}

/// Human-readable size.
pub fn mib(bytes: usize) -> String {
    format!("{:.0} MiB", bytes as f64 / (1 << 20) as f64)
}
