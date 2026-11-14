# TLS in Atrium

Last updated 2026-11-14. Atrium uses one TLS stack everywhere: **rustls 0.23** with the
**`ring`** crypto provider. Nothing in the tree links OpenSSL, and new code should not
start.

## Where it is used

| crate | role | features |
|---|---|---|
| `navigator-fetch` | the Navigator's jailed fetcher (client) | `ring`, `std`, `tls12` |
| `arca/arca-courier` | Arca's courier: the only process that sends secrets over the network (arca.md §5) | `ring`, `std`, `tls12` |
| `capsa/capsa-share` | `atrium-share-webdav`, the WebDAV share server (network-shares.md S2): WebPKI against `/etc/ssl/cert.pem`, or a self-signed server pinned by the SHA-256 of its public key, chosen by the user in Scrinium | `ring`, `std`, `tls12` |
| `tabellarius-relay` | relay server | `ring`, `std` |
| `tabellarius-macos` | macOS client | `ring`, `std` |

All of them build `rustls::ClientConfig::builder_with_provider(ring)` (or the server
equivalent) with `with_safe_default_protocol_versions()`. Clients load roots from a PEM
trust store before entering capability mode (`navigator_fetch::tls_config`); the courier
uses the same function.

## Why rustls and ring

- **Memory-safe protocol code.** The parser and state machine are safe Rust, which matters
  most in the courier and the fetcher, since both parse bytes from hostile servers.
- **No legacy surface.** Only TLS 1.2 and 1.3. There is no SSLv3, RC4, export ciphers,
  CBC-mode suites or renegotiation. TLS 1.2 is enabled (`tls12`) only because some servers
  still need it, and it uses the AEAD suites only.
- **Audited.** Cure53 audited rustls, and webpki verifies certificates.
- **ring** is small and builds cleanly for our `aarch64-unknown-freebsd` cross target. Its
  primitives come from BoringSSL (constant-time, assembly where it matters).

### Alternative not taken: aws-lc-rs

rustls's other provider, `aws-lc-rs`, is FIPS-validated and is rustls's upstream default.
It needs a C/cmake build of AWS-LC, which complicates the FreeBSD cross-build and the
sysroot. We don't need FIPS. If a certification ever requires it, switch the provider in
one place per crate. The rustls API is the same.

## Gaps: future work

None of these block current work. They are listed so later threads pick them up.

1. **Revocation.** rustls doesn't check whether a certificate has been revoked: there is no
   OCSP and no CRL fetching. webpki can check CRLs we supply
   (`WebPkiServerVerifier::builder().with_crls`). Plan: a system service that fetches and
   refreshes CRLs (or a CRLite-style filter) and hands them to the fetcher and the
   courier. Until then we rely on short certificate lifetimes, which are now common
   (Let's Encrypt: 90 days, heading to shorter).
2. **Encrypted Client Hello (ECH).** rustls supports ECH on the client side. We don't use
   it yet, so the server name is visible on the wire. Turning it on needs the HTTPS/SVCB
   DNS record, so it belongs with the fetcher's resolver work.
3. **Certificate pinning for Arca recipients.** arca.md §3 allows "an optional pinned key"
   per endpoint item. The courier's verifier should accept a pin set from the item's
   recipient and refuse on mismatch, on top of the WebPKI check.
4. **Trust store.** Each process loads a PEM bundle. Atrium needs one system root store
   with updates (and a way for the user to add a root, shown in the trusted UI). Today
   the path is per service (`/etc/ssl/cert.pem` on FreeBSD).
5. **Key material in memory.** rustls keeps session keys and plaintext buffers in ordinary
   heap memory, which is not mlocked. The courier holds a secret only for one request,
   inside a process with core dumps and tracing off (arca.md §6.3). Locked allocations
   for rustls's buffers would need an allocator hook, so it's worth it only if a threat
   needs it.
6. **Post-quantum key exchange.** rustls offers the X25519MLKEM768 hybrid with
   aws-lc-rs (and in newer `ring`/rustls releases). Enable it when it's available on our
   provider.
7. **Session resumption** for the courier's per-endpoint reuse (arca.md §5.2): the default
   in-memory ticket cache is fine. Never persist it to disk.
