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
  panic: `install_panic_release` wraps the panic hook and `release_on_panic`
  restores from it, waiting at most 250 ms for the controller lock.
- A missing, `None`, non-finite or implausible reading (outside -20 to
  125 degrees C), or a gap between ticks longer than the policy allows,
  drives the channels using that sensor to 100 % without ramping. Control is
  taken again only after a configurable run of good readings, also after a
  resume.
- A restart after a crash with a stored original raises the channel to at
  least its minimum duty and restores the BIOS mode. A channel found in
  manual mode with no stored original is never recorded as the BIOS mode: it
  is raised to its minimum if below and left blocked.

## Verified by replay only

Everything. In detail, the replay bus checked:

- the register addresses, the write order (duty, then mode), the preserved
  low nibble, the restore order and the refusal of any other register;
- that every operation takes the ISA mutex (`FakeBusLocks`) and releases it
  on every path, including failures, and that a held mutex stops an
  operation before any write;
- the controller over a replayed chip: take control, sensor failure to
  100 %, stop, drop and a restart after a simulated crash.

The replay bus returns what the test fixture holds. It cannot tell whether
the real chip behaves as the Linux driver describes.

## Not done

- The datasheet cross-check above.
- Wiring into the service and the UI. Nothing in `syspilot-svc` or
  `syspilot-ui` calls this code.
- A durable `OriginalStore`. The service must persist the record to disk
  before the first write, or a crash with a lost journal leaves the channel
  blocked.
- The service must hold the ISA guard for every sequence (the channel does
  that itself), call `install_panic_release` once and call `suspend` and
  `resume` from its power notifications.
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
6. A killed service process leaves the fans at or above the minimum duty and
   the next start restores the BIOS mode.
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
