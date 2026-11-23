# Architecture overview

SysPilot is two programs. A service does everything that needs rights; an app
shows state and asks for changes. This page describes the service side, which
exists today, and the seams the app plugs into.

```
            signed-in user                      LocalSystem
  ┌───────────────────────────┐      ┌─────────────────────────────────┐
  │ syspilot (app)            │      │ syspilot-svc (service)          │
  │ syspilot-svc <command>    │      │                                 │
  │                           │ pipe │  pipe threads ─┐                │
  │  Client ──────────────────┼──────┼─▶              ├─▶ event queue  │
  │                           │      │  registry      │        │       │
  │  SectionReader ◀──────────┼──────┼─ watches ──────┘        ▼       │
  │        shared memory      │      │  service controls   event loop  │
  └───────────────────────────┘      │                     │    │      │
                                     │              Enforcer   Hub     │
                                     │                 │        │      │
                                     └─────────────────┼────────┼──────┘
                                              registry, services,  sensors
                                              power, commands
```

## Crates

| Crate    | Depends on         | Role                                                              |
| -------- | ------------------ | ----------------------------------------------------------------- |
| `proto`  | —                  | Data contract and the client. No behaviour beyond validation.     |
| `engine` | `proto`            | The enforcement state machine and its Win32 backends.             |
| `pawnio` | —                  | The PawnIO device interface.                                      |
| `hw`     | `proto`, `pawnio`  | Sensor providers and the hub that samples them.                   |
| `svc`    | all of the above   | Event loop, pipe server, installer, command line.                 |
| `ui`     | `proto`            | The desktop app. It links none of the privileged code.            |

`proto` is the only crate both sides link. If a change does not touch `proto`,
it cannot change what the app and the service say to each other.

## The service

`syspilot-svc` is one executable with three faces: the Windows service
(`service`, started by the service control manager), a console run for
development (`run`), and a command-line client (everything else).

### One loop

All state belongs to one thread, the event loop in `svc/src/host.rs`. Everything
else only puts events on a queue:

- **Pipe threads** (`pipe.rs`). One listener and one thread per connection do
  the blocking pipe I/O. A request is forwarded with a reply channel; the thread
  writes the answer back. A client that stops reading stalls only its own
  thread.
- **Registry watches** (`watch.rs`). Each watched key has an event handle armed
  with `RegNotifyChangeKeyValue` and a thread-pool wait. The pool multiplexes
  any number of handles on its own threads, so there is no 64-handle limit and
  no thread per key.
- **Service controls** (`service.rs`). Stop, session logon, suspend and resume.

The loop waits on the queue's event handle with a timeout equal to the nearest
thing that is due: a contested retry, a paused watch, or the next sensor
sample. When none of those exist the timeout is infinite and the process does
not wake up at all.

### Core and host

`core.rs` holds the decisions and no Win32: catalog, profile and enforcer
behind `handle(request, elevated) -> (response, effects)`. It is tested against
a fake operating system. `host.rs` is the Win32 shell around it: it routes
events, keeps watches in line with what the enforcer wants watched, opens
sensor providers when someone needs values and tells subscribers what changed.

### Data on disk

```
%ProgramData%\SysPilot\
  profile.toml          what is enabled, custom actions, logging settings
  state\applied.toml    snapshots of every value before it was changed
  state\history.toml    recent enforcement events
  logs\                 sensor logs (CSV), readable by users
```

Files are replaced atomically (write a sibling, rename over). The directory is
owned by Administrators with a protected ACL; see the
[security model](../security-model.md).

The catalog is not on disk. It is compiled into the service.

### Sensors

Sensor providers implement one trait (`hw::Provider`) and are grouped by a
`Hub`. The service opens them only while a client subscribed to sensor values
or the logger needs them, samples once per second, and publishes the values to
a read-only shared-memory section. The app reads the section directly, so a
live graph costs no pipe traffic. When the last interested party leaves, the
providers are dropped and sampling stops.

Whether the PawnIO driver is present is checked on every status request, so
installing it while the service runs is picked up without a restart.

## Instances

Every name the service uses (service name, pipe, shared-memory section, data
directory) derives from an *instance* name. The product is the instance
`SysPilot`. Tests and development use other names (`--instance SysPilot-Test`),
which is what lets the end-to-end tests install a real service next to an
installed product without touching it.
