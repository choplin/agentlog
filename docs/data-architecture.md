# Data architecture

Agentlog builds a local, derived catalog from coding-agent session history.
This is the current technical contract, not a privacy policy.

## At a glance

| Question | Current behavior |
| --- | --- |
| What does Agentlog read? | Local session sources listed in [Source discovery and root precedence](providers.md#source-discovery-and-root-precedence). |
| Does Agentlog modify provider logs? | No. Collectors read provider-owned sources; only Agentlog-owned storage is written. |
| What enters the catalog? | Source/session metadata, visible user and assistant text, and bounded tool names/statuses. |
| What is omitted? | Provider-native reasoning, structured tool payloads, structured diffs, images, attachments, and raw records. |
| Does Agentlog send catalog data elsewhere? | No. The current runtime has no server, account, telemetry, AI service, or remote synchronization path. |
| What does `purge` remove? | Rows in Agentlog's SQLite catalog. It keeps provider logs, configuration, database files, and schema. |

## Data ownership and flow

```text
provider-owned sources (read only)
        |
        v
provider-specific bounded projection
        |
        v
Agentlog-owned SQLite catalog
        |
        +--> list / show / filters / diagnostics / TUI
```

Rules:

1. Provider collectors discover only their configured or default roots.
2. File collectors use metadata and byte reads. OpenCode copies its bounded
   database and WAL to a private temporary directory; SQLite opens only the copy.
3. Each accepted source becomes a bounded normalized snapshot.
4. Agentlog writes snapshots only to its own SQLite catalog.
5. `list`, `show`, and the TUI read the catalog and never trigger provider sync.

`--home` and `AGENTLOG_HOME` change only Agentlog-owned storage. Provider roots
use separate settings; see
[Source discovery and root precedence](providers.md#source-discovery-and-root-precedence).

## Agentlog-owned storage

Agentlog keeps `config.toml` and `agentlog.sqlite3` in one data directory.

| Precedence | Data directory |
| ---: | --- |
| 1 | `--home DIR` |
| 2 | `AGENTLOG_HOME` |
| 3 | absolute, non-empty `XDG_DATA_HOME` + `/agentlog` |
| 4 | `$HOME/.local/share/agentlog` |

Agentlog may create this directory and update its catalog. It does not create
provider roots or write into them.

## Stored catalog data

Schema v1 has three tables.

| Table | Stored fields |
| --- | --- |
| `sources` | Provider, source format, canonical local locator, last-success time, diagnostic status/message/time |
| `sessions` | Source-scoped session identity, title, repository, CWD, model, execution kind, start time, last-visible-event time |
| `transcript_items` | Ordered visible user text, visible assistant text, or a bounded tool name and collector-emitted status |

Notes:

- Missing provider metadata remains absent; Agentlog does not invent values.
- A valid source with no supported visible conversation can produce no session.
- Interactive diagnostics show source locators. Serialized list and preview
  results omit them.

## Excluded data

Collectors select supported fields instead of copying provider records.

| Provider-native field | Catalog behavior |
| --- | --- |
| Hidden reasoning, thinking, chain of thought | Not selected |
| Tool input, arguments, output, result, error, raw payload | Not selected |
| Structured patch or diff | Not selected |
| Image, screenshot, document, binary attachment | Not selected |
| Provider-private blob or raw record | Not selected |
| Recognized synthetic, environment, or control transcript record | Not selected |
| Provider metadata-only record | Not stored as transcript content; allowlisted session metadata may be projected |
| Tool name and status | Bounded normalized marker may be stored |

This is a structural boundary, not content inspection. Visible conversation
text is retained regardless of subject matter and can contain patches, JSON,
encoded data, secrets, or text copied from an otherwise excluded field.

The catalog has no raw-provider archive, AI enrichment, tags, ctx index, or
configurable retention subsystem. See [Safety bounds](providers.md#safety-bounds).

## Refresh and last-good behavior

| Outcome | Catalog result | Previous successful sessions |
| --- | --- | --- |
| `ok` | Accepted source replaces its snapshot | Replaced |
| `partial` | Bounded accepted snapshot is stored | Replaced |
| `error` | Diagnostic is updated | Retained |
| `missing` | Source was absent from a completed discovery | Retained |

Malformed, unsupported, oversized, or unreadable sources become `error`.
Collectors with an after-read stability check also reject inputs that change
during collection. Codex currently has no after-read fingerprint comparison;
see [Read-only provider boundary](providers.md#read-only-provider-boundary).

A provider setup/discovery failure does not mark retained sources missing and
does not stop later providers. A catalog/storage failure rolls back that
provider's staged transaction.

## Local-only operation

The current runtime reads local provider sources and writes or reads the local
SQLite catalog. It has no outbound data client or remote catalog path. Build and
installation distribution are outside this runtime boundary.

## Purging Agentlog data

`agentlog purge` previews the target path, row counts, and approximate SQLite
main/WAL/SHM size. Interactive confirmation is bound to the previewed database
identity and content. `--yes` skips the prompt, not the path, schema, identity,
size, or writer-lock checks.

| Purge removes | Purge keeps |
| --- | --- |
| All `sources` rows | Provider-owned logs and roots |
| Cascaded `sessions` rows | `config.toml` |
| Cascaded `transcript_items` rows | Agentlog data directory |
|  | SQLite files and schema |

Purge refuses an external, unknown-version, lookalike, symlinked, or otherwise
unvalidated database. Purged rows can be rebuilt only from provider sources
that still exist, remain readable, and use a supported format.
