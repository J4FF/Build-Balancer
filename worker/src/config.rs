// config -- a worker's data folder
//
//   worker.json            the pairing (relay, ids, keys) and what this machine lends
//   state.json             how far the inbox was read, the jobs already taken
//   ws/<project>/<slot>/   a project's files between jobs: tree/ (the files), known (what the
//                          sync wrote), meta.json (name, size, last use)
//
// Projects can be kept on other drives as well (Config::stores): on each drive the owner
// names, the worker has a folder of its own, DRIVE/BuildBalancer/<worker id>/, with the same
// ws/ in it and a limit of its own.  A place is the data folder or such a folder (places()).
// A drive that is not there (unplugged) is no place for the time being; what lies on it is
// neither counted nor missed -- a project is a copy, and the next job brings it again.
//   jobs/<id>/             a job's script, its output (out.log) and its record
//   logs/worker.log
//
// worker.json holds this machine's secret key and its access token: only its owner can read it.

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

use agent_core::record;
use bb_core::crypt::{DataKey, Keys};
use bb_core::proto::{OfferChange, Workspace};
use bb_core::util::{below, Cli};

record! {
    /// a drive (or a folder) besides the data folder that projects are kept on
    pub struct Store {
        /// what the owner named: a drive ("E:/", "/media/me/Stick") or any folder
        pub path: String,
        /// bytes lent there; 0: given up -- nothing new goes there, what lies there is removed
        pub disk: u64,
        /// a drive was mounted at `path` when it was named: it counts only while one is (a
        /// mount point left behind is an empty folder on another drive)
        pub mount: bool,
    }
}

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
        /// (disk: in the data folder; 0 where projects are kept on other drives only)
        pub cpu: u32,
        pub ram: u64,
        pub disk: u64,
        pub jobs: u32,
        /// more room for projects, on other drives
        pub stores: Vec<Store>,
        /// "low": jobs run below the normal priority, so the machine stays usable; "normal"
        pub priority: String,
        /// free words for whoever picks a worker ("gpu", "fast-disk")
        pub tags: Vec<String>,
        /// more programs to look for besides the usual build tools
        pub tools: Vec<String>,
        /// a project's files go by themselves when no job used them for this many days (0: never)
        pub keep_days: u32,
        /// jobs may start programs that outlive them (bb run --leave-running)
        pub leave_running: bool,
        /// Only this machine sets what it lends: the main machine's changes (bb limits, the
        /// app's machines page) are refused.  Off by default -- the machines are one owner's.
        pub offer_locked: bool,
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
    bb_core::util::share_with_user(dir);
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

/// Apply --cpu / --ram / --disk / --store / --jobs / --priority / --keep-days / --leave-running /
/// --main-may-set / --tags / --tools / --name of a command line to a config -> whether
/// anything changed.
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
        // (0: nothing in the data folder -- for a machine that keeps projects on other drives only)
        c.disk = if n == 0 { 0 } else { n.max(256 << 20) };
    }
    for (_, v) in cli.opts.iter().filter(|(n, _)| n == "--store") {
        let usage = "--store takes DRIVE=SIZE, like E:=100G or /media/me/Stick=50% (a share of the drive's free space); DRIVE=off gives a drive up";
        let (path, amount) = v.rsplit_once('=').ok_or(usage)?;
        let path = clean_path(path);
        let known = c.stores.iter().position(|s| same_path(&s.path, &path));
        if matches!(amount.trim(), "off" | "none" | "0") {
            // given up, not forgotten: what lies there has to go first (cleanup::give_up)
            match known {
                Some(i) => c.stores[i].disk = 0,
                None => return Err(format!("{} is no drive this worker keeps projects on", path)),
            }
            continue;
        }
        if plat::file_exists(&path) != 2 {
            return Err(format!("{} is not there: plug the drive in (or make the folder) first", path));
        }
        let free = plat::fs::free_space(&path).map_or(0, |f| f.0);
        let n = bb_core::util::share_parse(amount.trim(), free, bb_core::util::size_parse).ok_or(usage)?.max(256 << 20);
        match known {
            Some(i) => c.stores[i].disk = n,
            None => c.stores.push(Store { mount: mount_point(&path), path, disk: n }),
        }
    }
    if c.disk == 0 && !c.stores.iter().any(|s| s.disk > 0) {
        return Err(String::from("nothing would be lent to keep projects on: give --disk a size, or name a drive with --store DRIVE=SIZE"));
    }
    if let Some(v) = cli.value("--jobs") {
        c.jobs = v.parse::<u32>().ok().filter(|n| (1..=64).contains(n)).ok_or("--jobs takes a number from 1 to 64")?;
    }
    if let Some(v) = cli.value("--keep-days") {
        c.keep_days = v.parse::<u32>().ok().filter(|n| *n <= 3650).ok_or("--keep-days takes a number of days (0: keep projects until they are removed by hand)")?;
    }
    if let Some(v) = cli.value("--leave-running") {
        c.leave_running = yes_no(v).ok_or("--leave-running takes yes or no")?;
    }
    if let Some(v) = cli.value("--main-may-set") {
        c.offer_locked = !yes_no(v).ok_or("--main-may-set takes yes or no")?;
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

fn yes_no(v: &str) -> Option<bool> {
    match v.trim() {
        "yes" | "on" | "true" | "1" => Some(true),
        "no" | "off" | "false" | "0" => Some(false),
        _ => None,
    }
}

/// A change of the main machine's as the options of `bb-worker config`, so it is checked as
/// they are: the power lent -- processors, memory, disk, drives, jobs at once, priority.  Not
/// what jobs may do here, the name, or whether the main machine may change it.
pub fn change_options(ch: &OfferChange) -> Cli {
    let mut cli = Cli::default();
    for (name, v) in [("--cpu", &ch.cpu), ("--ram", &ch.ram), ("--disk", &ch.disk), ("--jobs", &ch.jobs), ("--priority", &ch.priority)] {
        if !v.trim().is_empty() {
            cli.opts.push((String::from(name), String::from(v.trim())));
        }
    }
    for s in &ch.stores {
        cli.opts.push((String::from("--store"), s.clone()));
    }
    cli
}

/// what is lent, in one line for the log
pub fn offer_line(c: &Config) -> String {
    let drives: String = c.stores.iter().map(|s| format!(", {} on {}", if s.disk == 0 { String::from("nothing") } else { bb_core::util::size_text(s.disk) }, s.path)).collect();
    format!("{} processors, {} memory, {} disk{}, {} jobs, {} priority", c.cpu, bb_core::util::size_text(c.ram), bb_core::util::size_text(c.disk), drives, c.jobs, c.priority)
}

pub fn offer_text(c: &Config) -> String {
    // the drives besides the data folder, one line each
    let mut disk = if c.disk == 0 { String::from("nothing in the data folder") } else { bb_core::util::size_text(c.disk) };
    // (one that was given up and holds nothing of this worker any more is no news)
    for s in c.stores.iter().filter(|s| s.disk > 0 || plat::file_exists(&store_root(&s.path, &c.id)) != 0) {
        let state = if s.disk == 0 {
            "given up: what lies there is removed"
        } else if !store_there(s) {
            "not there right now"
        } else {
            ""
        };
        disk += &format!("\n  drive        {}   {}{}", s.path, if s.disk == 0 { String::new() } else { bb_core::util::size_text(s.disk) }, if state.is_empty() { String::new() } else { format!("   ({})", state) });
    }
    format!(
        "  processors   {} of {}\n  memory       {} of {}\n  disk         {}\n  jobs at once {}\n  priority     {}\n  tidy up      {}\n  tags         {}\n  left running {}\n  set by       {}\n",
        c.cpu,
        plat::cpu_count(),
        bb_core::util::size_text(c.ram),
        bb_core::util::size_text(plat::mem_total()),
        disk,
        c.jobs,
        c.priority,
        if c.keep_days == 0 { String::from("never by itself") } else { format!("projects unused for {} days are removed", c.keep_days) },
        if c.tags.is_empty() { String::from("-") } else { c.tags.join(", ") },
        if c.leave_running { "jobs may start programs that go on after them (bb run --leave-running)" } else { "no: what a job starts ends with it (--leave-running yes allows it)" },
        if c.offer_locked { "this machine only (--main-may-set yes lets the main machine, too)" } else { "this machine and the main machine (bb limits there; --main-may-set no stops it)" }
    )
}

/// A drive or a folder as it was named, in one spelling: forward slashes, no slash at the end
/// (but "E:/" and "/"), a bare drive letter as the drive's root.
pub fn clean_path(path: &str) -> String {
    let mut p = path.trim().replace('\\', "/");
    if p.ends_with(':') {
        // ("E:" alone would be "wherever the terminal is on E:")
        p.push('/');
    }
    let mut p = bb_core::util::abs_path(&p).replace('\\', "/");
    while p.len() > 1 && p.ends_with('/') && !p.ends_with(":/") {
        p.pop();
    }
    p
}

/// whether two cleaned paths name the same place (Windows does not tell A from a)
pub fn same_path(a: &str, b: &str) -> bool {
    if cfg!(windows) { a.eq_ignore_ascii_case(b) } else { a == b }
}

/// Whether a drive is mounted at this folder: a drive's root ("E:/", "/"), or a folder on
/// another device than the folder above it (Linux, macOS).
pub fn mount_point(path: &str) -> bool {
    let up = plat::fs::parent(path);
    if up.is_empty() || up == path {
        return true;
    }
    match (plat::fs::stat(path), plat::fs::stat(up)) {
        (Some(a), Some(b)) => a.dev != b.dev,
        _ => false,
    }
}

/// Whether a drive (or folder) projects are kept on is there right now: plugged in, and --
/// where a drive was mounted at it -- mounted, not only its empty mount point.
pub fn store_there(s: &Store) -> bool {
    plat::file_exists(&s.path) == 2 && (!s.mount || mount_point(&s.path))
}

/// a place projects are kept in: the data folder, or a worker's folder on another drive
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Place {
    /// what the owner named ("": the data folder)
    pub path: String,
    /// the folder that holds ws/
    pub root: String,
    /// bytes lent here (0: nothing new goes here)
    pub limit: u64,
}

/// a worker's own folder on a drive its owner named
pub fn store_root(path: &str, worker_id: &str) -> String {
    below(path, &format!("BuildBalancer/{}", worker_id))
}

/// The places of a worker whose drives are there right now, the data folder first.  One that
/// was given up (limit 0) is still among them as long as it is there: what lies in it is
/// still to be removed.
pub fn places(dir: &str, c: &Config) -> Vec<Place> {
    let mut out = alloc::vec![Place { path: String::new(), root: String::from(dir), limit: c.disk }];
    out.extend(c.stores.iter().filter(|s| store_there(s)).map(|s| Place { path: s.path.clone(), root: store_root(&s.path, &c.id), limit: s.disk }));
    out
}

/// the folders that hold a ws/ of the worker of this data folder
pub fn roots(dir: &str) -> Vec<String> {
    match load(dir) {
        Some(c) => places(dir, &c).into_iter().map(|p| p.root).collect(),
        None => alloc::vec![String::from(dir)],
    }
}

/// the folder of a project's copy in a place
pub fn ws_dir(root: &str, project_id: &str, slot: &str) -> String {
    below(root, &format!("ws/{}/{}", project_id, slot))
}

/// the place (its root) that holds a project's copy
pub fn root_of(dir: &str, project_id: &str, slot: &str) -> Option<String> {
    roots(dir).into_iter().find(|r| plat::file_exists(&ws_dir(r, project_id, slot)) == 2)
}

pub fn job_dir(dir: &str, job: &str) -> String {
    below(dir, &format!("jobs/{}", job))
}

/// every workspace of one place, from its meta.json
pub fn workspaces_in(root: &str) -> Vec<Workspace> {
    let mut out = Vec::new();
    let ws = plat::fs::join(root, "ws");
    for p in plat::fs::read_dir(&ws).unwrap_or_default() {
        let pd = plat::fs::join(&ws, &p.name);
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

/// every workspace of a worker, with the place (its root) it lies in
pub fn located(dir: &str) -> Vec<(String, Workspace)> {
    roots(dir).into_iter().flat_map(|r| workspaces_in(&r).into_iter().map(move |w| (r.clone(), w))).collect()
}

/// every workspace of a worker, in all its places
pub fn workspaces(dir: &str) -> Vec<Workspace> {
    located(dir).into_iter().map(|(_, w)| w).collect()
}

/// write a workspace's record into its folder in the place `root`
pub fn save_workspace(root: &str, w: &Workspace) {
    let p = plat::fs::join(&ws_dir(root, &w.project_id, &w.slot), "meta.json");
    bb_core::util::put_file(&p, bb_core::j::pretty(&bb_core::j::Json::to_json(w)).as_bytes());
}
