# SysPilot

SysPilot keeps a Windows PC the way you set it up. You choose privacy, security
and performance settings from a catalog where every entry cites Microsoft's own
documentation; a small service applies them, notices when an update or another
program changes one back, and restores it. Turning a setting off puts back
exactly what was there before.

It also reads the machine's sensors (temperatures, loads, fan speeds) with close
to no cost when nobody is looking.

> **Status: pre-release.** The service, the enforcement engine, the catalog and
> the command line work and are tested end to end. The desktop app and the
> hardware sensor providers are being built. See [the roadmap](#roadmap).

## What makes it different

- **Settings stay set.** Each setting is watched with a registry notification,
  so a change is noticed and undone within milliseconds, without polling.
- **Everything is reversible.** Before SysPilot writes anything it records what
  was there, including "nothing". Turning a tweak off, or uninstalling, restores
  that exactly.
- **It does not fight.** If something else keeps rewriting a value, SysPilot
  stops after a few rounds, reports the setting as *contested* and retries with
  a growing delay instead of burning CPU in a tug of war.
- **It tells the truth about Windows.** A setting Windows protects is reported
  as not enforceable. SysPilot never takes ownership of keys or bypasses
  protections to force a value.
- **Sourced catalog.** Every built-in tweak names the Microsoft page that
  documents the value it writes. Entries that weaken security say so and state
  the risk.
- **Nearly free when idle.** One event-driven process, no timers when nothing
  is contested and no client is connected, and a few megabytes of memory.

## Requirements

- Windows 10 or 11, x64.
- Administrator rights to install the service and to change settings.
- Optional: the [PawnIO](https://pawnio.eu/) driver for sensors that need
  hardware access (CPU temperatures, fan speeds, memory temperatures). SysPilot
  does not ship or install the driver. Without it everything else works and the
  features that need it are shown as unavailable.

## Using it

Until the first release there is no installer; [build from source](#building).

From an elevated terminal:

```console
> syspilot-svc install
SysPilot service installed and running.

> syspilot-svc list
performance.disk-idle-timeout-ac  off                Keep the hard disk on while plugged in
privacy.advertising-id            off (already set)  Turn off the advertising ID
security.audit-secure-boot        ok                 Check that Secure Boot is on
...

> syspilot-svc show privacy.advertising-id
> syspilot-svc enable privacy.advertising-id
> syspilot-svc history
> syspilot-svc uninstall
SysPilot service removed and settings restored.
```

`syspilot-svc --help` lists every command. The product service must be
installed from under `Program Files`; a LocalSystem service whose executable a
standard user could replace would hand that user the machine.

A profile is a TOML file. Besides catalog tweaks it can hold your own actions:
registry values, service start types, power settings and commands.

```toml
schema = 1
enabled = ["privacy.advertising-id", "security.llmnr"]

[[custom]]
id = "custom.keep-my-value"
title = "Keep my value"
action = { registry = { scope = "machine", key = 'SOFTWARE\Example', name = "Value", data = { dword = 1 } } }
```

```console
> syspilot-svc export profile.toml
> syspilot-svc apply profile.toml
```

## How it is built

Two programs with a strict split of privilege:

| Part              | Runs as                  | Does                                              |
| ----------------- | ------------------------ | ------------------------------------------------- |
| `syspilot-svc`    | LocalSystem service      | Applies and restores settings, reads sensors.     |
| `syspilot` (app)  | The signed-in user       | Shows state and sends requests. Holds no rights.  |

They talk over a local named pipe. Anyone signed in can read the catalog,
states and sensors; only an elevated administrator can change anything or read
the profile. The service decides that from the caller's token, not from what the
client claims.

| Crate              | Purpose                                                       |
| ------------------ | ------------------------------------------------------------- |
| `crates/proto`     | The contract: actions, catalog, profile, messages, client.    |
| `crates/engine`    | Apply, snapshot, revert, drift and contested handling.        |
| `crates/hw`        | Sensor providers and the sampling hub.                        |
| `crates/pawnio`    | Thin wrapper over the PawnIO driver interface.                |
| `crates/svc`       | The service, its installer and the command line.              |
| `crates/ui`        | The desktop app.                                              |
| `catalog/tweaks`   | The built-in tweaks, as data.                                 |

More in [`docs/`](docs/README.md): [architecture](docs/architecture/overview.md),
[enforcement](docs/architecture/enforcement.md),
[protocol](docs/protocol.md), [security model](docs/security-model.md),
[overhead budget](docs/architecture/overhead.md),
[decisions](docs/decisions.md) and the
[catalog authoring guide](docs/tweak-catalog.md).

## Building

Rust (the pinned toolchain installs itself through `rustup`) and the MSVC build
tools are the only requirements.

```console
> cargo build --release
> cargo test --workspace
```

Some tests install a temporary service named `SysPilot-Test` and write scratch
registry keys named `SysPilot-Test`; they need an elevated terminal and remove
everything they create. See [CONTRIBUTING.md](CONTRIBUTING.md).

## Roadmap

- [x] Enforcement engine with exact revert and contested handling
- [x] Service, installer and command line
- [x] Sourced tweak catalog
- [ ] Sensor providers (operating system, GPU, storage; CPU and board through PawnIO)
- [ ] Desktop app with Simple and Advanced levels
- [ ] Sensor logging to CSV from the app
- [ ] Signed installer and first release
- [ ] Fan curves (read-only fan speeds first; control only after validation on real boards)

## Safety

SysPilot changes system settings. The catalog is conservative and sourced, and
every change is reversible, but you are the one deciding what to enable. Read
the description and the side effects of a tweak before turning it on,
especially in the Advanced level. Vulnerabilities: see
[SECURITY.md](SECURITY.md).

## License

The service and the app are licensed under [GPL-3.0-or-later](LICENSE). The
reusable libraries (`proto`, `engine`, `pawnio`) are dual-licensed under
[MIT](LICENSE-MIT) or [Apache-2.0](LICENSE-APACHE). Third-party components are
listed in [THIRD-PARTY.md](THIRD-PARTY.md).
