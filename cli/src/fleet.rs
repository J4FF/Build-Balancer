// fleet -- the paired workers as they are right now, and which of them fits a job
//
//   fleet(ctx)            every paired worker with what the relay knows (admitted, last seen)
//                         and its own sealed status (what it lends, what runs, its tools)
//   Needs::parse(text)    "os=linux arch=x86_64 cpu>=8 ram>=16G disk>=20G tool=cargo tag=gpu"
//   pick(views, ..)       the worker for a job: online, fits the needs, the least busy first;
//                         when none fits, the error says for each worker why not
//   view_json             the record `bb workers --json` and the MCP tool hand out

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

use bb_core::j::{self, Json, Value};
use bb_core::proto::WorkerStatus;
use bb_core::util::size_text;

use crate::conf::{Ctx, PairedWorker};

#[derive(Clone)]
pub struct WorkerView {
    pub paired: PairedWorker,
    /// the relay still admits it (false: removed there, or of another relay)
    pub admitted: bool,
    pub online: bool,
    /// seconds since the relay heard from it (-1: never)
    pub idle_secs: i64,
    pub status: Option<WorkerStatus>,
}

pub fn fleet(ctx: &Ctx) -> Result<Vec<WorkerView>, String> {
    let (devices, relay_time) = ctx.relay.devices()?;
    Ok(ctx
        .workers
        .iter()
        .map(|p| {
            let dev = devices.iter().find(|d| d.id == p.id);
            let idle_secs = dev.filter(|d| d.last_seen > 0).map_or(-1, |d| (relay_time - d.last_seen).max(0));
            let status = dev.filter(|d| !d.status.is_empty()).and_then(|d| {
                let sealed = crypto::b64::decode(&d.status)?;
                let plain = bb_core::crypt::open(&ctx.pair_key(p)?, &bb_core::crypt::aad(&p.id, "status"), &sealed)?;
                WorkerStatus::from_json(&j::parse(&plain)?)
            });
            WorkerView { paired: p.clone(), admitted: dev.is_some(), online: (0..=bb_core::ONLINE_SECS).contains(&idle_secs), idle_secs, status }
        })
        .collect())
}

/// what a job needs of a worker
#[derive(Default, Debug, Clone)]
pub struct Needs {
    pub os: String,
    pub arch: String,
    pub cpu: u32,
    pub ram: u64,
    pub disk: u64,
    pub tools: Vec<String>,
    pub tags: Vec<String>,
}

impl Needs {
    /// "os=linux cpu>=8 ram>=16G tool=cargo tool=cmake tag=gpu" (blanks or commas between the
    /// terms; "=" and ">=" mean the same for amounts)
    pub fn parse(text: &str) -> Result<Needs, String> {
        let mut n = Needs::default();
        for term in text.split([' ', ',', ';']).filter(|t| !t.is_empty()) {
            let (k, v) = term.split_once(">=").or_else(|| term.split_once('=')).ok_or_else(|| format!("needs: \"{}\" is not KEY=VALUE (os, arch, cpu, ram, disk, tool, tag)", term))?;
            match k.to_ascii_lowercase().as_str() {
                "os" => n.os = v.to_ascii_lowercase(),
                "arch" => n.arch = v.to_ascii_lowercase().replace("amd64", "x86_64").replace("x64", "x86_64").replace("arm64", "aarch64"),
                "cpu" | "cpus" => n.cpu = v.parse().map_err(|_| format!("needs: cpu takes a number, not \"{}\"", v))?,
                "ram" | "mem" | "memory" => n.ram = bb_core::util::size_parse(v).ok_or_else(|| format!("needs: ram takes a size like 8G, not \"{}\"", v))?,
                "disk" => n.disk = bb_core::util::size_parse(v).ok_or_else(|| format!("needs: disk takes a size like 20G, not \"{}\"", v))?,
                "tool" | "tools" => n.tools.extend(v.split('+').map(String::from)),
                "tag" | "tags" => n.tags.extend(v.split('+').map(|t| t.to_ascii_lowercase())),
                _ => return Err(format!("needs: unknown key \"{}\" (os, arch, cpu, ram, disk, tool, tag)", k)),
            }
        }
        Ok(n)
    }
}

/// why a worker cannot take a job with these needs (None: it can)
pub fn why_not(v: &WorkerView, n: &Needs, cpu: u32, ram: u64) -> Option<String> {
    if !v.admitted {
        return Some(String::from("removed from the relay"));
    }
    if !v.online {
        return Some(if v.idle_secs < 0 { String::from("never seen online (is bb-worker running there?)") } else { format!("offline for {}", bb_core::util::dur_text(v.idle_secs as u64 * 1000)) });
    }
    if v.status.is_none() {
        return Some(String::from("has not reported yet"));
    }
    unmet(v, n, cpu, ram)
}

/// Which of a job's needs a worker does not meet, as far as it is known: its system from the
/// pairing, everything else from its last report (None: nothing known speaks against it).
/// Being online is not asked here -- a worker that is away is still a Linux or a Windows one.
pub fn unmet(v: &WorkerView, n: &Needs, cpu: u32, ram: u64) -> Option<String> {
    let (os, arch) = match &v.status {
        Some(s) => (s.os.as_str(), s.arch.as_str()),
        None => (v.paired.os.as_str(), v.paired.arch.as_str()),
    };
    if !n.os.is_empty() && !os.is_empty() && os != n.os {
        return Some(format!("runs {}", os));
    }
    if !n.arch.is_empty() && !arch.is_empty() && arch != n.arch {
        return Some(format!("is {}", arch));
    }
    let s = v.status.as_ref()?;
    if s.protocol != bb_core::PROTOCOL {
        return Some(format!("speaks protocol {} (this bb: {}); update it", s.protocol, bb_core::PROTOCOL));
    }
    let want_cpu = n.cpu.max(cpu);
    if want_cpu > s.offer.cpu {
        return Some(format!("lends {} processors", s.offer.cpu));
    }
    let want_ram = n.ram.max(ram);
    if want_ram > s.offer.ram {
        return Some(format!("lends {} of memory", size_text(s.offer.ram)));
    }
    if n.disk > s.offer.disk.saturating_sub(s.disk_used) {
        return Some(format!("has {} of its lent disk space left", size_text(s.offer.disk.saturating_sub(s.disk_used))));
    }
    if let Some(t) = n.tools.iter().find(|t| !s.has_tool(t)) {
        return Some(format!("has no {}", t));
    }
    if let Some(t) = n.tags.iter().find(|t| !s.tags.contains(t)) {
        return Some(format!("has no tag {}", t));
    }
    None
}

/// The worker for a job: `name` picks one outright (it still has to be able to take the job);
/// otherwise the fitting worker with the fewest jobs, then the most idle processors.
pub fn pick<'a>(views: &'a [WorkerView], name: &str, n: &Needs, cpu: u32, ram: u64) -> Result<&'a WorkerView, String> {
    if views.is_empty() {
        return Err(String::from("no worker is paired yet: run `bb pair` and follow what it prints"));
    }
    if !name.is_empty() && name != "auto" {
        let v = views.iter().find(|v| v.paired.id == name || v.paired.name.eq_ignore_ascii_case(name)).ok_or_else(|| format!("no worker \"{}\"; paired: {}", name, views.iter().map(|v| v.paired.name.as_str()).collect::<Vec<_>>().join(", ")))?;
        return match why_not(v, n, cpu, ram) {
            // a named worker that is offline still gets the job, which starts once the worker is
            // back -- unless what is known of it says it never could run it
            Some(why) if v.admitted && !v.online => match unmet(v, n, cpu, ram) {
                Some(never) => Err(format!("worker {} cannot take this job: it {}", v.paired.name, never)),
                None => {
                    bb_core::util::err(&format!("note: {} is {}; the job waits at the relay until it is back\n", v.paired.name, why));
                    Ok(v)
                }
            },
            Some(why) => Err(format!("worker {} cannot take this job: it {}", v.paired.name, why)),
            None => Ok(v),
        };
    }
    let key = |v: &&WorkerView| {
        let s = v.status.as_ref();
        let jobs = s.map_or(0, |s| s.running.len() + s.queued.len());
        (jobs, core::cmp::Reverse(s.map_or(0, |s| s.cpu_idle())), s.map_or(0, |s| s.cpu_busy))
    };
    views.iter().filter(|v| why_not(v, n, cpu, ram).is_none()).min_by_key(key).ok_or_else(|| {
        let reasons: Vec<String> = views.iter().map(|v| format!("{} {}", v.paired.name, why_not(v, n, cpu, ram).unwrap_or_default())).collect();
        format!("no worker can take this job: {}", reasons.join("; "))
    })
}

pub fn view_json(v: &WorkerView) -> Value {
    let mut o = j::obj([
        ("name", j::s(&v.paired.name)),
        ("id", j::s(&v.paired.id)),
        ("online", Value::Bool(v.online)),
        ("admitted", Value::Bool(v.admitted)),
        ("seconds_since_seen", Value::i64(v.idle_secs)),
        ("os", j::s(&v.paired.os)),
        ("arch", j::s(&v.paired.arch)),
        ("host", j::s(&v.paired.host)),
    ]);
    if let Some(h) = hint(v) {
        o.insert("hint", j::s(&h));
    }
    let Some(s) = &v.status else { return o };
    let jobs = |list: &[bb_core::proto::JobBrief]| Value::Array(list.iter().map(|b| b.to_json()).collect());
    o.insert("version", j::s(&s.version));
    o.insert("status_age_seconds", Value::i64((bb_core::util::now() - s.time).max(0)));
    o.insert("cpu_model", j::s(&s.cpu_model));
    o.insert("cpu", j::obj([("total", j::n(s.cpu_total as u64)), ("lent", j::n(s.offer.cpu as u64)), ("idle", j::n(s.cpu_idle() as u64)), ("machine_busy_percent", j::n(s.cpu_busy as u64))]));
    o.insert("leave_running", Value::Bool(s.leave_running));
    o.insert("ram", j::obj([("total", j::n(s.ram_total)), ("free_now", j::n(s.ram_free)), ("lent", j::n(s.offer.ram)), ("held", j::n(s.ram_held())), ("idle", j::n(s.ram_idle()))]));
    o.insert("disk", j::obj([("lent", j::n(s.offer.disk)), ("used", j::n(s.disk_used)), ("free_on_volume", j::n(s.disk_free))]));
    o.insert("limits_held_by", j::obj([("cpu", j::s(&s.enforce_cpu)), ("ram", j::s(&s.enforce_ram)), ("disk", j::s(&s.enforce_disk))]));
    o.insert("jobs_at_once", j::n(s.offer.jobs as u64));
    o.insert("priority", j::s(&s.priority));
    o.insert("drives", Value::Array(s.drives.iter().map(|d| d.to_json()).collect()));
    // whether `bb limits` can change what it lends (false: too old, or its owner keeps it to itself)
    o.insert("main_may_set", Value::Bool(s.offer_settable()));
    o.insert("running", jobs(&s.running));
    o.insert("queued", jobs(&s.queued));
    o.insert("tools", Value::Object(s.tools.iter().map(|t| (t.name.clone(), j::s(&t.version))).collect()));
    o.insert("tags", j::strs(&s.tags));
    o.insert("workspaces", Value::Array(s.workspaces.iter().map(|w| w.to_json()).collect()));
    o
}

/// what a worker lends, one line each, as `bb limits WORKER` prints it
pub fn offer_text(s: &WorkerStatus) -> String {
    let mut disk = String::new();
    for d in &s.drives {
        let place = if d.path.is_empty() { String::from("in the data folder") } else { format!("on {}", d.path) };
        let lent = if d.lent == 0 { String::from("nothing") } else { size_text(d.lent) };
        let state = if !d.there { String::from("not there right now") } else { format!("{} used, {} free", size_text(d.used), size_text(d.free)) };
        disk += &format!("{}{} {} ({})\n", if disk.is_empty() { "" } else { "               " }, lent, place, state);
    }
    if disk.is_empty() {
        // (a worker before 0.1.7 tells the sum only)
        disk = format!("{}\n", size_text(s.offer.disk));
    }
    let set_by = if s.drives.is_empty() {
        format!("that machine only: version {} cannot be told yet (bb update -w)", s.version)
    } else if s.offer_locked {
        String::from("that machine only: its owner keeps it to themselves (bb-worker config --main-may-set yes there allows it)")
    } else {
        String::from("that machine and this one (bb limits)")
    };
    format!(
        "  processors   {} of {}\n  memory       {} of {}\n  disk         {}  jobs at once {}\n  priority     {}\n  set by       {}\n",
        s.offer.cpu,
        s.cpu_total,
        size_text(s.offer.ram),
        size_text(s.ram_total),
        disk,
        s.offer.jobs,
        if s.priority.is_empty() { "?" } else { &s.priority },
        set_by
    )
}

/// the table `bb workers` prints
pub fn table(views: &[WorkerView]) -> String {
    let mut rows: Vec<[String; 7]> = alloc::vec![["WORKER", "STATE", "SYSTEM", "CPU idle/lent", "RAM idle/lent", "DISK used/lent", "JOBS"].map(String::from)];
    for v in views {
        let state = if !v.admitted {
            String::from("removed")
        } else if v.online {
            String::from("online")
        } else if v.idle_secs < 0 {
            String::from("never seen")
        } else {
            format!("offline {}", bb_core::util::dur_text(v.idle_secs as u64 * 1000))
        };
        let sys = format!("{}/{}", v.paired.os, v.paired.arch);
        let row = match &v.status {
            Some(s) => [
                v.paired.name.clone(),
                state,
                sys,
                format!("{}/{} of {}, {}% busy", s.cpu_idle(), s.offer.cpu, s.cpu_total, s.cpu_busy),
                format!("{}/{}", size_text(s.ram_idle()), size_text(s.offer.ram)),
                format!("{}/{}", size_text(s.disk_used), size_text(s.offer.disk)),
                format!("{} running, {} queued", s.running.len(), s.queued.len()),
            ],
            None => [v.paired.name.clone(), state, sys, String::from("-"), String::from("-"), String::from("-"), String::from("-")],
        };
        rows.push(row);
    }
    let mut width = [0usize; 7];
    for r in &rows {
        for (i, c) in r.iter().enumerate() {
            width[i] = width[i].max(c.chars().count());
        }
    }
    let mut out = String::new();
    for r in &rows {
        for (i, c) in r.iter().enumerate() {
            out += c;
            if i + 1 < r.len() {
                out.extend(core::iter::repeat_n(' ', width[i] - c.chars().count() + 2));
            }
        }
        out.push('\n');
    }
    for v in views {
        if let Some(s) = &v.status {
            let tools: Vec<&str> = s.tools.iter().map(|t| t.name.as_str()).collect();
            out += &format!("\n{}: {}\n  tools: {}\n", v.paired.name, if s.cpu_model.is_empty() { "?" } else { &s.cpu_model }, if tools.is_empty() { String::from("(none found yet)") } else { tools.join(" ") });
            if !s.tags.is_empty() {
                out += &format!("  tags:  {}\n", s.tags.join(" "));
            }
            for b in &s.running {
                out += &format!("  {} {} ({}): {}\n", b.state, b.id, b.project, b.command);
            }
        }
        if let Some(h) = hint(v) {
            out += &format!("\n{}: {}\n", v.paired.name, h);
        }
        if let Some(s) = v.status.as_ref().filter(|s| older(&s.version)) {
            let how = if bb_update::can_update_itself(&s.version) {
                format!("`bb update -w {}` has it fetch the newest release and start again.", v.paired.name)
            } else {
                // it would take the order and do nothing: only from this version on a program knows it
                format!("It cannot update itself yet (that came with {}): run the install line there once more ({}#set-it-up-in-three-steps).", bb_update::FIRST, bb_core::REPOSITORY)
            };
            out += &format!("\n{}: runs version {} (this machine: {}).  {}\n", v.paired.name, s.version, bb_core::VERSION, how);
        }
    }
    out
}

/// whether a worker's version is older than this program's
pub fn older(version: &str) -> bool {
    let n = |v: &str| -> Option<[u32; 3]> {
        let mut it = v.trim().trim_start_matches('v').split(['.', '-', '+']).map(|p| p.parse::<u32>().ok());
        Some([it.next()??, it.next()??, it.next()??])
    };
    matches!((n(version), n(bb_core::VERSION)), (Some(a), Some(b)) if a < b)
}

/// what to do about a worker that is not there (None: it is)
pub fn hint(v: &WorkerView) -> Option<String> {
    let host = if v.paired.host.is_empty() { v.paired.name.as_str() } else { v.paired.host.as_str() };
    if !v.admitted {
        return Some(String::from("it was removed at the relay; `bb unpair` takes it off this list, `bb pair` adds it anew"));
    }
    if v.online {
        return None;
    }
    Some(if v.idle_secs < 0 {
        format!("paired, but it never came online.  Nothing runs there yet: start `bb-worker run` on {host} (or the app, or `sudo bb-worker service install` to have it start with the machine).  Was the machine paired anew since?  Then this entry is left over: bb unpair {name}", name = v.paired.name)
    } else {
        format!("offline for {}.  Is {host} switched on, and does bb-worker run there (`bb-worker service status`, or the app)?", bb_core::util::dur_text(v.idle_secs as u64 * 1000))
    })
}
