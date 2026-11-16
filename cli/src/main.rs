// bb -- Build Balancer on the main machine: hand builds to the machines that lend their power
//
//   bb setup RELAY KEY            connect this machine to a relay (the relay prints the line)
//   bb pair                       add a worker: prints the command to run there, with a one-time code
//   bb workers                    what every worker is, lends and does right now
//   bb unpair WORKER
//   bb init                       make the current folder a project (writes buildbalancer.ini)
//   bb run [options] COMMAND..    send the project to a worker, run COMMAND there, bring the outputs back
//   bb task NAME.. | --all        run tasks of buildbalancer.ini, several at once on different workers
//   bb tasks                      the project's tasks
//   bb sync [-w WORKER | --all]   send the project without running anything
//   bb jobs | status JOB | logs JOB [-f] | wait JOB.. | cancel JOB | fetch JOB | forget JOB
//   bb clean -w WORKER            remove the project's workspace on a worker
//   bb relay                      what the relay holds
//   bb mcp                        the same as tools for an AI agent (Model Context Protocol on stdio)
//   bb guide                      how an agent (or a person) works with bb
//
// Every command takes --json (one JSON document on standard output, progress on standard
// error) and --config DIR.  A job's id, the start of it, or "last" names a job.
#![no_std]
#![no_main]
#![deny(unsafe_code)]

extern crate alloc;

mod conf;
mod fleet;
mod guide;
mod jobs;
mod mcp;
mod run;

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

use bb_core::client::Relay;
use bb_core::crypt::{self, Keys};
use bb_core::j::{self, Json, Value};
use bb_core::proto::{Device, Hello, Welcome, ROLE_WORKER};
use bb_core::util::{err, now, out, size_text, tick_ms, Cli};

use crate::conf::{Ctx, MainConfig, PairedWorker, Project};

/// a command's result: text for people or a document for programs, and the exit code
pub struct Reply {
    pub code: i32,
    pub text: String,
    pub json: Value,
}

impl Reply {
    pub fn ok(text: impl Into<String>, json: Value) -> Reply {
        Reply { code: 0, text: text.into(), json }
    }
}

pub type CmdResult = Result<Reply, String>;

/// progress for people: a line on standard error
pub fn say(s: &str) {
    err(&format!("bb: {}\n", s));
}

fn usage() -> String {
    String::from(
        "bb setup RELAY KEY [--name NAME]      connect this machine to a relay (bb-relay prints the line)\n\
         bb pair [--minutes N]                 add a worker (prints the command for that machine)\n\
         bb workers                            what the workers are, lend and do\n\
         bb unpair WORKER\n\
         \n\
         bb init [--name NAME]                 make this folder a project\n\
         bb run [options] [--] COMMAND ARGS..  run a command on a worker\n\
         \x20   -w, --worker NAME    this worker (default: the least busy one that fits)\n\
         \x20   --needs \"os=linux cpu>=8 ram>=8G tool=cargo tag=gpu\"\n\
         \x20   --cpu N --ram SIZE   what the job holds on the worker (default: all it lends)\n\
         \x20   --out PATTERN        files to bring back (\"build/**/*.exe\"; repeatable)\n\
         \x20   --env NAME=VALUE     (repeatable)   --cwd DIR   --timeout 30m   --slot NAME\n\
         \x20   --sh \"SCRIPT\"        a shell script instead of COMMAND (--shell sh|bash|cmd|powershell)\n\
         \x20   -d, --detach         do not wait: prints the job's id\n\
         \x20   --no-sync            do not send the project's files first\n\
         \x20   --no-fetch           leave the outputs at the relay (bb fetch JOB)   --to DIR\n\
         bb task NAME.. | --all [-w NAME] [-d] run tasks of buildbalancer.ini (several: at once)\n\
         bb tasks                              list them\n\
         bb sync [-w NAME | --all]             send the project only\n\
         \n\
         bb jobs [--all]                       the jobs, newest first\n\
         bb status JOB                         one job (JOB: an id, its start, or \"last\")\n\
         bb logs JOB [-f] [--tail N]           its output (-f: follow)\n\
         bb wait JOB.. [--timeout 30m]         until they ended\n\
         bb cancel JOB\n\
         bb fetch JOB [--to DIR]               bring its outputs here\n\
         bb forget JOB | --done                drop job records\n\
         bb clean -w NAME [--slot NAME]        remove this project's workspace on a worker\n\
         \n\
         bb relay                              what the relay holds\n\
         bb mcp                                serve these as tools for an AI agent (MCP, stdio)\n\
         bb guide                              how to work with bb, for agents and people\n\
         \n\
         everywhere: --json (a JSON document instead of text), --config DIR, --version\n",
    )
}

fn main_cmd() -> i32 {
    let args: Vec<String> = plat::args().iter().skip(1).cloned().collect();
    let Some(cmd) = args.first().map(String::as_str) else {
        out(&usage());
        return 2;
    };
    let rest = &args[1..];
    // --json and --config count wherever they stand before a command's own words
    let before_dashes = rest.iter().take_while(|a| *a != "--");
    let json = before_dashes.clone().any(|a| a == "--json");
    let r = match cmd {
        "--version" | "-V" | "version" => Ok(Reply::ok(bb_core::version_line("bb"), j::obj([("version", j::s(bb_core::VERSION)), ("protocol", j::n(bb_core::PROTOCOL as u64))]))),
        "--help" | "-h" | "help" => Ok(Reply::ok(usage(), Value::Null)),
        "guide" => Ok(Reply::ok(guide::TEXT, j::obj([("guide", j::s(guide::TEXT))]))),
        "setup" => setup(rest),
        "pair" => pair(rest, json),
        "workers" | "ls" => workers(rest),
        "unpair" => unpair(rest),
        "init" => init(rest),
        "relay" => relay_cmd(rest),
        "run" => run::run(rest, json),
        "task" => run::task(rest, json),
        "tasks" => run::tasks(rest),
        "sync" => run::sync(rest, json),
        "jobs" => run::jobs(rest),
        "status" => run::status(rest),
        "logs" | "log" => run::logs(rest, json),
        "wait" => run::wait(rest, json),
        "cancel" => run::cancel(rest),
        "fetch" => run::fetch(rest),
        "forget" => run::forget(rest),
        "clean" => run::clean(rest),
        "mcp" => {
            return match Cli::parse(rest, &["--config"], &[], false) {
                Ok(c) => mcp::serve(&conf::config_dir(c.value("--config"))),
                Err(e) => {
                    err(&format!("bb: {}\n", e));
                    2
                }
            };
        }
        other => Err(format!("unknown command \"{}\" (bb help)", other)),
    };
    match r {
        Ok(rep) => {
            if json {
                out(&format!("{}\n", j::pretty(&rep.json)));
            } else if !rep.text.is_empty() {
                out(&rep.text);
                if !rep.text.ends_with('\n') {
                    out("\n");
                }
            }
            rep.code
        }
        Err(e) => {
            if json {
                out(&format!("{}\n", j::pretty(&j::obj([("ok", Value::Bool(false)), ("error", j::s(&e))]))));
            } else {
                err(&format!("bb: {}\n", e));
            }
            1
        }
    }
}

/// a command's options: `valued` take a value, everything else in `flags` is a switch; --json
/// and --config belong to every command
pub fn parse(args: &[String], valued: &[&str], flags: &[&str], short: &[(&str, &str)], command: bool) -> Result<Cli, String> {
    let mut v: Vec<&str> = valued.to_vec();
    v.push("--config");
    let cli = Cli::parse(args, &v, short, command)?;
    let mut known = v.clone();
    known.extend_from_slice(flags);
    known.push("--json");
    cli.only(&known)?;
    Ok(cli)
}

pub fn ctx(cli: &Cli) -> Result<Ctx, String> {
    Ctx::load(&conf::config_dir(cli.value("--config")))
}

fn setup(args: &[String]) -> CmdResult {
    let cli = parse(args, &["--name"], &["--force"], &[], false)?;
    let (Some(addr), Some(key)) = (cli.pos.first(), cli.pos.get(1)) else {
        return Err(String::from("bb setup RELAY KEY -- bb-relay prints this line when it starts (a relay that is taken already: `bb-relay setup-key` on the server)"));
    };
    let dir = conf::config_dir(cli.value("--config"));
    let path = plat::fs::join(&dir, "main.json");
    let old = j::load::<MainConfig>(&path).filter(|m| !m.id.is_empty());
    if let Some(o) = &old
        && !cli.flag("--force") {
            return Err(format!("this machine is already set up with the relay {}; --force connects it to this one instead (workers of the old relay have to join anew)", o.relay));
        }
    let (token, fp) = crypt::setup_key_decode(key).ok_or("this is not a setup key: copy the whole line bb-relay printed")?;
    let relay = Relay::open(addr, &fp, "", "").map_err(|e| if fp.is_empty() { format!("{} (a relay started with --plain is addressed as http://HOST:PORT)", e) } else { e })?;
    let info = relay.info()?;
    // a machine that is set up anew keeps who it is, so its job records stay readable
    let mut m = old.unwrap_or_else(|| MainConfig { id: bb_core::util::new_id("m"), secret: Keys::generate().secret_hex(), data_key: crypto::hex(&plat::random_bytes::<32>()), ..MainConfig::default() });
    m.name = cli.value("--name").map(String::from).unwrap_or_else(|| if m.name.is_empty() { plat::net::hostname() } else { m.name.clone() });
    m.token = relay.claim(&token, &m.id, &m.name)?;
    m.relay = relay.address();
    m.relay_fp = fp;
    m.relay_id = info.relay_id;
    if !plat::fs::mkdir_private(&dir) && plat::file_exists(&dir) != 2 {
        return Err(format!("{} cannot be created", dir));
    }
    if !j::save(&path, &m) {
        return Err(format!("{} cannot be written", path));
    }
    Ok(Reply::ok(
        format!("This machine (\"{}\") is now the main machine of the relay {}.\n\nNext: add a machine that lends its power --\n\n    bb pair\n\nprints the command to run on it.\n", m.name, m.relay),
        j::obj([("ok", Value::Bool(true)), ("name", j::s(&m.name)), ("id", j::s(&m.id)), ("relay", j::s(&m.relay))]),
    ))
}

/// the relay's address as the join command shows it
fn relay_short(addr: &str) -> &str {
    addr.strip_prefix("https://").unwrap_or(addr)
}

fn pair(args: &[String], json: bool) -> CmdResult {
    let cli = parse(args, &["--minutes"], &[], &[], false)?;
    let mut ctx = ctx(&cli)?;
    let minutes = cli.value("--minutes").and_then(|m| m.parse::<u32>().ok()).unwrap_or(10).clamp(1, 60);
    let code = crypt::code_new();
    let (slot, key) = crypt::code_secrets(&code, &ctx.main.relay_id);
    ctx.relay.pair_open(&slot, minutes * 60)?;
    let command = format!("bb-worker join {} {}", relay_short(&ctx.main.relay), code);
    if json {
        // an agent shows this to its person and keeps waiting for the document that follows
        out(&format!("{}\n", j::text(&j::obj([("waiting", Value::Bool(true)), ("code", j::s(&code)), ("run_on_worker", j::s(&command)), ("minutes", j::n(minutes as u64))]))));
    } else {
        out(&format!("On the machine that should lend its power, run:\n\n    {}\n\n(bb-worker is the program for that machine.  The code works once, for {} minutes.)\nwaiting ...\n", command, minutes));
    }
    let end = tick_ms() + minutes as u64 * 60_000;
    let sealed = loop {
        match ctx.relay.pair_get(&slot, "worker", 20) {
            Ok(Some(b)) => break b,
            Ok(None) if tick_ms() < end => {}
            Ok(None) => return Err(String::from("nobody joined in time; run `bb pair` again for a new code")),
            Err(e) if e.status == 404 => return Err(String::from("the pairing expired; run `bb pair` again for a new code")),
            Err(e) => return Err(format!("{}", e)),
        }
    };
    let hello = crypt::open(&key, &crypt::aad("worker", "pair"), &sealed).and_then(|p| j::parse(&p)).and_then(|v| Hello::from_json(&v)).ok_or("a message arrived that this code does not open: it was not written with the code (nothing was paired)")?;
    let refuse = |why: &str| -> String {
        let w = Welcome { protocol: bb_core::PROTOCOL, error: String::from(why), ..Welcome::default() };
        let _ = ctx.relay.pair_put(&slot, "main", &crypt::seal(&key, &crypt::aad("main", "pair"), j::text(&w.to_json()).as_bytes()));
        format!("not paired: {}", why)
    };
    if hello.protocol != bb_core::PROTOCOL {
        return Err(refuse(&format!("the worker speaks protocol {}, this bb {}: update the older program", hello.protocol, bb_core::PROTOCOL)));
    }
    if crypt::key32(&hello.public).is_none() || hello.name.trim().is_empty() {
        return Err(refuse("the worker's hello is incomplete"));
    }
    if hello.relay_fp != ctx.main.relay_fp {
        return Err(refuse("the worker sees another certificate at the relay than this machine: the connection of one of the two is being tampered with, or the address leads elsewhere"));
    }
    // two workers of one name would be told apart by nobody
    let mut name: String = hello.name.trim().chars().take(60).collect();
    let mut n = 2;
    while ctx.workers.iter().any(|w| w.name.eq_ignore_ascii_case(&name)) {
        name = format!("{}-{}", hello.name.trim(), n);
        n += 1;
    }
    let worker = PairedWorker { id: bb_core::util::new_id("w"), name: name.clone(), public: hello.public.clone(), os: hello.os.clone(), arch: hello.arch.clone(), host: hello.host.clone(), added: now() };
    let token = crypt::token_new();
    ctx.relay.device_add(&Device { id: worker.id.clone(), name: name.clone(), role: String::from(ROLE_WORKER), token_hash: crypt::token_hash(&token), ..Device::default() })?;
    let w = Welcome {
        protocol: bb_core::PROTOCOL,
        error: String::new(),
        relay_id: ctx.main.relay_id.clone(),
        relay_fp: ctx.main.relay_fp.clone(),
        main_id: ctx.main.id.clone(),
        main_name: ctx.main.name.clone(),
        main_public: ctx.keys.public_hex(),
        worker_id: worker.id.clone(),
        worker_token: token,
        data_key: ctx.main.data_key.clone(),
    };
    ctx.relay.pair_put(&slot, "main", &crypt::seal(&key, &crypt::aad("main", "pair"), j::text(&w.to_json()).as_bytes()))?;
    ctx.workers.push(worker.clone());
    ctx.save_workers()?;
    Ok(Reply::ok(
        format!("Paired: \"{}\" ({}/{}, host {}).\nIt works once `bb-worker run` (or its service) is started there; `bb workers` shows it.\n", name, worker.os, worker.arch, worker.host),
        j::obj([("ok", Value::Bool(true)), ("worker", worker.to_json())]),
    ))
}

fn workers(args: &[String]) -> CmdResult {
    let cli = parse(args, &[], &[], &[], false)?;
    let ctx = ctx(&cli)?;
    let views = fleet::fleet(&ctx)?;
    let json = j::obj([("relay", j::s(&ctx.main.relay)), ("workers", Value::Array(views.iter().map(fleet::view_json).collect()))]);
    let text = if views.is_empty() { String::from("No worker is paired yet.  `bb pair` adds one.\n") } else { fleet::table(&views) };
    Ok(Reply::ok(text, json))
}

fn unpair(args: &[String]) -> CmdResult {
    let cli = parse(args, &[], &[], &[], false)?;
    let mut ctx = ctx(&cli)?;
    let name = cli.pos.first().ok_or("bb unpair WORKER")?;
    let w = ctx.worker(name)?.clone();
    match ctx.relay.device_remove(&w.id) {
        Ok(()) => {}
        Err(e) if e.status == 404 => {}
        Err(e) => return Err(format!("{}", e)),
    }
    ctx.workers.retain(|x| x.id != w.id);
    ctx.save_workers()?;
    Ok(Reply::ok(format!("\"{}\" is removed: the relay no longer admits it.  (`bb-worker leave` there forgets the pairing too.)\n", w.name), j::obj([("ok", Value::Bool(true)), ("removed", j::s(&w.name))])))
}

fn init(args: &[String]) -> CmdResult {
    let cli = parse(args, &["--name"], &[], &[], false)?;
    let dir = plat::cwd();
    let path = plat::fs::join(&dir, conf::PROJECT_FILE);
    if plat::file_exists(&path) != 0 {
        let p = Project::load(&dir)?;
        return Ok(Reply::ok(format!("{} is already a project (\"{}\", {}).\n", dir, p.name, p.id), j::obj([("ok", Value::Bool(true)), ("project", j::s(&p.name)), ("id", j::s(&p.id)), ("created", Value::Bool(false))])));
    }
    let folder = dir.rsplit(['/', '\\']).find(|p| !p.is_empty()).unwrap_or("project");
    let name = cli.value("--name").unwrap_or(folder);
    let id = bb_core::util::new_id("p");
    if !bb_core::util::put_file(&path, conf::project_template(name, &id).as_bytes()) {
        return Err(format!("{} cannot be written", path));
    }
    Ok(Reply::ok(
        format!(
            "{} written: this folder is the project \"{}\".\n\n  bb run -- COMMAND          sends its files to a worker and runs COMMAND there\n  bb run --out \"build/**\" -- COMMAND     ... and brings build/ back\n  bb task NAME               runs a task you define in {}\n\nWhat is sent: every file here except what .gitignore and .bbignore leave out.\n",
            conf::PROJECT_FILE,
            name,
            conf::PROJECT_FILE
        ),
        j::obj([("ok", Value::Bool(true)), ("project", j::s(name)), ("id", j::s(&id)), ("created", Value::Bool(true)), ("file", j::s(&path))]),
    ))
}

fn relay_cmd(args: &[String]) -> CmdResult {
    let cli = parse(args, &[], &[], &[], false)?;
    let ctx = ctx(&cli)?;
    let s = ctx.relay.stats()?;
    let n = |k: &str| j::u64_of(&s, k);
    let text = format!(
        "relay {} ({}), version {}\n  devices   {}\n  blobs     {} in {} files (limit {})\n  channels  {} holding {}\n  disk      {} free of {}\n",
        ctx.main.relay,
        j::str_of(&s, "relay_id"),
        j::str_of(&s, "version"),
        n("devices"),
        size_text(n("blob_bytes")),
        n("blobs"),
        size_text(n("blob_limit")),
        n("channels"),
        size_text(n("channel_bytes")),
        size_text(n("disk_free")),
        size_text(n("disk_total"))
    );
    Ok(Reply::ok(text, s))
}

rt::main!(main_cmd());
