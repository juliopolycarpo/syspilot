# Overhead budget

The service is always running. A tool that promises a faster, quieter machine
has to be nearly invisible itself, so its cost is budgeted and the budget is
treated as a requirement.

## Budget

| Measure                                   | Budget          |
| ----------------------------------------- | --------------- |
| Wake-ups while idle                       | none            |
| Private memory while idle                 | at most 5 MB    |
| Release executable (`syspilot-svc.exe`)   | at most 1.5 MB  |
| Threads while idle                        | a handful       |
| Disk writes while idle                    | none            |

*Idle* means: no client connected, logging off, nothing contested, no
`interval` trigger in the profile. That is the state the service is in almost
all the time.

## How the design meets it

- **No timers by default.** The event loop waits on one handle with an infinite
  timeout. A timeout exists only while something is due: a contested retry, a
  paused watch, an `interval` trigger, or sensor sampling for a connected
  client or the logger.
- **Notifications instead of polling.** Drift is detected by
  `RegNotifyChangeKeyValue`. The kernel wakes the service when a watched key
  changes and at no other time.
- **Sensors on demand, in another process.** The service never reads hardware
  itself. When a client subscribes to sensor values it starts a *sensor
  worker* (`syspilot-sensors.exe`, next to the service), asks it for a
  sample once a second and lets it exit when the last subscriber leaves. With
  the app closed and logging off, no sensor is read and no worker exists.
  The reason is measured: the first use of the Windows performance counters
  costs 6.7 MB of private memory that is never returned, and takes over a
  second. In a worker that cost ends with the worker.
- **Bounded reaction to noise.** A key that changes constantly is paused; a
  value something else keeps rewriting becomes contested with an exponential
  back-off. Neither can turn into a busy loop.
- **Batched log writes.** The CSV logger buffers rows and flushes once a
  minute.
- **Few dependencies.** The service links `windows-sys`, `serde`, `toml` and
  `postcard`. No async runtime, no COM, no WMI. It is built for size
  (`opt-level = "s"`, fat LTO, `panic = "abort"`, static CRT).

Adding a dependency to `crates/svc` requires an entry in
[decisions](../decisions.md) with its size impact.

## Measuring

Private memory and thread count of a running instance:

```powershell
Get-Process syspilot-svc |
  Select-Object Id, @{n='PrivateMB'; e={[math]::Round($_.PrivateMemorySize64/1MB, 2)}},
                @{n='Threads'; e={$_.Threads.Count}}
```

Wake-ups: record a trace with Windows Performance Recorder (`wpr -start
GeneralProfile`), leave the machine idle for a few minutes, stop it, and look
at the context-switch count for `syspilot-svc.exe` in Windows Performance
Analyzer. Idle should show none.

Executable size is checked in CI.

## Measurements

| Date       | Build                                        | Private memory | Threads | Executable |
| ---------- | -------------------------------------------- | -------------- | ------- | ---------- |
| 2026-11-23 | release, 58 tweaks loaded, no sensor providers | 1.6 MB       | 5       | 1.31 MB    |
| 2026-11-23 | release, with the Windows sensor providers   | 1.8 MB idle, 1.9 MB while sampling | 5 | 1.40 MB |

Measured on a console run. The second row is after a status request, a sensor
listing and a session of the app: the service itself does not grow. The sensor
worker, which exists only while values are wanted, uses 8.5 to 8.8 MB; its
executable is 0.42 MB and has no size budget.
