# Security model

SysPilot runs a LocalSystem service that changes system settings on request and
can run commands. That is exactly the shape of a local privilege escalation if
any part of the boundary is soft. This page states the boundary and what holds
it.

## Who may do what

| Caller                                   | May                                                        |
| ---------------------------------------- | ---------------------------------------------------------- |
| Any local signed-in user                 | Read status, catalog, entry states, sensors, system info.  |
| An elevated administrator                | Everything: change settings, read and replace the profile. |
| Remote callers                           | Nothing. The pipe rejects remote clients.                  |

**The rule: no request from a non-elevated caller may cause the service to
write to the system or reveal the profile.** The app's Simple and Advanced
levels are a presentation choice and are not part of this boundary.

"Elevated administrator" is tested with `CheckTokenMembership` against the
Administrators group on the caller's impersonated token. For an administrator
who has not elevated, that group is deny-only and the test is false. The check
happens in the service, once per connection, after the first message (Windows
does not allow impersonating a pipe client that has sent nothing).

## What protects the boundary

### The pipe

- The first instance is created with `FILE_FLAG_FIRST_PIPE_INSTANCE`. If the
  name is taken, the service refuses to start rather than share it, so another
  process cannot pre-create the pipe and sit between app and service.
- `PIPE_REJECT_REMOTE_CLIENTS`.
- The ACL gives authenticated users read and write data only, without
  `FILE_CREATE_PIPE_INSTANCE`: they can connect but cannot add instances.
- Clients connect with `SECURITY_IDENTIFICATION`, so the service can identify
  them but cannot act as them. The service reverts to itself immediately after
  the check and distrusts the result if reverting fails.
- Clients verify that the pipe is owned by LocalSystem or Administrators before
  sending anything. An elevated app will not take instructions from a pipe a
  standard user created.

### The data directory

The profile can contain commands that the service runs as LocalSystem. Whoever
can write the profile is LocalSystem.

- `%ProgramData%` lets every user create subdirectories. The service therefore
  never trusts a data directory because it exists. It creates the directory
  with an explicit owner (Administrators) and a protected ACL (LocalSystem and
  Administrators only), and if the directory already exists with any other
  owner it refuses to start.
- The owner is set explicitly because a directory created by an elevated
  administrator is otherwise owned by that user's own SID, the same SID the
  user has without elevation, and the check could not tell the two apart.
- The `logs` subdirectory additionally grants users read access. Sensor logs
  are not secrets, and nothing is ever read back from that directory.

### The executable

- `install` registers the product service only from under `Program Files`,
  where standard users cannot replace the file.
- The service command line is quoted.
- The catalog is compiled in. There is no catalog file to tamper with.

### Commands

- A command's program must be an absolute path or start with a `%Variable%\`
  root; nothing is resolved through `PATH`.
- Commands are started without a shell, with arguments passed as an array and
  quoted by the launcher, inside a job object that kills the process tree on
  timeout.
- Catalog entries do not use commands. They exist for custom actions, which
  only an elevated administrator can define.

### Input

- Frames are limited to 1 MiB and decoded with a bounds-checked decoder.
- Every action, profile and custom entry is validated before it is stored or
  applied, and a rejected profile is rejected whole.
- A stored profile that fails validation at start is set aside and reported;
  the service starts with nothing enabled and reverts what was applied.

### The driver

- SysPilot talks to PawnIO only through its documented device interface and
  loads only the modules it pins by SHA-256.
- Version 1 performs no hardware writes: no fan duty, no MSR, no SMBus device
  registers. Fan control is compiled out unless the `fan-control` feature is
  enabled, and that feature is off in release builds.

## What SysPilot does not defend against

- An administrator. Anyone who can elevate can already do everything SysPilot
  can.
- Malware already running elevated or as LocalSystem.
- Another program that keeps rewriting a setting. SysPilot reports the conflict
  (*contested*) instead of escalating it.
- Settings Windows protects. They are reported as not enforceable.

## Reporting a vulnerability

See [SECURITY.md](../SECURITY.md).
