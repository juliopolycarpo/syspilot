# Third-party components

## Linked into the service

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
