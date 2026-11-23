# UI glossary

One word per concept, everywhere: in the app, the command line's output, the
README and release notes. A new piece of text uses these words or adds a row
here first.

Text is in sentence case. Sentences say what the user will notice, not what
the registry does; the exact values are detail for the Advanced level.

## Things

| Term | Means | Not |
| ---- | ----- | --- |
| Tweak | A setting from the built-in catalog that SysPilot can keep in place. | Hack, optimization, fix |
| Check | A catalog entry that only reports (for example, whether Secure Boot is on). It has no switch. | Audit, test |
| Custom action | A registry value, service start type, power setting or command the user defined, kept in place like a tweak. | Rule, script |
| Level | How much the app shows: **Simple** or **Advanced**. | Mode, profile |
| Profile | The file that lists what is turned on, with the custom actions. Used on the command line and in the documentation, not in the app. | Config |
| History | The record of what SysPilot applied, restored and turned off. | Log |
| Sensor | One reading of the hardware. | Probe, metric |
| Sensor log | The CSV files of recorded readings. | History |
| Hardware access | What reading sensors through the PawnIO driver adds. | Driver features, low-level access |
| Service | The background part that applies settings. | Daemon, agent, engine |

## States

Shown as a badge next to a tweak, a check or a history row.

| Badge | Means |
| ----- | ----- |
| Applied | Turned on and in place. |
| Not applied yet | Turned on, but the setting currently has another value. |
| Already set | Turned off in SysPilot, but the setting has the tweak's value anyway. |
| Restored | Something changed the setting and SysPilot put it back. |
| Contested | Something keeps changing the setting. SysPilot stopped restoring it and tries again later. |
| Blocked by Windows | Windows does not allow the change. SysPilot does not work around it. |
| Failed | The change was attempted and did not work; the reason is shown. |
| Turned off | The user turned the tweak off and the previous value was put back. |
| Passed | A check found what it expects. |
| Needs attention | A check found something else. |
| Not available | The tweak does not exist on this version of Windows. |

Colours carry the same meaning everywhere: green for in place, amber for
something to look at, red for something that did not happen, grey for
information.

## Actions

| Verb | For | Not |
| ---- | --- | --- |
| Turn on, turn off | A tweak, a custom action, sensor recording. | Enable, disable, activate, apply |
| Apply again | Retrying a tweak that is not in place. | Reapply, retry, force |
| Add | Creating a custom action. | Create, new |
| Remove | Deleting a custom action. Its previous value is put back. | Delete |
| Clear | Emptying the history. | Delete, reset |
| Put back | What happens to a setting when its tweak is turned off or after something changed it. | Revert, roll back, undo |
| Restart as administrator | Starting the app again elevated. | Elevate, run as admin |
| Get PawnIO | Opening the driver's official page. SysPilot never installs it. | Install driver |

The command line keeps its established verbs (`enable`, `disable`, `reapply`):
they are an interface for scripts, and changing them would break their users.
