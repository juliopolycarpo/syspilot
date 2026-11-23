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
service are released together.

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

## Compatibility rules

- Enums are externally tagged and only ever grow at the end. postcard encodes a
  variant by its index, so reordering or removing one is a protocol break.
- Any change to a message's fields bumps `PROTOCOL_VERSION`.
- TOML documents (catalog, profile) use kebab-case keys and carry a `schema`
  number where they are stored.
