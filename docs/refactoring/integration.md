# Monoengine integration tests

This document describes the active integration-test contract. Compose service
registration and lifecycle rules live in [`test-infra.md`](./test-infra.md).

## Test stack

`docker-compose.test.yml` provides PostgreSQL, Redis, RustFS, optional
profiled `git-cli`, profile `app` monoengine, and profile `web` website-next.
Use the fixed project name:

```bash
docker compose -p monoengine-it -f docker-compose.test.yml up -d --wait
```

The embedded `VaultCore` is part of monoengine; no external Vault container is
used. Database schemas must be created through the project's migrations.

Mailpit remains an optional capture service for website authentication and
website product-email IT only. Monoengine never connects to it, and neither
the default integration suite nor CI requires a monoengine SMTP success path.

## Active integration targets

| Target | Purpose | Prerequisites |
| --- | --- | --- |
| `integration_vault` | CLI config, Vault bootstrap, redaction, and HTTP smoke | PostgreSQL and Redis |
| `integration_git_cli` | Git HTTP protocol round trips | PostgreSQL, Redis, `--profile git` |
| `integration_website_auth` | Better Auth cookie to monoengine session bridge | `--profile app --profile web`, `WEBSITE_IT=1` |
| `integration_website_mail` | Website internal product-email API acceptance (Bearer + allowlisted event → 202; bad bearer → 401) | `--profile app --profile web`, `WEBSITE_IT=1`, website tip with internal mail route |

Run the normal project gate with the test environment loaded:

```bash
source .env.test && cargo test --all
```

For the real website-session and internal-mail checks:

```bash
docker compose -p monoengine-it -f docker-compose.test.yml \
  --profile app --profile web up -d --wait
source .env.test
WEBSITE_IT=1 cargo test -p monoengine --test integration_website_auth -- --test-threads=1
WEBSITE_IT=1 cargo test -p monoengine --test integration_website_mail -- --test-threads=1
```

When `WEBSITE_IT=1` is set, unavailable monoengine or website-next endpoints
are failures, not passing skips.

## Notification and product-email coverage

Monoengine tests cover trigger selection, user preferences, in-app delivery,
optional Slack/webhook handling, and the website-mail client’s request/error
behavior. The website owns product-email rendering and delivery.

Do not add or retain tests that:

- seed or query `email_jobs`;
- set `[mail]`, `mail.password`, or SMTP endpoint configuration;
- require `SmtpMailer` to deliver to Mailpit;
- treat Mailpit availability as a monoengine startup gate.

Website email capture, if needed, belongs to website-next’s test provider
configuration and may use the Compose `mailpit` service. See
[`website-mail.md`](./website-mail.md).

## CI

`.github/workflows/config-validation.yml` runs formatting, Clippy, the
compose-backed integration targets (including `integration_website_auth` and
`integration_website_mail` under `WEBSITE_IT=1`), and the real website session
check after checking out the `orbit` and `website` siblings. Product email is
proven via the website internal API + `EMAIL_PROVIDER=test`; no local SMTP
dependency is required for monoengine notification paths.
