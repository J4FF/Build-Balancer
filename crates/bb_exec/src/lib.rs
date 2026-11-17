// bb_exec -- running a job on a machine, under the limits its owner set
//
//   sys     processors a process may use, priority, free memory, processor load, the process
//           list (the one file with `unsafe`: system calls JFLib's platform layer has no use for)
//   run     find a program, write a script, start it on its processors, read its output,
//           measure its memory, end it
//   tools   which build tools a machine has
//   svc     the programs as system services, run as the right user
//   Cores   which processors are lent and which of them a running job holds
//   CpuMeter  how busy the whole machine is
#![no_std]

extern crate alloc;

pub mod run;
pub mod svc;
pub mod sys;
pub mod tools;

pub use run::{resolve, script, spawn, Proc, Spec};

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

/// The processors a worker lends, and who holds them.  Jobs get distinct processors, so two
/// jobs with four each really run on eight.
pub struct Cores {
    /// the lent processors, bit n = processor n (0: this machine has more than 64, or the
    /// system does not say: nothing is pinned then, jobs only get the count as a hint)
    lent: u64,
    free: u64,
}

impl Cores {
    /// lend `count` of the processors this process may run on (the first ones)
    pub fn new(count: u32) -> Cores {
        let have = sys::affinity();
        let mut lent = 0u64;
        let mut n = 0;
        for bit in 0..64 {
            if n < count && have & (1 << bit) != 0 {
                lent |= 1 << bit;
                n += 1;
            }
        }
        // more than 64 processors: a mask of 64 would lock jobs out of most of them
        if plat::cpu_count() > 64 {
            lent = 0;
        }
        Cores { lent, free: lent }
    }

    /// how many processors are lent as a mask (0 when nothing is pinned)
    pub fn lent_count(&self) -> u32 {
        self.lent.count_ones()
    }

    /// whether jobs are really held to their processors
    pub fn pinning(&self) -> bool {
        self.lent != 0
    }

    /// how many of the lent processors no job holds
    pub fn free_count(&self) -> u32 {
        self.free.count_ones()
    }

    /// take `count` free processors -> their mask (0: not pinning, or not enough free ones)
    pub fn take(&mut self, count: u32) -> u64 {
        if self.lent == 0 || self.free.count_ones() < count {
            return 0;
        }
        let mut mask = 0u64;
        let mut n = 0;
        for bit in 0..64 {
            if n < count && self.free & (1 << bit) != 0 {
                mask |= 1 << bit;
                n += 1;
            }
        }
        self.free &= !mask;
        mask
    }

    pub fn give_back(&mut self, mask: u64) {
        self.free |= mask & self.lent;
    }

    /// every lent processor (0: not pinning): where a light job runs, beside the others
    pub fn lent_mask(&self) -> u64 {
        self.lent
    }
}

/// A script that starts a program on its own, in `cwd`, with `env` (the worker's environment
/// plus these), its output appended to `log`; on Linux and macOS it notes its process id in
/// `pid` first.  -> (the interpreter, its arguments before the script, the script, the
/// script's extension).  For plat::process::launch, which starts a program outside the
/// worker's process tree but has no folder, environment or output of its own.
pub fn launcher(program: &str, args: &[String], cwd: &str, env: &[(String, Option<String>)], log: &str, pid: &str) -> Result<(String, Vec<String>, String, &'static str), String> {
    let set: Vec<(&str, &str)> = env.iter().filter_map(|(k, v)| v.as_deref().map(|v| (k.as_str(), v))).collect();
    if cfg!(windows) {
        // cmd: in quotes everything is taken as it is, but a quote or a line break; a percent
        // sign is doubled in a batch file
        let bad = |t: &str| t.contains(['"', '\r', '\n', '\0']);
        if bad(program) || args.iter().any(|a| bad(a)) || bad(cwd) || set.iter().any(|(k, v)| bad(k) || bad(v)) {
            return Err(String::from("a quote or a line break cannot be handed to a program that is left running on Windows: put the command into a script (--sh)"));
        }
        let q = |t: &str| format!("\"{}\"", t.replace('%', "%%"));
        let mut text = format!("@echo off\r\ncd /d {} || exit /b 1\r\n", q(cwd));
        for (k, v) in &set {
            text += &format!("set {}\r\n", q(&format!("{}={}", k, v)));
        }
        text += &q(program);
        for a in args {
            text.push(' ');
            text += &q(a);
        }
        text += &format!(" >> {} 2>&1\r\n", q(log));
        return Ok((String::from("cmd.exe"), alloc::vec![String::from("/d"), String::from("/c")], text, "cmd"));
    }
    let q = |t: &str| format!("'{}'", t.replace('\'', "'\\''"));
    let mut text = format!("#!/bin/sh\necho $$ > {}\ncd {} || exit 1\nexec env", q(pid), q(cwd));
    for (k, v) in &set {
        text.push(' ');
        text += &q(&format!("{}={}", k, v));
    }
    text.push(' ');
    text += &q(program);
    for a in args {
        text.push(' ');
        text += &q(a);
    }
    text += &format!(" >> {} 2>&1 < /dev/null\n", q(log));
    Ok((String::from("/bin/sh"), Vec::new(), text, "sh"))
}

/// how busy all processors of the machine were since the last call, in percent
#[derive(Default)]
pub struct CpuMeter {
    last: (u64, u64),
}

impl CpuMeter {
    pub fn percent(&mut self) -> u32 {
        let now = sys::cpu_times();
        let (busy, total) = (now.0.saturating_sub(self.last.0), now.1.saturating_sub(self.last.1));
        let first = self.last.1 == 0;
        self.last = now;
        if first || total == 0 { 0 } else { (busy * 100 / total).min(100) as u32 }
    }
}
