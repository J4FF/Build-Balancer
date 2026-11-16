// guide -- `bb guide`: how to work with Build Balancer, written for an AI agent that sits on
// the main machine and decides which machine builds what (people may read it too).  It is
// also what the MCP server hands out as its instructions.

pub const TEXT: &str = "\
BUILD BALANCER -- guide for agents

What it is.  This machine (the main machine) can hand build work to other machines of its
owner (workers).  A job = this project's files + one command, run on one worker.  The worker
keeps its copy of the project between jobs, so its own build cache stays warm and only changed
files travel.  Files you name as outputs come back into the project folder.

The loop.
  1. bb workers --json          who is there: online, os/arch, processors and memory lent and
                                idle, running jobs, tools (cargo, gcc, cmake, python ...), tags.
  2. Decide which worker gets which piece of work.  Split the work into commands that are
     independent (per target, per test suite, per sub-project); give long ones to the
     strongest idle worker; never send to a worker that lacks the tool.
  3. bb run -w NAME --out \"PATTERN\" --json -- COMMAND ARGS...
       blocks until the command ended; prints one JSON document: state, exit_code,
       duration_ms, log_tail, outputs.  Exit code of bb = exit code of the command.
     For several at once, start them detached and wait:
       bb run -d -w a --json -- CMD1      -> {\"job\": \"j-...\"}
       bb run -d -w b --json -- CMD2
       bb wait j-... j-... --json
  4. On failure read the log: bb logs JOB --tail 200   (or --json; the whole log: bb logs JOB)

Choosing a worker.
  -w NAME               that worker.
  (no -w)               the least busy worker that fits --needs.
  --needs \"os=linux arch=x86_64 cpu>=8 ram>=16G tool=cargo tag=gpu\"
  --cpu N --ram 8G      what the job holds there.  Default: all processors the worker lends,
                        so jobs on one worker run one after the other.  Give --cpu to let
                        several run side by side (the job is pinned to N processors and gets
                        BB_CPUS=N, CARGO_BUILD_JOBS=N, CMAKE_BUILD_PARALLEL_LEVEL=N).
  A worker that is offline can still be named: the job waits at the relay until it is back.

Commands and scripts.
  bb run -- cargo build --release          a program with arguments, no shell involved
  bb run --sh 'make -j$BB_CPUS && make test'   a script for the worker's shell (sh on Linux
                                           and macOS, cmd on Windows; --shell bash|powershell|
                                           pwsh).  Single quotes: $BB_CPUS is the worker's to
                                           fill in, not the shell's that starts bb.
  --cwd sub/dir   --env NAME=VALUE   --timeout 30m
  The command runs in the same folder below the project's root that bb was started in.
  Windows and Linux workers differ: paths, shells, tool names.  Check \"os\" before you write
  the command.

Outputs.
  --out \"target/release/app\" --out \"build/**/*.exe\" --out reports/
  Patterns are relative to the project's root: a file, a folder (everything below it) or a
  glob (* within a name, ** across folders).  They come back to the same paths here, also
  when the command failed.  Without --out nothing comes back but the log.
  --no-fetch leaves them at the relay; bb fetch JOB [--to DIR] gets them later.

What is sent.  Every file of the project except what .gitignore and .bbignore (same syntax)
leave out; .git itself is left out.  So generated files that the build needs but git ignores
must be listed in buildbalancer.ini under [sync] include.  bb sync -w NAME sends without
running anything.  --no-sync runs in the workspace as the worker already has it.

Slots.  A worker holds one copy of a project per slot (default slot: main); jobs of one slot
run one after the other.  --slot NAME gives a job its own copy, e.g. one per target, so they
can run at the same time on one worker without sharing build folders.

Tasks.  buildbalancer.ini can name commands:
    [task.linux-release]
    run = cargo build --release
    needs = os=linux tool=cargo
    outputs = target/release/app
  bb tasks --json lists them, bb task NAME.. runs them (several: at once, spread over the
  workers), bb task --all runs every task.

Jobs.  bb jobs --json | bb status JOB --json | bb logs JOB [-f] | bb cancel JOB
  JOB is an id, the start of one, or \"last\".  States: submitted -> queued -> syncing ->
  running -> collecting -> done | failed | cancelled | killed | error.
    done       exit code 0
    failed     the command ended with another exit code (see exit_code and the log)
    killed     a limit ended it: time (--timeout), the worker's memory or disk limit (see error)
    error      it never ran or its result is incomplete: no such program on that worker, the
               sync failed, the worker restarted (see error; usually: fix and run again)
  Exit code of bb run / bb wait: the command's own, 130 cancelled, 137 killed, 1 error,
  124 not finished within --timeout.

Tidying up.  A worker keeps a project (its files and the build output that grew there) for
the next job.  bb clean -w NAME removes this project there, bb clean --all on every worker;
the next job simply sends it again.  Workers also drop projects nobody used for some days.

Rules of thumb.
  - Look at bb workers before every batch: owners change what they lend, machines go offline.
  - Respect \"idle\": cpu.idle and ram.idle are what running jobs leave free of what is lent.
  - One heavy job per worker, or several with --cpu that add up to what it lends.
  - Keep the main machine's own build for what must be local; everything independent can go out.
  - A first job on a worker sends the whole project (minutes for a large one); later ones
    send only what changed.
  - Do not retry a failed command blindly: read log_tail first.
";
