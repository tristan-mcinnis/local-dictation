# Free GitHub release (no Apple Developer ID)

This is the lane in use. It needs no paid Apple account. The result is a DMG that
anyone can download from GitHub Releases. macOS blocks the first open because the
app is not notarized, and the README and the release notes say how to get past that.

The other lane (Developer ID, notarization, Sparkle) is in
[`direct-distribution.md`](direct-distribution.md). It is kept for the day a
Developer ID exists. The two lanes do not share a signing identity.

## Make the DMG

```bash
git status                      # must be clean (the build stamps the commit)
./scripts/make-dmg.sh           # slim build, ad-hoc signed, arm64
```

Set `RELEASE_OUT=/some/dir` to choose the output folder. The default is
`dist/github-release/`. The script never uploads or pushes.

| Output | What it is |
| --- | --- |
| `Local-Dictation-<version>-macos-arm64.dmg` | The download. Compressed (UDZO). The app and an `/Applications` link. |
| `SHA256SUMS` | Checked with `shasum -a 256 -c SHA256SUMS`. |
| `RELEASE_NOTES.md` | Built from `RELEASE_NOTES.template.md`, `WHATS_NEW.md` and `first-open.md`. |
| `VERIFY.txt` | Output of `scripts/verify-dmg.sh`. |

Edit `WHATS_NEW.md` before each release. The version comes from `Cargo.toml`
through the app's `Info.plist`. The script stops if the two disagree.

## What the build does

- `build-app.sh --slim` with `SIGN_IDENTITY=-`. It signs the Swift helper first,
  then the app. It never uses `--deep` to sign.
- No Sparkle. The script clears `SU_FEED_URL`, `SU_PUBLIC_ED_KEY` and
  `SPARKLE_FRAMEWORK`. With no feed and no EdDSA key, the app writes no update
  keys to `Info.plist` and `src/updater.rs` never starts. The app makes no update
  check and no update network call. People update by downloading a new DMG.
- Slim, not bundled. The speech model alone is 1.5 GB, and with the cleanup model
  the bundle is about 2 GB. That is at GitHub's 2 GiB limit for one asset. A slim
  app ships a copy of `download-models.sh` in `Contents/Resources`. It detects it
  is outside a source tree and puts the models in
  `~/Library/Application Support/Local Dictation/models`, with no Xcode.
- arm64 only. The MLX helper and the Metal library are Apple Silicon only.

## Check the DMG

`make-dmg.sh` runs `scripts/verify-dmg.sh` for you. To run it by hand:

```bash
./scripts/verify-dmg.sh path/to/Local-Dictation-<version>-macos-arm64.dmg
```

It attaches the image read-only and hidden, never launches the app, and detaches.
It checks the file name, the image format, strict signatures on the app and the
helper, `lipo -archs`, the version against the file name, the notices and
licences, the absence of Sparkle and models, and records `spctl --assess`. An
ad-hoc build is rejected by `spctl`. That is expected.

## Publish

The GitHub step is yours. The script prints the exact command. It makes a draft:

```bash
gh release create v<version> --repo tristan-mcinnis/local-dictation --draft \
  --title "Local Dictation <version>" --notes-file RELEASE_NOTES.md \
  Local-Dictation-<version>-macos-arm64.dmg SHA256SUMS
```

Open the draft on GitHub, read it, then publish it. The tag `v<version>` must not
already exist on the remote, or the release attaches to the old commit.
