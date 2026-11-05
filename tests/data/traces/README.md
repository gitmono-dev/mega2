# Traces test data

`libra-h2-three-checkpoints.pack` is a SHA-1 PACK (33 objects) that a real
Libra client produced for the H2 handoff (plan-20261015 DEP-TR-H2). It was
captured on 2026-11-05 as follows:

1. A Libra live selector using `seed_prunable` (`live_clean_rewrite_denied`
   or `live_new_stream_epoch_created`, sessions `prune-a` and `prune-b`)
   seeded two temporary checkpoints and one committed checkpoint through
   `HistoryManager::append_checkpoint_commit`, then pushed them with
   `libra agent push --mega2` over HTTP to the local H1 Mega2 TraceRepo
   (`v0.42.95`, server `222a49cf-e2e2-468c-b471-3ca49c4f8a87`).
2. `libra mega2 traces fetch` read stream
   `ca3c7ad8-9196-4635-b248-81ee62fe02ac` from that server into an empty
   repository; this file is the received pack, byte for byte.

Tip commit `d9bf5910abfdcd70f6ba5ec0ec1c69ba453075f7` carries checkpoints
`1c6cc65e-f8bd-409a-8f0e-69fda086a5d0` (committed),
`b09030ec-0419-4fd9-861e-0252c2ae7a00` and
`d7cf0133-e2f6-425b-81e7-d44dc0540e02` under `checkpoint/<id[..2]>/<id[2..]>/`.
All transcript text is a synthetic sentinel; no credential or real session
content is present.
