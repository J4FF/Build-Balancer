// bb_worker -- a machine that lends its power, as a library: `bb-worker` is its command line,
// the Build Balancer app runs the same engine inside its window
//
//   config    the data folder: pairing, what is lent, workspaces
//   join      pair with a main machine by a one-time code
//   engine    Engine: the inbox, the scheduler, the limits, the status
//   job       one job from sync to result
//   cleanup   remove a project's files, expire unused ones, forget everything
#![no_std]
#![deny(unsafe_code)]

extern crate alloc;

pub mod cleanup;
pub mod config;
pub mod engine;
pub mod job;
pub mod join;

pub use engine::{Engine, Running, Sched, Worker};
