# Drover architecture

This document is for engineers working on Drover. It covers how the components
fit together, the consistency, concurrency and persistence guarantees, and the
design decisions and assumptions made where the specification left room.

## Components

| Logical component | Module | Responsibility |
|---|---|---|
| API layer | `api.rs` | Routing, bearer authentication, body limits, request parsing, error responses, static UI. |
| Execution manager | `executions.rs` | Admission control, sync/async execution, execution state transitions and persistence, restart recovery, retention. |
| Rule engine | `engine/mod.rs`, `engine/run.rs` | Selection (precomputed plans), per-record evaluation, aggregation, parallel batch runs. |
| Worker pool | `engine/pool.rs` | Fixed set of OS threads (`worker_threads`) for CPU-bound evaluation. |
| Language adapters | `lang/` | `RuleLanguage` trait; CEL and Starlark implementations. |
| Cache manager | `cache.rs` | Current snapshot, refresh, change detection. |
| Repositories | `store/` | `Repository<T>` for rules, groups, tests and configuration; `ExecutionRepository`; memory, file and PostgreSQL backends. |
| Administration | `admin.rs`, `ui/` | Validated changes (validate → persist → apply); embedded web interface. |
| Tests | `testing.rs` | Expectation semantics; runner shared by startup, API, admin and CLI. |
| Startup | `app.rs`, `main.rs` | Ordered initialization, readiness, background tasks, shutdown. |

The engine does not depend on any storage technology. It consumes plain
definitions (`model.rs`) and produces plain results. The data source layer
returns definitions and knows nothing about rules beyond their shape.

## Request flow

```text
HTTP worker (tokio)                              Worker pool (OS threads)
───────────────────                              ────────────────────────
parse body → validate → snapshot.select(group)
admission permit (or 503)
                     ── CheckJob ──────────────▶ claim chunk → evaluate records
                                                   per record, per language partition:
                                                   adapter converts the record once,
                                                   evaluates the partition's programs
                     ◀── results (oneshot) ───── last chunk assembles in order
persist execution → respond
```

## Language abstraction

Each language implements `lang::RuleLanguage`:

```rust
trait RuleLanguage {
    type Program;   // compiled rule, built at load/refresh time
    type Config;    // `rule`-scoped config, prepared once per snapshot
    fn compile(&self, rule_id, source, limits) -> Result<Program, String>;
    fn prepare_config(&self, config) -> Result<Config, String>;
    fn evaluate(&self, record, config, programs, limits, sink);
}
```

A blanket implementation turns it into the object-safe `LanguageAdapter` used by
the engine. Downcasting between the two is confined to that one implementation,
and it also catches panics from language runtimes.

`evaluate` receives every program of its language for one record, so the
adapter converts the record once per record, not once per rule:

- **CEL** builds borrowed CEL values that reference the parsed JSON strings.
- **Starlark** freezes the record into an immutable heap.

Adding a language means implementing the trait and registering it in
`LanguageRegistry::builtin`. Nothing else in the engine, API or result model
changes.

Results are interpreted through one shared contract, `verdict_from_json`: a
boolean, or `{pass, message}`. Rule errors (`RuleErrorInfo`) are a separate
type, never a `false`.

## Snapshot and cache consistency

`engine::Snapshot` is immutable and contains:

- the compiled rules, sorted by id, each with its program or its compile error;
- one precomputed `Plan` per group plus one for all enabled rules (rule indices
  partitioned by language, program handles cloned once);
- the `rule`-scoped configuration as JSON and as each language's prepared form;
- the typed `Settings`, a version number and the build diagnostics.

`cache::RuleCache` holds the current snapshot as `RwLock<Arc<Snapshot>>`.

- **Readers** clone the `Arc`, which takes nanoseconds. A check holds that one
  snapshot for its whole duration.
- **Refreshes are serialized.** A refresh loads rules, groups and configuration,
  compares them with the loaded definitions, and builds a new snapshot on the
  blocking pool. Compiled programs and statistics are reused for rules whose
  language and source are unchanged. The refresh then swaps the pointer.
- **Failures are contained.** Any load or validation error leaves the old
  snapshot in place and is recorded in `RefreshStatus`.

So a request can never observe a partially updated rule set or partially
updated group memberships: it sees the old snapshot or the new one.

### Change detection

| Trigger | When |
|---|---|
| Administration write | Immediately after persisting (forced reload). |
| `POST /admin/api/refresh` | On demand (forced reload). |
| Polling | Every `refresh_interval_secs` (default 30 s). |

Polling asks each repository for a fingerprint first:

- **Files:** mtime plus size, so an unchanged file costs one `stat`.
- **Memory:** a write counter.
- **PostgreSQL:** none, because `updated_at` is not reliable when rows are
  edited by hand without a trigger. PostgreSQL definitions are therefore
  re-read and compared on every poll, which is cheap at rule-table sizes.

Fingerprints are taken *before* reading, so a write that races with a read is
caught by the next poll rather than lost.

## Concurrency and resource bounds

- **I/O threads** (tokio, `server.io_threads`, default min(4, CPUs)) handle
  HTTP and data source I/O. They never evaluate rules.
- **Worker threads** (`worker_threads`, default CPU count) evaluate rules. A
  check is split into chunks of at most 256 records, and up to `threads`
  tickets per check claim chunks through an atomic cursor. A large batch
  spreads across the pool; a small one costs a single task. Per-rule
  statistics are accumulated per worker and flushed once per chunk, which keeps
  shared counters out of the hot loop.
- **Admission** (`max_queued_checks`) bounds the checks that are pending or
  running, sync and async alike. Excess requests get 503 immediately rather
  than queueing without limit, which also bounds the memory held by async
  request bodies.
- **Deadlines:**
  - `check_timeout_ms` is checked between records and passed to Starlark;
  - `rule_timeout_ms` interrupts a Starlark evaluation;
  - dropping a sync request (client disconnect) cancels its check.
- **Starlark** evaluates all of a record's Starlark rules on one evaluator,
  because creating one allocates a 1 MB scratch stack. Isolation is kept per rule:
  - deadlines are re-armed before each call;
  - ticks are charged per call, against a cap that leaves every rule its full
    budget;
  - after any error, the remaining rules run on a fresh evaluator;
  - a rule that hits the heap limit is retried once on a fresh heap before it
    is blamed, so garbage from earlier rules does not count against it.

  `tests` in `lang/starlark.rs` pin these guarantees down.
- **CEL** is not interruptible. It is non-Turing-complete, so evaluation time is
  bounded by the expression and the record size; the record size is bounded by
  `max_body_bytes`.

## Executions

| Mode | Writes | Visible to `GET` |
|---|---|---|
| Async | insert `pending` (before 202) → update `running` → update `completed`/`failed` | from the 202 onward |
| Sync | one insert of the terminal state | after the response (the id is unknown before) |

- **Unique ids.** Ids are UUIDv7: unique, time-ordered (good B-tree locality)
  and unguessable enough for a resource URL. `insert` never overwrites: the
  PostgreSQL primary key, an exclusive hard-link for files, a map check in
  memory. Each execution is updated only by the task that owns it, so
  concurrent checks cannot overwrite each other.
- **Retries.** Background updates retry three times (0.1 s, 0.5 s, 2 s). After
  that the failure is logged and counted.
- **Recovery.** At startup, every `pending` or `running` execution is marked
  `failed` with code `interrupted`. Request records are never persisted (size,
  privacy), so interrupted checks cannot be resumed.
- **Retention.** A background task deletes terminal executions older than
  `execution_retention_secs` every 10 minutes. The file backend skips files
  modified after the cutoff without parsing them, because a file is written at
  or after its execution's finish time.

## Data sources

- `Repository<T>` is generic over `Definition` (`Rule`, `RuleGroup`,
  `TestCase`, `ConfigEntry`), with typed aliases per category.
- `Stores::open` builds one repository per category from `DataSourcesConfig`,
  sharing locks between categories that use the same file and pools between
  those that use the same database URL.
- **Files.** A document maps section names to lists. Writes re-read the whole
  document under the lock, replace one section and write a temporary file,
  then fsync and rename it, preserving the other sections. Parse errors name
  the file, section and item index; duplicate keys are rejected.
- **PostgreSQL.** Real columns per field (tags as `TEXT[]`, JSON values as
  `JSONB`) so operators can edit with SQL. Memberships live in a child table
  with `ON DELETE CASCADE`. Missing tables are created when
  `auto_migrate: true`. Table names are validated identifiers; values are
  always bound parameters.

## Error model

`error::Error` has one variant per category from the specification, each with a
stable `code` and HTTP status. See [api.md](api.md#errors) for the full table.
Three distinctions run through the whole API:

- **Rule failure vs rule error:** `RuleStatus::Fail` vs `RuleStatus::Error`
  with a `RuleErrorKind`.
- **Validation outcome vs execution status:** `Outcome` (`valid`, `invalid`,
  `error`) vs `ExecutionStatus` (`pending`, `running`, `completed`, `failed`).
- **Test status vs evaluation outcome:** `TestStatus` (`passed`, `failed`,
  `error`) vs the evaluated `Outcome`.

The main API redacts data source and internal detail
(`Error::public_message`); the administration API returns it.

## Security

- **Rule sandboxing.**
  - **CEL:** only `record` and `config` are resolvable, and there is no I/O.
  - **Starlark:** `load()` is disabled in the dialect, and the globals are a
    side-effect-free subset of the standard library (no `breakpoint`,
    `debug` or internals). `print` is redirected to the debug log, and inputs
    are frozen. Both languages run with execution limits.
- **Starlark `unsafe`.** Two small blocks, each with a `SAFETY` comment:
  allocating JSON into a dedicated frozen heap, and reading frozen values
  whose owners provably outlive the evaluation. The second avoids a shared
  `Arc` clone per call, which measured as cross-core contention.
- **Credentials.** Bearer tokens are compared in constant time. The
  administration UI keeps the token in `sessionStorage` and sends it as a
  header, never as a cookie, so there is no CSRF surface.
- **UI hardening.** Server values are rendered with `textContent`, never
  HTML. The UI is served with a strict CSP (`default-src 'self'`, no inline
  scripts or styles), `X-Frame-Options: DENY`, `nosniff` and `no-referrer`.
- **Out of scope:**
  - TLS, which is expected from a reverse proxy (PostgreSQL TLS is available
    via the `postgres-tls` feature);
  - rate limiting;
  - multiple administrator roles or audit trails of administrative changes;
  - an OS-level sandbox (separate process with rlimits) for untrusted rule
    authors. The language-level limits are best effort, as the Starlark
    runtime itself documents.

## Performance notes

These findings come from `benches/engine.rs` on an Apple M1:

- **Dominant per-record costs** are record conversion and setup, about
  0.8–1.6 µs per record with trivial rules.
- **CEL field access** costs about 0.4 µs. Most of that is the `cel` crate's
  qualified-name resolution, which is upstream behavior.
- **Allocator.** The system allocator limited parallel scaling. `mimalloc`
  (default feature, binary only) improved pool throughput 1.7–3×.
- **Experiments that made things worse** and were reverted:
  - freeing request records on the workers: cross-thread frees made the
    allocator contend;
  - creating a Starlark evaluator per rule: the 1 MB scratch allocation
    dominated.
- **Compilation** of 500 rules takes about 10–16 ms. It happens only when
  rules change, and only for the rules that changed.

## Decisions and assumptions

These are the points where the specification was open, and what Drover does.

### Configuration

1. **Two configuration layers.** Data source definitions, credentials and the
   listen address cannot live in a data source that has not been opened yet,
   so they are in a bootstrap file. Everything else (`system` entries) lives in
   the configuration data source.
2. **Unique keys and two scopes.** Configuration keys are unique across scopes,
   and the scopes are `rule` and `system`. Unknown `system` keys are rejected
   on write but only warned about on load, so a shared data source can carry
   settings for other Drover versions.
3. **Hot vs restart settings.** Each setting is either *hot* (applied on the
   next refresh) or *restart*. Resizing the worker pool or the admission limit
   at runtime was judged not worth the complexity.

### Rules

4. **Rule result contract.** A boolean, or `{pass: bool, message?: string}`.
   Starlark `None` gets a specific "missing return" diagnostic.
5. **Starlark entry point.** `check(record, config)` with two positional
   parameters. Defining a function lets Drover compile once and call per
   record; module-level code runs only at compile time.
6. **Records must be JSON objects.** An empty `records` array is valid and
   completes as `valid`.
7. **Outcome priority.** A record is `invalid` if any rule failed, even if
   another rule errored; a definite rejection is more useful to clients than
   "unknown". Checks aggregate their records the same way, and the summary
   keeps every count.
8. **Disabled rules.** They are not run by checks, including through groups,
   but rule-scoped tests still evaluate them, so a rule can be tested before
   it is enabled.
9. **Invalid stored rules.** They fail startup by default
   (`fail_on_invalid_rules: true`). A refresh never fails because of one: the
   rule reports `compile` errors instead. The administration API rejects them
   before persisting.
10. **Group memberships belong to groups.** A rule's group assignments are
    edits to group definitions, which may live in another data source, so
    rule and membership changes are not atomic together. A partial failure is
    reported as `apply_failed` and names what was saved. Unknown members are
    ignored with a diagnostic, and deleting a rule removes it from its groups.

### Checks and executions

11. **Unknown group mode.** Defaults to `strict`. Strict rejection happens
    before an execution exists, so no execution is persisted for rejected
    requests (the same applies to every 4xx).
12. **`failed` means execution-level failure only:** timeout, worker failure,
    or interruption by a restart. Validation failures and rule errors still
    produce a `completed` execution.
13. **Sync executions** are persisted once, at the end. A sync check whose
    execution cannot be persisted fails with `execution_persistence_failed`,
    even though the results were computed, because the specification requires
    every execution to be persisted.
14. **Execution inputs** are not stored. Results reference records by index.
15. **`rules_version`** is a per-process counter. It identifies snapshots
    within one process lifetime, not across restarts.

### Tests

16. **Test runs** are synchronous, run on the worker pool and keep the latest
    result per test in memory. The specification lists test definitions, not
    results, as persistent data.
17. **Tests never use fallback mode.** A test naming an unknown group is a
    test `error`.
18. **Startup test policy** defaults to `warn`: failing tests are logged and
    reported in `/admin/api/status`, but do not keep the service down.

### Other

19. **Admission rejects rather than queues** beyond `max_queued_checks`, with
    503 and `Retry-After`.
20. **File rewrites** do not preserve comments or formatting beyond field
    order and block-scalar sources. Use PostgreSQL, or treat files as
    Drover-owned, when that matters.
21. **Reserved identifiers.** `evaluate` (rules) and `run` (tests), because
    they collide with administration routes.
22. **HTTP.** HTTP/1.1 only, with TLS termination expected in front of Drover.
