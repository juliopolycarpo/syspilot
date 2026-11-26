# SysPilot documentation

| Document                                              | Read it to learn                                              |
| ----------------------------------------------------- | ------------------------------------------------------------- |
| [Architecture overview](architecture/overview.md)     | The processes, the crates and how a request flows.            |
| [Enforcement](architecture/enforcement.md)            | How settings are applied, watched, restored and reverted.     |
| [Overhead budget](architecture/overhead.md)           | What the resident service may cost and how that is measured.  |
| [Protocol](protocol.md)                               | The pipe framing, the messages and the sensor section.        |
| [Security model](security-model.md)                   | Who may do what, and what protects that boundary.             |
| [Tweak catalog](tweak-catalog.md)                     | How to write and source a catalog entry.                      |
| [Fan curves](fan-curves.md)                           | The `[[fan]]` table of a profile, its rules and when it is followed. |
| [Tuning](tuning.md)                                   | The `[[tuning]]` table of a profile, the Tuning page, when values are set and what happens if Windows stops. |
| [UI glossary](ui-glossary.md)                         | The one word the app uses for each thing, state and action.   |
| [UI review](ui-review.md)                             | How the app's look and accessibility are checked, and the record of passes. |
| [Decisions](decisions.md)                             | Choices that were not obvious, with their reasons.            |
| [Research: PawnIO](research/pawnio.md)                | The driver interface SysPilot talks to.                       |
| [Research: UI toolkits](research/ui-toolkits.md)      | Why the app is built with windows-reactor (WinUI 3).          |
| [Research: fan control](research/fan-control.md)      | The fan registers behind the `fan-control` feature and what a human must validate first. |
| [Research: Radeon tuning](research/radeon-tuning.md)  | The driver interface behind tuning, what it offers on the development machine and what was verified. |
| [Research: per-program priority](research/perf-options.md) | What is documented, and what is not, about the Image File Execution Options `PerfOptions` values. |
| [Research: reference machine](research/reference-machine.md) | What the hardware providers found on the development machine, and what they did not. |

Code-level documentation lives in the crates as rustdoc:
`cargo doc --workspace --no-deps --open`.
