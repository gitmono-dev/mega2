# Notification

`mega2` retains notification event selection, user preferences
(`enabled` plus per-event toggles), and a single outbound channel:
generic webhook under `[notification.webhook]`. Unrelated
`[storage_events]` is not part of this surface.

It does not own email rendering, queues, SMTP, Slack, or an in-app
inbox. Product email, if any, lives in website frontend / website. The retired
client contract is the tombstone in [`website-mail.md`](./website-mail.md).

## Outbound boundary

The only notification delivery configuration is `[notification.webhook]`.
When no webhook target is configured, the notification surface is silent
(expected). A webhook failure is logged and does not fail the originating
CL or issue request.

Consequently, mega2 has no:

- `[mail]` configuration or `mail.password` SecretRef;
- `SmtpMailer`, `lettre`, `EmailDispatcher`, or `email_jobs` outbox;
- Slack channel or `[notification.slack]`;
- in-app inbox writer or `user_inbox_notifications`;
- `website_mail_*` client keys;
- Mailpit success-path requirement;
- admin email-job or mail-template API.

## Preferences

`GET|PUT /notification/preferences` keeps `enabled` and per-event
selection. There is no `delivery_mode`, `settings.email`, or
`preferred_locale`. Existing event triggers still enforce those
preferences before emitting a webhook.

Chat notification events were removed with the chat product surface.

## Testing

Test notification triggers and webhook delivery with focused Rust tests.
Do not add tests that seed `email_jobs`, configure an SMTP server, or
assert mega2-to-SMTP capture delivery.

See also [`integration.md`](./integration.md) for the active integration
matrix and [`test-infra.md`](./test-infra.md) for Compose service ownership.
