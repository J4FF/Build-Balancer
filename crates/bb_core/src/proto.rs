// proto -- the records that travel between the machines
//
// At the relay (plain JSON, it has to route by them):
//   RelayInfo       GET /v1/info
//   Device          the relay's list of admitted devices (its token only as a hash)
//
// Sealed between two machines (the relay sees bytes):
//   Hello, Welcome  the two messages of a pairing, under the pairing code's key
//   WorkerStatus    what a worker offers and does right now, to the main machine
//   JobSpec         a job, main machine -> worker (the worker's inbox channel)
//   Event           what happens to a job, worker -> main machine (the job's channel)
//
// Channel names: "<worker id>/in" is a worker's inbox, "<main id>/job/<job id>" a job's
// events.  The first part is the device that reads the channel.

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

use agent_core::record;

use crate::j::{self, Json, Value};

record! {
    /// what a relay says about itself to anyone who asks
    pub struct RelayInfo {
        pub product: String,
        pub version: String,
        pub protocol: u32,
        /// made once per relay; the salt of pairing codes
        pub relay_id: String,
        /// a main machine has taken this relay (its setup key is used up)
        pub claimed: bool,
        pub time: i64,
    }
}

/// a device's role at the relay: it may admit and remove devices and read every channel
pub const ROLE_MAIN: &str = "main";
/// a device's role at the relay: it reads its own channels and writes into others'
pub const ROLE_WORKER: &str = "worker";

record! {
    /// a device the relay admits
    pub struct Device {
        pub id: String,
        pub name: String,
        pub role: String,
        /// hex of BLAKE3 of its access token ("" in listings)
        pub token_hash: String,
        pub added: i64,
        /// when the relay last heard from it (any request, also a waiting long poll)
        pub last_seen: i64,
        /// its sealed status (base64), opaque to the relay
        pub status: String,
        pub status_time: i64,
    }
}

record! {
    /// pairing, worker -> main machine, sealed with the pairing code's key
    pub struct Hello {
        pub protocol: u32,
        pub name: String,
        /// the worker's X25519 public key (hex)
        pub public: String,
        pub os: String,
        pub arch: String,
        pub host: String,
        pub version: String,
        /// the certificate the worker saw at the relay (hex of its SHA-256; "" over plain http)
        pub relay_fp: String,
    }
}

record! {
    /// pairing, main machine -> worker, sealed with the pairing code's key
    pub struct Welcome {
        pub protocol: u32,
        /// why the main machine refuses ("" when it accepts)
        pub error: String,
        pub relay_id: String,
        /// the certificate the main machine pins: the worker compares it with the one it saw
        pub relay_fp: String,
        pub main_id: String,
        pub main_name: String,
        pub main_public: String,
        pub worker_id: String,
        pub worker_token: String,
        /// the fleet's data key (hex)
        pub data_key: String,
    }
}

record! {
    /// what a worker lends: logical processors, memory and disk space in bytes, jobs at once
    pub struct Offer {
        pub cpu: u32,
        pub ram: u64,
        pub disk: u64,
        pub jobs: u32,
    }
}

record! {
    /// a build tool a worker found on its PATH
    pub struct Tool {
        pub name: String,
        /// the first line of its version output ("" when it did not answer)
        pub version: String,
        pub path: String,
    }
}

record! {
    /// a job in a worker's status
    pub struct JobBrief {
        pub id: String,
        pub project: String,
        pub slot: String,
        pub state: String,
        pub cpu: u32,
        pub ram: u64,
        pub started: i64,
        pub command: String,
    }
}

record! {
    /// a project folder a worker keeps between jobs
    pub struct Workspace {
        pub project_id: String,
        pub project: String,
        pub slot: String,
        pub bytes: u64,
        pub used: i64,
    }
}

record! {
    /// what a worker is, offers and does right now
    pub struct WorkerStatus {
        pub time: i64,
        pub id: String,
        pub name: String,
        pub version: String,
        pub protocol: u32,
        pub os: String,
        pub arch: String,
        pub host: String,
        pub cpu_model: String,
        /// the machine: logical processors, memory, and what of it is free right now
        pub cpu_total: u32,
        pub ram_total: u64,
        pub ram_free: u64,
        /// how busy all processors are, in percent (other programs included)
        pub cpu_busy: u32,
        /// the volume of the workspaces
        pub disk_total: u64,
        pub disk_free: u64,
        /// what the owner lends
        pub offer: Offer,
        /// what running jobs hold of it
        pub cpu_reserved: u32,
        pub ram_reserved: u64,
        pub disk_used: u64,
        /// how each limit is held: "affinity", "job-object", "watchdog", "quota", "none"
        pub enforce_cpu: String,
        pub enforce_ram: String,
        pub enforce_disk: String,
        pub running: Vec<JobBrief>,
        pub queued: Vec<JobBrief>,
        pub workspaces: Vec<Workspace>,
        pub tools: Vec<Tool>,
        pub tags: Vec<String>,
        pub started: i64,
    }
}

impl WorkerStatus {
    /// processors and memory not held by a running job
    pub fn cpu_idle(&self) -> u32 {
        self.offer.cpu.saturating_sub(self.cpu_reserved)
    }

    pub fn ram_idle(&self) -> u64 {
        self.offer.ram.saturating_sub(self.ram_reserved)
    }

    pub fn has_tool(&self, name: &str) -> bool {
        self.tools.iter().any(|t| t.name.eq_ignore_ascii_case(name))
    }
}

record! {
    pub struct EnvVar {
        pub name: String,
        pub value: String,
    }
}

record! {
    /// A tree of files as blobs: the manifest's text, cut into chunks (hex blob ids).  Empty
    /// chunks = no tree.
    pub struct TreeRef {
        pub chunks: Vec<String>,
        pub files: u64,
        pub bytes: u64,
    }
}

impl TreeRef {
    pub fn is_empty(&self) -> bool {
        self.chunks.is_empty()
    }
}

record! {
    /// a job as the main machine hands it to a worker
    pub struct JobSpec {
        pub protocol: u32,
        pub id: String,
        /// the main machine's device id (its events go to "<main>/job/<id>")
        pub main: String,
        pub project_id: String,
        pub project: String,
        /// which copy of the project on the worker: jobs of one slot run one after the other
        pub slot: String,
        /// the project's files (empty: run in the workspace as it is)
        pub tree: TreeRef,
        /// the program and its arguments, run without a shell ...
        pub argv: Vec<String>,
        /// ... or a script for a shell: "sh", "bash", "cmd", "powershell", "pwsh"
        pub shell: String,
        pub script: String,
        /// the folder inside the workspace to run in ("" = its root)
        pub cwd: String,
        pub env: Vec<EnvVar>,
        /// processors and memory to hold for it (0: all the worker lends / none in particular)
        pub cpu: u32,
        pub ram: u64,
        /// seconds until it is stopped (0: never)
        pub timeout: u64,
        /// files to send back: glob patterns inside the workspace
        pub outputs: Vec<String>,
        pub created: i64,
        /// a name for people (a task's name)
        pub label: String,
    }
}

impl JobSpec {
    /// the command on one line, for lists and logs
    pub fn command_text(&self) -> String {
        if !self.shell.is_empty() {
            return format!("[{}] {}", self.shell, self.script.lines().next().unwrap_or(""));
        }
        self.argv.iter().map(|a| if a.contains(' ') { format!("\"{}\"", a) } else { a.clone() }).collect::<Vec<_>>().join(" ")
    }
}

/// the channel a worker reads its orders from
pub fn inbox(worker: &str) -> String {
    format!("{}/in", worker)
}

/// the channel a job's events go to
pub fn job_channel(main: &str, job: &str) -> String {
    format!("{}/job/{}", main, job)
}

/// an order to a worker: {"t": kind, ...fields}
pub fn order(kind: &str, fields: Value) -> Value {
    let mut v = j::obj([("t", j::s(kind))]);
    if let Some(m) = fields.as_object() {
        for (k, x) in m {
            v.insert(k, x.clone());
        }
    }
    v
}

pub const ORDER_START: &str = "job.start";
pub const ORDER_CANCEL: &str = "job.cancel";
pub const ORDER_WS_REMOVE: &str = "ws.remove";
/// bring the worker's programs up to the newest release and start again (when nothing runs)
pub const ORDER_UPDATE: &str = "update";

/// a job's states, in the order it passes through them
pub const ST_SUBMITTED: &str = "submitted"; // sent to the relay, the worker has not answered yet
pub const ST_QUEUED: &str = "queued"; // the worker has it and waits for room
pub const ST_SYNCING: &str = "syncing"; // the worker fetches the project's files
pub const ST_RUNNING: &str = "running";
pub const ST_COLLECTING: &str = "collecting"; // the worker uploads the outputs
/// the ends
pub const ST_DONE: &str = "done"; // exit code 0
pub const ST_FAILED: &str = "failed"; // the command ended with another exit code
pub const ST_CANCELLED: &str = "cancelled"; // stopped on request
pub const ST_KILLED: &str = "killed"; // stopped by a limit: time, memory, disk
pub const ST_ERROR: &str = "error"; // it never ran or its result is lost: refused, sync failed, no such program

/// whether a state is an end
pub fn is_final(state: &str) -> bool {
    matches!(state, ST_DONE | ST_FAILED | ST_CANCELLED | ST_KILLED | ST_ERROR)
}

record! {
    /// how a job ended
    pub struct Done {
        pub state: String,
        /// the command's exit code (-1 when it never ran)
        pub exit: i64,
        /// what went wrong, for "killed" and "error"
        pub error: String,
        pub duration_ms: u64,
        /// the most memory the job's processes held together (0: not measured)
        pub peak_ram: u64,
        /// the outputs it sent back
        pub artifacts: TreeRef,
        /// what the sync before it had to fetch
        pub sync_files: u64,
        pub sync_bytes: u64,
        pub time: i64,
    }
}

/// what happens to a job
#[derive(Clone, Debug, PartialEq)]
pub enum Event {
    /// it entered a state; `detail` says more ("waiting for the workspace", "312 files")
    State { state: String, detail: String, time: i64 },
    /// output: `err` for standard error
    Log { err: bool, data: String },
    Done(Done),
}

impl Event {
    pub fn to_json(&self) -> Value {
        match self {
            Event::State { state, detail, time } => j::obj([("t", j::s("state")), ("state", j::s(state)), ("detail", j::s(detail)), ("time", Value::i64(*time))]),
            Event::Log { err, data } => j::obj([("t", j::s(if *err { "err" } else { "out" })), ("d", j::s(data))]),
            Event::Done(d) => {
                let mut v = d.to_json();
                v.insert("t", j::s("done"));
                v
            }
        }
    }

    pub fn from_json(v: &Value) -> Option<Event> {
        match j::str_of(v, "t") {
            "state" => Some(Event::State { state: String::from(j::str_of(v, "state")), detail: String::from(j::str_of(v, "detail")), time: v.get("time").and_then(Value::as_i64).unwrap_or(0) }),
            "out" | "err" => Some(Event::Log { err: j::str_of(v, "t") == "err", data: String::from(j::str_of(v, "d")) }),
            "done" => Done::from_json(v).map(Event::Done),
            _ => None,
        }
    }
}
