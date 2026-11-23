# Image File Execution Options: PerfOptions

Research note for a later feature that would set a per-program CPU, I/O or
memory priority. No product code uses this yet. Each fact is marked
**Microsoft** (stated on a Microsoft page, linked) or **secondary** (from a
blog, a gist or a tool's documentation, not confirmed by Microsoft).

**Summary.** Microsoft documents the Image File Execution Options (IFEO) key
and the Win32 priority enumerations. No Microsoft page found names the
`PerfOptions` subkey or its values. The numbers in the table below are
therefore secondary facts. They match documented enumerations in the I/O and
memory cases, which makes them credible but not authoritative.

## The key

| Fact | Status |
|---|---|
| IFEO lives at `HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Image File Execution Options`. | Microsoft |
| A subkey is named after the executable without its path (`notepad.exe`). When the program starts, Windows reads specific values under it. | Microsoft |
| The performance values sit in a `PerfOptions` subkey: `...\Image File Execution Options\<image>.exe\PerfOptions`. | Secondary |
| `PerfOptions` does not exist by default. | Secondary |
| All values are `REG_DWORD`. | Secondary |

Microsoft sources: an archived post by a Microsoft engineer, [Image File
Execution Options](https://learn.microsoft.com/archive/blogs/junfeng/image-file-execution-options)
(2004), and the [GFlags flag table](https://learn.microsoft.com/windows-hardware/drivers/debugger/gflags-flag-table),
which lists the global flags that can be set per image. Neither mentions
`PerfOptions`. The post lists the values that a 2004 Regmon trace of a small
program showed being read, and it says its author could not find documentation
for some of them, so it does not settle what later Windows versions read.

## Values

| Value | Accepted data | Meaning |
|---|---|---|
| `CpuPriorityClass` | 1 Idle, 2 Normal (default), 3 High, 4 Real-time, 5 Below Normal, 6 Above Normal | CPU priority class of the process. |
| `IoPriority` | 0 Very Low, 1 Low, 2 Normal (default) | I/O priority of the process's file requests. |
| `PagePriority` | 1 Very Low, 2 Low, 3 Medium, 4 Below Normal, 5 Normal (default) | Memory priority: which pages are trimmed first. |
| `WorkingSetLimitInKB` | A size in KB | A maximum working set. Reported as a hard limit; a value that is too low degrades performance. |
| `UseFilter` and `FilterFullPath` | `UseFilter` = 1, plus subkeys that hold a `FilterFullPath` string | Narrows the match from the image name to a full path. |

All of this table is **secondary**. The sources are a [gist by Helder
Magalhaes](https://gist.github.com/HelderMagalhaes/899766e74da8cd3923fd47c41c07b320)
(CC BY 4.0; it cites System Informer, Windows Internals 7th edition and
Microsoft's API pages) and a 2013 blog post by Alois Kraus, [How To Persist
CPU and IO Priority For A
Process](https://geekswithblogs.net/akraus1/archive/2013/08/14/153728.aspx).
They agree on `CpuPriorityClass` 1, 2, 3, 5 and 6 and on `IoPriority` 0 to 2.
Kraus adds that "all other values set the priority back to normal". Only the
gist lists `PagePriority`, `WorkingSetLimitInKB` and the path filter. I did not
test any value on a live system.

### How the numbers relate to documented enumerations

The meanings below come from Microsoft pages, but the link between those
enumerations and the registry values is an inference from matching numbers,
not a Microsoft statement.

- **`IoPriority` 0 to 2** matches the first three members of the kernel's
  [`IO_PRIORITY_HINT`](https://learn.microsoft.com/windows-hardware/drivers/ddi/wdm/ne-wdm-_io_priority_hint):
  `IoPriorityVeryLow` (0, "the system uses this value for background I/O"),
  `IoPriorityLow` (1) and `IoPriorityNormal` (2, "the default setting").
  Microsoft says `IoPriorityHigh` (3) and `IoPriorityCritical` (4) are
  "reserved for use by the system". The user-mode
  [`PRIORITY_HINT`](https://learn.microsoft.com/windows/win32/api/winbase/ne-winbase-priority_hint)
  stops at Normal (very low, low, normal).
- **`PagePriority` 1 to 5** matches
  [`MEMORY_PRIORITY_INFORMATION`](https://learn.microsoft.com/windows/win32/api/processthreadsapi/ns-processthreadsapi-memory_priority_information):
  `MEMORY_PRIORITY_VERY_LOW` (1) to `MEMORY_PRIORITY_NORMAL` (5, "the default
  priority for all threads and processes"). Microsoft says the priority is a
  hint to the memory manager when it trims working-set pages: other factors
  equal, lower-priority pages go first.
- **`CpuPriorityClass` 1 to 6** does not match the Win32 constants of
  [`SetPriorityClass`](https://learn.microsoft.com/windows/win32/api/processthreadsapi/nf-processthreadsapi-setpriorityclass)
  (`IDLE_PRIORITY_CLASS` is `0x40`, `NORMAL_PRIORITY_CLASS` is `0x20`, and so
  on). The numbers 1 to 6 follow the order Idle, Normal, High, Real-time,
  Below Normal, Above Normal. I recall that order from the native
  `PROCESS_PRIORITY_CLASS` enumeration in Windows headers, but I did not find a
  Microsoft page for it, so treat it as unverified. The class names themselves
  are documented on the `SetPriorityClass` page.

## When the values take effect

- Windows reads IFEO values when the process starts (**Microsoft**, for IFEO in
  general). A running process is not changed, so a value applies to the next
  start of the program. This is the expected consequence for `PerfOptions`,
  and I found no source that tests it.
- Matching is by image name, case-insensitive (**secondary**). A rule for
  `chrome.exe` applies to every `chrome.exe`, whichever program started it,
  unless the path filter above is used.
- Child processes (**secondary**, from the gist): `CpuPriorityClass` is
  inherited only when it is Idle or Below Normal; `IoPriority` and
  `PagePriority` are inherited. This agrees with the `SetPriorityClass` page,
  which says that "the idle-priority class is inherited by child processes".
  Microsoft says nothing about inheritance for the other classes.

## Caveats

- **Real-time is unsafe.** Microsoft warns that a real-time process "can cause
  disk caches not to flush or cause the mouse to be unresponsive" and that
  High needs "extreme care" (`SetPriorityClass`). One forum report says
  `CpuPriorityClass` 4 is not honored; the gist lists it. The reports
  disagree, so a feature should not offer it.
- **I/O above Normal is not available.** The gist reports that 3 (High) cannot
  be set through IFEO, and Microsoft reserves High and Critical for the
  system. A feature should offer 0 to 2 only.
- **CPU priority does not cover background I/O.** Microsoft notes on the
  `SetPriorityClass` page that "even an idle CPU priority process can easily
  interfere with system responsiveness when it uses the disk and memory".
  Lowering the three values together is what the feature would be for.
- **Reports of it not working.** Forum posts describe the values being ignored
  for some programs. I have no test that explains why, so the effect has to be
  verified per program, for example by reading the priority back from a
  started process.
- **IFEO is a security-sensitive key.** The same key holds the `Debugger`
  value, which launches another program in place of an image, and attackers
  use it for persistence. Security tools watch the key. A write by SysPilot
  would look like that activity, and a mistake in the subkey name would not be
  harmless. Writes need the same care as other machine-wide settings, and only
  `PerfOptions` under an image should be touched.
- **Reversal.** The key does not exist by default, so a feature can put the
  state back by deleting its own values and then the subkey if it created it.
  A `RegData::Absent` action covers this.
- **Mismatch with the catalog rules.** `docs/tweak-catalog.md` requires a
  Microsoft page that documents the exact key and value. `PerfOptions` has
  none, so it cannot be a built-in catalog entry. It could be a custom action
  the user adds, or wait for a Microsoft page.

## Not found

Searches did not turn up a Windows Internals excerpt that I could check, a
Sysinternals page, or a Microsoft blog post on `PerfOptions`. The gist cites
Windows Internals 7th edition, part 1, for how the kernel sets up a new
process, and Geoff Chappell's notes on `LdrOpenImageFileOptionsKey`. I did not
read either, so neither is cited here as a source for a value.
