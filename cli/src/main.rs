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
//   bb clean [-w WORKER | --all]  remove what a worker (every worker) keeps of the project
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

mod guide;
mod mcp;
mod run;

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

use bb_core::j::{self, Json, Value};
use bb_core::util::{err, out, size_text, Cli};
// the library half of this program: its modules under the names the command files use
use bb_main::conf::Ctx;
use bb_main::{conf, fleet, jobs, ops};

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
         bb clean [-w NAME | --all]            remove what a worker (--all: every worker) keeps of this project\n\
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
    // (`bb run` ends its options at the command's first word: what follows is the command's)
    let json = if cmd == "run" { run::run_wants_json(rest) } else { rest.iter().take_while(|a| *a != "--").any(|a| a == "--json") };
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
    // A setup key is random text and may begin with "-" (relays before this fix made such
    // keys): it is the key, not an option.
    let is_key = |a: &String| a.starts_with('-') && bb_core::crypt::setup_key_decode(a).is_some();
    let options: Vec<String> = args.iter().filter(|a| !is_key(a)).cloned().collect();
    let mut cli = parse(&options, &["--name"], &["--force"], &[], false)?;
    cli.pos.extend(args.iter().filter(|a| is_key(a)).cloned());
    let (Some(addr), Some(key)) = (cli.pos.first(), cli.pos.get(1)) else {
        return Err(String::from("bb setup RELAY KEY -- bb-relay prints this line when it starts (a relay that is taken already: `bb-relay setup-key` on the server)"));
    };
    let dir = conf::config_dir(cli.value("--config"));
    let m = ops::setup(&dir, addr, key, cli.value("--name"), cli.flag("--force"))?;
    Ok(Reply::ok(
        format!("This machine (\"{}\") is now the main machine of the relay {}.\n\nNext: add a machine that lends its power --\n\n    bb pair\n\nprints the command to run on it.\n", m.name, m.relay),
        j::obj([("ok", Value::Bool(true)), ("name", j::s(&m.name)), ("id", j::s(&m.id)), ("relay", j::s(&m.relay))]),
    ))
}

fn pair(args: &[String], json: bool) -> CmdResult {
    let cli = parse(args, &["--minutes"], &[], &[], false)?;
    let mut ctx = ctx(&cli)?;
    let minutes = cli.value("--minutes").and_then(|m| m.parse::<u32>().ok()).unwrap_or(ops::PAIR_MINUTES);
    let p = ops::pair_begin(&ctx, minutes)?;
    if json {
        // an agent shows this to its person and keeps waiting for the document that follows
        out(&format!("{}\n", j::text(&j::obj([("waiting", Value::Bool(true)), ("code", j::s(&p.code)), ("relay", j::s(&p.relay)), ("run_on_worker", j::s(&p.command)), ("minutes", j::n(p.minutes as u64))]))));
    } else {
        out(&format!(
            "On the machine that should lend its power, run:\n\n    {}\n\nIt pairs the machine and starts lending right away.  (Or start the Build Balancer app there and\nenter the relay {} and the code {}.)\n\nNo program on that machine yet?  {}\n\nThe code works once, for {} minutes.\nwaiting ...\n",
            p.command, p.relay, p.code, install_hint(&p), p.minutes
        ));
    }
    // say now and then how long the code still works: getting the program over can take a while
    let mut told = p.minutes;
    let worker = loop {
        if let Some(w) = ops::pair_poll(&mut ctx, &p, 20)? {
            break w;
        }
        let left = (p.deadline.saturating_sub(bb_core::util::tick_ms()) / 60_000) as u32;
        if !json && left < told && (left % 5 == 0 || left <= 2) && left > 0 {
            told = left;
            out(&format!("... {} minute{} left for the code\n", left, if left == 1 { "" } else { "s" }));
        }
    };
    Ok(Reply::ok(
        format!("Paired: \"{}\" ({}/{}, host {}).\nIt lends its power as soon as it runs there: `bb-worker join` goes on running by itself, so does the app.\n`bb workers` shows it.\n", worker.name, worker.os, worker.arch, worker.host),
        j::obj([("ok", Value::Bool(true)), ("worker", worker.to_json())]),
    ))
}

/// where the programs come from for a machine that has none yet
fn install_hint(p: &ops::Pairing) -> String {
    format!("This line fetches it first, then pairs:\n\n    Linux, macOS:  {}\n    Windows:       {}", p.install_line_sh(), p.install_line_powershell())
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
    let w = ops::unpair(&mut ctx, name)?;
    Ok(Reply::ok(format!("\"{}\" is removed: the relay no longer admits it.  (`bb-worker leave --purge` there removes what it kept.)\n", w.name), j::obj([("ok", Value::Bool(true)), ("removed", j::s(&w.name))])))
}

fn init(args: &[String]) -> CmdResult {
    let cli = parse(args, &["--name"], &[], &[], false)?;
    let dir = plat::cwd();
    let (p, created) = ops::init_project(&dir, cli.value("--name"))?;
    let doc = j::obj([("ok", Value::Bool(true)), ("project", j::s(&p.name)), ("id", j::s(&p.id)), ("created", Value::Bool(created)), ("file", j::s(&plat::fs::join(&dir, conf::PROJECT_FILE)))]);
    if !created {
        return Ok(Reply::ok(format!("{} is already a project (\"{}\", {}).\n", dir, p.name, p.id), doc));
    }
    Ok(Reply::ok(
        format!(
            "{} written: this folder is the project \"{}\".\n\n  bb run -- COMMAND          sends its files to a worker and runs COMMAND there\n  bb run --out \"build/**\" -- COMMAND     ... and brings build/ back\n  bb task NAME               runs a task you define in {}\n\nWhat is sent: every file here except what .gitignore and .bbignore leave out.\n",
            conf::PROJECT_FILE,
            p.name,
            conf::PROJECT_FILE
        ),
        doc,
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
