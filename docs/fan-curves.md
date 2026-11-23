# Fan curves in a profile

A profile can hold one temperature curve per fan channel. The service follows
the curves only in a build with the `fan-control` cargo feature, only on a
board whose entry in `catalog/boards/` says `validated = true`, and only with
PawnIO installed. Releases are built without the feature, and no board is
validated yet, so today the curves are accepted by development builds and
nothing is written anywhere else. The app's **Fans** page (Advanced level)
shows each channel, its curve and, when control is off, the reason in words.
The reasons and the safety rules are in
[the research note](research/fan-control.md).

## The shape

```toml
schema = 1

[[fan]]
channel = "board.pwm2"
sources = ["cpu.package.temp", "gpu0.temp"]
points = [[40, 30], [60, 50], [80, 100]]
hysteresis-c = 3
min-duty-pct = 20
ramp-pct-per-sec = 10
```

| Field | Meaning | Default | Rule |
| --- | --- | --- | --- |
| `channel` | The fan output, named by the key of its duty sensor: `board.pwm<N>` is output `N` of the board's fan chip. The Fans page lists the channels of the board. | none | One curve per channel. |
| `sources` | Keys of the temperature sensors that drive the curve. The hottest wins. A sensor's key is the one its sensor log uses as a column name, such as `cpu0.package.temp`. | none | At least one. |
| `points` | `[temperature in °C, duty in %]` pairs. Between two points the duty is interpolated; below the first it holds the first duty, above the last the last. | none | 2 to 16 points, temperatures strictly increasing, duty 0 to 100, **the last point at 100 %**. |
| `enabled` | `false` keeps the curve in the file and leaves the channel to the BIOS. | `true` | |
| `hysteresis-c` | Degrees the temperature must fall before the duty is lowered. | `3` | 0 to 20. |
| `min-duty-pct` | The duty the fan never goes below. | `20` | 0 to 100. The board entry has its own minimum for the channel; the higher of the two applies. |
| `ramp-pct-per-sec` | The most the duty may change per second, up or down. | `10` | 1 to 255. |

The last point must be at 100 % so that a curve can never cap cooling below
what the fan can deliver. When a source sensor is missing, fails or reads an
implausible value, the service does not interpolate: the channel goes to 100 %
until the readings are good again.

## Applying it

```console
> syspilot-svc apply profile.toml
```

A build without the feature refuses a profile with a `[[fan]]` table:
`Fan control is not enabled in this build.` With the feature, a profile for a
board that is not validated is accepted, shown on the Fans page and not
followed; the page and `syspilot-svc status` say why.

Take the curves out of the profile (or set `enabled = false`) and the service
hands the fans back to the BIOS and stops sampling for them. Nothing runs
while no curve is enabled.

## Building with the feature

Two cargo features, one in each executable that takes part. Do this on a
development machine only:

```console
> cargo build -p syspilot-svc -p syspilot-sensors --features syspilot-svc/fan-control,syspilot-sensors/fan-control
```

Do not run that build on a board whose fans you cannot reach: the checklist in
[the research note](research/fan-control.md) comes first.
