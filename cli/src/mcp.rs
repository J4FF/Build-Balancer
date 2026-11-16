// mcp -- `bb mcp`: Build Balancer as tools for an AI agent (Model Context Protocol, JSON-RPC
// 2.0 over standard input and output, one message per line)
//
//   bb_workers   who is there and what each lends and does
//   bb_run       run a command (or script) of a project on a worker
//   bb_task      run a task of buildbalancer.ini
//   bb_tasks     the project's tasks
//   bb_job       a job's state; can wait for its end
//   bb_logs      a job's output, in pieces
//   bb_jobs      the newest jobs
//   bb_cancel    stop a job
//   bb_fetch     bring a job's outputs into a folder
//
// Every tool answers with one JSON document (as text and as structuredContent).  A failure
// of a tool is an answer with isError, not a protocol error, so the agent can read why.
// Register it with an agent as the command `bb mcp` (e.g. `claude mcp add build-balancer -- bb mcp`).

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

use bb_core::j::{self, Value};
use bb_core::proto::{self, EnvVar};
use bb_core::util::tick_ms;

use crate::conf::{Ctx, JobRec, Project};
use crate::jobs::{self, Request};
use crate::{fleet, guide};

/// the newest protocol revision this server was written against; an older client's own
/// revision is answered with that one
const MCP_VERSION: &str = "2025-06-18";

pub fn serve(dir: &str) -> i32 {
    agent_io::serve_stdio(|line| handle(dir, line))
}

fn response(id: Value, result: Result<Value, (i64, String)>) -> Option<String> {
    let mut v = j::obj([("jsonrpc", j::s("2.0")), ("id", id)]);
    match result {
        Ok(r) => v.insert("result", r),
        Err((code, message)) => v.insert("error", j::obj([("code", Value::i64(code)), ("message", j::s(&message))])),
    };
    Some(format!("{}\n", j::text(&v)))
}

fn handle(dir: &str, line: &[u8]) -> Option<String> {
    let Some(req) = j::parse(line) else { return response(Value::Null, Err((-32700, String::from("not JSON")))) };
    let id = req.get("id").cloned();
    let method = j::str_of(&req, "method");
    let empty = j::obj([]);
    let params = req.get("params").unwrap_or(&empty);
    // a message without an id is a notification: nothing to answer
    let id = id?;
    let result = match method {
        "initialize" => {
            let theirs = j::str_of(params, "protocolVersion");
            Ok(j::obj([
                ("protocolVersion", j::s(if theirs.is_empty() || theirs > MCP_VERSION { MCP_VERSION } else { theirs })),
                ("capabilities", j::obj([("tools", j::obj([]))])),
                ("serverInfo", j::obj([("name", j::s("build-balancer")), ("version", j::s(bb_core::VERSION))])),
                ("instructions", j::s(guide::TEXT)),
            ]))
        }
        "ping" => Ok(j::obj([])),
        "tools/list" => Ok(j::obj([("tools", tools())])),
        "tools/call" => {
            let name = j::str_of(params, "name");
            let args = params.get("arguments").unwrap_or(&empty);
            let (doc, is_error) = match call(dir, name, args) {
                Ok(v) => (v, false),
                Err(e) => (j::obj([("ok", Value::Bool(false)), ("error", j::s(&e))]), true),
            };
            Ok(j::obj([("content", Value::Array(alloc::vec![j::obj([("type", j::s("text")), ("text", j::s(&j::pretty(&doc)))])])), ("structuredContent", doc), ("isError", Value::Bool(is_error))]))
        }
        _ => Err((-32601, format!("method {} is not known here", method))),
    };
    response(id, result)
}

/// a property of a tool's input: (name, JSON type, description)
type Prop = (&'static str, &'static str, &'static str);

fn tool(name: &str, description: &str, props: &[Prop], required: &[&str]) -> Value {
    let properties = Value::Object(
        props
            .iter()
            .map(|(n, ty, d)| {
                let mut p = j::obj([("description", j::s(d))]);
                match *ty {
                    "strings" => {
                        p.insert("type", j::s("array"));
                        p.insert("items", j::obj([("type", j::s("string"))]));
                    }
                    "map" => {
                        p.insert("type", j::s("object"));
                        p.insert("additionalProperties", j::obj([("type", j::s("string"))]));
                    }
                    t => {
                        p.insert("type", j::s(t));
                    }
                }
                (String::from(*n), p)
            })
            .collect(),
    );
    j::obj([("name", j::s(name)), ("description", j::s(description)), ("inputSchema", j::obj([("type", j::s("object")), ("properties", properties), ("required", j::strs(required))]))])
}

const P_DIR: Prop = ("project_dir", "string", "The project's folder on this machine (it or a folder above holds buildbalancer.ini). Default: the folder the server was started in.");
const P_WORKER: Prop = ("worker", "string", "A worker's name. Leave out to let the least busy fitting worker be picked.");
const P_NEEDS: Prop = ("needs", "string", "What the worker must have, e.g. \"os=linux cpu>=8 ram>=16G tool=cargo tag=gpu\".");
const P_WAIT: Prop = ("wait_seconds", "integer", "Wait up to this long for the job to end before answering (0: answer at once with the job id). Keep it below your own tool timeout and ask again with bb_job.");
const P_JOB: Prop = ("job", "string", "A job id, the start of one, or \"last\".");

fn tools() -> Value {
    Value::Array(alloc::vec![
        tool("bb_workers", "List the machines that lend their power: online or not, OS and architecture, processors / memory / disk lent and idle, running and queued jobs, the build tools found there, tags. Call this before deciding where to run something.", &[], &[]),
        tool(
            "bb_run",
            "Run a command on a worker: the project's files are sent first (only what changed), the command runs in the worker's copy, files matching `outputs` come back into the project folder. Returns the job with state, exit_code and log_tail once it ended, or its id when wait_seconds ran out.",
            &[
                ("command", "strings", "The program and its arguments, run without a shell, e.g. [\"cargo\", \"build\", \"--release\"]. Either this or `script`."),
                ("script", "string", "A script for the worker's shell instead of `command` (sh on Linux, cmd on Windows)."),
                ("shell", "string", "The shell for `script`: sh, bash, cmd, powershell or pwsh."),
                P_WORKER,
                P_NEEDS,
                ("cpu", "integer", "Processors the job holds on the worker (default: all it lends, so jobs there run one at a time)."),
                ("ram", "string", "Memory the job holds, like \"8G\"."),
                ("timeout", "string", "Stop the job after this long, like \"30m\"."),
                ("outputs", "strings", "Files to bring back, relative to the project's root: a file, a folder, or a glob like \"build/**/*.exe\"."),
                ("env", "map", "Environment variables for the command."),
                ("cwd", "string", "The folder inside the project to run in (default: its root)."),
                ("slot", "string", "A separate copy of the project on the worker (default \"main\"); jobs of one slot run one after the other."),
                ("sync", "boolean", "Send the project's files first (default true)."),
                P_DIR,
                P_WAIT,
            ],
            &[],
        ),
        tool("bb_task", "Run a task defined in the project's buildbalancer.ini ([task.NAME]).", &[("task", "string", "The task's name."), P_WORKER, P_DIR, P_WAIT], &["task"]),
        tool("bb_tasks", "List the tasks the project's buildbalancer.ini defines.", &[P_DIR], &[]),
        tool("bb_job", "The state of a job: state, exit_code, error, duration, outputs, the end of its log. With wait_seconds it waits for the job to end. Outputs of a finished job are fetched into the project folder.", &[P_JOB, P_WAIT, ("log_tail_lines", "integer", "Lines of output to include (default 40).")], &["job"]),
        tool("bb_logs", "A job's output from a byte position on (the whole log may be large: read it in pieces, or only its tail).", &[P_JOB, ("from_byte", "integer", "Start here (default 0); use next_byte of the last answer to continue."), ("max_bytes", "integer", "At most this much (default 20000)."), ("tail_lines", "integer", "Instead of a position: the last N lines.")], &["job"]),
        tool("bb_jobs", "The newest jobs with their states.", &[("limit", "integer", "How many (default 20).")], &[]),
        tool("bb_cancel", "Stop a job; everything it started on the worker ends.", &[P_JOB], &["job"]),
        tool("bb_fetch", "Bring a finished job's outputs into a folder (default: the project folder, same relative paths).", &[P_JOB, ("to", "string", "Another folder to write them to.")], &["job"]),
    ])
}

fn a_str<'a>(a: &'a Value, k: &str) -> &'a str {
    a.get(k).and_then(Value::as_str).unwrap_or("")
}

fn a_u64(a: &Value, k: &str) -> u64 {
    a.get(k).and_then(Value::as_u64).unwrap_or(0)
}

fn a_strs(a: &Value, k: &str) -> Vec<String> {
    a.get(k).and_then(Value::as_array).map(|v| v.iter().filter_map(Value::as_str).map(String::from).collect()).unwrap_or_default()
}

fn project(a: &Value) -> Option<Project> {
    let dir = a_str(a, "project_dir");
    Project::find(&if dir.is_empty() { plat::cwd() } else { bb_core::util::abs_path(dir) })
}

fn no_project() -> String {
    format!("no {} in project_dir or above: run `bb init` in the project's root first", crate::conf::PROJECT_FILE)
}

/// wait for a job's end up to `secs`, fetch its outputs when it ended, and describe it
fn settle(ctx: &Ctx, rec: &mut JobRec, secs: u64, tail: usize) -> Result<Value, String> {
    if secs > 0 {
        jobs::follow(ctx, rec, Some(tick_ms() + secs.min(3600) * 1000), &mut |_| {}, &mut |_| {})?;
    } else {
        jobs::pump(ctx, rec, 0)?;
    }
    let mut files = Vec::new();
    let mut fetch_error = String::new();
    if proto::is_final(&rec.state) && !rec.artifacts.is_empty() && rec.fetched_to.is_empty() {
        match jobs::fetch(ctx, rec, None) {
            Ok(f) => files = f,
            Err(e) => fetch_error = e,
        }
    }
    let mut doc = jobs::job_json(ctx, rec, tail);
    if !files.is_empty() {
        doc.insert("output_files", j::strs(&files[..files.len().min(200)]));
    }
    if !fetch_error.is_empty() {
        doc.insert("fetch_error", j::s(&fetch_error));
    }
    if !proto::is_final(&rec.state) {
        doc.insert("hint", j::s("not finished yet: call bb_job with this job id (and wait_seconds) to follow it"));
    }
    Ok(doc)
}

fn call(dir: &str, name: &str, a: &Value) -> Result<Value, String> {
    let ctx = Ctx::load(dir)?;
    match name {
        "bb_workers" => {
            let views = fleet::fleet(&ctx)?;
            Ok(j::obj([("workers", Value::Array(views.iter().map(fleet::view_json).collect()))]))
        }
        "bb_run" => {
            let argv = a_strs(a, "command");
            let script = String::from(a_str(a, "script"));
            if argv.is_empty() == script.is_empty() {
                return Err(String::from("give either `command` (an array: program and arguments) or `script` (text for the worker's shell)"));
            }
            let sync = a.get("sync").and_then(Value::as_bool).unwrap_or(true);
            let project = project(a);
            if project.is_none() && sync {
                return Err(no_project());
            }
            let env: Vec<EnvVar> = a.get("env").and_then(Value::as_object).map(|m| m.iter().filter_map(|(k, v)| v.as_str().map(|v| EnvVar { name: k.clone(), value: String::from(v) })).collect()).unwrap_or_default();
            let ram = a_str(a, "ram");
            let timeout = a_str(a, "timeout");
            let req = Request {
                worker: String::from(a_str(a, "worker")),
                needs: String::from(a_str(a, "needs")),
                cpu: a_u64(a, "cpu") as u32,
                ram: if ram.is_empty() { 0 } else { bb_core::util::size_parse(ram).ok_or("ram: a size like \"8G\"")? },
                timeout: if timeout.is_empty() { 0 } else { bb_core::util::dur_parse(timeout).ok_or("timeout: a duration like \"30m\"")? },
                shell: if script.is_empty() { String::new() } else if a_str(a, "shell").is_empty() { String::from("default") } else { String::from(a_str(a, "shell")) },
                argv,
                script,
                cwd: String::from(a_str(a, "cwd")),
                env,
                outputs: a_strs(a, "outputs"),
                slot: String::from(a_str(a, "slot")),
                label: String::new(),
                sync,
            };
            let mut views = fleet::fleet(&ctx)?;
            let mut notes = Vec::new();
            let mut rec = jobs::submit(&ctx, project.as_ref(), &req, &mut views, &mut |m| notes.push(String::from(m)))?;
            let mut doc = settle(&ctx, &mut rec, a_u64(a, "wait_seconds"), 40)?;
            doc.insert("sync", j::strs(&notes));
            Ok(doc)
        }
        "bb_task" => {
            let project = project(a).ok_or_else(no_project)?;
            let t = project.task(a_str(a, "task"))?;
            let req = Request {
                worker: if a_str(a, "worker").is_empty() { t.worker.clone() } else { String::from(a_str(a, "worker")) },
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
            };
            let mut views = fleet::fleet(&ctx)?;
            let mut rec = jobs::submit(&ctx, Some(&project), &req, &mut views, &mut |_| {})?;
            settle(&ctx, &mut rec, a_u64(a, "wait_seconds"), 40)
        }
        "bb_tasks" => {
            let project = project(a).ok_or_else(no_project)?;
            let list: Vec<Value> = project.tasks.iter().map(|t| j::obj([("name", j::s(&t.name)), ("command", j::s(&if t.script.is_empty() { t.argv.join(" ") } else { format!("sh: {}", t.script) })), ("needs", j::s(&t.needs)), ("outputs", j::strs(&t.outputs))])).collect();
            Ok(j::obj([("project", j::s(&project.name)), ("dir", j::s(&project.dir)), ("tasks", Value::Array(list))]))
        }
        "bb_job" => {
            let mut rec = ctx.find_job(a_str(a, "job"))?;
            let tail = a.get("log_tail_lines").and_then(Value::as_u64).unwrap_or(40) as usize;
            settle(&ctx, &mut rec, a_u64(a, "wait_seconds"), tail)
        }
        "bb_logs" => {
            let mut rec = ctx.find_job(a_str(a, "job"))?;
            jobs::pump(&ctx, &mut rec, 0)?;
            let fin = Value::Bool(proto::is_final(&rec.state));
            let tail = a_u64(a, "tail_lines") as usize;
            if tail > 0 {
                return Ok(j::obj([("job", j::s(&rec.id)), ("state", j::s(&rec.state)), ("finished", fin), ("text", j::s(&jobs::log_tail(&ctx, &rec, tail))), ("log_bytes", j::n(rec.log_bytes))]));
            }
            let from = a_u64(a, "from_byte");
            let max = a.get("max_bytes").and_then(Value::as_u64).unwrap_or(20_000).clamp(100, 1 << 20) as usize;
            let mut b = jobs::log_part(&ctx, &rec.id, from, max);
            // do not cut a character in two
            while !b.is_empty() && core::str::from_utf8(&b).is_err_and(|e| e.error_len().is_none()) {
                b.pop();
            }
            Ok(j::obj([("job", j::s(&rec.id)), ("state", j::s(&rec.state)), ("finished", fin), ("text", j::s(&String::from_utf8_lossy(&b))), ("next_byte", j::n(from + b.len() as u64)), ("log_bytes", j::n(rec.log_bytes))]))
        }
        "bb_jobs" => {
            let limit = a.get("limit").and_then(Value::as_u64).unwrap_or(20).clamp(1, 200) as usize;
            let mut all = ctx.jobs();
            all.truncate(limit);
            for r in all.iter_mut().filter(|r| !proto::is_final(&r.state)) {
                let _ = jobs::pump(&ctx, r, 0);
            }
            Ok(j::obj([("jobs", Value::Array(all.iter().map(|r| jobs::job_json(&ctx, r, 0)).collect()))]))
        }
        "bb_cancel" => {
            let mut rec = ctx.find_job(a_str(a, "job"))?;
            jobs::pump(&ctx, &mut rec, 0)?;
            if !proto::is_final(&rec.state) {
                jobs::cancel(&ctx, &rec)?;
                jobs::follow(&ctx, &mut rec, Some(tick_ms() + 15_000), &mut |_| {}, &mut |_| {})?;
            }
            Ok(jobs::job_json(&ctx, &rec, 10))
        }
        "bb_fetch" => {
            let mut rec = ctx.find_job(a_str(a, "job"))?;
            jobs::pump(&ctx, &mut rec, 0)?;
            let to = a_str(a, "to");
            let files = jobs::fetch(&ctx, &mut rec, (!to.is_empty()).then_some(to))?;
            let mut doc = jobs::job_json(&ctx, &rec, 0);
            doc.insert("output_files", j::strs(&files[..files.len().min(500)]));
            Ok(doc)
        }
        _ => Err(format!("no tool \"{}\"", name)),
    }
}
