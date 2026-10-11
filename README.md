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
                     lightspeed-envd on /run/lightspeed/envd.sock in the guest
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

## Configuration

See [config.example.json](config.example.json). Notes:
- Images must be pinned by digest.
- `stateDir` must be absolute and short (AF_UNIX path limits).
- Templates opt in to credential rules by name.
- `envd.kind` is `exposedSocket` (preferred) or `loopbackForward`
  (development only).

```bash
cargo run -- --config config.example.json
curl http://127.0.0.1:19190/health
```

Register it in Lightspeed under **Environment providers → Register provider**
with transport `WebSocket` and endpoint `ws://<host>:19190/control`.

## Status

The controller logic, store, reconciliation, channel and credential rewriting
are unit tested. Nothing has run against a real smolvm yet. Before it can:

1. **envd Unix-socket listener.** envd only listens on TCP today. The
   `exposedSocket` channel needs a `--listen-unix <path>` option in
   `crates/environment-daemon`. Until then, use `loopbackForward`, but note
   that smolvm chooses the host bind address for published ports.
2. **Guest image.** Build an OCI image with `lightspeed-envd` and publish it
   by digest. Apple Silicon hosts need an aarch64 Linux envd.
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
