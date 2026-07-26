# Long-lived mTLS client fixture provenance

Purpose: a deterministic client-authentication identity signed by the frozen
Requests test CA. The leaf subject has `CN=requests`; the configured chain is
leaf first, then issuer.

This fixture was generated once offline. Builds and tests must never run
OpenSSL or regenerate it.

## Tool and inputs

- OpenSSL: `OpenSSL 3.6.0 1 Oct 2025`
- historical ephemeral generation directory:
  `/tmp/requests-rust-mtls-client.ZjaUWl`
- issuer certificate:
  `tests/certs/expired/ca/ca.crt`
  - SHA-256:
    `1407cb9c2502bf453ffa3f54cb846022f161a46b73fe4caf436157d95ad93367`
  - DER SHA-256:
    `689a5bba85004b77c2cdf8d0dce94643e202020163d6a2b3f1dee1115d44c4ce`
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
openssl req -new -newkey rsa:2048 -nodes -config client.cnf -keyout client.key -out client.csr
openssl x509 -req -in client.csr -CA "$REPO_ROOT/tests/certs/expired/ca/ca.crt" -CAkey "$REPO_ROOT/tests/certs/expired/ca/ca-private.key" -set_serial 0x120000000000000000000000000000000000000D -days 6200 -sha256 -copy_extensions copy -out client.pem
cp client.pem client-chain.pem
cat "$REPO_ROOT/tests/certs/expired/ca/ca.crt" >> client-chain.pem
cp client-chain.pem client-combined.pem
cat client.key >> client-combined.pem
```

The CSR was validation-only and is not checked in.

## Result

- serial:
  `120000000000000000000000000000000000000D`
- validity: 2026-07-26 08:31:18 UTC through
  2043-07-17 08:31:18 UTC
- subject: `C=US, ST=DE, O=Python Software Foundation,
  OU=python-requests, CN=requests`
- issuer: `C=US, O=Python Software Foundation, OU=python-requests,
  CN=Self-Signed Root CA`
- basic constraints: critical `CA:FALSE`
- key usage: critical digital signature and key encipherment
- extended key usage: critical TLS Web Client Authentication
- certificate SHA-256 fingerprint:
  `4B:51:37:32:65:3E:F9:E3:B7:C2:ED:76:BF:DE:31:1B:56:0F:F6:B8:E6:0F:3B:50:CA:6A:C3:9D:4D:93:23:DB`
- certificate DER SHA-256:
  `4b513732653ef9e3b7c2ed76bfde311b560ff6b8e60f3b50ca6ac39d4d9323db`
- private-key encoding: unencrypted PKCS#8
- certificate/private-key RSA modulus SHA-256:
  `ede7f3fea08eac3b6c98d3602c8b0b634a6209173e4501a25066b0640c3a084d`

`client-chain.pem` contains exactly two certificates in this order:

1. client leaf (`CN=requests`);
2. frozen issuer (`CN=Self-Signed Root CA`).

`client-combined.pem` contains that same ordered two-certificate chain followed
by the one PKCS#8 private key.

File SHA-256:

- `client.cnf`:
  `9217615846760a31a51eaa25b8d8fbe85cfecbc97841aceea9852bf7badaee7f`
- `client.pem`:
  `558b9e3f87b31444c2d253eba2415559128b43282eea3fc83349f22db85e16ee`
- `client-chain.pem`:
  `22cecdcfa7590772fe66f78aaea6908d3a68311a7cd15755e30eb0529b58b001`
- `client.key`:
  `d78880595600e9e6d44285c39716aaad1fe569f329fd7c2151f847f1c6cfdb14`
- `client-combined.pem`:
  `24f2e77b259f7e2493f7de7aabf10a2b822f3e3cc52de477651e2ee3abe07c24`

Validation:

```bash
export REPO_ROOT=/path/to/requests-rewrite
openssl verify -purpose sslclient -CAfile "$REPO_ROOT/tests/certs/expired/ca/ca.crt" client.pem
openssl pkey -in client.key -check -noout
openssl pkey -in client-combined.pem -check -noout
openssl x509 -in client.pem -noout -serial -dates -fingerprint -sha256 -subject -issuer
openssl x509 -in client.pem -noout -ext basicConstraints
openssl x509 -in client.pem -noout -ext keyUsage
openssl x509 -in client.pem -noout -ext extendedKeyUsage
openssl crl2pkcs7 -nocrl -certfile client-chain.pem | openssl pkcs7 -print_certs -noout
openssl x509 -in client.pem -noout -modulus | openssl sha256
openssl rsa -in client.key -noout -modulus | openssl sha256
```

The leaf verified for SSL client purpose, both standalone and combined keys
passed integrity checks, the certificate and key modulus hashes matched, and
the printed chain order was leaf then issuer.
