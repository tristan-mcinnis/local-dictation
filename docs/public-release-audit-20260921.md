# Public-release readiness audit — 2026-09-21

> **Status 2026-10-03.** The tracked-file items below are fixed at HEAD:
> `.memory/` is untracked and gitignored, `prompts-lab/cleanup_stream.json` is
> deleted, personal names and paths are neutralised, `tests/scrub.sh` guards the
> tree, and `scripts/download-models.sh` fetches a public Cohere 4-bit MLX
> conversion. Still open: the same material remains in Git history (publish
> from a fresh history or run `git filter-repo`), the test voice needs recorded
> consent, and there is no Developer ID certificate yet.

An evidence-backed audit of Local Dictation for public, direct-distribution
release: secrets, licences, personal and machine-specific data, dependencies,
permissions, onboarding, bundle identity, signing and release assumptions. It
covers the **current tracked files and the full Git history** (218 commits).

**Verdict: NOT PUBLIC-READY as it stands.** There are no credential leaks, and
the licence posture is strong, but four blockers remain — two data-exposure
items and two release-credential items — plus a set of pre-public cleanups. The
repository must stay private until the blockers are resolved and a human makes
the release decision.

Commands and methods are listed so every claim can be re-run. Commit audited:
`7ae94e6` on `main` (the release plumbing added by this change is on top of it).

## How this was checked

```bash
git ls-files | wc -l                         # 198 tracked files
gitleaks detect --source . --log-opts=--all  # full history, redacted
git rev-list --objects --all                 # object inventory (1,863 objects)
git rev-list --objects --all | git cat-file --batch-check='%(objecttype) %(objectname) %(objectsize) %(rest)'   # large blobs
git log --all --diff-filter=D --name-only    # files deleted but still in history
git grep -nIE '<secret patterns>' -- .      # tracked-file secret scan
cargo metadata --format-version 1 --all-features   # dependency licence tally
codesign -dvvv / plutil / spctl / stapler    # signature and bundle facts
```

## 1. Secrets and credentials — PASS

- **`gitleaks` over all history: "218 commits scanned, no leaks found"** (0
  findings). This covers added content of files later deleted, so it also sees
  the removed audio and scripts below.
- Targeted grep for private keys, `sk-`/`ghp_`/`github_pat_`/`xox*`/`AKIA`/`AIza`
  tokens, PEM/certificate blocks and `password=…`/`api_key=…` assignments across
  the tracked tree and every historical diff: **no matches**.
- No tracked or historical filename resembling a key, keystore, `.env`,
  `.mobileprovision`, `.p12`, `.pem` or certificate.
- No runtime secret exists by design: the app is offline, has no account and no
  API keys; `.gitignore` excludes `target/`, `dist/`, `models/*/` and
  `native/cohere-mlx/.build/`.

Decision: **no secret remediation required.**

## 2. Git history — one BLOCKER, one note

- **BLOCKER — third-party audio in history.** Six Superwhisper reference cues
  were committed and later deleted, but remain reachable in history:
  `resources/sounds/reference/superwhisper-{Start,StartClassic,Stop,StopClassic,PreStop,PreStop_fast}.m4a`.
  They are another product's audio assets. Publicly exposing them is a licence
  and goodwill problem even though they are not in the working tree.
- History also holds other removed files (`build.rs`,
  `docs/feature-architecture.html`, several `examples/*_lab.rs`,
  `scripts/gen-*_corpus.sh`) — none sensitive.
- Large objects: `docs/demo.gif` (2.2 MB) and `testdata/dictation_sample.wav`
  (271 KB) are the only blobs over 200 KB. Both are in the working tree today.
- Commit identities expose personal email addresses:
  `Tristan McInnis <tristan.mcinnis@gmail.com>` and `Claude
  <noreply@anthropic.com>`. For an open-source project this is normal and
  expected; it is recorded here, not flagged.
- The repository is **private** today, with `origin` at
  `github.com/tristan-mcinnis/local-dictation`.

## 3. Personal and private data — two BLOCKERS

### BLOCKER — `.memory/` session notes are tracked

40 files (~110 KB) under `.memory/01a0514b-…/` are tracked on purpose by the
repo contract, but they are agent session notes containing machine layout,
absolute paths, model-registry contents, a personal `superwhisper` path and
workflow detail. Example: `.memory/…/local-dictation.md:8` names
the old repo path under the author's home folder, and a `result.json` records the
contents of `~/Models`. This is private working context, not product
documentation, and must not ship in a public repository.

### BLOCKER — real dictation text in `prompts-lab/cleanup_stream.json`

The file's own header states its `GOLD` values are "real history.db dictation
text (verbatim)", including project and client content (a client slide-review
segment; a speaker-ID planning segment). `prompts-lab/lab.json` is authored test
copy and is fine. Publishing a user's real dictated speech — even lightly
rendered — is a privacy breach.

### Other personal/machine traces — clean up before public

- Absolute machine paths in docs: `docs/dictation-reliability-acceptance.md:201`
  (the repo path under the author's home folder) and
  `docs/truncation-diagnosis-20260909.md:14` (the author's model folder).
- Personal names in code tests, examples and docs: `examples/latency_lab.rs`
  (a colleague's and the author's names), `src/audio.rs`, `src/cues.rs`,
  `src/corrections.rs`, and `docs/cleanup-behaviour-20260909.md` (a client
  project name). `src/history.rs` used a client brand in test fixtures.
- `testdata/dictation_sample.wav` is 8.35 s of a real human voice (its transcript
  is quoted in `docs/superwhisper-comparison-2026-08-25.md`). Publishing it needs
  the speaker's consent, or it should be replaced with a synthetic sample.

### Media — PASS

`docs/demo.gif`, `docs/pill.png` and `resources/AppIcon.icns` were rendered and
inspected frame-by-frame (gif) or at full size. All on-screen text is generic
demo copy ("welcome to use the local dictation app…", a groceries/movies
bullet-list demo, a Spanish equivalent). No names, emails, client content, chat
threads, file paths, faces or voices. Safe to publish.

## 4. Licensing — strong, two additions made

- The project itself is **MIT OR Apache-2.0** (`Cargo.toml`, both licence files,
  Copyright 2026 Tristan McInnis). This is a normal, permissive dual licence and
  needs no change to go public.
- `THIRD-PARTY-NOTICES.md` is unusually complete: it names the two models
  (Cohere Transcribe 03-2026, Apache-2.0; **"S1-mini" by "Superwhisper"**,
  Apache-2.0 with the required naming clause), the native Swift packages with
  their licences, a Rust crate tally, and the Kenney CC0 completion sounds with
  per-file provenance.
- Dependency tally from `cargo metadata --all-features`: **198 crates, all
  permissive.** The only crate offering a copyleft option is `r-efi`
  (`MIT OR Apache-2.0 OR LGPL-2.1-or-later`), and MIT/Apache applies. No GPL,
  AGPL, SSPL or proprietary licence is present.
- **Added by this change:** Sparkle 2 is MIT and is now credited in
  `THIRD-PARTY-NOTICES.md`; the new Swift signer uses only Apple's CryptoKit.
- **Before public:** verify the Cohere Q4 conversion's derived-work attribution
  one more time against `scripts/models/convert_superwhisper_cohere.py` (it
  re-keys Safetensors headers, tensor bytes unchanged), and confirm the removed
  Superwhisper audio (Section 2) is not relied on by any licence claim.

## 5. Permissions, privacy strings and bundle identity

- Bundle id `com.tristanmcinnis.local-dictation`; name "Local Dictation";
  `LSMinimumSystemVersion` 14.0; `LSUIElement` (menu-bar agent).
- The only usage string is `NSMicrophoneUsageDescription`, which is accurate.
  Accessibility use is requested through TCC and needs no usage string; the
  Setup window explains it. No camera, screen-recording, contacts or other
  privacy-sensitive entitlement is requested.
- The new release entitlements file adds only
  `com.apple.security.device.audio-input`, required under hardened runtime.
  There is no sandbox and no `disable-library-validation`.
- `LocalDictationBuiltFromCommit` is stamped for traceability.
- **Before public:** adding `LSApplicationCategoryType` is optional polish; it is
  not a blocker.

## 6. Signing and release assumptions

- Today's build signs with the first **Apple Development** identity, or ad-hoc
  with `SIGN_IDENTITY=-`. On this machine exactly one codesigning identity
  exists: an Apple Development certificate. **No Developer ID Application
  certificate exists.**
- Before this change there was **no hardened runtime, no entitlements file, no
  notarization, no Sparkle feed and no appcast**; `docs/architecture.md`
  acknowledged that a copied build is blocked by Gatekeeper. Sparkle's
  `sign_update`/`generate_appcast` and the Sparkle framework are not installed.
  `notarytool`/`stapler` are available via Xcode.
- **Added by this change:** a tested, credential-gated release pipeline
  (`scripts/release/`, `docs/release/`) for Developer ID signing, hardened
  runtime, notarization and stapling, Sparkle 2 EdDSA, an HTTPS appcast,
  publication gates, forward-only rollback and a Homebrew cask path. See
  `docs/release/direct-distribution.md`.

## 7. Onboarding and documentation

- `README.md` is strong: what it is, install, permissions, the model story, the
  offline/telemetry stance, subcommands and trade-offs all exist.
- Gaps for a public reader, now addressed or flagged: the README framed the
  build as a "personal build" and said to "check their terms before broader
  distribution"; there was no direct-distribution or Homebrew section. This
  change adds both and the release runbook.
- **Before public:** the Cohere Q4 checkpoint has **no public clean-room install
  path**. A stranger cannot obtain the fixed speech model; the app can only
  fetch the S1-mini cleanup model. Publishing an app most readers cannot run is
  a product decision, not just a docs gap.

## Verdict

| Area | Status |
| --- | --- |
| Secrets (files + history) | **PASS** |
| Git history third-party audio | **BLOCKER** |
| `.memory/` session notes | **BLOCKER** |
| Real dictation text in `prompts-lab/` | **BLOCKER** |
| Personal paths / names / test voice | Clean up before public |
| Media (gif/png/icon) | **PASS** |
| Project + dependency licences | **PASS** (Sparkle and CryptoKit credited) |
| Permissions / privacy strings | **PASS** |
| Bundle identity | **PASS** |
| Signing / notarization / Sparkle | Pipeline built and dry-run tested; **credentials are a human gate** |
| Onboarding docs | Improved; model-availability gap remains |
| Distribution | Direct + Homebrew paths built and tested offline; **nothing published** |

### Required before any public switch

1. Untrack `.memory/` (or exclude it from the public export) and purge it from
   history if the public repo is the same repository.
2. Replace or remove the real dictation text in `prompts-lab/cleanup_stream.json`.
3. Purge the `resources/sounds/reference/superwhisper-*.m4a` objects from
   history (for example with `git filter-repo`), or publish from a fresh
   history.
4. Scrub the absolute paths and personal names listed in Section 3, and obtain
   consent for `testdata/dictation_sample.wav` or replace it synthetically.
5. Resolve the Cohere model distribution question.
6. Complete the credential and publication human gates in
   `docs/release/direct-distribution.md`.
7. Re-run `gitleaks` and this audit against the final history, then take the
   public-release decision as a deliberate human act.
