// bb_core -- what the relay, the workers and the main machine's `bb` share
//
//   j        JSON: the records' mapping (agent_core::jmap), text / parse with the limits used here
//   util     sizes ("8G"), durations ("30m"), ids, relative paths, the command line
//   crypt    device keys, pair keys, sealed messages, the pairing code, encrypted blobs
//   proto    the records that travel: pairing, device lists, a worker's status, jobs, events
//   client   the relay's HTTP interface from a program's side (TLS pinned to the relay's
//            certificate, bearer token, long polls, blob packs)
//
// The relay never sees a key: everything it stores and passes on is sealed between the
// machines that paired (docs/ARCHITECTURE.md).
#![no_std]
#![forbid(unsafe_code)]

extern crate alloc;

pub mod client;
pub mod crypt;
pub mod j;
pub mod proto;
pub mod util;

pub use j::{Field, Json, Value};

/// product name shown by every program
pub const PRODUCT: &str = "Build Balancer";
/// version of the programs
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
/// version of the relay interface and of the sealed records; programs with different numbers
/// refuse each other with a message instead of guessing
pub const PROTOCOL: u32 = 1;
/// the relay's default port
pub const DEFAULT_PORT: u16 = 7700;
/// the largest plaintext piece of a file (and of a manifest) that becomes one blob
pub const CHUNK: usize = 4 << 20;
/// the most a blob pack carries in one request
pub const PACK_MAX: usize = 16 << 20;
/// seconds a long poll waits at the relay
pub const POLL_WAIT: u32 = 25;
/// a device that was heard from within this many seconds counts as online
pub const ONLINE_SECS: i64 = 60;

/// "NAME VERSION" line printed by --version
pub fn version_line(program: &str) -> alloc::string::String {
    alloc::format!("{program} {VERSION} ({PRODUCT}, protocol {PROTOCOL})\n")
}

/// the operating system this program was built for, as workers report it: "windows", "linux", "macos"
pub fn os_name() -> &'static str {
    if cfg!(windows) {
        "windows"
    } else if cfg!(target_os = "macos") {
        "macos"
    } else {
        "linux"
    }
}

/// the processor architecture this program was built for: "x86_64", "aarch64"
pub fn arch_name() -> &'static str {
    if cfg!(target_arch = "aarch64") { "aarch64" } else { "x86_64" }
}
