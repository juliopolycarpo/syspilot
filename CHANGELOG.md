# Changelog

Every change to SysPilot, by release. This file is generated from the commit
messages by [git-cliff](https://git-cliff.org); do not edit it by hand.

## [0.3.0](https://github.com/juliopolycarpo/syspilot/compare/v0.2.2...v0.3.0) - 2026-11-26

### Features

- **proto:** Describe device tuning ([#21](https://github.com/juliopolycarpo/syspilot/pull/21))
- **hw:** Set Radeon tuning values through the driver ([#22](https://github.com/juliopolycarpo/syspilot/pull/22))
- **tune:** Add the tuner executable ([#23](https://github.com/juliopolycarpo/syspilot/pull/23))
- **svc:** Apply tuning settings and report them ([#24](https://github.com/juliopolycarpo/syspilot/pull/24))
- **ui:** Add a Tuning page ([#25](https://github.com/juliopolycarpo/syspilot/pull/25))

### Fixes

- **xtask:** Give each scratch directory a name of its own ([#16](https://github.com/juliopolycarpo/syspilot/pull/16))
- **package:** Reinstall WiX over a broken tool directory ([#29](https://github.com/juliopolycarpo/syspilot/pull/29))
- **hw:** Report a failed adapter query instead of no Radeon card ([#32](https://github.com/juliopolycarpo/syspilot/pull/32))

### Performance

- **svc:** Look at tuned devices again only for the Tuning page ([#31](https://github.com/juliopolycarpo/syspilot/pull/31))

### Documentation

- Describe tuning ([#26](https://github.com/juliopolycarpo/syspilot/pull/26))
- Say that only the Tuning page looks at the devices again ([#33](https://github.com/juliopolycarpo/syspilot/pull/33))
- Research a GeForce tuning backend ([#34](https://github.com/juliopolycarpo/syspilot/pull/34))
- Research processor tuning through the power plan ([#35](https://github.com/juliopolycarpo/syspilot/pull/35))
- Settle where the active power plan lives ([#36](https://github.com/juliopolycarpo/syspilot/pull/36))
- Say from which driver each NVML setter exists ([#37](https://github.com/juliopolycarpo/syspilot/pull/37))

### Build and CI

- Optimise proto and engine for size ([#14](https://github.com/juliopolycarpo/syspilot/pull/14))

## [0.2.2](https://github.com/juliopolycarpo/syspilot/compare/v0.2.1...v0.2.2) - 2026-11-24

### Fixes

- **ui:** Keep a tweak's details open once opened
- **ui:** Let the menu button open and close the navigation pane

## [0.2.1](https://github.com/juliopolycarpo/syspilot/compare/v0.2.0...v0.2.1) - 2026-11-24

### Fixes

- **update:** Remove the package once it is installed

### Build and CI

- Install and remove the package on every push

## [0.2.0](https://github.com/juliopolycarpo/syspilot/compare/v0.1.1...v0.2.0) - 2026-11-24

### Features

- Look for new releases and install them, by choice

### Build and CI

- **installer:** Ship a Windows Installer package
- **release:** Publish only the product package

## [0.1.1](https://github.com/juliopolycarpo/syspilot/compare/v0.1.0...v0.1.1) - 2026-11-24

### Features

- **catalog:** Add crash and storage health tweaks to reliability
- **catalog:** Add Start, search and lock screen tweaks to look and feel

### Fixes

- **ui:** Dim the hide button of a reading
- **ui:** Name all three colour choices in Settings
- **ui:** Say "Start the service" when the service is not running
- **fan:** Recover every channel the original store records
- **fan:** Restore from the panic hook without the controller lock
- **fan:** Keep one process-wide panic hook over a swappable slot
- **catalog:** Mark update-no-auto-restart as weakening
- **ui:** Keep a trend line's point count steady between buckets
- **package:** Keep cargo's lock files out of the release zip

### Performance

- **ui:** Slide charts instead of redrawing every segment
- **ui:** Draw trend lines from two-second averages
- **ui:** Draw trend lines from a fixed pool of sliding segments

### Reverted

- perf(ui): slide charts instead of redrawing every segment

### Documentation

- **readme:** Point at the 0.1.0 pre-release
- **readme:** Refresh the screenshots and show the Fans page
- **ui-review:** Record the contrast theme seen on screen
- Explain how to verify an unsigned release

## [0.1.0](https://github.com/juliopolycarpo/syspilot/releases/tag/v0.1.0) - 2026-11-23

### Features

- **proto:** Define the IPC contract
- **hw:** Add the sensor provider contract and the sampling hub
- **engine:** Add the enforcement state machine
- **engine:** Add the Win32 backends
- **proto:** Restrict the profile and history to administrators
- **engine:** Report the time to the next deadline and pending reverts
- **proto:** Let a security descriptor move between threads
- **svc:** Add the service, its installer and the command line
- **catalog:** Add the built-in tweak catalog
- **svc:** Embed the built-in tweak catalog
- **ui:** Start the app on windows-reactor
- **ui:** Build the app on the service's data
- **hw:** Add the CPU and memory providers
- **hw:** Add the GPU provider
- **hw:** Add the storage and volume providers
- **hw:** Add the network provider
- **hw:** Add the battery provider
- **hw:** Add the ACPI thermal zone provider
- **hw:** Parse the SMBIOS table
- **hw:** Split the firmware table reply from its header
- **hw:** Collect the system information
- **hw:** Print the system information in the sensors example
- **pawnio:** Wrap the PawnIO device interface
- **xtask:** Fetch and verify the pinned PawnIO modules
- **hw:** Add the read-only bus layer over PawnIO
- **pawnio:** Name STATUS_PRIVILEGED_INSTRUCTION
- **hw:** Add the Intel CPU temperature and power provider
- **hw:** Support chip detection in the Super I/O bus layer
- **hw:** Add the Super I/O hardware monitor providers
- **hw:** Add the DIMM temperature provider
- **hw:** Restrict SMN reads to the thermal block
- **hw:** Add the AMD Zen temperature and power provider
- **hw:** Let a provider mark single sensors as needing the driver
- **proto:** Define the sensor worker protocol
- **svc:** Read sensors through a worker process
- **ui:** Fold per-thread and per-core readings away
- **hw:** Keep the SPD page select behind the spd-paging feature
- **hw:** Let a provider say why its last sample had no values
- **hw:** Decide in one place what the driver gives this machine
- **hw:** Add the driver_sensors example
- **sensors:** Read what the PawnIO driver gives this machine
- **ui:** Give the app an icon
- **ui:** Choose the colours on the command line
- **hw:** Add the fan-curve engine
- **hw:** Add the fan-control board catalog and loader
- **hw:** Add the fan-control feature and a narrow Super I/O fan write
- **hw:** Add the fail-safe fan controller behind fan-control
- **hw:** Add the NCT677x fan channel behind fan-control
- **catalog:** Add five performance tweaks with Microsoft sources
- **hw:** Read the chipset temperature through IntelPCHThermal
- **hw:** Probe the chipset temperature as its own feature
- **proto:** Describe fan control in the status and the worker protocol
- **hw:** Say what the fans of this machine allow and why
- **hw:** Keep the saved BIOS registers in a file that survives a crash
- **hw:** Run a profile's fan curves on a validated board
- **hw:** Hand the Super I/O chip the probe found to its caller
- **sensors:** Follow the profile's fan curves in the worker
- **svc:** Show the fans in the status command
- **svc:** Hand the profile's fan curves to the sensor worker
- **ui:** Add a Fans page at the Advanced level
- **ui:** Add demo scenarios for fan control
- **ui:** Let the user hide a sensor

### Fixes

- **catalog:** Cite the page that states the value for consumer features
- **hw:** Keep the total CPU load when one processor steps back a tick
- **hw:** Read the GPU perf data again when it comes back empty
- **hw:** Label the SMBIOS processor max speed as the socket's limit
- **hw:** Use the fixed ioctl_smbus_xfer signature
- **ui:** List the sensors before asking for the status
- **svc:** Print byte readings with a binary prefix
- **svc:** Replace a sensor worker that failed
- **svc:** Keep enforcing while the sensor worker probes
- **ui:** Name the details expander of each tweak
- **ui:** Make the custom action hints one valid example
- **ui:** Treat raw board inputs as detail and do not colour them
- **hw:** Read a SATA drive's temperature from SMART attribute 194
- **hw:** Do not offer an Intel package power meter that reads far below the TDP
- **ui:** Shorten the note about readings left to the Advanced level
- **ui:** Defer to the system colours under a contrast theme
- **ui:** Drop the limits line of a fan without a curve
- **scripts:** Give a test instance its shortcut and Installed apps entry

### Documentation

- **research:** Record the PawnIO interface and the UI toolkit choice
- Add the tweak catalog authoring guide
- Describe the product, its architecture and its security model
- **architecture:** Rejoin the measurements table
- **research:** Correct the SPD5118 format and mark what is replay-only
- **research:** Record what the providers found on the reference machine
- **readme:** Describe the app, the installer and the three programs
- Describe the sensor worker's protocol and trust, and what a build downloads
- **ui:** Add the glossary and the review procedure
- **research:** Record the fan-control registers and the validation gate
- **readme:** Say where fan control stands
- **research:** Document what is known about IFEO PerfOptions
- **research:** Drop an unsupported claim about the Regmon trace
- **research:** Record the IntelPCHThermal module and what this PC reads
- **glossary:** Add the fan terms
- **fans:** Document the fan curves of a profile
- Record how fan control is wired and why it runs in the worker
- **readme:** Mention the chipset temperature

### Refactoring

- **hw:** Share the little-endian buffer readers
- **hw:** Share the sample backoff between bus providers

### Tests

- **catalog:** Validate the built-in catalog against the editorial rules
- **svc:** Accept a test run without an interactive session
- **hw:** Check the registry QWORD read and the adapter memory query on real data
- **hw:** Replay the recorded NCT6779D and Xeon registers
- **hw:** Sample every provider of the driver probe for ten seconds
- **ui:** Give the demo board raw inputs

### Build and CI

- Check formatting, lints, tests, the release build and its size
- Keep the sensor worker with the service artifact
- Package, install and uninstall with PowerShell scripts
- Build a draft release from a version tag
- Sign the package's own files when given a certificate
- Move upload-artifact to v7
- **xtask:** Fetch the IntelPCHThermal module with the others
- Lint and test the fan-control feature tier

### Chores

- Bootstrap the cargo workspace
- **scripts:** Drive the app through UI Automation
- **scripts:** Measure the idle service against its budget

