// bb-worker -- lends this machine's power to a main machine
//
//   bb-worker join RELAY CODE [--name NAME] [--cpu N|P%] [--ram SIZE|P%] [--disk SIZE|P%] [--jobs N] [--force]
//   bb-worker run
//   bb-worker config [--cpu ..] [--ram ..] [--disk ..] [--jobs N] [--priority low|normal]
//                    [--tags a,b] [--tools x,y] [--name NAME]      (no option: show)
//   bb-worker status
//   bb-worker leave                         forget the pairing (the workspaces stay)
//   bb-worker service install | uninstall | start | stop | status
//   bb-worker --version | --help                                (every command takes --data DIR)
//
// A worker opens no port.  It asks the relay for orders (a long poll), runs jobs of the one
// main machine it paired with, and reports what it offers and does.  Its owner decides how
// much it lends: processors (jobs are pinned to them), memory (a job that takes the jobs
// beyond it is ended), disk space for the projects' workspaces (docs/ARCHITECTURE.md).
//
// The main machine is trusted: its jobs run as the user this program runs as, with that
// user's rights.  Pair only with your own main machine.
#![no_std]
#![no_main]
#![deny(unsafe_code)]

extern crate alloc;

mod config;
mod job;
mod join;

use alloc::boxed::Box;
use alloc::collections::VecDeque;
use alloc::format;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::Ordering::Relaxed;
use core::sync::atomic::{AtomicBool, AtomicU64};

use bb_core::client::Relay;
use bb_core::crypt::{self, DataKey};
use bb_core::j::{self, Json, Value};
use bb_core::proto::{self, Event, JobBrief, JobSpec, Offer, Tool, WorkerStatus};
use bb_core::util::{now, out, size_text, tick_ms, Cli};
use bb_exec::{Cores, CpuMeter};
use plat::sync::{Condvar, Mutex};

use crate::config::{Config, State};

const SERVICE: &str = if cfg!(windows) { "BuildBalancerWorker" } else { "buildbalancer-worker" };
const VALUED: &[&str] = &["--data", "--name", "--cpu", "--ram", "--disk", "--jobs", "--priority", "--tags", "--tools"];

/// a job that holds processors right now
pub struct Running {
    pub spec: JobSpec,
    pub state: Mutex<String>,
    /// the processors it holds: their number and which ones (0: not pinned)
    pub cpu: u32,
    pub mask: u64,
    pub ram: u64,
    pub started: i64,
    pub cancel: AtomicBool,
    /// memory of its processes, as its thread last measured it
    pub rss: AtomicU64,
    /// set by the watchdog: why it has to end
    pub kill: Mutex<Option<String>>,
}

pub struct Sched {
    pub queue: VecDeque<JobSpec>,
    pub running: Vec<Arc<Running>>,
    pub cores: Cores,
    /// the status changed: tell the main machine soon
    pub dirty: bool,
}

pub struct Worker {
    pub dir: String,
    pub cfg: Mutex<Config>,
    /// the key shared with the main machine
    pub pair: [u8; 32],
    pub data: DataKey,
    pub relay: Relay,
    pub sched: Mutex<Sched>,
    pub wake: Condvar,
    pub tools: Mutex<Vec<Tool>>,
    pub state: Mutex<State>,
    pub started: i64,
}

fn fail(s: &str) -> i32 {
    plat::log_error("worker", s);
    1
}

fn usage() -> i32 {
    out("bb-worker join RELAY CODE [--name NAME] [--cpu N|P%] [--ram SIZE|P%] [--disk SIZE|P%] [--jobs N] [--force]\n\
         bb-worker run                      work until stopped\n\
         bb-worker config [--cpu ..] [--ram ..] [--disk ..] [--jobs N] [--priority low|normal]\n\
         \x20                [--tags a,b] [--tools x,y] [--name NAME]     show or change what is lent\n\
         bb-worker status\n\
         bb-worker tools                    the build tools jobs can use on this machine\n\
         bb-worker leave                    forget the pairing\n\
         bb-worker service install | uninstall | start | stop | status\n\
         bb-worker --version\n\
         (every command: --data DIR)\n\
         \n\
         RELAY and CODE come from `bb pair` on the main machine.\n");
    2
}

fn run() -> i32 {
    let rest: Vec<String> = plat::args().iter().skip(1).cloned().collect();
    if rest.iter().any(|a| a == "--version" || a == "-V") {
        out(&bb_core::version_line("bb-worker"));
        return 0;
    }
    if rest.iter().any(|a| a == "--help" || a == "-h") || rest.is_empty() {
        return usage();
    }
    let cli = match Cli::parse(&rest, VALUED, &[], false).and_then(|c| c.only(&["--data", "--name", "--cpu", "--ram", "--disk", "--jobs", "--priority", "--tags", "--tools", "--force", "--service"]).map(|_| c)) {
        Ok(c) => c,
        Err(e) => {
            bb_core::util::err(&format!("bb-worker: {}\n", e));
            return 2;
        }
    };
    plat::log_levels(plat::LOG_INFO, plat::LOG_INFO);
    let dir = config::data_dir(&cli);
    match cli.pos.first().map(String::as_str).unwrap_or("") {
        "join" => match join::join(&cli, &dir) {
            Ok(m) => {
                out(&m);
                0
            }
            Err(e) => fail(&e),
        },
        "run" if cli.flag("--service") => plat::service::run_service(SERVICE, || serve(&config::data_dir(&Cli::parse(&plat::args()[1..], VALUED, &[], false).unwrap_or_default()))),
        "run" => serve(&dir),
        "config" => config_cmd(&cli, &dir),
        "status" => status_cmd(&dir),
        "tools" => {
            // what a job on this machine can use, as this user (a service sees its own PATH)
            let t0 = tick_ms();
            let extra = config::load(&dir).map(|c| c.tools).unwrap_or_default();
            for t in bb_exec::tools::detect(&extra) {
                out(&format!("  {:<10} {}\n             {}\n", t.name, t.version, t.path));
            }
            out(&format!("(looked for {} programs in {})\n", bb_exec::tools::DEFAULT.len() + extra.len(), bb_core::util::dur_text(tick_ms() - t0)));
            0
        }
        "leave" => {
            if plat::delete(&plat::fs::join(&dir, "worker.json")) {
                out("The pairing is forgotten.  On the main machine, `bb unpair NAME` removes this worker from the relay.\n");
                0
            } else {
                fail("this machine is not paired")
            }
        }
        "service" => service_cmd(&cli, &dir),
        _ => usage(),
    }
}

fn not_paired(dir: &str) -> String {
    format!("this machine is not paired yet ({} has no worker.json): run `bb pair` on the main machine, then `bb-worker join RELAY CODE` here", dir)
}

fn config_cmd(cli: &Cli, dir: &str) -> i32 {
    let Some(mut c) = config::load(dir) else { return fail(&not_paired(dir)) };
    match config::apply_options(&mut c, cli, dir) {
        Ok(true) => {
            if let Err(e) = config::save(dir, &c) {
                return fail(&e);
            }
            out(&format!("\"{}\" now lends:\n{}(a running worker takes this over within a few seconds)\n", c.name, config::offer_text(&c)));
            0
        }
        Ok(false) => {
            out(&format!("\"{}\" lends:\n{}", c.name, config::offer_text(&c)));
            0
        }
        Err(e) => fail(&e),
    }
}

fn status_cmd(dir: &str) -> i32 {
    let Some(c) = config::load(dir) else { return fail(&not_paired(dir)) };
    out(&format!("worker \"{}\" ({})\n  main machine {}\n  relay        {}\n  data folder  {}\n  service      {}\n{}", c.name, c.id, c.main_name, c.relay, dir, plat::service::state(SERVICE).text(), config::offer_text(&c)));
    let ws = config::workspaces(dir);
    for w in &ws {
        out(&format!("  workspace    {} / {}: {}, used {} ago\n", w.project, w.slot, size_text(w.bytes), bb_core::util::ago_text(w.used)));
    }
    0
}

fn service_cmd(cli: &Cli, dir: &str) -> i32 {
    let r = match cli.pos.get(1).map(String::as_str).unwrap_or("status") {
        "install" => (|| {
            config::load(dir).ok_or_else(|| not_paired(dir))?;
            let exe = plat::current_exe().ok_or("the program's own path is unknown")?;
            let args = alloc::vec![String::from("run"), String::from("--service"), String::from("--data"), String::from(dir)];
            let spec = plat::service::ServiceSpec { name: SERVICE, display: "Build Balancer Worker", description: "Build Balancer: runs build jobs of the paired main machine within the limits set with bb-worker config", exe: &exe, args: &args };
            plat::service::install(&spec).and_then(|_| plat::service::start(SERVICE))?;
            Ok(format!(
                "service {} installed and started (data: {})\nJobs now run as the service's user ({}): tools that live only in your own\nuser profile (rustup, nvm ...) are not on its PATH -- `bb workers` on the main machine shows what it finds.",
                SERVICE,
                dir,
                if cfg!(windows) { "LocalSystem" } else { "root" }
            ))
        })(),
        "uninstall" => plat::service::uninstall(SERVICE).map(|_| format!("service {} removed (pairing and workspaces stay)", SERVICE)),
        "start" => plat::service::start(SERVICE).map(|_| format!("service {} started", SERVICE)),
        "stop" => plat::service::stop(SERVICE).map(|_| format!("service {} stopped", SERVICE)),
        "status" => Ok(format!("service {}: {}", SERVICE, plat::service::state(SERVICE).text())),
        _ => return usage(),
    };
    match r {
        Ok(m) => {
            out(&format!("{}\n", m));
            0
        }
        Err(e) => fail(&e),
    }
}

/// the processors a job holds: all that are lent when it names none, never more than are lent
fn job_cpus(spec: &JobSpec, lent: u32) -> u32 {
    if spec.cpu == 0 { lent } else { spec.cpu.min(lent) }.max(1)
}

fn brief(spec: &JobSpec, state: &str, cpu: u32, ram: u64, started: i64) -> JobBrief {
    JobBrief { id: spec.id.clone(), project: spec.project.clone(), slot: spec.slot.clone(), state: String::from(state), cpu, ram, started, command: spec.command_text().chars().take(200).collect() }
}

fn serve(dir: &str) -> i32 {
    let Some(cfg) = config::load(dir) else { return fail(&not_paired(dir)) };
    let p = match config::paired(&cfg) {
        Ok(p) => p,
        Err(e) => return fail(&e),
    };
    plat::log_file(Some(&plat::fs::join(&plat::fs::join(dir, "logs"), "worker.log")), 10 << 20, 3);
    // one worker per data folder
    let Some(_lock) = plat::FileLock::acquire(&plat::fs::join(dir, "worker.lock")) else { return fail("another bb-worker already runs on this data folder") };
    let relay = match Relay::open(&cfg.relay, &cfg.relay_fp, &cfg.id, &cfg.token) {
        Ok(r) => r,
        Err(e) => return fail(&e),
    };
    let cores = Cores::new(cfg.cpu);
    let state = j::load::<State>(&plat::fs::join(dir, "state.json")).unwrap_or_default();
    let w = Arc::new(Worker {
        dir: String::from(dir),
        cfg: Mutex::new(cfg.clone()),
        pair: p.pair,
        data: p.data,
        relay,
        sched: Mutex::new(Sched { queue: VecDeque::new(), running: Vec::new(), cores, dirty: true }),
        wake: Condvar::new(),
        tools: Mutex::new(Vec::new()),
        state: Mutex::new(state),
        started: now(),
    });
    out(&format!("{} worker \"{}\" ({}) for main machine \"{}\" through {}\n{}", bb_core::PRODUCT, cfg.name, cfg.id, cfg.main_name, cfg.relay, config::offer_text(&cfg)));
    if !w.sched.lock().cores.pinning() {
        plat::log_warn("worker", "jobs cannot be pinned to processors on this machine; they get the lent number as a hint only");
    }
    job::recover(&w);
    let w2 = w.clone();
    let _ = plat::thread_spawn(0, Box::new(move || inbox(&w2)));
    let w3 = w.clone();
    let _ = plat::thread_spawn(0, Box::new(move || tools_loop(&w3)));

    let mut meter = CpuMeter::default();
    meter.percent();
    let mut cfg_time = plat::fs::stat(&plat::fs::join(dir, "worker.json")).map_or(0, |m| m.mtime_ns);
    let (mut next_status, mut next_cfg) = (0u64, tick_ms() + 3000);
    let mut online: Option<bool> = None;
    while !plat::service::stop_requested() {
        let t = tick_ms();
        if t >= next_cfg {
            next_cfg = t + 3000;
            reload_config(&w, &mut cfg_time);
        }
        start_jobs(&w);
        watch_memory(&w);
        let dirty = core::mem::take(&mut w.sched.lock().dirty);
        if dirty || t >= next_status {
            next_status = t + 10_000;
            let ok = send_status(&w, &mut meter);
            if online != Some(ok) {
                plat::log_info("worker", if ok { "connected to the relay" } else { "the relay is not reachable; trying again" });
                online = Some(ok);
            }
        }
        let g = w.sched.lock();
        if !g.dirty {
            let _ = w.wake.wait_timeout(g, Some(500));
        }
    }
    plat::log_info("worker", "stopping");
    // the job threads see the stop request, end their processes and report
    for _ in 0..150 {
        if w.sched.lock().running.is_empty() {
            break;
        }
        plat::sleep_ms(100);
    }
    0
}

/// take over a changed worker.json (bb-worker config)
fn reload_config(w: &Worker, seen: &mut i64) {
    let path = plat::fs::join(&w.dir, "worker.json");
    let t = plat::fs::stat(&path).map_or(0, |m| m.mtime_ns);
    if t == *seen {
        // processors that could not be re-dealt while jobs ran
        let lent = w.cfg.lock().cpu;
        let mut s = w.sched.lock();
        if s.running.is_empty() && s.cores.lent_count() != lent && s.cores.pinning() {
            s.cores = Cores::new(lent);
        }
        return;
    }
    *seen = t;
    let Some(new) = config::load(&w.dir) else { return };
    let mut cur = w.cfg.lock();
    if new.id != cur.id || new.secret != cur.secret {
        plat::log_warn("worker", "worker.json now holds another pairing: restart bb-worker to use it");
        return;
    }
    if *cur != new {
        plat::log_info("worker", &format!("settings taken over: {} processors, {} memory, {} disk, {} jobs", new.cpu, size_text(new.ram), size_text(new.disk), new.jobs));
        *cur = new;
        drop(cur);
        w.sched.lock().dirty = true;
    }
}

/// start every queued job that fits into what is lent and free
fn start_jobs(w: &Arc<Worker>) {
    let (lent_cpu, lent_ram, lent_jobs) = {
        let c = w.cfg.lock();
        (c.cpu, c.ram, c.jobs.max(1))
    };
    loop {
        let mut s = w.sched.lock();
        let held_cpu: u32 = s.running.iter().map(|r| r.cpu).sum();
        let held_ram: u64 = s.running.iter().map(|r| r.ram).sum();
        if s.running.len() as u32 >= lent_jobs {
            return;
        }
        let fit = s.queue.iter().position(|q| {
            let ws_free = !s.running.iter().any(|r| r.spec.project_id == q.project_id && r.spec.slot == q.slot);
            ws_free && held_cpu + job_cpus(q, lent_cpu) <= lent_cpu && held_ram + q.ram <= lent_ram
        });
        let Some(i) = fit else { return };
        let Some(spec) = s.queue.remove(i) else { return };
        let cpu = job_cpus(&spec, lent_cpu);
        let mask = s.cores.take(cpu);
        let r = Arc::new(Running { ram: spec.ram, spec, state: Mutex::new(String::from(proto::ST_SYNCING)), cpu, mask, started: now(), cancel: AtomicBool::new(false), rss: AtomicU64::new(0), kill: Mutex::new(None) });
        s.running.push(r.clone());
        s.dirty = true;
        drop(s);
        let w2 = w.clone();
        if plat::thread_spawn(0, Box::new(move || job::run(w2, r.clone()))).is_none() {
            plat::log_error("worker", "no thread for a job could be started");
            return;
        }
    }
}

/// The memory limit: when the jobs together hold more than is lent, the one furthest above
/// what it asked for has to end.
fn watch_memory(w: &Worker) {
    let lent = w.cfg.lock().ram;
    let s = w.sched.lock();
    let total: u64 = s.running.iter().map(|r| r.rss.load(Relaxed)).sum();
    if lent == 0 || total <= lent {
        return;
    }
    if let Some(v) = s.running.iter().max_by_key(|r| r.rss.load(Relaxed).saturating_sub(r.ram)) {
        let mut k = v.kill.lock();
        if k.is_none() {
            let why = format!("memory limit: the jobs held {} of the {} this worker lends (this one {}; bb-worker config --ram)", size_text(total), size_text(lent), size_text(v.rss.load(Relaxed)));
            plat::log_warn("worker", &format!("{}: {}", v.spec.id, why));
            *k = Some(why);
        }
    }
}

fn send_status(w: &Worker, meter: &mut CpuMeter) -> bool {
    let c = w.cfg.lock().clone();
    let (free, total) = plat::fs::free_space(&w.dir).unwrap_or((0, 0));
    let workspaces = config::workspaces(&w.dir);
    // everything that asks the system or takes another lock comes before the scheduler's lock
    let (host, cpu_model, ram_free, cpu_busy) = (plat::net::hostname(), plat::cpu_brand(), bb_exec::sys::mem_available(), meter.percent());
    let tools = w.tools.lock().clone();
    let s = w.sched.lock();
    let st = WorkerStatus {
        time: now(),
        id: c.id.clone(),
        name: c.name.clone(),
        version: String::from(bb_core::VERSION),
        protocol: bb_core::PROTOCOL,
        os: String::from(bb_core::os_name()),
        arch: String::from(bb_core::arch_name()),
        host,
        cpu_model,
        cpu_total: plat::cpu_count(),
        ram_total: plat::mem_total(),
        ram_free,
        cpu_busy,
        disk_total: total,
        disk_free: free,
        offer: Offer { cpu: c.cpu, ram: c.ram, disk: c.disk, jobs: c.jobs },
        cpu_reserved: s.running.iter().map(|r| r.cpu).sum(),
        ram_reserved: s.running.iter().map(|r| r.ram).sum(),
        disk_used: workspaces.iter().map(|x| x.bytes).sum(),
        enforce_cpu: String::from(if s.cores.pinning() { "affinity" } else { "hint" }),
        enforce_ram: String::from("watchdog"),
        enforce_disk: String::from("quota"),
        running: s.running.iter().map(|r| brief(&r.spec, &r.state.lock(), r.cpu, r.ram, r.started)).collect(),
        queued: s.queue.iter().map(|q| brief(q, proto::ST_QUEUED, job_cpus(q, c.cpu), q.ram, 0)).collect(),
        workspaces,
        tools,
        tags: c.tags.clone(),
        started: w.started,
    };
    drop(s);
    let sealed = crypt::seal(&w.pair, &crypt::aad(&c.id, "status"), j::text(&st.to_json()).as_bytes());
    match w.relay.status_put(&sealed) {
        Ok(()) => true,
        Err(e) => {
            if e.status == 401 {
                plat::log_error("worker", "the relay no longer admits this worker (it was unpaired on the main machine); pair it again with bb-worker join --force");
            }
            false
        }
    }
}

/// look for the build tools now and every half hour (a new compiler shows up without a restart)
fn tools_loop(w: &Worker) {
    let mut first = true;
    loop {
        let extra = w.cfg.lock().tools.clone();
        // the first time each tool is told as it is found: jobs may be waiting for it
        let found = bb_exec::tools::detect_with(&extra, &mut |t| {
            if first {
                w.tools.lock().push(t.clone());
                w.sched.lock().dirty = true;
                w.wake.notify_all();
            }
        });
        first = false;
        let changed = {
            let mut t = w.tools.lock();
            let changed = *t != found;
            *t = found;
            changed
        };
        if changed {
            w.sched.lock().dirty = true;
            w.wake.notify_all();
        }
        for _ in 0..1800 {
            if plat::service::stop_requested() {
                return;
            }
            plat::sleep_ms(1000);
        }
    }
}

fn save_state(w: &Worker) {
    let s = w.state.lock().clone();
    bb_core::util::put_file(&plat::fs::join(&w.dir, "state.json"), j::pretty(&s.to_json()).as_bytes());
}

/// read orders from the relay until the worker stops
fn inbox(w: &Arc<Worker>) {
    let relay = w.relay.fork();
    let (id, main) = {
        let c = w.cfg.lock();
        (c.id.clone(), c.main_id.clone())
    };
    let channel = proto::inbox(&id);
    let aad = crypt::aad(&main, &channel);
    while !plat::service::stop_requested() {
        let after = w.state.lock().inbox_after;
        match relay.ch_read(&channel, after, bb_core::POLL_WAIT) {
            Ok(msgs) => {
                if msgs.is_empty() {
                    continue;
                }
                for (seq, sealed) in msgs {
                    match crypt::open(&w.pair, &aad, &sealed).and_then(|p| j::parse(&p)) {
                        Some(v) => order(w, &v),
                        None => plat::log_warn("worker", "an order that does not open was dropped (not from the paired main machine)"),
                    }
                    w.state.lock().inbox_after = seq;
                }
                save_state(w);
            }
            Err(e) => {
                // unpaired (401) or the relay is away: ask again later
                plat::sleep_ms(if e.status == 401 { 30_000 } else { 3000 });
            }
        }
    }
}

fn order(w: &Arc<Worker>, v: &Value) {
    match j::str_of(v, "t") {
        proto::ORDER_START => {
            let Some(spec) = v.get("job").and_then(JobSpec::from_json) else { return };
            take_job(w, spec);
        }
        proto::ORDER_CANCEL => {
            let id = j::str_of(v, "id");
            let mut s = w.sched.lock();
            if let Some(i) = s.queue.iter().position(|q| q.id == id) {
                let spec = s.queue.remove(i);
                s.dirty = true;
                drop(s);
                if let Some(spec) = spec {
                    job::refuse(w, &spec, proto::ST_CANCELLED, "cancelled before it started");
                }
            } else if let Some(r) = s.running.iter().find(|r| r.spec.id == id) {
                r.cancel.store(true, Relaxed);
            }
            w.wake.notify_all();
        }
        proto::ORDER_WS_REMOVE => {
            let (project, slot) = (j::str_of(v, "project_id"), j::str_of(v, "slot"));
            if !bb_core::util::id_ok(project) {
                return;
            }
            let busy = w.sched.lock().running.iter().any(|r| r.spec.project_id == project && (slot.is_empty() || r.spec.slot == slot));
            if busy {
                plat::log_warn("worker", "a workspace in use was not removed");
                return;
            }
            let path = if slot.is_empty() { bb_core::util::below(&w.dir, &format!("ws/{}", project)) } else if bb_core::util::slug(slot) == slot { config::ws_dir(&w.dir, project, slot) } else { return };
            bb_core::util::remove_tree(&path);
            plat::log_info("worker", &format!("workspace {} {} removed on the main machine's request", project, slot));
            w.sched.lock().dirty = true;
            w.wake.notify_all();
        }
        other => plat::log_warn("worker", &format!("an order of unknown kind \"{}\" was dropped (a newer main machine?)", other)),
    }
}

/// check a job and queue it
fn take_job(w: &Arc<Worker>, spec: JobSpec) {
    let c = w.cfg.lock().clone();
    if !bb_core::util::id_ok(&spec.id) || spec.main != c.main_id {
        return; // not a job of this pairing: there is nobody to tell
    }
    {
        let mut st = w.state.lock();
        if st.seen.contains(&spec.id) {
            return; // delivered twice
        }
        st.seen.push(spec.id.clone());
        let extra = st.seen.len().saturating_sub(2000);
        st.seen.drain(..extra);
    }
    let bad = if spec.protocol != bb_core::PROTOCOL {
        Some(format!("the job speaks protocol {}, this worker {}: update the older program", spec.protocol, bb_core::PROTOCOL))
    } else if !bb_core::util::id_ok(&spec.project_id) || spec.slot.is_empty() || bb_core::util::slug(&spec.slot) != spec.slot {
        Some(String::from("the job names no valid project or slot"))
    } else if !spec.cwd.is_empty() && !bb_core::util::rel_ok(&spec.cwd) {
        Some(format!("\"{}\" is not a folder inside the project", spec.cwd))
    } else if spec.env.iter().any(|e| e.name.is_empty() || e.name.contains(['=', '\0']) || e.value.contains('\0')) {
        Some(String::from("an environment variable has a name or value that cannot be set"))
    } else if spec.ram > c.ram {
        Some(format!("the job asks for {} of memory, this worker lends {}", size_text(spec.ram), size_text(c.ram)))
    } else {
        None
    };
    if let Some(why) = bad {
        job::refuse(w, &spec, proto::ST_ERROR, &why);
        return;
    }
    let detail = {
        let mut s = w.sched.lock();
        let ahead = s.queue.len() + s.running.len();
        s.queue.push_back(spec.clone());
        s.dirty = true;
        if ahead == 0 { String::new() } else { format!("{} job(s) ahead on this worker", ahead) }
    };
    plat::log_info("job", &format!("{} queued: {}", spec.id, spec.command_text()));
    job::Emitter::new(w, &spec.main, &spec.id).send(&Event::State { state: String::from(proto::ST_QUEUED), detail, time: now() }, 20);
    w.wake.notify_all();
}

rt::main!(run());
