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

use bb_core::client::Relay;
use bb_core::crypt;
use bb_core::j::{self, Json};
use bb_core::proto::{self, Done, Event, JobSpec, TreeRef, Workspace};
use bb_core::util::{below, now, size_text, tick_ms};
use bb_sync::{Apply, Known};

use crate::config;
use crate::{Running, Worker};

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
}

impl Emitter {
    pub fn new(w: &Worker, main: &str, job: &str) -> Emitter {
        let channel = proto::job_channel(main, job);
        let id = w.cfg.lock().id.clone();
        Emitter { relay: w.relay.fork(), key: w.pair, aad: crypt::aad(&id, &channel), channel }
    }

    /// Send an event, trying again for up to `patience` seconds while the relay cannot be
    /// reached -> whether it arrived.
    pub fn send(&self, e: &Event, patience: u64) -> bool {
        let sealed = crypt::seal(&self.key, &self.aad, j::text(&e.to_json()).as_bytes());
        let end = tick_ms() + patience * 1000;
        loop {
            match self.relay.ch_append(&self.channel, &sealed) {
                Ok(_) => return true,
                // not reachable right now: wait; anything else (removed from the relay, channel full) will not get better
                Err(e) if e.status == 0 || e.status >= 500 && e.status != 507 => {
                    if tick_ms() >= end || plat::service::stop_requested() {
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

/// a job's output on its way: to the file at once, to the main machine in pieces
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
    lost: u64,
    last: u64,
}

impl LogBuf {
    fn new(job_dir: &str) -> LogBuf {
        LogBuf {
            file: plat::File::open(&plat::fs::join(job_dir, "out.log"), plat::FO_WRITE),
            file_len: 0,
            segs: Vec::new(),
            pending: 0,
            carry: [Vec::new(), Vec::new()],
            relayed: 0,
            cut: false,
            lost: 0,
            last: tick_ms(),
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

    /// send what has gathered when it is enough, old enough, or `force`
    fn flush(&mut self, em: &Emitter, force: bool) {
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
            if !em.send(&Event::Log { err, data: text.clone() }, 4) {
                self.lost += text.len() as u64;
            } else if self.lost > 0 {
                let note = format!("[build balancer] {} of output could not be sent while the relay was unreachable; all of it is in the worker's job folder\n", size_text(self.lost));
                em.send(&Event::Log { err: true, data: note }, 4);
                self.lost = 0;
            }
            if self.relayed > LOG_RELAY_MAX {
                self.cut = true;
                em.send(&Event::Log { err: true, data: format!("[build balancer] output cut here after {}; the rest is in the worker's job folder\n", size_text(LOG_RELAY_MAX)) }, 4);
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

/// tell the main machine that a job will not run
pub fn refuse(w: &Worker, spec: &JobSpec, state: &str, why: &str) {
    plat::log_info("job", &format!("{} refused: {}", spec.id, why));
    let d = Done { state: String::from(state), exit: -1, error: String::from(why), time: now(), ..Done::default() };
    Emitter::new(w, &spec.main, &spec.id).send(&Event::Done(d), 20);
}

/// the whole job; ends by giving its processors back and waking the scheduler
pub fn run(w: Arc<Worker>, r: Arc<Running>) {
    let spec = &r.spec;
    let em = Emitter::new(&w, &spec.main, &spec.id);
    let jd = config::job_dir(&w.dir, &spec.id);
    plat::mkdir(&jd);
    bb_core::util::put_file(&plat::fs::join(&jd, "spec.json"), j::pretty(&spec.to_json()).as_bytes());
    plat::log_info("job", &format!("{} starts: {} ({} processors)", spec.id, spec.command_text(), r.cpu));
    let t0 = tick_ms();
    let mut done = Done { state: String::from(proto::ST_ERROR), exit: -1, ..Done::default() };
    if let Err(e) = execute(&w, &r, &em, &jd, &mut done) {
        done.state = String::from(proto::ST_ERROR);
        done.error = e;
    }
    done.duration_ms = tick_ms() - t0;
    done.time = now();
    plat::log_info("job", &format!("{} ends: {} (exit {}){}", spec.id, done.state, done.exit, if done.error.is_empty() { String::new() } else { format!(" -- {}", done.error) }));
    bb_core::util::put_file(&plat::fs::join(&jd, "done.json"), j::pretty(&done.to_json()).as_bytes());
    em.send(&Event::Done(done), 300);
    // the workspace's size, for the quota and the status (the workspace stays held meanwhile)
    let wsd = config::ws_dir(&w.dir, &spec.project_id, &spec.slot);
    if plat::file_exists(&wsd) == 2 {
        config::save_workspace(&w.dir, &Workspace { project_id: spec.project_id.clone(), project: spec.project.clone(), slot: spec.slot.clone(), bytes: bb_core::util::tree_size(&wsd, u64::MAX), used: now() });
    }
    {
        let mut s = w.sched.lock();
        s.running.retain(|x| x.spec.id != spec.id);
        s.cores.give_back(r.mask);
        s.dirty = true;
    }
    w.wake.notify_all();
}

/// what the workspaces hold together, and the one of this job alone
fn disk_used(w: &Worker, spec: &JobSpec) -> (u64, u64) {
    let all = config::workspaces(&w.dir);
    let own = all.iter().find(|x| x.project_id == spec.project_id && x.slot == spec.slot).map_or(0, |x| x.bytes);
    (all.iter().map(|x| x.bytes).sum(), own)
}

/// Make room for `need` more bytes inside the lent disk space: workspaces nobody uses right
/// now go, the one unused the longest first -> the bytes that may be written.
fn make_room(w: &Worker, spec: &JobSpec, need: u64) -> u64 {
    let lent = w.cfg.lock().disk;
    loop {
        let all = config::workspaces(&w.dir);
        let used: u64 = all.iter().map(|x| x.bytes).sum();
        let left = lent.saturating_sub(used);
        if need <= left {
            return left;
        }
        let busy: Vec<(String, String)> = w.sched.lock().running.iter().map(|r| (r.spec.project_id.clone(), r.spec.slot.clone())).collect();
        let victim = all.iter().filter(|x| !busy.iter().any(|b| b.0 == x.project_id && b.1 == x.slot) && !(x.project_id == spec.project_id && x.slot == spec.slot)).min_by_key(|x| x.used);
        let Some(v) = victim else { return left };
        plat::log_info("disk", &format!("workspace {} / {} ({}) removed to make room", v.project, v.slot, size_text(v.bytes)));
        if !bb_core::util::remove_tree(&config::ws_dir(&w.dir, &v.project_id, &v.slot)) {
            return left;
        }
    }
}

fn execute(w: &Worker, r: &Running, em: &Emitter, jd: &str, done: &mut Done) -> Result<(), String> {
    let spec = &r.spec;
    let wsd = config::ws_dir(&w.dir, &spec.project_id, &spec.slot);
    let tree_dir = plat::fs::join(&wsd, "tree");
    let known_path = plat::fs::join(&wsd, "known");
    let relay = w.relay.fork();

    // the project's files
    if !spec.tree.is_empty() {
        set_state(w, r, em, proto::ST_SYNCING, format!("{} files, {}", spec.tree.files, size_text(spec.tree.bytes)));
        let tree = bb_sync::fetch_tree(&relay, &w.data, &spec.tree)?;
        let mut known = Known::load(&known_path);
        let need = bb_sync::need_bytes(&tree_dir, &tree, &known);
        let mut budget = make_room(w, spec, need);
        // never fill the volume to the brim, whatever is lent
        let free = plat::fs::free_space(&w.dir).map_or(u64::MAX, |f| f.0);
        budget = budget.min(free.saturating_sub(256 << 20));
        if need > budget {
            return Err(format!("the project needs {} more on this worker's disk, but only {} are free of the {} it lends (bb-worker config --disk)", size_text(need), size_text(budget), size_text(w.cfg.lock().disk)));
        }
        let st = bb_sync::apply(&relay, &w.data, &tree_dir, &tree, &mut known, &Apply { delete: true, compare: false, budget }, &mut |_, _| {});
        // also after a failure: what arrived is not fetched again
        known.save(&known_path);
        let st = st?;
        done.sync_files = st.written;
        done.sync_bytes = st.bytes;
        config::save_workspace(&w.dir, &Workspace { project_id: spec.project_id.clone(), project: spec.project.clone(), slot: spec.slot.clone(), bytes: disk_used(w, spec).1.max(tree.bytes()), used: now() });
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
    let (worker_name, lent_disk, low_priority) = {
        let c = w.cfg.lock();
        (c.name.clone(), c.disk, c.priority != "normal")
    };
    let mut env: Vec<(String, Option<String>)> = Vec::new();
    let cpus = format!("{}", r.cpu);
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
    let mut p = bb_exec::spawn(&bb_exec::Spec { program, args, cwd, env, affinity: r.mask, low_priority })?;

    let mut log = LogBuf::new(jd);
    let deadline = (spec.timeout > 0).then(|| tick_ms() + spec.timeout * 1000);
    let (used0, own0) = disk_used(w, spec);
    let free0 = plat::fs::free_space(&wsd).map_or(0, |f| f.0);
    let (mut next_mem, mut next_disk, mut next_exact) = (tick_ms() + 500, tick_ms() + 10_000, 0u64);
    let mut peak = 0u64;
    let mut stop: Option<(&str, String)> = None;
    let exit = loop {
        let any = p.pump(&mut |err, b| log.push(err, b));
        log.flush(em, false);
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
                stop = Some((proto::ST_KILLED, format!("time limit of {} reached", bb_core::util::dur_text(spec.timeout * 1000))));
            } else if plat::service::stop_requested() {
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
                        stop = Some((proto::ST_KILLED, format!("disk limit: the workspaces grew beyond the {} this worker lends (bb-worker config --disk)", size_text(lent_disk))));
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
    log.flush(em, true);
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

fn collect(w: &Worker, relay: &Relay, tree_dir: &str, patterns: &[String]) -> Result<TreeRef, String> {
    let files = bb_sync::expand(tree_dir, patterns, OUTPUT_FILES_MAX)?;
    if files.is_empty() {
        return Ok(TreeRef::default());
    }
    let tree = bb_sync::scan_files(tree_dir, &files, &w.data)?;
    Ok(bb_sync::upload(relay, &w.data, tree_dir, &tree, &mut |_, _| {})?.0)
}

/// Jobs that were running when the worker last stopped (power cut, kill): their main machine
/// still waits for an end.  And job folders older than a week go.
pub fn recover(w: &Worker) {
    let root = plat::fs::join(&w.dir, "jobs");
    for e in plat::fs::read_dir(&root).unwrap_or_default() {
        let jd = plat::fs::join(&root, &e.name);
        let done = plat::fs::join(&jd, "done.json");
        if plat::file_exists(&done) == 1 {
            let age = plat::fs::stat(&done).map_or(0, |m| now() - m.mtime_ns.div_euclid(1_000_000_000));
            if age > 7 * 86_400 {
                bb_core::util::remove_tree(&jd);
            }
            continue;
        }
        let Some(spec) = j::load::<JobSpec>(&plat::fs::join(&jd, "spec.json")) else {
            bb_core::util::remove_tree(&jd);
            continue;
        };
        let d = Done { state: String::from(proto::ST_ERROR), exit: -1, error: String::from("the worker stopped while the job ran (restart, power cut); run it again"), time: now(), ..Done::default() };
        bb_core::util::put_file(&done, j::pretty(&d.to_json()).as_bytes());
        Emitter::new(w, &spec.main, &spec.id).send(&Event::Done(d), 10);
    }
}
