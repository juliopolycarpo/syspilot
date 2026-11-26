# Research: GeForce tuning through NVIDIA's libraries

What NVIDIA's driver offers a program for setting the power limit, clocks,
voltages and fans of a GeForce card, and which of it decision 21 in
[decisions](../decisions.md) allows. Read from public sources on 2026-11-26.
No GeForce card was at hand, so nothing here is verified on hardware; claims
from third-party tools are marked as such. The question is #18.

## Two libraries

The driver installs two libraries a program can load:

- **NVAPI** (`nvapi64.dll`), the driver's general interface. Its headers are
  [public on GitHub](https://github.com/NVIDIA/nvapi).
- **NVML** (`nvml.dll`), the management library behind `nvidia-smi`,
  [documented by NVIDIA](https://docs.nvidia.com/deploy/nvml-api/index.html).
  It is not on the `PATH`. It sits in `System32` with a DCH driver, else
  under `%ProgramW6432%\NVIDIA Corporation\NVSMI\`.

## NVAPI sets nothing in public

The public headers read the values SysPilot would show and set none of
them. They have `NvAPI_GPU_GetPstates20`, `NvAPI_GPU_GetThermalSettings`,
`NvAPI_GPU_GetAllClockFrequencies` and `NvAPI_GPU_GetOverclockStatus`. They
have no `SetPstates20`, no power or thermal policy setter, no fan setter and
no voltage setter. `GetPstates20` reports a clock offset per state with its
range and an "editable" bit, but there is nothing public to write it with. An
NVIDIA moderator
[said so in 2025](https://forums.developer.nvidia.com/t/overclocking/342592):
"NVAPI SDK currently does not provide full GPU overclocking and undervolting
support."

The tools that do set these values on GeForce use private functions, reached
by number through `nvapi_QueryInterface`. These are not in the headers, and
their numbers and layouts come from watching other tools. They include
`NvAPI_GPU_SetPstates20` and `NvAPI_GPU_ClientPowerPoliciesSetStatus`, named in
the [nvapi-sys](https://docs.rs/nvapi-sys/0.1.3/nvapi_sys/gpu/pstate/private)
crate as "undocumented private API". Third-party tools report using them:
[nvoclock](https://github.com/arcnmx/nvoclock) for fans, limits, offsets and
the boost curve, tested on one Pascal card in 2017, and
[NVFC](https://github.com/graphitemaster/NVFC), which got as far as fans. How
far they work on current cards was not found.

**Not usable under decision 21.** A private function the vendor does not
document is not an interface it supports for tuning. It is the same kind of
guess decision 12 rules out for registers.

## NVML sets most of it, with GeForce support unclear

NVML documents setters for most values the Tuning page has
([device commands](https://docs.nvidia.com/deploy/nvml-api/api/group__nvmlDeviceCommands.html)):

| Value | Call | What the documentation says |
| --- | --- | --- |
| Power limit | `nvmlDeviceSetPowerManagementLimit` | Kepler or newer, "fully supported devices", administrator. Not kept across a restart or a driver unload. |
| Clock offsets, core and memory | `nvmlDeviceSetClockOffsets` | Maxwell or newer, privileged user. |
| Locked core clocks | `nvmlDeviceSetGpuLockedClocks`, `nvmlDeviceResetGpuLockedClocks` | Volta or newer, administrator. Back to default after a restart or a driver reload. |
| Locked memory clocks | `nvmlDeviceSetMemoryLockedClocks`, `nvmlDeviceResetMemoryLockedClocks` | Ampere or newer, administrator. |
| Fan | `nvmlDeviceSetFanSpeed_v2`, `nvmlDeviceSetFanControlPolicy`, `nvmlDeviceSetDefaultFanSpeed_v2` | Maxwell or newer; the last one restores the driver's own control. |
| Temperature threshold | `nvmlDeviceSetTemperatureThreshold` | Maxwell or newer. |
| Voltage | none | No setter. |

Whether these work on a GeForce card is the open question. NVML names
GeForce under ["limited support"](https://docs.nvidia.com/deploy/nvml-api/index.html),
and most setters are documented for "fully supported" devices: Tesla, Quadro
and some Titan cards. On a forum, users report that `nvmlDeviceSetClockOffsets`
[runs on a GTX 1650 Super and an RTX 4060 Laptop under Linux](https://forums.developer.nvidia.com/t/nvmldevicegetminmaxclockofpstate-nvmldevicesetclockoffsets-issues/318332),
with the offset applied to every performance state at once. Nothing primary
was found for the power limit, fan or temperature setters on GeForce under
Windows. A backend would have to read `NVML_ERROR_NOT_SUPPORTED` as "this
control does not exist", the way the Radeon backend reads its feature flags.

A clock offset for all states is a different control from the Radeon's clock
per state. The protocol already allows it: a control without a `state`.

## Running as a service

Not settled by any source. NVAPI's
[requirements](https://docs.nvidia.com/nvapi/sysreq.html) say only "Windows 10
or later". An
[archived R384 page](https://archive.docs.nvidia.com/gameworks/content/gameworkslibrary/coresdk/nvapi_r384.htm)
says "the application context using NVAPI must be launched in Session 1 or
higher", written for Windows Vista. Nothing was found for NVML in session 0.
The tuner runs as LocalSystem in session 0, so this needs the same SYSTEM
probe the Radeon backend needed before any code.

## Putting the defaults back

- **Power limit:** write the value `nvmlDeviceGetPowerManagementDefaultLimit`
  reports. The documentation points there but does not call it a reset.
- **Locked clocks:** the two `Reset` calls above.
- **Fan:** `nvmlDeviceSetDefaultFanSpeed_v2`.
- **Clock offsets:** not documented; writing 0 is the likely way and
  unverified.

Like the Radeon's, the values do not survive a restart or a driver reload,
so the service's rounds after start, wake and logon apply here as well.

## Licences

- **NVAPI headers:** MIT (`SPDX-License-Identifier: MIT` in
  [nvapi.h](https://github.com/NVIDIA/nvapi/blob/main/nvapi.h), and the
  repository's README). That is compatible with GPL-3.0-or-later as long as
  the notice is kept. Nothing stops a binding with its own declarations.
- **NVML header:** a custom NVIDIA licence that grants use "in individual and
  commercial software". It asks for its disclaimer in the documentation and
  in comments, and says nothing about modifying or redistributing. The
  [CUDA EULA](https://docs.nvidia.com/cuda/eula/index.html) forbids using its
  SDK in a way that would subject it to an open-source licence, and it is not
  clear whether `nvml.h` falls under that EULA. The safe course is the one the
  Radeon binding takes: own declarations of the few functions and
  structures, and `nvml.dll` loaded from the driver at run time, with no
  NVIDIA header in the repository. Whether that is enough is a legal
  question this note does not answer.

## What this means for a backend

A GeForce backend would speak NVML, not NVAPI, and offer the power limit,
one core and one memory clock offset, and the fan where the card accepts
them. It offers no voltages. Before any code, on a real GeForce card:

- [ ] which NVML setters a GeForce card accepts on Windows, per generation;
- [ ] whether NVML works from a LocalSystem process in session 0;
- [ ] what a clock offset of 0 does after other values were set, and whether
  the fan's default call gives control back to the driver as documented.
