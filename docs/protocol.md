# Protocol

The app and the command line talk to the service over a named pipe and read
sensor values from a shared-memory section. Both are defined in `crates/proto`;
this page is the map, the code is the reference.

## Pipe

| Property   | Value                                                             |
| ---------- | ----------------------------------------------------------------- |
| Name       | `\\.\pipe\<instance>`, e.g. `\\.\pipe\SysPilot`                   |
| Mode       | Byte stream, local clients only                                   |
| Framing    | 4-byte little-endian length, then that many bytes                 |
| Encoding   | [postcard](https://postcard.jamesmunns.com/) of `Request` / `Response` |
| Frame limit| 1 MiB; a longer frame closes the connection                       |

### Handshake

The first frame on a connection must be `Hello { protocol }`. The service
answers `Hello(Welcome { protocol, version, elevated })`, or
`Error(ProtocolMismatch)` if the versions differ. `PROTOCOL_VERSION` is bumped
whenever a message changes shape; there is no negotiation, the app and the
service are released together. Version 2 added `ServiceStatus.fans`; version 3
added `Profile.updates`, `ServiceStatus.update` and the three update requests.

`elevated` tells the client how the service sees it. It is computed once per
connection from the client's token and does not change afterwards.

Before the handshake the client checks that the pipe is owned by LocalSystem or
Administrators, and refuses to talk to anything else.

### Requests

After the handshake the client sends one request and reads one response, in
strict alternation.

| Request                         | Response      | Needs elevation |
| ------------------------------- | ------------- | --------------- |
| `Status`                        | `Status`      | no              |
| `Catalog`                       | `Catalog`     | no              |
| `States`                        | `States`      | no              |
| `Sensors`                       | `Sensors`     | no              |
| `SystemInfo`                    | `SystemInfo`  | no              |
| `Subscribe { sensors }`         | `Subscribed`  | no              |
| `Profile`                       | `Profile`     | yes             |
| `History { limit }`             | `History`     | yes             |
| `SetTweak { id, enabled }`      | `Ok`          | yes             |
| `PutCustom(action)`             | `Ok`          | yes             |
| `RemoveCustom { id }`           | `Ok`          | yes             |
| `Reapply { id }`                | `Ok`          | yes             |
| `SetProfile(profile)`           | `Ok`          | yes             |
| `SetLogging(config)`            | `Ok`          | yes             |
| `SetFans(curves)`               | `Ok`          | yes             |
| `ClearHistory`                  | `Ok`          | yes             |
| `Shutdown`                      | `Ok`          | yes (console runs only) |
| `SetUpdates(mode)`              | `Ok`          | yes             |
| `CheckForUpdate`                | `Ok`          | yes             |
| `InstallUpdate`                 | `Ok`          | yes             |

Any request can instead be answered with `Error { code, message }`. The message
is written for the person using the app and can be shown as is.

| Code               | Meaning                                              |
| ------------------ | ---------------------------------------------------- |
| `ProtocolMismatch` | App and service are from different releases.         |
| `AccessDenied`     | The request needs an elevated administrator.         |
| `NotFound`         | No entry with that ID.                               |
| `Invalid`          | The request is malformed; the message names the field. |
| `Failed`           | The service could not do it (for example, save).     |
| `Unsupported`      | Not available in this build or on this Windows.      |

The profile and the history are privileged to read, not only to write: custom
actions can contain command lines, and the history shows what changed on the
machine.

### Events

`Subscribe` turns the connection into a one-way stream of
`Response::Event(...)` frames. A client that wants both sends requests on a
second connection.

| Event            | Sent when                                                      |
| ---------------- | -------------------------------------------------------------- |
| `StatesChanged`  | An entry's state or the history changed.                       |
| `ProfileChanged` | The stored profile changed.                                    |
| `SensorsChanged` | The set of sensors changed; read `Sensors` again.              |
| `StatusChanged`  | Something in `Status` changed.                                 |
| `Sampled`        | A new sample is in the section (sensor subscribers only).      |
| `Stopping`       | The service is shutting down.                                  |

Events say *that* something changed, not *what*. The client re-reads what it
shows. That keeps events tiny and makes a missed event harmless.

A subscriber that does not read is dropped when 64 events are waiting for it.

Subscribing with `sensors: true` is what makes the service sample. When the
last such subscriber disconnects, sampling stops.

### Fans in the status

`Status` carries `fan_control` (whether this service accepts curves) and
`fans`, a `FanStatus`:

| Field          | Content                                                                 |
| -------------- | ----------------------------------------------------------------------- |
| `availability` | `NotInBuild` (no `fan-control` feature, as in every release), `ReadOnly { reason }` (the reason is in words for the user: PawnIO missing, board unknown, entry not validated) or `Controllable`. |
| `board`        | The board as the catalog names it, or its SMBIOS name; empty when unknown. |
| `channels`     | The board entry's channels, plus any profile curve on a channel the entry does not list. Empty for a board that is not in the catalog. |

Each channel is a `FanChannelStatus`:

| Field             | Content                                                            |
| ----------------- | ------------------------------------------------------------------ |
| `channel`         | The key of its duty sensor, `board.pwm<N>`. A profile curve names the channel by it. |
| `label`           | The header's name from the catalog entry.                          |
| `tach_sensor`     | The key of its tachometer sensor, e.g. `board.fan2`, if the entry names one. |
| `min_duty_pct`    | The lowest duty the entry allows.                                  |
| `curve`, `sources`| The profile's curve and the sensor keys that drive it; empty if none. |
| `active`          | The profile enables the curve and the board is `Controllable`.     |
| `target_duty_pct` | The duty last asked for; `None` while the BIOS holds the channel.  |
| `phase`           | `Bios`, `Manual` (following the curve), `FailSafe` (a source failed, held at 100 %), `ReleasePending` (handing back failed, retried) or `Blocked` (found in manual mode with no saved original, left alone). |
| `problem`         | The last thing that went wrong with the channel, if still so.      |

`StatusChanged` is sent when a channel's phase or problem changes or its
target moves by five points. Clients re-read `Status`; the sensor values of
the fan (`board.fanN`, `board.pwmN`) travel in the section like any other.

### Updates in the status

`Status` carries `update`, an `UpdateStatus`:

| Field          | Content                                                                 |
| -------------- | ----------------------------------------------------------------------- |
| `mode`         | What the profile asks for: `Off` (never look, never mention), `Notify` (look once a day and say so; the default) or `Automatic` (also install, while no client is subscribed). |
| `state`        | `Idle`, `Checking`, `Available { version }`, `Installing`, `Failed { reason }`, or `Unavailable` for a copy that does not update itself (a console run or a test instance). |
| `checked_unix` | When a look last succeeded, since the service started.                  |

`CheckForUpdate` and `InstallUpdate` answer `Ok` at once: the work is done by
the updater, and its outcome arrives as a `StatusChanged` event and a new
`state`. `InstallUpdate` needs no earlier look and installs whoever is
connected; the installation stops the service and closes the app, so a client
that sent it should expect its connection to end. A copy in the state
`Unavailable` answers both with `Unsupported`.

## Sensor section

| Property | Value                                                    |
| -------- | -------------------------------------------------------- |
| Name     | `Global\<instance>.Sensors`                              |
| Size     | 16 KiB                                                   |
| Access   | Service: read/write. Authenticated users: read.          |
| Capacity | 1020 sensors                                             |

The section is an array of 64-bit words, always accessed atomically:

| Word | Content                                                          |
| ---- | ---------------------------------------------------------------- |
| 0    | Magic `SYSP` and layout version                                  |
| 1    | Section size and capacity                                        |
| 2    | Sequence number (odd while a write is in progress)               |
| 3    | Sample time, milliseconds since the Unix epoch (0: no sample)    |
| 4    | Sample count and sensor-set generation                           |
| 5–7  | Reserved                                                         |
| 8…   | Per sensor: `id` and a valid flag, then the value as `f64` bits  |

Readers use the sequence number as a seqlock: read it, copy the data, read it
again, and retry if it was odd or changed. A reader never blocks the writer and
never sees half a sample.

`Sensors` returns the metadata (name, unit, device) for each ID together with a
`generation`. When the generation in the section differs from the one the
client holds, the IDs mean something else and the client must fetch `Sensors`
again.

## Sensor worker

The service and `syspilot-sensors.exe` talk over the worker's standard input
and output (`proto::worker`). This is internal: both always come from the same
build, so it carries no version.

| Direction         | Content                                                        |
| ----------------- | -------------------------------------------------------------- |
| Worker to service | Once, first: a `Probed` frame with the driver state, the features, the system inventory, the sensor list and the `FanStatus` the board allows (channels, availability and reason, no curves yet). |
| Service to worker | The byte `s` for each sample wanted. Any other byte is ignored by an older worker. |
| Worker to service | One `Sample` frame per `s`: one optional value per sensor, in the order of the sensor list. |
| Service to worker | The byte `f` followed by a `FanPlan` frame (the state directory and the enabled curves). It replaces any earlier plan; an empty one follows nothing and hands every channel back to the BIOS, including any the state directory still records from a crashed worker. Sent only when the curves changed and to each new worker; the first one goes out even when the profile has no enabled curve. |
| Service to worker | The byte `z` when the machine is about to sleep: every channel goes back to the BIOS and nothing is written until `r`, the byte for the wake. |
| Worker to service | A `Fans` frame, a `FanRuntime` per channel (phase, target duty, problem), whenever something in it changed. It may arrive between a `Sample` and the next one; the service keeps the latest. |

Frames use the pipe's framing. The end of the worker's input tells it to exit,
and with fan control it first hands every channel back to the BIOS. A plan
that does not decode ends the worker the same way. The service waits up to 20 s
for `Probed` and 750 ms for each `Sample`. While a session holds a channel and
no request comes for two seconds, the worker reads the sensors and steps by
itself.

## Updater

The service and `syspilot-update.exe` exchange even less (`proto::update`),
and again both come from the same build. The service starts the updater with
one command and reads one line from its standard output.

| Command               | Line on success                     |
| --------------------- | ----------------------------------- |
| `check`               | `current`, or `available <version>` |
| `install <directory>` | `installing <version>`, or `current` when there was nothing newer |

A failure is a non-zero exit code and a sentence on standard error, which
becomes the `reason` of `Failed`. After `installing` the updater has started
Windows Installer and exits; the service is stopped by the installation. If it
is still running half an hour later, the state becomes `Failed` and names the
installer's log.

## Compatibility rules

- Enums are externally tagged and only ever grow at the end. postcard encodes a
  variant by its index, so reordering or removing one is a protocol break.
- Any change to a message's fields bumps `PROTOCOL_VERSION`.
- TOML documents (catalog, profile) use kebab-case keys and carry a `schema`
  number where they are stored.
