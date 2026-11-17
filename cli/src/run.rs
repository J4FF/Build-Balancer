// run -- the commands around jobs: run, task, sync, jobs, status, logs, wait, cancel, fetch,
// forget, clean

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

use bb_core::j::{self, Value};
use bb_core::proto;
use bb_core::util::{out, size_text, tick_ms, Cli};

use crate::conf::{self, Ctx, JobRec, Project, Task};
use crate::jobs::{self, Request};
use crate::{ctx, fleet, parse, say, CmdResult, Reply};

const RUN_VALUED: &[&str] = &["--worker", "--needs", "--cpu", "--ram", "--timeout", "--out", "--env", "--slot", "--cwd", "--sh", "--shell", "--label", "--to"];
const RUN_FLAGS: &[&str] = &["--detach", "--no-sync", "--no-fetch", "--quiet", "--all", "--light", "--leave-running"];
const RUN_SHORT: &[(&str, &str)] = &[("-w", "--worker"), ("-d", "--detach"), ("-q", "--quiet"), ("-o", "--out")];

/// the options of `bb run` that `bb task` and `bb sync` share, laid over a request
fn overrides(cli: &Cli, r: &mut Request) -> Result<(), String> {
    if let Some(w) = cli.value("--worker") {
        r.worker = String::from(w);
    }
    if let Some(n) = cli.value("--needs") {
        r.needs = String::from(n);
    }
    if let Some(v) = cli.value("--cpu") {
        r.cpu = v.parse().map_err(|_| "--cpu takes a number of processors")?;
    }
    if let Some(v) = cli.value("--ram") {
        r.ram = bb_core::util::size_parse(v).ok_or("--ram takes a size like 8G")?;
    }
    if let Some(v) = cli.value("--timeout") {
        r.timeout = bb_core::util::dur_parse(v).ok_or("--timeout takes a duration like 90s, 30m or 2h")?;
    }
    if let Some(v) = cli.value("--slot") {
        r.slot = String::from(v);
    }
    if let Some(v) = cli.value("--cwd") {
        r.cwd = String::from(v);
    }
    if let Some(v) = cli.value("--label") {
        r.label = String::from(v);
    }
    r.outputs.extend(cli.values("--out"));
    r.env.extend(conf::parse_env(&cli.values("--env"))?);
    if cli.flag("--no-sync") {
        r.sync = false;
    }
    if cli.flag("--light") {
        r.light = true;
    }
    if cli.flag("--leave-running") {
        r.leave_running = true;
    }
    Ok(())
}

fn task_request(t: &Task) -> Request {
    Request {
        worker: t.worker.clone(),
        needs: t.needs.clone(),
        cpu: t.cpu,
        ram: t.ram,
        timeout: t.timeout,
        argv: t.argv.clone(),
        script: t.script.clone(),
        shell: if t.script.is_empty() { String::new() } else if t.shell.is_empty() { String::from("default") } else { t.shell.clone() },
        cwd: t.cwd.clone(),
        env: t.env.clone(),
        outputs: t.outputs.clone(),
        slot: t.slot.clone(),
        label: t.name.clone(),
        sync: true,
        ..Request::default()
    }
}

/// what a finished (or detached) job looks like to people
fn summary(rec: &JobRec, files: &[String]) -> String {
    let mut s = format!("job {} on {}: {}", rec.id, rec.worker, rec.state);
    if proto::is_final(&rec.state) {
        s += &format!(" (exit {}, {}", rec.exit, bb_core::util::dur_text(rec.duration_ms));
        if rec.peak_ram > 0 {
            s += &format!(", peak memory {}", size_text(rec.peak_ram));
        }
        s += ")";
        if !rec.error.is_empty() {
            s += &format!("\n  {}", rec.error);
        }
        if !rec.outputs.is_empty() {
            s += &if rec.artifacts.files == 0 {
                String::from("\n  outputs: no file matched the patterns")
            } else if rec.fetched_to.is_empty() {
                format!("\n  outputs: {} files, {} at the relay (bb fetch {})", rec.artifacts.files, size_text(rec.artifacts.bytes), rec.id)
            } else {
                let shown: Vec<&str> = files.iter().take(8).map(String::as_str).collect();
                format!("\n  outputs: {} files, {} -> {}\n    {}{}", rec.artifacts.files, size_text(rec.artifacts.bytes), rec.fetched_to, shown.join("\n    "), if files.len() > 8 { "\n    ..." } else { "" })
            };
        }
    }
    s
}

/// Follow jobs to their end, printing their output (with the job's name in front when there
/// are several), then fetch their outputs -> the reply for them.
fn finish(ctx: &Ctx, mut recs: Vec<JobRec>, cli: &Cli, json: bool, deadline: Option<u64>) -> CmdResult {
    let quiet = json || cli.flag("--quiet");
    let many = recs.len() > 1;
    let mut shown: Vec<u64> = alloc::vec![0; recs.len()];
    let mut states: Vec<String> = alloc::vec![String::new(); recs.len()];
    let mut partial: Vec<String> = alloc::vec![String::new(); recs.len()];
    loop {
        let open = recs.iter().filter(|r| !proto::is_final(&r.state)).count();
        for (i, rec) in recs.iter_mut().enumerate() {
            let was_final = proto::is_final(&rec.state);
            // one job: wait at the relay; several: ask each briefly in turn
            let wait = if was_final { 0 } else if open <= 1 { bb_core::POLL_WAIT.min(deadline.map_or(u32::MAX, |d| (d.saturating_sub(tick_ms()) / 1000) as u32)) } else { 1 };
            if !was_final
                && let Err(e) = jobs::pump(ctx, rec, wait) {
                    if !quiet {
                        say(&format!("{}; trying again", e));
                    }
                    plat::sleep_ms(3000);
                }
            if !json {
                let new = jobs::log_from(ctx, &rec.id, shown[i]);
                shown[i] += new.len() as u64;
                if !new.is_empty() {
                    if many {
                        // whole lines only, each with its job in front
                        let name = if rec.label.is_empty() { rec.worker.clone() } else { rec.label.clone() };
                        partial[i] += &String::from_utf8_lossy(&new);
                        while let Some(p) = partial[i].find('\n') {
                            let line: String = partial[i].drain(..=p).collect();
                            out(&format!("[{}] {}", name, line));
                        }
                    } else {
                        plat::write_stdout(&new);
                    }
                }
            }
            let now_state = format!("{} {}", rec.state, rec.detail);
            if states[i] != now_state {
                states[i] = now_state;
                if !quiet && !proto::is_final(&rec.state) {
                    say(&format!("{}{} on {}: {}{}", if many { format!("[{}] ", rec.label) } else { String::new() }, rec.id, rec.worker, rec.state, if rec.detail.is_empty() { String::new() } else { format!(" ({})", rec.detail) }));
                }
            }
        }
        if recs.iter().all(|r| proto::is_final(&r.state)) || deadline.is_some_and(|d| tick_ms() >= d) {
            break;
        }
    }
    for (i, rec) in recs.iter().enumerate() {
        if !json && !partial[i].is_empty() {
            out(&format!("[{}] {}\n", if rec.label.is_empty() { &rec.worker } else { &rec.label }, partial[i]));
        }
    }
    let mut text = String::new();
    let mut docs = Vec::new();
    let mut code = 0;
    for rec in recs.iter_mut() {
        let mut files = Vec::new();
        // what this command says, else what the job was sent with (bb run -d --no-fetch / --to)
        let no_fetch = cli.flag("--no-fetch") || rec.no_fetch;
        let to = cli.value("--to").map(String::from).or_else(|| (!rec.fetch_to.is_empty()).then(|| rec.fetch_to.clone()));
        if proto::is_final(&rec.state) && !rec.artifacts.is_empty() && !no_fetch {
            match jobs::fetch(ctx, rec, to.as_deref()) {
                Ok(f) => files = f,
                Err(e) => {
                    rec.error = if rec.error.is_empty() { format!("the outputs could not be fetched: {}", e) } else { format!("{}; the outputs could not be fetched: {}", rec.error, e) };
                    code = 1;
                }
            }
        }
        text += &format!("{}\n", summary(rec, &files));
        let mut doc = jobs::job_json(ctx, rec, 40);
        if !files.is_empty() {
            doc.insert("output_files", j::strs(&files[..files.len().min(500)]));
        }
        docs.push(doc);
        if code == 0 {
            code = jobs::exit_code(rec);
        }
    }
    let json_doc = if many { j::obj([("ok", Value::Bool(code == 0)), ("jobs", Value::Array(docs))]) } else { docs.into_iter().next().unwrap_or(Value::Null) };
    // the summary goes to standard error: standard output stays the command's own output
    // (--quiet keeps it for jobs that went wrong: an error must not pass silently)
    if !json && (!cli.flag("--quiet") || code != 0) {
        bb_core::util::err(&format!("bb: {}", text.replace('\n', "\nbb: ").trim_end_matches("bb: ")));
    }
    Ok(Reply { code, text: String::new(), json: json_doc })
}

fn detached(ctx: &Ctx, recs: &[JobRec]) -> Reply {
    let text: String = recs.iter().map(|r| format!("job {} sent to {}  (bb status {} / bb logs -f {} / bb wait {})\n", r.id, r.worker, r.id, r.id, r.id)).collect();
    let docs: Vec<Value> = recs.iter().map(|r| jobs::job_json(ctx, r, 0)).collect();
    let json = if docs.len() == 1 { docs.into_iter().next().unwrap_or(Value::Null) } else { j::obj([("ok", Value::Bool(true)), ("jobs", Value::Array(docs))]) };
    Reply::ok(text, json)
}

/// Keep in a job's record what `bb run` / `bb task` said of its outputs, so that `bb wait`
/// and the MCP tools do the same later: --no-fetch, --to DIR (as an absolute path: the later
/// command may run elsewhere).
fn remember_fetch(ctx: &Ctx, rec: &mut JobRec, cli: &Cli) {
    let (no_fetch, to) = (cli.flag("--no-fetch"), cli.value("--to"));
    if !no_fetch && to.is_none() {
        return;
    }
    rec.no_fetch = no_fetch;
    rec.fetch_to = to.map(bb_core::util::abs_path).unwrap_or_default();
    ctx.save_job(rec);
}

/// Whether `bb run`'s own options ask for JSON.  What follows the first word of the command
/// belongs to the command: `bb run cargo metadata --json` is not a request to bb.
pub fn run_wants_json(args: &[String]) -> bool {
    parse(args, RUN_VALUED, RUN_FLAGS, RUN_SHORT, true).is_ok_and(|c| c.flag("--json"))
}

pub fn run(args: &[String], json: bool) -> CmdResult {
    let cli = parse(args, RUN_VALUED, RUN_FLAGS, RUN_SHORT, true)?;
    let ctx = ctx(&cli)?;
    let mut req = Request { sync: true, ..Request::default() };
    overrides(&cli, &mut req)?;
    match cli.value("--sh") {
        Some(script) => {
            if !cli.rest.is_empty() {
                return Err(String::from("either --sh \"SCRIPT\" or a COMMAND, not both"));
            }
            req.script = String::from(script);
            req.shell = String::from(cli.value("--shell").unwrap_or("default"));
        }
        None => {
            if cli.rest.is_empty() {
                return Err(String::from("bb run [options] COMMAND ARGS..  (or --sh \"SCRIPT\"; bb sync sends the project without a command)"));
            }
            req.argv = cli.rest.clone();
        }
    }
    let project = Project::find(&plat::cwd());
    // inside a project, a command runs where it was typed: the same folder below the project's root
    if req.cwd.is_empty()
        && let Some(p) = &project {
            let here = plat::cwd();
            if here.len() > p.dir.len() && here.starts_with(&p.dir) {
                req.cwd = here[p.dir.len()..].trim_matches(['/', '\\']).replace('\\', "/");
            }
        }
    let quiet = json || cli.flag("--quiet");
    let mut views = fleet::fleet(&ctx)?;
    let mut rec = jobs::submit(&ctx, project.as_ref(), &req, &mut views, &mut |m| {
        if !quiet {
            say(m)
        }
    })?;
    remember_fetch(&ctx, &mut rec, &cli);
    if cli.flag("--detach") {
        return Ok(detached(&ctx, &[rec]));
    }
    finish(&ctx, alloc::vec![rec], &cli, json, None)
}

pub fn task(args: &[String], json: bool) -> CmdResult {
    let cli = parse(args, RUN_VALUED, RUN_FLAGS, RUN_SHORT, false)?;
    let ctx = ctx(&cli)?;
    let project = Project::find(&plat::cwd()).ok_or_else(|| format!("no {} here or above: `bb init` makes this folder a project", conf::PROJECT_FILE))?;
    let tasks: Vec<&Task> = if cli.flag("--all") { project.tasks.iter().collect() } else { cli.pos.iter().map(|n| project.task(n)).collect::<Result<_, _>>()? };
    if tasks.is_empty() {
        return Err(format!("bb task NAME.. (or --all); {}", if project.tasks.is_empty() { format!("{} defines none yet", conf::PROJECT_FILE) } else { format!("there are: {}", project.tasks.iter().map(|t| t.name.as_str()).collect::<Vec<_>>().join(", ")) }));
    }
    let quiet = json || cli.flag("--quiet");
    let mut views = fleet::fleet(&ctx)?;
    let mut recs = Vec::new();
    for t in tasks {
        let mut req = task_request(t);
        overrides(&cli, &mut req)?;
        let rec = jobs::submit(&ctx, Some(&project), &req, &mut views, &mut |m| {
            if !quiet {
                say(m)
            }
        })
        .map_err(|e| format!("task {}: {}{}", t.name, e, if recs.is_empty() { String::new() } else { format!(" (already sent: {})", recs.iter().map(|r: &JobRec| r.id.as_str()).collect::<Vec<_>>().join(", ")) }))?;
        if !quiet {
            say(&format!("task {} -> {} as {}", t.name, rec.worker, rec.id));
        }
        let mut rec = rec;
        remember_fetch(&ctx, &mut rec, &cli);
        recs.push(rec);
    }
    if cli.flag("--detach") {
        return Ok(detached(&ctx, &recs));
    }
    finish(&ctx, recs, &cli, json, None)
}

pub fn tasks(args: &[String]) -> CmdResult {
    let _cli = parse(args, &[], &[], &[], false)?;
    let project = Project::find(&plat::cwd()).ok_or_else(|| format!("no {} here or above: `bb init` makes this folder a project", conf::PROJECT_FILE))?;
    let mut text = format!("project \"{}\" ({}) in {}\n", project.name, project.id, project.dir);
    let mut list = Vec::new();
    for t in &project.tasks {
        let cmd = if t.script.is_empty() { t.argv.join(" ") } else { format!("sh: {}", t.script) };
        text += &format!("  {:<18} {}{}{}\n", t.name, cmd, if t.needs.is_empty() { String::new() } else { format!("   [needs {}]", t.needs) }, if t.outputs.is_empty() { String::new() } else { format!("   [outputs {}]", t.outputs.join(" ")) });
        list.push(j::obj([("name", j::s(&t.name)), ("command", j::s(&cmd)), ("needs", j::s(&t.needs)), ("worker", j::s(&t.worker)), ("cpu", j::n(t.cpu as u64)), ("ram", j::n(t.ram)), ("timeout_seconds", j::n(t.timeout)), ("outputs", j::strs(&t.outputs))]));
    }
    if project.tasks.is_empty() {
        text += &format!("  (no tasks yet: add [task.NAME] sections to {})\n", conf::PROJECT_FILE);
    }
    Ok(Reply::ok(text, j::obj([("project", j::s(&project.name)), ("id", j::s(&project.id)), ("dir", j::s(&project.dir)), ("tasks", Value::Array(list))])))
}

pub fn sync(args: &[String], json: bool) -> CmdResult {
    let cli = parse(args, &["--worker", "--slot"], &["--all", "--detach", "--quiet"], RUN_SHORT, false)?;
    let ctx = ctx(&cli)?;
    let project = Project::find(&plat::cwd()).ok_or_else(|| format!("no {} here or above: `bb init` makes this folder a project", conf::PROJECT_FILE))?;
    let mut views = fleet::fleet(&ctx)?;
    let names: Vec<String> = if cli.flag("--all") { views.iter().filter(|v| v.online).map(|v| v.paired.name.clone()).collect() } else { alloc::vec![String::from(cli.value("--worker").unwrap_or("auto"))] };
    if names.is_empty() {
        return Err(String::from("no worker is online"));
    }
    let quiet = json || cli.flag("--quiet");
    let mut recs = Vec::new();
    for n in names {
        let req = Request { worker: n, slot: String::from(cli.value("--slot").unwrap_or("")), label: String::from("sync"), sync: true, ..Request::default() };
        recs.push(jobs::submit(&ctx, Some(&project), &req, &mut views, &mut |m| {
            if !quiet {
                say(m)
            }
        })?);
    }
    if cli.flag("--detach") {
        return Ok(detached(&ctx, &recs));
    }
    finish(&ctx, recs, &cli, json, None)
}

pub fn jobs(args: &[String]) -> CmdResult {
    let cli = parse(args, &[], &["--all"], &[], false)?;
    let ctx = ctx(&cli)?;
    let mut all = ctx.jobs();
    if !cli.flag("--all") {
        all.truncate(20);
    }
    // what is still open may have moved on
    for r in all.iter_mut().filter(|r| !proto::is_final(&r.state)) {
        let _ = jobs::pump(&ctx, r, 0);
    }
    let mut text = if all.is_empty() { String::from("No jobs yet.  `bb run -- COMMAND` starts one.\n") } else { format!("{:<13} {:<10} {:<14} {:<8} {}\n", "JOB", "STATE", "WORKER", "AGE", "COMMAND") };
    for r in &all {
        text += &format!("{}\n", jobs::job_line(r));
    }
    Ok(Reply::ok(text, j::obj([("jobs", Value::Array(all.iter().map(|r| jobs::job_json(&ctx, r, 0)).collect()))])))
}

fn one_job(cli: &Cli, ctx: &Ctx, what: &str) -> Result<JobRec, String> {
    ctx.find_job(cli.pos.first().ok_or_else(|| format!("bb {} JOB  (an id, the start of one, or \"last\")", what))?)
}

pub fn status(args: &[String]) -> CmdResult {
    let cli = parse(args, &[], &[], &[], false)?;
    let ctx = ctx(&cli)?;
    let mut rec = one_job(&cli, &ctx, "status")?;
    jobs::pump(&ctx, &mut rec, 0)?;
    let mut text = format!("{}\n  command  {}\n  project  {} (slot {})\n", summary(&rec, &[]), rec.command, rec.project, rec.slot);
    if !proto::is_final(&rec.state) && !rec.detail.is_empty() {
        text += &format!("  detail   {}\n", rec.detail);
    }
    let tail = jobs::log_tail(&ctx, &rec, 10);
    if !tail.is_empty() {
        text += &format!("  last output:\n    {}\n", tail.replace('\n', "\n    "));
    }
    Ok(Reply::ok(text, jobs::job_json(&ctx, &rec, 40)))
}

pub fn logs(args: &[String], json: bool) -> CmdResult {
    let cli = parse(args, &["--tail"], &["--follow"], &[("-f", "--follow"), ("-n", "--tail")], false)?;
    let ctx = ctx(&cli)?;
    let mut rec = one_job(&cli, &ctx, "logs")?;
    jobs::pump(&ctx, &mut rec, 0)?;
    let tail = cli.value("--tail").and_then(|t| t.parse::<usize>().ok());
    if json {
        let text = match tail {
            Some(n) => jobs::log_tail(&ctx, &rec, n),
            None => String::from_utf8_lossy(&jobs::log_from(&ctx, &rec.id, 0)).into_owned(),
        };
        return Ok(Reply::ok("", j::obj([("job", j::s(&rec.id)), ("state", j::s(&rec.state)), ("finished", Value::Bool(proto::is_final(&rec.state))), ("log", j::s(&text))])));
    }
    let mut shown = match tail {
        Some(n) => {
            out(&format!("{}\n", jobs::log_tail(&ctx, &rec, n)));
            rec.log_bytes
        }
        None => 0,
    };
    if cli.flag("--follow") && !proto::is_final(&rec.state) {
        // follow prints from the start of the log: skip what the tail already showed
        let skip = shown;
        shown = 0;
        jobs::follow(&ctx, &mut rec, None, &mut |b| {
            let from = skip.saturating_sub(shown).min(b.len() as u64) as usize;
            shown += b.len() as u64;
            plat::write_stdout(&b[from..]);
        }, &mut |_| {})?;
        bb_core::util::err(&format!("bb: {}\n", summary(&rec, &[])));
        return Ok(Reply { code: jobs::exit_code(&rec), text: String::new(), json: Value::Null });
    }
    if tail.is_none() {
        plat::write_stdout(&jobs::log_from(&ctx, &rec.id, 0));
    }
    Ok(Reply::ok("", Value::Null))
}

pub fn wait(args: &[String], json: bool) -> CmdResult {
    let cli = parse(args, &["--timeout", "--to"], &["--no-fetch", "--quiet"], RUN_SHORT, false)?;
    let ctx = ctx(&cli)?;
    if cli.pos.is_empty() {
        return Err(String::from("bb wait JOB..  (ids, starts of ids, or \"last\")"));
    }
    let recs: Vec<JobRec> = cli.pos.iter().map(|n| ctx.find_job(n)).collect::<Result<_, _>>()?;
    let deadline = match cli.value("--timeout") {
        Some(v) => Some(tick_ms() + bb_core::util::dur_parse(v).ok_or("--timeout takes a duration like 90s, 30m or 2h")? * 1000),
        None => None,
    };
    // waiting shows no output of its own: the logs are one command away
    let quiet = Cli { flags: cli.flags.iter().cloned().chain([String::from("--quiet")]).collect(), pos: Vec::new(), opts: cli.opts.clone(), rest: Vec::new() };
    let mut rep = finish(&ctx, recs, &quiet, true, deadline)?;
    if !json {
        let docs: Vec<Value> = match rep.json.get("jobs").and_then(Value::as_array) {
            Some(a) => a.to_vec(),
            None => alloc::vec![rep.json.clone()],
        };
        rep.text = docs.iter().map(|d| format!("job {} on {}: {}{}\n", j::str_of(d, "job"), j::str_of(d, "worker"), j::str_of(d, "state"), d.get("exit_code").and_then(Value::as_i64).map_or(String::new(), |c| format!(" (exit {})", c)))).collect();
    }
    Ok(rep)
}

pub fn cancel(args: &[String]) -> CmdResult {
    let cli = parse(args, &[], &[], &[], false)?;
    let ctx = ctx(&cli)?;
    let mut rec = one_job(&cli, &ctx, "cancel")?;
    jobs::pump(&ctx, &mut rec, 0)?;
    if !proto::is_final(&rec.state) {
        jobs::cancel(&ctx, &rec)?;
        // the worker confirms within moments when it is online
        jobs::follow(&ctx, &mut rec, Some(tick_ms() + 20_000), &mut |_| {}, &mut |_| {})?;
    }
    let text = if proto::is_final(&rec.state) { format!("job {}: {}\n", rec.id, rec.state) } else { format!("job {}: cancel sent; the worker has not confirmed yet (it is {})\n", rec.id, rec.state) };
    Ok(Reply::ok(text, jobs::job_json(&ctx, &rec, 0)))
}

pub fn fetch(args: &[String]) -> CmdResult {
    let cli = parse(args, &["--to"], &[], &[], false)?;
    let ctx = ctx(&cli)?;
    let mut rec = one_job(&cli, &ctx, "fetch")?;
    jobs::pump(&ctx, &mut rec, 0)?;
    let files = jobs::fetch(&ctx, &mut rec, cli.value("--to"))?;
    let mut doc = jobs::job_json(&ctx, &rec, 0);
    doc.insert("output_files", j::strs(&files[..files.len().min(500)]));
    let text = if files.is_empty() { format!("job {} has no outputs{}\n", rec.id, if rec.outputs.is_empty() { " (it named no --out patterns)" } else { " (no file matched its patterns)" }) } else { format!("{} files, {} -> {}\n  {}\n", files.len(), size_text(rec.artifacts.bytes), rec.fetched_to, files.iter().take(40).map(String::as_str).collect::<Vec<_>>().join("\n  ")) };
    Ok(Reply::ok(text, doc))
}

pub fn forget(args: &[String]) -> CmdResult {
    let cli = parse(args, &[], &["--done"], &[], false)?;
    let ctx = ctx(&cli)?;
    let recs: Vec<JobRec> = if cli.flag("--done") { ctx.jobs().into_iter().filter(|r| proto::is_final(&r.state)).collect() } else { alloc::vec![one_job(&cli, &ctx, "forget")?] };
    for r in &recs {
        if !proto::is_final(&r.state) {
            let _ = ctx.relay.ch_delete(&proto::job_channel(&ctx.main.id, &r.id));
        }
        for ext in ["json", "log", "lock"] {
            plat::delete(&ctx.job_path(&r.id, ext));
        }
    }
    Ok(Reply::ok(format!("{} job record(s) dropped\n", recs.len()), j::obj([("ok", Value::Bool(true)), ("dropped", j::n(recs.len() as u64))])))
}

pub fn clean(args: &[String]) -> CmdResult {
    let cli = parse(args, &["--worker", "--slot"], &["--all"], RUN_SHORT, false)?;
    let ctx = ctx(&cli)?;
    let project = Project::find(&plat::cwd()).ok_or_else(|| format!("no {} here or above", conf::PROJECT_FILE))?;
    let workers: Vec<&conf::PairedWorker> = match (cli.value("--worker"), cli.flag("--all")) {
        (Some(w), _) => alloc::vec![ctx.worker(w)?],
        (None, true) => ctx.workers.iter().collect(),
        (None, false) => return Err(String::from("bb clean -w WORKER [--slot NAME]   or   bb clean --all   (every worker)")),
    };
    for w in &workers {
        crate::ops::clean(&ctx, w, &project.id, cli.value("--slot").unwrap_or(""))?;
    }
    let names: Vec<&str> = workers.iter().map(|w| w.name.as_str()).collect();
    Ok(Reply::ok(
        format!("asked {} to remove everything of \"{}\" (its files and its jobs' folders); a worker that is offline does it when it is back, and the next job there starts from a fresh copy\n", names.join(", "), project.name),
        j::obj([("ok", Value::Bool(true)), ("workers", j::strs(&names))]),
    ))
}
