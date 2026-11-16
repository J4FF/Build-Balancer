// join -- pair this machine with a main machine
//
//   1. ask the relay who it is and note the certificate it shows (nothing is trusted yet)
//   2. from the code: the slot at the relay and the key of the two pairing messages
//   3. leave a sealed hello with this machine's new public key in the slot
//   4. wait for the sealed welcome: the main machine's key, this machine's id and token, the
//      fleet's data key, and the certificate the main machine pins for the relay
//   5. that certificate must be the one seen in step 1 -- otherwise someone sits between this
//      machine and the relay, and nothing is kept
//
// Only who knows the code can write a hello the main machine opens, and only the main
// machine can answer it: the relay passes both on without being able to read or replace them.

use alloc::format;
use alloc::string::String;

use bb_core::client::Relay;
use bb_core::crypt::{self, Keys};
use bb_core::j::{self, Json};
use bb_core::proto::{Hello, Welcome};
use bb_core::util::{out, Cli};

use crate::config::{self, Config};

pub fn join(cli: &Cli, dir: &str) -> Result<String, String> {
    let (Some(addr), Some(code)) = (cli.pos.get(1), cli.pos.get(2)) else {
        return Err(String::from("bb-worker join RELAY CODE -- both come from `bb pair` on the main machine"));
    };
    if !crypt::code_ok(code) {
        return Err(format!("\"{}\" is not a pairing code (it looks like K7QF-2M9X-PD4T)", code));
    }
    if let Some(old) = config::load(dir)
        && !cli.flag("--force") {
            return Err(format!("this machine is already paired with {} (as \"{}\"); --force pairs it anew", old.main_name, old.name));
        }
    let (off_cpu, off_ram, off_disk) = config::default_offer(dir);
    let mut cfg = Config {
        name: plat::net::hostname(),
        cpu: off_cpu,
        ram: off_ram,
        disk: off_disk,
        jobs: 4,
        priority: String::from("low"),
        ..Config::default()
    };
    config::apply_options(&mut cfg, cli, dir)?;

    // 1. the relay as it shows itself
    let (info, fp_seen) = Relay::probe(addr)?;
    let relay = Relay::open(addr, &fp_seen, "", "")?;

    // 2. + 3. the hello
    out("deriving the pairing key ...\n");
    let (slot, key) = crypt::code_secrets(code, &info.relay_id);
    let keys = Keys::generate();
    let hello = Hello {
        protocol: bb_core::PROTOCOL,
        name: cfg.name.clone(),
        public: keys.public_hex(),
        os: String::from(bb_core::os_name()),
        arch: String::from(bb_core::arch_name()),
        host: plat::net::hostname(),
        version: String::from(bb_core::VERSION),
        relay_fp: fp_seen.clone(),
    };
    let sealed = crypt::seal(&key, &crypt::aad("worker", "pair"), j::text(&hello.to_json()).as_bytes());
    relay.pair_put(&slot, "worker", &sealed).map_err(|e| match e.status {
        404 => String::from("the relay has no pairing open for this code: is `bb pair` still running on the main machine, and is the code typed right?"),
        409 => String::from("this pairing code was used already; run `bb pair` again for a new one"),
        _ => format!("{}", e),
    })?;

    // 4. the welcome
    out("waiting for the main machine ...\n");
    let mut sealed = None;
    for _ in 0..8 {
        match relay.pair_get(&slot, "main", 20) {
            Ok(Some(b)) => {
                sealed = Some(b);
                break;
            }
            Ok(None) => {}
            Err(e) if e.status == 404 => return Err(String::from("the pairing ended before the main machine answered (it timed out or `bb pair` was stopped)")),
            Err(e) => return Err(format!("{}", e)),
        }
    }
    let sealed = sealed.ok_or("the main machine did not answer; run `bb pair` again")?;
    let plain = crypt::open(&key, &crypt::aad("main", "pair"), &sealed).ok_or("the answer does not open with this code: it did not come from the main machine")?;
    let w = j::parse(&plain).and_then(|v| Welcome::from_json(&v)).ok_or("the main machine's answer is not readable")?;
    if !w.error.is_empty() {
        return Err(format!("the main machine refuses: {}", w.error));
    }
    if w.protocol != bb_core::PROTOCOL {
        return Err(format!("the main machine speaks protocol {}, this program {}: update the older one", w.protocol, bb_core::PROTOCOL));
    }

    // 5. the same relay for both
    if w.relay_fp != fp_seen || w.relay_id != info.relay_id {
        return Err(String::from(
            "the relay shows this machine another certificate than the main machine knows.\nNothing was kept: either the address leads to a different relay, or the connection is being tampered with.",
        ));
    }
    if crypt::key32(&w.main_public).is_none() || crypt::key32(&w.data_key).is_none() || !bb_core::util::id_ok(&w.worker_id) {
        return Err(String::from("the main machine's answer is incomplete"));
    }
    cfg.id = w.worker_id;
    cfg.token = w.worker_token;
    cfg.relay = relay.address();
    cfg.relay_fp = fp_seen;
    cfg.relay_id = info.relay_id;
    cfg.secret = keys.secret_hex();
    cfg.main_id = w.main_id;
    cfg.main_name = w.main_name;
    cfg.main_public = w.main_public;
    cfg.data_key = w.data_key;
    config::save(dir, &cfg)?;
    Ok(format!(
        "\nPaired with \"{}\" as worker \"{}\" ({}).\n\nThis machine lends:\n{}\n  change it any time:  bb-worker config --cpu 6 --ram 12G --disk 40G\n\nStart working:\n  bb-worker run                 in this terminal, or\n  bb-worker service install     as a system service that starts with the machine\n",
        cfg.main_name,
        cfg.name,
        cfg.id,
        config::offer_text(&cfg)
    ))
}
