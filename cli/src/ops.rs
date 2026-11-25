// ops -- what a main machine does besides jobs, as functions the command line and the app share
//
//   setup            connect this machine to a relay with its setup key: it becomes the main machine
//   pair_begin       open a pairing: the code and the command for the other machine
//   pair_poll        wait a while for that machine; when it came, admit it and answer it
//   unpair           remove a worker from the relay and from this machine's list
//   init_project     make a folder a project (writes buildbalancer.ini)
//   clean            ask a worker to remove a project's files
//   parse_setup_line "bb setup HOST:PORT KEY" as people paste it

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

use bb_core::client::Relay;
use bb_core::crypt::{self, Keys};
use bb_core::j::{self, Json};
use bb_core::proto::{self, Device, Hello, OfferChange, Welcome, ROLE_WORKER};
use bb_core::util::{now, tick_ms};

use crate::conf::{self, Ctx, MainConfig, PairedWorker, Project};

/// this machine's main.json, when it is set up
pub fn main_config(dir: &str) -> Option<MainConfig> {
    j::load::<MainConfig>(&plat::fs::join(dir, "main.json")).filter(|m| !m.id.is_empty())
}

/// a line as people paste it -> (relay, setup key): "bb setup HOST:PORT KEY" or "HOST:PORT KEY"
pub fn parse_setup_line(line: &str) -> Option<(String, String)> {
    let words: Vec<&str> = line.split_whitespace().filter(|w| !matches!(*w, "bb" | "bb.exe" | "./bb" | "setup")).collect();
    match words.as_slice() {
        [addr, key] if crypt::setup_key_decode(key).is_some() => Some((String::from(*addr), String::from(*key))),
        _ => None,
    }
}

/// Connect this machine to the relay at `addr` with its setup key.  A machine that was set up
/// before keeps who it is (its id and keys), so its job records and paired workers of the same
/// relay stay valid; `force` allows replacing the relay.
pub fn setup(dir: &str, addr: &str, key: &str, name: Option<&str>, force: bool) -> Result<MainConfig, String> {
    let path = plat::fs::join(dir, "main.json");
    let old = main_config(dir);
    if let Some(o) = &old
        && !force
    {
        return Err(format!("this machine is already set up with the relay {}; --force connects it to this one instead (workers of the old relay have to join anew)", o.relay));
    }
    let (token, fp) = crypt::setup_key_decode(key).ok_or("this is not a setup key: copy the whole line bb-relay printed")?;
    let relay = Relay::open(addr, &fp, "", "").map_err(|e| if fp.is_empty() { format!("{} (a relay started with --plain is addressed as http://HOST:PORT)", e) } else { e })?;
    let info = relay.first_info()?;
    let mut m = old.unwrap_or_else(|| MainConfig { id: bb_core::util::new_id("m"), secret: Keys::generate().secret_hex(), data_key: crypto::hex(&plat::random_bytes::<32>()), ..MainConfig::default() });
    m.name = name.map(String::from).unwrap_or_else(|| if m.name.is_empty() { plat::net::hostname() } else { m.name.clone() });
    m.token = relay.claim(&token, &m.id, &m.name)?;
    m.relay = relay.address();
    m.relay_fp = fp;
    m.relay_id = info.relay_id;
    if !plat::fs::mkdir_private(dir) && plat::file_exists(dir) != 2 {
        return Err(format!("{} cannot be created", dir));
    }
    if !j::save(&path, &m) {
        return Err(format!("{} cannot be written", path));
    }
    Ok(m)
}

/// a pairing that waits for the other machine
#[derive(Clone)]
pub struct Pairing {
    pub code: String,
    /// the relay as the other machine addresses it ("host:port")
    pub relay: String,
    /// the whole line for a terminal there
    pub command: String,
    pub minutes: u32,
    /// tick_ms() when the slot ends
    pub deadline: u64,
    slot: String,
    key: [u8; 32],
}

impl Pairing {
    /// One line for a Linux or macOS machine that has no Build Balancer yet: it fetches the
    /// programs of the latest release, pairs and starts lending.
    pub fn install_line_sh(&self) -> String {
        format!("curl -fsSL {}/releases/latest/download/install.sh | sh -s -- worker {} {}", bb_core::REPOSITORY, self.relay, self.code)
    }

    /// the same for Windows (PowerShell)
    pub fn install_line_powershell(&self) -> String {
        format!("powershell -c \"& ([scriptblock]::Create((irm {}/releases/latest/download/install.ps1))) worker {} {}\"", bb_core::REPOSITORY, self.relay, self.code)
    }
}

/// how long a pairing code works when nothing else is asked for: long enough to get the
/// program onto the other machine first
pub const PAIR_MINUTES: u32 = 30;

pub fn pair_begin(ctx: &Ctx, minutes: u32) -> Result<Pairing, String> {
    let minutes = minutes.clamp(1, 60);
    let code = crypt::code_new();
    let (slot, key) = crypt::code_secrets(&code, &ctx.main.relay_id);
    ctx.relay.pair_open(&slot, minutes * 60)?;
    let relay = String::from(ctx.main.relay.strip_prefix("https://").unwrap_or(&ctx.main.relay));
    Ok(Pairing { command: format!("bb-worker join {} {}", relay, code), relay, code, minutes, deadline: tick_ms() + minutes as u64 * 60_000, slot, key })
}

/// a pairing for pictures: nothing is opened anywhere (the app's --demo)
pub fn demo_pairing(relay: &str, code: &str) -> Pairing {
    Pairing { code: String::from(code), relay: String::from(relay), command: format!("bb-worker join {} {}", relay, code), minutes: PAIR_MINUTES, deadline: tick_ms() + PAIR_MINUTES as u64 * 60_000, slot: String::new(), key: [0; 32] }
}

/// Wait up to `wait` seconds for the machine with the code.  Ok(None): nobody yet (ask again
/// until `deadline`); Ok(Some): it is paired and in `ctx.workers`; Err: the pairing is over.
pub fn pair_poll(ctx: &mut Ctx, p: &Pairing, wait: u32) -> Result<Option<PairedWorker>, String> {
    let sealed = match ctx.relay.pair_get(&p.slot, "worker", wait) {
        Ok(Some(b)) => b,
        Ok(None) if tick_ms() < p.deadline => return Ok(None),
        Ok(None) => return Err(String::from("nobody joined in time, and the code is void now: run `bb pair` again for a new one")),
        Err(e) if e.status == 404 => return Err(String::from("the code has expired: run `bb pair` again for a new one")),
        Err(e) => return Err(format!("{}", e)),
    };
    let hello = crypt::open(&p.key, &crypt::aad("worker", "pair"), &sealed).and_then(|b| j::parse(&b)).and_then(|v| Hello::from_json(&v)).ok_or("a message arrived that this code does not open: it was not written with the code (nothing was paired)")?;
    let refuse = |why: &str| -> String {
        let w = Welcome { protocol: bb_core::PROTOCOL, error: String::from(why), ..Welcome::default() };
        let _ = ctx.relay.pair_put(&p.slot, "main", &crypt::seal(&p.key, &crypt::aad("main", "pair"), j::text(&w.to_json()).as_bytes()));
        format!("not paired: {}", why)
    };
    if hello.protocol != bb_core::PROTOCOL {
        return Err(refuse(&format!("the worker speaks protocol {}, this program {}: update the older one", hello.protocol, bb_core::PROTOCOL)));
    }
    if crypt::key32(&hello.public).is_none() || hello.name.trim().is_empty() {
        return Err(refuse("the worker's hello is incomplete"));
    }
    if hello.relay_fp != ctx.main.relay_fp {
        return Err(refuse("the worker sees another certificate at the relay than this machine: the connection of one of the two is being tampered with, or the address leads elsewhere"));
    }
    // Onto this machine's list first: a worker that was welcomed but is on no list could be
    // neither used nor removed.  The list is the file's as it is now -- another pairing may
    // have ended meanwhile.
    let wanted: String = hello.name.trim().chars().take(60).collect();
    let mut worker = PairedWorker { id: bb_core::util::new_id("w"), name: String::new(), public: hello.public.clone(), os: hello.os.clone(), arch: hello.arch.clone(), host: hello.host.clone(), added: now() };
    ctx.update_workers(&mut |list| {
        // two workers of one name would be told apart by nobody
        let mut name = wanted.clone();
        let mut n = 2;
        while list.iter().any(|w| w.name.eq_ignore_ascii_case(&name)) {
            name = format!("{}-{}", wanted, n);
            n += 1;
        }
        worker.name = name;
        list.push(worker.clone());
    })?;
    let token = crypt::token_new();
    let device = Device { id: worker.id.clone(), name: worker.name.clone(), role: String::from(ROLE_WORKER), token_hash: crypt::token_hash(&token), ..Device::default() };
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
    let welcome = crypt::seal(&p.key, &crypt::aad("main", "pair"), j::text(&w.to_json()).as_bytes());
    let sent = ctx.relay.device_add(&device).and_then(|_| ctx.relay.pair_put(&p.slot, "main", &welcome));
    if let Err(e) = sent {
        // not paired after all: off the list again, and not admitted at the relay
        let _ = ctx.relay.device_remove(&worker.id);
        let _ = ctx.update_workers(&mut |list| list.retain(|x| x.id != worker.id));
        return Err(format!("{}", e));
    }
    Ok(Some(worker))
}

/// remove a worker: the relay no longer admits it, this machine forgets it
pub fn unpair(ctx: &mut Ctx, name: &str) -> Result<PairedWorker, String> {
    let w = ctx.worker(name)?.clone();
    match ctx.relay.device_remove(&w.id) {
        Ok(()) => {}
        Err(e) if e.status == 404 => {}
        Err(e) => return Err(format!("{}", e)),
    }
    ctx.update_workers(&mut |list| list.retain(|x| x.id != w.id))?;
    Ok(w)
}

/// make `dir` a project -> (the project, whether its file was written now)
pub fn init_project(dir: &str, name: Option<&str>) -> Result<(Project, bool), String> {
    let path = plat::fs::join(dir, conf::PROJECT_FILE);
    if plat::file_exists(&path) != 0 {
        return Project::load(dir).map(|p| (p, false));
    }
    let folder = dir.rsplit(['/', '\\']).find(|p| !p.is_empty()).unwrap_or("project");
    let id = bb_core::util::new_id("p");
    if !bb_core::util::put_file(&path, conf::project_template(name.unwrap_or(folder), &id).as_bytes()) {
        return Err(format!("{} cannot be written", path));
    }
    Project::load(dir).map(|p| (p, true))
}

/// Ask a worker to bring its programs up to the newest release.  It fetches and checks the
/// release itself, waits until it has no job, and starts again; a worker that is offline
/// does it when it is back.  (Workers older than 0.1.2 do not know the order and ignore it.)
pub fn ask_update(ctx: &Ctx, worker: &PairedWorker) -> Result<(), String> {
    let key = ctx.pair_key(worker).ok_or("the worker's key is damaged")?;
    let inbox = proto::inbox(&worker.id);
    let order = proto::order(proto::ORDER_UPDATE, j::obj([("version", j::s(bb_core::VERSION))]));
    ctx.relay.ch_append(&inbox, &crypt::seal(&key, &crypt::aad(&ctx.main.id, &inbox), j::text(&order).as_bytes()))?;
    Ok(())
}

/// Tell a worker to lend something else (an empty id is given one) -> the change's id, which
/// the worker's status names once it has handled it (WorkerStatus::offer_change, offer_error).
/// One that is offline takes it when it is back.
pub fn set_offer(ctx: &Ctx, worker: &PairedWorker, change: &mut OfferChange) -> Result<String, String> {
    if change.is_empty() {
        return Err(String::from("nothing to change"));
    }
    if change.id.is_empty() {
        change.id = bb_core::util::new_id("o");
    }
    let key = ctx.pair_key(worker).ok_or("the worker's key is damaged")?;
    let inbox = proto::inbox(&worker.id);
    let order = proto::order(proto::ORDER_OFFER, j::obj([("change", change.to_json())]));
    ctx.relay.ch_append(&inbox, &crypt::seal(&key, &crypt::aad(&ctx.main.id, &inbox), j::text(&order).as_bytes()))?;
    Ok(change.id.clone())
}

/// ask a worker to remove a project's files (`slot` "": all of its copies, and its jobs' folders)
pub fn clean(ctx: &Ctx, worker: &PairedWorker, project_id: &str, slot: &str) -> Result<(), String> {
    let key = ctx.pair_key(worker).ok_or("the worker's key is damaged")?;
    let inbox = proto::inbox(&worker.id);
    let order = proto::order(proto::ORDER_WS_REMOVE, j::obj([("project_id", j::s(project_id)), ("slot", j::s(slot))]));
    ctx.relay.ch_append(&inbox, &crypt::seal(&key, &crypt::aad(&ctx.main.id, &inbox), j::text(&order).as_bytes()))?;
    Ok(())
}
