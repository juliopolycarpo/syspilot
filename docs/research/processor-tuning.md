# Research: processor tuning through the power plan

What Windows lets a program set for the processor, what it does on Intel and
AMD parts, which of it decision 21 in [decisions](../decisions.md) allows,
and how it would live with the Performance page's tweaks. The question is
#19. Read from public sources on 2026-11-26. The values in the table were
read, not written, on the development machine: a Xeon E5-2680 v4 under
Windows 11 build 26300, which is neither hybrid nor known to use HWP.
Nothing was verified on a hybrid Intel or a Ryzen part.

## The settings

Processor power management lives in the power plan, subgroup
`54533251-82be-4824-96c1-47b60b740d00` (`SUB_PROCESSOR`). Microsoft documents
each setting under
[processor power management options](https://learn.microsoft.com/en-us/windows-hardware/customize/power-settings/configure-processor-power-management-options).
It files most of them as
[legacy](https://learn.microsoft.com/en-us/windows-hardware/customize/power-settings/legacy-configuration-options),
meaning no longer for platform makers to configure, though administrators
and users may still set them.

| Setting | GUID | Range | Balanced, AC / DC | Hidden |
| --- | --- | --- | --- | --- |
| [Maximum processor state](https://learn.microsoft.com/en-us/windows-hardware/customize/power-settings/options-for-perf-state-engine-maxperformance) (`PROCTHROTTLEMAX`) | `bc5038f7-23e0-4960-96da-33abaf5935ec` | 0–100 % | 100 / 100 | No |
| Minimum processor state (`PROCTHROTTLEMIN`) | `893dee8e-2bef-41e0-89c6-b55d0929964c` | 0–100 % | 5 / 5 | No |
| [Boost mode](https://learn.microsoft.com/windows-hardware/customize/power-settings/options-for-perf-state-engine-perfboostmode) (`PERFBOOSTMODE`) | `be337238-0d82-4146-a960-4f3749d470c7` | a choice | 2 / 2 | Yes |
| [Boost policy](https://learn.microsoft.com/en-us/windows-hardware/customize/power-settings/legacy-config-options-perfboostpol) (`PERFBOOSTPOL`) | `45bcc044-d885-43e2-8605-ee0ec6e96b59` | 0–100 % | 60 / 40 | Yes |
| [Maximum frequency](https://learn.microsoft.com/en-us/windows-hardware/customize/power-settings/options-for-perf-state-engine-maxfrequency) (`PROCFREQMAX`) | `75b0ae3f-bce0-45a7-8c89-c9611c25e100` | MHz, 0 = no limit | 0 / 0 | Yes |
| [Core parking, minimum cores](https://learn.microsoft.com/en-us/windows-hardware/customize/power-settings/options-for-core-parking-cpmincores) (`CPMINCORES`) | `0cc5b647-c1df-4637-891a-dec35c318583` | 0–100 % | 100 / 100 | Yes |
| Core parking, maximum cores (`CPMAXCORES`) | `ea062031-0e34-4ff1-9b6d-eb1059334028` | 0–100 % | 100 / 100 | Yes |
| [Energy performance preference](https://learn.microsoft.com/en-us/windows-hardware/customize/power-settings/options-for-perf-state-engine-perfenergypreference) (`PERFEPP`) | `36687f9e-e3a5-4dbf-b1dc-15eb381c6863` | 0–100 % | 33 / 50 | Yes |
| Heterogeneous policy (`HETEROPOLICY`) | `7f2f5cfa-f10c-4823-b5e1-e93ae85f46b5` | a choice, 0–4 | 4 / 4 | Yes |

Notes on the table:

- **Defaults** vary by machine, form factor and build. The ones above are
  this machine's Balanced plan, read with `PowerReadACDefaultIndex`.
- **Boost mode:** Microsoft lists values 0 to 4. This build also offers 5
  and 6.
- **Maximum frequency of 0:** Microsoft's page gives 100 to 64000 MHz.
  Reading 0 as "no limit" is inferred from the default.
- **Core parking:** the minimum at 100 % turns the parking algorithm off,
  as Microsoft documents.
- **Hybrid parts:** each setting has a class 1 twin that applies to the
  other kind of core, its GUID ending one digit higher (for example
  `...33abaf5935ed`). Intel's
  [hybrid guide](https://cdrdv2-public.intel.com/685869/optimizing%20_software_for_performance_hybrid_architecture.pdf)
  maps `PerfEnergyPreference` to the efficient cores and
  `PerfEnergyPreference1` to the performance cores. This build has class 2
  twins too, which Microsoft does not document.
- **Hidden:** this concerns only the Control Panel and `powercfg /q`. The
  API reads and writes hidden settings like any other.

The calls are the ones the engine's power actions already make:

1. `PowerWriteACValueIndex` or `PowerWriteDCValueIndex` on the active
   scheme;
2. then [`PowerSetActiveScheme`](https://learn.microsoft.com/en-us/windows/win32/api/powersetting/nf-powersetting-powersetactivescheme),
   without which a change to the active scheme does not take effect.

By default users may change power settings
([power policy settings](https://learn.microsoft.com/en-us/windows/win32/power/power-policy-settings)).
The service is LocalSystem anyway.

## What they do

**Intel with HWP.** Windows writes the maximum and minimum state, and the
energy preference, into the processor's HWP request. The
[SDM](https://cdrdv2-public.intel.com/835755/253669-sdm-vol-3b.pdf)
(Vol. 3B, 16.4) calls the maximum a hint that the processor may exceed. It
maps the preference to 0–255, from performance to efficiency. Microsoft
[says](https://learn.microsoft.com/en-us/windows-server/administration/performance-tuning/hardware/power/processor-power-management-tuning)
that on HWP parts the minimum and maximum bound the range and the
preference biases it. It also says the older increase and decrease
thresholds barely matter there.

**AMD.** Windows uses CPPC2, and the chipset driver brings its own
processor settings.
[AMD](https://www.amd.com/en/resources/support-articles/faqs/PA-400.html)
asks for its Ryzen Balanced plan on Zen+ and Zen 2. On Zen 3 it asks
for a provisioning package on top of the stock plan. AMD does not say
what either one changes.

Effects that are only reported, not documented:

- A maximum state below 100 % turns boost off on many machines. Users
  report it
  ([Q&A](https://learn.microsoft.com/en-us/answers/a/7909151)), and it fits
  the HWP hint, but no primary source says so.
- The maximum frequency is ignored on some laptops, according to an OEM
  forum.

**The power mode overrides the plan.** Each position of the Settings power
mode slider is a small overlay plan. Its processor settings, the energy
preference among them, win over the active plan's
([slider](https://learn.microsoft.com/en-us/windows-hardware/customize/desktop/customize-power-slider)).
Three consequences:

- A value written to the plan can be shadowed without the plan changing.
- The only call that sets the overlay, `PowerSetActiveOverlayScheme`, is
  undocumented. Decision 21 asks for an interface its maker supports, so it
  is out.
- `PowerRegisterForEffectivePowerModeNotifications` is documented and
  tells when the mode changes.

## Power limits are out

There is no supported user-mode way to set PL1 and PL2, or PPT, TDC and EDC:

- **Intel Dynamic Tuning** is configured by the machine maker and has
  [no user-facing interface](https://www.intel.com/content/www/us/en/support/articles/000102775/processors.html).
- **Intel XTU** has an SDK that, per a 2026 press release, Intel opens to
  partners. It is not public and its terms are unknown.
- **AMD's Ryzen Master Monitoring SDK**
  [reads](https://www.amd.com/en/developer/ryzen-master-monitoring-sdk.html)
  these limits but does not set them. Setting them is a feature of the
  Ryzen Master app only.

What is left is model-specific registers, which decision 12 rules out. A
processor device would offer only power plan settings, and no undervolt.

## Living with the tweaks

The engine already owns this ground. A catalog entry can carry a `power`
action, and two entries in `performance.toml` write processor settings of
the active plan on AC power:

- `performance.core-parking-ac`: minimum cores at 100 %;
- `performance.processor-minimum-state-ac`: minimum state at 100 %.

The engine watches the power schemes subtree for drift (decision 4) and puts
those values back. [The catalog](../tweak-catalog.md#ownership) gives every
setting exactly one owner. A Tuning control on the same setting would be a
second owner, and the two would fight the way two tools do.

Several other facts shape a design:

- **The plan holds the values.** A value is stored per plan, so a write to
  one plan does nothing once another is active. The engine already writes
  to the active plan and checks again whenever the plan changes; a tuner
  that only looks at its triggers would miss that.
- **One plan for the machine.** Microsoft documents `PowerSetActiveScheme`
  as acting "for the current user", but the active plan is a single value,
  `ActivePowerScheme` under
  `HKLM\SYSTEM\CurrentControlSet\Control\Power\User\PowerSchemes`. A plan
  the service sets is the one every session runs. That is how the engine's
  power actions already work from the service.
- **Defaults** come from `PowerReadACDefaultIndex` and
  `PowerReadDCDefaultIndex`
  ([doc](https://learn.microsoft.com/en-us/windows/win32/api/powrprof/nf-powrprof-powerreadacdefaultindex)).
  These take the plan's personality (Balanced, High performance or Power
  saver), not the plan itself. The call worked on this machine with the
  Balanced GUID.
- **No crash isolation is needed.** These writes cannot hang a driver, so
  the tuner child process, which exists to isolate the graphics driver,
  adds nothing here.

That points away from a tuner backend. Two designs fit what exists:

- **Catalog entries** for choices: boost on or off, core parking off. These
  already work with drift, revert and ownership.
- **Numeric controls** (maximum state, maximum frequency, energy
  preference) written by the engine from the service. They would be shown
  on the Tuning page, and the ownership test would cover catalog and tuning
  together. Each control would have to say that the power mode can shadow
  it.

Which one, and whether the processor belongs on the Tuning page at all, is a
product decision. Before either, someone has to try on a hybrid Intel and a
Ryzen machine:

- [ ] whether a maximum state below 100 % turns boost off;
- [ ] whether the energy preference written to the plan holds under each
  power mode.
