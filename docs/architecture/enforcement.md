# Enforcement

The engine (`crates/engine`) makes the system match a set of enabled entries and
keeps it that way. This page describes its rules. The implementation is
`enforcer.rs` (the state machine) and `ops.rs` (what apply, detect and revert
mean for each kind of action).

## Vocabulary

- **Action**: one thing to keep true. Four kinds exist: a registry value, a
  service start type, a power setting, and a command.
- **Entry**: a catalog tweak or a custom action from the profile. It has one or
  more actions and is either enabled or not.
- **Unit**: one action for one target. A machine-wide action has one unit. A
  per-user action has one unit per signed-in user.
- **Snapshot**: what was there before the unit was first applied.

## Applying

When an entry is enabled, for each unit the engine:

1. Detects the current state. If it already matches, nothing is written, but
   the unit is still tracked so that later drift is restored.
2. Captures a snapshot, including "the value did not exist" and which registry
   keys had to be created.
3. Saves the snapshot to disk. Only then does it write.

The order matters: if the service dies between steps, the worst case is a saved
snapshot for a value that was never changed, and reverting that is harmless.

## Reverting

Disabling an entry restores each unit's snapshot: the old value and type, or
deletion if there was none, and removal of keys that were created for it and
are empty again. Commands run their `undo` command if they have one; a command
without one is not reversible, and the catalog rules keep such entries out of
the Simple level.

Per-user units of a user who is not signed in cannot be restored now, because
their registry hive is not loaded. They are marked pending and restored when
that user next signs in.

## Triggers

Each action is re-evaluated on its triggers:

| Trigger    | Fires                                                       |
| ---------- | ----------------------------------------------------------- |
| `boot`     | When the service starts.                                    |
| `logon`    | When a user signs in (per-user actions for that user).      |
| `resume`   | When the machine wakes up.                                  |
| `change`   | When the watched location changes.                          |
| `interval` | Every N seconds.                                            |

An action without explicit triggers gets the defaults for its kind:

| Action                                   | Default triggers    |
| ---------------------------------------- | ------------------- |
| Machine-wide registry, service, power    | `boot`, `change`    |
| Per-user registry                        | `logon`, `change`   |
| Command run as LocalSystem               | `boot`              |
| Command run as the user                  | `logon`             |

`resume` and `interval` are opt-in. `interval` is the only trigger that makes
the service wake on a timer.

### Watching without polling

Everything that can drift lives in the registry, so one mechanism covers all
three declarative kinds:

- a registry value: its key;
- a service start type: `SYSTEM\CurrentControlSet\Services\<name>`;
- a power setting: the power schemes subtree.

If the key does not exist yet, the nearest existing ancestor is watched and the
watch moves down once the key appears.

Windows does not notify for a write that leaves the data unchanged, so the
engine restoring a value does not wake itself up again.

A key that changes more than 20 times a second is paused for five seconds and
then treated as changed once. A noisy key costs a bounded number of wake-ups.

## Drift and contested values

When a watched value differs from what an enabled unit wants, the engine
restores it and counts a drift. That is the normal case: an update or an app
flipped a setting once.

If the same unit is restored three times within a minute, something is
actively writing it. Restoring again would only start a loop, so the unit
becomes **contested**: the engine leaves the value alone, reports the state,
and retries once after a back-off that starts at one minute and doubles up to
an hour. After a successful retry the unit is on probation for a minute; drift
during probation goes straight back to contested with a longer back-off.

## States

| State             | Meaning                                                                  |
| ----------------- | ------------------------------------------------------------------------ |
| Compliant         | The system matches.                                                      |
| Different         | The system does not match (and the entry is off, or about to be applied).|
| Contested         | Something keeps changing it; retry pending.                              |
| Not enforceable   | Windows refused the write. Nothing is forced.                            |
| Not applicable    | This Windows build does not have the setting.                            |
| Unknown           | Cannot be checked (a command without a check, or no user signed in).     |
| Failed            | An unexpected error; the message says which.                             |

An entry's state is the worst of its actions' states.

## What the engine will not do

- Take ownership of a registry key, change an ACL, or use backup/restore
  privileges to get past access denied. Refused means *not enforceable*.
- Write anything for an entry that is not enabled.
- Delete a key it did not create.
- Keep retrying at full speed. Every retry path has a back-off.

## Audit entries

A catalog entry marked `audit` is only ever detected, never applied. It reports
whether the system is in the documented good state (Secure Boot on, UAC on) and
cannot be enabled.
