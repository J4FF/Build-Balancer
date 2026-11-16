# Architecture

How the programs fit together, what travels between them and how it is protected.
The source files say the same in their head comments; this is the map.

## The parts

| Program | Runs on | Does |
| --- | --- | --- |
| `bb` | the main machine | decides and submits: scans the project, uploads what is missing, seals a job for a worker, reads its events, fetches its outputs |
| `bb-worker` | every machine that lends power | asks the relay for orders, syncs the project into a workspace, runs the command under the owner's limits, sends output and results |
| `bb-relay` | one address all of them reach | admits devices, passes sealed messages on (channels), stores encrypted file content (blobs) |
| `BuildBalancer` | any of the machines with a screen | the desktop app: a window around the same code. The worker's engine (`bb_worker::Engine`) runs inside it; the main machine's pages call what `bb` calls (`bb_main`) and work on the same files |

`bb-worker` and `bb` are thin command lines over two libraries, `bb_worker` and `bb_main`;
the app links both. Whatever holds a worker's data folder -- `bb-worker run`, its service or
the app -- holds `worker.lock`, so there is one engine per folder.

Every program only makes outgoing HTTPS connections to the relay. Nothing listens on a
worker or on the main machine, so NAT, laptops on the road and firewalls are no obstacle.
In one network the relay can simply run on the main machine.

```
bb ──┐                                     ┌── bb-worker  (long poll: "<worker>/in")
     ├── HTTP/1.1 + TLS 1.3, bearer token ─┤
bb ──┘            bb-relay                 └── bb-worker
                  ├ devices     who is admitted (token hashes), last seen, sealed status
                  ├ pairing     slots that hold the two messages of a pairing for minutes
                  ├ channels    append-only lists of sealed messages, read with long polls
                  └ blobs       encrypted chunks by id, least recently used go first
```

## Keys

| Key | Made by | Held by | Used for |
| --- | --- | --- | --- |
| device key (X25519) | each device, once | that device (`main.json` / `worker.json`) | the pair key |
| pair key | X25519 of two device keys, through HKDF-SHA256 over both public keys | main machine and one worker | sealing jobs, events and status between the two |
| data key (32 random bytes) | the main machine, at setup | the main machine and every worker (handed over in the pairing) | blob ids and blob encryption, so content one device uploaded serves all |
| access token | the relay (main machine) / the main machine (workers) | the device; the relay keeps its BLAKE3 hash | being admitted at the relay |
| relay certificate (Ed25519, self-signed) | the relay, at its first start | the relay; devices pin its SHA-256 | TLS |

The relay holds no pair key and no data key.

**Sealed message** = 24 random bytes of nonce ‖ XChaCha20-Poly1305(pair key, nonce,
associated data, JSON). The associated data is `bb1|<sender id>|<channel>`, so a message moved
into another channel, or sent back to its author, does not open.

**Blob** = one pack byte (0 stored, 1 LZ4, 2 zstd) ‖ XChaCha20-Poly1305(encryption key, nonce,
`bb1blob`‖pack‖id, packed content). The id is BLAKE3 keyed with a key derived from the data
key; the nonce is derived from the id. Equal content therefore gives the equal blob (stored
once, an upload can be repeated), and the relay cannot test a guess of a file against an id.
After decryption the content must hash to the id it was asked for by.

## Setting a relay up

A relay nobody took yet has a setup key: 16 random bytes and, for a relay with TLS, the
fingerprint of its certificate. The relay keeps the key's hash (`setup.hash`), which the
claim compares, and the key itself (`setup.key`, readable by its owner only) -- so that it
shows the same line at every start, in `bb-relay status` and after `bb-relay service
install`, until a main machine has used it. A line that was copied stays good when the
terminal is closed or the service is installed in between. The claim removes both files.
A running relay also notes the address it tells the other machines (`address`), which is
where `status` and `service install` have the setup line's address from.

## Services

`bb-relay service install` and `bb-worker service install` make the programs start with the
machine. What runs as a service is the same program with `run --service --data FOLDER`; who
runs it is decided so that nothing changes hands and no job gets more rights than the person
who set the machine up:

| | Runs as | Data folder |
| --- | --- | --- |
| Relay, Linux | the owner of its data folder: a relay first started by hand keeps running as that user; a new one runs as root | the folder that holds a relay already (the caller's, found through `SUDO_USER`, or the system's), else `/var/lib/buildbalancer/relay` |
| Worker, Linux | the person who installs it (the one behind `sudo`, or `--user NAME`); as root only with `--as-root` | that user's `~/.local/share/buildbalancer/worker`, where `bb-worker join` put the pairing |
| Windows | LocalSystem | the folder of the user who installs |
| macOS | root | `/Library/Application Support/BuildBalancer` |

On Linux the unit is JFLib's (`plat::service::unit_text`) with three lines added
(`bb_exec::svc`): `User=`, `WorkingDirectory=` and, for the worker, `Environment=PATH=` with
the PATH of that user's login shell (`su - USER -c 'printenv PATH'`) plus `~/.cargo/bin` and
`~/.local/bin`, so a job finds what its owner installed for themselves. A relay or a pairing
that exists under the other identity (made with sudo and asked for without, or the other way
round) is named in the error instead of being treated as missing.

`bb-worker join` pairs and then goes on as `bb-worker run`: a machine that joined is online
without a second command (`--no-run` only pairs).

## Updating

`bb_update` is what every way of updating ends in -- the button in the app's settings, `bb
update`, `bb-worker update`, `bb-relay update`, and the order `update` that the main machine
sends a worker:

1. **Which version is the newest:** `REPOSITORY/releases/latest/download/VERSION` (a release
   from before that file existed is found through the address "the latest release" leads
   to).
2. **Fetch:** `SHA256SUMS` and the archive of this system from
   `REPOSITORY/releases/download/vVERSION/`. An archive that does not match its checksum is
   dropped and nothing changes. Every request is https, checked against the system's
   certificate authorities, and stays https wherever a server sends it on to; plain http
   is taken only from a place someone named with `BB_RELEASE_URL`.
3. **Replace** the four programs in the folder the running program lies in -- all four or
   none. One update at a time works in a folder: it holds the lock on
   `.buildbalancer-update.lock`, a file that stays there between updates (a lock is on the
   file, not on its name) and belongs to whoever the folder belongs to. The new files are
   written beside the old ones. Then the update writes down what it is about to do
   (`.buildbalancer-update.journal`: each program and the name its old file is kept under),
   and only then puts the new files in place: on Windows the old file is moved aside
   (`bb.exe.old`; a running program can be renamed, not overwritten), elsewhere it gets a
   second name and the new file then takes the first (a running program must keep its name,
   or it could not find the new one to start). When all four are in place the journal goes,
   and from then on the update counts. If one of the four cannot be put in place, the others
   go back. An update that was cut short -- the power, a kill -- leaves its journal, and
   the next program that starts takes all of it back: two new programs beside two old ones
   are worse than four old ones. Where a program cannot be put back just then (it is in
   use), the journal and the old files stay -- they are all there is to go back to --, the
   program that starts says so, and no new update is laid on top until a later start has
   taken everything back. The same start clears away what a finished update left (on
   Windows the replaced files, once the programs that ran from them have ended); while an
   update is at work in the folder, that tidying keeps its hands off.
4. **Start again**, so the new version runs: the app starts the program that is in its
   place now and closes (the new window waits for the old one to be gone, then lends and
   reconnects as at every start); `bb-worker run` in a terminal does the same, and ends only
   once the new worker holds the data folder -- nobody watches a worker in a terminal, so
   if the new one does not come up, the old one works on and tries again later; as a
   service it ends with an error code, and the service manager starts it again (systemd's
   `Restart=on-failure`).

A worker updates only while no job runs, and from the moment it decides to, no job starts:
`Engine::hold` reserves that under the scheduler's lock. Jobs that arrive meanwhile are
taken and wait -- on disk, too -- and run after the restart, or at once when the update
did not happen. An update that fails (a download can) is tried again a few times, a minute
apart.

A worker learns that it should update or start again in three ways. The order `update` in
its inbox (sealed by the main machine like every order) has it fetch the release. The file
`restart.request` in its data folder, which `bb-worker update` writes, has it start again
with the programs that are there -- "now" after the command replaced them, else the version
they have, so that a worker still running an older one starts again, too. And it sees by
itself that another file has taken its program's place (`bb_update::replaced`): an update
made from anywhere else -- `bb update`, the app, the install line, another user who cannot
write to this worker's data folder -- needs no way to ask it, and a service is never
stopped for an update, which would cut its jobs off. Each of the three sets
`Worker::restart`, and whoever runs the engine -- the app or `bb-worker run` -- acts on it
once no job runs. Versions before 0.1.2 know none of this: the main machine does not send
them the order and says that those machines need the install line once more.

The main machine cannot choose what a worker installs: the worker fetches the release of
the project itself, and the word to a worker needs no way to the releases from the main
machine. `BB_RELEASE_URL` names another place with `VERSION`, `SHA256SUMS` and the archives
side by side (a mirror; the tests).

## Pairing

```
main machine                        relay                          new worker
bb pair
  code = 12 random characters (60 bits)
  slot, key = Argon2id(code, salt = relay id)      32 MiB, 2 passes
  POST /v1/pair/<slot>/open ───────▶ slot open for 10 min
  shows:  bb-worker join RELAY CODE
                                                                   bb-worker join RELAY CODE
                                                    ◀── GET /v1/info   (notes the certificate it sees)
                                                                   slot, key = Argon2id(code, relay id)
                                    hello ◀──────── POST /v1/pair/<slot>/worker
                                                    seal(key, {name, public key, os, certificate seen})
  GET /v1/pair/<slot>/worker ◀────── hello
  opens it with key; checks the certificate the worker saw against its own pin
  POST /v1/devices {id, name, token hash}          admits the worker
  POST /v1/pair/<slot>/main ───────▶ welcome ─────▶ GET /v1/pair/<slot>/main
  seal(key, {main's public key, worker id, token, data key, pinned certificate})
                                                                   checks the certificate again, keeps everything
```

The relay sees the slot (16 bytes of the Argon2id output) and two sealed messages. Guessing
the code from the slot costs an Argon2id run per guess over 2^60 codes; the slot lives for
minutes and takes one hello. A relay (or someone in the path) that answered the worker with
its own certificate is caught by the comparison in the last step, because the main machine's
pin travels inside the sealed welcome.

The setup key that makes a machine the main machine is 16 random bytes (a one-time token, the
relay keeps its hash in `setup.hash`) and the certificate's fingerprint, base64url. Whoever can
run `bb-relay setup-key` on the server can make a main machine: the server's file system is
the authority.

## A job

```
bb run -w laptop --out "build/**" -- make

 1. scan the project        Tree: every file with size and chunk ids (4 MiB chunks);
                            files known with the same size and time are not read again
 2. upload                  ask the relay which chunk ids it lacks, send those in packs;
                            the Tree's text itself becomes blobs: a TreeRef names them
 3. order                   JobSpec {id, project, slot, TreeRef, argv | script, cpu, ram,
                            timeout, outputs} sealed into channel "<worker>/in"
 4. worker: queued          checked (protocol, paths, memory asked for), waits for processors
 5. worker: syncing         fetch the Tree; compare with what the last sync wrote ("known");
                            fetch changed files, remove files the last Tree had and this one
                            has not; everything else in the workspace (build output) stays
 6. worker: running         start the command pinned to the job's processors; output goes
                            as sealed events into "<main>/job/<job id>" and into out.log
 7. worker: collecting      expand the output patterns, upload those files, TreeRef of them
 8. done                    {state, exit, duration, peak memory, outputs' TreeRef}
 9. bb                      reads the events (long poll), prints the output, fetches the
                            outputs into the project folder, deletes the channel
```

`bb` need not stay: events wait in the job's channel, and any later `bb status / logs / wait`
reads on from the number of the last event it saw (kept in the job's record). The job's lock
file lets several `bb` processes follow one job without reading events twice.

A worker need not be online when a job is sent: the order waits in its inbox.

### What survives a restart

A job's folder on the worker (`<worker data>/jobs/<job id>/`) says where the job stands:

| File | Written | Means |
| --- | --- | --- |
| `spec.json` | when the worker accepts the job, before it is queued | the job is this worker's; an order delivered twice is recognised by it |
| `started` | when the job gets its processors | from here on it cannot simply be run again |
| `out.log` | while it runs | all of its output (the relay gets it in pieces, 32 MiB at most) |
| `done.json` | at its end | the result |
| `sent` | when the relay took the result | the main machine can read it |

When a worker starts, jobs with only `spec.json` are queued again in their old order; a job
with `started` and no `done.json` was interrupted and ends as `error`; a result without
`sent` is sent again, then and every half minute, until the relay takes it. Folders of jobs
that ended a week ago are removed.

An engine looks at those folders once, when it starts. So an engine that is stopped takes
no job any more: the stop is set under the scheduler's lock, and `spec.json` is written and
the job queued under it. An order that reached a stopped engine is not counted as read, and
the next engine on the data folder -- in the app the one after the switch, elsewhere the
program started again -- reads it and takes the job.

A job's output never holds the job up: the loop that watches the process (pipes, limits,
cancel) hands pieces to a thread that sends them. When the relay is away the pieces queue up
(256 of them), then count as "not sent" -- the worker's `out.log` has all of it, and the
job's log on the main machine says how much is missing.

On the main machine a job's record is written before its order is sent, and an event counts
as read (the record's `after`) only when what it brought is on disk; the job's channel at
the relay is deleted after the final record was written. The list of paired workers is
changed under a lock file on the list as the file holds it, and a new worker is on it before
it gets its welcome.

### States

`submitted → queued → syncing → running → collecting → done | failed | cancelled | killed | error`

`failed` is a command that ended with a non-zero exit code; `killed` a limit (time, memory,
disk); `error` a job that never ran or whose result is incomplete (no such program, sync
failed, worker restarted).

### Workspaces

`<worker data>/ws/<project id>/<slot>/tree` holds the project's files, `known` what the sync
wrote (size, time, chunk ids per file), `meta.json` its size and last use. Jobs of one
project and slot never run at the same time. Only files a sync wrote are ever removed by a
sync; a file a build changed is fetched again when the next Tree still has the old content.
A sync never writes through a link: whatever lies on the way to a file and is not a real
folder is removed first, and output patterns are not followed through links either.

A workspace is a cache, so it may go at any time except while a job runs in it. Every way
of removing one -- the app's button, `bb-worker clean` (a request file in `requests/` when a
worker runs on the folder), the main machine's order `ws.remove`, the expiry after
`keep_days` without use, making room for another project -- goes through one function of the
engine that reserves the workspace first: it refuses while a job runs there, and no job
starts there until the files are gone. With a project go the folders of its finished jobs.
`bb-worker leave --purge` removes the pairing and everything else.

## The relay's interface

Plain JSON where the relay has to understand (devices), opaque bytes everywhere else.
`Authorization: Bearer <device id>.<token>`.

| Request | Who | |
| --- | --- | --- |
| `GET /v1/info` | anyone | product, version, protocol, relay id |
| `POST /v1/claim` | setup key | become a main machine |
| `GET/POST /v1/devices`, `DELETE /v1/devices/<id>` | main | admit, list (last seen, sealed status), remove |
| `POST /v1/pair/<slot>/open` | main | open a pairing slot |
| `POST/GET /v1/pair/<slot>/worker`, `.../main` | the slot / main | the two pairing messages |
| `PUT /v1/status` | device | its sealed status |
| `POST /v1/ch/<owner>/<topic>` | device | append a message (up to 1 MiB) |
| `GET /v1/ch/<owner>/<topic>?after=N&wait=S` | owner, main | messages after N: frames of number (u64), length (u32), bytes; waits up to S seconds |
| `DELETE /v1/ch/<owner>/<topic>` | owner, main | |
| `POST /v1/blobs/has` | device | ids (32 bytes each) -> one byte each |
| `POST /v1/blobs/put` | device | a pack: id, length (u32), bytes, repeated |
| `POST /v1/blobs/get` | device | ids -> id, length (u32; ffffffff = not there), bytes, repeated |
| `GET /v1/stats` | main | counts and sizes |

Channels are files (`channels/*.ch`), blobs are files (`blobs/ab/<id>`); the relay keeps only
indexes in memory, so a restart loses nothing. Blobs beyond `--store-limit` go, least
recently used first; channels nobody touched for `--keep-days` go (the last access is kept
in the channel's file, so a restart does not forget it). A job whose blobs were
dropped fails with a message that says so, and sending the project again repairs it.

## Limits on a worker

- **Processors.** `Cores` deals the lent processors out as bit masks; a job's process is
  started while the worker's own affinity is set to the job's mask (Windows: the process's,
  Linux: the starting thread's), so the child and everything it starts inherit it. A job
  waits for processors of its own rather than share them. macOS has no call for this: jobs
  get the number as a hint there (`BB_CPUS` and the variables build tools read).
- **Priority.** The same way: below-normal class (Windows) or nice 10 (Linux) around the
  start of the job's process; on macOS the command is started through `/usr/bin/nice`. The
  worker itself keeps normal priority -- it has to answer.
- **Memory.** Each job's thread sums the resident memory of the job's process tree every two
  seconds (Windows: a process snapshot and working sets; Linux: `/proc/<pid>/status`; macOS:
  `proc_listallpids` and `proc_pidinfo`). The
  scheduler adds the jobs up and ends the one furthest above its reservation when the sum
  passes what is lent.
- **Disk.** Workspace sizes are recorded after every job. A sync first makes room by removing
  workspaces unused the longest; while a job runs, the volume's free space is watched and the
  workspace measured when it shrank enough to matter.

## Built on JFLib

The programs are `#![no_std]` and use JFLib's crates for everything below them: `rt`
(runtime), `plat` (files, sockets, threads, processes, services), `netio` (HTTP/1.1 server
and client, TLS 1.3, X.509), `crypto` (X25519, XChaCha20-Poly1305, BLAKE3, Argon2id, HKDF),
`compress` (LZ4, zstd), `base` (INI, glob), `agent_core` / `agent_io` (JSON, stdio framing).
They depend on those crates directly at one exact version instead of the facade crate
`jflib`, which would compile the whole GUI stack for three headless programs. Only the
desktop app adds `jflib-ui` (an immediate-mode UI that draws itself; no toolkit of the
system is linked) and the style `jflib-style-netlab`. Its texts are German in the source
and English through `gui/locales/ui.tsv`, the way JFLib's own apps do it.

The one place with `unsafe` is `crates/bb_exec/src/sys.rs`: the handful of system calls for
processor affinity, priority, free memory and the process list that `plat` has no use for
(Windows: kernel32 and psapi, Linux: system calls and `/proc`, macOS: libSystem).
