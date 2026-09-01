# Fixed ClientHello fixtures

These are the complete first TLS records captured from independent client
implementations against a local TCP listener on 2026-09-01. The listener sent
no response. Random and ephemeral key-share bytes are intentionally frozen;
tests check parser compatibility and exact wire retention, not cryptographic
values.

- client_hello_openssl_3_6_3.rs: OpenSSL 3.6.3, invoked as
  openssl s_client -connect 127.0.0.1:PORT -servername fixture.example -brief.
  1546 bytes; SHA-256 228f135c07a4d5491653e229e5c73302f51b589dcbce17c3695aad0ac91ec78f.
- client_hello_apple_secure_transport.rs: Apple curl 8.7.1 / libcurl 8.7.1
  using SecureTransport, invoked with --noproxy '*' --resolve
  fixture.example:PORT:127.0.0.1 https://fixture.example:PORT/.
  325 bytes; SHA-256 6c801c49925112cd01849a1ea4a0983ef740fd3a7bd049af6a49dd5d809142ed.

The Rust files are expressions included only by unit tests. Regeneration is a
reviewed compatibility update because client defaults and fingerprints change
over time.
