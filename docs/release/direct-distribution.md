# Direct distribution and Sparkle updates

Local Dictation ships two ways. Both start from the same signed, notarized
build:

1. **Direct download** — a notarized DMG for a human, plus an in-app **Sparkle 2**
   update feed. The app checks an HTTPS appcast and installs signed updates
   itself.
2. **Homebrew cask** — `brew install --cask local-dictation` from a tap. The
   cask installs the same app; Sparkle still owns in-app updates
   (`auto_updates true`, so Homebrew does not fight it). See
   [`homebrew.md`](homebrew.md).

This document is the direct-download half. Everything here is owned by this
repository: the bundle identity, the signing identity, the notarization
credentials, the Sparkle key, the appcast and the release artifacts. Nothing is
published by running these scripts alone.

## What already works without Apple credentials

`scripts/release/dry-run.sh` exercises the entire pipeline offline against
synthetic artifacts: hardened-runtime signing, entitlements, Ed25519
signing/verification, appcast generation and contract checks, the publication
refusals, rollback, and Homebrew cask generation. It needs no network, no Apple
account, no Keychain and no `/Applications` write. Run it after any change to
the pipeline:

```bash
./scripts/release/dry-run.sh     # 42 checks, 0 failures
```

## The pipeline

```
scripts/build-app.sh --release          # compile, embed Sparkle, harden, sign
        │
        ├─ scripts/release/notarize.sh  # notarytool submit --wait, staple, spctl
        │
        ├─ ditto → Local-Dictation-<v>.zip   # the update archive
        │
        ├─ scripts/release/make-appcast.sh   # Ed25519 sign + appcast.xml
        │
        ├─ scripts/release/verify-release.sh # signature, runtime, staple, feed
        │
        └─ scripts/release/publish.sh        # gated upload (the only egress)
```

`scripts/release/build-release.sh` drives all of it in order. `--dry-run` prints
the plan and changes nothing.

| Script | Job |
| --- | --- |
| `lib.sh` | shared helpers; reads credentials, never prints them |
| `entitlements.plist` | the minimum hardened-runtime entitlements |
| `build-release.sh` | the orchestrator; `--dry-run`, `--skip-notarize` |
| `notarize.sh` | `notarytool submit --wait`, `stapler`, `spctl` |
| `make-appcast.sh` | signs the archive, writes and validates the appcast |
| `appcast.py` | appcast build/contract-check (stdlib only) |
| `sparkle_sign.swift` | standard Ed25519 sign/verify (CryptoKit) |
| `verify-release.sh` | end-to-end release verification |
| `publish.sh` | gated upload and rollback |
| `update-cask.sh` | renders the Homebrew cask from a DMG |
| `dry-run.sh` | the offline test of all of the above |

## Signing and the hardened runtime

`build-app.sh --release` refuses anything but a **Developer ID Application**
identity, refuses a dirty tree, and requires `SU_FEED_URL`, `SU_PUBLIC_ED_KEY`
and the entitlements file. It signs inside-out — the MLX helper, then Sparkle's
`Installer.xpc`, `Downloader.xpc`, `Autoupdate`, `Updater.app` and the framework
binary, then the framework, then the app — and never with `--deep`, which Apple
and Sparkle both warn is a common source of notarization and sandbox failures.

`scripts/release/entitlements.plist` carries exactly one resource entitlement:

- `com.apple.security.device.audio-input` — CoreAudio capture under hardened
  runtime. Accessibility (AX) injection is a TCC grant, not an entitlement, so
  it is correctly absent.

`com.apple.security.cs.disable-library-validation` is **not** used. Sparkle and
the `cohere-mlx-bridge` helper are re-signed with the same Developer ID, so
library validation passes without weakening the runtime. `get-task-allow` is
never present (it is debug-only and blocks notarization).

## Credentials

No credential is ever stored in this repository. The pipeline reads them from
the environment or a user-owned file:

| Credential | Where it comes from | Used by |
| --- | --- | --- |
| Developer ID identity | `SIGN_IDENTITY` (or the login keychain) | `build-app.sh --release` |
| Notarization profile | `NOTARY_PROFILE` (preferred) or `NOTARY_APPLE_ID` + `NOTARY_TEAM_ID` + `NOTARY_PASSWORD` | `notarize.sh` |
| Sparkle private key | `SU_PRIVATE_KEY` (env) or `SU_PRIVATE_KEY_FILE` (0600) | `make-appcast.sh` |
| Sparkle public key | `SU_PUBLIC_ED_KEY` (not secret) | the app and the appcast |
| Upload command | `RELEASE_UPLOAD_CMD` or `RELEASE_UPLOAD_DIR` | `publish.sh` |

`lib.sh` refuses a `SU_PRIVATE_KEY_FILE` whose mode is not `0600`/`0400`, and
no helper echoes a credential value. The Sparkle private key is best generated
and kept by Sparkle's own `generate_keys` (Keychain, account `ed25519`); use
`generate_keys -x` to export a 32-byte base64 seed into a 0600 file when the
offline signer needs it.

Notarization uses a `notarytool` Keychain profile (`xcrun notarytool
store-credentials "<profile>" --apple-id … --team-id …`). `altool` is dead and
is not used.

## Sparkle 2

- `SUFeedURL` and `SUPublicEDKey` are written into `Info.plist` only when both
  are supplied. A build without them is a plain app that never checks for
  updates (dev builds and cask installs without a feed behave this way).
- The signature is a standard Ed25519 signature over the raw archive bytes.
  `scripts/release/sparkle_sign.swift` produces and verifies the same signature
  Sparkle's `sign_update` does; production prefers `sign_update` when
  `SIGN_UPDATE` points at it, with the CryptoKit signer as the documented
  fallback.
- `src/updater.rs` (Cargo feature `sparkle`, part of `full`) starts
  `SPUStandardUpdaterController` on the main thread, loading the framework from
  the running bundle at runtime. It is a quiet no-op unless the bundle carries an
  HTTPS feed, a public key and the embedded framework — so a cask install with no
  feed key never starts an update check.
- The appcast is RSS 2.0 with the Sparkle namespace; `sparkle:version`,
  `sparkle:shortVersionString` and a three-part `sparkle:minimumSystemVersion`
  are top-level item elements, and the enclosure carries `url`, `length`,
  `type` and `sparkle:edSignature`. `appcast.py check` enforces that contract.
- `Sparkle.framework` is never committed. Supply it at build time with
  `SPARKLE_FRAMEWORK=/path/to/Sparkle.framework`. Sparkle 2.10 requires macOS
  12; this app's floor is 14, so the bundle floor is unaffected. Keep the feed
  URL HTTPS: Sparkle refuses to load a plaintext feed by default.

## Publication gates

`publish.sh` refuses to upload unless **all** of these hold:

- a clean, committed working tree;
- an appcast that passes `appcast.py check`, with an HTTPS feed and a signed
  enclosure whose archive exists locally;
- `ALLOW_PUBLISH=1` — the explicit human decision;
- a configured upload destination (`RELEASE_UPLOAD_CMD` or `RELEASE_UPLOAD_DIR`).

Without `ALLOW_PUBLISH=1` or with `--dry-run`, it prints the plan and uploads
nothing. It never echoes the upload command, which may embed a token.

## Rollback

Sparkle 2 **does not downgrade**. Its removal of Sparkle 1's
`SPARKLE_AUTOMATED_DOWNGRADES` is deliberate. A feed advertising a lower
`sparkle:version` produces "no update found", not a rollback.

So rollback has two halves:

1. **Un-advertise** the bad version for clients that have not updated yet:
   `scripts/release/publish.sh --rollback` restores `appcast.xml.prev` and
   re-uploads only the appcast. The item disappears; clients still see the
   previous version as latest.
2. **Recover already-updated clients** by shipping a **new, higher**
   `CFBundleVersion` built from the reverted code. Only a forward release can
   move a client backward in behaviour. `build-release.sh` stamps every artifact
   with `LocalDictationBuiltFromCommit`, so the reverted build is traceable.

Keep at least the previous appcast: `make-appcast.sh` writes `appcast.xml.prev`
on every update. Removing an archive from the host only fails in-flight
downloads; it is not a downgrade.

## Human gates

These cannot be done from the repository and are deliberately not automated:

1. **Apple Developer Program enrollment and payment.**
2. **Developer ID Application certificate** in the login keychain, and a
   `notarytool` Keychain profile.
3. **A Sparkle signing keypair** (`generate_keys`), with the private seed kept
   in the Keychain or a 0600 file and the public key in `SU_PUBLIC_ED_KEY`.
4. **An HTTPS host for the appcast and archives**, and the `RELEASE_UPLOAD_CMD`
   that puts them there.
5. **The final decision to publish**, expressed as `ALLOW_PUBLISH=1`.
6. **The first externally published release** and its update feed, verified on a
   second machine with a fresh download.

Until 1–6 exist, the pipeline is a tested dry run; it cannot produce a
publishable, notarized artifact, and must not be treated as if it can.
