# R25 patch-coordinate method evidence

- Initial source plan SHA: `ea0d3d5d2efe9d722c3f4defc8d7347503e2df47`; checkpoint SHA: `81f2e5ce1b26177f0b0956504b17129c8a5e2cec`; first-run hunk-manifest SHA-256: `536a0e19bc736af89096e264c1773be0511a8285799597f29ca2da81bd959903`.
- Initial FIX-OX-08 A/B run: `FAIL` at A exact Docker file hash. Patch commands returned without offset/fuzz, but Apple patch placed zero-context reverse deletions at the wrong boundary. Expected SHA-256 `5f5af6405e95bbc961cce6aaeb231561c2ed933ceac9f02ff9138592e027472b`; actual `3d01eaf62d77e48fdfd9c36985cb38e16c2750a7ff8eb58cb3a96041a581a366`.
- The failed run made 34 patch invocations before stopping; raw stdout/stderr/exit files are archived in `failed-attempt/raw-results.tar.gz`.
- `apple-patch-preflight.json` and its script record the corrected seven-case Apple patch fixture. Canonical source hunks and Apple reverse-application headers are separate; the execution manifest will be regenerated after R25 receives Claude PASS.
