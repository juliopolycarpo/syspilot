# Fan control: registers, checks and what a human must still validate

Research note for the `fan-control` cargo feature of `syspilot-hw`. Decision
12 keeps version 1 from writing to hardware, so the feature is off by
default and must stay off until the checklist at the end is done on a real
board. Nothing here was run against real hardware: every write path was
exercised against the replay bus and the PawnIO fake only.

## Source

- Linux `drivers/hwmon/nct6775-core.c` and `nct6775.h`, the tree's last
  commit touching the file being `39cd14ca605d1aa55a0ad26767014a88b5c39f20`
  (2026-11-16), read on 2026-11-23. The `nct6776` and `nct6779` cases of
  `nct6775_probe` select the tables below.
- The Nuvoton NCT6779D datasheet was **not** consulted. Every register
  claim below rests on the Linux driver alone and is **unverified** against
  the datasheet.

## Registers

For duty output `n` (1 to 5 on the NCT6779D, 1 to 3 on the NCT6776F), the
bank-and-index addresses of the hardware monitor are:

| Role | Register | Linux table | Meaning |
|---|---|---|---|
| Fan mode | `0x102, 0x202, 0x302, 0x802, 0x902` | `NCT6775_REG_FAN_MODE` | bits 6:4 select the mode, 0 being manual (`reg_to_pwm_enable` reads `(reg >> 4) & 7`); bits 3:0 hold tolerance settings |
| Manual duty | `0x109, 0x209, 0x309, 0x809, 0x909` | `NCT6775_REG_PWM` | `0..=255` for `0..=100 %`; the output while the mode is manual |
| Duty read-back | `0x001, 0x003, 0x011, 0x013, 0x015` | `NCT6775_REG_PWM_READ` | the duty the chip currently drives; Linux reads it in the automatic modes |

Linux switches to manual in `store_pwm_enable`: it reads the fan-mode
register, keeps `reg & 0x0f`, ORs in `pwm_enable_to_reg(manual) << 4`, which
is 0, and writes it back. `NCT6775_REG_FAN_MODE` and `NCT6775_REG_PWM` are the
same tables for the NCT6776F and the NCT6779D.

The code in `crates/hw/src/fan/nct677x.rs` does the same and no more:

- **Taking control**: writes the manual duty first, then the fan-mode
  register with bits 7:4 cleared and bits 3:0 kept. The duty goes first
  because in the BIOS's automatic modes the manual register is not the
  output, and the mode switch would otherwise make the fan jump to whatever
  it held.
- **Setting a duty**: refuses unless the channel is still in manual mode,
  then writes the manual duty register. It tells the controller when the
  BIOS or another tool took the channel back.
- **Restoring**: writes the saved manual duty, then the saved fan-mode
  value, so the output type, temperature source and SmartFan tables, which
  are never touched, resume the BIOS's curve.
- **Never written**: the DC/PWM output type, the temperature-source select
  registers, the SmartFan target, step and stop/start registers, the
  tachometer registers. A write allow-list in `bus/mod.rs`
  (`is_permitted_fan_register`) holds the ten registers above and is checked
  by both the PawnIO and the replay bus.

The bus offers no raw data-port write: the only fan write is
`SuperIo::hwm_write_fan_register`, which exists only with the feature, takes
a 16-bit address and a value, rejects an address outside the allow-list and
leaves bank 0 selected, also when a step fails.

## What the build guarantees

- Without the feature there is no trait method, function or type that can
  write a fan register. The `SuperIo` trait carries a `compile_fail`
  doctest that fails to compile a call to the write method, active only
  without the feature. `spd-paging` follows the same pattern.
- The board catalog (`catalog/boards/`) is always compiled but only
  describes. A board is offered for control only if its entry says
  `validated = true` with a validator and a date, names a chip that has a
  write path, and matches the chip the driver detected. The shipped entry,
  for the reference board, says `validated = false` and so stays read-only.
  An unknown board is read-only.
- `tests/driver.rs` refuses to open the driver whenever the feature is on,
  so `SYSPILOT_TEST_PAWNIO` cannot run a write against hardware by mistake.

## Fail-safe behaviour (pure logic, tested against fakes)

`fan/failsafe.rs` holds the controller. Its rules are tested against
`FakeFanChannel` and `FakeOriginalStore` with injected failures:

- The original registers are saved to an `OriginalStore` before the first
  write and kept until a restore is confirmed. A failed restore keeps the
  record and is retried on every tick.
- Stop, pre-shutdown, suspend, drop and a panic path restore the originals.
  The release profile has `panic = "abort"`, so `Drop` does not run on a
  panic: `install_panic_release` wraps the panic hook and `PanicRelease`
  restores from it.
- **The panic path does not go through the controller.** A panic during
  `tick`, `suspend` or `stop` runs the hook on the same thread in the middle
  of the step, and the step holds the controller across its register I/O. A
  hook that waited for the controller (the first design waited 250 ms and
  gave up) leaves the channel in manual mode when the process then aborts.
  Instead, each channel keeps its saved registers in a small shared record
  (`Original`) that the controller sets before its first write to the channel
  and clears after a confirmed restore; the controller hands the hook a
  `PanicRelease` made of those records, a second handle on each channel's
  output (`FanChannel::duplicate`, the same bus and the same allow-listed
  writes) and the store behind a per-call lock (`SharedStore`). The hook
  restores each channel that has a record and clears its stored record. None
  of these three locks is held across register I/O (the bus lock is, for one
  operation), and every wait in the hook, the bus lock included, is bounded
  to 250 ms. The session owns its
  controller outright, with no lock around it. One case remains: a panic
  inside a bus transaction itself (not the driver returning an error, which is
  an ordinary failure) leaves the bus lock held by the thread now running the
  hook, so the hook gives up after 250 ms; the record stays in
  `fan-originals.toml` and the next start restores it. A record the hook
  could not clear is harmless: the next start writes the same registers again.
- **One hook per process, over a swappable slot.** `install_panic_release`
  installs the hook the first time (a `Once`) and puts the session's
  `PanicRelease` in a static slot; the hook reads the slot when a panic
  happens. Each new plan puts its release in the slot, which drops the
  previous one (and with it that session's duplicate bus handles and its
  PawnIO handle); a session's `Drop` empties the slot if it still holds that
  session's release, so a stopped worker does not keep a handle open. The
  earlier design installed one hook per session, each holding its
  controller, so every replaced session stayed open and a panic walked all
  of them.
- A missing, `None`, non-finite or implausible reading (outside -20 to
  125 degrees C), or a gap between ticks longer than the policy allows,
  drives the channels using that sensor to 100 % without ramping. Control is
  taken again only after a configurable run of good readings, also after a
  resume.
- A restart after a crash with a stored original raises the channel to at
  least its minimum duty and restores the BIOS mode. A channel found in
  manual mode with no stored original is never recorded as the BIOS mode: it
  is raised to its minimum if below and left blocked.
- Recovery visits every channel in `fan-originals.toml`, not only the ones
  the new plan has a curve for. The session lists the store
  (`OriginalStore::channels`) and gives each recorded channel that has no
  curve a restore-only plan (`ChannelPlan::restore_only`): the controller
  restores it, clears its record and retries a failed restore on every step,
  and never takes it. A record for a channel the board entry does not list is
  left untouched and reported as blocked. A restore that fails leaves the
  channel in the "release pending" phase, never "BIOS".

## How it is wired (decision 18)

The curves of a profile run in the sensor worker, next to the registers, and
not in the service. Both ends need the `fan-control` feature:
`syspilot-sensors/fan-control` for the worker that writes and
`syspilot-svc/fan-control` for the service that accepts curves. A release has
neither.

- **Whether control is allowed** is decided once, in the worker's probe, by
  `fan::layout::assess`: PawnIO usable, the baseboard's name read from
  SMBIOS, the Super I/O chip the probe found, then the catalog compiled into
  the binary (`BoardCatalog::shipped`). The first thing that fails is the
  reason shown, in words: PawnIO is not installed, the board's name could not
  be read, the sensor chip has no fan control, the board is not in the
  catalog, its entry has not been validated. Only a validated entry for the
  detected chip yields a `ControlGrant`, and the worker opens a fan bus only
  from one. The reference board's entry stays `validated = false`.
- **The plan** is the profile's enabled curves, sent by the service to the
  live worker (`FANS_REQUEST` and a `FanPlan` frame) when they change and to
  every worker that replaces it. A service that accepts curves sends a first
  plan even when it has no enabled curve: the plan carries the directory of
  the store, which is how a worker learns where to look for what a crash
  left. With no enabled curve the service keeps no worker alive for the fans
  (a worker that runs for the sensors gets the empty plan and recovers), and
  the worker holds a session only while something needs one. A board that
  cannot follow curves keeps no worker either.
- **The session** (`fan/session.rs`) opens the fan bus, puts each curve on its
  channel, runs crash recovery for every channel of the store before the
  first step and then steps from the sensor samples: once per request, and by
  itself every two seconds if the service stops asking. With no curve to run
  and nothing in the store it opens nothing; with records and no curve it
  recovers and is dropped, unless a restore failed and has to be retried. A curve for a channel the entry does not list is that
  channel's problem and the BIOS keeps it; the others run. The entry's
  minimum duty is a floor that a profile can raise and never lower.
- **The original registers** are kept in `fan-originals.toml` in the
  service's state directory (`FileOriginalStore`): written to a temporary
  file, flushed and renamed over the old one, before the first write to a
  channel, and removed once the restore is confirmed. A file that does not
  parse is never overwritten, which leaves the channel alone.
- **What happens at each event**:

  | Event | What the worker does |
  |---|---|
  | A source sensor is missing, fails or reads implausibly | Drives that channel to 100 % at once; takes control again after a run of good readings |
  | The service closes the pipe (stop, crash, uninstall) | Restores every channel before it exits |
  | The service stops asking for samples | Steps by itself every two seconds |
  | The plan changes | Restores the channels, then opens a session for the new curves |
  | The plan has no enabled curve | Restores the channels it holds and every channel the store still records, then holds no session (unless a restore failed and is being retried) |
  | The machine sleeps | Restores every channel at once and writes nothing until it wakes |
  | The machine wakes | Takes control again after a run of good readings |
  | A panic in the worker | The panic hook restores from the saved registers, without the controller (the release profile aborts) |
  | The worker is killed | Nothing runs. The chip keeps the last duty written, which is never below the minimum, until the next worker that gets a plan restores the BIOS mode from the saved registers, whatever the plan holds. No worker is started only for that: with no curve and no one looking at the sensors, the channel waits for the next one |
  | A restore fails | The record is kept and the restore is retried on every step |

- **What the status says**: `ServiceStatus.fans` joins the board's channels,
  the profile's curves and the controller's report: for each channel the
  curve, whether it is followed, the duty asked for, the phase and the last
  problem. The reason for a read-only board is in `FanAvailability`. A worker
  that ends takes its report with it, so the status reads "the BIOS holds the
  channel".
- **Tests** use the replay bus, named fakes for the store, the access and the
  worker, and `FakeHardware` in the service. With the feature on, the tests
  that would open the driver or start the real worker (`crates/hw/tests/driver.rs`,
  `crates/sensors/tests/worker.rs`) do not run, and nothing starts a service.

## Verified by replay only

Everything. In detail, the replay bus checked:

- the register addresses, the write order (duty, then mode), the preserved
  low nibble, the restore order and the refusal of any other register;
- that every operation takes the ISA mutex (`FakeBusLocks`) and releases it
  on every path, including failures, and that a held mutex stops an
  operation before any write;
- the controller over a replayed chip: take control, sensor failure to
  100 %, stop, drop and a restart after a simulated crash;
- the session and the worker loop over a replayed NCT6779D: curves following
  samples, a sensor missing from a sample, suspend and resume, a new plan, a
  worker that steps by itself, and the restore when the input closes. The
  service side was checked with fake workers that speak the protocol.

The replay bus returns what the test fixture holds. It cannot tell whether
the real chip behaves as the Linux driver describes.

## Not done

- The datasheet cross-check above.
- Running it on a real board. The wiring above has only been exercised on
  the replay bus and with fakes; the checklist below is still open.
- A curve editor. Curves are written in the profile file and the app shows
  them ([the profile format](../fan-curves.md)).
- A hung hardware read in the worker is not covered by the two-second step:
  the step needs the read to return. The service gives up on a worker that
  misses a sample and replaces it, and the replaced worker is killed if it
  does not leave in two seconds; its registers are then restored by the next
  session, not at once.
- Chips of other families (ITE IT87xx, other Nuvoton parts). They have no
  write path, and the board catalog refuses a validated entry for them.

## What a human must validate before the feature may be enabled

On the real board, with a case fan in reach and a way to stop the test
(power button), enabling the feature on a development build only:

1. The catalog entry's `pwm` to header mapping: change one channel and
   watch which header's fan reacts. Fill in the labels from what is seen.
2. The fan reacts to a duty change and the tachometer follows. A channel
   whose tachometer does not move is wired to a DC header or to a different
   output type, which this code does not handle.
3. The minimum duty keeps each fan turning, including from a stop, at the
   value in the entry. Record `may-stop` only for a fan seen to restart.
4. The duty read-back register (`REG_PWM_READ`) agrees with the manual duty
   while in manual mode. The state read relies on it and Linux reads it in
   automatic modes only.
5. Restoring returns BIOS control: after a stop the fan follows the BIOS
   curve again and the two registers read as before the first write.
6. A killed service process or worker leaves the fans at or above the minimum
   duty and the next start restores the BIOS mode (the saved registers are in
   `fan-originals.toml`).
7. Suspend and resume: the controller releases before sleep and takes
   control again only after sensors read correctly.
8. Sensor failure: unplug or block a sensor reading and confirm the fans go
   to 100 %.
9. Another tool (a BIOS-side fan utility, or the board vendor's) does not
   fight the chip. The ISA mutex protects the bus between programs, not the
   fan registers.

Only after every step passes is `validated = true` set in the entry, with
the validator and date, and only then may a release build enable the feature,
by a decision recorded in `docs/decisions.md`.
