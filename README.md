# lightspeed-provider-smolvm

A [Lightspeed](https://github.com/cptspacemanspiff/lightspeed) environment
provider that runs each environment as a [smolvm](https://github.com/smol-machines/smolvm)
microVM. It implements the same controller contract as Lightspeed's Incus
provider, so Lightspeed registers it like any other provider.

## How it fits together

```
Lightspeed ──ws /control──────────────▶ provider ──smolvm CLI──▶ machines
Lightspeed ──ws /routes/{u}/{b}/{e}/{i}/{t}─▶ provider
                                          │ ownership check, secretEnv rewrite
                                          ▼
                     <stateDir>/vms/<target>/envd.sock   (host AF_UNIX, 0700 dir)
                                          │ smolvm --expose-socket (vsock bridge)
                                          ▼
                     lightspeed-envd on /var/lib/lightspeed-envd/envd.sock in the guest
```

- **The provider is the only network-facing component.** Lightspeed never
  connects to a machine directly. The guest's envd is reachable only through a
  host socket in the provider's private state directory, so access is governed
  by the provider's OS account. The guest needs no network for this channel.
- **Guests are generic.** envd holds no tenant identity. Ownership (universe,
  binding, environment, incarnation, request, template) lives in the
  provider's store and on each machine as `lightspeed.*` labels.
- **Network is off unless allowlisted.** A machine gets `--net` only together
  with `--allow-host`/`--allow-host-pattern` entries: the template's egress
  hosts plus its credentials' hosts.
- **Credentials never enter the guest.** Lightspeed sends credential values
  in `secretEnv` on every `process/start` and `job/start`. The relay strips
  each value that has a credential rule and writes it to
  `<stateDir>/vms/<target>/secrets/<ENV>`. The machine was created with
  `--credential` and `--secret-file` for that file, so smolvm re-reads it on
  every request and puts it only into HTTPS request headers for the rule's
  hosts. The guest sees a placeholder. A `secretEnv` entry with no rule is
  refused by default (`unknownSecretEnv: "reject"`).

## Persistence

| State | Where |
|---|---|
| Environment records, power intent, idle policy | Lightspeed (Postgres) |
| Who owns each machine | `<stateDir>/targets/<id>.json`, mirrored as machine labels |
| Machine disks and config | smolvm's own data directory |
| Credential values | `<stateDir>/vms/<id>/secrets/` (latest value only) |
| Durable user work | Lightspeed VFS (`fs/materialize`, `fs/capture`) |

On startup the provider reconciles its store with `smolvm machine ls --json`:
- An interrupted create is marked failed, or active if the machine exists.
- An interrupted close is finished.
- A machine without a record is re-recorded from its labels.
- A machine without usable labels is reported and left alone.

## envd injection

Templates use ordinary OCI images (`ubuntu`, `alpine`, a project's own dev
image) with no `lightspeed-envd` installed. The provider supplies it:

- `envd.binaries` in the config names a **static (musl)** envd per guest
  architecture, pinned by sha256. Only `x86_64` is supported so far.
- At startup the provider verifies the hash and copies the binary to
  `<stateDir>/envd/<sha256>/lightspeed-envd`. Published directories are
  never modified, and ones no machine references are removed at startup.
- Each machine mounts that directory read-only at `/opt/lightspeed-envd`
  and runs envd as its workload command, so envd sees the image's
  filesystem and the processes it starts inherit smolvm's credential
  placeholders.
- envd runs as a dedicated unprivileged `lightspeed-envd` user (home and
  state directory `/var/lib/lightspeed-envd`), created on first start by an
  init command the provider adds before the template's own `init`. Agent
  commands run as that user, so its access is the agent's access.
  `"envdUser": "root"` skips the user; the microVM is then the only
  isolation boundary.
- A machine keeps the envd it was created with, since smolvm reuses the
  create-time mount on every restart. To upgrade envd, create new machines.
  envd's `upgrade` subcommand fails on the read-only mount, by design.

Image requirements: a Linux userland with `/bin/sh` (envd runs commands
through it), `useradd` or BusyBox `adduser` unless `envdUser` is `root`, and
whatever tools agents need. envd itself needs no particular libc.

A template with `"envdSource": "image"` instead starts an envd the image
ships, through its own `command`.

To build envd for local use (needs `musl-tools` and the
`x86_64-unknown-linux-musl` Rust target):

```bash
./scripts/build-envd.sh     # writes out/lightspeed-envd-x86_64-<sha> and out/provider.json
cargo run -- --config out/provider.json
```

## Configuration

See [config.example.json](config.example.json). Notes:
- Images must be pinned by digest.
- `envd.binaries.<arch>` must be set when any template injects envd.
- `stateDir` is optional. It defaults to
  `~/.local/state/lightspeed-provider-smolvm` (or `$XDG_STATE_HOME/...`,
  `%LOCALAPPDATA%\...` on Windows). It must be absolute, at most 64 bytes
  (AF_UNIX path limits), and owned by the user running the provider. That
  must be the same user that owns the smolvm machines, because smolvm keeps
  its machine database per user.
- Templates opt in to credential rules by name.
- `envd.kind` is `exposedSocket` (preferred) or `loopbackForward`
  (development only).

`config.example.json` points at a placeholder envd path and hash, so it
won't start as is. Run the config `scripts/build-envd.sh` writes instead:

```bash
cargo run -- --config out/provider.json
curl http://127.0.0.1:19190/health
```

Logs go to stdout. `RUST_LOG` sets the level (default `info`; `debug` adds
every smolvm command with its duration and stderr, plus per-RPC timings).
`--log-format json` (or `LIGHTSPEED_SMOLVM_PROVIDER_LOG_FORMAT=json`) prints
one JSON object per line for a log collector. Credential values and data
frames are never logged.

Register it in Lightspeed under **Environment providers → Register provider**
with transport `WebSocket` and endpoint `ws://<host>:19190/control`.

## Docker

`docker/` holds an image and a Compose service for it. The image compiles
nothing: `docker/stage` builds the provider and a static envd (from
`../lightspeed`, `LIGHTSPEED_DIR`) on the host and copies them, with the host's
smolvm install, into `docker/dist/`. The entrypoint adds the image's envd to
the config mounted at `/etc/lightspeed/smolvm-provider.json`.

```bash
docker/stage && docker compose -f docker/compose.yaml build
```

`docker/compose.yaml` passes `/dev/kvm` through and adds its group (`KVM_GID`).
It leaves out the published port, the config and the `/data` mount (smolvm's
machines and the provider's state), for a project to add by `extends`.
[lightspeed-integration](../lightspeed-integration) does this.

## Status

The controller logic, store, reconciliation, channel and credential rewriting
are unit tested. Nothing has run against a real smolvm yet. Before it can:

1. **envd Unix-socket listener.** Needs Lightspeed's `envd-unix-listener`
   branch (`--listen unix:///<path>`) until it merges.
2. **First real run.** Check injected envd end to end on `ubuntu` and
   `alpine`: user creation, socket access from smolvm's namespace helper
   with envd's 0600 socket, and restarts keeping the same envd.
3. **Placeholder inheritance.** smolvm injects credential placeholders into
   the workload environment. envd must be that workload (the template
   `command`) so the processes it starts inherit them. Check this end to end.
4. **Windows.** smolvm uses AF_UNIX on Windows too, but tokio has no
   AF_UNIX client there, so `channel::connect_host_socket` is a stub.
5. **macOS.** The `smolvm` binary needs the Hypervisor.framework
   entitlement (see `third_party/smolvm/docs/limitations.md`).

Not implemented: `adoptTarget`, public ingress, forks (`machine branch`).

## smolvm

`third_party/smolvm` is a submodule pinned to the smolvm revision the CLI
flags in [src/smolvm.rs](src/smolvm.rs) were checked against
(`src/cli/machine.rs` upstream). When you bump it, re-check the flags.

```bash
git submodule update --init
```

The Lightspeed wire types come from a sibling checkout
(`../lightspeed/crates/environment-protocol`). Switch to a pinned git
dependency for CI.
