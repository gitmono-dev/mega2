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

## Monorepo initial object-ID format

`[monorepo].object_format` selects the object-ID algorithm used while an empty
Monorepo creates its initial commit, trees, and blobs. Its canonical values are
`sha1` (the default) and `sha256`; the parser also accepts `sha-1` and
`sha-256` for configuration compatibility. The setting is restart-required and
does not convert an existing repository.

`blake3` is intentionally recognized but rejected by `config validate` and by
the initializer until monoengine consumes the explicit BLAKE3 APIs planned for
`git-internal` 0.9.0. `black3` is not a valid spelling or value.

This is an initialization-only contract. Ceres still has SHA-1-only protocol
and pack paths, so `sha256` must not be presented as an end-to-end Git
clone/fetch/push format until the repository context, protocol, and pack work
are completed. Use it only for a controlled bootstrap invocation; do not start
a normal Git service with this setting. Run
`monoengine --config <path> service init --yes` to create the initial graph;
the command requires an existing config and exits without starting Git listeners
or normal service runtime dependencies.

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

## Push morphology and trunk authentication

`[monorepo].push_policy` is `"review"` (default, CL pipeline) or `"trunk"`
(MonoWriteQueue). `[monorepo].max_push_commits` (default 250) bounds first-parent
chain length in trunk morphology only; review morphology keeps the
`MAX_CL_CHAIN_COMMITS` constant. Both fields are restart-required.

`[git].push_auth` omitted keeps the existing OAuth / UserStorage chain (review
only). Explicit `"token"` or `"none"` requires `push_policy = "trunk"`; trunk
requires an explicit `push_auth`. `[[git.push_tokens]]` entries authorize by
component-boundary path prefix (`/foo` does not authorize `/foobar`); omitted
`paths` means the whole repository. Token values may be literals (IT/tests) or
`vault://secret/config/<profile>/git/push_tokens/<name>#<field>` SecretRefs.
File-mounted secrets use `${file:...}` and are expanded at load.

Trunk HTTP start additionally refuses open change lists, a `push_policy`
change while `push_queue` has non-terminal rows (`queue_control.last_policy`),
and resets `blob_paths.indexed_push_id` to NULL on a successful empty-queue
morphology switch.

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

## Test configuration

`.env.test.example` documents public test endpoints for PostgreSQL, Redis, and
RustFS. Its `MAILPIT_*` values are optional website-next SMTP-capture inputs;
monoengine neither reads them nor sends SMTP. The Compose topology and service
profiles are documented in [`test-infra.md`](./test-infra.md).
