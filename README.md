# Drover

Drover is a lightweight REST service that evaluates rules against JSON records.
Rules are written in **CEL** or **Starlark**, stored in configurable data sources
(PostgreSQL, YAML or JSON files) and changed at runtime without redeploying the
applications that call the API. It ships with a web administration interface
for rules, rule groups, tests and configuration.

```text
POST /api/v1/check  {"group": "onboarding", "records": [{"age": 15, "country": "US"}]}

200 OK
{
  "execution_id": "01a1729b-a410-72bd-90d9-fcda05983e3f",
  "status": "completed",
  "outcome": "invalid",
  "summary": {"records": 1, "valid": 0, "invalid": 1, "errored": 0, ...},
  "results": [{"index": 0, "outcome": "invalid", "rules": [
    {"rule": "customer.adult", "status": "fail", "message": "customer is under the minimum age"},
    {"rule": "customer.country", "status": "fail", "message": "unsupported country US"}
  ]}],
  ...
}
```

- Synchronous and asynchronous checks; every execution is persisted and can be retrieved by id.
- Rules are compiled once and cached in memory; cache updates are atomic.
- Data sources are configured per category (rules, groups, tests, configuration,
  executions) and can be mixed freely.
- Regression tests per rule, group or rule set, run at startup, through the API,
  from the CLI or in the administration interface.
- Bounded resources: a fixed evaluation thread pool, admission control, and
  per-rule time, tick and heap limits for Starlark.

Further documentation:

- [docs/api.md](docs/api.md): request and response formats, error codes.
- [docs/architecture.md](docs/architecture.md): components, consistency,
  concurrency, and every design decision and assumption.

## Quick start

Requirements: Rust 1.85 or later. A C compiler is needed for the default `mimalloc` feature.

```sh
cargo build --release

# Run the file-based example (rules, groups, tests and config in examples/data)
DROVER_ADMIN_TOKEN=change-me-to-a-long-secret \
  ./target/release/drover -c examples/drover.yaml
```

Then:

```sh
# Synchronous check against a group
curl -s localhost:8080/api/v1/check -d '{
  "group": "onboarding",
  "records": [{"age": 34, "country": "IT", "email": "ada@example.com"},
              {"age": 15, "country": "US", "email": "bad"}]}'

# Asynchronous check: 202 Accepted + Location header, then poll
curl -si 'localhost:8080/api/v1/check?mode=async' -d '[{"items": []}]'
curl -s localhost:8080/api/v1/executions/<execution_id>

# Run the test suite
curl -s -X POST localhost:8080/api/v1/tests/run
```

Open <http://localhost:8080/admin/> and sign in with the admin token.

## Command line

```text
drover [serve] [-c drover.yaml]      run the server (default)
drover check-config [-c FILE]        validate configuration, data sources and rules, then exit
drover test [-c FILE] [--json]       run the stored test suite; exit status 1 if any test does not pass
```

The configuration file is taken from `-c`, then `DROVER_CONFIG`, then `./drover.yaml`.
`drover test` is meant for CI pipelines that validate rule changes before deployment.

## Configuration

There are two layers of configuration:

1. The **bootstrap file** (YAML or JSON) covers what Drover needs before it can
   reach its data sources: listen address, credentials, logging, data sources.
2. **Configuration entries** are key/value entries stored in the configuration
   data source. Each has a scope:
   - `rule` entries are exposed to rules as the `config` variable;
   - `system` entries are Drover settings and are never visible to rules.

### Bootstrap file

```yaml
server:
  bind: "127.0.0.1:8080"
  max_body_bytes: 16777216      # request body limit (413 above)
  io_threads: 2                 # async I/O threads; default min(4, CPUs)
  shutdown_grace_secs: 30
auth:
  admin_token: "${DROVER_ADMIN_TOKEN}"   # required for /admin/api
  api_tokens: ["${CLIENT_TOKEN}"]        # bearer tokens for /api/v1 and /metrics; empty = open
  allow_unauthenticated_admin: false     # development only
log:
  level: info                   # tracing filter, e.g. "drover=debug,info"
  format: text                  # text | json
datasources:
  rules:      { type: yaml, path: data/definitions.yaml }
  groups:     { type: yaml, path: data/definitions.yaml }
  tests:      { type: yaml, path: data/tests.yaml }
  config:     { type: yaml, path: data/config.yaml }
  executions: { type: json, path: data/executions }
```

- `${VAR}` and `${VAR:-default}` are replaced with environment variables. A
  missing variable without a default is an error.
- `DROVER_BIND`, `DROVER_ADMIN_TOKEN`, `DROVER_API_TOKENS` (comma-separated) and
  `DROVER_LOG` override the file.
- Relative file paths are resolved against the directory of the configuration file.
- Unknown keys are rejected, so typos fail fast.

### Data sources

Each of the five categories takes one of these data source types:

| `type` | Options | Notes |
|---|---|---|
| `memory` | — | Not persistent. For development and tests. |
| `yaml`, `json` | `path`, `section` (default: category name) | Definitions: a document whose top level maps sections to lists (`rules: [...]`). Several categories may share one file. Executions: `path` is a directory with one file per execution. |
| `postgres` | `url`, `table` (default `drover_<category>`, may be `schema.table`), `max_connections` (5), `connect_timeout_secs` (10), `auto_migrate` (true) | One table per category with one column per field, so definitions can be edited with SQL. Group memberships live in `<groups table>_members`. Categories using the same URL share a connection pool. Build with `--features postgres-tls` for TLS. |

Example with mixed sources ([examples/drover-mixed.yaml](examples/drover-mixed.yaml)):

```yaml
datasources:
  rules:      { type: postgres, url: "${DROVER_DB_URL}" }
  groups:     { type: json, path: data/groups.json }
  tests:      { type: yaml, path: data/tests.yaml }
  config:     { type: postgres, url: "${DROVER_DB_URL}", table: drover_config }
  executions: { type: postgres, url: "${DROVER_AUDIT_DB_URL}" }
```

File writes are atomic (temporary file and rename), and categories sharing a
file share a lock. When Drover rewrites a file it keeps multi-line rule sources
as YAML block scalars but drops comments.

### Settings (`system` entries)

| Key | Default | Applies | Meaning |
|---|---|---|---|
| `worker_threads` | CPU count | restart | OS threads evaluating rules. |
| `max_queued_checks` | 256 | restart | Checks admitted at once (pending + running). Excess checks get 503. |
| `unknown_group_mode` | `strict` | live | `strict` rejects checks naming a nonexistent group (422); `fallback` runs all rules and says so in the response. |
| `max_records_per_check` | 10000 | live | Records accepted per check. |
| `check_timeout_ms` | 30000 | live | Wall-clock limit per check, queueing included (0 = none). |
| `rule_timeout_ms` | 1000 | live | Wall-clock limit per Starlark rule evaluation (0 = none). |
| `starlark_max_ticks` | 1000000 | live | Function calls + loop iterations per Starlark evaluation. |
| `starlark_max_heap_bytes` | 64 MiB | live | Best-effort Starlark heap limit per evaluation. |
| `starlark_max_callstack` | 100 | live | Starlark call depth. |
| `refresh_interval_secs` | 30 | live | Polling interval for external data source changes (0 = off). |
| `startup_tests` | `warn` | restart | `off`, `warn` (log failures and start) or `fail` (abort startup). |
| `fail_on_invalid_rules` | `true` | restart | Abort startup if a stored rule does not compile. |
| `execution_retention_secs` | 604800 | live | Finished executions older than this are purged (0 = keep). |

Values are type-checked when written through the administration API and at
startup. An invalid value prevents startup. A refresh that loads an invalid
value is rejected and the previous configuration stays active.

## Writing rules

A rule has an `id`, a `language`, a `source`, an optional `description`, an
optional failure `message`, an `enabled` flag and free-form `metadata`.

The rule sees two values: `record`, the JSON record being checked, and `config`,
the `rule`-scoped configuration entries as a map. It returns one of:

- `true` (pass) or `false` (fail);
- a map `{"pass": bool, "message": string}`, where `message` explains the failure.

Anything else, and any error raised while evaluating, is a rule **error**,
reported separately from failures (`status: "error"` with an `error.kind` of
`compile`, `runtime`, `timeout`, `resource_limit`, `invalid_result` or `internal`).

**CEL** rules are a single expression:

```cel
record.age >= config.min_age
```

```cel
record.items.all(i, i.qty > 0)
  ? true
  : {"pass": false, "message": "every item needs a positive quantity"}
```

Use `has(record.field)` for optional fields; reading a missing field is a
runtime error. JSON integers are CEL `int` (or `uint` above `i64::MAX`), and
other numbers are `double`.

**Starlark** rules are a module that defines `check(record, config)`:

```python
def check(record, config):
    email = record.get("email", "")
    if "@" not in email:
        return {"pass": False, "message": "invalid e-mail address: %r" % email}
    return True
```

`record` and `config` are read-only. `load()` is disabled, and `print()` writes
to the debug log. The standard library includes `json`, `struct`, `record`,
`enum`, `set`, `map`, `filter` and `partial`. Each evaluation is limited by
`rule_timeout_ms`, `starlark_max_ticks`, `starlark_max_heap_bytes` and
`starlark_max_callstack`.

Rules are compiled when they are loaded or changed, never per request. A rule
that does not compile is rejected by the administration API. If one is found in
a data source, startup fails unless `fail_on_invalid_rules` is `false`, in which
case the rule reports a `compile` error on every evaluation.

## Rule groups

A group is a named list of rule ids; a rule may be in several groups. A check
without `group` runs every enabled rule. A check with `group` runs the group's
enabled rules, and an unknown group follows `unknown_group_mode`. Disabled rules
are never selected by checks, but rule-scoped tests still evaluate them.

## Tests

```yaml
tests:
  - id: onboarding.minor-rejected
    tags: [onboarding]
    record: {age: 15, country: IT, email: kid@example.com}
    scope: {type: group, group: onboarding}      # all | group | rule
    expect: {outcome: invalid, rules: [customer.adult], exclusive: true}
```

`scope` selects which rules evaluate the record: `{type: all}` (the default),
`{type: group, group: ...}` or `{type: rule, rule: ...}`.

`expect.outcome` takes one of three values:

| `outcome` | The test passes when |
|---|---|
| `valid` | every rule in scope passes |
| `invalid` | at least one rule fails; with `rules`, each listed rule fails; with `exclusive: true`, no other rule fails |
| `error` | a rule errors and none fails; `rules` and `exclusive` apply to the erroring rules |

A test that expects a rejection *passes* when the record is rejected. A test's
status is `passed`, `failed` or `error`; `error` means the test itself could not
be evaluated, for example because its scope names a group that does not exist.
Results include the actual outcome, the failing and erroring rules, every rule
outcome and human-readable diagnostics.

Tests run in four places, all through the same engine as checks:

- at startup, per the `startup_tests` setting;
- `POST /api/v1/tests/run` with `{"ids": [...]}` and/or `{"tags": [...]}`
  (empty runs everything);
- the administration interface;
- `drover test` on the command line.

## Administration interface

`/admin/` is a single-page interface embedded in the binary. It needs no external
assets and is served with a strict Content Security Policy. It lets you:

- **Rules:** list, create, edit, enable/disable and delete rules. The source
  editor is language-aware (template, help, indentation, line numbers) and shows
  compile errors as you type. A *Try it* panel evaluates unsaved source against
  sample records. After you create a rule, Drover offers to add test records
  with expected outcomes and runs them immediately.
- **Groups:** manage groups and their members.
- **Tests:** create and edit tests, run one, several or all of them, and see the
  diagnostics for any that fail.
- **Configuration:** manage entries. Entries visible to rules are marked, and
  system settings are type-checked and labelled *live* or *restart required*.
- **Status:** rule snapshot version, invalid rules, refresh history, worker
  pool, data sources and startup report.

Every change is validated, persisted to its data source and then applied to the
running instance:

- An invalid change returns 422 and nothing is written.
- A data source failure returns 503 and nothing is applied.
- A change that was saved but could not be applied returns 500 `apply_failed`;
  the next successful refresh picks it up.

## Caching and refresh

Rules, groups and configuration are held in an immutable snapshot that is
replaced atomically. A check uses the snapshot it started with from start to
finish, so it never sees half-applied changes. Unchanged rules keep their
compiled programs across refreshes. A refresh is triggered by:

- **Administration changes:** immediately.
- **`POST /admin/api/refresh`:** explicit reload.
- **Polling, every `refresh_interval_secs`:** picks up changes made outside
  Drover (edited files, SQL). File data sources are checked by modification
  time and size first; PostgreSQL tables are re-read and compared. A snapshot
  is rebuilt only when something changed.

If a refresh fails (data source down, invalid setting), the current snapshot
stays active and the error appears in `/admin/api/status`.

## Startup and readiness

Startup proceeds in this order:

1. Load and validate configuration.
2. Open the data sources.
3. Load and compile rules and groups.
4. Apply `fail_on_invalid_rules`.
5. Start the worker pool and mark checks interrupted by a previous run as `failed`.
6. Run the startup tests.
7. Start polling.

Any failure exits with status 1. Drover never serves requests from a partial
state. The HTTP port opens at the beginning so probes can connect:

- `GET /health/live` returns 200 as soon as the process is up.
- `GET /health/ready` returns 503 (`starting`, `failed`, `stopping`) until
  initialization completes, then 200 with the rule count.
- API routes answer 503 `not_ready` until then.

On SIGTERM or Ctrl-C, Drover stops accepting connections, finishes in-flight
requests, waits up to `shutdown_grace_secs` for asynchronous checks and exits.

## Security

- **Rules are untrusted code.** CEL has no I/O and always terminates. Starlark
  runs without `load()`, without file or network access and with read-only
  inputs, bounded by the time, tick, heap and stack limits above.
- **Authentication.** Bearer tokens, compared in constant time. The
  administration API is disabled until `auth.admin_token` is set. The main API
  is open unless `auth.api_tokens` is set, and Drover warns at startup when it
  is open. `/health/*` is unauthenticated and reveals only counts.
- **Error detail.** Main-API errors omit storage-specific detail.
  Administration responses include it, because operators need it.
- **Not built in.** TLS termination, rate limiting and per-user roles are
  expected from the surrounding infrastructure. See
  [docs/architecture.md](docs/architecture.md#security).

## Observability

- `GET /metrics` (Prometheus text format) reports checks by mode and status,
  rejected checks, records, rule evaluations by status, check duration
  histograms, worker pool usage, admitted checks, cache refreshes, test runs
  and persistence failures.
- The administration API reports evaluations, failures, errors and mean
  evaluation time per rule.
- Logs go to stderr as text or JSON; set the level with `log.level`.

## Performance

Benchmarks use `cargo bench`, 10,000 records per batch, an Apple M1 laptop (4
performance + 4 efficiency cores) and the default `mimalloc` allocator.
Numbers vary by about ±25% between runs.

| Workload | 1 thread | 8 worker threads |
|---|---|---|
| 5 CEL rules | ~190k records/s | ~750k–1M records/s |
| 5 Starlark rules | ~350k records/s | ~0.9–1.3M records/s |
| 25 CEL + 25 Starlark rules | ~30k records/s | ~130–160k records/s |

Compiling 500 rules takes about 10–16 ms. With the macOS system allocator
(`--no-default-features --features postgres`), parallel throughput is 1.7–3×
lower.

## Building and testing

```sh
cargo build --release                       # default features: postgres, mimalloc
cargo build --release --no-default-features # files and memory only, system allocator
cargo test                                  # unit and integration tests
cargo bench                                 # benchmarks (benches/engine.rs)
cargo clippy --all-targets && cargo fmt --check
```

The PostgreSQL integration tests run when `DROVER_TEST_POSTGRES_URL` is set, and
each run creates uniquely named tables:

```sh
docker run -d --name drover-pg -e POSTGRES_USER=drover -e POSTGRES_PASSWORD=drover \
  -e POSTGRES_DB=drover -p 55432:5432 postgres:17-alpine
DROVER_TEST_POSTGRES_URL=postgres://drover:drover@127.0.0.1:55432/drover cargo test
```

The tests are organized as follows:

| Where | Covers |
|---|---|
| `src/**` unit tests | Languages, limits, engine selection and aggregation, settings, cache refresh, test expectations |
| `tests/api.rs` | Sync and async checks, polling, persistence, input validation, admission, timeouts, auth, metrics |
| `tests/admin.rs` | Administration CRUD, validation, refresh, `apply_failed`, file persistence across restarts |
| `tests/startup.rs` | Startup policies, recovery of interrupted executions, unavailable data sources, CLI |
| `tests/stores.rs` | Conformance of every data source backend (memory, JSON, YAML, shared file, PostgreSQL) |

## Project layout

```text
src/
  main.rs         CLI, server lifecycle, signals
  bootstrap.rs    bootstrap configuration file
  app.rs          startup sequence and runtime assembly
  api.rs          HTTP routes, authentication, errors
  admin.rs        administration service (validate → persist → apply)
  executions.rs   execution manager (admission, sync/async, persistence)
  testing.rs      test evaluation and runner
  cache.rs        cache manager (snapshots, refresh, polling)
  engine/         rule engine: snapshot and plans, worker pool, parallel runs
  lang/           language adapters: CEL, Starlark
  store/          data sources: memory, JSON/YAML files, PostgreSQL
  settings.rs     typed system settings
  model.rs        domain model
  metrics.rs      Prometheus metrics
ui/               administration interface (embedded)
examples/         example configurations and data
benches/          benchmarks
```
