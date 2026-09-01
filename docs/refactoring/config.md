# Configuration

`config/config.toml` is the canonical sample configuration. Validate a selected
base/profile/environment combination before starting a service:

```bash
cargo run -p monoengine -- --config config/config.toml config validate
```

Configuration source diagnostics are available with `--show-sources`; use
`--deny-warnings` in automation that must reject ignored or deprecated input.
Secrets are supplied by deployment configuration or supported Vault
`SecretRef`s and must never be committed.

## Cedar authorization enforcement

The `[cedar]` section controls the authorization enforcement switch (ADR-UN-01):

```toml
[cedar]
enforcement = "off"   # off | shadow | enforce
```

- `off` (default): do not build or consume authorization data; zero behavior
  change.
- `shadow`: build the store, evaluate, record would-deny logs, but do not
  change allow decisions.
- `enforce`: build the store, evaluate, and deny unauthorized requests.

`config validate` rejects any other `enforcement` value, and rejects a
`monorepo.admin` entry equal to the reserved anonymous principal
`User::"__anonymous__"` (ADR-UN-06 ⑤). Source diagnostics for `cedar.enforcement`
are available via `config validate --show-sources --format json` (the JSON
output includes the field's winning source).


## Website authentication and product email

Browser sessions are validated against website Better Auth. The `[oauth]`
section supplies the website API base URL, accepted session-cookie names, and
the CORS allow-list. The complete trust boundary is documented in
[`website-auth.md`](./website-auth.md).

Product-email delivery is delegated to the website. Monoengine configuration
contains the website email API base URL and bearer/SecretRef used by the
notification client; see [`website-mail.md`](./website-mail.md) for the
request contract and Compose values.

There is no `[mail]` section, `MailConfig`, `mail.password` SecretRef, or
`MEGA_MAIL__*` environment override. These removed inputs are rejected rather
than silently ignored.

## Notification settings

Notification configuration controls monoengine-owned delivery such as in-app,
Slack, and webhook notifications. The `email` preference requests delivery
through the website API; it does not configure a local mail provider. Refer to
[`notification.md`](./notification.md) for behavior and test boundaries.

## Snowflake worker identity

The service selects the Snowflake worker ID before the first ID is generated:

- A valid MEGA_ID_GENERATOR_WORKER_ID value in 0..=255 takes precedence.
- Otherwise, startup claims the first available
  mega:snowflake:worker:<id> Redis slot with SET NX PX for 30 seconds and
  refreshes it every 15 seconds using an ownership token.
- If Redis is unavailable or all 256 slots are occupied, startup records the
  stable FNV-1a hash of POD_UID, then HOSTNAME, or the local fallback identity
  for diagnostics, but refuses ID writes until an exclusive worker ID is
  configured. The hash is never advertised as a uniqueness guarantee.

The generator uses 8 worker bits and 8 sequence bits: this preserves 256 IDs
per millisecond per worker and expands the worker space from 64 to 256. The
two additional low bits used by the worker layout reduce the timestamp horizon
by a factor of four, so old writers must be stopped before switching layouts.
Production startup requires `MEGA_ID_GENERATOR_LAYOUT_VERSION=8+8-v1`; set it
only after all 6+8 writers are stopped. If that maintenance window cannot be
guaranteed, keep the old release running and do not start the new layout. A
different or missing marker fails closed. Worker source and a non-reversible
process-identity digest are emitted in structured logs; Redis URLs, lease
tokens, and pod secrets are not logged.

## Test configuration

`.env.test.example` documents public test endpoints for PostgreSQL, Redis, and
RustFS. Its `MAILPIT_*` values are optional website-next SMTP-capture inputs;
monoengine neither reads them nor sends SMTP. The Compose topology and service
profiles are documented in [`test-infra.md`](./test-infra.md).
