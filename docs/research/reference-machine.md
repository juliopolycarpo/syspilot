# The reference machine

Research note. What the PawnIO-backed providers detected on the development
machine, and what they did not. Every value was read through PawnIO 2.2.0
with the 0.2.12 modules on 2026-11-23 (`SYSPILOT_TEST_PAWNIO=1 cargo test -p
syspilot-hw --test driver` and the `driver_sensors` example), except where a
line says it comes from the recorded fixtures in `crates/hw/tests/fixtures`.
Nothing here identifies one physical unit: no serial number, UUID or host
name.

This is the only machine the code has run on. Everything the tests check
against other hardware is replay of values recorded here or written by hand
from datasheets and the Linux drivers; the last section lists it.

## Platform

| Item | Value |
|---|---|
| Board | MACHINIST E5-MR9A PRO (X99) |
| Processor | Intel Xeon E5-2680 v4, family 6 model `0x4F` stepping 1 (Broadwell-EP), 14 cores, 28 logical processors |
| Super I/O | Nuvoton NCT6779D |
| SMBus controller | `i801`, PCI `8086:8C22`, I/O base `0xF000` |

## What `probe()` returns

| Key | Available | Detail |
|---|---|---|
| `cpu-sensors` | yes | 14 cores, package and per-core temperature (TjMax 100 °C); power: package; not available: dram (counter reads 0) |
| `board-sensors` | yes | NCT6779D (id/revision `0xc562`) at ports `0x2e`/`0x2f`, monitor at `0x0a20` (5 temperatures, 11 voltages, 5 fans, 5 duty) |
| `memory-sensors` | no | i801 SMBus controller: nothing acknowledged at SMBus addresses `0x50`-`0x57`, so no memory module is reachable through this controller |
| `fan-control` | no | Not part of this version |

The driver state is `Ready { version: "2.2.0" }`. The two providers declare 43
sensors (17 processor, 26 board). The release build of the example probes in
about 50 to 60 ms when idle (350 ms with every core busy) and holds about
0.9 MiB of private memory with the hub, 0.8 MiB after dropping it.

## Processor

- TjMax is 100 °C (`IA32_TEMPERATURE_TARGET` = `0x640a00`).
- Idle readings: package 43 to 45 °C, cores 37 to 44 °C.
- `MSR_RAPL_POWER_UNIT` = `0xa0e03`: energy unit 2^-14 J, the usual value for
  this part. The package counter runs, so `cpu.package.power` is declared.
- The DRAM energy counter (`0x619`) reads 0 and the processor declares no
  PP0 domain, so neither is shown. A constant 0 W would mislead.
- **Package power is not trustworthy.** It reads 0.8 W idle and 12 W with 28
  busy processes (package 57 °C), for a part with a 120 W TDP. The previous
  measurement gave about 9.7 W under all-core load against a 120 W limit. The
  unit register is the standard one for this model, so a wrong unit is
  unlikely; the cause is unknown. The sensor is declared as read, with no
  accuracy claim. Not checked: the package power limit register.

## Super I/O: NCT6779D

- Detected on the primary pair (`0x2E`/`0x2F`), chip ID `0xC5`, revision
  `0x62`, hardware monitor at `0x0A20`, vendor ID `0x5CA3`. Nothing answers on
  the secondary pair (`0x4E`/`0x4F`). The I/O space lock bit
  (configuration register `0x28`) is clear.
- 26 sensors are declared: five temperatures (PECI agent 0, SYSTIN, AUXTIN1
  to AUXTIN3), eleven voltages, five fan speeds and five fan duties.
- Not declared because they read nothing usable: CPUTIN and AUXTIN0 (negative
  readings, which are rejected), voltage inputs 1, 4 and 5 (read 0) and 12
  (reads the clipped `0xFF`).
- Fan 2 turns at about 600 RPM (duty 7 to 8 percent). Fans 1, 3, 4 and 5 read
  0 RPM, which the chip reports for a stopped fan or an empty header. Their
  duty registers read 100, 50, 100 and 100 percent.
- AVCC (3.30 V), 3VCC (3.30 V), 3VSB (3.34 V) and VBAT (3.28 V) match their
  nominal values. The other voltage inputs are shown under their input
  number; what they measure depends on the board and was not checked against
  a schematic.
- Values were steady over ten seconds apart from the processor-fed PECI
  temperature (34 to 36 °C) and the fan.

### The floating temperature inputs

AUXTIN1 and AUXTIN2 read a constant 103 and 109 °C with nothing attached,
and AUXTIN3 reads a constant 22 °C. All three are fixed over ten seconds and
under all-core load.

They come from the chip's alternate registers (`0x493` = `0x67`, `0x494` =
`0x6D`, `0x495` = `0x16`), because no temperature slot routes sources 4, 5
or 6 to them. Neither the registers nor the plausible range (-55 to 125 °C)
say whether a thermistor is connected.

What the references do:

- The Linux `nct6775` driver exposes an input only when a temperature source
  select register routes it. The alternate registers are read only in a
  block under `#ifdef USE_ALTERNATE`, which `nct6775-core.c` does not define
  (read on 2026-11-23; `nct6775-platform.c` was not checked). It has no rule
  that tests a value for "not connected".
- LibreHardwareMonitor reads the alternate registers for unrouted sources,
  as SysPilot does, and rejects only values outside -55 to 125 °C (and, for
  the alternates, zero or below). It has no connection test either.

So there is no sound value-based rule, and the code is left alone. The only
rule the references support is Linux's: hide what no slot routes. On this
board it would hide all three inputs, including AUXTIN3, which may be a real
thermistor at room temperature; the other chips' alternate registers cannot be
checked here. Until a second board shows which choice loses real sensors, the
floating inputs are shown as read.

## SATA drive temperature

Read on 2026-11-23 with the `sensors` example, `Get-StorageReliabilityCounter`
and the `root\wmi` class `MSStorageDriver_ATAPISmartData`, all read-only.

| Drive | Windows `Temperature` | SMART attribute 194, raw byte 0 | ACS-3 Device Statistics, current temperature | `StorageDeviceTemperatureProperty` |
|---|---|---|---|---|
| XrayDisk 1TB SSD (SATA) | 41 | 41 | 46 (maximum 54) | `ERROR_INVALID_FUNCTION` |
| CT500BX500SSD1 (SATA) | 38 | 38 (raw bytes 2 and 4: minimum 22, maximum 51) | read fails (`ERROR_IO_DEVICE`) | `ERROR_INVALID_FUNCTION` |
| Asgard AN4+ 1TNVMe (NVMe) | 40 | not applicable | not applicable | composite 40 |

SysPilot used to read the Device Statistics value, so the XrayDisk showed 46 °C
against Windows's 41 °C (issue 6) and the BX500 had no temperature sensor.
The two are different sensors on the XrayDisk. Device Statistics is
standardised by T13 (ACS-3) but optional, and a drive may fill "current
temperature" from any sensor; attribute 194 is vendor-defined but is the
drive's own "Temperature_Celsius" and what Windows and SMART tools report. The
provider now reads attribute 194 (raw byte 0, through
`IOCTL_STORAGE_PREDICT_FAILURE`, which needs no access right) and keeps
Device Statistics as a fallback for a drive that has the log but no attribute
194. Attribute 190 is not read: no source was consulted for how drives encode
it.

## Memory modules

The machine has memory, but no memory module answers at `0x50`-`0x57` through
the i801 controller. `SmbusI801` loads and `ioctl_identity` answers, so the
controller is found; the modules are not behind it. On server boards the DIMM
bus can hang off the processor's memory controller instead of the PCH. This
was not investigated further, so the DIMM provider and the SPD decoding have
never read a real module.

## Not verified on real hardware

- The AMD provider (SMN thermal registers, CCD layout, Tctl offsets): replay
  only, against registers written from the `k10temp` driver.
- DIMM decoding (DDR4 SPD, the JC-42.4 sensor, the SPD5118 hub): replay only.
- Every Super I/O chip other than the NCT6779D, including all ITE IT87xx
  chips and the NCT6791D to NCT6799D layouts.
- The `spd-paging` feature, which is the only SMBus write in the code. It was
  never built into a run on this machine.
- Package power accuracy and the DRAM energy domain (above).
