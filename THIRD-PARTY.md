# Third-party components

## Linked into the service and the sensor worker

| Crate                                         | Licence             | Used for                          |
| --------------------------------------------- | ------------------- | --------------------------------- |
| [`windows-sys`](https://crates.io/crates/windows-sys), `windows-link` | MIT OR Apache-2.0 | Win32 API declarations |
| [`serde`](https://crates.io/crates/serde), `serde_core` | MIT OR Apache-2.0 | Serialization framework     |
| [`toml`](https://crates.io/crates/toml), `toml_datetime`, `toml_parser`, `toml_writer`, `serde_spanned` | MIT OR Apache-2.0 | Catalog and profile files |
| [`winnow`](https://crates.io/crates/winnow)   | MIT                 | Parser used by `toml`             |
| [`postcard`](https://crates.io/crates/postcard), `cobs` | MIT OR Apache-2.0 | Pipe message encoding       |
| [`thiserror`](https://crates.io/crates/thiserror) | MIT OR Apache-2.0 | Error types in `postcard`'s dependencies |

Build-time only (not part of any shipped binary): `serde_derive`,
`thiserror-impl`, `syn`, `quote`, `proc-macro2`, `unicode-ident`
(MIT OR Apache-2.0; `unicode-ident` additionally Unicode-3.0).

The exact versions are in `Cargo.lock`. `cargo tree -p syspilot-svc` prints the
current list.

## Linked into the updater

Besides `windows-sys` from the table above:

| Crate                                         | Licence             | Used for                          |
| --------------------------------------------- | ------------------- | --------------------------------- |
| [`ed25519-compact`](https://crates.io/crates/ed25519-compact) | MIT | Verifying a release's signature (Ed25519, SHA-512) |

## Linked into the update companion

Besides the crates in the first table:

| Crate                                         | Licence             | Used for                          |
| --------------------------------------------- | ------------------- | --------------------------------- |
| [`windows`](https://crates.io/crates/windows), `windows-core`, `windows-numerics`, `windows-result`, `windows-strings`, `windows-collections`, `windows-future`, `windows-threading` | MIT OR Apache-2.0 | Direct2D, DirectWrite, DWM and UI Automation calls for its window |

Build-time only: `windows-implement`, `windows-interface` (MIT OR
Apache-2.0). The libraries those calls reach are part of Windows.

## Inside the installer

| Component                                     | Licence             | Used for                          |
| --------------------------------------------- | ------------------- | --------------------------------- |
| [WiX toolset](https://wixtoolset.org/) utility extension, custom action library | MS-RL | Closing the app before its files are replaced. Embedded in the `.msi` by the build; not installed |

The WiX toolset itself (MS-RL) builds the package and is not part of it.

## Linked into or shipped with the app

| Component                                     | Licence             | Used for                          |
| --------------------------------------------- | ------------------- | --------------------------------- |
| [`windows-reactor`](https://crates.io/crates/windows-reactor) and the `windows-*` support crates it depends on | MIT OR Apache-2.0 | Declarative WinUI 3 views |
| [`rustc-hash`](https://crates.io/crates/rustc-hash) | MIT OR Apache-2.0 | Hashing inside `windows-reactor` |
| [Windows App SDK runtime](https://www.nuget.org/packages/Microsoft.WindowsAppSDK.Runtime) 2.4.0 | Microsoft licence terms, see the package page | WinUI 3. Shipped unmodified as separate files next to `syspilot.exe` |
| [WebView2 projection](https://www.nuget.org/packages/Microsoft.Web.WebView2) (`Microsoft.Web.WebView2.Core.dll`) | Microsoft licence terms, see the package page | Staged by the toolkit's setup helper; SysPilot does not use WebView2 |

The runtime files are fetched at build time by
[`windows-reactor-setup`](https://crates.io/crates/windows-reactor-setup)
(MIT OR Apache-2.0, build-time only) and are not stored in this repository.
The service does not load any of them.

The app's icon is compiled into the executable with
[`embed-resource`](https://crates.io/crates/embed-resource) (MIT, build-time
only, with its build-time dependencies), which runs the Windows SDK's resource
compiler.

## Not distributed

### PawnIO

SysPilot can use the [PawnIO](https://pawnio.eu/) driver to read hardware
sensors. **SysPilot does not include, install or update the driver.** It
detects an installed driver and talks to it through its device interface.

PawnIO *modules* are small programs the driver runs. SysPilot uses the official
modules from the PawnIO project, which are licensed under the LGPL. They are
not stored in this repository: `cargo xtask fetch-modules` downloads the pinned
release and verifies each file by SHA-256. A release that ships modules ships
them as separate, unmodified files next to the LGPL text.

## Catalog sources

Catalog entries cite Microsoft documentation as their source. The entries
describe documented settings in SysPilot's own words; no documentation text is
reproduced beyond setting names and values.
