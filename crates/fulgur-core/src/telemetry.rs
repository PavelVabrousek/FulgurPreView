//! Platform telemetry for process RAM and CPU time.

#[derive(Clone, Copy, Debug, serde::Serialize)]
pub struct SystemMemory {
    pub total_bytes: u64,
    pub available_bytes: u64,
}

pub fn system_memory() -> Option<SystemMemory> {
    let mut system = sysinfo::System::new();
    system.refresh_memory_specifics(sysinfo::MemoryRefreshKind::nothing().with_ram());
    let total_bytes = system.total_memory();
    let available_bytes = system.available_memory();
    (total_bytes > 0 && available_bytes <= total_bytes).then_some(SystemMemory {
        total_bytes,
        available_bytes,
    })
}

#[cfg(windows)]
pub fn peak_rss_bytes() -> Option<u64> {
    use windows_sys::Win32::System::ProcessStatus::{
        GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS,
    };
    use windows_sys::Win32::System::Threading::GetCurrentProcess;
    let mut counters: PROCESS_MEMORY_COUNTERS = unsafe { std::mem::zeroed() };
    let size = std::mem::size_of::<PROCESS_MEMORY_COUNTERS>() as u32;
    counters.cb = size;
    // SAFETY: counters is initialized, correctly sized, and writable for the call.
    let ok = unsafe { GetProcessMemoryInfo(GetCurrentProcess(), &mut counters, size) };
    (ok != 0).then_some(counters.PeakWorkingSetSize as u64)
}

#[cfg(not(windows))]
pub fn peak_rss_bytes() -> Option<u64> {
    None
}
