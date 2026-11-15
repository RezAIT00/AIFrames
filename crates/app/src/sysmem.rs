//! Process memory and installed RAM (for the memory readout and the frame-cache default).

/// (private bytes, working set) of this process.
#[cfg(windows)]
pub fn process_memory() -> Option<(u64, u64)> {
    #[repr(C)]
    #[derive(Default)]
    struct Counters {
        cb: u32,
        page_fault_count: u32,
        peak_working_set_size: usize,
        working_set_size: usize,
        quota_peak_paged_pool_usage: usize,
        quota_paged_pool_usage: usize,
        quota_peak_non_paged_pool_usage: usize,
        quota_non_paged_pool_usage: usize,
        pagefile_usage: usize,
        peak_pagefile_usage: usize,
        private_usage: usize,
    }
    #[link(name = "kernel32")]
    extern "system" {
        fn GetCurrentProcess() -> isize;
        fn K32GetProcessMemoryInfo(process: isize, counters: *mut Counters, cb: u32) -> i32;
    }
    let mut c = Counters { cb: std::mem::size_of::<Counters>() as u32, ..Default::default() };
    // SAFETY: documented Win32 calls; the pseudo handle needs no closing and `c` is ours.
    let ok = unsafe { K32GetProcessMemoryInfo(GetCurrentProcess(), &mut c, c.cb) };
    (ok != 0).then_some((c.private_usage as u64, c.working_set_size as u64))
}

#[cfg(not(windows))]
pub fn process_memory() -> Option<(u64, u64)> {
    None
}

/// Installed physical memory in bytes.
#[cfg(windows)]
pub fn total_ram() -> Option<u64> {
    #[repr(C)]
    struct Status {
        length: u32,
        memory_load: u32,
        total_phys: u64,
        avail_phys: u64,
        total_page_file: u64,
        avail_page_file: u64,
        total_virtual: u64,
        avail_virtual: u64,
        avail_extended_virtual: u64,
    }
    #[link(name = "kernel32")]
    extern "system" {
        fn GlobalMemoryStatusEx(s: *mut Status) -> i32;
    }
    let mut s = Status { length: std::mem::size_of::<Status>() as u32, memory_load: 0, total_phys: 0, avail_phys: 0, total_page_file: 0, avail_page_file: 0, total_virtual: 0, avail_virtual: 0, avail_extended_virtual: 0 };
    // SAFETY: documented Win32 call filling a struct we own (length set as required).
    let ok = unsafe { GlobalMemoryStatusEx(&mut s) };
    (ok != 0).then_some(s.total_phys)
}

#[cfg(not(windows))]
pub fn total_ram() -> Option<u64> {
    None
}

/// The default decoded-frame cache for a machine: 1/64 of the RAM, between 256 MB and 512 MB.
/// Preview frames are small (a 960×540 frame is 2 MB), so this holds a few hundred of them; the
/// playback engine keeps its own small look-ahead on top.
pub fn default_cache_mb(total_ram: Option<u64>) -> u32 {
    let mb = total_ram.map(|b| b / (64 << 20)).unwrap_or(512);
    mb.clamp(256, 512) as u32
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cache_default_scales_with_ram() {
        assert_eq!(default_cache_mb(Some(8 << 30)), 256);
        assert_eq!(default_cache_mb(Some(24 << 30)), 384);
        assert_eq!(default_cache_mb(Some(128 << 30)), 512);
        assert_eq!(default_cache_mb(None), 512);
        #[cfg(windows)]
        {
            let (private, ws) = process_memory().expect("memory info");
            assert!(private > 0 && ws > 0);
            assert!(total_ram().unwrap_or(0) > 1 << 30);
        }
    }
}
