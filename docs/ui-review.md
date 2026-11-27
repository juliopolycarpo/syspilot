# UI review

How the app's look and behaviour are checked before a release. The review is
done on screenshots of demo mode, so it needs no service and changes nothing
on the machine.

## Capturing

```powershell
cargo build -p syspilot-ui
powershell -Command "./scripts/capture-pages.ps1"
powershell -Command "./scripts/capture-pages.ps1 -Scenario no-driver -Pages home,sensors,settings"
powershell -Command "./scripts/capture-pages.ps1 -Scenario standard-user -Pages home,privacy,custom-actions,settings"
powershell -Command "./scripts/capture-pages.ps1 -Scenario stopped -Pages home -Levels simple"
```

This writes one PNG per page, level, theme and scenario to `target/ui-shots`
(68 for the commands above). It needs an interactive desktop. Flows that need
a click are driven by accessible name:

```powershell
./scripts/drive-app.ps1 -ProcessId <pid> -Name 'Turn off Spectre variant 2 and Meltdown mitigations' -Action Toggle
./scripts/capture-window.ps1 -ProcessId <pid> -Path target/ui-shots/confirm.png
```

Both scripts find the window through `scripts/window-finder.ps1`, which picks
the visible `SysPilot - <page>` window instead of `Process.MainWindowHandle`;
an elevated process also owns a 50x50 overlay that the latter can return.
`./scripts/test-window-finder.ps1` checks that lookup and needs a desktop.

## Checklist

For every screenshot:

1. **Spacing.** Gaps are the tokens in `crates/ui/src/widgets.rs` (4, 8, 12,
   24). Cards in a list share one left and one right edge.
2. **Type.** Five sizes only: caption 12, body 14, subtitle 20, title 28,
   figure 32. One title per page.
3. **Tokens.** No colour or size is written in a page; pages use `widgets`.
4. **States.** The page makes sense when it is loading, empty, populated,
   failed, without the driver, without administrator rights and with the
   service stopped.
5. **Overflow.** Long tweak titles and registry paths wrap; nothing is cut
   off or runs under a switch.
6. **Contrast.** Text and badges are readable in both themes; a state is
   never told by colour alone (badges have words, dots have a caption).
7. **Wording.** Terms and verbs are the ones in the
   [glossary](ui-glossary.md); sentence case; no jargon at the Simple level.
8. **Levels.** Simple and Advanced look like one product: the same row,
   with more detail in it.
9. **Weakening settings.** They exist only at the Advanced level and ask for
   confirmation, stating the risk.

Accessibility, per page at the Advanced level:

```powershell
./scripts/drive-app.ps1 -ProcessId <pid> -Unnamed
```

must print nothing: every control the keyboard can reach has an accessible
name. Tab order follows reading order, which WinUI gives a layout that is
built top to bottom.

## What is not covered

- **Display scaling.** Captures are taken at the desktop's scale. 150 % and
  200 % need the system setting changed, which the capture does not do.
  WinUI lays out in device-independent units, so problems here would be
  layout overflow, which the 1196-pixel-wide captures already stress.
- **High contrast.** The capture scripts do not turn a contrast theme on,
  because it is a system setting. Under one, the app asks Windows
  (`SPI_GETHIGHCONTRAST`) and drops its own colours: state text keeps the
  system text colour, badge fills give way to an outline, and the dots and
  sparklines are drawn with the text brush, which WinUI maps to the user's
  contrast colours. Unit tests pin that palette, and it was seen once on
  screen (see the record). The check runs at start and on each theme
  change; if Windows reports no theme change, the app picks up a contrast
  theme on its next start. To look again, turn on a contrast theme in
  Settings > Accessibility > Contrast themes and open the app.
- **Screen readers.** Names are checked through UI Automation; the app has
  not been listened to with Narrator.

## Record

| Date | Captures | Findings | Fixed |
| ---- | -------- | -------- | ----- |
| 2026-11-23 | Every page, both levels, dark | Pages shrank to their content's width; filter box stretched | Yes |
| 2026-11-23 | Sensors on a 28-thread processor | 28 per-thread rows buried everything else | Yes: folded per device |
| 2026-11-23 | History | A day count read "1 d 0 h" | Yes |
| 2026-11-23 | 68: both themes, both levels, four scenarios | Custom action form hints described a per-user text value while the form starts on a machine-wide number | Yes |
| 2026-11-23 | Accessibility, seven pages | The expander of every tweak row (22 on Privacy) had no accessible name | Yes |
| 2026-11-23 | Tweak pages again after the fix | None | |
| 2026-11-23 | Code review for contrast themes; light captures after the change | Fixed state colours overrode a contrast theme's colours | Yes: the palette defers to the system |
| 2026-11-23 | Night sky contrast theme, installed product, Sensors and Security at the Advanced level | None: system colours throughout, badges outlined, sparklines in the text colour | |
| 2026-11-25 | Tuning: dark and light, the held-back, standard-user and nothing-to-tune scenarios, and a real Radeon RX 550 through a test instance | State tables left an empty line under every value and centred the boxes in their column; a table with no voltages showed a column of dashes; a device that is not found repeated its key as a caption | Yes |
| 2026-11-25 | Tuning driven through UI Automation on the real card: enter a power limit, Save, turn tuning off | None: every box and the switch are named, the entry showed as not saved, the card took the value and went back to its default | |
