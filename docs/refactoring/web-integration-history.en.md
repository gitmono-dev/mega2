# Website Integration Stack Changes

English · [中文](web-integration-history.md)

Date: 2026-09-04
Scope: the `web` profile in `docker/docker-compose.test.yml`; existing `website-*` service names are retained.

## Why the `website-*` names remain

The mega2 configuration and integration-test contracts use `MEGA_OAUTH__WEBSITE_*`, `MEGA_NOTIFICATION__WEBSITE_*`, and the `website-next` / `website-db-init` service names (see the README integration-stack naming contract and ADR-WA-07). DEP-01 changes only the build context and image contents. It leaves service names, the `website` database name, and configuration keys unchanged to avoid disrupting mega2 tests and configuration.

## Changes

| Item | Previous | Current |
|---|---|---|
| `website-next` / `website-db-init` build context | `../monoui` | Website application sibling checkout |
| Dockerfile | `apps/next-app/Dockerfile` | `apps/web/Dockerfile` |
| Targets | Default runner / `builder` | `runner` / `db-init` |
| Database initialization | `drizzle-kit push --force` | `drizzle-kit migrate` |
| Added service | — | `website-collab` (`17002` on the host to `7002` in the container) |
| Environment passed to `website-next` | — | `MEGA_COLLAB_*` / `NEXT_PUBLIC_MEGA_COLLAB_PUBLIC_URL` |
| Bucket | The default remains `monoui` (renaming was considered but not done; see ADR-ARCH-05) | Unchanged |

## Rationale

The website integration stack now builds its frontend from the website application source. The collaboration service runs separately as `website-collab`, which provides browser WebSocket and internal bridge traffic.
