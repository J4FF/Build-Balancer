// bb_main -- the main machine as a library: `bb` is its command line, the Build Balancer app
// its window
//
//   conf    what the main machine keeps (identity, paired workers, job records) and a
//           project's settings (buildbalancer.ini)
//   fleet   the workers as they are right now, and which of them fits a job
//   jobs    submit, follow, fetch, cancel
//   ops     setup, pairing, unpairing, projects, cleaning up on workers, what they lend
#![no_std]
#![deny(unsafe_code)]

extern crate alloc;

pub mod conf;
pub mod fleet;
pub mod jobs;
pub mod ops;
