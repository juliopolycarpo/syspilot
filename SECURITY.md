# Security policy

SysPilot runs a LocalSystem service. A flaw in it can be a local privilege
escalation, so reports are taken seriously and handled privately first.

## Reporting

Please do not open a public issue for a vulnerability. Use GitHub's private
reporting instead:

**[Report a vulnerability](https://github.com/juliopolycarpo/syspilot/security/advisories/new)**

Include what you found, how to reproduce it and which version or commit you
tested. You will get an answer within a week.

## Scope

In scope, for example:

- a standard user causing the service to write to the system, run a command,
  or reveal the profile;
- a way to make the service load a profile, state or catalog it should not
  trust;
- a way to impersonate the service to the app;
- a way to make an installed copy accept an update that the release key did
  not sign, or an older version;
- memory safety problems in `unsafe` code.

Out of scope:

- anything that requires already being an administrator or LocalSystem;
- vulnerabilities in the PawnIO driver itself (report those to its authors);
- a catalog entry that documents its effect correctly but that you consider a
  bad idea. Open a normal issue for that.

The boundary SysPilot defends is described in
[docs/security-model.md](docs/security-model.md).

## Supported versions

Until 1.0, only the latest release is supported.
