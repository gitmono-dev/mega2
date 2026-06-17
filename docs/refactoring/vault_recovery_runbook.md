# Vault recovery runbook

This runbook documents the operational behavior introduced by
`docs/refactoring/vault.md` phase A/J hardening.

## Scope

- `core_key.json` contains the local auto-unseal key material for the embedded
  libvault instance.
- The database `vault` table contains encrypted vault data.
- Service startup is fail-closed when the database is initialized but
  `core_key.json` is missing. Startup must not delete vault rows.

## Normal backup

1. Back up `mega_base()/vault/core_key.json` outside the application host.
2. Store the backup encrypted in an external secret manager, an offline
   encrypted store, or an equivalent restricted credential system.
3. Exclude `core_key.json` from container images, log collection, source
   control, support bundles, and ordinary filesystem snapshots unless those
   snapshots are encrypted and access controlled.
4. On Unix, keep the vault directory mode at `0700` and `core_key.json` at
   `0600`.

## Restore when DB data exists but key file is missing

1. Stop monoengine.
2. Restore the matching `core_key.json` for the same database backup or live
   database.
3. Set permissions:

   ```bash
   chmod 700 "$(dirname "$CORE_KEY_PATH")"
   chmod 600 "$CORE_KEY_PATH"
   ```

4. Start monoengine.
5. Confirm vault-backed consumers can read their secrets.

If no matching key material exists, the encrypted vault data cannot be
recovered with the current embedded libvault design. Do not start the service
expecting it to reinitialize automatically; it will fail closed.

## Explicit reset when recovery is not required

Use this only when losing all vault secrets is acceptable.

1. Stop monoengine.
2. Back up the database and any existing `core_key.json`.
3. Delete rows from the `vault` table or recreate the database in a controlled
   maintenance flow.
4. Remove the old `core_key.json`.
5. Start monoengine to perform a first initialization against an uninitialized
   vault store.
6. Recreate required secrets.

Ordinary service startup must never perform this reset implicitly.

## Regenerating unseal shares

`VaultCore::rekey_unseal_shares()` calls libvault
`generate_unseal_keys()` and rewrites `core_key.json` with a fresh Shamir share
set for the current KEK. This preserves vault data and is covered by unit
tests.

Current limitation: libvault re-splits the same KEK. Previously exported
Shamir share sets can still reconstruct that KEK, so this is not a full
compromise recovery if old shares were leaked. Full invalidation of old key
material requires a KEK rotation or external KMS/transit auto-unseal design,
which is outside the current vendored libvault primitives.

## Root token handling

The Rust application interface no longer exposes root token access to ordinary
callers, and secret operations go through the narrowed secret interface with an
audit hook. The current local auto-unseal file still contains root recovery
material for compatibility and recovery. Removing it safely requires a separate
root recovery token or external credential custody design; doing so without
that recovery path can make future maintenance impossible.
