# Tuning

Some devices let their power limit, clocks, voltages and fan bounds be set
through their driver. SysPilot can keep such values in place: it sets them
when Windows starts, after sleep, and again when something else changed them.
The app's **Tuning** page (Advanced level) lists the devices and their values;
the same settings live in the profile.

What is supported today:

| Devices | Through | Values |
| --- | --- | --- |
| AMD Radeon RX 400 and RX 500 series | The Radeon driver's Overdrive N interface (`atiadlxx.dll`) | Power limit, the clock and voltage of each performance state of the graphics processor and of the memory, and, where the driver offers them, the temperature limit, the fan's target temperature and speed bounds, and the lowest clock used to stay quiet |

A device shows only the values its driver offers. Many cards offer no fan
values at all; the Radeon RX 550 this was developed on is one. Newer Radeon
cards (RX 5000 and later) use another interface that SysPilot does not speak
yet, and a processor whose clock is locked has nothing to offer. Why tuning is
done this way, and only this way, is decision 21 in
[decisions](decisions.md).

## What a setting means

A setting belongs to one device and lists values. While it is on, the device
is held at **the driver's defaults plus the listed values**: a value the
setting does not list is put back to its default, also when another tool
changed it. Turning the setting off puts every value back to its default and
then leaves the device alone. The listed values are kept for the next time.

Nothing is written to the device permanently. The values live in the driver,
which starts from its defaults again when it or the machine restarts. That is
why SysPilot sets them again. Removing SysPilot puts the defaults back first.

## In the app

Each device is a card with a switch, its values and three buttons.

- Enter values in the boxes. They are yours until you choose **Save**;
  **Discard** forgets them. The line under a value shows its range and the
  driver's default, and what the device has now when that is something else.
- **Save** stores the values. If tuning is on for the device, they take
  effect at once.
- The switch turns tuning on or off for the device, with the values saved
  for it. Values entered and not saved stay entries.
- **Use defaults** enters the default of every value, to be saved like any
  other entry.

Before a clock or a voltage of yours takes effect, the app asks. Limits take
effect without a question: they make a device slower or louder, not unstable.
The question can come on any change, not only one to the device it names:
while values are held back, saving or removing values on one card, or
**Apply again**, sets the held values of every card too.

Changing anything needs the app to run as administrator.

### States

| Badge | Means |
| --- | --- |
| Applied | Tuning is on and the device has your values. |
| Not applied yet | Tuning is on and the device has other values right now. SysPilot is setting yours. |
| Contested | Something else keeps changing the values. SysPilot set them three times and stopped. **Apply again** starts over. |
| Held back | Windows did not shut down normally after the values were last set, so SysPilot has not set them again. Change them, or choose **Apply again** if they were not the cause. A card that has your values anyway says **Applied**. |
| Failed | The values could not be set; the card says why. |
| Not found | The profile has values for a device that is not in this PC now. They are kept until you choose **Remove**. |

A device without a badge is left to its driver.

## In the profile

```toml
schema = 1

[[tuning]]
device = "gpu.amd-699f.bus-3"

[tuning.values]
power-limit = -10
core-p7-clock = 1150
core-p7-voltage = 1000
```

| Field | Meaning | Default |
| --- | --- | --- |
| `device` | The device's key, as `syspilot-svc status` and the Tuning page show it: vendor, PCI device ID and bus number. A card moved to another slot gets another key. | none |
| `enabled` | `false` keeps the values in the file and leaves the device to its driver. | `true` |
| `values` | The values to hold, by control key. Anything not listed is held at the driver's default. | none |

Control keys:

| Key | Unit | What it is |
| --- | --- | --- |
| `power-limit` | % | How much more or less power than its default the device may draw. |
| `temperature-limit` | °C | Above this the device slows itself down. |
| `fan-target-temperature` | °C | The temperature its fan works to hold. |
| `fan-max-speed`, `fan-min-speed` | RPM | The bounds of the driver's own fan curve. |
| `quiet-clock-floor` | MHz | How far the clock may drop to keep the fan at its target. |
| `core-p<N>-clock`, `core-p<N>-voltage` | MHz, mV | Performance state `N` of the graphics processor. Higher states are used under more load. |
| `memory-p<N>-clock`, `memory-p<N>-voltage` | MHz, mV | Performance state `N` of the graphics memory. The voltage is what the driver calls it; on Radeon RX 400 and RX 500 cards it is reported to be the lowest processor voltage used while the memory runs in that state, not a voltage of the memory itself. |

A value outside the range the driver states is refused when the profile is
applied, and so is a key the device does not have:

```console
> syspilot-svc apply profile.toml
Radeon RX 550 Series: (tuning.values.power-limit): expected -75..=75 | received: 90
```

Within the states of one kind, a clock may not be lower than in the state
before it. Such a setting is stored and then shown as **Failed**
with the reason, and nothing on the device is changed.

`syspilot-svc status` lists each device, where its setting stands and every
value that is not at its default:

```console
Tuning
  Radeon RX 550 Series - setting applied
    power-limit         -10 % (default 0 %)
```

## When values are set

The service does not watch the device. It looks at it at these moments, and
sets the values if they differ:

- when the service starts, when the settings change, when the machine wakes,
  when someone signs in, and on **Apply again**;
- 45 seconds and three minutes after each of those, because the driver's own
  app may set its values shortly after sign-in;
- one minute after every time it set values.

After the third time in one such round, the device is **Contested** and left
alone until the next of the moments above. Between rounds nothing runs. A
value that another tool sets later stays until then. The Tuning page shows
it: while the page is open, SysPilot looks at the devices again each time the
page reloads, at most every ten seconds.

With no setting on and no app open, SysPilot does not look at the devices at
all.

## With the driver's own app

AMD Software keeps its own tuning profile and sets it when you sign in and
when the driver restarts. If both it and SysPilot hold values for the same
card, they overwrite each other, and SysPilot gives up first (**Contested**).
Keep the tuning in one of the two: reset the card's tuning in AMD Software
(Performance, Tuning, Reset) when SysPilot holds the values.

## If Windows stops

A clock that is too high or a voltage that is too low can freeze the picture
or stop Windows. For the cases where the machine has to be restarted:

- Before it sets values, the service leaves a mark on disk. The mark is
  removed once the machine has run with the values for a minute, or the
  service stops normally.
- A service that finds the mark when it starts sets nothing. The device runs
  on the driver's defaults and its card says **Held back**.
- Saving other values, or **Apply again**, lifts the hold.

Values that fail only under load, minutes later, are not caught by this:
lower them, or turn tuning off, after the restart.

## How it works

The service loads no vendor code. For each look and each change it starts
`syspilot-tune.exe`, which sits next to it, hands it one job and reads one
report; a tuner that does not answer within 15 seconds is terminated. The
tuner checks every value of a setting against the driver's ranges before it
writes the first one, and writes only what differs from what the device has.
The messages are in [protocol](protocol.md#tuner) and what protects the
feature in the [security model](security-model.md#tuning).

To see what the tuner finds on a machine without changing anything:

```console
> cargo run -p syspilot-hw --example tuning
```
