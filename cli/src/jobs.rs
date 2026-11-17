// jobs -- a job from the main machine's side
//
//   submit      pick a worker, send what the project lacks at the relay, seal the job into the
//               worker's inbox, keep a record
//   pump        read the job's events from the relay into the record and the log file
//   follow      pump until the job ended (printing its output as it comes)
//   fetch       bring the job's outputs into a folder
//   cancel      ask the worker to stop it
//
// A record and a log file per job live in the config folder, so every later `bb` call --
// from another terminal, from an agent -- finds the job by its id.  Whoever reads a job's
// channel holds the job's lock file; others read what that one wrote.

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

use bb_core::crypt;
use bb_core::j::{self, Json, Value};
use bb_core::proto::{self, EnvVar, Event, JobBrief, JobSpec, TreeRef};
use bb_core::util::{now, size_text, tick_ms};
use bb_sync::{Apply, Known};

use crate::conf::{Ctx, JobRec, Project};
use crate::fleet::{self, Needs, WorkerView};

/// what to run and where
#[derive(Default, Clone, Debug)]
pub struct Request {
    /// a worker's name, or "" / "auto" to let the needs decide
    pub worker: String,
    pub needs: String,
    pub cpu: u32,
    pub ram: u64,
    pub timeout: u64,
    pub argv: Vec<String>,
    pub script: String,
    pub shell: String,
    pub cwd: String,
    pub env: Vec<EnvVar>,
    pub outputs: Vec<String>,
    pub slot: String,
    pub label: String,
    /// send the project's files first (false: run in the workspace as the worker has it)
    pub sync: bool,
    /// a light job: no processors held, it starts at once beside the others (JobSpec::light)
    pub light: bool,
    /// start the program and leave it running on the worker (JobSpec::leave_running)
    pub leave_running: bool,
}

/// the id of the workspace for commands outside a project
const ADHOC: &str = "p-adhoc";

/// Send a job.  `views` is the fleet as fleet() gave it; the chosen worker's entry is updated,
/// so several jobs submitted in a row spread over the workers.  `say` gets progress lines.
pub fn submit(ctx: &Ctx, project: Option<&Project>, req: &Request, views: &mut [WorkerView], say: &mut dyn FnMut(&str)) -> Result<JobRec, String> {
    let needs = Needs::parse(&req.needs)?;
    if !req.cwd.is_empty() && !bb_core::util::rel_ok(&req.cwd.replace('\\', "/")) {
        return Err(format!("--cwd \"{}\": a folder inside the project (a relative path without ..)", req.cwd));
    }
    let slot = if req.slot.is_empty() { String::from("main") } else { bb_core::util::slug(&req.slot) };
    if slot.is_empty() {
        return Err(String::from("the slot's name needs letters or digits"));
    }
    let light = req.light || req.leave_running;
    let (worker, key) = {
        let v = fleet::pick(views, &req.worker, &needs, req.cpu, req.ram)?;
        // what the worker said of itself last: a program is left running only where its owner
        // allows it, and only a worker from 0.1.4 on knows either kind of job
        if let Some(s) = &v.status {
            let old = bb_update::newer("0.1.4", &s.version);
            if req.leave_running && old {
                return Err(format!("{} runs version {}, which cannot leave a program running: bring it up to date first (bb update -w {})", v.paired.name, s.version, v.paired.name));
            }
            if req.leave_running && !s.leave_running {
                return Err(format!("{} lets no program outlive its job: on that machine, `bb-worker config --leave-running yes` (or the switch in the app) allows it", v.paired.name));
            }
            if light && old {
                say(&format!("note: {} runs version {}: it runs this light job as an ordinary one of one processor", v.paired.name, s.version));
            }
        }
        (v.paired.clone(), ctx.pair_key(&v.paired).ok_or("the worker's key is damaged; pair it again")?)
    };

    // the project's files
    let (project_id, project_name, project_dir, tree) = match (project, req.sync) {
        (Some(p), true) => {
            let known_path = bb_core::util::below(&ctx.dir, &format!("projects/{}/known", p.id));
            let mut known = Known::load(&known_path);
            let (tree, st) = bb_sync::scan(&p.dir, &p.sync, &ctx.data, &mut known)?;
            known.save(&known_path);
            if st.skipped > 0 {
                say(&format!("note: {} links or special files are not part of what is sent", st.skipped));
            }
            let mut last = 0u64;
            let (tref, up) = bb_sync::upload(&ctx.relay, &ctx.data, &p.dir, &tree, &mut |done, total| {
                if tick_ms() - last > 2000 {
                    last = tick_ms();
                    say(&format!("sending {} of {}", size_text(done), size_text(total)));
                }
            })?;
            say(&format!("project {}: {} files, {}; {} new for the relay", p.name, st.files, size_text(st.bytes), if up.chunks == 0 { String::from("nothing") } else { format!("{} ({} sent)", size_text(up.bytes), size_text(up.sent)) }));
            (p.id.clone(), p.name.clone(), p.dir.clone(), tref)
        }
        (Some(p), false) => (p.id.clone(), p.name.clone(), p.dir.clone(), TreeRef::default()),
        (None, false) => (String::from(ADHOC), String::from("(no project)"), plat::cwd(), TreeRef::default()),
        (None, true) => return Err(format!("this folder is no project yet: `bb init` writes its {} (or --no-sync runs a command without sending files)", crate::conf::PROJECT_FILE)),
    };

    let spec = JobSpec {
        protocol: bb_core::PROTOCOL,
        id: bb_core::util::new_id("j"),
        main: ctx.main.id.clone(),
        project_id,
        project: project_name,
        slot,
        tree,
        argv: req.argv.clone(),
        shell: req.shell.clone(),
        script: req.script.clone(),
        cwd: req.cwd.replace('\\', "/"),
        env: req.env.clone(),
        // (a light job holds none; a worker before 0.1.4 runs it as a job of one processor)
        cpu: if light && req.cpu == 0 { 1 } else { req.cpu },
        ram: req.ram,
        timeout: req.timeout,
        light,
        leave_running: req.leave_running,
        outputs: req.outputs.clone(),
        created: now(),
        label: req.label.clone(),
    };
    let rec = JobRec {
        id: spec.id.clone(),
        worker_id: worker.id.clone(),
        worker: worker.name.clone(),
        project_id: spec.project_id.clone(),
        project: spec.project.clone(),
        project_dir,
        slot: spec.slot.clone(),
        label: spec.label.clone(),
        command: spec.command_text(),
        outputs: spec.outputs.clone(),
        state: String::from(proto::ST_SUBMITTED),
        created: spec.created,
        exit: -1,
        ..JobRec::default()
    };
    // the record first: a job that runs on a worker while nothing here knows of it could be
    // neither followed nor cancelled
    if !ctx.save_job(&rec) {
        return Err(format!("{} cannot be written", ctx.job_path(&rec.id, "json")));
    }
    let inbox = proto::inbox(&worker.id);
    let order = proto::order(proto::ORDER_START, j::obj([("job", spec.to_json())]));
    let sealed = crypt::seal(&key, &crypt::aad(&ctx.main.id, &inbox), j::text(&order).as_bytes());
    if let Err(e) = ctx.relay.ch_append(&inbox, &sealed) {
        // never sent: no record of a job that does not exist
        plat::delete(&ctx.job_path(&rec.id, "json"));
        return Err(format!("{}", e));
    }
    // the next job of this batch sees this worker as busier
    if let Some(v) = views.iter_mut().find(|v| v.paired.id == worker.id)
        && let Some(s) = &mut v.status {
            let cpu = if spec.cpu == 0 { s.offer.cpu } else { spec.cpu.min(s.offer.cpu) };
            s.cpu_reserved = (s.cpu_reserved + cpu).min(s.offer.cpu);
            s.queued.push(JobBrief { id: spec.id.clone(), ..JobBrief::default() });
        }
    Ok(rec)
}

/// add output to the job's log file -> whether all of it is there now
fn append_log(ctx: &Ctx, rec: &mut JobRec, text: &str) -> bool {
    let Some(f) = plat::File::open(&ctx.job_path(&rec.id, "log"), plat::FO_RW) else { return false };
    // at the place the record knows: what a failed attempt left beyond it is written over
    let at = rec.log_bytes.min(f.size().unwrap_or(rec.log_bytes));
    if f.write_at(at, text.as_bytes()) != Some(text.len()) {
        return false;
    }
    rec.log_bytes = at + text.len() as u64;
    true
}

/// Read the job's new events (waiting up to `wait` seconds for the first) into the record and
/// the log file.  A job that ended is not asked about again.
pub fn pump(ctx: &Ctx, rec: &mut JobRec, wait: u32) -> Result<(), String> {
    if proto::is_final(&rec.state) {
        return Ok(());
    }
    let Some(_lock) = plat::FileLock::acquire(&ctx.job_path(&rec.id, "lock")) else {
        // another bb reads this job right now: take over what it wrote
        plat::sleep_ms(400);
        if let Some(r) = ctx.load_job(&rec.id) {
            *rec = r;
        }
        return Ok(());
    };
    if let Some(r) = ctx.load_job(&rec.id) {
        *rec = r;
        if proto::is_final(&rec.state) {
            return Ok(());
        }
    }
    let worker = ctx.workers.iter().find(|w| w.id == rec.worker_id).ok_or_else(|| format!("the job's worker {} is no longer paired", rec.worker))?;
    let key = ctx.pair_key(worker).ok_or("the worker's key is damaged")?;
    let channel = proto::job_channel(&ctx.main.id, &rec.id);
    let aad = crypt::aad(&worker.id, &channel);
    let msgs = ctx.relay.ch_read(&channel, rec.after, wait)?;
    if msgs.is_empty() {
        return Ok(());
    }
    // an event counts as read once what it brought is kept: output that could not be written
    // stays at the relay and is read again
    let mut log_failed = false;
    for (seq, sealed) in msgs {
        let Some(ev) = crypt::open(&key, &aad, &sealed).and_then(|p| j::parse(&p)).and_then(|v| Event::from_json(&v)) else {
            rec.after = seq;
            continue;
        };
        match ev {
            Event::State { state, detail, time } => {
                if state == proto::ST_RUNNING && rec.started == 0 {
                    rec.started = time;
                }
                rec.state = state;
                rec.detail = detail;
            }
            Event::Log { data, .. } => {
                if !append_log(ctx, rec, &data) {
                    log_failed = true;
                    break;
                }
            }
            Event::Done(d) => {
                rec.state = d.state;
                rec.detail = String::new();
                rec.exit = d.exit;
                rec.error = d.error;
                rec.duration_ms = d.duration_ms;
                rec.peak_ram = d.peak_ram;
                rec.sync_files = d.sync_files;
                rec.sync_bytes = d.sync_bytes;
                rec.artifacts = d.artifacts;
                rec.ended = d.time;
            }
        }
        rec.after = seq;
    }
    if !ctx.save_job(rec) || log_failed {
        // the relay keeps the events: they are read again once the record can be written
        return Err(format!("{} cannot be written (is the disk full?)", ctx.job_path(&rec.id, if log_failed { "log" } else { "json" })));
    }
    if proto::is_final(&rec.state) {
        // everything is kept here now: the relay can let the channel go
        let _ = ctx.relay.ch_delete(&channel);
    }
    Ok(())
}

/// the bytes of a job's log from `from` on (64 MiB at most)
pub fn log_from(ctx: &Ctx, id: &str, from: u64) -> Vec<u8> {
    log_part(ctx, id, from, 64 << 20)
}

/// at most `max` bytes of a job's log from `from` on: only they are read
pub fn log_part(ctx: &Ctx, id: &str, from: u64, max: usize) -> Vec<u8> {
    let mut out = Vec::new();
    let Some(f) = plat::File::open(&ctx.job_path(id, "log"), plat::FO_READ) else { return out };
    let mut buf = alloc::vec![0u8; max.clamp(1, 64 << 10)];
    while out.len() < max {
        let want = buf.len().min(max - out.len());
        match f.read_at(from + out.len() as u64, &mut buf[..want]) {
            Some(n) if n > 0 => out.extend_from_slice(&buf[..n]),
            _ => break,
        }
    }
    out
}

/// the last `lines` lines of a job's log
pub fn log_tail(ctx: &Ctx, rec: &JobRec, lines: usize) -> String {
    let size = plat::fs::stat(&ctx.job_path(&rec.id, "log")).map_or(0, |m| m.size);
    let b = log_from(ctx, &rec.id, size.saturating_sub(256 << 10));
    let text = String::from_utf8_lossy(&b);
    let all: Vec<&str> = text.lines().collect();
    all[all.len().saturating_sub(lines)..].join("\n")
}

/// Pump until the job ended or `deadline` (tick_ms) passed -> whether it ended.  `on_log` gets
/// the output as it arrives, `on_state` every change of state.
pub fn follow(ctx: &Ctx, rec: &mut JobRec, deadline: Option<u64>, on_log: &mut dyn FnMut(&[u8]), on_state: &mut dyn FnMut(&JobRec)) -> Result<bool, String> {
    let mut shown = 0u64;
    let mut state = (String::new(), String::new());
    let mut trouble_since: Option<u64> = None;
    loop {
        let wait = match deadline {
            Some(d) => (d.saturating_sub(tick_ms()) / 1000).min(bb_core::POLL_WAIT as u64) as u32,
            None => bb_core::POLL_WAIT,
        };
        match pump(ctx, rec, wait) {
            Ok(()) => trouble_since = None,
            Err(e) => {
                // the relay may be away for a moment; a job does not depend on this terminal
                let since = *trouble_since.get_or_insert_with(tick_ms);
                if tick_ms() - since > 600_000 {
                    return Err(e);
                }
                if tick_ms() - since < 3000 {
                    bb_core::util::err(&format!("{}; trying again\n", e));
                }
                plat::sleep_ms(3000);
            }
        }
        let new = log_from(ctx, &rec.id, shown);
        if !new.is_empty() {
            shown += new.len() as u64;
            on_log(&new);
        }
        if state.0 != rec.state || state.1 != rec.detail {
            state = (rec.state.clone(), rec.detail.clone());
            on_state(rec);
        }
        if proto::is_final(&rec.state) {
            return Ok(true);
        }
        if deadline.is_some_and(|d| tick_ms() >= d) {
            return Ok(false);
        }
    }
}

/// Bring a job's outputs into `to` (None: the project's folder, where their paths belong)
/// -> their paths.
pub fn fetch(ctx: &Ctx, rec: &mut JobRec, to: Option<&str>) -> Result<Vec<String>, String> {
    if !proto::is_final(&rec.state) {
        return Err(format!("job {} is still {}", rec.id, rec.state));
    }
    if rec.artifacts.is_empty() {
        return Ok(Vec::new());
    }
    let root = to.map(bb_core::util::abs_path).unwrap_or_else(|| rec.project_dir.clone());
    let tree = bb_sync::fetch_tree(&ctx.relay, &ctx.data, &rec.artifacts)?;
    bb_sync::apply(&ctx.relay, &ctx.data, &root, &tree, &mut Known::default(), &Apply { delete: false, compare: true, budget: u64::MAX }, &mut |_, _| {})?;
    rec.fetched_to = root;
    rec.fetched_files = tree.files.len() as u64;
    ctx.save_job(rec);
    Ok(tree.files.iter().map(|f| f.path.clone()).collect())
}

/// ask the job's worker to stop it
pub fn cancel(ctx: &Ctx, rec: &JobRec) -> Result<(), String> {
    let worker = ctx.workers.iter().find(|w| w.id == rec.worker_id).ok_or_else(|| format!("the job's worker {} is no longer paired", rec.worker))?;
    let key = ctx.pair_key(worker).ok_or("the worker's key is damaged")?;
    let inbox = proto::inbox(&worker.id);
    let order = proto::order(proto::ORDER_CANCEL, j::obj([("id", j::s(&rec.id))]));
    ctx.relay.ch_append(&inbox, &crypt::seal(&key, &crypt::aad(&ctx.main.id, &inbox), j::text(&order).as_bytes()))?;
    Ok(())
}

/// the exit code a finished job gives `bb`: the command's own when it ran to its end
pub fn exit_code(rec: &JobRec) -> i32 {
    match rec.state.as_str() {
        proto::ST_DONE => 0,
        proto::ST_FAILED => (rec.exit.clamp(1, 255)) as i32,
        proto::ST_CANCELLED => 130,
        proto::ST_KILLED => 137,
        proto::ST_ERROR => 1,
        _ => 124, // not ended within the time waited
    }
}

/// a job as --json and the MCP tools show it; `tail`: lines of output to include
pub fn job_json(ctx: &Ctx, rec: &JobRec, tail: usize) -> Value {
    let fin = proto::is_final(&rec.state);
    let mut v = j::obj([
        ("job", j::s(&rec.id)),
        ("state", j::s(&rec.state)),
        ("finished", Value::Bool(fin)),
        ("ok", Value::Bool(rec.state == proto::ST_DONE)),
        ("worker", j::s(&rec.worker)),
        ("project", j::s(&rec.project)),
        ("slot", j::s(&rec.slot)),
        ("command", j::s(&rec.command)),
        ("created", Value::i64(rec.created)),
    ]);
    if !rec.label.is_empty() {
        v.insert("task", j::s(&rec.label));
    }
    if !rec.detail.is_empty() {
        v.insert("detail", j::s(&rec.detail));
    }
    if fin {
        v.insert("exit_code", Value::i64(rec.exit));
        v.insert("duration_ms", j::n(rec.duration_ms));
        if !rec.error.is_empty() {
            v.insert("error", j::s(&rec.error));
        }
        if rec.peak_ram > 0 {
            v.insert("peak_ram", j::n(rec.peak_ram));
        }
        v.insert("synced", j::obj([("files", j::n(rec.sync_files)), ("bytes", j::n(rec.sync_bytes))]));
        v.insert(
            "outputs",
            j::obj([("patterns", j::strs(&rec.outputs)), ("files", j::n(rec.artifacts.files)), ("bytes", j::n(rec.artifacts.bytes)), ("fetched_to", if rec.fetched_to.is_empty() { Value::Null } else { j::s(&rec.fetched_to) })]),
        );
    }
    v.insert("log_bytes", j::n(rec.log_bytes));
    v.insert("log_file", j::s(&ctx.job_path(&rec.id, "log")));
    if tail > 0 {
        v.insert("log_tail", j::s(&log_tail(ctx, rec, tail)));
    }
    v
}

/// one line about a job for people
pub fn job_line(rec: &JobRec) -> String {
    let what = if rec.label.is_empty() { rec.command.clone() } else { format!("{} ({})", rec.label, rec.command) };
    let what: String = what.chars().take(60).collect();
    let end = if proto::is_final(&rec.state) {
        format!("exit {} in {}", rec.exit, bb_core::util::dur_text(rec.duration_ms))
    } else if rec.detail.is_empty() {
        String::new()
    } else {
        rec.detail.chars().take(40).collect()
    };
    format!("{:<13} {:<10} {:<14} {:<8} {}  {}", rec.id, rec.state, rec.worker, bb_core::util::ago_text(rec.created), what, end)
}
