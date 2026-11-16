// sys -- what JFLib's platform layer does not offer and a worker needs: which processors a
// process may run on, a lower priority, free memory, how busy the processors are, and the
// processes below a process with their memory.
//
//   affinity() / set_affinity(mask)   the processors (bit n = processor n) of this process
//                                     (Windows) or of the calling thread (Linux); a child takes
//                                     over what its parent has when it is started
//   lower_priority() / restore_priority()   below normal / nice 10 around the start of a child,
//                                     which takes it over
//   mem_available()                   memory that programs can still get without swapping
//   cpu_times()                       (busy, total) processor time of the whole machine so far
//   own_pid(), processes(), rss(pid)  every process with its parent; a process's resident memory
//
// The only file of Build Balancer with `unsafe`: Windows calls through raw-dylib imports (no
// import libraries), Linux through the system calls of rt::os and the files of /proc, macOS
// through libSystem.
#![allow(unsafe_code)]

use alloc::vec::Vec;

/// a process as the system lists it
#[derive(Clone, Copy, Debug)]
pub struct ProcInfo {
    pub pid: u32,
    pub parent: u32,
    /// resident memory in bytes (Linux; 0 on Windows, where rss() reads it)
    pub rss: u64,
}

#[cfg(windows)]
mod imp {
    use super::ProcInfo;
    use alloc::vec::Vec;

    #[repr(C)]
    struct MemStatus {
        length: u32,
        load: u32,
        total_phys: u64,
        avail_phys: u64,
        total_page: u64,
        avail_page: u64,
        total_virtual: u64,
        avail_virtual: u64,
        avail_ext: u64,
    }

    #[repr(C)]
    struct ProcessEntry {
        size: u32,
        usage: u32,
        pid: u32,
        heap: usize,
        module: u32,
        threads: u32,
        parent: u32,
        pri: i32,
        flags: u32,
        exe: [u16; 260],
    }

    #[repr(C)]
    #[derive(Default)]
    struct MemCounters {
        cb: u32,
        page_faults: u32,
        peak_working_set: usize,
        working_set: usize,
        quota_peak_paged: usize,
        quota_paged: usize,
        quota_peak_nonpaged: usize,
        quota_nonpaged: usize,
        pagefile: usize,
        peak_pagefile: usize,
    }

    #[link(name = "kernel32", kind = "raw-dylib")]
    unsafe extern "system" {
        fn GetCurrentProcess() -> isize;
        fn GetCurrentProcessId() -> u32;
        fn SetProcessAffinityMask(process: isize, mask: usize) -> i32;
        fn GetProcessAffinityMask(process: isize, mask: *mut usize, system: *mut usize) -> i32;
        fn SetPriorityClass(process: isize, class: u32) -> i32;
        fn GetPriorityClass(process: isize) -> u32;
        fn GlobalMemoryStatusEx(status: *mut MemStatus) -> i32;
        fn GetSystemTimes(idle: *mut u64, kernel: *mut u64, user: *mut u64) -> i32;
        fn CreateToolhelp32Snapshot(flags: u32, pid: u32) -> isize;
        fn Process32FirstW(snapshot: isize, entry: *mut ProcessEntry) -> i32;
        fn Process32NextW(snapshot: isize, entry: *mut ProcessEntry) -> i32;
        fn OpenProcess(access: u32, inherit: i32, pid: u32) -> isize;
        fn CloseHandle(handle: isize) -> i32;
        fn K32GetProcessMemoryInfo(process: isize, counters: *mut MemCounters, size: u32) -> i32;
    }

    pub fn affinity() -> u64 {
        let (mut mask, mut system) = (0usize, 0usize);
        // SAFETY: both pointers lead to live usize values; the pseudo handle needs no closing.
        let ok = unsafe { GetProcessAffinityMask(GetCurrentProcess(), &mut mask, &mut system) };
        if ok != 0 { mask as u64 } else { 0 }
    }

    pub fn set_affinity(mask: u64) -> bool {
        // SAFETY: a plain call with values.
        mask != 0 && unsafe { SetProcessAffinityMask(GetCurrentProcess(), mask as usize) } != 0
    }

    pub fn lower_priority() -> u32 {
        const BELOW_NORMAL: u32 = 0x4000;
        const IDLE: u32 = 0x40;
        // SAFETY: plain calls with values.
        unsafe {
            let old = GetPriorityClass(GetCurrentProcess());
            if old == 0 || old == BELOW_NORMAL || old == IDLE || SetPriorityClass(GetCurrentProcess(), BELOW_NORMAL) == 0 {
                return 0;
            }
            old
        }
    }

    pub fn restore_priority(old: u32) {
        if old != 0 {
            // SAFETY: a plain call with values.
            unsafe { SetPriorityClass(GetCurrentProcess(), old) };
        }
    }

    pub fn mem_available() -> u64 {
        // SAFETY: MemStatus is the layout of MEMORYSTATUSEX, its length field says so.
        let mut s: MemStatus = unsafe { core::mem::zeroed() };
        s.length = core::mem::size_of::<MemStatus>() as u32;
        // SAFETY: the pointer leads to a live structure of the announced size.
        if unsafe { GlobalMemoryStatusEx(&mut s) } != 0 { s.avail_phys } else { 0 }
    }

    pub fn cpu_times() -> (u64, u64) {
        let (mut idle, mut kernel, mut user) = (0u64, 0u64, 0u64);
        // SAFETY: a FILETIME is two 32-bit halves, low first: the layout of a u64 here.
        if unsafe { GetSystemTimes(&mut idle, &mut kernel, &mut user) } == 0 {
            return (0, 0);
        }
        // kernel time includes idle time
        let total = kernel + user;
        (total.saturating_sub(idle), total)
    }

    pub fn own_pid() -> u32 {
        // SAFETY: no arguments.
        unsafe { GetCurrentProcessId() }
    }

    pub fn processes() -> Vec<ProcInfo> {
        const SNAP_PROCESS: u32 = 2;
        let mut out = Vec::new();
        // SAFETY: the snapshot handle is checked and closed; the entry announces its size.
        unsafe {
            let snap = CreateToolhelp32Snapshot(SNAP_PROCESS, 0);
            if snap == -1 || snap == 0 {
                return out;
            }
            let mut e: ProcessEntry = core::mem::zeroed();
            e.size = core::mem::size_of::<ProcessEntry>() as u32;
            let mut more = Process32FirstW(snap, &mut e);
            while more != 0 {
                out.push(ProcInfo { pid: e.pid, parent: e.parent, rss: 0 });
                more = Process32NextW(snap, &mut e);
            }
            CloseHandle(snap);
        }
        out
    }

    pub fn rss(pid: u32) -> u64 {
        const QUERY_LIMITED: u32 = 0x1000;
        let mut c = MemCounters::default();
        c.cb = core::mem::size_of::<MemCounters>() as u32;
        // SAFETY: the handle is checked and closed; the counters announce their size.
        unsafe {
            let h = OpenProcess(QUERY_LIMITED, 0, pid);
            if h == 0 {
                return 0;
            }
            let ok = K32GetProcessMemoryInfo(h, &mut c, c.cb);
            CloseHandle(h);
            if ok != 0 { c.working_set as u64 } else { 0 }
        }
    }
}

/// macOS: libSystem's calls (it is linked into every program there).  The system has no way to
/// hold a process to chosen processors, so nothing is pinned: jobs get the lent number as a
/// hint (BB_CPUS and the tools' own variables).  A lower priority comes from `nice` in front
/// of the job's command (run.rs): a process there cannot lower what it starts and keep its own.
#[cfg(target_os = "macos")]
mod imp {
    use super::ProcInfo;
    use alloc::vec::Vec;
    use core::ffi::{c_char, c_int, c_void};

    /// proc_pidinfo flavours and the sizes of their records (<sys/proc_info.h>)
    const PROC_PIDTBSDINFO: c_int = 3;
    const PROC_PIDTASKINFO: c_int = 4;
    const BSDINFO_SIZE: usize = 136;
    const TASKINFO_SIZE: usize = 96;
    /// host_statistics flavours (<mach/host_info.h>)
    const HOST_CPU_LOAD_INFO: c_int = 3;
    const HOST_VM_INFO64: c_int = 4;

    unsafe extern "C" {
        fn getpid() -> c_int;
        fn sysctlbyname(name: *const c_char, oldp: *mut c_void, oldlenp: *mut usize, newp: *const c_void, newlen: usize) -> c_int;
        fn proc_listallpids(buffer: *mut c_void, buffersize: c_int) -> c_int;
        fn proc_pidinfo(pid: c_int, flavor: c_int, arg: u64, buffer: *mut c_void, buffersize: c_int) -> c_int;
        fn mach_host_self() -> u32;
        fn host_statistics(host: u32, flavor: c_int, info: *mut u32, count: *mut u32) -> c_int;
        fn host_statistics64(host: u32, flavor: c_int, info: *mut u32, count: *mut u32) -> c_int;
    }

    pub fn affinity() -> u64 {
        0
    }

    pub fn set_affinity(_mask: u64) -> bool {
        false
    }

    pub fn lower_priority() -> u32 {
        0
    }

    pub fn restore_priority(_old: u32) {}

    fn page_size() -> u64 {
        let mut v = 0u64;
        let mut len = core::mem::size_of::<u64>();
        // SAFETY: the name ends with NUL; the value and its length are live and as long as said.
        let r = unsafe { sysctlbyname(c"hw.pagesize".as_ptr(), &mut v as *mut u64 as *mut c_void, &mut len, core::ptr::null(), 0) };
        if r == 0 && v != 0 { v } else { 16384 }
    }

    pub fn mem_available() -> u64 {
        // vm_statistics64 starts with free, active, inactive and wired page counts (32 bit each);
        // the buffer is larger than every version of the record
        let mut info = [0u32; 64];
        let mut count = info.len() as u32;
        // SAFETY: the buffer holds `count` 32-bit values; the port needs no release for one call.
        if unsafe { host_statistics64(mach_host_self(), HOST_VM_INFO64, info.as_mut_ptr(), &mut count) } != 0 {
            return 0;
        }
        (info[0] as u64 + info[2] as u64) * page_size()
    }

    pub fn cpu_times() -> (u64, u64) {
        // host_cpu_load_info: ticks of user, system, idle, nice
        let mut ticks = [0u32; 4];
        let mut count = 4u32;
        // SAFETY: the buffer holds the four values the flavour gives.
        if unsafe { host_statistics(mach_host_self(), HOST_CPU_LOAD_INFO, ticks.as_mut_ptr(), &mut count) } != 0 {
            return (0, 0);
        }
        let total: u64 = ticks.iter().map(|t| *t as u64).sum();
        (total - ticks[2] as u64, total)
    }

    pub fn own_pid() -> u32 {
        // SAFETY: no arguments.
        unsafe { getpid() as u32 }
    }

    pub fn processes() -> Vec<ProcInfo> {
        let mut out = Vec::new();
        // SAFETY: a null buffer asks for the number of processes.
        let n = unsafe { proc_listallpids(core::ptr::null_mut(), 0) };
        if n <= 0 {
            return out;
        }
        let mut pids = alloc::vec![0i32; n as usize + 64];
        // SAFETY: the buffer is as long as the call is told, in bytes.
        let got = unsafe { proc_listallpids(pids.as_mut_ptr() as *mut c_void, (pids.len() * 4) as c_int) };
        for &pid in pids.iter().take(got.max(0) as usize) {
            if pid <= 0 {
                continue;
            }
            // proc_bsdinfo: flags, status, xstatus, pid, ppid (32 bit each) come first
            let mut info = [0u32; BSDINFO_SIZE / 4];
            // SAFETY: the buffer has the record's size; a process of another user gives 0.
            let r = unsafe { proc_pidinfo(pid, PROC_PIDTBSDINFO, 0, info.as_mut_ptr() as *mut c_void, BSDINFO_SIZE as c_int) };
            if r as usize == BSDINFO_SIZE {
                out.push(ProcInfo { pid: pid as u32, parent: info[4], rss: 0 });
            }
        }
        out
    }

    pub fn rss(pid: u32) -> u64 {
        // proc_taskinfo: the virtual size, then the resident size (64 bit each)
        let mut info = [0u64; TASKINFO_SIZE / 8];
        // SAFETY: the buffer has the record's size.
        let r = unsafe { proc_pidinfo(pid as c_int, PROC_PIDTASKINFO, 0, info.as_mut_ptr() as *mut c_void, TASKINFO_SIZE as c_int) };
        if r as usize == TASKINFO_SIZE { info[1] } else { 0 }
    }
}

#[cfg(target_os = "linux")]
mod imp {
    use super::ProcInfo;
    use alloc::format;
    use alloc::string::String;
    use alloc::vec::Vec;

    #[cfg(target_arch = "x86_64")]
    const SYS_SCHED_SETAFFINITY: usize = 203;
    #[cfg(target_arch = "aarch64")]
    const SYS_SCHED_SETAFFINITY: usize = 122;
    #[cfg(target_arch = "x86_64")]
    const SYS_SETPRIORITY: usize = 141;
    #[cfg(target_arch = "aarch64")]
    const SYS_SETPRIORITY: usize = 140;

    /// a file of /proc: its size is unknown before it is read
    pub fn read_proc(path: &str) -> Option<Vec<u8>> {
        let f = plat::File::open(path, plat::FO_READ)?;
        let mut out = Vec::new();
        let mut buf = [0u8; 4096];
        loop {
            match f.read_at(out.len() as u64, &mut buf) {
                Some(0) | None => break,
                Some(n) => out.extend_from_slice(&buf[..n]),
            }
            if out.len() > 1 << 20 {
                break;
            }
        }
        Some(out)
    }

    pub fn affinity() -> u64 {
        // room for 1024 processors: the kernel refuses a mask shorter than its own
        let mut mask = [0u64; 16];
        // SAFETY: the buffer is as long as the call is told.
        let r = unsafe { rt::os::syscall3(rt::os::SYS_SCHED_GETAFFINITY, 0, core::mem::size_of_val(&mask), mask.as_mut_ptr() as usize) };
        if r > 0 { mask[0] } else { 0 }
    }

    pub fn set_affinity(mask: u64) -> bool {
        let mut m = [0u64; 16];
        m[0] = mask;
        // SAFETY: the buffer is as long as the call is told; pid 0 is the calling thread.
        mask != 0 && unsafe { rt::os::syscall3(SYS_SCHED_SETAFFINITY, 0, core::mem::size_of_val(&m), m.as_ptr() as usize) } == 0
    }

    pub fn lower_priority() -> u32 {
        // SAFETY: setpriority(PRIO_PROCESS, 0, 10) takes values only.  On Linux "0" is the calling
        // thread, and a child forked from it takes its nice value over.
        unsafe { rt::os::syscall3(SYS_SETPRIORITY, 0, 0, 10) };
        0
    }

    pub fn restore_priority(_old: u32) {
        // raising a priority again needs a privilege: the thread that started a job stays at
        // nice 10, and it ends with its job
    }

    fn field_kb(text: &str, name: &str) -> Option<u64> {
        let line = text.lines().find(|l| l.starts_with(name))?;
        line[name.len()..].trim().trim_end_matches("kB").trim().parse::<u64>().ok()
    }

    pub fn mem_available() -> u64 {
        read_proc("/proc/meminfo").and_then(|b| field_kb(&String::from_utf8_lossy(&b), "MemAvailable:")).map_or(0, |kb| kb << 10)
    }

    pub fn cpu_times() -> (u64, u64) {
        let Some(b) = read_proc("/proc/stat") else { return (0, 0) };
        let text = String::from_utf8_lossy(&b);
        let Some(line) = text.lines().next().filter(|l| l.starts_with("cpu ")) else { return (0, 0) };
        let v: Vec<u64> = line.split_ascii_whitespace().skip(1).filter_map(|x| x.parse().ok()).collect();
        if v.len() < 5 {
            return (0, 0);
        }
        // user nice system idle iowait irq softirq steal: guest time is part of user time
        let total: u64 = v.iter().take(8).sum();
        (total - v[3] - v[4], total)
    }

    pub fn own_pid() -> u32 {
        // SAFETY: getpid takes no arguments.
        unsafe { rt::os::syscall3(rt::os::SYS_GETPID, 0, 0, 0) as u32 }
    }

    pub fn processes() -> Vec<ProcInfo> {
        let mut out = Vec::new();
        for e in plat::fs::read_dir("/proc").unwrap_or_default() {
            let Ok(pid) = e.name.parse::<u32>() else { continue };
            let Some(b) = read_proc(&format!("/proc/{}/status", pid)) else { continue };
            let text = String::from_utf8_lossy(&b);
            let parent = field_kb(&text, "PPid:").unwrap_or(0) as u32;
            out.push(ProcInfo { pid, parent, rss: field_kb(&text, "VmRSS:").map_or(0, |kb| kb << 10) });
        }
        out
    }

    pub fn rss(pid: u32) -> u64 {
        read_proc(&format!("/proc/{}/status", pid)).and_then(|b| field_kb(&String::from_utf8_lossy(&b), "VmRSS:")).map_or(0, |kb| kb << 10)
    }
}

/// the processors this process may run on: bit n = processor n (0: unknown, or more than 64)
pub fn affinity() -> u64 {
    imp::affinity()
}

/// Limit the processors: of the whole process on Windows, of the calling thread on Linux.  A
/// child started afterwards takes the mask over, so set it, start the child, and set it back.
pub fn set_affinity(mask: u64) -> bool {
    imp::set_affinity(mask)
}

/// Lower the priority a child started now takes over (Windows: of this process, below normal;
/// Linux: of the calling thread, nice 10) -> what to hand to restore_priority() right after
/// the child is started.
pub fn lower_priority() -> u32 {
    imp::lower_priority()
}

/// undo lower_priority() (Windows; on Linux the calling thread keeps its nice value)
pub fn restore_priority(old: u32) {
    imp::restore_priority(old)
}

/// memory programs can still get without swapping (0: unknown)
pub fn mem_available() -> u64 {
    imp::mem_available()
}

/// (busy, total) processor time of the whole machine since it started, in the system's own unit
pub fn cpu_times() -> (u64, u64) {
    imp::cpu_times()
}

pub fn own_pid() -> u32 {
    imp::own_pid()
}

/// every process with its parent; on Linux also its resident memory (it comes from the same
/// file), on Windows that takes rss()
pub fn processes() -> Vec<ProcInfo> {
    imp::processes()
}

/// the resident memory of one process in bytes (0: gone, or not allowed to ask)
pub fn rss(pid: u32) -> u64 {
    imp::rss(pid)
}

/// the ids of the processes this process started and that still run
pub fn children() -> Vec<u32> {
    let me = own_pid();
    processes().iter().filter(|p| p.parent == me && p.pid != me).map(|p| p.pid).collect()
}
