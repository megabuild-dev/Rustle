# Releasing

Self-updates and the `release.yml` workflow depend on one Ed25519 key pair.

```bash
# 1. generate once; keep key.pem offline/in a secret store
openssl genpkey -algorithm ed25519 -out key.pem
# 2. public key as 64 hex characters (stored in the GitHub repository VARIABLE RUSTLE_UPDATE_PUBKEY_HEX)
openssl pkey -in key.pem -pubout -outform DER | tail -c 32 | xxd -p -c 64
# 3. private key goes into the repository SECRET RUSTLE_SIGNING_KEY (paste the PEM)
```

Then bump `version` in the workspace `Cargo.toml`, commit, and push a tag that matches (`v0.1.1`). The workflow builds the
dashboard and the server for x86_64 and aarch64, embeds the public key (`RUSTLE_UPDATE_PUBKEY_HEX`, read at compile time), signs
each binary (`rustle-<arch>-linux.sig`, raw 64 bytes), verifies the signature, and publishes the binaries, `SHA256SUMS` and
`rustle-installer.tar.gz` to a GitHub release.

Installed servers see the release in Settings > Updates and verify the signature against the key baked into the binary
they are already running. A binary built without the variable has updates disabled.

Private repository note: the installer downloads from public release URLs. While the repository is private, download the release
assets with `gh release download`, then run `RUSTLE_BINARY=./rustle-x86_64-linux ./install/install.sh` from the extracted
`rustle-installer.tar.gz`.
