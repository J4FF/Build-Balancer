// bb_exec -- running a job on a machine, under the limits its owner set
//
//   sys     processors a process may use, priority, free memory, processor load, the process
//           list (the one file with `unsafe`: system calls JFLib's platform layer has no use for)
//   run     find a program, write a script, start it on its processors, read its output,
//           measure its memory, end it
//   tools   which build tools a machine has
//   Cores   which processors are lent and which of them a running job holds
//   CpuMeter  how busy the whole machine is
#![no_std]

extern crate alloc;

pub mod run;
pub mod sys;
pub mod tools;

pub use run::{resolve, script, spawn, Proc, Spec};

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
