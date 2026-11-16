// config -- a worker's data folder
//
//   worker.json            the pairing (relay, ids, keys) and what this machine lends
//   state.json             how far the inbox was read, the jobs already taken
//   ws/<project>/<slot>/   a project's files between jobs: tree/ (the files), known (what the
//                          sync wrote), meta.json (name, size, last use)
//   jobs/<id>/             a job's script, its output (out.log) and its record
//   logs/worker.log
//
// worker.json holds this machine's secret key and its access token: only its owner can read it.

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

use agent_core::record;
use bb_core::crypt::{DataKey, Keys};
use bb_core::proto::Workspace;
use bb_core::util::{below, Cli};

record! {
    pub struct Config {
        pub name: String,
        pub id: String,
        pub token: String,
        /// the relay: "https://host:port", its certificate's SHA-256 (hex) and its id
        pub relay: String,
        pub relay_fp: String,
        pub relay_id: String,
        /// this machine's X25519 secret (hex)
        pub secret: String,
        pub main_id: String,
        pub main_name: String,
        pub main_public: String,
        /// the fleet's data key (hex)
        pub data_key: String,
        /// what is lent: logical processors, bytes of memory, bytes of disk, jobs at once
        pub cpu: u32,
        pub ram: u64,
        pub disk: u64,
        pub jobs: u32,
        /// "low": jobs run below the normal priority, so the machine stays usable; "normal"
        pub priority: String,
        /// free words for whoever picks a worker ("gpu", "fast-disk")
        pub tags: Vec<String>,
        /// more programs to look for besides the usual build tools
        pub tools: Vec<String>,
        /// a project's files go by themselves when no job used them for this many days (0: never)
        pub keep_days: u32,
    }
}

record! {
    pub struct State {
        pub inbox_after: u64,
        /// the newest job ids taken, so an order that arrives twice runs once
        pub seen: Vec<String>,
    }
}

pub fn default_dir() -> String {
    // run by root (a service): a place of the system's, not root's home
    if cfg!(target_os = "macos") && plat::process::elevated() {
        return String::from("/Library/Application Support/BuildBalancer/worker");
    }
    if !cfg!(windows) && plat::process::elevated() {
        return String::from("/var/lib/buildbalancer/worker");
    }
    let base = plat::data_dir().unwrap_or_else(|| String::from("."));
    below(&base, "buildbalancer/worker")
}

/// the system's folder of a worker run by root
pub fn system_dir() -> &'static str {
    if cfg!(target_os = "macos") { "/Library/Application Support/BuildBalancer/worker" } else { "/var/lib/buildbalancer/worker" }
}

/// The folder that holds a pairing under the other identity: the system's when asked as the
/// person who called sudo (or as a plain user), that person's when asked as root.
pub fn other_place(dir: &str) -> Option<String> {
    if cfg!(windows) {
        return None;
    }
    let mut places = alloc::vec![String::from(system_dir())];
    if let Some(u) = bb_exec::svc::sudo_user() {
        places.push(bb_exec::svc::user_data_dir(&u, "worker"));
    }
    places.into_iter().find(|p| p != dir && load(p).is_some())
}

pub fn data_dir(cli: &Cli) -> String {
    bb_core::util::abs_path(&cli.value("--data").map(String::from).unwrap_or_else(default_dir))
}

pub fn load(dir: &str) -> Option<Config> {
    bb_core::j::load::<Config>(&plat::fs::join(dir, "worker.json")).filter(|c| !c.id.is_empty())
}

pub fn save(dir: &str, c: &Config) -> Result<(), String> {
    if !plat::fs::mkdir_private(dir) && plat::file_exists(dir) != 2 {
        return Err(format!("{} cannot be created", dir));
    }
    if bb_core::j::save(&plat::fs::join(dir, "worker.json"), c) { Ok(()) } else { Err(format!("{} cannot be written", plat::fs::join(dir, "worker.json"))) }
}

/// the keys a paired worker works with
pub struct Paired {
    /// the key shared with the main machine
    pub pair: [u8; 32],
    pub data: DataKey,
}

pub fn paired(c: &Config) -> Result<Paired, String> {
    let keys = Keys::from_hex(&c.secret).ok_or("worker.json: the secret key is damaged")?;
    let main = bb_core::crypt::key32(&c.main_public).ok_or("worker.json: the main machine's key is damaged")?;
    let pair = bb_core::crypt::pair_key(&keys, &main).ok_or("worker.json: the main machine's key is not usable")?;
    let data = DataKey::new(&bb_core::crypt::key32(&c.data_key).ok_or("worker.json: the data key is damaged")?);
    Ok(Paired { pair, data })
}

/// what a machine lends when its owner says nothing: three quarters of its processors, half
/// of its memory, half of the free disk space but 50 GiB at most
pub fn default_offer(dir: &str) -> (u32, u64, u64) {
    let cpus = plat::cpu_count();
    let free = plat::fs::free_space(dir).or_else(|| plat::fs::free_space(plat::fs::parent(dir))).map_or(20 << 30, |f| f.0);
    ((cpus * 3 / 4).max(1), plat::mem_total() / 2, (free / 2).min(50 << 30))
}

/// Apply --cpu / --ram / --disk / --jobs / --priority / --tags / --tools / --name of a command
/// line to a config -> whether anything changed.
pub fn apply_options(c: &mut Config, cli: &Cli, dir: &str) -> Result<bool, String> {
    let before = c.clone();
    let cpus = plat::cpu_count() as u64;
    if let Some(v) = cli.value("--cpu") {
        let n = bb_core::util::share_parse(v, cpus, |s| s.trim().parse::<u64>().ok()).ok_or("--cpu takes a number of processors or a share like 50%")?;
        c.cpu = n.clamp(1, cpus) as u32;
    }
    if let Some(v) = cli.value("--ram") {
        let total = plat::mem_total();
        let n = bb_core::util::share_parse(v, total, bb_core::util::size_parse).ok_or("--ram takes a size like 8G or a share like 50%")?;
        c.ram = n.clamp(256 << 20, total.max(256 << 20));
    }
    if let Some(v) = cli.value("--disk") {
        // the data folder may not exist yet: its parent lies on the same volume
        let free = plat::fs::free_space(dir).or_else(|| plat::fs::free_space(plat::fs::parent(dir))).map_or(0, |f| f.0);
        let n = bb_core::util::share_parse(v, free, bb_core::util::size_parse).ok_or("--disk takes a size like 40G or a share of the free space like 50%")?;
        c.disk = n.max(256 << 20);
    }
    if let Some(v) = cli.value("--jobs") {
        c.jobs = v.parse::<u32>().ok().filter(|n| (1..=64).contains(n)).ok_or("--jobs takes a number from 1 to 64")?;
    }
    if let Some(v) = cli.value("--keep-days") {
        c.keep_days = v.parse::<u32>().ok().filter(|n| *n <= 3650).ok_or("--keep-days takes a number of days (0: keep projects until they are removed by hand)")?;
    }
    if let Some(v) = cli.value("--priority") {
        if v != "low" && v != "normal" {
            return Err(String::from("--priority takes low or normal"));
        }
        c.priority = String::from(v);
    }
    let words = |v: &str| -> Vec<String> { v.split([',', ' ']).map(bb_core::util::slug).filter(|s| !s.is_empty()).collect() };
    if let Some(v) = cli.value("--tags") {
        c.tags = words(v);
    }
    if let Some(v) = cli.value("--tools") {
        c.tools = v.split([',', ' ']).map(|s| String::from(s.trim())).filter(|s| !s.is_empty() && !s.contains(['/', '\\'])).collect();
    }
    if let Some(v) = cli.value("--name") {
        if v.trim().is_empty() || v.len() > 60 {
            return Err(String::from("--name takes 1 to 60 characters"));
        }
        c.name = String::from(v.trim());
    }
    Ok(*c != before)
}

pub fn offer_text(c: &Config) -> String {
    format!(
        "  processors   {} of {}\n  memory       {} of {}\n  disk         {}\n  jobs at once {}\n  priority     {}\n  tidy up      {}\n  tags         {}\n",
        c.cpu,
        plat::cpu_count(),
        bb_core::util::size_text(c.ram),
        bb_core::util::size_text(plat::mem_total()),
        bb_core::util::size_text(c.disk),
        c.jobs,
        c.priority,
        if c.keep_days == 0 { String::from("never by itself") } else { format!("projects unused for {} days are removed", c.keep_days) },
        if c.tags.is_empty() { String::from("-") } else { c.tags.join(", ") }
    )
}

/// the folder of a project's copy
pub fn ws_dir(dir: &str, project_id: &str, slot: &str) -> String {
    below(dir, &format!("ws/{}/{}", project_id, slot))
}

pub fn job_dir(dir: &str, job: &str) -> String {
    below(dir, &format!("jobs/{}", job))
}

/// every workspace of the data folder, from its meta.json
pub fn workspaces(dir: &str) -> Vec<Workspace> {
    let mut out = Vec::new();
    let root = plat::fs::join(dir, "ws");
    for p in plat::fs::read_dir(&root).unwrap_or_default() {
        let pd = plat::fs::join(&root, &p.name);
        for s in plat::fs::read_dir(&pd).unwrap_or_default() {
            let sd = plat::fs::join(&pd, &s.name);
            let mut w = bb_core::j::load::<Workspace>(&plat::fs::join(&sd, "meta.json")).unwrap_or_default();
            w.project_id = p.name.clone();
            w.slot = s.name.clone();
            out.push(w);
        }
    }
    out
}

pub fn save_workspace(dir: &str, w: &Workspace) {
    let p = plat::fs::join(&ws_dir(dir, &w.project_id, &w.slot), "meta.json");
    bb_core::util::put_file(&p, bb_core::j::pretty(&bb_core::j::Json::to_json(w)).as_bytes());
}
