# Homebrew cask

Homebrew is the second install path alongside the direct download. Both install
the same signed, notarized `Local Dictation.app`; Sparkle keeps owning in-app
updates in both cases.

Nothing here publishes a cask or touches a public tap. The repository owns the
cask **template** and the generator; pushing a tap is a separate human gate.

## Files

- `scripts/release/homebrew/local-dictation.rb.tmpl` — the cask, with
  `@VERSION@`, `@SHA256@` and `@URL@` placeholders.
- `scripts/release/update-cask.sh` — renders the template from a real DMG,
  syntax-checks the Ruby, optionally writes it into a local tap checkout, and
  reports Homebrew cask-stanza style issues.

## How a user installs it

Once a tap exists (a human gate), the user runs:

```bash
brew install --cask <tap>/local-dictation
```

The cask:

- downloads the notarized DMG over HTTPS and verifies its `sha256`;
- installs `Local Dictation.app` into `/Applications`;
- declares `auto_updates true`, because Sparkle — not Homebrew — manages
  updates;
- requires macOS Sonoma (14) and Apple Silicon;
- on uninstall, quits the app; on `zap`, removes the app's config, support,
  log and preferences paths.

## Generating the cask for a release

```bash
# From a built, notarized DMG:
VERSION=0.5.0 \
ARTIFACT=dist/releases/Local-Dictation-0.5.0.dmg \
SOURCE_URL="https://github.com/tristan-mcinnis/local-dictation/releases/download/v0.5.0/Local-Dictation-0.5.0.dmg" \
OUT=dist/releases/homebrew/local-dictation.rb \
  scripts/release/update-cask.sh
```

`ARTIFACT` computes the `sha256` from the file, so the cask can never name a
hash that disagrees with the artifact. `SOURCE_URL` defaults to the canonical
GitHub release asset for the version. The generator refuses a malformed
`sha256` (must be 64 hex) and a non-HTTPS URL, and `ruby -c` has to pass.

To stage it into a tap checkout you own (still local, still not pushed):

```bash
TAP_DIR=~/Documents/code/homebrew-tap scripts/release/update-cask.sh
# writes ~/Documents/code/homebrew-tap/Casks/local-dictation.rb
```

## Why a cask and not a formula

Local Dictation is a GUI app bundle, so a cask is the correct Homebrew type. A
formula is for command-line software; this app has no supported CLI install
story.

## The Homebrew agent story (deliberately not used)

A LaunchAgent or a Homebrew `service` stanza is not used. The app is a menu-bar
agent that registers its own login item and runs as the logged-in user; a
Homebrew service would manage it as a daemon, which is the wrong lifecycle and
would fight the app's own model loading and permission grants.

## Human gates

1. A tap repository (private or public) and its push.
2. Either a **public** release asset, or a public download host: a cask URL that
   only authenticated users can reach is not installable by `brew`. Making the
   source repository public is its own gate (see the public-release audit).
3. The first `brew audit --cask --new` pass against a real tap, which needs a
   real, downloadable artifact.
4. The first `brew install --cask` on a clean machine.

Until those exist, `update-cask.sh` renders and syntax-checks the cask offline,
and `scripts/release/dry-run.sh` verifies the rendered cask (fields, hash match,
stanza style). No tap is created and nothing is pushed.
