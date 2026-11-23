# Decisions

Choices that were not obvious, newest last. Each entry says what was decided,
why, and what was given up. An entry is never edited to say something else; a
later entry supersedes it.

## 1. Two processes, one contract crate

**Decision.** A LocalSystem service does all privileged work. The app runs as
the user and links only `proto`.

**Why.** The app is large, draws untrusted text and will change often. Keeping
it unprivileged means a bug in it cannot change the system. A single shared
crate makes the boundary reviewable in one place.

**Cost.** Everything the app shows crosses a pipe.

## 2. Rust, no async runtime, four dependencies in the service

**Decision.** The service uses threads and blocking Win32 calls. It depends on
`windows-sys`, `serde`, `toml` and `postcard`.

**Why.** The workload is a handful of clients and a few dozen handles. An async
runtime would add size and background threads for no benefit, and the
[overhead budget](architecture/overhead.md) is a requirement.

**Cost.** One thread per client connection. Connections are capped at 32.

## 3. Windows built-ins only, no WMI or child processes for built-in work

**Decision.** Registry, service, power and sensor access use the Win32 APIs
directly. The service starts a process only for a `command` action.

**Why.** WMI and COM pull in megabytes of working set and their own threads.
Shelling out to `reg.exe` or `powercfg.exe` is slow, locale-dependent and hard
to make safe.

## 4. All drift detection goes through registry notifications

**Decision.** Service start types and power settings are watched through the
registry keys that store them, with the same mechanism as registry values.

**Why.** One mechanism, no polling, no per-kind watcher. The alternative
(service change notifications, power setting callbacks) covers less and needs
three code paths.

**Cost.** A power-scheme change notifies for the whole schemes subtree, so the
engine re-checks all power actions when any power setting changes.

## 5. The catalog is compiled into the service

**Decision.** `catalog/tweaks/*.toml` is embedded with `include_str!`.

**Why.** What the service may change as LocalSystem should be fixed by the
binary. A catalog on disk would be one more file whose ACL has to be right, and
one more thing that can be out of sync with the engine.

**Cost.** A catalog change needs a new release. Custom actions cover the gap.

## 6. Reading the profile and the history requires elevation

**Decision.** `Profile` and `History` are privileged requests, like the ones
that write.

**Why.** Custom actions can contain command lines and paths, and the history
is a record of what changed on the machine. Neither is a standard user's
business on a shared PC.

**Cost.** The app, running unelevated, can show states but not custom action
titles until the user elevates.

## 7. The pipe is open to authenticated users, not to the interactive group

**Decision.** The pipe and the sensor section grant read access to
Authenticated Users, and the pipe rejects remote clients.

**Why.** Granting only Interactive would be tighter, but processes in a
service session (continuous integration, scheduled diagnostics) could then not
read status at all, and the end-to-end tests could not run in CI. What these
callers can reach is read-only and not sensitive.

## 8. Test instances are a first-class concept

**Decision.** Every external name derives from an instance name; the product
is the instance `SysPilot`.

**Why.** The end-to-end tests install a real service and talk over a real
pipe. With instances they can do that next to an installed product, and a
developer can run a console instance without touching the service.

**Cost.** The product-only rule (install from Program Files) has to be keyed on
the instance name.

## 9. Explicit owner on the data directory

**Decision.** The data directory is created owned by Administrators, and an
existing directory with any other owner is refused.

**Why.** See the [security model](security-model.md#the-data-directory). Found
by a test: a directory created by an elevated administrator is owned by the
user, not by Administrators, so the ownership check rejected the service's own
directory on the second start.

## 10. The installer is `syspilot-svc install` plus a script

**Decision.** The executable registers itself in place. Copying files into
Program Files is a PowerShell script's job.

**Why.** No installer framework to depend on or to trust, and the service
registration logic is tested like everything else.

**Cost.** No Add/Remove Programs entry until the script writes one.

## 11. Slint for the app

**Decision.** The desktop app uses Slint with the Fluent style.

**Why.** Compared in [research/ui-toolkits.md](research/ui-toolkits.md): small
self-contained binaries, no runtime to install, and views can be rendered
headless for review.

**Cost.** Slint's free licence for this use is GPLv3, which fixes the licence
of the app and the service.

*Superseded by 15.*

## 12. No hardware writes in version 1

**Decision.** Version 1 reads sensors and never writes to hardware. Fan control
code is behind a cargo feature that release builds do not enable.

**Why.** A wrong write to a Super I/O chip or an embedded controller can stop
fans or hang a machine. That needs validation per board, which a first release
cannot claim.

## 13. PawnIO is detected, never distributed

**Decision.** SysPilot does not ship or install the driver. It detects it,
shows which features need it and links to its official page.

**Why.** Installing a kernel driver is the user's decision and the driver has
its own signed installer and update path.

## 14. Conventional Commits, signed and signed off

**Decision.** Commits follow Conventional Commits, are SSH-signed and carry a
DCO sign-off. Changes land by fast-forward, so every commit on `main` is signed
by its author.

## 15. windows-reactor (WinUI 3) for the app

**Decision.** The desktop app uses `windows-reactor` with a self-contained
Windows App SDK runtime. This supersedes decision 11.

**Why.** The owner asked for it, and a spike showed that every gate holds on
a real machine: elevated, unpackaged, nothing to install, talking to the
service ([research/ui-toolkits.md](research/ui-toolkits.md)). Real WinUI
controls bring the Windows look, text rendering, keyboard behaviour and
accessibility tree for free.

**Cost.** About 57 MB of runtime files next to the app executable; the
service does not load them. The toolkit is a preview, so its version is pinned
and a new release may need view changes. Building for the first time needs
network access to fetch the runtime packages. Screenshots for the visual
review need an interactive desktop.

**Licence.** The toolkit no longer dictates one (Slint's free option was
GPLv3). The app and the service stay GPL-3.0-or-later by choice.

## 16. Sensors are read by a worker process

**Decision.** The service does not read hardware. A separate executable,
`syspilot-sensors.exe`, does it while values are wanted and exits afterwards.

**Why.** Measured on the reference machine: adding the first Windows
performance counter costs 6.7 MB of private memory, which is not returned when
the counter is closed, and takes 1.1 s. In the service that would break the
5 MB budget for good after the app was opened once, and would stall the event
loop that enforces settings. In a worker both costs end with the worker. It
also means a hung SMBus transaction or driver call cannot hang the service,
and the service executable carries no hardware code.

**Cost.** A second executable to ship, a small protocol between the two, and
about 1.2 s before the first values appear after the app opens. The service
still waits for the worker's report when it starts one; making that wait
asynchronous is left for later.

## 17. What a build downloads, and where it puts it

**Decision.** Beyond crates, a build fetches two things, both pinned and
neither committed:

| What | Fetched by | Lands in |
| ---- | ---------- | -------- |
| Windows App SDK runtime 2.4.0 and the WebView2 projection (NuGet packages) | the app's build script, through `windows-reactor-setup` | `%LOCALAPPDATA%\windows-reactor-setup` (a per-user cache), then copied next to `syspilot.exe` in `target` |
| The PawnIO.Modules release, checked by SHA-256 | `cargo xtask fetch-modules` | `pawnio-modules\` in the repository root (ignored by git) |

**Why.** The runtime ships with the app so that nothing has to be installed
on the user's machine (decision 15); the modules are someone else's signed
LGPL files (decision 13). Listing them here is the whole inventory: building
SysPilot installs no program, driver, service or system-wide component.
Deleting the two directories undoes it.

## 18. Fan curves run in the worker, behind a feature

**Decision.** The curves of a profile are followed by the sensor worker, not by
the service, and only in a build with the `fan-control` cargo feature, which
is off by default and in every release. The feature exists in each executable
that takes part: `syspilot-sensors/fan-control` for the worker that writes
and `syspilot-svc/fan-control` for the service that accepts curves, which
also enables `syspilot-hw/fan-control`. The service sends the enabled curves
and forwards sleep and wake. The worker decides whether a board may be
controlled, steps the curves from its own sensor samples and hands every fan
back to the BIOS when its input closes. This builds on decision 12; it does
not lift it. No board is validated, so no build writes to a fan.

**Why.** The worker already sits next to the registers and reads the
sensors, so a step needs no round trip. The end of its input, which is what a
stopped or crashed service looks like, is a dead-man's switch that the service
itself could not provide: a crashed service cannot restore anything, a worker
whose pipe closed can. The worker can also step by itself if the service
stalls, and the panic hook that restores the fans belongs to the process that
holds the controller (the release profile aborts on panic). The service
stays free of hardware code, as decision 16 asked, and idle: with no enabled
curve it keeps no worker and samples nothing. Its first plan goes out even
with no curve, so that whichever worker runs hands back the channels a crash
left in manual mode.

**Cost.** The control loop lives in the process the service may replace when
it misses a sample, so a replaced worker has to give the fans back in the
two seconds it gets before it is killed, and a killed one leaves the chip at
its last duty until the next session restores the saved registers from a file.
That file, `fan-originals.toml` in the protected state directory, is the one
record of the BIOS mode, so it is written atomically before the first write
and never overwritten when it does not parse. A hung hardware read in the
worker is not covered by its two-second step. A build that follows curves
needs both features, and a worker built without it reports the board as not
controllable, so a half-enabled build shows "not in this build" and writes
nothing. Protocol version 2 adds the fan status.
