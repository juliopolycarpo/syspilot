# UI toolkit choice

Research note, 2026-11. The UI is a thin client over a named pipe, so the
toolkit is replaceable without touching the service.

## Requirements

- Feels at home on Windows 11: light and dark following the system theme,
  per-monitor DPI.
- Runs unpackaged (plain `.exe`) and elevated.
- Self-contained: no separately installed runtime.
- Small on disk and in memory.
- Keyboard navigation and UI Automation.
- Custom drawing for charts and the fan-curve editor.
- Views can be rendered to PNG without a visible window, for the
  screenshot harness.
- Licence compatible with GPL-3.0-or-later.

## Candidates

### windows-reactor (WinUI 3)

- <https://crates.io/crates/windows-reactor>, 0.100.0 (2026-09-03), from the
  windows-rs repository. Declarative components over WinUI 3
  (`Microsoft.UI.Xaml`) with real system controls, Mica and system UIA.
- Its readme states: "Reactor is a preview; public APIs may change."
- Requires the Windows App SDK runtime (2.4.0 for 0.100.0). Self-contained
  deployment stages about 57 MB across roughly 30 DLLs and locale folders
  next to the executable; a single executable is not possible.
- The runtime version is pinned by the crate, so runtime security fixes
  arrive only with a new windows-rs release.
- Open crash issues at the time of writing (microsoft/windows-rs #5440,
  #5399); panics are fatal.
- Headless rendering covers canvas drawings only; the recording test API
  was removed (#5434).
- Elevated, unpackaged, self-contained operation is not covered by any
  upstream test or sample.

### Slint

- <https://crates.io/crates/slint>, 1.19.0 (2026-11-16). Stable 1.x API.
- Statically linked: one executable, no runtime.
- `fluent` widget style with light and dark variants following the system
  colour scheme. Text and controls are drawn by Slint, not by the system;
  there is no Mica backdrop.
- UI Automation through AccessKit.
- `Path`, `Image` from a pixel buffer and `TouchArea` cover charts and the
  curve editor.
- `Window::take_snapshot()` and the software renderer produce PNGs without
  a visible window.
- Licence: GPL-3.0-only, or a royalty-free licence with attribution, or
  commercial. A GPL application uses the GPLv3 option with no further
  obligations. The combined executable is GPLv3.

### Others, briefly

| Toolkit | Verdict |
|---|---|
| `winio` | Native Win32 backend, small community, accessibility unknown. |
| `native-windows-gui` | Unmaintained since 2022. |
| `egui` | Easy custom drawing, does not look like Windows. |
| `iced`, `xilem`, `floem`, `gpui`, `freya`, Blitz | Pre-1.0 and/or weak accessibility. |
| WebView2 (`wry`, `tauri`) | Extra processes and memory; Windows feel is CSS only. |

## Gates

| Gate | windows-reactor | Slint |
|---|---|---|
| Elevated and unpackaged | Unproven | Yes: no packaging dependency |
| Self-contained | 57 MB of staged DLLs | Single executable |
| Minimum Windows | Windows 10 1809 | Windows 10 |
| Headless PNG of whole views | No | Yes |
| API stability | Preview | 1.x |

This table is from documentation alone. The Reactor column was then tested;
see the spike below.

## Spike: windows-reactor on a real machine

The table above was filled in from documentation. The open gates were then
tested with a one-screen app (navigation pane, a list of the service's
catalog read over the pipe) built against windows-reactor 0.100.0 with
`windows-reactor-setup` staging the runtime. Windows 11 build 26300.

| Gate                                  | Result                                          |
|---------------------------------------|-------------------------------------------------|
| Runs elevated                         | Yes                                             |
| Runs unpackaged                       | Yes, a plain `.exe` started from a terminal     |
| Self-contained, nothing installed     | Yes: runtime files staged next to the `.exe`    |
| Talks to the service                  | Yes: catalog listed through `proto::Client`     |
| Follows the system theme              | Yes, without any code                           |
| Whole-window capture for review       | Yes, with `PrintWindow` (`scripts/capture-window.ps1`) |

| Measure                               | Value                                           |
|---------------------------------------|-------------------------------------------------|
| `syspilot.exe`, release               | 2.0 MB                                          |
| Staged runtime                        | 32 files, 53.5 MB, plus 3.4 MB of locale data   |
| Private memory, one page shown        | 53 MB (debug build)                             |
| First build                           | Downloads the Windows App SDK runtime 2.4.0 and WebView2 projection packages from NuGet into `%LOCALAPPDATA%\windows-reactor-setup` |

Two things differ from the documentation review:

- The published 0.100.0 API is not the one in the repository's `master`
  samples. Children of multi-slot controls are given through typed slots
  (`NavigationViewSlot::MenuItems`). The samples at the release commit match.
- Headless rendering is still canvas-only, but capturing the composed window
  works and shows exactly what a user sees, which is what the review needs.

## Decision

**windows-reactor**, self-contained.

Real WinUI 3 controls give the app the look, behaviour, text rendering,
keyboard handling and UI Automation tree of Windows itself, which no drawn
toolkit matches, and all gates pass in practice. The costs are accepted:

- about 57 MB of runtime files shipped next to a 2 MB executable. The
  service is unaffected: it does not load the runtime and keeps its own
  budget;
- a preview API. The app holds no logic (it is a view over `proto`), so a
  breaking release costs view code only. The version is pinned;
- screenshots need an interactive desktop. The review loop runs on a
  developer machine, not in CI.

Slint remains the fallback if Reactor stalls. It was the earlier choice on
paper, mainly for its single-file deployment and headless rendering.

Measured size, startup time and memory of the app are recorded in
[`docs/architecture/overhead.md`](../architecture/overhead.md).

## GPU telemetry without vendor SDKs

Checked alongside the toolkit question because it shapes the monitoring
views.

- `D3DKMTQueryAdapterInfo` with `KMTQAITYPE_ADAPTERPERFDATA` (62) returns
  `D3DKMT_ADAPTER_PERFDATA`: `Temperature` in tenths of a degree Celsius,
  `FanRPM`, `Power` in tenths of a percent (not watts),
  `MemoryFrequency` in hertz, and `PowerStateOverride`. Adapters without
  sensors report zero. Source:
  <https://learn.microsoft.com/windows-hardware/drivers/ddi/d3dkmthk/ns-d3dkmthk-_d3dkmt_adapter_perfdata>.
- `KMTQAITYPE_ADAPTERPERFDATA_CAPS` (63) returns the limits
  (`TemperatureMax`, `TemperatureWarning`, `MaxFanRPM`).
- Utilisation: the PDH counter `\GPU Engine(*)\Utilization Percentage`.
  Task Manager's headline number is the busiest engine: sum instances per
  engine across processes, then take the maximum
  (<https://devblogs.microsoft.com/directx/gpus-in-the-task-manager/>).
  `D3DKMTQueryStatistics` is "reserved for system use" and not used.
- `windows-sys` 0.61 exposes the D3DKMT calls under the
  `Wdk_Graphics_Direct3D` feature.
