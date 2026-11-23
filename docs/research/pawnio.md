# PawnIO: device interface, modules and licences

Research note. Facts were read from source at the stated tag or commit;
anything not confirmed that way is marked **unverified**. The driver
integration tests (`crates/hw/tests/driver.rs`) call the driver on the
[reference machine](reference-machine.md); what they exercised is listed
under [Verification status](#verification-status) at the end, and everything
else in the register sections below was written from the sources and checked
by replay only.

Sources:

- Driver: <https://github.com/namazso/PawnIO> at tag `2.2.0`
  (`PawnIO/include/pawnio_um.h`, `PawnIO/src/vm.cpp`, `PawnIO/PawnIO.inf.in`,
  `PawnIOLib/PawnIOLib.cpp`, `COPYING`).
- Modules: <https://github.com/namazso/PawnIO.Modules> at tag `0.2.12`, and
  its wiki.
- Reference client: <https://github.com/LibreHardwareMonitor/LibreHardwareMonitor>
  at commit `9e2d677` (`LibreHardwareMonitorLib/Hardware/…`).

## Device interface

| Item | Value |
|---|---|
| NT device | `\Device\PawnIO` |
| User-mode path | `\\?\GLOBALROOT\Device\PawnIO` |
| Access | `GENERIC_READ \| GENERIC_WRITE`, share read/write, `OPEN_EXISTING` |
| Device security | `D:P(A;;GA;;;SY)(A;;GA;;;BA)`: SYSTEM and Administrators only |
| Device type | `0xA1B2` (41394) |
| Method / access | `METHOD_BUFFERED`, `FILE_ANY_ACCESS` for every IOCTL |

| IOCTL | Function | Code |
|---|---|---|
| `IOCTL_PIO_LOAD_BINARY` | `0x821` | `0xA1B22084` |
| `IOCTL_PIO_EXECUTE_FN` | `0x841` | `0xA1B22104` |
| `IOCTL_PIO_VERSION` | `0x861` | `0xA1B22184` |

One handle holds one loaded module. Each module the service uses gets its
own handle.

### Version

`IOCTL_PIO_VERSION` takes no input and requires an output buffer of exactly
4 bytes. It returns `(major << 16) | (minor << 8) | patch`; `0x00020200` for
2.2.0. `pawnio_version` in PawnIOLib returns the library API version, not
the driver version, and is not usable for detection.

### Load

Input: the release `.bin` file as is, which is `[u32 LE signature length]
[signature][AMX blob]`. No output. After a successful signature check the
module's `main()` runs; a failure status from `main()` becomes the IOCTL
status.

| Status | Meaning |
|---|---|
| `STATUS_ALREADY_INITIALIZED` (`0xC0000510`) | The handle already has a module. |
| `STATUS_INVALID_SIGNATURE` (`0xC000A000`) | The blob is not signed by the trusted key (2.2.0 passes the BCrypt status through; **unverified** at runtime). |
| `STATUS_NOT_SUPPORTED` (`0xC00000BB`) | The module refuses this platform (wrong CPU vendor, missing device). |
| `STATUS_NO_SUCH_DEVICE` (`0xC000000E`) | `IntelPCHThermal`'s `main()` found none of the devices it supports. SysPilot counts it as a refusal, like `STATUS_NOT_SUPPORTED` (**not seen at runtime**: the reference machine has the device). |

### Execute

Input: a 32-byte, NUL-padded ASCII function name (at most 31 characters,
must start with `ioctl_`) followed by N little-endian `u64` cells. Output:
M `u64` cells. Functions declared with fixed sizes reject any other count
with `STATUS_INVALID_PARAMETER`.

| Status | Meaning |
|---|---|
| `STATUS_INVALID_PARAMETER` (`0xC000000D`) | Short input, bad name, wrong cell count, or nothing loaded. |
| `STATUS_OBJECT_NAME_NOT_FOUND` (`0xC0000034`) | No such function in the module. |
| `STATUS_ACCESS_DENIED` (`0xC0000022`) | The module's allow-list refuses the argument (e.g. an MSR). |
| `STATUS_UNSUCCESSFUL` (`0xC0000001`) | The module aborted. |

Caveats in 2.2.0:

- The returned byte count is the full output buffer length, not the number
  of cells the function wrote.
- Input and output share one system buffer and the output tail is not
  zeroed. Callers must size the output exactly and not trust cells the
  function did not write.

### Detecting an installation

- Uninstall key `HKLM\SOFTWARE\Microsoft\Windows\CurrentVersion\Uninstall\PawnIO`:
  `DisplayVersion` (e.g. `2.2.0.0`), `InstallLocation`, `Publisher`.
- Service `PawnIO`: kernel driver, demand start. Whether opening the device
  starts a stopped service is **unverified**, so the service state is
  checked separately.
- The authoritative version is `IOCTL_PIO_VERSION` after opening the device.
- Opening the device when the driver is absent is expected to fail with
  `ERROR_FILE_NOT_FOUND` (**unverified**).

Users are sent to <https://pawnio.eu> to install it.

## Module signing

The official driver trusts a single RSA-4096 key (`k_pubkey_namazso_2023` in
`vm.cpp`): PKCS#1 v1.5 over SHA-256 of the AMX blob. Only modules merged
into PawnIO.Modules and released by its author load on the official driver.
SysPilot therefore uses the upstream modules unchanged and cannot ship its
own.

## Module release

`0.2.12` (2026-11-17), asset
<https://github.com/namazso/PawnIO.Modules/releases/download/0.2.12/release_0_2_12.zip>,
83,556 bytes, SHA-256
`09698897ce19b92f1043a2e322cfcf0b1fe823e0b576971b178a65244fb0af9d`.

The archive holds `COPYING` (LGPL-2.1) and 28 `.bin` files. The ones
SysPilot uses:

| Module | SHA-256 |
|---|---|
| `IntelMSR.bin` | `30a5a302283de1e080dfee99e686eca91d7e53e911095cb91a320fb961f1ce12` |
| `AMDFamily17.bin` | `dae74615761b78bdf064dfb3e136252ddcc6fc727d88f14738d0e5800d427a91` |
| `LpcIO.bin` | `b3896a1cab0d808fca31fe2ebcae045d59dac690da87b17c858bb8da357eb45e` |
| `SmbusI801.bin` | `a0f7d066e7efda28c0e754c1f52dbd8dc280d388ba8575b514a80cb67490530c` |
| `IntelPCHThermal.bin` | `478500ae78aebef3ae7a0b5fe4848d2d5737e8f7c5d51200c284a61c21d7865d` |

## Module functions

All cells are `u64`. R is read-only, W changes hardware state.

### IntelMSR

Requires an Intel x64 CPU.

| Function | In → out | |
|---|---|---|
| `ioctl_read_msr` | `[msr]` → `[value]` | R |
| `ioctl_write_msr` | `[msr, value]` → `[]` | W, never called by SysPilot |

Read allow-list (relevant part): `0xCE` platform info, `0xE7`/`0xE8`
MPERF/APERF, `0x198` perf status, `0x19C` core thermal status, `0x1A2`
temperature target, `0x1B1` package thermal status, `0x606` RAPL units,
`0x611` package energy, `0x614` package power info, `0x619` DRAM energy,
`0x639` PP0 energy, `0x641` PP1 energy, `0x64D` platform energy. On the
reference machine `0x641` and `0x64D` pass the allow-list but fail with
`STATUS_PRIVILEGED_INSTRUCTION` (the processor raises `#GP`: no such domain),
and `0x1A0`, `0x1FC` and `0x3A` are refused with `STATUS_ACCESS_DENIED`.

The module does not pin the calling thread. The caller sets the thread
affinity to the target core before each per-core read.

### AMDFamily17

Families 17h to 1Ah. **Replay only**: no AMD machine has run this module.

| Function | In → out | |
|---|---|---|
| `ioctl_read_msr` | `[msr]` → `[value]` | R |
| `ioctl_read_smn` | `[address]` → `[value]` | R, needs the PCI mutex |
| `ioctl_write_msr` | `[msr, value]` → `[]` | W, never called |

### LpcIO

| Function | In → out | |
|---|---|---|
| `ioctl_select_slot` | `[slot]` → `[]` | Slot 0 is ports `0x2E`/`0x2F`, slot 1 is `0x4E`/`0x4F`. |
| `ioctl_find_bars` | `[]` → `[]` | Discovers the chip's I/O ranges. The chip must be in configuration mode. |
| `ioctl_pio_inb` | `[port]` → `[value]` | R. Only the selected register ports, `0x25C`/`0x25D`, and discovered ranges. |
| `ioctl_pio_outb` | `[port, value]` → `[]` | W. Same port restriction. |
| `ioctl_superio_inb` | `[register]` → `[value]` | R |
| `ioctl_superio_inw` | `[register]` → `[value]` | R |
| `ioctl_superio_outb` | `[register, value]` → `[]` | W |

Reading a Super I/O chip is not possible without port writes: entering
configuration mode, selecting a logical device and selecting a register
bank are all writes to index registers. SysPilot treats those index writes
as part of a read transaction and never writes a data register that
changes chip behaviour unless fan control is compiled in and active.

### SmbusI801

Intel PCH SMBus controller. Fails with `STATUS_NOT_SUPPORTED` when no
supported controller is found.

| Function | In → out | |
|---|---|---|
| `ioctl_identity` | `[]` → 3 cells | R |
| `ioctl_write_protection` | `[]` → `[flag]` | R. Whether SPD write protection is on. |
| `ioctl_smbus_xfer` | `[addr7, rw, command, protocol, data…]` (9 cells) → 5 cells | `rw`: 1 read, 0 write. |

Protocols: 0 quick, 1 byte, 2 byte data, 3 word data, 4 process call,
5 block data, 7 block process call, 8 I2C block data. A read returns the
value in `out[0]`; a block read returns the length in `out[0]` and the
bytes packed eight per cell after it.

| Status | Meaning |
|---|---|
| `STATUS_NO_SUCH_DEVICE` | No acknowledge: nothing at that address. |
| `STATUS_RETRY` | Bus error. |
| `STATUS_IO_TIMEOUT` | The bus is held (for example by firmware). |
| `STATUS_DEVICE_BUSY` | The host controller is busy. |

`ioctl_smbus_xfer` can write to any device on the bus. The default build of
SysPilot issues no SMBus write. The one write it can make, the DDR4 SPD page
select, exists only with the off-by-default `spd-paging` cargo feature of
`syspilot-hw`.

### IntelPCHThermal

Read only: the source has no write primitive (no `pci_config_write_*`,
`virtual_write_*` or MSR write). Requires an Intel x64 CPU. Added in
PawnIO.Modules pull request 79 (<https://github.com/namazso/PawnIO.Modules/pull/79>).

`main()` first looks for one of the LPC/eSPI device IDs of recent client
chipsets on bus 0 and, if it finds one, maps the fixed window at
`0xFE000000` (route 2). Only when no such controller exists does it look for
one of the legacy thermal functions on bus 0 (`0x9C24`, `0x8C24`, `0x9CA4`,
`0x9D31`, `0xA131`, `0x9DF9`, `0xA379`, `0x02F9`, `0x06F9`, `0xA1B1`, `0x8D24`,
`0xA2B1`), requires PCI command bit 1 (memory decoding) to be set and BAR0 to
be a programmed memory BAR, and maps 256 bytes of it (route 1). It fails with
`STATUS_NO_SUCH_DEVICE` when it finds neither, and `STATUS_NOT_SUPPORTED` when
the function is not decoding memory or BAR0 is empty.

| Function | In → out | |
|---|---|---|
| `ioctl_read_raw` | `[]` → `[raw, route]` | R. Route 1 is the PCI thermal BAR0 (16-bit read at offset 0), route 2 the fixed PWRM window (32-bit read at `+0x1560`). |
| `ioctl_identity` | `[]` → `[route, didvid, pci_address, mmio_base, size]` | R. `didvid` is `device << 16 \| vendor`, `pci_address` is `bus << 16 \| device << 8 \| function`. |

The module returns the register unvalidated; the consumer decodes it:

- **Route 1** (PCI BAR0): bits 8:0 are the reading, `field / 2 - 50` °C. The
  Linux driver `drivers/thermal/intel/intel_pch_thermal.c` (checked on
  2026-11-23) masks the register with `WPT_TEMP_TSR` (`0x01FF`) and converts
  with `GET_WPT_TEMP(x) = x * 1000 / 2 + (-50 * 1000)` millidegrees ("Resolution
  of 1/2 degree C and an offset of -50C"). Its device ID list is the one in the
  module, minus `0xA2B1`, which the module's author took from HWMonitor.
- **Route 2** (PWRM): bit 9 is a valid flag and bits 8:0 are whole degrees.
  **Unverified**: Linux has no driver for this register and no Intel document
  was consulted. The layout is the module author's, from an analysis of
  HWMonitor 1.65 and one read on an Intel HM870 platform (`0xFE001560` read
  `0x80000231`, valid bit set, 49 °C, against 43 °C from the board's embedded
  controller). Bit 31 is set in that value and is not part of the reading.
- On both routes a field of `0` or `0x1FF` is not a reading. SysPilot also
  drops results outside 0 to 125 °C.

## Reading hardware

### Shared bus mutexes

Monitoring tools serialise bus access through named global mutexes. A tool
that skips them corrupts the other tools' multi-step transactions.

| Bus | Mutex |
|---|---|
| ISA / Super I/O | `Global\Access_ISABUS.HTP.Method` |
| SMBus | `Global\Access_SMBUS.HTP.Method` |
| Embedded controller | `Global\Access_EC` |
| PCI configuration | `Global\Access_PCI` |

LibreHardwareMonitor creates them with an ACL open to everyone and waits
10 to 100 ms.

### Intel CPU

- TjMax: MSR `0x1A2`, bits 23:16. Fall back to 100 °C.
- Core temperature: MSR `0x19C` on that core. Valid when bit 31 is set;
  temperature is TjMax minus bits 22:16.
- Package temperature: MSR `0x1B1`, same layout.
- Energy unit: MSR `0x606`, joules = 1 / 2^(bits 12:8).
- Power: difference of the 32-bit wrapping energy counters (`0x611`
  package, `0x639` cores, `0x619` DRAM) times the unit, divided by elapsed
  time.
- On Haswell-EP and Broadwell-EP the DRAM domain uses a fixed unit of 15.3 µJ
  (2^-16 J) instead of the unit in `0x606`. Checked on 2026-11-23 against
  Linux: `arch/x86/events/rapl.c` sets the DRAM hardware unit to 16 for
  Haswell-X and Broadwell-X (citing the Haswell-EP datasheet, volume 2,
  330784-001; the commit that added it says the DRAM domain "will be counted 4x
  higher" without it), and `intel_rapl_common.c` sets `dram_domain_energy_unit
  = 15300` for the same two models. The later parts in the code's table
  (Skylake-SP and the others) were not checked. The reference machine's DRAM
  counter reads 0, so the sensor has never been shown on hardware.
- Package power is not offered on models `0x3F` and `0x4F` when the counter
  reads below 5 % of the TDP in `MSR_PKG_POWER_INFO` (`0x614`, bits 14:0 in
  the power unit of `0x606` bits 3:0), read over a 250 ms window at probe
  time. The reference machine's meter reads 0.8 W for a 120 W part; see
  [the reference machine](reference-machine.md).

### Nuvoton NCT67xx Super I/O

1. Enter configuration mode: write `0x87` twice to the register port.
2. Chip ID: register `0x20`, revision `0x21`.
3. `ioctl_find_bars`, select logical device `0x0B` (register `0x07`), read
   the base address word at `0x60` twice; the reads must match. Mask with
   `0xFFF8` when the low three bits are 5. Invalid when below `0x100` or
   `(address & 0xF007) != 0`.
4. Leave configuration mode: write `0xAA`.
5. Hardware monitor access: address register at base + 5, data register at
   base + 6, bank select register `0x4E`. To read 16-bit address `a`: write
   `0x4E` to base + 5, `a >> 8` to base + 6, `a & 0xFF` to base + 5, read
   base + 6.
6. Vendor check: registers `0x804F` (high) and `0x004F` (low) read `0x5CA3`.

| ID / revision | Chip |
|---|---|
| `0xB4` / `0x7x` | NCT6771F |
| `0xC3` / `0x30` | NCT6776F |
| `0xC5` / `0x6x` | NCT6779D (the reference machine reads revision `0x62`; the low nibble is ignored) |
| `0xC8` / `0x03` | NCT6791D |
| `0xC9` / `0x11`, `0x13` | NCT6792D, NCT6792D-A |
| `0xD1` / `0x21` | NCT6793D |
| `0xD3` / `0x52` | NCT6795D |
| `0xD4` / `0x23`, `0x2A` | NCT6796D, NCT6796D-R |
| `0xD4` / `0x51` | NCT6797D |
| `0xD4` / `0x2B` | NCT6798D |
| `0xD8` / `0x02` | NCT6799D |

NCT6779D and later:

- Voltages: registers `0x480`–`0x48F`, 8 mV per count. Zero is invalid.
- Fan tachometers: counts at `0x4B0`, `0x4B2`, `0x4B4`, `0x4B6`, `0x4B8`,
  `0x4BA`, `0x4CC`: `(high << 5) | (low & 0x1F)`; RPM = 1,350,000 / count.
  A count at or above `0x1FFF` means stopped; below `0x15` is invalid.
- PWM read-back: `0x001`, `0x003`, `0x011`, `0x013`, `0x015`, `0x017`,
  `0x029`, percent = value / 2.55.
- Temperatures (NCT6779D): seven slots, each an integer register, a
  half-degree register (bit 7, none for the first) and a source-select
  register: `0x027` (select `0x621`), `0x073`/`0x074` (`0x100`),
  `0x075`/`0x076` (`0x200`), `0x077`/`0x078` (`0x300`), `0x079`/`0x07A`
  (`0x800`), `0x07B`/`0x07C` (`0x900`), `0x150`/`0x151` (`0x622`). Bits 4:0 of
  the select register name the source the slot shows: 1 SYSTIN, 2 CPUTIN,
  3 AUXTIN0, 4 AUXTIN1, 5 AUXTIN2, 6 AUXTIN3, `0x10` PECI 0. The first slot
  with a plausible reading wins.
- A source no slot routes is read from an integer-only alternate register
  (SYSTIN `0x490`, CPUTIN `0x491`, AUXTIN0 to AUXTIN3 `0x492`–`0x495`), kept
  when it reads 1…125 °C, as LibreHardwareMonitor does. The Linux driver does
  not read them; see the [reference machine](reference-machine.md) for what
  that does to unconnected AUXTIN inputs.
- Readings outside −55…125 °C are dropped.

### ITE IT87xx Super I/O

1. Enter: write `0x87`, `0x01`, `0x55`, then `0x55` (port `0x2E`) or `0xAA`
   (port `0x4E`).
2. Chip ID: word at register `0x20` (e.g. `0x8628`).
3. Hardware monitor is logical device 4, base address at `0x60`/`0x61`.
4. Exit: write `0x02` to register `0x02`.
5. Access: address register at base + 5, data at base + 6. Vendor register
   `0x58` reads `0x90`.
6. Temperatures at `0x29 + i`; voltages at `0x20`–`0x28`; fan tachometers
   16-bit, low bytes `0x0D`, `0x0E`, `0x0F`, `0x80`, `0x82`, `0x4C`, high
   bytes `0x18`, `0x19`, `0x1A`, `0x81`, `0x83`, `0x4D`;
   RPM = 1,350,000 / (count × 2).

### Memory modules over SMBus

- SPD EEPROMs answer at `0x50`–`0x57`; byte 2 is the memory type (`0x0C`
  DDR4, `0x12` DDR5). Byte 2 is read from page 0, which the EEPROMs power up
  on.
- DDR4: 512 bytes in two pages. Select page `p` by writing to address
  `0x36 + p`. Bit 7 of byte `0x0E` announces a thermal sensor at
  `0x18 | (spd address & 7)`; temperature is register `0x05`, big-endian
  word, 13-bit two's complement in 1/16 °C (the JC-42.4 / TSE2004 layout).
- DDR5: the SPD5118 hub at the same `0x50`–`0x57` address reports the
  temperature at register `0x31`. The SMBus word read returns the low byte
  first, so the value is little-endian: bits 12:2 are an 11-bit two's
  complement count in 0.25 °C, the low two bits are not part of the value
  (the Linux `spd5118` driver computes `sign_extend32(reg >> 2, 10) * 250`
  millidegrees). It is not the DDR4 format.
- **No temperature read needs the page select.** The JC-42.4 sensors at
  `0x18`–`0x1F` and the SPD5118 hub registers (below `0x80`) are not paged.
  Only the SPD contents on page 1 (manufacturer, part number) need it, and
  so does making sure the bus is on page 0 before reading byte 2 and byte
  `0x0E`: without the select, a bus another tool left on page 1 shows the
  wrong bytes and a DDR4 module can be missed. SysPilot therefore reads page
  0 only and makes no SMBus write unless built with `spd-paging`.
- On server platforms the DIMM bus can hang off the CPU's memory controller
  instead of the PCH; `SmbusI801` then finds no modules. This is what the
  reference machine shows.

## Verification status

Run on the [reference machine](reference-machine.md) (Xeon E5-2680 v4,
NCT6779D, i801 SMBus with no module behind it) with driver 2.2.0 and modules
0.2.12:

- **Read on hardware**: the device interface (open, version, load, execute),
  `IntelMSR` (temperatures, RAPL package counter), `LpcIO` with the NCT6779D
  (detection, 26 sensors), `SmbusI801` identity and the empty scan of
  `0x50`–`0x57`, and `IntelPCHThermal` on route 1 (the thermal function
  `8086:8C24`: identity and raw reads).
- **Replay only** (recorded or hand-written registers, no driver):
  - the AMD provider and `AMDFamily17` (SMN thermal block, CCD layout, Tctl
    offsets);
  - DIMM decoding: DDR4 SPD, the JC-42.4 sensor and the SPD5118 hub;
  - every Super I/O chip other than the NCT6779D: the ITE IT87xx family and
    the NCT6791D to NCT6799D layouts, the NCT6771F and NCT6776F;
  - `IntelPCHThermal` route 2 (the fixed PWRM window and its valid bit), and
    the refusal paths (`STATUS_NO_SUCH_DEVICE`, a function that does not decode
    memory);
  - the Intel DRAM energy unit and the PP0 domain;
  - the `spd-paging` write path.
- **Not exercised at all**: the driver's error statuses beyond what the
  tests provoke (`STATUS_INVALID_SIGNATURE` at runtime, a stopped service),
  and anything that writes (no write was issued apart from the Super I/O
  index writes a read needs).

## Licences

- **Driver**: GPL-2.0 with this exception (`COPYING`): "…as a special
  exception, the copyright holders of PawnIO give you permission to combine
  PawnIO program with free software programs or libraries that are released
  under the GNU LGPL and with independent modules that communicate with
  PawnIO solely through the device IO control interface. … Note that this
  exception does not include programs that communicate with PawnIO over the
  Pawn interface." SysPilot talks to the driver only through
  `DeviceIoControl`.
- **Official driver binaries**: proprietary; redistribution of the
  installer is allowed, and the author recommends sending users to
  <https://pawnio.eu>. SysPilot does not distribute the driver.
- **PawnIOLib**: LGPL-2.1-or-later. Not used.
- **Modules**: LGPL-2.1-or-later. Shipped unmodified as separate files next
  to the LGPL text, never embedded in an executable.
