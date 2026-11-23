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
| `cpu-sensors` | yes | 14 cores, package and per-core temperature (TjMax 100 °C); not available: package (reads 0.79 W, below the 6.0 W floor for a 120 W part), dram (counter reads 0) |
| `board-sensors` | yes | NCT6779D (id/revision `0xc562`) at ports `0x2e`/`0x2f`, monitor at `0x0a20` (5 temperatures, 11 voltages, 5 fans, 5 duty) |
| `memory-sensors` | no | i801 SMBus controller: nothing acknowledged at SMBus addresses `0x50`-`0x57`, so no memory module is reachable through this controller |
| `chipset-sensors` | yes | Intel PCH `8086:8C24` at PCI `00:1f.6`, PCI thermal BAR0 at `0x383ffff14000` (256 bytes), reads 49.0 °C |
| `fan-control` | no | Not part of this version |

The driver state is `Ready { version: "2.2.0" }`. The three providers declare 43
sensors (16 processor, 26 board, 1 chipset). The release build of the example probes in
about 300 to 385 ms when idle, 250 ms of it the package power watch window
(see Processor); before that window it took about 50 to 60 ms idle and 350 ms
with every core busy. The first version of this note measured 0.9 MiB of
private memory with the hub, 0.8 MiB after dropping it; that was not
re-measured.

## Processor

- TjMax is 100 °C (`IA32_TEMPERATURE_TARGET` = `0x640a00`).
- Idle readings: package 43 to 45 °C, cores 37 to 44 °C.
- `MSR_RAPL_POWER_UNIT` = `0xa0e03`: energy unit 2^-14 J, the usual value for
  this part. The package counter runs, so `cpu.package.power` is declared.
- The DRAM energy counter (`0x619`) reads 0 and the processor declares no
  PP0 domain, so neither is shown. A constant 0 W would mislead.
- **The package power meter is miscalibrated, and the provider no longer
  offers it** (issue 3). Details below.

### Package power (issue 3)

Measured on 2026-11-23 with raw reads of the RAPL registers through the
`IntelMSR` module, no writes. The raw reads, the 1 ms cadence and the
busy-thread runs came from a throwaway read-only program (since deleted)
built on the same `PawnMsr` the provider uses, run with a busy loop of up to
28 threads for about 12 s per run; the probe-level figures (the 0.77 and
0.79 W) are from the `driver_sensors` example and the driver tests.

| Register | Value | Meaning |
|---|---|---|
| `0x606` `MSR_RAPL_POWER_UNIT` | `0xa0e03` | power unit 2^-3 W, energy unit 2^-14 J (61 µJ), time unit 2^-10 s |
| `0x614` `MSR_PKG_POWER_INFO` | `0x002f078001d003c0` | TDP 960 / 8 = 120 W, maximum power limit 240 W |
| `0x610` `MSR_PKG_POWER_LIMIT` | `0x00078480001583c0` | limit 1 = 120 W, limit 2 = 144 W, both enabled |
| `0x611` `MSR_PKG_ENERGY_STATUS` | counts up | see below |
| `0x619` `MSR_DRAM_ENERGY_STATUS`, `0x618` `MSR_DRAM_POWER_LIMIT` | `0`, `0` | DRAM RAPL is not running |
| `0x61C` `MSR_DRAM_POWER_INFO` | `0x002f016800200156` | the PCU knows a 42.75 W DRAM TDP; only the counter is off |
| `0x639` `MSR_PP0_ENERGY_STATUS` | `0` | no PP0 domain on this model |
| `0x641`, `0x64D` | `#GP` (`STATUS_PRIVILEGED_INSTRUCTION`) | no PP1 or platform domain |

What the counter does:

- It has 32 bits and its upper half reads zero, which the SDM says is
  reserved. That alone would not show that the module returns the whole
  64-bit register (one that truncated would read zero too); other registers
  do: `0x610`, `0x614`, `0xCE` and `0x198` came back with nonzero bits above
  bit 31 (table above).
- All 28 logical processors read the same package counter (one socket).
- It advances once per millisecond, the known Broadwell-EP cadence, with 14
  counts per step idle and 42 busy.
- Over one-second intervals measured with the monotonic clock it advanced
  12 700 to 13 100 counts idle and 42 300 to 43 600 with every logical
  processor busy. At the declared unit that is 0.77 to 0.80 W idle and 2.6 W
  busy for a 120 W part. With 14 or 28 busy threads the reading is the same
  (2.6 W), with one thread it equals idle, with seven it is 0.8 to 1.6 W.
- During the busy runs `IA32_APERF` / `IA32_MPERF` on core 0 gave 1.21 (about
  2.9 GHz against the 2.4 GHz base), so the cores did run; the package
  temperature rose from 41-44 °C to 46-47 °C. Earlier runs with another load
  read 9.7 W and 12 W (the figures in the issue).

So the counter is alive, updates at the right rate, follows load, and its
reading is a small and roughly constant fraction of the real power. The unit
is the one in the register, which agrees with the SDM (volume 4,
`MSR_RAPL_POWER_UNIT`, energy status units at bits 12:8) and with Linux's
`arch/x86/events/rapl.c`, which reads the unit the same way and masks the
counter to 32 bits. The fixed 2^-16 J unit of the DRAM domain on Haswell-EP
and Broadwell-EP (SDM volume 4, and Linux's powercap driver) is already in the
code; it applies to the DRAM counter only, and using it for the package would
lower the reading further. A wrap or time-base error is excluded by the
numbers above (no wrap at these rates, and the counter's own 1 ms cadence
matches the wall clock).

Windows runs on Hyper-V here (`HypervisorPresent` is true,
virtualization-based security and memory integrity are running). A
hypervisor that virtualised the RAPL MSRs would return zero or a static or
synthetic value, not a counter that ticks every millisecond in proportion to
load while the neighbouring registers (power limits, TDP, turbo ratios, APERF
and MPERF) read as the hardware's. That argues against the hypervisor, but it
was not tested with virtualization-based security off, which would need a
reboot.

What points to the board or the part instead: another report of a MACHINIST
X99 board with a Xeon E5-2690 v4 (14 cores, 135 W) shows the same symptom,
about 1 W under load, on a machine described as running Linux
(<https://n3xus.dev/en/blog/custom-built-server-x99-xeon-parts-prices-time/>,
read 2026-11-23; it does not say which RAPL interface it used). The DRAM
domain being off (`0x618` is zero) fits firmware that leaves RAPL half
configured. HWiNFO's author says package power on Intel is measured by the
processor from the VR's IMON/SVID telemetry
(<https://www.hwinfo.com/forum/threads/package-power-reading.5736/>), and the
E5 v4 datasheet (volume 1) describes the processor's SVID link to the voltage
regulator. A board whose regulator reports little current to the processor
would give a low, load-following value. That is a hypothesis; the exact
mechanism is not verified. A closer parallel is a Core i7-7820X that read
1.7 W under AVX load until SVID support was enabled in the BIOS
(<https://www.hwinfo.com/forum/threads/cpu-package-power-strange-results.5516/>).
This board's BIOS may have an equivalent setting; it was not looked at, and
nothing was changed.

Decision: SysPilot cannot repair a meter that the firmware or the part
scales wrongly, so on the models whose datasheet gives package C-state power
(`0x3F` and `0x4F`) the probe watches the package counter for 250 ms and does
not declare `cpu.package.power` when it reads below 5 % of the TDP from
`MSR_PKG_POWER_INFO`. The datasheet (Intel Xeon E5 v4 Datasheet, volume one,
document 333809-003, section 2.10) lists the package C6 power, the deepest
idle state, as 14 W for the 120 W 16- and 18-core parts and 12 to 14 W for
the 140 to 160 W parts, at least 8.1 % of the TDP (the E5-2683 v3 row is 20 W
at 120 W) and as maximums. 5 % is below the lowest of those maximums, and
this machine reads 0.7 % idle. The datasheet gives maximums, not typical
values, so that a healthy meter in package C6 reads above 5 % is an
expectation, not something measured. The row of the E5-2680 v4 itself was not
in the text read. The feature detail says why:

> not available: package (reads 0.79 W, below the 6.0 W floor for a 120 W part)

Limits of the rule: it needs the probe to see a low reading, so a probe made
while the machine is under heavy load can read above 5 % (the 12 W run was
10 % of the TDP) and declare the sensor; it cannot tell a meter that is wrong
by 30 % from a right one; and it applies to two models only, because no source
for the other models' idle power was consulted. The DRAM domain needs no code:
a zero counter is already not declared (Linux's `rapl_check_domain` does the
same). The probe now takes about 300 ms instead of 50 to 60 ms, 250 ms of it
the watch window.

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

## Chipset: Intel PCH thermal

Read on 2026-11-23 through the `IntelPCHThermal` module (PawnIO.Modules
0.2.12), reads only.

- The chipset is the one behind `PCI\VEN_8086&DEV_8C24`, "8 Series/C220 Series
  Thermal". The module selected route 1 (PCI thermal BAR0): no modern LPC/eSPI
  controller is present, the thermal function at `00:1f.6` decodes memory, and
  BAR0 is a 64-bit memory BAR at `0x383ffff14000`, mapped for 256 bytes.
- `ioctl_identity` returned `[1, 0x8C248086, 0x1F06, 0x383FFFF14000, 0x100]`.
- `ioctl_read_raw` returned `[0xC6, 1]`: field 198, so 198 / 2 - 50 = 49.0 °C.
  The register held the same value on all 10 samples of a ten-second run
  (min 49.0, max 49.0) and on three reads half a second apart.
- In the `driver_sensors` run that read it, the package was at 51 °C and
  SYSTIN at 48 °C; the two SATA drives read 38 and 41 °C (see below). The
  sensor was not compared with a second instrument, so 49 °C is plausible but
  not independently confirmed.
- Only the PCI route ran here. The PWRM route has never read hardware in this
  project (see [PawnIO](pawnio.md#intelpchthermal)).

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
- The chipset temperature on the fixed PWRM route (modern client PCHs): replay
  only, and the layout is unverified against Intel documentation. Also the
  refusal paths of the module (no thermal function, a function that does not
  decode memory).
- Every Super I/O chip other than the NCT6779D, including all ITE IT87xx
  chips and the NCT6791D to NCT6799D layouts.
- The `spd-paging` feature, which is the only SMBus write in the code. It was
  never built into a run on this machine.
- Why the package meter is miscalibrated (firmware, VR telemetry or the part),
  and whether it reads correctly with virtualization-based security off.
- The package power floor on any model other than this one: no other Intel
  server or client part was available to check that 5 % of the TDP is safely
  below a healthy meter's idle reading. The two covered models rest on the
  datasheet's C6 maximums.
- Reading SMART attribute 194 on a drive behind a USB bridge or an HBA: only
  the two SATA drives of this machine were read, both on the AHCI driver.
- What `StorageDeviceTemperatureProperty` returns for a SATA drive on a driver
  that implements it. Microsoft's documentation for the property and its
  descriptor does not say where an ATA device's value comes from, and both
  SATA drives here refuse it. The provider tries it before SMART, so on such a
  machine the property's value wins; if it turns out to come from the Device
  Statistics page, the order should be flipped.
