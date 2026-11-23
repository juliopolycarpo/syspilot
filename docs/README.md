# SysPilot documentation

| Document                                              | Read it to learn                                              |
| ----------------------------------------------------- | ------------------------------------------------------------- |
| [Architecture overview](architecture/overview.md)     | The processes, the crates and how a request flows.            |
| [Enforcement](architecture/enforcement.md)            | How settings are applied, watched, restored and reverted.     |
| [Overhead budget](architecture/overhead.md)           | What the resident service may cost and how that is measured.  |
| [Protocol](protocol.md)                               | The pipe framing, the messages and the sensor section.        |
| [Security model](security-model.md)                   | Who may do what, and what protects that boundary.             |
| [Tweak catalog](tweak-catalog.md)                     | How to write and source a catalog entry.                      |
| [Decisions](decisions.md)                             | Choices that were not obvious, with their reasons.            |
| [Research: PawnIO](research/pawnio.md)                | The driver interface SysPilot talks to.                       |
| [Research: UI toolkits](research/ui-toolkits.md)      | Why the app is built with windows-reactor (WinUI 3).          |

Code-level documentation lives in the crates as rustdoc:
`cargo doc --workspace --no-deps --open`.
