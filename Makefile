# One entry point for agents and humans. Wraps the existing scripts; the
# scripts stay the source of truth. Warnings are errors: clippy -D warnings for
# Rust, -warnings-as-errors for the Swift helper (a compiler flag, not
# unsafeFlags in Package.swift).

SWIFT_STRICT := -Xswiftc -warnings-as-errors
NATIVE       := native/cohere-mlx

.PHONY: build build-rust build-swift test test-rust test-swift scrub app install

## Compile everything, warnings as errors. Downloads no models, installs nothing.
build: build-swift build-rust

## Lint gate (featureless and full), then the real release build.
build-rust:
	cargo clippy --all-targets -- -D warnings
	cargo clippy --all-targets --features full -- -D warnings
	cargo build --features full --release

## Compile check of the Swift helper. The shippable helper (with the Metal
## kernels) comes from $(NATIVE)/build.sh via xcodebuild, run by `make app`.
build-swift:
	cd $(NATIVE) && swift build $(SWIFT_STRICT)

## Offline tests. No model downloads: suites whose model is absent skip.
## cargo test also runs the publish scrub (tests/scrub_gate.rs).
test: test-rust test-swift

test-rust:
	cargo test -q
	cargo test -q --features full

## Swift bridge protocol tests, warnings as errors.
test-swift:
	zsh $(NATIVE)/test.sh $(SWIFT_STRICT)

scrub:
	sh tests/scrub.sh

## Build "dist/Local Dictation.app" without installing it.
app:
	./scripts/build-app.sh

## Full installer: prerequisites, model stack, build, /Applications, login item.
install:
	./install.sh
