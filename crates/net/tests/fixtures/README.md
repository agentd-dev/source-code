# TLS test fixtures — NOT secrets

Self-signed test-only PKI for the `net` TLS server/client round-trip tests.
Generated once with openssl (P-256, 25-year validity so CI never rots):

- `ca.pem` / `ca.key` — the throwaway test CA (kept so future fixtures can be
  minted from the same root).
- `server.pem` / `server.key` — server identity, SANs `localhost`, `127.0.0.1`,
  `::1`, `extendedKeyUsage = serverAuth`.
- `client.pem` / `client.key` — client (mTLS) identity, `clientAuth`.

These keys protect nothing: they are committed test data, valid only against
this CA, used exclusively on loopback in tests. Do not use outside tests.
