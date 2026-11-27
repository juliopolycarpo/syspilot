# Security model

SysPilot runs a LocalSystem service that changes system settings on request and
can run commands. That is exactly the shape of a local privilege escalation if
any part of the boundary is soft. This page states the boundary and what holds
it.

## Who may do what

| Caller                                   | May                                                        |
| ---------------------------------------- | ---------------------------------------------------------- |
| Any local signed-in user                 | Read status, catalog, entry states, sensors, system info.  |
| An elevated administrator                | Everything: change settings, read and replace the profile, look for and install an update. |
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

### The sensor worker

- The service starts `syspilot-sensors.exe` by its full path next to its own
  executable. Nothing is searched for, so the worker is as hard to replace as
  the service.
- The worker takes no arguments and no instructions beyond "one more sample".
  It is given nothing but its standard streams, so a client cannot reach it,
  and what it sends back is decoded with the same bounded decoder as the pipe.
- A worker that stops answering is dropped after 750 ms and terminated. It
  cannot stall enforcement.

### Updates

A service that installs new versions of itself is a way to run code as
LocalSystem on every machine that has it. What a release must get past:

- **The service does not download or verify anything.** It starts
  `syspilot-update.exe` by its full path next to its own executable, for one
  job, and reads one line of a fixed vocabulary from it. A version text that
  is not digits and dots is discarded. The updater is given no instructions
  beyond "look" or "install into this directory".
- **Only an elevated administrator can ask** for a look, an installation or a
  change of the update mode. The daily look needs nobody.
- **The update companion runs elevated, so it runs from where only
  administrators write.** The app copies `syspilot-companion.exe` into
  `companion\` in the data directory and starts it with its own token, which
  the companion passes on to the app it opens afterwards. The app creates that
  folder only inside a data directory that already exists and is owned by
  SYSTEM or Administrators, as the service leaves it; `%ProgramData%` comes
  from the user's environment, so a folder the user made, there or anywhere
  they point it, is refused. It never creates the data directory itself,
  which would take the open permissions of `%ProgramData%`. The companion
  sends no request: it reads the handshake and the status, with the same
  check of the pipe's owner as the app. Its window loads only Windows' own
  libraries (Direct2D, DirectWrite, DWM, UI Automation), never the
  `DWriteCore.dll` that ships beside the app, and the folder it runs from,
  which Windows searches first for them, is that administrators-only one.
- **The download lands where only LocalSystem and Administrators can write**:
  `updates\` in the data directory, with the same protected ACL as the
  profile. Between verification and installation nobody else can replace the
  file.
- **A signature from a pinned key, over the whole package.** The keys are in
  `installer/update-signers` and compiled into the updater. The signature is
  an OpenSSH file signature; the updater accepts only Ed25519, SHA-512 and the
  namespace `syspilot-update`, so a signature the same key made for anything
  else (a commit, a login) is not a release. Nothing in the package is read
  or run before this check passes, and a package that fails it is deleted.
- **The network is not trusted.** WinHTTP validates GitHub's certificate, but
  the tag, the redirect and every byte are treated as hints. Downloads are
  capped in size. A release without a signature is not offered at all.
- **No going back.** After the signature, the package must declare the
  version its tag named, which must be newer than the running one; Windows
  Installer refuses a downgrade as well. A signed old release cannot be
  replayed as a new one.
- **The signing key is not on GitHub.** CI builds a draft release without it;
  the maintainer signs with a key held in an SSH agent that asks before each
  use. Taking over the repository or a workflow does not yield a release that
  installed copies accept.
- **Windows Installer does the installing**, started by its full path in the
  system directory, with no window. A failed installation is rolled back.

What this does not defend against: the release key itself being stolen, and
a maintainer signing a bad build. The first is why a second, offline key
should be listed before it is needed; the second is what review is for.

### Tuning

Tuning lets the service change how a device runs, so what can reach it
matters:

- **Only an elevated administrator can change a setting or ask for it to be
  applied again.** Anyone may read the status, which shows the values.
- **Values go through the vendor's driver, never to the hardware.** The
  tuner calls the interface the vendor publishes for its own tuning tool. The
  driver states the range of each value; the tuner checks every value of a
  setting against it before the first write. The service also checks a
  setting against the ranges of its last look before it stores the setting;
  before the first look, only the tuner's check applies.
- **The service loads no vendor library.** It starts `syspilot-tune.exe` by
  its full path next to its own executable, with no arguments, hands it one
  job on its standard input and reads one report, decoded with the same
  bounded decoder as the pipe. The vendor's library is loaded by name from
  the system directory only.
- **A tuner that hangs is terminated** after 15 seconds. It cannot stall
  enforcement.
- **Values that stop Windows are not set again unasked.** A mark is written
  to `state\` before values are set and removed once the machine has run
  with them for a minute, or the service stops normally. A service that
  finds the mark at start sets nothing until an administrator changes the
  setting or asks for it again.

What this does not defend against: a value inside the driver's range that
this device cannot hold, set by an administrator who was asked first. The
values do not survive a restart of the machine by themselves, and the mark is
what keeps SysPilot from putting them back.

### Removal

Removing SysPilot runs `syspilot-svc uninstall` as LocalSystem. Unless
settings are kept, it puts every tuned device back to its driver's defaults.
It deletes the data directory and, for each user profile, the app's
preferences file. In a user's profile it follows no link and deletes that one file by name (and
its directory only if that leaves it empty), so a junction planted there
cannot point the deletion at anything else.

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

- SysPilot talks to PawnIO only through its documented device interface.
- The modules it loads are the PawnIO project's own. The build fetches them
  pinned by SHA-256, a release puts them in `pawnio-modules` next to the
  executables (under `Program Files`, where standard users cannot write), and
  the driver itself refuses any module not signed with the PawnIO key.
- Only the sensor worker opens the driver. The service does not, and the app
  cannot.
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
