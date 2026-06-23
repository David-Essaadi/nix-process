# nix-process

A small, TUI-less process manager for services defined in a `flake.nix` — like
`devenv up`, but it just streams prefixed logs straight to your terminal and
**always** tears every process down cleanly on exit.

## What it does

- Reads a `processes` attribute out of your flake (`nix eval .#processes --json`).
- Starts each service in its **own process group**, so the whole tree can be
  signalled at once.
- Waits for each service to become healthy (TCP port / HTTP / custom command)
  before starting the things that `depends_on` it.
- Streams interleaved, name-prefixed, color-coded stdout/stderr — no TUI.
- Guarantees shutdown: on Ctrl-C every group gets `SIGTERM`, and anything still
  alive after a grace period gets `SIGKILL`. A second Ctrl-C force-kills now.
- Recovers from a previous crash: each child's pgid is written to a state file;
  the next `up` reaps any orphaned groups left behind (guarding against PID
  reuse via the process start time in `/proc`).

## Defining processes

In your `flake.nix`:

```nix
{
  outputs = { self, nixpkgs }:
    let pkgs = nixpkgs.legacyPackages.x86_64-linux; in
    {
      processes = {
        db = {
          command = "${pkgs.postgresql}/bin/postgres -D ./pgdata -k /tmp";
          health_check.command = "${pkgs.postgresql}/bin/pg_isready -h /tmp";
        };
        web = {
          command = "${pkgs.nodejs}/bin/node server.js";
          cwd = "./app";
          env = { PORT = "3000"; };
          depends_on = [ "db" ];
          health_check.tcp_port = 3000;     # or .http = "http://127.0.0.1:3000/health"
          shutdown_signal = "SIGINT";       # optional; default SIGTERM
        };
      };
    };
}
```

### Per-process fields

| field             | required | meaning                                                  |
|-------------------|----------|----------------------------------------------------------|
| `command`         | yes      | shell command, run via `sh -c`                           |
| `cwd`             | no       | working directory                                        |
| `env`             | no       | extra environment variables (string → string)            |
| `depends_on`      | no       | names that must be **ready** before this one starts      |
| `oneshot`         | no       | run-to-completion task (see below); default `false`      |
| `shutdown_signal` | no       | signal sent on shutdown (default `SIGTERM`)              |
| `health_check`    | no       | readiness probe; omitted ⇒ ready as soon as it starts    |

### Oneshot tasks

A normal process is long-running: it exiting is treated as a failure and brings
everything down. A **oneshot** (`oneshot = true`) is instead expected to run to
completion — a setup step, a migration, a build:

- It runs no health check; its **successful exit (code 0) is its "ready" signal**,
  so anything that `depends_on` it waits for it to finish.
- A **non-zero exit is fatal** and aborts the run (exit code 1).
- A completed oneshot is not a "crash" and is skipped during shutdown.

```nix
processes = {
  migrate = { command = "mix ecto.migrate"; oneshot = true; depends_on = [ "db" ]; };
  web     = { command = "mix phx.server"; depends_on = [ "migrate" ]; };
};
```

### Health checks

Set exactly one probe:

- `tcp_port = 3000` — ready when a TCP connect to `127.0.0.1:3000` succeeds.
- `http = "http://127.0.0.1:3000/health"` — ready on HTTP status `< 400`
  (plain `http://` only; for HTTPS use a `command` probe with curl).
- `command = "pg_isready ..."` — ready when the command exits `0`.

Plus optional `timeout_seconds` (default 60) and `interval_seconds` (default 1).

## Tests (run a command with services up)

Declare one-off commands that need some services running — tests, seeds, a
console. nix-process brings up the **transitive closure** of the named services
(dep-ordered, health-gated), runs the command in the foreground, then tears the
services down and exits with the command's status. This is the `devenv test` /
`process-compose run` capability.

```nix
tests.backend = {
  command = "mix test";        # run with inherited stdio (raw, interactive)
  services = [ "db" ];         # db (+ its depends_on) brought up first
  # cwd / env optional
};
```

```sh
nix-process test backend       # db up → mix test → db down; exits with mix's code
```

## Usage

```
nix-process up [flags]           start all processes and supervise them
nix-process test <name> [flags]  bring up a test's services, run it, tear down
nix-process down [flags]         clean up orphaned processes from a prior run

Flags:
  --flake <ref>      flake reference (default ".")
  --attr <attr>      attribute holding the process map (default "processes")
  --tests-attr <a>   attribute holding the test map (default "tests")
  --grace-seconds N  seconds to wait after SIGTERM before SIGKILL (default 10)
  --state <path>     state file path (default ".nix-process/state.json")
```

Run it from your flake directory:

```sh
nix run github:you/nix-process#up      # or, installed: nix-process up
```

## Try the example

```sh
cd example
nix develop                            # brings python3/bash onto PATH
nix run ..#up                          # starts ticker → web → worker
# ...watch the prefixed logs, then Ctrl-C for a clean shutdown.

# Exercise the SIGKILL escalation (a process that ignores SIGTERM):
nix run ..#up -- --attr stubbornDemo --grace-seconds 3
```

## Building / hacking

```sh
nix develop          # cargo, rustc, clippy, rust-analyzer
cargo build
cargo clippy
nix build            # produces ./result/bin/nix-process
```

## How shutdown & recovery work

- Every child is started with `setpgid(0,0)` so it leads its own process group.
- On the first `SIGINT`/`SIGTERM`, the supervisor sends each group its
  `shutdown_signal`, waits up to `--grace-seconds`, then `SIGKILL`s survivors.
- A second signal immediately `SIGKILL`s every group and exits `130`.
- The state file (`.nix-process/state.json`) records each child's `pid`, `pgid`
  and start-time. A later `up`/`down` reads any stale file and kills leftover
  groups, skipping entries whose PID has been recycled.
```
