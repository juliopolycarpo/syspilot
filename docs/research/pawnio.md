# PawnIO: device interface, modules and licences

Research note. Facts were read from source at the stated tag or commit;
anything not confirmed that way is marked **unverified**. Nothing here was
confirmed by calling the driver; the driver integration tests do that.

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
`0x611` package energy, `0x619` DRAM energy, `0x639` PP0 energy, `0x641`
PP1 energy, `0x64D` platform energy.

The module does not pin the calling thread. The caller sets the thread
affinity to the target core before each per-core read.

### AMDFamily17

Families 17h to 1Ah.

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

`ioctl_smbus_xfer` can write to any device on the bus. SysPilot issues one
kind of write: the DDR4 SPD page select.

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
- On Haswell-EP and later server parts the DRAM domain uses a fixed unit of
  15.3 µJ instead of the unit in `0x606` (**unverified** against Intel
  documentation here; checked on hardware before the DRAM sensor is shown).

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
| `0xC5` / `0x60` | NCT6779D |
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
- Temperatures (NCT6779D): integer register and half-degree register, bit 7:
  CPUTIN `0x073`/`0x074`, SYSTIN `0x075`/`0x076`, AUXTIN0 `0x077`/`0x078`,
  AUXTIN1 `0x079`/`0x07A`, AUXTIN2 `0x07B`/`0x07C`, AUXTIN3 `0x150`/`0x151`,
  PECI 0 `0x027`. Readings outside −55…125 °C are dropped.

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
  DDR4, `0x12` DDR5).
- DDR4: 512 bytes in two pages. Select page `p` by writing to address
  `0x36 + p`. Bit 7 of byte `0x0E` announces a thermal sensor at
  `0x18 | (spd address & 7)`; temperature is register `0x05`, big-endian
  word, 13-bit two's complement in 1/16 °C.
- DDR5: the SPD5118 hub at the same `0x50`–`0x57` address reports the
  temperature at register `0x31` in the same format.
- On server platforms the DIMM bus can hang off the CPU's memory controller
  instead of the PCH; `SmbusI801` then finds no modules. Verified per
  machine by the driver tests.

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
