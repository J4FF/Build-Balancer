// store -- what the relay keeps: the admitted devices, pairing slots, channels and blobs
//
//   <data>/relay.json          the relay's id and its devices (tokens as hashes)
//   <data>/setup.hash          the hash of an unused setup key (gone once a main machine took it)
//   <data>/tls.pem, tls.key    the relay's certificate and its key
//   <data>/channels/<name>.ch  one file per channel: records of number, time, length, bytes
//   <data>/blobs/ab/<id>       one file per blob
//
// Everything here is opaque to the relay: sealed messages and encrypted blobs.  Memory holds
// the index of each (numbers and offsets, ids and sizes), the bytes stay in the files.

use alloc::collections::BTreeMap;
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

use agent_core::record;
use bb_core::proto::Device;
use bb_core::util::now;
use plat::sync::{Condvar, Mutex};

record! {
    pub struct RelayFile {
        pub relay_id: String,
        pub created: i64,
        pub devices: Vec<Device>,
    }
}

/// the two messages of a pairing wait here
pub struct PairSlot {
    pub expires: i64,
    pub worker: Option<Vec<u8>>,
    pub main: Option<Vec<u8>>,
}

/// where a channel's messages lie in its file
pub struct Channel {
    pub file: String,
    pub next: u64,
    /// (number, offset of the bytes, their length)
    pub index: Vec<(u64, u64, u32)>,
    pub size: u64,
    pub touched: i64,
}

pub struct State {
    pub file: RelayFile,
    pub pairs: BTreeMap<String, PairSlot>,
    pub channels: BTreeMap<String, Channel>,
    /// id -> (bytes, last use)
    pub blobs: BTreeMap<[u8; 32], (u32, i64)>,
    pub blob_bytes: u64,
    pub seen: BTreeMap<String, i64>,
    /// device -> (sealed status, time)
    pub status: BTreeMap<String, (Vec<u8>, i64)>,
}

pub struct Store {
    pub dir: String,
    pub st: Mutex<State>,
    /// notified whenever a channel or a pairing slot got a message
    pub changed: Condvar,
    /// the most bytes of blobs kept
    pub blob_limit: u64,
    pub keep_days: i64,
}

/// the size of a channel record's head: number, time, length
const HEAD: u64 = 20;
/// the largest message a channel takes
pub const MSG_MAX: usize = 1 << 20;
/// the most a channel holds; beyond it appends are refused (a job's log is cut by its worker long before)
pub const CHANNEL_MAX: u64 = 256 << 20;

/// whether a channel name is one: "<device id>/<topic>" of lower-case letters, digits, '-', '_', '/'
pub fn channel_ok(name: &str) -> bool {
    let Some((owner, topic)) = name.split_once('/') else { return false };
    bb_core::util::id_ok(owner)
        && !topic.is_empty()
        && name.len() <= 120
        && topic.split('/').all(|p| !p.is_empty() && p.bytes().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == b'-' || c == b'_'))
}

impl Store {
    pub fn path(&self, name: &str) -> String {
        plat::fs::join(&self.dir, name)
    }

    /// open (or start) the data folder
    pub fn open(dir: &str, blob_limit: u64, keep_days: i64) -> Result<Store, String> {
        for d in ["", "channels", "blobs"] {
            let p = if d.is_empty() { String::from(dir) } else { plat::fs::join(dir, d) };
            if !plat::fs::mkdir_private(&p) && plat::file_exists(&p) != 2 {
                return Err(format!("{} cannot be created or belongs to someone else", p));
            }
        }
        let path = plat::fs::join(dir, "relay.json");
        let file = match bb_core::j::load::<RelayFile>(&path) {
            Some(f) if !f.relay_id.is_empty() => f,
            _ if plat::file_exists(&path) != 0 => return Err(format!("{} is damaged; move it away to start a new relay", path)),
            _ => {
                let f = RelayFile { relay_id: bb_core::util::new_id("r"), created: now(), devices: Vec::new() };
                if !bb_core::j::save(&path, &f) {
                    return Err(format!("{} cannot be written", path));
                }
                f
            }
        };
        let mut st = State { file, pairs: BTreeMap::new(), channels: BTreeMap::new(), blobs: BTreeMap::new(), blob_bytes: 0, seen: BTreeMap::new(), status: BTreeMap::new() };
        load_channels(&plat::fs::join(dir, "channels"), &mut st);
        load_blobs(&plat::fs::join(dir, "blobs"), &mut st);
        Ok(Store { dir: String::from(dir), st: Mutex::new(st), changed: Condvar::new(), blob_limit, keep_days })
    }

    pub fn save(&self, st: &State) -> bool {
        bb_core::j::save(&self.path("relay.json"), &st.file)
    }

    fn channel_file(&self, name: &str) -> String {
        plat::fs::join(&self.path("channels"), &format!("{}.ch", crypto::hex(&crypto::blake3::hash(name.as_bytes())[..16])))
    }

    /// add a message -> its number (None: the channel is full or the file cannot be written)
    pub fn append(&self, st: &mut State, name: &str, data: &[u8]) -> Option<u64> {
        let t = now();
        if !st.channels.contains_key(name) {
            let file = self.channel_file(name);
            // the first record names the channel, so the files explain themselves at the next start
            let mut rec = record_bytes(0, t, name.as_bytes());
            rec.shrink_to_fit();
            if !bb_core::util::put_file(&file, &rec) {
                return None;
            }
            st.channels.insert(String::from(name), Channel { file, next: 1, index: Vec::new(), size: rec.len() as u64, touched: t });
        }
        let ch = st.channels.get_mut(name)?;
        if ch.size + data.len() as u64 > CHANNEL_MAX {
            return None;
        }
        let seq = ch.next;
        let f = plat::File::open(&ch.file, plat::FO_RW)?;
        let rec = record_bytes(seq, t, data);
        if f.write_at(ch.size, &rec) != Some(rec.len()) {
            return None;
        }
        ch.index.push((seq, ch.size + HEAD, data.len() as u32));
        ch.size += rec.len() as u64;
        ch.next += 1;
        ch.touched = t;
        Some(seq)
    }

    /// The messages after number `after`, at most `max_bytes` of them, as the answer's frames
    /// (number, length, bytes).  An inbox whose reader has passed most of it is rewritten
    /// without what was read.
    pub fn read(&self, st: &mut State, name: &str, after: u64, max_bytes: usize) -> Vec<u8> {
        let mut out = Vec::new();
        let Some(ch) = st.channels.get_mut(name) else { return out };
        let from = ch.index.partition_point(|e| e.0 <= after);
        if from < ch.index.len() {
            if let Some(f) = plat::File::open(&ch.file, plat::FO_READ) {
                for &(seq, off, len) in &ch.index[from..] {
                    if !out.is_empty() && out.len() + len as usize > max_bytes {
                        break;
                    }
                    let mut buf = alloc::vec![0u8; len as usize];
                    if read_full(&f, off, &mut buf) {
                        out.extend_from_slice(&seq.to_le_bytes());
                        out.extend_from_slice(&len.to_le_bytes());
                        out.extend_from_slice(&buf);
                    }
                }
            }
            // The last access outlives a restart: now and then it is written into the time of
            // the channel's first record (the one with its name), which loading reads back.
            let t = now();
            if t - ch.touched > 3600
                && let Some(f) = plat::File::open(&ch.file, plat::FO_RW)
            {
                let _ = f.write_at(8, &t.to_le_bytes());
            }
            ch.touched = t;
        }
        if name.ends_with("/in") && from > 64 && from * 2 > ch.index.len() {
            compact(ch, name, from);
        }
        out
    }

    pub fn has_after(st: &State, name: &str, after: u64) -> bool {
        st.channels.get(name).is_some_and(|c| c.index.last().is_some_and(|e| e.0 > after))
    }

    pub fn remove_channel(&self, st: &mut State, name: &str) -> bool {
        match st.channels.remove(name) {
            Some(c) => {
                plat::delete(&c.file);
                true
            }
            None => false,
        }
    }

    pub fn blob_path(&self, id: &[u8; 32]) -> String {
        let h = crypto::hex(id);
        plat::fs::join(&plat::fs::join(&self.path("blobs"), &h[..2]), &h)
    }

    /// keep a blob (its file is written before the lock is taken)
    pub fn blob_note(&self, st: &mut State, id: [u8; 32], len: u32) {
        if let Some(old) = st.blobs.insert(id, (len, now())) {
            st.blob_bytes -= old.0 as u64;
        }
        st.blob_bytes += len as u64;
    }

    /// drop the blobs unused the longest until the store is below 90 % of its limit -> how many went
    pub fn evict(&self, st: &mut State) -> usize {
        if st.blob_bytes <= self.blob_limit {
            return 0;
        }
        let mut order: Vec<([u8; 32], u32, i64)> = st.blobs.iter().map(|(k, v)| (*k, v.0, v.1)).collect();
        order.sort_by_key(|e| e.2);
        let goal = self.blob_limit / 10 * 9;
        let mut gone = 0;
        for (id, len, _) in order {
            if st.blob_bytes <= goal {
                break;
            }
            plat::delete(&self.blob_path(&id));
            st.blobs.remove(&id);
            st.blob_bytes -= len as u64;
            gone += 1;
        }
        gone
    }

    /// what time does: pairing slots end, channels nobody touched for keep_days go
    pub fn sweep(&self, st: &mut State) {
        let t = now();
        st.pairs.retain(|_, p| p.expires > t);
        let old: Vec<String> = st.channels.iter().filter(|(n, c)| !n.ends_with("/in") && t - c.touched > self.keep_days * 86_400).map(|(n, _)| n.clone()).collect();
        for n in old {
            self.remove_channel(st, &n);
        }
    }
}

fn record_bytes(seq: u64, time: i64, data: &[u8]) -> Vec<u8> {
    let mut r = Vec::with_capacity(HEAD as usize + data.len());
    r.extend_from_slice(&seq.to_le_bytes());
    r.extend_from_slice(&time.to_le_bytes());
    r.extend_from_slice(&(data.len() as u32).to_le_bytes());
    r.extend_from_slice(data);
    r
}

fn read_full(f: &plat::File, mut off: u64, mut buf: &mut [u8]) -> bool {
    while !buf.is_empty() {
        match f.read_at(off, buf) {
            Some(n) if n > 0 => {
                off += n as u64;
                buf = &mut buf[n..];
            }
            _ => return false,
        }
    }
    true
}

/// rewrite an inbox without its first `drop` messages (their reader has them)
fn compact(ch: &mut Channel, name: &str, drop: usize) {
    let Some(f) = plat::File::open(&ch.file, plat::FO_READ) else { return };
    let mut data = record_bytes(0, now(), name.as_bytes());
    let mut index = Vec::new();
    for &(seq, off, len) in &ch.index[drop..] {
        let mut buf = alloc::vec![0u8; len as usize];
        if !read_full(&f, off, &mut buf) {
            return;
        }
        let rec = record_bytes(seq, now(), &buf);
        index.push((seq, data.len() as u64 + HEAD, len));
        data.extend_from_slice(&rec);
    }
    drop_file(f);
    if bb_core::util::put_file(&ch.file, &data) {
        ch.index = index;
        ch.size = data.len() as u64;
    }
}

fn drop_file(f: plat::File) {
    let _ = f;
}

fn load_channels(dir: &str, st: &mut State) {
    for e in plat::fs::read_dir(dir).unwrap_or_default() {
        if !e.name.ends_with(".ch") {
            continue;
        }
        let file = plat::fs::join(dir, &e.name);
        let Some(f) = plat::File::open(&file, plat::FO_READ) else { continue };
        let size = f.size().unwrap_or(0);
        let mut off = 0u64;
        let mut name = String::new();
        let mut ch = Channel { file: file.clone(), next: 1, index: Vec::new(), size: 0, touched: 0 };
        while off + HEAD <= size {
            let mut head = [0u8; HEAD as usize];
            if !read_full(&f, off, &mut head) {
                break;
            }
            let seq = u64::from_le_bytes(head[0..8].try_into().unwrap_or([0; 8]));
            let time = i64::from_le_bytes(head[8..16].try_into().unwrap_or([0; 8]));
            let len = u32::from_le_bytes(head[16..20].try_into().unwrap_or([0; 4]));
            if len as usize > MSG_MAX || off + HEAD + len as u64 > size {
                break; // a record cut short by a crash: what came before it counts
            }
            if seq == 0 {
                let mut b = alloc::vec![0u8; len as usize];
                if read_full(&f, off + HEAD, &mut b) {
                    name = String::from_utf8_lossy(&b).into_owned();
                }
            } else {
                ch.index.push((seq, off + HEAD, len));
                ch.next = seq + 1;
            }
            ch.touched = ch.touched.max(time);
            off += HEAD + len as u64;
        }
        ch.size = off;
        if channel_ok(&name) {
            st.channels.insert(name, ch);
        } else {
            drop_file(f);
            plat::delete(&file);
        }
    }
}

fn load_blobs(dir: &str, st: &mut State) {
    for sub in plat::fs::read_dir(dir).unwrap_or_default() {
        let d = plat::fs::join(dir, &sub.name);
        for e in plat::fs::read_dir(&d).unwrap_or_default() {
            let p = plat::fs::join(&d, &e.name);
            match (bb_core::crypt::key32(&e.name), plat::fs::stat(&p)) {
                (Some(id), Some(m)) if m.is_file() => {
                    st.blobs.insert(id, (m.size as u32, m.mtime_ns.div_euclid(1_000_000_000)));
                    st.blob_bytes += m.size;
                }
                // a temporary file a crash left behind
                _ => {
                    plat::delete(&p);
                }
            }
        }
    }
}
