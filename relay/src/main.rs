// bb-relay -- the Build Balancer relay: the one machine everything else can reach
//
//   bb-relay service install [the options of run]     the usual way on a server: installs the
//                                                      relay as a system service, starts it and
//                                                      prints the line for the main machine
//   bb-relay [run] [--listen ADDR:PORT] [--public HOST[:PORT]] [--data DIR]
//                  [--store-limit SIZE] [--keep-days N] [--threads N] [--plain]
//                                                      in this terminal, for as long as it is open
//   bb-relay status [--data DIR]           what the data folder holds, and the setup line again
//   bb-relay setup-key [--new] [--data DIR]   the setup key (--new: another one, for a new or
//                                             another main machine)
//   bb-relay service uninstall | start | stop | status
//   bb-relay --version | --help
//
// It makes its data folder, its certificate and a setup key, and prints the two commands for
// the main machine and for the workers.  It only passes data on: messages and blobs are sealed
// between the machines, the relay has no key (docs/ARCHITECTURE.md).  Devices reach it over TLS
// 1.3 with its own certificate, which they pin by its fingerprint; --plain serves http for a
// relay on the same machine or a trusted network.
#![no_std]
#![no_main]
#![deny(unsafe_code)]

extern crate alloc;

mod api;
mod store;

use alloc::boxed::Box;
use alloc::format;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;

use bb_core::util::{out, size_text, Cli};
use bb_exec::svc;
use crypto::ed25519::Keypair;
use netio::server::Wrap;
use netio::tls::{self, Identity};
use netio::x509::{Cert, CertBuilder};
use netio::{Server, ServerConfig, Stream};
use plat::net::{IpAddr, SocketAddr, TcpStream};

use crate::store::Store;

const SERVICE: &str = if cfg!(windows) {
    "BuildBalancerRelay"
} else if cfg!(target_os = "macos") {
    "org.buildbalancer.relay"
} else {
    "buildbalancer-relay"
};
const VALUED: &[&str] = &["--listen", "--public", "--data", "--store-limit", "--keep-days", "--threads"];
const DEFAULT_LISTEN: &str = "0.0.0.0:7700";

fn fail(s: &str) -> i32 {
    plat::log_error("relay", s);
    1
}

fn usage() -> i32 {
    out("bb-relay service install [options of run]   on a server: install as a system service, start,\n\
         \x20                                          and print the line for the main machine\n\
         bb-relay [run] [--listen ADDR:PORT] [--public HOST[:PORT]] [--data DIR]\n\
         \x20              [--store-limit SIZE] [--keep-days N] [--threads N] [--plain]\n\
         \x20                                          run in this terminal\n\
         bb-relay status [--data DIR]        devices, storage, and the setup line again\n\
         bb-relay setup-key [--new]          the setup key (--new: one for another main machine)\n\
         bb-relay service uninstall | start | stop | status\n\
         bb-relay --version\n\
         \n\
         defaults: --listen 0.0.0.0:7700, --store-limit 20G, --keep-days 7, TLS with the relay's\n\
         own certificate (--plain: http).\n");
    2
}

fn default_dir() -> String {
    // run by root (a service): a place of the system's, not root's home
    if cfg!(target_os = "macos") && plat::process::elevated() {
        return String::from("/Library/Application Support/BuildBalancer/relay");
    }
    if !cfg!(windows) && plat::process::elevated() {
        return String::from("/var/lib/buildbalancer/relay");
    }
    let base = plat::data_dir().unwrap_or_else(|| String::from("."));
    bb_core::util::below(&base, "buildbalancer/relay")
}

fn data_dir(cli: &Cli) -> String {
    bb_core::util::abs_path(&cli.value("--data").map(String::from).unwrap_or_else(default_dir))
}

fn holds_relay(dir: &str) -> bool {
    plat::file_exists(&plat::fs::join(dir, "relay.json")) == 1
}

/// The relay's certificate: one self-signed Ed25519 certificate, made at the first start.
/// Devices pin its SHA-256, so it needs no authority and no name that fits.
fn identity(dir: &str) -> Result<(Identity, [u8; 32]), String> {
    let (cp, kp) = (plat::fs::join(dir, "tls.pem"), plat::fs::join(dir, "tls.key"));
    let read = || -> Option<(Vec<u8>, Keypair)> {
        let der = netio::pem::decode(&String::from_utf8_lossy(&plat::read_file(&cp)?), "CERTIFICATE")?;
        let key = Keypair::from_seed(&bb_core::crypt::key32(&String::from_utf8_lossy(&plat::read_file(&kp)?))?);
        (Cert::parse(&der)?.public_key == key.public).then_some((der, key))
    };
    let (der, key) = match read() {
        Some(x) => x,
        None if plat::file_exists(&cp) != 0 || plat::file_exists(&kp) != 0 => return Err(format!("{} and tls.key do not fit together; remove both for a new certificate (every device then has to pair again)", cp)),
        None => {
            let key = Keypair::from_seed(&plat::random_bytes());
            let t = bb_core::util::now();
            let der = CertBuilder {
                serial: plat::random_bytes(),
                cn: format!("Build Balancer relay {}", plat::net::hostname()),
                org: String::from("Build Balancer"),
                not_before: t - 86_400,
                not_after: t + 30 * 365 * 86_400,
                public_key: key.public,
                dns: alloc::vec![String::from("localhost"), plat::net::hostname()],
                ips: alloc::vec![IpAddr::LOCALHOST_V4, IpAddr::LOCALHOST_V6],
                server_auth: true,
                ..CertBuilder::default()
            }
            .sign(None, &key);
            if !plat::fs::write_file_private(&cp, netio::pem::encode("CERTIFICATE", &der).as_bytes()) || !plat::fs::write_file_private(&kp, crypto::hex(key.seed()).as_bytes()) {
                return Err(format!("{} cannot be written", cp));
            }
            (der, key)
        }
    };
    let fp = crypto::sha256(&der);
    Ok((Identity { chain: alloc::vec![der], key }, fp))
}

/// the fingerprint of the certificate a data folder holds (None: none yet, or a --plain relay)
fn stored_fingerprint(dir: &str) -> Option<[u8; 32]> {
    plat::read_file(&plat::fs::join(dir, "tls.pem")).and_then(|b| netio::pem::decode(&String::from_utf8_lossy(&b), "CERTIFICATE")).map(|der| crypto::sha256(&der))
}

/// a new setup key: its hash for the relay to compare, and the key itself so it can be shown
/// again until it is used (both only their owner can read)
fn new_setup_key(dir: &str, fp: Option<&[u8; 32]>) -> Result<String, String> {
    let token: [u8; 16] = plat::random_bytes();
    let hash = bb_core::crypt::token_hash(&bb_core::crypt::setup_token_text(&token));
    let key = bb_core::crypt::setup_key_encode(&token, fp);
    if !plat::fs::write_file_private(&plat::fs::join(dir, "setup.hash"), hash.as_bytes()) || !plat::fs::write_file_private(&plat::fs::join(dir, "setup.key"), key.as_bytes()) {
        return Err(format!("{} cannot be written", dir));
    }
    Ok(key)
}

/// The setup key that is still waiting to be used.  A relay nobody took yet shows the same key
/// at every start: closing the terminal, or installing the service, between copying the line
/// and running it must not make the line worthless.
fn waiting_setup_key(dir: &str, fp: Option<&[u8; 32]>) -> Option<String> {
    let read = |name: &str| plat::read_file(&plat::fs::join(dir, name)).map(|b| String::from(String::from_utf8_lossy(&b).trim()));
    let (key, hash) = (read("setup.key")?, read("setup.hash")?);
    let (token, key_fp) = bb_core::crypt::setup_key_decode(&key)?;
    // the key the relay still accepts, for the certificate it shows now
    (bb_core::crypt::token_hash(&token) == hash && key_fp == fp.map(|f| crypto::hex(f)).unwrap_or_default()).then_some(key)
}

/// the address the other machines are told: --public, the listen address, or this machine's
/// first address that is not loopback
fn public_addr(cli: &Cli, listen: &SocketAddr) -> (String, bool) {
    if let Some(p) = cli.value("--public") {
        let p = p.trim_start_matches("https://").trim_start_matches("http://");
        let with_port = if p.rsplit_once(':').is_some_and(|(_, port)| port.parse::<u16>().is_ok()) { String::from(p) } else { plat::net::host_port(p, listen.port) };
        return (with_port, true);
    }
    if !listen.ip.is_unspecified() {
        return (plat::net::host_port(&format!("{}", listen.ip), listen.port), true);
    }
    let own = plat::net::local_addrs().into_iter().map(|a| a.ip).find(|ip| matches!(ip, IpAddr::V4(b) if b[0] != 127 && !(b[0] == 169 && b[1] == 254)));
    match own {
        Some(ip) => (plat::net::host_port(&format!("{}", ip), listen.port), false),
        None => (plat::net::host_port(&plat::net::hostname(), listen.port), false),
    }
}

/// What a firewall on this machine does about the relay's port, when it can be told: a relay
/// that starts fine behind a closed port looks healthy and is reachable by nobody.
fn firewall_hint(port: u16) -> Option<String> {
    if !cfg!(target_os = "linux") {
        return None;
    }
    let text = |path: &str| plat::read_file(path).map(|b| String::from_utf8_lossy(&b).into_owned());
    // ufw: on when its configuration says so; its rules are readable for root only
    if text("/etc/ufw/ufw.conf").is_some_and(|c| c.lines().any(|l| l.trim().eq_ignore_ascii_case("ENABLED=yes"))) {
        let open = text("/etc/ufw/user.rules").map(|r| r.lines().any(|l| !l.trim_start().starts_with('#') && l.contains("ACCEPT") && l.split_whitespace().any(|w| w == format!("{}", port))));
        return match open {
            Some(true) => None,
            Some(false) => Some(format!("The firewall of this machine (ufw) is on and has no rule for port {port}: nobody can reach\nthe relay until you run\n\n       ufw allow {port}/tcp\n")),
            None => Some(format!("The firewall of this machine (ufw) is on.  If it has no rule for port {port} yet:\n\n       sudo ufw allow {port}/tcp\n")),
        };
    }
    // firewalld: asks its daemon (root, or it does not answer)
    if plat::fs::stat("/run/firewalld").is_some() || bb_exec::tools::run_brief("firewall-cmd", &["--state"], 4000).is_some_and(|(code, _)| code == 0) {
        let open = bb_exec::tools::run_brief("firewall-cmd", &["--query-port", &format!("{}/tcp", port)], 4000).map(|(code, _)| code == 0);
        if open != Some(true) {
            return Some(format!("The firewall of this machine (firewalld) is on{}:\n\n       firewall-cmd --permanent --add-port={port}/tcp && firewall-cmd --reload\n", if open == Some(false) { format!(" and has port {port} closed") } else { format!(".  If port {port} is not open yet") }));
        }
    }
    None
}

/// what a running relay tells about itself for `status` and `service install`
fn note_address(dir: &str, addr: &str, sure: bool, plain: bool) {
    let text = format!("{}{}\n{}\n", if plain { "http://" } else { "" }, addr, if sure { "sure" } else { "guessed" });
    plat::write_file(&plat::fs::join(dir, "address"), text.as_bytes());
}

/// (the address as the other machines take it, whether it is more than this machine's guess)
fn noted_address(dir: &str) -> Option<(String, bool)> {
    let text = String::from_utf8_lossy(&plat::read_file(&plat::fs::join(dir, "address"))?).into_owned();
    let mut lines = text.lines();
    let addr = lines.next().filter(|a| !a.trim().is_empty())?;
    Some((String::from(addr.trim()), lines.next() == Some("sure")))
}

/// The next steps for the person who set the relay up: the line for the main machine while
/// nobody took the relay, how to add workers afterwards.
fn next_steps(dir: &str, addr: &str, sure: bool) -> String {
    let devices = bb_core::j::load::<store::RelayFile>(&plat::fs::join(dir, "relay.json")).map(|f| f.devices).unwrap_or_default();
    let claimed = devices.iter().any(|d| d.role == bb_core::proto::ROLE_MAIN);
    let port = addr.rsplit_once(':').and_then(|(_, p)| p.parse::<u16>().ok()).unwrap_or(bb_core::DEFAULT_PORT);
    let mut b = String::new();
    match waiting_setup_key(dir, stored_fingerprint(dir).as_ref()).filter(|_| !claimed) {
        Some(key) => {
            b += "This relay has no main machine yet.\n\n  1. On your MAIN machine (the one you start builds from), run this line -- or open the\n     Build Balancer app there, choose \"This is my main machine\" and paste it:\n\n";
            b += &format!("       bb setup {} {}\n\n", addr, key);
            b += "     (The line stays valid until it is used: `bb-relay status` shows it again.)\n";
            b += &format!("     No Build Balancer on that machine yet?  {}#set-it-up-in-three-steps\n\n", bb_core::REPOSITORY);
            b += "  2. Every machine that should lend its power is added from the main machine:\n     `bb pair` there (or \"Add a machine\" in the app) prints the line for it.\n\n";
        }
        None if claimed => {
            b += &format!("{} device(s) admitted.  Machines are added from the main machine: `bb pair` there (or \"Add a\nmachine\" in the app) prints the line to run on the new machine.\n\n", devices.len());
            if let Some(key) = waiting_setup_key(dir, stored_fingerprint(dir).as_ref()) {
                b += &format!("A setup key for another main machine is waiting to be used:\n\n       bb setup {} {}\n\n", addr, key);
            } else {
                b += "A new or second main machine needs a setup key: `bb-relay setup-key --new`.\n\n";
            }
        }
        None => b += "This relay has no main machine yet, and no setup key: `bb-relay setup-key` makes one.\n\n",
    }
    if !sure {
        b += &format!("({} is this machine's address as it sees itself.  Behind a router or with a DNS name,\n use the address the other machines reach it by -- or start with --public HOST.)\n\n", addr.trim_start_matches("http://"));
    }
    if let Some(hint) = firewall_hint(port) {
        b += &hint;
        b += "\n";
    }
    if cfg!(target_os = "linux") {
        b += &format!("At a hoster, its own firewall (security group) has to let TCP port {} in as well.\n\n", port);
    } else if cfg!(windows) {
        b += &format!("Windows may ask whether bb-relay may accept connections: allow it, or the other machines\ncannot reach port {}.\n\n", port);
    }
    b
}

fn run() -> i32 {
    let args = plat::args();
    let rest: Vec<String> = args.iter().skip(1).cloned().collect();
    if rest.iter().any(|a| a == "--version" || a == "-V") {
        out(&bb_core::version_line("bb-relay"));
        return 0;
    }
    if rest.iter().any(|a| a == "--help" || a == "-h") {
        return usage();
    }
    let cli = match Cli::parse(&rest, VALUED, &[], false) {
        Ok(c) => c,
        Err(e) => {
            bb_core::util::err(&format!("bb-relay: {}\n", e));
            return 2;
        }
    };
    if let Err(e) = cli.only(&["--listen", "--public", "--data", "--store-limit", "--keep-days", "--threads", "--plain", "--service", "--new"]) {
        bb_core::util::err(&format!("bb-relay: {}\n", e));
        return 2;
    }
    plat::log_levels(plat::LOG_INFO, plat::LOG_INFO);
    match cli.pos.first().map(String::as_str).unwrap_or("run") {
        "run" if cli.flag("--service") => plat::service::run_service(SERVICE, || serve_from_args()),
        "run" => serve(&cli),
        "setup-key" => setup_key_cmd(&cli),
        "status" => status(&cli),
        "service" => service_cmd(&cli),
        _ => usage(),
    }
}

/// the service's entry: run_service takes a plain function
fn serve_from_args() -> i32 {
    let rest: Vec<String> = plat::args().iter().skip(1).cloned().collect();
    match Cli::parse(&rest, VALUED, &[], false) {
        Ok(c) => serve(&c),
        Err(e) => fail(&e),
    }
}

fn serve(cli: &Cli) -> i32 {
    let dir = data_dir(cli);
    let plain = cli.flag("--plain");
    let limit = match cli.value("--store-limit").map(bb_core::util::size_parse) {
        Some(None) => return fail("--store-limit takes a size like 20G"),
        Some(Some(v)) => v.max(64 << 20),
        None => 20 << 30,
    };
    let keep = cli.value("--keep-days").and_then(|v| v.parse::<i64>().ok()).unwrap_or(7).clamp(1, 3650);
    let Some(listen) = SocketAddr::parse(cli.value("--listen").unwrap_or(DEFAULT_LISTEN)) else { return fail("--listen takes ADDRESS:PORT, like 0.0.0.0:7700") };
    let store = match Store::open(&dir, limit, keep) {
        Ok(s) => Arc::new(s),
        Err(e) => return fail(&e),
    };
    plat::log_file(Some(&plat::fs::join(&plat::fs::join(&dir, "logs"), "relay.log")), 10 << 20, 3);
    let (wrap, fp): (Option<Wrap>, Option<[u8; 32]>) = if plain {
        (None, None)
    } else {
        let (id, fp) = match identity(&dir) {
            Ok(x) => x,
            Err(e) => return fail(&e),
        };
        let cfg = tls::ServerConfig { identity: id, client_auth: None, client_auth_optional: false };
        let w: Wrap = Arc::new(move |tcp: TcpStream| {
            let s = tls::accept(Box::new(tcp), cfg.clone()).ok()?;
            Some((Box::new(s) as Box<dyn Stream>, None))
        });
        (Some(w), Some(fp))
    };
    let (claimed, relay_id) = {
        let st = store.st.lock();
        (st.file.devices.iter().any(|d| d.role == bb_core::proto::ROLE_MAIN), st.file.relay_id.clone())
    };
    // a relay nobody took yet has a setup key: the one it showed before, as long as it still fits
    if !claimed
        && waiting_setup_key(&dir, fp.as_ref()).is_none()
        && let Err(e) = new_setup_key(&dir, fp.as_ref())
    {
        return fail(&e);
    }
    let threads = cli.value("--threads").and_then(|v| v.parse::<usize>().ok()).unwrap_or(64).clamp(8, 512);
    // a connection that only waits gives its thread back after a while; devices reconnect by themselves
    let cfg = ServerConfig { threads, max_body: 40 << 20, idle_ms: 20_000, read_ms: 30_000, write_ms: 60_000, max_requests: 100_000 };
    let s2 = store.clone();
    let server = match Server::start_with(&listen, cfg, Arc::new(move |req| api::handle(&s2, req)), wrap) {
        Ok(s) => s,
        Err(e) => return fail(&format!("{} cannot be opened: {} (does a relay run here already? `bb-relay service status`)", listen, e)),
    };
    let (addr, sure) = public_addr(cli, &server.local_addr());
    note_address(&dir, &addr, sure, plain);
    let shown = format!("{}{}", if plain { "http://" } else { "" }, addr);
    let mut b = format!("\n{} relay {}  (id {})\n", bb_core::PRODUCT, bb_core::VERSION, relay_id);
    b += &format!("  data folder   {}\n  listening on  {} ({})\n", dir, server.local_addr(), if plain { "http, no TLS" } else { "TLS 1.3" });
    if let Some(f) = &fp {
        b += &format!("  certificate   SHA-256 {}\n", bb_core::crypt::fingerprint_text(f));
    }
    b += &format!("  blob store    up to {}, channels kept {} days\n\n", size_text(limit), keep);
    b += &next_steps(&dir, &shown, sure);
    if plain && !listen.ip.is_loopback() {
        b += "(--plain: access tokens travel unencrypted.  Jobs, sources and results stay sealed\n end to end, but use this only inside a network you trust.)\n\n";
    }
    if !cli.flag("--service") {
        // by its full path: sudo has a PATH of its own
        let exe = plat::current_exe().unwrap_or_else(|| String::from("bb-relay"));
        b += &format!("The relay runs as long as this terminal is open.  To keep it running, also after a\nrestart of the machine, end it (Ctrl+C) and run:\n\n       {}\n\n", if cfg!(windows) { String::from("bb-relay service install   (in a terminal run as administrator)") } else { format!("sudo {} service install", exe) });
    }
    out(&b);
    plat::log_info("relay", &format!("started on {}", server.local_addr()));
    let mut ticks = 0u32;
    while !plat::service::stop_requested() {
        plat::sleep_ms(500);
        ticks += 1;
        if ticks % 120 == 0 {
            store.sweep(&mut store.st.lock());
        }
    }
    plat::log_info("relay", "stopping");
    store.changed.notify_all();
    server.stop();
    0
}

fn setup_key_cmd(cli: &Cli) -> i32 {
    let dir = data_dir(cli);
    if !holds_relay(&dir) {
        return fail(&format!("{} holds no relay yet: start bb-relay once", dir));
    }
    // a relay with TLS has its certificate by now; without one it runs with --plain
    let fp = stored_fingerprint(&dir);
    let addr = noted_address(&dir).map_or_else(|| String::from("<relay address>"), |a| a.0);
    if !cli.flag("--new")
        && let Some(k) = waiting_setup_key(&dir, fp.as_ref())
    {
        out(&format!("The relay's setup key (valid once, not used yet):\n\n    bb setup {} {}\n\n(`bb-relay setup-key --new` replaces it by another one.)\n", addr, k));
        return 0;
    }
    match new_setup_key(&dir, fp.as_ref()) {
        Ok(k) => {
            out(&format!("A new setup key (valid once; an older unused one is void now):\n\n    bb setup {} {}\n\n", addr, k));
            0
        }
        Err(e) => fail(&e),
    }
}

fn status(cli: &Cli) -> i32 {
    let dir = data_dir(cli);
    let Some(f) = bb_core::j::load::<store::RelayFile>(&plat::fs::join(&dir, "relay.json")) else {
        return fail(&format!("{} holds no relay{}", dir, elsewhere(&dir)));
    };
    out(&format!("relay {} in {}\nservice {}: {}\n", f.relay_id, dir, SERVICE, svc::state_text(SERVICE)));
    for d in &f.devices {
        out(&format!("  {:<7} {:<24} {}\n", d.role, d.name, d.id));
    }
    let blobs = bb_core::util::tree_size(&plat::fs::join(&dir, "blobs"), u64::MAX);
    let ch = bb_core::util::tree_size(&plat::fs::join(&dir, "channels"), u64::MAX);
    out(&format!("blobs {}, channels {}\n\n", size_text(blobs), size_text(ch)));
    match noted_address(&dir) {
        Some((addr, sure)) => out(&next_steps(&dir, &addr, sure)),
        None => out("The relay has not run yet: its address and setup line appear once it has.\n"),
    }
    0
}

/// The data folder a relay would have under the other identity: root's (the system's) when
/// asked as the person who called sudo, that person's when asked as root.  A relay started as
/// one and looked for as the other must not look like no relay at all.
fn other_place(dir: &str) -> Option<String> {
    if cfg!(windows) {
        return None;
    }
    let system = if cfg!(target_os = "macos") { "/Library/Application Support/BuildBalancer/relay" } else { "/var/lib/buildbalancer/relay" };
    let mut places = alloc::vec![String::from(system)];
    if let Some(u) = svc::sudo_user() {
        places.push(svc::user_data_dir(&u, "relay"));
    }
    places.into_iter().find(|p| p != dir && holds_relay(p))
}

/// what to say about a relay that is not in `dir` but may well be on this machine
fn elsewhere(dir: &str) -> String {
    if let Some(o) = other_place(dir) {
        return format!(" -- but {} does: add --data {}", o, o);
    }
    // root's folder cannot be looked into from outside; that it is there is enough for a hint
    let system = if cfg!(target_os = "macos") { "/Library/Application Support/BuildBalancer" } else { "/var/lib/buildbalancer" };
    if !cfg!(windows) && !plat::process::elevated() && plat::fs::stat(system).is_some() {
        return format!(" -- a relay that was installed with sudo lives in {}/relay: run this command with sudo", system);
    }
    String::new()
}

fn service_cmd(cli: &Cli) -> i32 {
    let r = match cli.pos.get(1).map(String::as_str).unwrap_or("status") {
        "install" => install_service(cli),
        "uninstall" => svc::uninstall(SERVICE).map(|_| format!("service {} removed (the data folder stays)", SERVICE)),
        "start" => svc::start(SERVICE).map(|_| format!("service {} started", SERVICE)),
        "stop" => svc::stop(SERVICE).map(|_| format!("service {} stopped", SERVICE)),
        "status" => Ok(format!("service {}: {}", SERVICE, svc::state_text(SERVICE))),
        _ => return usage(),
    };
    match r {
        Ok(m) => {
            out(&format!("{}\n", m));
            0
        }
        Err(e) => fail(&e),
    }
}

/// Where the service's relay lives and who runs it.  A relay that exists already is the one
/// the service runs -- wherever it was started first, as root or as the person who now calls
/// sudo -- and it runs as the owner of its folder, so nothing in it changes hands.
fn service_place(cli: &Cli) -> Result<(String, Option<svc::User>), String> {
    let owner = |dir: &str| plat::fs::stat(dir).filter(|_| cfg!(target_os = "linux")).map(|m| m.uid).filter(|uid| *uid != 0).and_then(svc::user_of);
    if cli.value("--data").is_some() {
        let dir = data_dir(cli);
        let user = owner(&dir);
        return Ok((dir, user));
    }
    let system = default_dir();
    let own = svc::sudo_user().map(|u| svc::user_data_dir(&u, "relay")).filter(|d| *d != system && holds_relay(d));
    match own {
        Some(o) if holds_relay(&system) => Err(format!("there are two relays on this machine: {} and {}.  Say which one the service is to run: --data FOLDER", system, o)),
        Some(o) => {
            let user = owner(&o);
            Ok((o, user))
        }
        None => Ok((system, None)),
    }
}

fn install_service(cli: &Cli) -> Result<String, String> {
    let exe = plat::current_exe().ok_or("the program's own path is unknown")?;
    let (dir, user) = service_place(cli)?;
    let mut args = alloc::vec![String::from("run"), String::from("--service"), String::from("--data"), dir.clone()];
    for (n, v) in &cli.opts {
        if n != "--data" {
            args.push(n.clone());
            args.push(v.clone());
        }
    }
    if cli.flag("--plain") {
        args.push(String::from("--plain"));
    }
    let before = plat::fs::stat(&plat::fs::join(&dir, "address")).map_or(0, |m| m.mtime_ns);
    let service = svc::Service { name: SERVICE, display: "Build Balancer Relay", description: "Build Balancer: passes sealed build data between the machines that paired through it", exe: &exe, args: &args, user: user.as_ref(), path: None };
    svc::install(&service).and_then(|_| svc::start(SERVICE))?;
    let mut m = format!("\nThe relay is installed as the service {} and running{}: it starts with the machine.\n  data folder   {}\n  its log       {}\n\n", SERVICE, user.as_ref().map(|u| format!(" (as {})", u.name)).unwrap_or_default(), dir, plat::fs::join(&plat::fs::join(&dir, "logs"), "relay.log"));
    // the service makes or opens the relay; what it notes there says what to do next
    let end = bb_core::util::tick_ms() + 15_000;
    let noted = loop {
        match noted_address(&dir) {
            Some(a) if plat::fs::stat(&plat::fs::join(&dir, "address")).map_or(0, |m| m.mtime_ns) != before => break Some(a),
            _ if bb_core::util::tick_ms() > end => break None,
            _ => plat::sleep_ms(200),
        }
    };
    match noted {
        Some((addr, sure)) => m += &next_steps(&dir, &addr, sure),
        None => m += &format!("It has not come up within 15 seconds, though: its log says why (on Linux also:\njournalctl -u {} -e).  `bb-relay status` shows the setup line once it runs.\n", SERVICE),
    }
    Ok(m)
}

rt::main!(run());
