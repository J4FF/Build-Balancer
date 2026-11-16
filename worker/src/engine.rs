// engine -- the worker itself: what `bb-worker run` and the desktop app both start
//
//   Engine::start(dir)     take the data folder, connect, and work on threads of its own:
//                          the inbox (orders from the relay), the scheduler (start jobs that
//                          fit, watch memory, report the status, follow worker.json, tidy up),
//                          the tool detection, and one thread per running job
//   engine.status()        what this machine offers and does right now (also sent to the main machine)
//   engine.online()        whether the relay took the last status
//   engine.remove_project  remove a project's files here (cleanup.rs)
//   engine.stop()          end the jobs, let the threads go
//
// A worker opens no port.  It asks the relay for orders (a long poll), runs jobs of the one
// main machine it paired with, and reports what it offers and does.

use alloc::boxed::Box;
use alloc::collections::VecDeque;
use alloc::format;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::Ordering::Relaxed;
use core::sync::atomic::{AtomicBool, AtomicU64, AtomicU8};

use bb_core::client::Relay;
use bb_core::crypt::{self, DataKey};
use bb_core::j::{self, Json, Value};
use bb_core::proto::{self, Event, JobBrief, JobSpec, Offer, Tool, WorkerStatus};
use bb_core::util::{now, size_text, tick_ms};
use bb_exec::{Cores, CpuMeter};
use plat::sync::{Condvar, Mutex};

use crate::config::{self, Config, State};
use crate::{cleanup, job};

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
    /// workspaces being removed right now (project id, slot; "": all its slots): no job starts in them
    pub removing: Vec<(String, String)>,
    /// the status changed: tell the main machine soon
    pub dirty: bool,
}

impl Sched {
    /// whether a job with this id waits or runs here
    pub fn has(&self, id: &str) -> bool {
        self.queue.iter().any(|q| q.id == id) || self.running.iter().any(|r| r.spec.id == id)
    }

    /// whether a workspace is being removed right now
    fn going(&self, project_id: &str, slot: &str) -> bool {
        self.removing.iter().any(|x| x.0 == project_id && (x.1.is_empty() || slot.is_empty() || x.1 == slot))
    }
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
    /// set once: every thread of this engine ends
    pub stop: Arc<AtomicBool>,
    /// Someone wants this worker to start again with newer programs (RESTART_*; 0: nobody).
    /// Whoever runs the engine -- `bb-worker run`, the app -- acts on it when no job is here.
    pub restart: AtomicU8,
    pub online: AtomicBool,
    meter: Mutex<CpuMeter>,
}

impl Worker {
    /// this engine was told to stop, or the system asks the whole program to
    pub fn stopping(&self) -> bool {
        self.stop.load(Relaxed) || plat::service::stop_requested()
    }

    /// whether a job runs in a project's workspace right now (`slot` "": in any of its slots)
    pub fn busy(&self, project_id: &str, slot: &str) -> bool {
        self.sched.lock().running.iter().any(|r| r.spec.project_id == project_id && (slot.is_empty() || r.spec.slot == slot))
    }

    /// tell the main machine soon that something changed
    pub fn touch(&self) {
        self.sched.lock().dirty = true;
        self.wake.notify_all();
    }

    /// Hold a workspace free of jobs so it can be removed (`slot` "": all slots of the project)
    /// -> false while a job runs in it, or while it is being removed already.  No job starts
    /// in it until release_removal.
    pub fn reserve_removal(&self, project_id: &str, slot: &str) -> bool {
        let mut s = self.sched.lock();
        if s.running.iter().any(|r| r.spec.project_id == project_id && (slot.is_empty() || r.spec.slot == slot)) || s.going(project_id, slot) {
            return false;
        }
        s.removing.push((String::from(project_id), String::from(slot)));
        true
    }

    pub fn release_removal(&self, project_id: &str, slot: &str) {
        let mut s = self.sched.lock();
        if let Some(i) = s.removing.iter().position(|x| x.0 == project_id && x.1 == slot) {
            s.removing.swap_remove(i);
        }
        s.dirty = true;
        drop(s);
        self.wake.notify_all();
    }

    /// Remove a project's files here (`slot` "": every slot) -> the bytes freed.  Err while a
    /// job runs in it; jobs that arrive meanwhile wait until it is gone.
    pub fn remove_project(&self, project_id: &str, slot: &str) -> Result<u64, String> {
        if !self.reserve_removal(project_id, slot) {
            return Err(String::from(cleanup::BUSY));
        }
        let r = cleanup::remove_project(&self.dir, project_id, slot);
        self.release_removal(project_id, slot);
        r
    }
}

/// Worker::restart: fetch the newest release first, then start again (the main machine asked)
pub const RESTART_UPDATE: u8 = 1;
/// Worker::restart: the programs were replaced already, only start again (`bb-worker update`)
pub const RESTART_ONLY: u8 = 2;
/// what `run` returns when the program is to be started again: as a service the service
/// manager does that for a program that ended with an error
pub const EXIT_RESTART: i32 = 75;

/// the file that asks the worker running on a data folder to start again (`bb-worker update`
/// writes it after it replaced the programs)
pub fn restart_request(dir: &str) -> String {
    plat::fs::join(dir, "restart.request")
}

/// Bring the programs in this program's folder up to the newest release -> the version that
/// was installed, or None when they are the newest already.
pub fn update_programs(say: &mut dyn FnMut(&str)) -> Result<Option<String>, String> {
    let c = bb_update::check()?;
    if !c.newer {
        return Ok(None);
    }
    let dir = bb_update::own_dir().ok_or("the program's own folder is unknown")?;
    bb_update::install(&c.latest, &dir, say)?;
    Ok(Some(c.latest))
}

pub fn not_paired(dir: &str) -> String {
    // a pairing made as root and looked for as the person behind sudo, or the other way round,
    // must not look like no pairing at all
    if let Some(other) = config::other_place(dir) {
        let whose = if plat::process::elevated() { "made without sudo: run this command without sudo, too" } else { "made as root: run this command with sudo, too" };
        return format!("{} holds no pairing, but {} does (it was {}; or name the folder with --data)", dir, other, whose);
    }
    // root's folder cannot be looked into from outside; that it is there is enough for a hint
    let system = plat::fs::parent(config::system_dir());
    if !cfg!(windows) && !plat::process::elevated() && dir != config::system_dir() && plat::fs::stat(config::system_dir()).or_else(|| plat::fs::stat(system)).is_some() {
        return format!("{} holds no pairing.  If this machine was paired with sudo, the pairing lies in {}: run this command with sudo, too", dir, config::system_dir());
    }
    format!("this machine is not paired yet ({} has no worker.json): run `bb pair` on the main machine, then `bb-worker join RELAY CODE` here", dir)
}

/// the processors a job holds: all that are lent when it names none, never more than are lent
fn job_cpus(spec: &JobSpec, lent: u32) -> u32 {
    if spec.cpu == 0 { lent } else { spec.cpu.min(lent) }.max(1)
}

fn brief(spec: &JobSpec, state: &str, cpu: u32, ram: u64, started: i64) -> JobBrief {
    JobBrief { id: spec.id.clone(), project: spec.project.clone(), slot: spec.slot.clone(), state: String::from(state), cpu, ram, started, command: spec.command_text().chars().take(200).collect() }
}

/// a worker at work
pub struct Engine {
    pub w: Arc<Worker>,
    /// one engine per data folder: held as long as it lives
    _lock: plat::FileLock,
    thread: Option<plat::Thread>,
}

impl Engine {
    pub fn start(dir: &str) -> Result<Engine, String> {
        let cfg = config::load(dir).ok_or_else(|| not_paired(dir))?;
        let p = config::paired(&cfg)?;
        let lock = plat::FileLock::acquire(&plat::fs::join(dir, "worker.lock")).ok_or("a worker already runs on this data folder (bb-worker, its service, or the Build Balancer app)")?;
        let relay = Relay::open(&cfg.relay, &cfg.relay_fp, &cfg.id, &cfg.token)?;
        let cores = Cores::new(cfg.cpu);
        let state = j::load::<State>(&plat::fs::join(dir, "state.json")).unwrap_or_default();
        // jobs that waited when the worker last stopped wait again
        let queue: VecDeque<JobSpec> = job::recover(dir, &cfg.main_id).into();
        if !queue.is_empty() {
            plat::log_info("worker", &format!("{} job(s) that waited before the restart are queued again", queue.len()));
        }
        let w = Arc::new(Worker {
            dir: String::from(dir),
            cfg: Mutex::new(cfg),
            pair: p.pair,
            data: p.data,
            relay,
            sched: Mutex::new(Sched { queue, running: Vec::new(), cores, removing: Vec::new(), dirty: true }),
            wake: Condvar::new(),
            tools: Mutex::new(Vec::new()),
            state: Mutex::new(state),
            started: now(),
            stop: Arc::new(AtomicBool::new(false)),
            restart: AtomicU8::new(0),
            online: AtomicBool::new(false),
            meter: Mutex::new(CpuMeter::default()),
        });
        if !w.sched.lock().cores.pinning() {
            plat::log_warn("worker", "jobs cannot be pinned to processors on this machine; they get the lent number as a hint only");
        }
        let (w1, w2, w3) = (w.clone(), w.clone(), w.clone());
        // without the inbox no order would ever arrive, without the scheduler none would start:
        // a worker that looks alive and does nothing is worse than one that does not start
        let threads = plat::thread_spawn(0, Box::new(move || inbox(&w1))).and_then(|_| plat::thread_spawn(0, Box::new(move || scheduler(&w3))));
        let Some(thread) = threads else {
            w.stop.store(true, Relaxed);
            return Err(String::from("the worker's threads could not be started (the system is out of threads or memory)"));
        };
        // the tools are a help for choosing a worker; jobs run without the list too
        if plat::thread_spawn(0, Box::new(move || tools_loop(&w2))).is_none() {
            plat::log_warn("worker", "the build tools of this machine could not be looked for (no thread)");
        }
        Ok(Engine { w, _lock: lock, thread: Some(thread) })
    }

    /// what this machine offers and does right now
    pub fn status(&self) -> WorkerStatus {
        status(&self.w)
    }

    pub fn online(&self) -> bool {
        self.w.online.load(Relaxed)
    }

    /// whether no job runs or waits here
    pub fn idle(&self) -> bool {
        let s = self.w.sched.lock();
        s.running.is_empty() && s.queue.is_empty()
    }

    /// remove a project's files here (Worker::remove_project)
    pub fn remove_project(&self, project_id: &str, slot: &str) -> Result<u64, String> {
        self.w.remove_project(project_id, slot)
    }

    /// End the engine: running jobs are stopped and reported as cancelled, the scheduler is
    /// waited for.  (The inbox thread may still wait at the relay for some seconds; it does
    /// nothing more when it comes back.)
    pub fn stop(mut self) {
        self.w.stop.store(true, Relaxed);
        self.w.wake.notify_all();
        if let Some(t) = self.thread.take() {
            t.join();
        }
    }
}

impl Drop for Engine {
    fn drop(&mut self) {
        self.w.stop.store(true, Relaxed);
        self.w.wake.notify_all();
    }
}

/// `bb-worker run`: work until the system asks the program to stop -> the exit code
pub fn run(dir: &str) -> i32 {
    // (a worker that was started again after an update: the one before it may hold the log
    // file for a moment longer)
    let log = plat::fs::join(&plat::fs::join(dir, "logs"), "worker.log");
    for _ in 0..30 {
        if plat::log_file(Some(&log), 10 << 20, 3) {
            break;
        }
        plat::sleep_ms(100);
    }
    let e = match Engine::start(dir) {
        Ok(e) => e,
        Err(m) => {
            plat::log_error("worker", &m);
            return 1;
        }
    };
    {
        let c = e.w.cfg.lock();
        bb_core::util::out(&format!("{} worker \"{}\" ({}) for main machine \"{}\" through {}\n{}", bb_core::PRODUCT, c.name, c.id, c.main_name, c.relay, config::offer_text(&c)));
    }
    plat::log_info("worker", &format!("version {} started", bb_core::VERSION));
    // an update that did not work is tried again a few times: a download can fail
    let (mut tries, mut not_before) = (0u32, 0u64);
    while !plat::service::stop_requested() {
        plat::sleep_ms(300);
        // a newer version: when nothing runs here, fetch it (if that is still to do) and start again
        let want = e.w.restart.load(Relaxed);
        if want != 0 && e.idle() && tick_ms() >= not_before {
            e.w.restart.store(0, Relaxed);
            let ready = match want {
                RESTART_ONLY => true,
                _ => match update_programs(&mut |m| plat::log_info("update", m)) {
                    Ok(Some(v)) => {
                        plat::log_info("update", &format!("version {} is installed", v));
                        true
                    }
                    Ok(None) => {
                        plat::log_info("update", "asked to update: this is the newest version already");
                        false
                    }
                    Err(m) => {
                        tries += 1;
                        if tries < 4 {
                            plat::log_warn("update", &format!("the update did not work, trying again in a minute: {}", m));
                            not_before = tick_ms() + 60_000;
                            e.w.restart.fetch_max(RESTART_UPDATE, Relaxed);
                        } else {
                            plat::log_warn("update", &format!("the update did not work, this version keeps running: {}", m));
                            tries = 0;
                        }
                        false
                    }
                },
            };
            if ready {
                plat::log_info("worker", "starting again with the new programs");
                e.stop();
                // The log file goes to whoever runs next.  While this program still holds it, the
                // new one cannot open it and would run without a log for as long as it lives.
                plat::log_file(None, 0, 0);
                return EXIT_RESTART;
            }
        }
    }
    plat::log_info("worker", "stopping");
    e.stop();
    0
}

/// start jobs, hold the limits, report, follow the settings, tidy up -- until the engine stops
fn scheduler(w: &Arc<Worker>) {
    w.meter.lock().percent();
    let mut cfg_time = plat::fs::stat(&plat::fs::join(&w.dir, "worker.json")).map_or(0, |m| m.mtime_ns);
    let (mut next_status, mut next_cfg, mut next_tidy, mut next_resend) = (0u64, tick_ms() + 3000, tick_ms() + 5000, 0u64);
    // an end is told again once the job's own attempt (five minutes at most) is surely over
    let mut resend_age = 0i64;
    let mut online: Option<bool> = None;
    while !w.stopping() {
        let t = tick_ms();
        if t >= next_cfg {
            next_cfg = t + 3000;
            reload_config(w, &mut cfg_time);
            cleanup::take_requests(&w.dir, &mut |project, slot| {
                if let Err(e) = w.remove_project(project, slot) {
                    plat::log_warn("worker", &e);
                }
            });
            // `bb-worker update` replaced the programs and asks whoever runs here to start again
            if plat::file_exists(&restart_request(&w.dir)) != 0 {
                plat::delete(&restart_request(&w.dir));
                w.restart.fetch_max(RESTART_ONLY, Relaxed);
            }
        }
        if t >= next_tidy {
            next_tidy = t + 3_600_000;
            let days = w.cfg.lock().keep_days;
            cleanup::expire(&w.dir, days, &|p, s| w.remove_project(p, s));
            job::prune(&w.dir);
        }
        if t >= next_resend {
            next_resend = t + 30_000;
            job::resend_pending(w, resend_age);
            resend_age = 360;
        }
        start_jobs(w);
        watch_memory(w);
        let dirty = core::mem::take(&mut w.sched.lock().dirty);
        if dirty || t >= next_status {
            next_status = t + 10_000;
            let ok = send_status(w);
            w.online.store(ok, Relaxed);
            if online != Some(ok) {
                plat::log_info("worker", if ok { "connected to the relay" } else { "the relay is not reachable; trying again" });
                online = Some(ok);
            }
        }
        let g = w.sched.lock();
        if !g.dirty && !w.stopping() {
            let _ = w.wake.wait_timeout(g, Some(500));
        }
    }
    // the job threads see the stop, end their processes and report
    for _ in 0..150 {
        if w.sched.lock().running.is_empty() {
            break;
        }
        plat::sleep_ms(100);
    }
    w.online.store(false, Relaxed);
}

/// take over a changed worker.json (bb-worker config, the app's sliders)
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
        plat::log_warn("worker", "worker.json now holds another pairing: restart the worker to use it");
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
        // the lent number changed: deal the processors anew as soon as no job holds any
        if s.running.is_empty() && s.cores.pinning() && s.cores.lent_count() != lent_cpu {
            s.cores = Cores::new(lent_cpu);
        }
        // what the allocator can really pin a job to (fewer than lent when this process itself
        // may only run on some processors)
        let pin_max = s.cores.lent_count();
        let pinned = |cpu: u32| cpu.min(pin_max);
        let fit = s.queue.iter().position(|q| {
            let cpu = job_cpus(q, lent_cpu);
            // one job per workspace, and none in a workspace that is being removed
            let ws_free = !s.running.iter().any(|r| r.spec.project_id == q.project_id && r.spec.slot == q.slot) && !s.going(&q.project_id, &q.slot);
            // a job that is to be pinned waits for processors of its own rather than run on all
            let cores_free = !s.cores.pinning() || s.cores.free_count() >= pinned(cpu);
            ws_free && cores_free && held_cpu + cpu <= lent_cpu && held_ram + q.ram <= lent_ram
        });
        let Some(i) = fit else { return };
        let Some(spec) = s.queue.remove(i) else { return };
        let cpu = job_cpus(&spec, lent_cpu);
        let take = pinned(cpu);
        let mask = s.cores.take(take);
        let r = Arc::new(Running { ram: spec.ram, spec, state: Mutex::new(String::from(proto::ST_SYNCING)), cpu, mask, started: now(), cancel: AtomicBool::new(false), rss: AtomicU64::new(0), kill: Mutex::new(None) });
        s.running.push(r.clone());
        s.dirty = true;
        drop(s);
        let (w2, r2) = (w.clone(), r.clone());
        if plat::thread_spawn(0, Box::new(move || job::run(w2, r2))).is_none() {
            // give back what the job held and tell its main machine: nothing may wait for a job that never ran
            plat::log_error("worker", "no thread for a job could be started");
            {
                let mut s = w.sched.lock();
                s.running.retain(|x| !Arc::ptr_eq(x, &r));
                s.cores.give_back(r.mask);
                s.dirty = true;
            }
            job::refuse(w, &r.spec, proto::ST_ERROR, "the worker could not start a thread for the job (out of threads or memory); run it again");
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

fn status(w: &Worker) -> WorkerStatus {
    let c = w.cfg.lock().clone();
    let (free, total) = plat::fs::free_space(&w.dir).unwrap_or((0, 0));
    let workspaces = config::workspaces(&w.dir);
    // everything that asks the system or takes another lock comes before the scheduler's lock
    let (host, cpu_model, ram_free, cpu_busy) = (plat::net::hostname(), plat::cpu_brand(), bb_exec::sys::mem_available(), w.meter.lock().percent());
    let tools = w.tools.lock().clone();
    let s = w.sched.lock();
    WorkerStatus {
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
    }
}

fn send_status(w: &Worker) -> bool {
    let st = status(w);
    let sealed = crypt::seal(&w.pair, &crypt::aad(&st.id, "status"), j::text(&st.to_json()).as_bytes());
    match w.relay.status_put(&sealed) {
        Ok(()) => true,
        Err(e) => {
            if e.status == 401 {
                plat::log_error("worker", "the relay no longer admits this worker (it was unpaired on the main machine); pair it again");
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
                w.touch();
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
            w.touch();
        }
        for _ in 0..1800 {
            if w.stopping() {
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

/// read orders from the relay until the engine stops
fn inbox(w: &Arc<Worker>) {
    let relay = w.relay.fork();
    let (id, main) = {
        let c = w.cfg.lock();
        (c.id.clone(), c.main_id.clone())
    };
    let channel = proto::inbox(&id);
    let aad = crypt::aad(&main, &channel);
    while !w.stopping() {
        let after = w.state.lock().inbox_after;
        match relay.ch_read(&channel, after, bb_core::POLL_WAIT) {
            Ok(msgs) => {
                // an engine that stopped meanwhile leaves the orders to the next one
                if msgs.is_empty() || w.stopping() {
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
                for _ in 0..(if e.status == 401 { 100 } else { 10 }) {
                    if w.stopping() {
                        return;
                    }
                    plat::sleep_ms(300);
                }
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
            match w.remove_project(project, slot) {
                Ok(bytes) => plat::log_info("worker", &format!("project {} removed on the main machine's request ({} freed)", project, size_text(bytes))),
                Err(e) => plat::log_warn("worker", &e),
            }
        }
        proto::ORDER_UPDATE => {
            plat::log_info("worker", "the main machine asks for the newest version: it is fetched as soon as no job is here");
            w.restart.store(RESTART_UPDATE, Relaxed);
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
    // a job whose folder exists was taken before (the worker stopped before it could note the
    // inbox position): it waits, runs or ended already
    let spec_path = plat::fs::join(&config::job_dir(&w.dir, &spec.id), "spec.json");
    if plat::file_exists(&spec_path) == 1 {
        return;
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
    // on disk before it is queued: a job that waits survives a restart of the worker
    if !bb_core::util::put_file(&spec_path, j::pretty(&spec.to_json()).as_bytes()) {
        job::refuse(w, &spec, proto::ST_ERROR, "the worker could not write the job to its disk");
        return;
    }
    let detail = {
        let mut s = w.sched.lock();
        if s.has(&spec.id) {
            return;
        }
        let ahead = s.queue.len() + s.running.len();
        s.queue.push_back(spec.clone());
        s.dirty = true;
        if ahead == 0 { String::new() } else { format!("{} job(s) ahead on this worker", ahead) }
    };
    plat::log_info("job", &format!("{} queued: {}", spec.id, spec.command_text()));
    job::Emitter::new(w, &spec.main, &spec.id).send(&Event::State { state: String::from(proto::ST_QUEUED), detail, time: now() }, 20);
    w.wake.notify_all();
}
