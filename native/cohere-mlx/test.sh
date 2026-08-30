#!/bin/zsh
set -euo pipefail

here="${0:A:h}"
cd "$here"
swift test
swift package dump-package >/dev/null
grep -q 'audio_f32_base64' Sources/CohereMLXBridgeProtocol/BridgeProtocol.swift
grep -q 'sample_rate' Sources/CohereMLXBridgeProtocol/BridgeProtocol.swift
print "protocol tests passed"
