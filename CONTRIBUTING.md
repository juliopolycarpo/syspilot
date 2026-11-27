# Contributing

## Building and testing

You need Windows 10 or 11 on x64, `rustup` and the MSVC build tools. The
toolchain version is pinned in `rust-toolchain.toml` and installs itself.

```console
> cargo build
> cargo nextest run --workspace
> cargo test --doc --workspace
> cargo clippy --workspace --all-targets -- -D warnings
> cargo fmt --all --check
```

All must pass before a change is merged; CI runs the same commands.

The tests run with [cargo-nextest](https://nexte.st), at the version
`.config/nextest.toml` requires (`cargo install cargo-nextest --locked`, or
its prebuilt binary). It runs each test in a process of its own and the
crates side by side, which halves the time on CI. It fails a test that
leaves a process holding the test's output, and ends one that hangs. It skips doctests, hence
the separate `cargo test --doc`. Plain `cargo test --workspace` runs the same
tests. It can be the quicker of the two on a machine whose antivirus scans
every process that starts.

### Tests that touch the system

Most tests run against fakes and need nothing. Some exercise the real
operating system and need an **elevated terminal**:

- the engine's `tests/os.rs` and the service's unit tests write scratch
  registry values;
- the service's `tests/service.rs` installs a real Windows service.

They keep to these rules, and so must new tests:

- Registry writes go only under `HKLM\Software\SysPilot-Test` and
  `HKCU\Software\SysPilot-Test`.
- Service, pipe and directory names come from an instance whose name starts
  with `SysPilot-Test`. Never the product instance.
- No catalog tweak is ever applied to the machine running the tests. Tests
  enforce custom actions that point at scratch keys.
- Nothing is written to hardware.
- A test removes what it created, also when it fails (use a guard with `Drop`).

Run without elevation, the tests that need it fail with a message that says so.

### Trying the service by hand

```console
> cargo run -p syspilot-svc -- --instance SysPilot-Test.dev run
```

and in a second elevated terminal:

```console
> cargo run -p syspilot-svc -- --instance SysPilot-Test.dev status
> cargo run -p syspilot-svc -- --instance SysPilot-Test.dev list
```

`list` and `show` only read. `enable` writes real settings even on a test
instance: the catalog is the same. Stop the run with Ctrl+C and remove its data
with `uninstall`.

## Code

- Windows only. No `cfg(windows)` gates, no portability layers.
- `unsafe` blocks carry a `// SAFETY:` comment that states why the call is
  sound; the lint enforces its presence, review enforces its content.
- No `unwrap` outside tests.
- A lint is suppressed with `#[expect(lint, reason = "…")]`, never
  `#[allow]`: the reason says why, and the expectation warns once there is
  nothing left to suppress, which `-D warnings` makes an error.
- Doc links resolve and point at public items:
  `cargo doc --workspace --no-deps --document-private-items` fails
  otherwise, and CI runs it.
- External I/O goes behind a trait with a named fake (`FakeRegistry`,
  `FakeHardware`), not an ad-hoc stub in the test.
- Every new function has a test. Every bug fix has a test that fails without
  the fix.
- Errors and assertion messages name the offending value and what was
  expected: `(install location): expected the executable under Program Files |
  received: C:\Users\…`.
- Text a user reads is plain language. Technical names (registry paths, GUIDs)
  belong in the Advanced level and in `detail` fields.
- The service has a size and memory [budget](docs/architecture/overhead.md). A
  new dependency in `crates/svc` needs an entry in
  [docs/decisions.md](docs/decisions.md).

## Catalog entries

See [docs/tweak-catalog.md](docs/tweak-catalog.md). The short version: every
entry cites the Microsoft page that documents the exact value it writes, and
`cargo test -p syspilot-proto --test builtin_catalog` enforces the editorial
rules.

## Commits and pull requests

- [Conventional Commits](https://www.conventionalcommits.org/): `feat(svc):
  …`, `fix(engine): …`, `docs: …`. One concern per commit, with a body that
  says why.
- Sign your commits and sign off on the
  [Developer Certificate of Origin](https://developercertificate.org/):
  `git commit -s -S`.
- A bug or improvement you find on the way that is not part of your change
  gets its own issue and its own change.

The subject of each commit on `main` becomes a line of
[CHANGELOG.md](CHANGELOG.md), so write it for a reader of the changelog.
A squash merge takes the subject from the PR title, which CI checks: a
subject that is not a Conventional Commit would stop every later release.
To revert a PR, title the revert `revert: …` rather than keeping GitHub's
`Revert "…"`.

## Releases

1. Open a `chore: release X.Y.Z` PR that sets the version in `Cargo.toml` and
   the README's status line, runs `cargo update -w` and writes the changelog
   with [git-cliff](https://git-cliff.org), configured in `cliff.toml`:

   ```console
   > git cliff --tag vX.Y.Z -o CHANGELOG.md
   ```

   Regenerate the file if another PR merges first. Keep the PR to that one
   commit: git-cliff lists any other commit on the branch, and the squash
   merge removes it again, so the tag's check would fail.
2. After the squash merge, tag the merge commit: `git tag -s vX.Y.Z -m
   "SysPilot X.Y.Z"` and push the tag. Only `vX.Y.Z` tags start a release;
   there are no pre-releases.
3. The tag runs the release workflow. It stops if `CHANGELOG.md` is not what
   git-cliff writes for the tag. Otherwise it builds and tests the package and
   creates a draft release whose notes are the new changelog section. Edit the
   notes for readers of the release.
4. Sign and publish the draft: `./scripts/sign-release.ps1 -Tag vX.Y.Z
   -Publish`.

## Licence

Contributions are accepted under the licence of the crate they touch: GPL-3.0-
or-later for `svc`, `ui` and `hw`; MIT or Apache-2.0 for `proto`, `engine`,
`pawnio` and `xtask`.
