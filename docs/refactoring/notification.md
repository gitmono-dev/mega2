# Notification

`monoengine` retains notification event selection, user preferences, and
non-email delivery such as in-app, Slack, and webhook notifications. It no
longer owns email rendering, queues, SMTP credentials, or SMTP delivery.

## Email delivery boundary

Product events that require email call the website internal notification API.
The website is the sole owner of templates, delivery providers, retries, and
SMTP configuration. The contract, authentication, and Compose topology are
defined in [`website-mail.md`](./website-mail.md). Stack IT coverage lives in
`bin/tests/integration_website_mail.rs` (`WEBSITE_IT=1`).

Consequently, monoengine has no:

- `[mail]` configuration or `mail.password` SecretRef;
- `SmtpMailer`, `lettre`, `EmailDispatcher`, or `email_jobs` outbox;
- Mailpit success-path requirement;
- admin email-job or mail-template API.

`notification.website_mail_base_url` and the corresponding bearer setting are
the only monoengine configuration for product-email delivery. The client is
best-effort: a website failure is logged without failing the originating CL or
issue request, and does not disable in-app, Slack, or webhook delivery.

## Preferences and delivery

The `email` delivery preference means “request website email delivery” and
does not reintroduce a local email channel. Existing event triggers continue
to enforce notification preferences before emitting in-app and optional
external-channel notifications.

Chat notification events are removed with the chat product surface and are not
forwarded to the website.

## Testing

Test notification triggers with focused Rust tests, the website-mail client
wire mock, and stack IT (`integration_website_mail` under `WEBSITE_IT=1`). Do
not add tests that seed `email_jobs`, configure an SMTP server, or assert
monoengine-to-Mailpit delivery. Mailpit, when started by the
Compose stack, is exclusively a website authentication/product-email capture
service.

See also [`integration.md`](./integration.md) for the active integration
matrix and [`test-infra.md`](./test-infra.md) for Compose service ownership.
