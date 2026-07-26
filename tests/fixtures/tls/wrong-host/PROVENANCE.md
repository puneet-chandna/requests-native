# Wrong-host TLS fixture provenance

Purpose: a valid server-authentication leaf signed by the frozen Requests test
CA whose only SAN is `wrong.test`. It must fail hostname verification when a
test client connects to `localhost`.

This dedicated leaf is necessary because the frozen valid server leaf's SAN
covers all loopback names used by the suite: `localhost`, `127.0.0.1`, and
`::1`. The other frozen CA-signed server leaf expired on
2025-02-17 00:38:22 UTC, so it cannot isolate hostname verification from
validity failure. No pre-existing frozen leaf is simultaneously valid,
server-authentication-capable, signed by this CA, and mismatched for those
loopback names.

This fixture was generated once offline. Builds and tests must never run
OpenSSL or regenerate it.

## Tool and inputs

- OpenSSL: `OpenSSL 3.6.0 1 Oct 2025`
- historical ephemeral generation directory:
  `/tmp/requests-rust-tls-wrong-host.NL8NnA`
- issuer certificate:
  `tests/certs/expired/ca/ca.crt`
  - SHA-256:
    `1407cb9c2502bf453ffa3f54cb846022f161a46b73fe4caf436157d95ad93367`
  - validity: 2025-03-29 13:51:45 UTC through
    2045-03-24 13:51:45 UTC
  - certificate fingerprint:
    `68:9A:5B:BA:85:00:4B:77:C2:CD:F8:D0:DC:E9:46:43:E2:02:02:01:63:D6:A2:B3:F1:DE:E1:11:5D:44:C4:CE`
- issuer private key:
  `tests/certs/expired/ca/ca-private.key`
  - SHA-256:
    `9a36bafea981da2dcd9a218b7ce23c778acdbee0009c0720531dd4e4d0549548`

## Exact generation commands

Run from the temporary generation directory:

```bash
export REPO_ROOT=/path/to/requests-rewrite
openssl req -new -newkey rsa:2048 -nodes -config wrong-host.cnf -keyout wrong-host.key -out wrong-host.csr
openssl x509 -req -in wrong-host.csr -CA "$REPO_ROOT/tests/certs/expired/ca/ca.crt" -CAkey "$REPO_ROOT/tests/certs/expired/ca/ca-private.key" -set_serial 0x120000000000000000000000000000000000000B -days 6200 -sha256 -copy_extensions copy -out wrong-host.pem
```

The CSR was validation-only and is not checked in.

## Result

- serial:
  `120000000000000000000000000000000000000B`
- validity: 2026-07-26 07:54:52 UTC through
  2043-07-17 07:54:52 UTC
- subject: `C=US, ST=DE, O=Python Software Foundation,
  OU=python-requests, CN=wrong.test`
- issuer: `C=US, O=Python Software Foundation, OU=python-requests,
  CN=Self-Signed Root CA`
- SAN: critical `DNS:wrong.test` only
- basic constraints: critical `CA:FALSE`
- key usage: critical digital signature and key encipherment
- extended key usage: critical TLS Web Server Authentication
- certificate SHA-256 fingerprint:
  `5F:58:DC:AA:29:E6:95:60:3B:19:F5:89:B4:21:2E:14:94:3C:07:61:33:C3:10:79:D9:CC:4E:73:D3:F7:59:FC`

File SHA-256:

- `wrong-host.cnf`:
  `8c4439b9169f293842fa0777382ba65082ecbfc58c45d7eb8508afde7f71e594`
- `wrong-host.pem`:
  `4683302b234393ea541fa73597507ecdf18e259d52a91feffa0a337d189eee4c`
- `wrong-host.key`:
  `de1b85229c0f8380329b7e697a72ef6a0995f9de659febd48ab5a2dd5dd552f6`

Validation:

```bash
export REPO_ROOT=/path/to/requests-rewrite
openssl verify -CAfile "$REPO_ROOT/tests/certs/expired/ca/ca.crt" wrong-host.pem
openssl pkey -in wrong-host.key -check -noout
openssl x509 -in wrong-host.pem -noout -serial -dates -fingerprint -sha256 -subject -issuer
openssl x509 -in wrong-host.pem -noout -ext subjectAltName
openssl x509 -in wrong-host.pem -noout -ext basicConstraints
openssl x509 -in wrong-host.pem -noout -ext keyUsage
openssl x509 -in wrong-host.pem -noout -ext extendedKeyUsage
```

The certificate verified as `OK`, the key passed its integrity check, and the
leaf validity ends before the issuer.
