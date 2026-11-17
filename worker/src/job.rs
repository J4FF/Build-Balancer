// job -- one job from start to end, on its own thread
//
//   syncing      fetch the project's tree and make the workspace hold it (only what changed)
//   running      start the command on the job's processors; its output goes to the main machine
//                as it comes and into jobs/<id>/out.log; cancel, time limit, memory and disk
//                limits end it with everything it started
//   collecting   upload the files the output patterns name
//   done         one last event with the exit code, the time, the peak memory, the outputs
//
// Events are sealed for the main machine and appended to the job's channel at the relay.
// The main machine need not be there: it reads them whenever it asks.

use alloc::format;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::Ordering::Relaxed;
use core::sync::atomic::{AtomicBool, AtomicU64};

use bb_core::client::Relay;
use bb_core::crypt;
use bb_core::j::{self, Json};
use bb_core::proto::{self, Done, Event, JobSpec, TreeRef, Workspace};
use bb_core::util::{below, now, size_text, tick_ms};
use bb_sync::{Apply, Known};

use crate::config::{self, Place};
use crate::engine::{Running, Worker};

/// output relayed per job; beyond it only the file on the worker grows
const LOG_RELAY_MAX: u64 = 32 << 20;
/// output kept per job in out.log
const LOG_FILE_MAX: u64 = 512 << 20;
/// the most files a job sends back
const OUTPUT_FILES_MAX: usize = 20_000;

/// sends a job's events
pub struct Emitter {
    relay: Relay,
    key: [u8; 32],
    aad: Vec<u8>,
    channel: String,
    stop: Arc<core::sync::atomic::AtomicBool>,
}

impl Emitter {
    pub fn new(w: &Worker, main: &str, job: &str) -> Emitter {
        let channel = proto::job_channel(main, job);
        let id = w.cfg.lock().id.clone();
        Emitter { relay: w.relay.fork(), key: w.pair, aad: crypt::aad(&id, &channel), channel, stop: w.stop.clone() }
    }

    /// Send an event, trying again for up to `patience` seconds while the relay cannot be
    /// reached -> whether it arrived.
    pub fn send(&self, e: &Event, patience: u64) -> bool {
        let sealed = crypt::seal(&self.key, &self.aad, j::text(&e.to_json()).as_bytes());
        let start = tick_ms();
        let end = start + patience * 1000;
        loop {
            match self.relay.ch_append(&self.channel, &sealed) {
                Ok(_) => return true,
                // not reachable right now: wait; anything else (removed from the relay, channel full) will not get better
                Err(e) if e.status == 0 || e.status >= 500 && e.status != 507 => {
                    // a worker that is stopping tries a little longer for what must arrive, then lets go
                    let stopping = self.stop.load(Relaxed) || plat::service::stop_requested();
                    if tick_ms() >= end.min(if stopping { start + 5000 } else { u64::MAX }) {
                        return false;
                    }
                    plat::sleep_ms(1500);
                }
                Err(_) => return false,
            }
        }
    }
}

/// the end of a byte string that is the start of a UTF-8 character still to be completed
fn incomplete_tail(b: &[u8]) -> usize {
    for back in 1..=b.len().min(3) {
        let c = b[b.len() - back];
        if c & 0xC0 == 0x80 {
            continue; // a continuation byte: look further back for its lead
        }
        let want = if c >= 0xF0 { 4 } else if c >= 0xE0 { 3 } else if c >= 0xC0 { 2 } else { 1 };
        return if want > back { back } else { 0 };
    }
    0
}

/// A job's output on its way: to the file at once, to the main machine through a thread of
/// its own -- a relay that is slow or away must never hold the loop that watches the job (its
/// pipes would fill, its limits would go unchecked).
struct LogBuf {
    file: Option<plat::File>,
    file_len: u64,
    /// (is stderr, bytes) in the order they came
    segs: Vec<(bool, Vec<u8>)>,
    pending: usize,
    /// the unfinished character at the end of each stream
    carry: [Vec<u8>; 2],
    relayed: u64,
    cut: bool,
    last: u64,
    tx: Option<plat::sync::Sender<(bool, String)>>,
    sender: Option<plat::Thread>,
    /// bytes that could not be sent (the queue was full, or the relay away for too long)
    lost: Arc<AtomicU64>,
    /// the job is over: nothing waits long for the relay any more
    closing: Arc<AtomicBool>,
}

/// the pieces of output that wait for the relay; beyond them output is only in the file
const LOG_QUEUE: usize = 256;

/// the thread that sends a job's output, piece by piece in the order it came
fn log_sender(em: Emitter, rx: plat::sync::Receiver<(bool, String)>, lost: Arc<AtomicU64>, closing: Arc<AtomicBool>) {
    let mut told = 0u64;
    let mut gave_up = false;
    while let Some((err, data)) = rx.recv() {
        let ending = closing.load(Relaxed);
        // at the end of a job one failure is enough: the rest stays in the file
        if gave_up || !em.send(&Event::Log { err, data: data.clone() }, if ending { 3 } else { 20 }) {
            lost.fetch_add(data.len() as u64, Relaxed);
            gave_up = ending;
            continue;
        }
        let l = lost.load(Relaxed);
        if l > told {
            let note = format!("[build balancer] {} of output could not be sent while the relay was unreachable; all of it is in the worker's job folder\n", size_text(l - told));
            told = l;
            em.send(&Event::Log { err: true, data: note }, 3);
        }
    }
    let l = lost.load(Relaxed);
    if l > told {
        em.send(&Event::Log { err: true, data: format!("[build balancer] {} of output could not be sent; all of it is in the worker's job folder\n", size_text(l - told)) }, 3);
    }
}

impl LogBuf {
    fn new(job_dir: &str, em: Emitter) -> LogBuf {
        let (tx, rx) = plat::sync::channel::<(bool, String)>(LOG_QUEUE);
        let (lost, closing) = (Arc::new(AtomicU64::new(0)), Arc::new(AtomicBool::new(false)));
        let (l2, c2) = (lost.clone(), closing.clone());
        let sender = plat::thread_spawn(0, alloc::boxed::Box::new(move || log_sender(em, rx, l2, c2)));
        LogBuf {
            file: plat::File::open(&plat::fs::join(job_dir, "out.log"), plat::FO_WRITE),
            file_len: 0,
            segs: Vec::new(),
            pending: 0,
            carry: [Vec::new(), Vec::new()],
            relayed: 0,
            cut: false,
            last: tick_ms(),
            // without the thread the output still reaches the file
            tx: sender.is_some().then_some(tx),
            sender,
            lost,
            closing,
        }
    }

    /// hand a piece to the sending thread without ever waiting (a full queue counts as lost)
    fn queue(&self, err: bool, text: String) {
        let len = text.len() as u64;
        if self.tx.as_ref().is_none_or(|tx| tx.try_send((err, text)).is_err()) {
            self.lost.fetch_add(len, Relaxed);
        }
    }

    /// the job is over: hand over the rest and wait for the sending thread (the result follows the output)
    fn finish(&mut self) {
        self.flush(true);
        self.closing.store(true, Relaxed);
        self.tx = None;
        if let Some(t) = self.sender.take() {
            t.join();
        }
    }

    fn push(&mut self, err: bool, data: &[u8]) {
        if let Some(f) = &self.file
            && self.file_len < LOG_FILE_MAX && f.write_at(self.file_len, data).is_some() {
                self.file_len += data.len() as u64;
            }
        if self.cut {
            return;
        }
        match self.segs.last_mut() {
            Some((e, b)) if *e == err && b.len() < 48 << 10 => b.extend_from_slice(data),
            _ => self.segs.push((err, data.to_vec())),
        }
        self.pending += data.len();
    }

    /// queue what has gathered when it is enough, old enough, or `force`
    fn flush(&mut self, force: bool) {
        if self.pending == 0 || !(force || self.pending >= 32 << 10 || tick_ms() - self.last >= 300) {
            return;
        }
        self.last = tick_ms();
        self.pending = 0;
        for (err, bytes) in core::mem::take(&mut self.segs) {
            let carry = &mut self.carry[err as usize];
            carry.extend_from_slice(&bytes);
            let keep = if force { 0 } else { incomplete_tail(carry) };
            let whole = carry.len() - keep;
            let text = String::from_utf8_lossy(&carry[..whole]).into_owned();
            carry.drain(..whole);
            if text.is_empty() {
                continue;
            }
            self.relayed += text.len() as u64;
            self.queue(err, text);
            if self.relayed > LOG_RELAY_MAX {
                self.cut = true;
                self.queue(true, format!("[build balancer] output cut here after {}; the rest is in the worker's job folder\n", size_text(LOG_RELAY_MAX)));
                self.segs.clear();
                break;
            }
        }
    }
}

fn set_state(w: &Worker, r: &Running, em: &Emitter, state: &str, detail: String) {
    *r.state.lock() = String::from(state);
    w.sched.lock().dirty = true;
    w.wake.notify_all();
    em.send(&Event::State { state: String::from(state), detail, time: now() }, 20);
}

/// Record a job's end in its folder and tell the main machine.  An end the relay did not take
/// is sent again later (resend_pending): the main machine must never wait forever.
pub fn finish(w: &Worker, spec: &JobSpec, done: &Done, patience: u64) {
    let jd = config::job_dir(&w.dir, &spec.id);
    if plat::file_exists(&plat::fs::join(&jd, "spec.json")) != 1 {
        bb_core::util::put_file(&plat::fs::join(&jd, "spec.json"), j::pretty(&spec.to_json()).as_bytes());
    }
    bb_core::util::put_file(&plat::fs::join(&jd, "done.json"), j::pretty(&done.to_json()).as_bytes());
    if Emitter::new(w, &spec.main, &spec.id).send(&Event::Done(done.clone()), patience) {
        bb_core::util::put_file(&plat::fs::join(&jd, "sent"), b"1");
    }
}

/// tell the main machine that a job will not run
pub fn refuse(w: &Worker, spec: &JobSpec, state: &str, why: &str) {
    plat::log_info("job", &format!("{} refused: {}", spec.id, why));
    finish(w, spec, &Done { state: String::from(state), exit: -1, error: String::from(why), time: now(), ..Done::default() }, 20);
}

/// the whole job; ends by giving its processors back and waking the scheduler
pub fn run(w: Arc<Worker>, r: Arc<Running>) {
    let spec = &r.spec;
    let em = Emitter::new(&w, &spec.main, &spec.id);
    let jd = config::job_dir(&w.dir, &spec.id);
    plat::mkdir(&jd);
    bb_core::util::put_file(&plat::fs::join(&jd, "spec.json"), j::pretty(&spec.to_json()).as_bytes());
    // from here on a restart finds a job that ran, not one that still waits its turn
    bb_core::util::put_file(&plat::fs::join(&jd, "started"), b"1");
    plat::log_info("job", &format!("{} starts: {} ({} processors)", spec.id, spec.command_text(), r.cpu));
    let t0 = tick_ms();
    let mut done = Done { state: String::from(proto::ST_ERROR), exit: -1, ..Done::default() };
    let place = place_of(&w, spec);
    if let Err(e) = execute(&w, &r, &em, &jd, &place, &mut done) {
        done.state = String::from(proto::ST_ERROR);
        done.error = e;
    }
    done.duration_ms = tick_ms() - t0;
    done.time = now();
    plat::log_info("job", &format!("{} ends: {} (exit {}){}", spec.id, done.state, done.exit, if done.error.is_empty() { String::new() } else { format!(" -- {}", done.error) }));
    bb_core::util::put_file(&plat::fs::join(&jd, "done.json"), j::pretty(&done.to_json()).as_bytes());
    if em.send(&Event::Done(done), 300) {
        bb_core::util::put_file(&plat::fs::join(&jd, "sent"), b"1");
    }
    // the workspace's size, for the quota and the status (the workspace stays held meanwhile)
    let wsd = config::ws_dir(&place.root, &spec.project_id, &spec.slot);
    if plat::file_exists(&wsd) == 2 {
        config::save_workspace(&place.root, &Workspace { project_id: spec.project_id.clone(), project: spec.project.clone(), slot: spec.slot.clone(), bytes: bb_core::util::tree_size(&wsd, u64::MAX), used: now() });
    }
    {
        let mut s = w.sched.lock();
        s.running.retain(|x| x.spec.id != spec.id);
        // (a light job held none: its mask is all of them)
        if !spec.light {
            s.cores.give_back(r.mask);
        }
        s.dirty = true;
    }
    w.wake.notify_all();
}

/// free bytes of the volume a folder lies on -- or will lie on: asked at the nearest folder
/// above it that is there
fn volume_free(path: &str) -> u64 {
    let mut p = path;
    loop {
        if let Some(f) = plat::fs::free_space(p) {
            return f.0;
        }
        let up = plat::fs::parent(p);
        if up.is_empty() || up == p {
            return 0;
        }
        p = up;
    }
}

/// what may still be written in a place: what is left of its limit, and never its volume to the brim
fn room(place: &Place) -> u64 {
    let used: u64 = config::workspaces_in(&place.root).iter().map(|x| x.bytes).sum();
    place.limit.saturating_sub(used).min(volume_free(&place.root).saturating_sub(256 << 20))
}

/// The place a job's project is kept in: the one that holds its copy -- unless that one was
/// given up --, else the one with the most room.  A copy on a drive that is not there right
/// now is not waited for: the project is fetched again, to a place that is.
fn place_of(w: &Worker, spec: &JobSpec) -> Place {
    let c = w.cfg.lock().clone();
    let mut places = config::places(&w.dir, &c);
    let lent: Vec<Place> = places.iter().filter(|p| p.limit > 0).cloned().collect();
    if let Some(p) = lent.iter().find(|p| plat::file_exists(&config::ws_dir(&p.root, &spec.project_id, &spec.slot)) == 2) {
        return p.clone();
    }
    // (nothing lent that is there: the data folder, whose limit then says why nothing fits)
    lent.into_iter().max_by_key(room).unwrap_or_else(|| places.swap_remove(0))
}

/// what the workspaces of a place hold together, and the one of this job alone
fn disk_used(place: &Place, spec: &JobSpec) -> (u64, u64) {
    let all = config::workspaces_in(&place.root);
    let own = all.iter().find(|x| x.project_id == spec.project_id && x.slot == spec.slot).map_or(0, |x| x.bytes);
    (all.iter().map(|x| x.bytes).sum(), own)
}

/// Make room for `need` more bytes inside what a place lends: workspaces nobody uses right
/// now go, the one unused the longest first -> the bytes that may be written.
fn make_room(w: &Worker, spec: &JobSpec, place: &Place, need: u64) -> u64 {
    loop {
        let all = config::workspaces_in(&place.root);
        let used: u64 = all.iter().map(|x| x.bytes).sum();
        let left = place.limit.saturating_sub(used);
        if need <= left {
            return left;
        }
        // the one unused the longest that can be held free of jobs while it goes
        let mut rest: Vec<&Workspace> = all.iter().filter(|x| !(x.project_id == spec.project_id && x.slot == spec.slot)).collect();
        rest.sort_by_key(|x| x.used);
        let Some(v) = rest.into_iter().find(|x| w.reserve_removal(&x.project_id, &x.slot)) else { return left };
        plat::log_info("disk", &format!("workspace {} / {} ({}) removed to make room", v.project, v.slot, size_text(v.bytes)));
        let gone = bb_core::util::remove_tree(&config::ws_dir(&place.root, &v.project_id, &v.slot));
        w.release_removal(&v.project_id, &v.slot);
        if !gone {
            return left;
        }
    }
}

/// a place in words, for a message: "this worker's disk", or the drive
fn place_name(place: &Place) -> String {
    if place.path.is_empty() { String::from("this worker's disk") } else { format!("this worker's drive {}", place.path) }
}

fn execute(w: &Worker, r: &Running, em: &Emitter, jd: &str, place: &Place, done: &mut Done) -> Result<(), String> {
    let spec = &r.spec;
    let wsd = config::ws_dir(&place.root, &spec.project_id, &spec.slot);
    let tree_dir = plat::fs::join(&wsd, "tree");
    let known_path = plat::fs::join(&wsd, "known");
    let relay = w.relay.fork();

    // the project's files
    if !spec.tree.is_empty() {
        set_state(w, r, em, proto::ST_SYNCING, format!("{} files, {}", spec.tree.files, size_text(spec.tree.bytes)));
        let tree = bb_sync::fetch_tree(&relay, &w.data, &spec.tree)?;
        let mut known = Known::load(&known_path);
        let need = bb_sync::need_bytes(&tree_dir, &tree, &known);
        let mut budget = make_room(w, spec, place, need);
        // never fill the volume to the brim, whatever is lent
        budget = budget.min(volume_free(&place.root).saturating_sub(256 << 20));
        if need > budget {
            return Err(format!(
                "the project needs {} more on {}, but only {} are free of the {} it lends there (bb-worker config {})",
                size_text(need),
                place_name(place),
                size_text(budget),
                size_text(place.limit),
                if place.path.is_empty() { "--disk" } else { "--store" }
            ));
        }
        let st = bb_sync::apply(&relay, &w.data, &tree_dir, &tree, &mut known, &Apply { delete: true, compare: false, budget }, &mut |_, _| {});
        // also after a failure: what arrived is not fetched again
        known.save(&known_path);
        let st = st?;
        done.sync_files = st.written;
        done.sync_bytes = st.bytes;
        config::save_workspace(&place.root, &Workspace { project_id: spec.project_id.clone(), project: spec.project.clone(), slot: spec.slot.clone(), bytes: disk_used(place, spec).1.max(tree.bytes()), used: now() });
    } else if !plat::mkdir(&tree_dir) {
        return Err(format!("{} cannot be created", tree_dir));
    }
    if spec.argv.is_empty() && spec.script.is_empty() {
        // a sync without a command
        done.state = String::from(proto::ST_DONE);
        done.exit = 0;
        return Ok(());
    }
    if r.cancel.load(Relaxed) {
        done.state = String::from(proto::ST_CANCELLED);
        return Ok(());
    }

    // the command
    let cwd = if spec.cwd.is_empty() { tree_dir.clone() } else { below(&tree_dir, &spec.cwd) };
    if plat::file_exists(&cwd) != 2 {
        return Err(format!("the project has no folder \"{}\" to run in", spec.cwd));
    }
    let (word, args) = if spec.script.is_empty() { (spec.argv[0].clone(), spec.argv[1..].to_vec()) } else { bb_exec::script(jd, &spec.shell, &spec.script)? };
    let program = bb_exec::resolve(&word, &cwd).ok_or_else(|| format!("{}: this worker has no such program (see its tools: bb workers)", word))?;
    if !cfg!(windows) && program.starts_with(&tree_dir) {
        // a script of the project that came from Windows has no executable bit
        if let Some(m) = plat::fs::stat(&program)
            && m.mode & 0o100 == 0 {
                plat::fs::set_mode(&program, 0o755);
            }
    }
    let (worker_name, low_priority, may_leave) = {
        let c = w.cfg.lock();
        // (a light job at the priority the jobs have: below them it would wait behind every
        // build on a busy machine, and it is the one that should be answered now)
        (c.name.clone(), c.priority != "normal", c.leave_running)
    };
    // (the limit of the place as it was when the job started)
    let lent_disk = place.limit;
    let mut env: Vec<(String, Option<String>)> = Vec::new();
    // (a light job holds no processor, but the build tools are told one: 0 is no number of jobs to them)
    let cpus = format!("{}", r.cpu.max(1));
    for (k, v) in [
        ("BB_JOB_ID", spec.id.as_str()),
        ("BB_PROJECT", spec.project.as_str()),
        ("BB_WORKSPACE", tree_dir.as_str()),
        ("BB_WORKER", worker_name.as_str()),
        ("BB_CPUS", cpus.as_str()),
        // tools that would otherwise start one process per processor of the whole machine
        ("CARGO_BUILD_JOBS", cpus.as_str()),
        ("CMAKE_BUILD_PARALLEL_LEVEL", cpus.as_str()),
        ("GOMAXPROCS", cpus.as_str()),
    ] {
        env.push((String::from(k), Some(String::from(v))));
    }
    if cfg!(windows) {
        env.push((String::from("NUMBER_OF_PROCESSORS"), Some(cpus.clone())));
    }
    if r.ram > 0 {
        env.push((String::from("BB_RAM_MB"), Some(format!("{}", r.ram >> 20))));
    }
    for e in &spec.env {
        env.retain(|(k, _)| !k.eq_ignore_ascii_case(&e.name));
        env.push((e.name.clone(), Some(e.value.clone())));
    }
    set_state(w, r, em, proto::ST_RUNNING, spec.command_text());
    if spec.leave_running {
        return leave_running(w, spec, jd, may_leave, &worker_name, &program, &args, &cwd, &env, done);
    }
    let mut p = bb_exec::spawn(&bb_exec::Spec { program, args, cwd, env, affinity: r.mask, low_priority })?;

    let mut log = LogBuf::new(jd, Emitter::new(w, &spec.main, &spec.id));
    // a light job runs LIGHT_SECS at most, whatever it asked for
    let limit = if spec.light { if spec.timeout == 0 { proto::LIGHT_SECS } else { spec.timeout.min(proto::LIGHT_SECS) } } else { spec.timeout };
    let deadline = (limit > 0).then(|| tick_ms() + limit * 1000);
    let (used0, own0) = disk_used(place, spec);
    let free0 = plat::fs::free_space(&wsd).map_or(0, |f| f.0);
    let (mut next_mem, mut next_disk, mut next_exact) = (tick_ms() + 500, tick_ms() + 10_000, 0u64);
    let mut peak = 0u64;
    let mut stop: Option<(&str, String)> = None;
    let exit = loop {
        let any = p.pump(&mut |err, b| log.push(err, b));
        log.flush(false);
        if let Some(code) = p.exited() {
            break code;
        }
        let t = tick_ms();
        if stop.is_none() {
            if r.cancel.load(Relaxed) {
                stop = Some((proto::ST_CANCELLED, String::from("cancelled")));
            } else if let Some(why) = r.kill.lock().take() {
                stop = Some((proto::ST_KILLED, why));
            } else if deadline.is_some_and(|d| t > d) {
                stop = Some((proto::ST_KILLED, if spec.light && limit != spec.timeout {
                    format!("a light job runs {} at most (bb run --light): this one needs a job of its own", bb_core::util::dur_text(limit * 1000))
                } else {
                    format!("time limit of {} reached", bb_core::util::dur_text(limit * 1000))
                }));
            } else if w.stopping() {
                stop = Some((proto::ST_CANCELLED, String::from("the worker was stopped")));
            } else if t >= next_disk {
                next_disk = t + 10_000;
                let free = plat::fs::free_space(&wsd).map_or(u64::MAX, |f| f.0);
                if free < 128 << 20 {
                    stop = Some((proto::ST_KILLED, String::from("the worker's disk is full")));
                } else if used0 + done.sync_bytes + free0.saturating_sub(free) > lent_disk && t >= next_exact {
                    // the volume shrank by that much; whether it was this job says only a look at its folder
                    next_exact = t + 60_000;
                    let others = used0 - own0;
                    let own = bb_core::util::tree_size(&wsd, lent_disk.saturating_sub(others));
                    if others + own > lent_disk {
                        stop = Some((proto::ST_KILLED, format!("disk limit: the workspaces grew beyond the {} lent on {} (bb-worker config {})", size_text(lent_disk), place_name(place), if place.path.is_empty() { "--disk" } else { "--store" })));
                    }
                }
            }
            if stop.is_some() {
                p.kill();
            }
        }
        if t >= next_mem {
            next_mem = t + 2000;
            let m = p.memory();
            r.rss.store(m, Relaxed);
            peak = peak.max(m);
        }
        if !any {
            plat::sleep_ms(20);
        }
    };
    // what it wrote just before the end; a child it left behind may hold the pipes open, so not forever
    let end = tick_ms() + 1500;
    while !p.drained() && tick_ms() < end {
        if !p.pump(&mut |err, b| log.push(err, b)) {
            plat::sleep_ms(10);
        }
    }
    log.finish();
    r.rss.store(0, Relaxed);
    done.exit = exit as i64;
    done.peak_ram = peak;
    match stop {
        Some((state, why)) => {
            done.state = String::from(state);
            done.error = why;
        }
        None => done.state = String::from(if exit == 0 { proto::ST_DONE } else { proto::ST_FAILED }),
    }

    // the outputs: also of a failed command (a test report is worth most then), not of a cancelled one
    if !spec.outputs.is_empty() && done.state != proto::ST_CANCELLED {
        set_state(w, r, em, proto::ST_COLLECTING, String::new());
        match collect(w, &relay, &tree_dir, &spec.outputs) {
            Ok(t) => done.artifacts = t,
            Err(e) => {
                if done.state == proto::ST_DONE {
                    done.state = String::from(proto::ST_ERROR);
                }
                done.error = if done.error.is_empty() { format!("the outputs could not be sent: {}", e) } else { format!("{}; the outputs could not be sent: {}", done.error, e) };
            }
        }
    }
    Ok(())
}

/// what a worker says to a job that is to leave a program running where its owner does not allow it
pub const LEAVE_REFUSED: &str = "this worker lets no program outlive its job: on it, `bb-worker config --leave-running yes` (or the switch in the app) allows that";

/// `bb run --leave-running`: hand the program to the system to start on its own -- outside
/// the worker's process tree, so the end of the job is not its end -- and end the job.  Its
/// output goes to a file in the job's folder; on Linux and macOS its process id is told.  The
/// job's result says that the start was handed over, not that the program runs: nothing
/// tells the worker that.  What the log holds after a moment -- a program that could not
/// start says so there -- goes into the job's output.
#[allow(clippy::too_many_arguments)]
fn leave_running(w: &Worker, spec: &JobSpec, jd: &str, allowed: bool, worker: &str, program: &str, args: &[String], cwd: &str, env: &[(String, Option<String>)], done: &mut Done) -> Result<(), String> {
    if !allowed {
        return Err(String::from(LEAVE_REFUSED));
    }
    let (log_path, pid_path) = (plat::fs::join(jd, "left-running.log"), plat::fs::join(jd, "left-running.pid"));
    let (shell, mut shell_args, text, ext) = bb_exec::launcher(program, args, cwd, env, &log_path, &pid_path)?;
    let script = plat::fs::join(jd, &format!("left-running.{}", ext));
    if !plat::write_file(&script, text.as_bytes()) {
        return Err(format!("{} cannot be written", script));
    }
    shell_args.push(script);
    let refs: Vec<&str> = shell_args.iter().map(String::as_str).collect();
    bb_exec::launch(&shell, &refs).map_err(|code| format!("{}: could not be started (system error {})", program, code))?;
    // its process id, as the script notes it before it becomes the program
    let mut pid = String::new();
    if !cfg!(windows) {
        let end = tick_ms() + 3000;
        while pid.is_empty() && tick_ms() < end {
            plat::sleep_ms(50);
            pid = plat::read_file(&pid_path).map(|b| String::from(String::from_utf8_lossy(&b).trim())).unwrap_or_default();
        }
    }
    // what it says at first: a program that could not start says it there
    plat::sleep_ms(1500);
    let mut head = [0u8; 2048];
    let first = plat::File::open(&log_path, plat::FO_READ).and_then(|f| f.read_at(0, &mut head)).map(|n| String::from_utf8_lossy(&head[..n]).into_owned()).unwrap_or_default();
    let mut log = LogBuf::new(jd, Emitter::new(w, &spec.main, &spec.id));
    let line = format!(
        "handed to the system to start and left running on {}{}: {}\nthe worker does not hold, watch or end it; whether it runs, and what it says, is in {} there{}\n",
        worker,
        if pid.is_empty() { String::new() } else { format!(" (process {})", pid) },
        spec.command_text(),
        log_path,
        if first.trim().is_empty() { String::new() } else { format!(" -- its first words:\n{}", first.trim_end()) }
    );
    log.push(false, line.as_bytes());
    log.finish();
    plat::log_info("job", &format!("{} handed over to run on its own{}: {}", spec.id, if pid.is_empty() { String::new() } else { format!(" (process {})", pid) }, spec.command_text()));
    done.exit = 0;
    done.state = String::from(proto::ST_DONE);
    Ok(())
}

fn collect(w: &Worker, relay: &Relay, tree_dir: &str, patterns: &[String]) -> Result<TreeRef, String> {
    let files = bb_sync::expand(tree_dir, patterns, OUTPUT_FILES_MAX)?;
    if files.is_empty() {
        return Ok(TreeRef::default());
    }
    let tree = bb_sync::scan_files(tree_dir, &files, &w.data)?;
    Ok(bb_sync::upload(relay, &w.data, tree_dir, &tree, &mut |_, _| {})?.0)
}

/// What the job folders say when a worker starts (the disk only, nothing is sent):
///   a job that waited its turn waits again -> the jobs to queue, the oldest first;
///   a job that ran when the worker stopped (restart, power cut) ends as an error, which
///     resend_pending tells its main machine;
///   folders of another pairing go.
pub fn recover(dir: &str, main_id: &str) -> Vec<JobSpec> {
    let mut queue: Vec<JobSpec> = Vec::new();
    let root = plat::fs::join(dir, "jobs");
    for e in plat::fs::read_dir(&root).unwrap_or_default() {
        let jd = plat::fs::join(&root, &e.name);
        let has = |name: &str| plat::file_exists(&plat::fs::join(&jd, name)) == 1;
        let spec = match j::load::<JobSpec>(&plat::fs::join(&jd, "spec.json")) {
            Some(s) if s.main == main_id && s.id == e.name => s,
            _ => {
                bb_core::util::remove_tree(&jd);
                continue;
            }
        };
        if has("done.json") {
            // ended: prune() takes it away when its week is over
        } else if has("started") {
            let d = Done { state: String::from(proto::ST_ERROR), exit: -1, error: String::from("the worker stopped while the job ran (restart, power cut); run it again"), time: now(), ..Done::default() };
            bb_core::util::put_file(&plat::fs::join(&jd, "done.json"), j::pretty(&d.to_json()).as_bytes());
        } else {
            queue.push(spec);
        }
    }
    queue.sort_by_key(|s| s.created);
    queue
}

/// The folders of jobs that ended more than a week ago go, their logs with them (now and then
/// while the worker runs: one that runs for months must not gather them for months).
pub fn prune(dir: &str) {
    let root = plat::fs::join(dir, "jobs");
    for e in plat::fs::read_dir(&root).unwrap_or_default() {
        let jd = plat::fs::join(&root, &e.name);
        if let Some(m) = plat::fs::stat(&plat::fs::join(&jd, "done.json"))
            && now() - m.mtime_ns.div_euclid(1_000_000_000) > 7 * 86_400
        {
            bb_core::util::remove_tree(&jd);
        }
    }
}

/// Send the ends of jobs their main machine has not heard of: `min_age` seconds after they
/// were recorded (0 at a start; later long enough that the job's own attempt is over).
pub fn resend_pending(w: &Worker, min_age: i64) {
    let root = plat::fs::join(&w.dir, "jobs");
    for e in plat::fs::read_dir(&root).unwrap_or_default() {
        let jd = plat::fs::join(&root, &e.name);
        let done_path = plat::fs::join(&jd, "done.json");
        if plat::file_exists(&plat::fs::join(&jd, "sent")) == 1 || plat::file_exists(&done_path) != 1 {
            continue;
        }
        let age = plat::fs::stat(&done_path).map_or(0, |m| now() - m.mtime_ns.div_euclid(1_000_000_000));
        if age < min_age || w.sched.lock().running.iter().any(|r| r.spec.id == e.name) {
            continue;
        }
        let (Some(spec), Some(done)) = (j::load::<JobSpec>(&plat::fs::join(&jd, "spec.json")), j::load::<Done>(&done_path)) else { continue };
        if w.stopping() || !Emitter::new(w, &spec.main, &spec.id).send(&Event::Done(done), 3) {
            // the relay is away: the next round tries again
            return;
        }
        bb_core::util::put_file(&plat::fs::join(&jd, "sent"), b"1");
        plat::log_info("job", &format!("{}: its end reached the main machine now", spec.id));
    }
}
