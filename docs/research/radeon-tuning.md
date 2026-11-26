# Research: Radeon tuning through ADL

What the Radeon driver offers for setting clocks, voltages, limits and fans,
which part of it SysPilot uses, and what was verified on the development
machine. The decision this supports is 21 in [decisions](../decisions.md); how
the feature behaves is in [tuning](../tuning.md).

## The interface

The Radeon driver installs `atiadlxx.dll` (the AMD Display Library, ADL) in
the system directory. AMD documents it in the
[ADL SDK](https://github.com/GPUOpen-LibrariesAndSDKs/display-library); AMD
Software uses the same library. Everything in it goes through the display
driver: no register is touched from user mode, and no other driver is needed.

Tuning lives in the *Overdrive* family, and which generation applies is asked
per adapter with `ADL2_Overdrive_Caps`:

| Version | Cards | State in SysPilot |
| --- | --- | --- |
| 5, 6 | Before the RX 400 series | Not supported. |
| 7 (Overdrive N) | RX 400 and RX 500 series (Polaris) | Supported; verified on an RX 550 only. |
| 8 (Overdrive 8) | RX 5000 series and later | Not supported yet. |

Overdrive N, as used:

| Call | Reads or sets |
| --- | --- |
| `ADL2_OverdriveN_CapabilitiesX2_Get` | The range of every value and a set of feature flags that say which values exist. |
| `ADL2_OverdriveN_SystemClocksX2_Get` / `_Set` | The performance states of the graphics processor: a clock (in units of 10 kHz) and a voltage (mV) each. |
| `ADL2_OverdriveN_MemoryClocksX2_Get` / `_Set` | The same for the memory. |
| `ADL2_OverdriveN_PowerLimit_Get` / `_Set` | The power limit (%), the temperature limit and the lowest clock used to stay quiet. |
| `ADL2_OverdriveN_FanControl_Get` / `_Set` | The fan's target temperature and its speed bounds. |

Each `Get` takes a mode: current or default. That is where the defaults
SysPilot shows come from, and how a reset works: read the defaults, set them.
For the fan and the power structure a set in "auto" mode does the same.

Rules the binding follows (`crates/hw/src/tuning/radeon.rs`):

- A value exists only if its feature flag is set. A card without the fan
  flags gets no fan values, even though the calls succeed.
- Each state says which of its two values may be changed. Only those are
  offered; on the card below that leaves out processor state 0 and all but
  one memory state.
- Clocks and voltages must not fall from one state to the next. SysPilot
  refuses such a table before anything is written, so that the reason can
  be named.
- The ranges come from the driver. The `step` it reports is not a rule: the
  defaults themselves are not on it (551 and 1183 MHz with a step that would
  not reach them), so SysPilot treats it as a hint for an editor.
- All values of a setting are checked before the first write, and only the
  structures that differ from what the card has are written.

## On the development machine

Radeon RX 550 (PCI device 699F), driver 31.0.21925.1001, Overdrive version 7.

| Value | Range | Default |
| --- | --- | --- |
| Power limit | −75 to 75 % | 0 % |
| Lowest clock when quiet | 214 to 1183 MHz | 850 MHz |
| Processor states 1 to 7, clock | 214 to 1800 MHz | 551, 734, 980, 1046, 1098, 1124, 1183 MHz |
| Processor states 1 to 7, voltage | 700 to 1075 mV | 750, 750, 875, 900, 950, 1000, 1050 mV |
| Memory state 2, clock | 1000 to 2000 MHz | 1500 MHz |
| Memory state 2, voltage | 700 to 1075 mV | 875 mV |

The driver sets none of the fan or temperature flags for this card, so it
offers no fan values. Its "memory voltage" is what the interface calls it.
Users of AMD's own tool report that on these cards it is not a voltage of
the memory chips but the lowest processor voltage used while the memory runs
in that state; this was not measured here.

Verified here:

- Reading everything above, from an interactive session and from session 0 as
  LocalSystem, which is where the tuner runs.
- Setting the power limit and reading it back, through the service, the
  tuner and the app, started from an elevated interactive session; turning
  tuning off put the default back.
- The same through an installed service, so with the tuner in session 0;
  uninstalling put the default back.

One trap of session 0: the "present" flag of every output is off there,
because it says whether the output is part of the caller's desktop. The
binding does not read it; a context created for connected adapters lists
only cards that are in the machine.

Not verified:

- That values are gone after a restart of the driver or the machine, which
  is what AMD's own tool assumes when it sets its profile again at sign-in.

- Setting clocks or voltages on real hardware. The code path is the same as
  for the power limit up to the structure that is written, and is covered by
  tests against a fake of the library that holds the RX 550's tables.
- Any card other than this one, and any card that offers fan values.

## Left for later

- **Overdrive 8**, for RX 5000 cards and later: one settings array instead of
  per-state tables, and a fan curve of five points.
- **NVIDIA.** To be researched. The starting point is what the public NVAPI
  SDK and NVML document; calls that only third-party tools know are out, for
  the reason decision 21 gives.
- **Processors.** The development machine's processor has a locked clock and
  a power meter that does not match its limits. What Windows itself offers
  (the power plan's processor settings) already fits SysPilot's power
  actions and needs no new interface.
