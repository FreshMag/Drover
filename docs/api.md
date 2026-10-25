# Drover HTTP API

All bodies are JSON (UTF-8). When `auth.api_tokens` is configured, the main API
requires `Authorization: Bearer <token>`. The administration API always requires
`Authorization: Bearer <auth.admin_token>`.

- [Errors](#errors)
- [Check](#post-apiv1check)
- [Executions](#get-apiv1executionsid)
- [Test runs](#post-apiv1testsrun)
- [Health and metrics](#health-and-metrics)
- [Administration API](#administration-api)

## Errors

Every error uses the same shape:

```json
{"error": {"code": "unknown_rule_group", "message": "rule group 'x' does not exist", "details": {"group": "x"}}}
```

`details` is optional. The main API omits storage and internal detail from
`message`; the administration API includes it.

| Code | HTTP | Meaning |
|---|---|---|
| `invalid_request` | 400 | Invalid parameters (query string, ids, limits such as `max_records_per_check`). |
| `invalid_json` | 400 | Malformed JSON, wrong body shape, unknown fields, a record that is not an object. |
| `unauthorized` | 401 | Missing or wrong bearer token (`WWW-Authenticate: Bearer`). |
| `forbidden` | 403 | Administration API disabled (no `auth.admin_token`). |
| `not_found` | 404 | Unknown execution, rule, group, test, configuration key or route. |
| `conflict` | 409 | Creating something that already exists. |
| `payload_too_large` | 413 | Body larger than `server.max_body_bytes`. |
| `unknown_rule_group` | 422 | Check names a nonexistent group and `unknown_group_mode` is `strict`. |
| `invalid_definition` | 422 | Invalid rule, group or test definition (identifier, unknown language, unknown references). |
| `rule_compilation_failed` | 422 | The rule source does not compile (`details.compile_error`). |
| `configuration_invalid` | 422 | Invalid configuration entry or setting value. |
| `apply_failed` | 500 | The change was saved but the running instance could not reload it. The next successful refresh applies it. |
| `internal_error` | 500 | Unexpected failure. |
| `data_source_error` | 503 | A data source could not be read or written (`details.data_source`). |
| `execution_scheduling_failed` | 503 | Too many checks in progress (`max_queued_checks`), or Drover is shutting down. `Retry-After: 1`. |
| `execution_persistence_failed` | 503 | The execution record could not be persisted (`details.execution_id`). |
| `not_ready` | 503 | Startup has not completed. `Retry-After: 1`. |
| `execution_timeout` | 504 | A synchronous check exceeded `check_timeout_ms` (`details.execution_id`). |

**Validation results are not errors.** A rule that returns `false` is reported
as a failure inside a `200` response, and so is a rule that raises an error.
Error codes are reserved for requests that could not be processed.

## `POST /api/v1/check`

The body is an object:

```json
{
  "records": [{"age": 30}, {"age": 12}],
  "group": "onboarding",
  "mode": "sync",
  "report": "all"
}
```

The body can also be a bare array of records, with the options passed as query
parameters (`?group=onboarding&mode=async&report=failures`). Giving the same
option in both places is a 400.

| Field | Default | Values |
|---|---|---|
| `records` | required | Array of JSON objects (may be empty). At most `max_records_per_check`. |
| `group` | none | Group whose rules run. Without it, all enabled rules run. |
| `mode` | `sync` | `sync` waits for the result; `async` returns immediately. |
| `report` | `all` | `all`: every rule outcome. `failures`: only failed and errored outcomes (every record is still listed). `summary`: no `results`. |

### Synchronous response: `200 OK`

```json
{
  "execution_id": "01a1729b-a410-72bd-90d9-fcda05983e3f",
  "status": "completed",
  "mode": "sync",
  "outcome": "invalid",
  "selection": {"requested_group": "onboarding", "group": "onboarding", "rule_count": 3, "rules_version": 4},
  "report": "failures",
  "record_count": 2,
  "created_at": "2026-10-25T10:29:25.136064Z",
  "started_at": "2026-10-25T10:29:25.136064Z",
  "finished_at": "2026-10-25T10:29:25.136245Z",
  "duration_ms": 0.156,
  "summary": {"records": 2, "valid": 1, "invalid": 1, "errored": 0,
              "rule_evaluations": 6, "rule_failures": 3, "rule_errors": 0},
  "results": [
    {"index": 0, "outcome": "valid", "rules": []},
    {"index": 1, "outcome": "invalid", "rules": [
      {"rule": "customer.adult", "status": "fail", "message": "customer is under the minimum age"},
      {"rule": "customer.email", "status": "error", "error": {"kind": "runtime", "message": "..."}}
    ]}
  ],
  "links": {"self": "/api/v1/executions/01a1729b-a410-72bd-90d9-fcda05983e3f"}
}
```

Fields with no value are omitted; for example `diagnostics` appears only when
it has entries.

- **`status`** is the execution state: `pending`, `running`, `completed` or
  `failed`. A synchronous response is `completed`; failures are returned as
  errors (`execution_timeout`, `internal_error`) that carry `execution_id`.
- **`outcome`** is the validation result, present when `completed`:
  - a record is `invalid` if any rule failed, otherwise `error` if any rule
    errored, otherwise `valid`;
  - the check aggregates its records the same way.

  A check whose records are invalid is still `completed`.
- **Rule `status`** is one of:
  - `pass`;
  - `fail`, with `message` from the rule's result or its default message;
  - `error`, with `error.kind` set to `compile`, `runtime`, `timeout`,
    `resource_limit`, `invalid_result` or `internal`.
- **`rules`** appear in rule id order. `index` is the record's position in the request.
- **`selection.fallback: true`** and a `diagnostics` entry indicate that the
  requested group did not exist and `unknown_group_mode` is `fallback`.
- **`selection.rules_version`** identifies the rule snapshot used. It
  increases with every applied change and restarts at 1 when the process starts.

### Asynchronous response: `202 Accepted`

```http
HTTP/1.1 202 Accepted
Location: /api/v1/executions/01a1729b-a48c-75a1-bc27-efba0d9418ca
Retry-After: 1
```

```json
{"execution_id": "01a1729b-a48c-75a1-bc27-efba0d9418ca", "status": "pending", "mode": "async",
 "selection": {...}, "report": "all", "record_count": 2, "created_at": "...",
 "links": {"self": "/api/v1/executions/01a1729b-a48c-75a1-bc27-efba0d9418ca"}}
```

Poll `Location` until `status` is `completed` or `failed`.

## `GET /api/v1/executions/{id}`

This returns the execution in the same shape as the synchronous response.

- **Still running:** `pending` or `running`, without `results`, and with `Retry-After: 1`.
- **Failed:** an `error` object such as `{"code": "execution_timeout", "message": "..."}`.
  Executions interrupted by a restart fail with code `interrupted`.
- **Unknown id, or purged after `execution_retention_secs`:** 404.

| Mode | When the execution is persisted |
|---|---|
| Async | `pending` before the 202 is returned, then `running`, then the terminal state. |
| Sync | Once, in its terminal state, before the response is sent. If the client disconnects first, the check is cancelled and not stored. |

## `POST /api/v1/tests/run`

The body is optional:

```json
{"ids": ["onboarding.valid-customer"], "tags": ["smoke"]}
```

This runs the tests matching any listed id or tag; an empty body runs all tests.
The run is synchronous.

```json
{
  "total": 5, "passed": 4, "failed": 1, "errors": 0, "duration_ms": 1.6, "rules_version": 4,
  "results": [{
    "id": "onboarding.minor-rejected",
    "status": "failed",
    "scope": {"type": "group", "group": "onboarding"},
    "expected": {"outcome": "invalid", "rules": ["customer.adult"], "exclusive": true},
    "actual": {"outcome": "invalid", "failed_rules": ["customer.adult", "customer.name"], "errored_rules": []},
    "diagnostics": ["expected only [customer.adult] to fail, but these did too: customer.name"],
    "rules": [{"rule": "customer.adult", "status": "fail", "message": "..."}, ...],
    "duration_ms": 0.05, "rules_version": 4, "ran_at": "..."
  }]
}
```

The test `status` field takes one of these values:

| `status` | Meaning |
|---|---|
| `passed` | The expectation was met. |
| `failed` | The evaluation contradicted the expectation; `diagnostics` explain how. |
| `error` | The test could not be evaluated as written: unknown group or rule, or an expected rule outside the scope. |

Unknown ids return 400.

## Health and metrics

| Endpoint | Auth | Response |
|---|---|---|
| `GET /health/live` | none | `200 {"status": "live"}` while the process runs. |
| `GET /health/ready` | none | `200 {"status": "ready", "rules_version", "rules", "invalid_rules", "groups"}`, or `503 {"status": "starting" \| "failed" \| "stopping"}`. |
| `GET /metrics` | API token | Prometheus text format. |

## Administration API

Every administration endpoint is under `/admin/api` and requires the admin
token.

| Method and path | Description |
|---|---|
| `GET /status` | Snapshot version, invalid rules, diagnostics, effective settings, refresh status, pool, data sources, startup report. |
| `POST /refresh` | Reload from the data sources now. Returns `{"changed", "version"}`. |
| `GET /languages` | Languages with editor template and help. |
| `GET /settings` | Known system settings: type, bounds (`max: null` means unbounded), default, apply mode. |
| `GET /rules`, `GET /rules/{id}` | Rules with `groups`, `state` (`active`, `disabled`, `invalid`, `pending`), `compile_error`, `stats`. |
| `POST /rules`, `PUT /rules/{id}` | Body: `{id, language, source, description?, message?, enabled?, metadata?, groups?}`. When `groups` is present, the rule's memberships become exactly that list; when absent, they are unchanged. Ids cannot be renamed. |
| `DELETE /rules/{id}` | Also removes the rule from its groups. `warnings` lists tests that reference it. |
| `POST /rules/evaluate` | `{language, source, records}`: compile and evaluate a draft without saving. Returns `{compiled, compile_error?, results: [{index, status, message?, error?}]}`. |
| `GET/POST /groups`, `GET/PUT/DELETE /groups/{name}` | Body: `{name, description?, rules: [ids]}`. Unknown rule ids are rejected; duplicates are removed. |
| `PUT/DELETE /groups/{name}/rules/{rule}` | Add or remove one member. |
| `GET/POST /tests`, `GET/PUT/DELETE /tests/{id}` | Test definitions; `latest` holds the most recent result. Scope references and expected rules are validated. |
| `POST /tests/run`, `POST /tests/{id}/run` | Run tests, same format as `/api/v1/tests/run`. |
| `GET/POST /config`, `GET/PUT/DELETE /config/{key}` | Configuration entries `{key, value, scope: "rule" \| "system", description?}`, returned with `exposed_to_rules` and `apply`. |

Successful changes return:

```json
{"data": {...}, "rules_version": 5, "warnings": ["..."], "restart_required": true}
```

`warnings` and `restart_required` appear only when they apply. Creating returns
201; everything else returns 200.

The ids `evaluate` (for rules) and `run` (for tests) are reserved, because they
collide with the routes above.
