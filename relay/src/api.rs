// api -- the relay's HTTP interface (bb_core::client is its other side)
//
//   GET    /v1/info                         anyone      what this relay is
//   POST   /v1/claim                        setup key   become this relay's main machine
//   GET    /v1/devices                      main        the admitted devices, last seen, sealed status
//   POST   /v1/devices                      main        admit a device (a paired worker)
//   DELETE /v1/devices/<id>                 main        remove one, with its channels
//   POST   /v1/pair/<slot>/open             main        open a pairing slot
//   POST   /v1/pair/<slot>/worker           the slot    the worker's sealed hello
//   GET    /v1/pair/<slot>/worker           main        ... read by the main machine
//   POST   /v1/pair/<slot>/main             main        the main machine's sealed welcome
//   GET    /v1/pair/<slot>/main             the slot    ... read by the worker
//   PUT    /v1/status                       device      its sealed status
//   POST   /v1/ch/<owner>/<topic>           device      add a message
//   GET    /v1/ch/<owner>/<topic>           owner, main the messages after ?after=N (long poll ?wait=S)
//   DELETE /v1/ch/<owner>/<topic>           owner, main
//   POST   /v1/blobs/has | put | get        device      blobs by id
//   GET    /v1/stats                        main        what the relay holds
//
// "the slot": whoever knows a slot's 128-bit name, which only the pairing code gives.
// Errors are {"error": code, "message": text} with the fitting status.

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

use bb_core::j::{self, Json, Value};
use bb_core::proto::{Device, RelayInfo, ROLE_MAIN, ROLE_WORKER};
use bb_core::util::now;
use netio::{Request, Response};

use crate::store::{channel_ok, PairSlot, Store, MSG_MAX};

fn error(status: u16, code: &str, message: &str) -> Response {
    Response::json(status, &j::obj([("error", j::s(code)), ("message", j::s(message))]))
}

fn ok() -> Response {
    Response::json(200, &j::obj([("ok", Value::Bool(true))]))
}

fn bytes(b: Vec<u8>) -> Response {
    Response::bytes(200, "application/octet-stream", b)
}

/// who asks: (device id, role), or the answer that turns them away
fn auth(store: &Store, req: &Request) -> Result<(String, String), Response> {
    let deny = || error(401, "unauthorized", "this device is not admitted at the relay (removed, or never paired)");
    let (id, token) = req.bearer().and_then(|b| b.split_once('.')).ok_or_else(deny)?;
    let hash = bb_core::crypt::token_hash(token);
    let mut st = store.st.lock();
    let role = st.file.devices.iter().find(|d| d.id == id && crypto::ct_eq(d.token_hash.as_bytes(), hash.as_bytes())).map(|d| d.role.clone()).ok_or_else(deny)?;
    st.seen.insert(String::from(id), now());
    Ok((String::from(id), role))
}

fn main_only(store: &Store, req: &Request) -> Result<String, Response> {
    let (id, role) = auth(store, req)?;
    if role != ROLE_MAIN {
        return Err(error(403, "forbidden", "only the main machine may do this"));
    }
    Ok(id)
}

fn wait_secs(req: &Request) -> u32 {
    req.query("wait").and_then(|w| w.parse::<u32>().ok()).unwrap_or(0).min(50)
}

pub fn handle(store: &Store, req: &Request) -> Response {
    let path = String::from(req.path());
    let parts: Vec<&str> = path.split('/').filter(|p| !p.is_empty()).collect();
    let m = req.method.as_str();
    match (m, parts.as_slice()) {
        ("GET", []) => Response::text(200, &format!("{} relay {}\nIt passes sealed data between the machines that paired through it.\n", bb_core::PRODUCT, bb_core::VERSION)),
        ("GET", ["v1", "info"]) => info(store),
        ("POST", ["v1", "claim"]) => claim(store, req),
        ("GET", ["v1", "devices"]) => devices(store, req),
        ("POST", ["v1", "devices"]) => device_add(store, req),
        ("DELETE", ["v1", "devices", id]) => device_remove(store, req, id),
        ("POST", ["v1", "pair", slot, "open"]) => pair_open(store, req, slot),
        ("POST", ["v1", "pair", slot, side]) => pair_put(store, req, slot, side),
        ("GET", ["v1", "pair", slot, side]) => pair_get(store, req, slot, side),
        ("PUT", ["v1", "status"]) => status_put(store, req),
        ("POST", ["v1", "blobs", "has"]) => blobs_has(store, req),
        ("POST", ["v1", "blobs", "put"]) => blobs_put(store, req),
        ("POST", ["v1", "blobs", "get"]) => blobs_get(store, req),
        ("GET", ["v1", "stats"]) => stats(store, req),
        (_, ["v1", "ch", ..]) => {
            let name = parts[2..].join("/");
            match m {
                "POST" => ch_append(store, req, &name),
                "GET" => ch_read(store, req, &name),
                "DELETE" => ch_delete(store, req, &name),
                _ => error(405, "method", "POST, GET or DELETE"),
            }
        }
        _ => error(404, "not_found", "no such path"),
    }
}

fn info(store: &Store) -> Response {
    let st = store.st.lock();
    let i = RelayInfo {
        product: String::from(bb_core::PRODUCT),
        version: String::from(bb_core::VERSION),
        protocol: bb_core::PROTOCOL,
        relay_id: st.file.relay_id.clone(),
        claimed: st.file.devices.iter().any(|d| d.role == ROLE_MAIN),
        time: now(),
    };
    Response::json(200, &i.to_json())
}

fn claim(store: &Store, req: &Request) -> Response {
    let Some(v) = req.json(j::big()) else { return error(400, "bad_request", "a JSON body is expected") };
    let (setup, id, name) = (j::str_of(&v, "setup"), j::str_of(&v, "id"), j::str_of(&v, "name"));
    if !bb_core::util::id_ok(id) || name.is_empty() || name.len() > 100 {
        return error(400, "bad_request", "id and name are needed");
    }
    // read at every claim: `bb-relay setup-key` writes a new one while the relay runs
    let hash_file = store.path("setup.hash");
    let want = plat::read_file(&hash_file).map(|b| String::from(String::from_utf8_lossy(&b).trim())).unwrap_or_default();
    if want.is_empty() || !crypto::ct_eq(want.as_bytes(), bb_core::crypt::token_hash(setup).as_bytes()) {
        return error(403, "setup_key", "this setup key is not valid (used already, or of another relay); `bb-relay setup-key` on the server makes a new one");
    }
    let token = bb_core::crypt::token_new();
    let mut st = store.st.lock();
    st.file.devices.retain(|d| d.id != id);
    st.file.devices.push(Device { id: String::from(id), name: String::from(name), role: String::from(ROLE_MAIN), token_hash: bb_core::crypt::token_hash(&token), added: now(), ..Device::default() });
    if !store.save(&st) {
        return error(500, "storage", "the relay cannot write its data folder");
    }
    plat::delete(&hash_file);
    plat::log_info("relay", &format!("main machine {} ({}) took this relay", name, id));
    Response::json(200, &j::obj([("token", j::s(&token))]))
}

fn devices(store: &Store, req: &Request) -> Response {
    if let Err(r) = main_only(store, req) {
        return r;
    }
    let st = store.st.lock();
    let list: Vec<Device> = st
        .file
        .devices
        .iter()
        .map(|d| {
            let (status, status_time) = st.status.get(&d.id).map(|(b, t)| (crypto::b64::encode(b), *t)).unwrap_or_default();
            Device { token_hash: String::new(), last_seen: st.seen.get(&d.id).copied().unwrap_or(0), status, status_time, ..d.clone() }
        })
        .collect();
    Response::json(200, &j::obj([("devices", j::list(&list)), ("time", Value::i64(now()))]))
}

fn device_add(store: &Store, req: &Request) -> Response {
    if let Err(r) = main_only(store, req) {
        return r;
    }
    let Some(d) = req.json(j::big()).and_then(|v| Device::from_json(&v)) else { return error(400, "bad_request", "a device is expected") };
    if !bb_core::util::id_ok(&d.id) || d.name.is_empty() || d.name.len() > 100 || d.token_hash.len() != 64 || (d.role != ROLE_WORKER && d.role != ROLE_MAIN) {
        return error(400, "bad_request", "a device needs an id, a name, a role and its token's hash");
    }
    let mut st = store.st.lock();
    if st.file.devices.len() >= 256 {
        return error(409, "limit", "this relay holds 256 devices at most");
    }
    st.file.devices.retain(|x| x.id != d.id);
    plat::log_info("relay", &format!("{} {} ({}) admitted", d.role, d.name, d.id));
    st.file.devices.push(Device { added: now(), last_seen: 0, status: String::new(), status_time: 0, ..d });
    if store.save(&st) { ok() } else { error(500, "storage", "the relay cannot write its data folder") }
}

fn device_remove(store: &Store, req: &Request, id: &str) -> Response {
    let me = match main_only(store, req) {
        Ok(m) => m,
        Err(r) => return r,
    };
    if me == id {
        return error(409, "self", "a main machine does not remove itself");
    }
    let mut st = store.st.lock();
    let before = st.file.devices.len();
    st.file.devices.retain(|d| d.id != id);
    if st.file.devices.len() == before {
        return error(404, "not_found", "no such device");
    }
    st.status.remove(id);
    st.seen.remove(id);
    let prefix = format!("{}/", id);
    let gone: Vec<String> = st.channels.keys().filter(|n| n.starts_with(&prefix)).cloned().collect();
    for n in gone {
        store.remove_channel(&mut st, &n);
    }
    plat::log_info("relay", &format!("device {} removed", id));
    if store.save(&st) { ok() } else { error(500, "storage", "the relay cannot write its data folder") }
}

fn slot_ok(slot: &str) -> bool {
    slot.len() == 32 && slot.bytes().all(|c| c.is_ascii_hexdigit())
}

fn pair_open(store: &Store, req: &Request, slot: &str) -> Response {
    if let Err(r) = main_only(store, req) {
        return r;
    }
    if !slot_ok(slot) {
        return error(400, "bad_request", "a slot is 32 hex characters");
    }
    let ttl = req.json(j::big()).map(|v| j::u64_of(&v, "ttl")).unwrap_or(0).clamp(60, 3600) as i64;
    let mut st = store.st.lock();
    store.sweep(&mut st);
    if st.pairs.len() >= 64 {
        return error(409, "limit", "too many open pairings");
    }
    st.pairs.insert(String::from(slot), PairSlot { expires: now() + ttl, worker: None, main: None });
    ok()
}

fn pair_put(store: &Store, req: &Request, slot: &str, side: &str) -> Response {
    if side != "worker" && side != "main" {
        return error(404, "not_found", "no such path");
    }
    if side == "main" {
        if let Err(r) = main_only(store, req) {
            return r;
        }
    }
    if req.body.is_empty() || req.body.len() > 16 << 10 {
        return error(413, "too_large", "a pairing message has at most 16 KiB");
    }
    let mut st = store.st.lock();
    let t = now();
    let Some(p) = st.pairs.get_mut(slot).filter(|p| p.expires > t) else {
        return error(404, "no_slot", "no pairing is open for this code (mistyped, expired, or `bb pair` is not running)");
    };
    let place = if side == "worker" { &mut p.worker } else { &mut p.main };
    if place.is_some() {
        // one hello per slot: a second machine with the same code is turned away
        return error(409, "taken", "this pairing code was used already");
    }
    *place = Some(req.body.clone());
    drop(st);
    store.changed.notify_all();
    ok()
}

fn pair_get(store: &Store, req: &Request, slot: &str, side: &str) -> Response {
    if side != "worker" && side != "main" {
        return error(404, "not_found", "no such path");
    }
    // the worker's hello is for the main machine; the welcome is read by whoever knows the slot
    if side == "worker" {
        if let Err(r) = main_only(store, req) {
            return r;
        }
    }
    let deadline = bb_core::util::tick_ms() + wait_secs(req) as u64 * 1000;
    let mut st = store.st.lock();
    loop {
        let t = now();
        let Some(p) = st.pairs.get(slot).filter(|p| p.expires > t) else {
            return error(404, "no_slot", "no pairing is open for this code (mistyped or expired)");
        };
        if let Some(b) = if side == "worker" { &p.worker } else { &p.main } {
            let b = b.clone();
            if side == "main" {
                // the welcome was fetched: the pairing is over
                st.pairs.remove(slot);
            }
            return bytes(b);
        }
        let left = deadline.saturating_sub(bb_core::util::tick_ms());
        if left == 0 || plat::service::stop_requested() {
            return Response::new(204);
        }
        st = store.changed.wait_timeout(st, Some(left.min(1000) as u32)).0;
    }
}

fn status_put(store: &Store, req: &Request) -> Response {
    let (id, _) = match auth(store, req) {
        Ok(a) => a,
        Err(r) => return r,
    };
    if req.body.len() > 256 << 10 {
        return error(413, "too_large", "a status has at most 256 KiB");
    }
    store.st.lock().status.insert(id, (req.body.clone(), now()));
    ok()
}

fn ch_append(store: &Store, req: &Request, name: &str) -> Response {
    if let Err(r) = auth(store, req) {
        return r;
    }
    if !channel_ok(name) {
        return error(400, "bad_request", "a channel is <device id>/<topic>");
    }
    if req.body.is_empty() || req.body.len() > MSG_MAX {
        return error(413, "too_large", "a message has 1 byte to 1 MiB");
    }
    let mut st = store.st.lock();
    let owner = name.split('/').next().unwrap_or("");
    if !st.file.devices.iter().any(|d| d.id == owner) {
        return error(404, "not_found", "the channel's device is not admitted here");
    }
    if !st.channels.contains_key(name) && st.channels.len() >= 4096 {
        return error(409, "limit", "too many channels");
    }
    let Some(seq) = store.append(&mut st, name, &req.body) else { return error(507, "full", "the channel is full or cannot be written") };
    drop(st);
    store.changed.notify_all();
    Response::json(200, &j::obj([("seq", j::n(seq))]))
}

/// the owner of a channel and every main machine may read and delete it
fn ch_access(store: &Store, req: &Request, name: &str) -> Result<String, Response> {
    let (id, role) = auth(store, req)?;
    if !channel_ok(name) {
        return Err(error(400, "bad_request", "a channel is <device id>/<topic>"));
    }
    if role != ROLE_MAIN && name.split('/').next() != Some(id.as_str()) {
        return Err(error(403, "forbidden", "this channel belongs to another device"));
    }
    Ok(id)
}

fn ch_read(store: &Store, req: &Request, name: &str) -> Response {
    let id = match ch_access(store, req, name) {
        Ok(i) => i,
        Err(r) => return r,
    };
    let after = req.query("after").and_then(|a| a.parse::<u64>().ok()).unwrap_or(0);
    let deadline = bb_core::util::tick_ms() + wait_secs(req) as u64 * 1000;
    let mut st = store.st.lock();
    loop {
        if Store::has_after(&st, name, after) {
            return bytes(store.read(&mut st, name, after, 8 << 20));
        }
        let left = deadline.saturating_sub(bb_core::util::tick_ms());
        if left == 0 || plat::service::stop_requested() {
            return bytes(Vec::new());
        }
        st = store.changed.wait_timeout(st, Some(left.min(1000) as u32)).0;
        // a device that waits here is online
        st.seen.insert(id.clone(), now());
    }
}

fn ch_delete(store: &Store, req: &Request, name: &str) -> Response {
    if let Err(r) = ch_access(store, req, name) {
        return r;
    }
    store.remove_channel(&mut store.st.lock(), name);
    ok()
}

fn ids_of(body: &[u8]) -> Option<Vec<[u8; 32]>> {
    (body.len() % 32 == 0 && body.len() <= 32 * 8192).then(|| body.chunks_exact(32).map(|c| c.try_into().unwrap_or([0; 32])).collect())
}

fn blobs_has(store: &Store, req: &Request) -> Response {
    if let Err(r) = auth(store, req) {
        return r;
    }
    let Some(ids) = ids_of(&req.body) else { return error(400, "bad_request", "ids of 32 bytes each, 8192 at most") };
    let t = now();
    let mut st = store.st.lock();
    let out: Vec<u8> = ids
        .iter()
        .map(|id| match st.blobs.get_mut(id) {
            Some(b) => {
                b.1 = t;
                1
            }
            None => 0,
        })
        .collect();
    bytes(out)
}

fn blobs_put(store: &Store, req: &Request) -> Response {
    if let Err(r) = auth(store, req) {
        return r;
    }
    let b = &req.body;
    // check the whole pack before any of it is kept
    let mut items: Vec<([u8; 32], &[u8])> = Vec::new();
    let mut p = 0;
    while p < b.len() {
        if p + 36 > b.len() {
            return error(400, "bad_request", "a pack is id, length, bytes, again and again");
        }
        let id: [u8; 32] = b[p..p + 32].try_into().unwrap_or([0; 32]);
        let len = u32::from_le_bytes(b[p + 32..p + 36].try_into().unwrap_or([0; 4])) as usize;
        p += 36;
        if len == 0 || len > bb_core::CHUNK + (1 << 16) || p + len > b.len() {
            return error(400, "bad_request", "a blob in the pack is cut short or too large");
        }
        items.push((id, &b[p..p + len]));
        p += len;
    }
    for (id, data) in &items {
        if store.st.lock().blobs.contains_key(id) {
            continue;
        }
        if !bb_core::util::put_file(&store.blob_path(id), data) {
            return error(507, "storage", "the relay cannot store the blob (disk full?)");
        }
    }
    let mut st = store.st.lock();
    for (id, data) in &items {
        store.blob_note(&mut st, *id, data.len() as u32);
    }
    let gone = store.evict(&mut st);
    if gone > 0 {
        plat::log_info("relay", &format!("store over its limit: {} blobs unused the longest dropped", gone));
    }
    Response::json(200, &j::obj([("stored", j::n(items.len() as u64))]))
}

fn blobs_get(store: &Store, req: &Request) -> Response {
    if let Err(r) = auth(store, req) {
        return r;
    }
    let Some(ids) = ids_of(&req.body) else { return error(400, "bad_request", "ids of 32 bytes each, 8192 at most") };
    let t = now();
    let known: Vec<Option<u32>> = {
        let mut st = store.st.lock();
        ids.iter()
            .map(|id| {
                st.blobs.get_mut(id).map(|b| {
                    b.1 = t;
                    b.0
                })
            })
            .collect()
    };
    let total: u64 = known.iter().map(|k| k.unwrap_or(0) as u64).sum();
    if total > 48 << 20 {
        return error(413, "too_large", "ask for less at once (48 MiB of blobs at most)");
    }
    let mut out = Vec::with_capacity(total as usize + ids.len() * 36);
    for (id, k) in ids.iter().zip(&known) {
        out.extend_from_slice(id);
        match k.and_then(|_| plat::read_file(&store.blob_path(id))) {
            Some(data) => {
                out.extend_from_slice(&(data.len() as u32).to_le_bytes());
                out.extend_from_slice(&data);
            }
            None => out.extend_from_slice(&u32::MAX.to_le_bytes()),
        }
    }
    bytes(out)
}

fn stats(store: &Store, req: &Request) -> Response {
    if let Err(r) = main_only(store, req) {
        return r;
    }
    let st = store.st.lock();
    let ch_bytes: u64 = st.channels.values().map(|c| c.size).sum();
    let (free, total) = plat::fs::free_space(&store.dir).unwrap_or((0, 0));
    Response::json(
        200,
        &j::obj([
            ("version", j::s(bb_core::VERSION)),
            ("relay_id", j::s(&st.file.relay_id)),
            ("devices", j::n(st.file.devices.len() as u64)),
            ("blobs", j::n(st.blobs.len() as u64)),
            ("blob_bytes", j::n(st.blob_bytes)),
            ("blob_limit", j::n(store.blob_limit)),
            ("channels", j::n(st.channels.len() as u64)),
            ("channel_bytes", j::n(ch_bytes)),
            ("disk_free", j::n(free)),
            ("disk_total", j::n(total)),
            ("time", Value::i64(now())),
        ]),
    )
}
