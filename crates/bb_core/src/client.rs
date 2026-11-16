// client -- the relay's interface from a program's side
//
//   Relay::open(addr, fingerprint, device id, token)   a client for one relay; https pinned to the
//                                relay's certificate (its SHA-256), http only when the address says so
//   Relay::probe(addr)           the relay's info and the certificate it shows, before anything is
//                                trusted (a worker's first contact; the pairing then proves it)
//   info / claim / devices / device_add / device_remove
//   pair_open / pair_put / pair_get                    the two messages of a pairing
//   status_put                   a worker's sealed status
//   ch_append / ch_read / ch_delete                    channels: sealed messages in order, long polls
//   blobs_has / blobs_put / blobs_get                  content by id, in packs
//
// One Relay holds one kept-alive connection behind a lock; a thread that waits in a long poll
// takes its own with fork().  Every call is blocking with timeouts.

use alloc::boxed::Box;
use alloc::format;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::fmt;

use netio::client::{Connector, Session};
use netio::tls::{self, ClientConfig, Trust};
use netio::{Client, Headers, HttpError, Stream, Url};
use plat::net::TcpStream;
use plat::sync::Mutex;

use crate::j::{self, Json, Value};
use crate::proto::{Device, RelayInfo};

/// why a call failed: `status` is the relay's answer (0: it was not reached)
#[derive(Debug, Clone)]
pub struct RelayError {
    pub status: u16,
    pub text: String,
}

impl fmt::Display for RelayError {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        if self.status == 0 { write!(f, "relay not reachable: {}", self.text) } else { write!(f, "relay answers {}: {}", self.status, self.text) }
    }
}

impl From<RelayError> for String {
    fn from(e: RelayError) -> String {
        format!("{}", e)
    }
}

pub type RelayResult<T> = Result<T, RelayError>;

fn fail<T>(status: u16, text: impl Into<String>) -> RelayResult<T> {
    Err(RelayError { status, text: text.into() })
}

/// An address as people type it ("relay.example.org", "10.0.0.5:7700", "https://host:port",
/// "http://127.0.0.1:7700") -> "scheme://host:port".  https unless http is spelled out.
/// What to tell someone whose first contact with a relay failed: a relay that runs behind a
/// closed port looks healthy on the server and is not there for anyone else.
pub fn unreachable_text(addr: &str, detail: &str) -> String {
    let shown = addr.trim_start_matches("https://").trim_end_matches('/');
    let port = shown.rsplit_once(':').and_then(|(_, p)| p.parse::<u16>().ok()).unwrap_or(crate::DEFAULT_PORT);
    format!(
        "the relay at {shown} cannot be reached ({detail}).  Check, in this order:\n  1. Does it run?  On the server: bb-relay service status\n  2. Is TCP port {port} open in the server's firewall?  With ufw: sudo ufw allow {port}/tcp\n  3. At a hoster: is port {port} open in its firewall (security group) as well?\n  4. Is {shown} the address this machine reaches the server by?"
    )
}

pub fn normalize_addr(addr: &str) -> Option<String> {
    let a = addr.trim().trim_end_matches('/');
    let (scheme, rest) = match a.split_once("://") {
        Some((s, r)) => (s.to_ascii_lowercase(), r),
        None => (String::from("https"), a),
    };
    if scheme != "http" && scheme != "https" || rest.is_empty() || rest.contains(['/', '@', ' ', '?', '#']) {
        return None;
    }
    let (host, port) = if let Some(r) = rest.strip_prefix('[') {
        let (h, p) = r.split_once(']')?;
        (format!("[{}]", h), p.strip_prefix(':'))
    } else if rest.matches(':').count() == 1 {
        let (h, p) = rest.split_once(':')?;
        (String::from(h), Some(p))
    } else if rest.contains(':') {
        (format!("[{}]", rest), None) // a bare IPv6 address
    } else {
        (String::from(rest), None)
    };
    let port: u16 = match port {
        Some(p) => p.parse().ok()?,
        None => crate::DEFAULT_PORT,
    };
    Some(format!("{}://{}:{}", scheme, host, port))
}

/// the TLS layer: exactly the pinned certificate, or any (to learn it); the certificate seen
/// lands in `seen`
fn connector(pin: Option<[u8; 32]>, seen: Arc<Mutex<Option<[u8; 32]>>>) -> Connector {
    Arc::new(move |_url: &Url, tcp: TcpStream| {
        let _ = tcp.set_nodelay(true);
        let trust = match pin {
            Some(p) => Trust::Pinned(alloc::vec![p]),
            None => Trust::Any,
        };
        // a pinned certificate needs no name: the relay is whoever holds its key
        let cfg = ClientConfig { trust, identity: None, server_name: String::new() };
        let s = tls::connect(Box::new(tcp), cfg).map_err(|e| HttpError::Tls(format!("{}", e)))?;
        if let Some(c) = s.peer_certs().first() {
            *seen.lock() = Some(crypto::sha256(&c.der));
        }
        Ok(Box::new(s) as Box<dyn Stream>)
    })
}

pub struct Relay {
    base: Url,
    pin: Option<[u8; 32]>,
    auth: String,
    session: Mutex<Session>,
    seen: Arc<Mutex<Option<[u8; 32]>>>,
}

impl Relay {
    /// A client for the relay at `addr`.  `fingerprint`: hex of the SHA-256 of its certificate
    /// ("" only for http).  `device` and `token`: who is asking ("" for the calls anyone may make).
    pub fn open(addr: &str, fingerprint: &str, device: &str, token: &str) -> Result<Relay, String> {
        let norm = normalize_addr(addr).ok_or_else(|| format!("{} is not a relay address (host:port)", addr))?;
        let base = Url::parse(&format!("{}/", norm)).ok_or_else(|| format!("{} is not a relay address", addr))?;
        let pin = match (base.is_https(), fingerprint.is_empty()) {
            (true, true) => return Err(String::from("an https relay needs its certificate fingerprint")),
            (true, false) => Some(crate::crypt::key32(fingerprint).ok_or("the relay's fingerprint is damaged")?),
            (false, _) => None,
        };
        let auth = if device.is_empty() { String::new() } else { format!("{}.{}", device, token) };
        Ok(Relay::build(base, pin, auth, false))
    }

    fn build(base: Url, pin: Option<[u8; 32]>, auth: String, learn: bool) -> Relay {
        let seen = Arc::new(Mutex::new(None));
        let mut client = Client::new();
        if base.is_https() {
            client = client.with_connector(connector(if learn { None } else { pin }, seen.clone()));
        }
        client.connect_ms = 8000;
        client.timeout_ms = 90_000;
        client.max_body = 96 << 20;
        client.user_agent = format!("build-balancer/{}", crate::VERSION);
        Relay { session: Mutex::new(Session::new(client, base.clone())), base, pin, auth, seen }
    }

    /// The relay's info for a first contact (setup, pairing).  A relay that does not answer
    /// there is mostly a closed port, and the error says what to look at.
    pub fn first_info(&self) -> Result<RelayInfo, String> {
        self.info().map_err(|e| if e.status == 0 { unreachable_text(&self.address(), &e.text) } else { format!("{}", e) })
    }

    /// The relay's info and the certificate it shows (hex; "" over http), without trusting it:
    /// what comes back is a claim until a pairing or a setup key confirms the fingerprint.
    pub fn probe(addr: &str) -> Result<(RelayInfo, String), String> {
        let norm = normalize_addr(addr).ok_or_else(|| format!("{} is not a relay address (host:port)", addr))?;
        let base = Url::parse(&format!("{}/", norm)).ok_or_else(|| format!("{} is not a relay address", addr))?;
        let r = Relay::build(base, None, String::new(), true);
        let info = r.first_info()?;
        let fp = r.seen.lock().map(|d| crypto::hex(&d)).unwrap_or_default();
        Ok((info, fp))
    }

    /// another client for the same relay and device, with a connection of its own
    pub fn fork(&self) -> Relay {
        Relay::build(self.base.clone(), self.pin, self.auth.clone(), false)
    }

    /// "https://host:port"
    pub fn address(&self) -> String {
        self.base.origin()
    }

    fn call(&self, method: &str, target: &str, json: bool, body: &[u8]) -> RelayResult<Vec<u8>> {
        let mut h = Headers::new();
        if !self.auth.is_empty() {
            h.set("Authorization", &format!("Bearer {}", self.auth));
        }
        if !body.is_empty() {
            h.set("Content-Type", if json { "application/json" } else { "application/octet-stream" });
        }
        let r = match self.session.lock().request(method, target, &h, body, 96 << 20) {
            Ok(r) => r,
            Err(e) => return fail(0, format!("{}", e)),
        };
        if (200..300).contains(&r.status) {
            return Ok(r.body_bytes().to_vec());
        }
        let text = j::parse(r.body_bytes()).map(|v| String::from(j::str_of(&v, "message"))).filter(|m| !m.is_empty()).unwrap_or_else(|| String::from(netio::http::reason(r.status)));
        fail(r.status, text)
    }

    fn call_json(&self, method: &str, target: &str, body: Option<&Value>) -> RelayResult<Value> {
        let text = body.map(j::text).unwrap_or_default();
        let b = self.call(method, target, true, text.as_bytes())?;
        if b.is_empty() {
            return Ok(Value::Null);
        }
        j::parse(&b).ok_or(RelayError { status: 0, text: String::from("the answer is not JSON") })
    }

    pub fn info(&self) -> RelayResult<RelayInfo> {
        let v = self.call_json("GET", "/v1/info", None)?;
        let info = RelayInfo::from_json(&v).filter(|i| !i.relay_id.is_empty()).ok_or(RelayError { status: 0, text: String::from("this is not a Build Balancer relay") })?;
        if info.protocol != crate::PROTOCOL {
            return fail(0, format!("the relay speaks protocol {}, this program {}: update the older one", info.protocol, crate::PROTOCOL));
        }
        Ok(info)
    }

    /// take a fresh relay with its setup token: this device becomes its main machine -> its access token
    pub fn claim(&self, setup_token: &str, id: &str, name: &str) -> RelayResult<String> {
        let v = self.call_json("POST", "/v1/claim", Some(&j::obj([("setup", j::s(setup_token)), ("id", j::s(id)), ("name", j::s(name))])))?;
        Ok(String::from(j::str_of(&v, "token")))
    }

    /// the admitted devices, and the relay's clock (their last_seen compares against it, not
    /// against this machine's)
    pub fn devices(&self) -> RelayResult<(Vec<Device>, i64)> {
        let v = self.call_json("GET", "/v1/devices", None)?;
        Ok((j::from_list(v.get("devices").unwrap_or(&Value::Null)), v.get("time").and_then(Value::as_i64).unwrap_or(0)))
    }

    pub fn device_add(&self, d: &Device) -> RelayResult<()> {
        self.call_json("POST", "/v1/devices", Some(&d.to_json())).map(|_| ())
    }

    pub fn device_remove(&self, id: &str) -> RelayResult<()> {
        self.call("DELETE", &format!("/v1/devices/{}", id), false, &[]).map(|_| ())
    }

    /// (main machine) open a pairing slot for `ttl` seconds
    pub fn pair_open(&self, slot: &str, ttl: u32) -> RelayResult<()> {
        self.call_json("POST", &format!("/v1/pair/{}/open", slot), Some(&j::obj([("ttl", j::n(ttl as u64))]))).map(|_| ())
    }

    /// leave a pairing message: `from` is "worker" or "main"
    pub fn pair_put(&self, slot: &str, from: &str, body: &[u8]) -> RelayResult<()> {
        self.call("POST", &format!("/v1/pair/{}/{}", slot, from), false, body).map(|_| ())
    }

    /// the pairing message `from` left, waiting up to `wait` seconds (None: not there yet)
    pub fn pair_get(&self, slot: &str, from: &str, wait: u32) -> RelayResult<Option<Vec<u8>>> {
        let b = self.call("GET", &format!("/v1/pair/{}/{}?wait={}", slot, from, wait), false, &[])?;
        Ok((!b.is_empty()).then_some(b))
    }

    pub fn status_put(&self, sealed: &[u8]) -> RelayResult<()> {
        self.call("PUT", "/v1/status", false, sealed).map(|_| ())
    }

    /// add a message to a channel -> its number
    pub fn ch_append(&self, channel: &str, body: &[u8]) -> RelayResult<u64> {
        let b = self.call("POST", &format!("/v1/ch/{}", channel), false, body)?;
        Ok(j::parse(&b).map(|v| j::u64_of(&v, "seq")).unwrap_or(0))
    }

    /// the messages of a channel after number `after` (in order), waiting up to `wait` seconds
    /// for the first
    pub fn ch_read(&self, channel: &str, after: u64, wait: u32) -> RelayResult<Vec<(u64, Vec<u8>)>> {
        let b = self.call("GET", &format!("/v1/ch/{}?after={}&wait={}", channel, after, wait), false, &[])?;
        let mut out = Vec::new();
        let mut p = 0;
        while p + 12 <= b.len() {
            let seq = u64::from_le_bytes(b[p..p + 8].try_into().unwrap_or([0; 8]));
            let len = u32::from_le_bytes(b[p + 8..p + 12].try_into().unwrap_or([0; 4])) as usize;
            p += 12;
            if p + len > b.len() {
                return fail(0, "a channel answer is cut short");
            }
            out.push((seq, b[p..p + len].to_vec()));
            p += len;
        }
        Ok(out)
    }

    pub fn ch_delete(&self, channel: &str) -> RelayResult<()> {
        self.call("DELETE", &format!("/v1/ch/{}", channel), false, &[]).map(|_| ())
    }

    /// which of these blobs the relay has (asking marks them as in use)
    pub fn blobs_has(&self, ids: &[[u8; 32]]) -> RelayResult<Vec<bool>> {
        let mut out = Vec::with_capacity(ids.len());
        for part in ids.chunks(8192) {
            let b = self.call("POST", "/v1/blobs/has", false, part.as_flattened())?;
            if b.len() != part.len() {
                return fail(0, "a blob answer has the wrong length");
            }
            out.extend(b.iter().map(|x| *x != 0));
        }
        Ok(out)
    }

    /// upload a pack made with pack_add
    pub fn blobs_put(&self, pack: &[u8]) -> RelayResult<()> {
        self.call("POST", "/v1/blobs/put", false, pack).map(|_| ())
    }

    /// fetch blobs; `on` gets each id with its bytes, None for one the relay does not have.
    /// The caller keeps a request below PACK_MAX of content.
    pub fn blobs_get(&self, ids: &[[u8; 32]], on: &mut dyn FnMut(&[u8; 32], Option<&[u8]>)) -> RelayResult<()> {
        if ids.is_empty() {
            return Ok(());
        }
        let b = self.call("POST", "/v1/blobs/get", false, ids.as_flattened())?;
        let mut answered: alloc::collections::BTreeSet<[u8; 32]> = alloc::collections::BTreeSet::new();
        let mut p = 0;
        while p + 36 <= b.len() {
            let id: [u8; 32] = b[p..p + 32].try_into().unwrap_or([0; 32]);
            let len = u32::from_le_bytes(b[p + 32..p + 36].try_into().unwrap_or([0; 4]));
            p += 36;
            answered.insert(id);
            if len == u32::MAX {
                on(&id, None);
                continue;
            }
            let len = len as usize;
            if p + len > b.len() {
                return fail(0, "a blob answer is cut short");
            }
            on(&id, Some(&b[p..p + len]));
            p += len;
        }
        // an answer that leaves an id out, or ends in half a record, is no answer
        if p != b.len() || ids.iter().any(|id| !answered.contains(id)) {
            return fail(0, "a blob answer does not hold every blob that was asked for");
        }
        Ok(())
    }

    pub fn stats(&self) -> RelayResult<Value> {
        self.call_json("GET", "/v1/stats", None)
    }
}

/// add a blob to a pack: id, length, bytes
pub fn pack_add(pack: &mut Vec<u8>, id: &[u8; 32], blob: &[u8]) {
    pack.extend_from_slice(id);
    pack.extend_from_slice(&(blob.len() as u32).to_le_bytes());
    pack.extend_from_slice(blob);
}
