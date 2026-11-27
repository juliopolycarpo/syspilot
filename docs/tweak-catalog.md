# Tweak catalog

The built-in tweaks live in `catalog/tweaks/` as TOML. They are plain data: the
service loads them, the UI lists them, and the engine applies and keeps them.
This page is the authoring guide. The schema itself is in
`crates/proto/src/catalog.rs` and `crates/proto/src/action.rs`.

## Layout

One file per category, named after it:

| File               | Category    | Holds                                              |
| ------------------ | ----------- | -------------------------------------------------- |
| `performance.toml` | performance | Responsiveness, latency and throughput.            |
| `privacy.toml`     | privacy     | Data sent to Microsoft or exposed to apps.         |
| `security.toml`    | security    | Attack surface, exploit mitigations and audits.    |
| `reliability.toml` | reliability | Stability, recoverability and update behavior.     |
| `other.toml`       | other       | Interface and quality-of-life adjustments.         |

Every entry is a `[[tweak]]` table with one or more `[[tweak.action]]` tables
after it. Entry IDs are dot-separated lowercase words (`[a-z0-9-]`, at most 80
characters), start with the category (`privacy.advertising-id`) and are unique
across all files. Profiles list the IDs they enforce, so do not rename an ID
without migrating them.

## Fields

| Field          | Required         | Meaning                                                                                      |
| -------------- | ---------------- | -------------------------------------------------------------------------------------------- |
| `id`           | yes              | Stable identifier, see above.                                                                |
| `title`        | yes              | Sentence case, at most 60 characters, no trailing period.                                    |
| `summary`      | yes              | What the user will notice, in plain words, ending with a period. Shown at both levels.       |
| `detail`       | Advanced entries | The mechanism: policy name, key, value, what each value means, which editions honor it.      |
| `level`        | yes              | `simple` or `advanced`.                                                                      |
| `category`     | yes              | `performance`, `privacy`, `security`, `reliability` or `other`. Must match the file.         |
| `direction`    | security entries | `hardens` or `weakens`. Allowed on other categories when it is meaningful.                   |
| `risk`         | weakening        | The danger in plain words, ending with a period.                                             |
| `side-effects` | optional         | What else changes or stops working. Leave it out when there is nothing to say.               |
| `reboot`       | optional         | `none` (default), `sign-out` or `restart`: what must happen before the change is complete.   |
| `source`       | yes              | One primary Microsoft page, see [Sources](#sources).                                         |
| `min-build`    | optional         | First Windows build the entry applies to: `22000`, `22621` or `26100`.                       |
| `audit`        | optional         | `true` for detect-only entries, see [Audit entries](#audit-entries).                         |
| `triggers`     | optional         | When the engine re-evaluates the entry: `boot`, `logon`, `resume`, `change` or `{ interval = secs }` (60 or more). Leave it out for the defaults of the actions. |

## Actions

Use TOML literal strings (single quotes) for registry paths so backslashes need
no escaping.

```toml
# Registry value. scope is "machine" (HKLM) or "user" (HKCU of every logged-on
# user). key has no hive prefix and no leading backslash. data is one of
# { dword }, { qword }, { sz }, { expand-sz }, { multi-sz }, { binary } or "absent".
[[tweak.action]]
registry = { scope = "machine", key = 'SOFTWARE\Policies\Microsoft\Windows\DataCollection', name = "AllowTelemetry", data = { dword = 1 } }

# Service start type: automatic, automatic-delayed, manual or disabled.
[[tweak.action]]
service = { service = "DiagTrack", start = "disabled" }

# Power setting of the active plan. Give ac, dc or both.
[[tweak.action]]
power = { subgroup = "0012ee47-9041-4b5d-9b77-535fba8b1442", setting = "6738e2c4-e8a5-4a42-b16a-e040e769756e", ac = 0 }
```

The schema also has a `command` action. The built-in catalog does not use it:
every built-in entry must be reversible by the engine without running code.

## Rules

### Sources

- Every entry cites one primary Microsoft page: `learn.microsoft.com`,
  `support.microsoft.com`, `devblogs.microsoft.com`, `techcommunity.microsoft.com`,
  `www.microsoft.com`, `msrc.microsoft.com` or `docs.microsoft.com`.
- The page must state the exact key, value, service or GUID, and what the data
  means. Open it and check; do not copy from a blog, forum or Q&A answer.
- Use locale-free URLs (`https://learn.microsoft.com/windows/...`, never
  `/en-us/`). Add an anchor only when you have confirmed it exists on the page.
- Prefer `HKLM\SOFTWARE\Policies\...` keys: they are the documented way to set a
  behavior by policy.
- If the page names the value but not the data, or a GUID is documented on a
  sibling page, say so in the pull request. Drop the entry when no Microsoft page
  states the value.

### Levels

- **Simple** entries are safe, reversible, do not weaken anything and use no
  jargon. The summary says what the user will notice. The schema rejects a Simple
  entry that weakens anything or cannot be reverted exactly.
- **Advanced** entries cover everything else and need a `detail`.
- Security entries are always Advanced and always have a `direction`.
- An entry that `weakens` is Advanced and has a `risk` that a non-expert can
  read.

### Audit entries

Set `audit = true` for facts Windows does not let software change or that are
unsafe to change from here: Secure Boot, the UAC configuration, firmware state.
An audit entry is category `security`, direction `hardens`, and its action holds
the value that counts as compliant. It is reported and never applied. Keep them
to a handful: the test accepts 5 to 10.

### Protected settings

Defender tamper protection and similar protected settings are included only when
Microsoft documents the key. Say in `side-effects` that Windows may refuse the
change.

### Out of scope

The catalog does not turn off Windows Update as a whole, Defender real-time
protection, the firewall, UAC or SmartScreen. UAC may appear as an audit entry.

### Wording

- Titles and summaries use "Turn on" and "Turn off", "Require", "Block" and
  "Limit". Do not start a title with "Disable" or "Enable".
- One term per concept across the catalog, US English, no marketing words
  ("boost", "turbo"), no exclamation marks.
- One concept per entry. Several values are fine when they are one switch
  (the three activity history policies); two switches are two entries.
- Capitalize only the first word of a title and proper nouns. Add a product name
  to `PROPER_NOUNS` in `crates/proto/tests/builtin_catalog.rs` when you need one.

### Ownership

A registry value, service or power setting belongs to exactly one entry. Two
entries writing the same target would fight over it, so the test rejects the
second one. Opposite choices of one setting are not two entries.

## Example

```toml
[[tweak]]
id = "security.smb1-server"
title = "Turn off the SMBv1 server"
summary = "This PC no longer accepts SMBv1 connections. SMB 2 and SMB 3 keep working."
detail = '''Sets HKLM\SYSTEM\CurrentControlSet\Services\LanManServer\Parameters\SMB1 (REG_DWORD) to 0. The value does not exist by default and the default is enabled. Microsoft states that SMBv1 has significant security vulnerabilities and strongly encourages you not to use it.'''
level = "advanced"
category = "security"
direction = "hardens"
side-effects = "Old computers and software that only speak SMBv1 can no longer connect to shares on this PC."
reboot = "restart"
source = "https://learn.microsoft.com/windows-server/storage/file-server/troubleshoot/detect-enable-and-disable-smbv1-v2-v3"

[[tweak.action]]
registry = { scope = "machine", key = 'SYSTEM\CurrentControlSet\Services\LanmanServer\Parameters', name = "SMB1", data = { dword = 0 } }
```

A Simple entry is shorter: `detail` is optional and `side-effects`, `reboot` and
`direction` are only there when they apply. `privacy.advertising-id` in
`privacy.toml` is one.

A weakening entry adds `direction = "weakens"` and a `risk`:

```toml
direction = "weakens"
risk = "Programs running on this PC can read memory they should not be able to see."
```

## Validate

```sh
cargo nextest run -p syspilot-proto --test builtin_catalog
```

The test parses every `*.toml` file under `catalog/tweaks/` with
`Catalog::parse_all` and then checks the rules above: one file per category,
IDs, titles, summaries, wording, sources, levels, directions, risks, audit
entries, `min-build`, no `command` actions and one owner per target. Each rule is
a separate test and lists every offending entry:

```
(catalog entry "privacy.x"): expected title <= 60 chars | received: 71
```

The test checks shape, not truth. Reviewers still open the cited page and confirm
that it says what the entry claims.
