//! Sparkle-compatible Ed25519 signer/verifier for the Local Dictation release
//! pipeline.
//!
//! Sparkle 2 signs an update archive with a standard Ed25519 (RFC 8032) key:
//! `sparkle:edSignature` in the appcast is the base64 of the 64-byte signature
//! over the archive bytes, and `SUPublicEDKey` in Info.plist is the base64 of
//! the 32 raw public-key bytes. CryptoKit's `Curve25519.Signing` is that same
//! standard primitive, so a signature made here verifies in Sparkle and vice
//! versa.
//!
//! This tool exists for two reasons:
//!   1. the offline dry run needs to sign and independently verify without the
//!      Sparkle distribution present;
//!   2. production prefers the official `sign_update`/`generate_appcast` from
//!      the Sparkle release, and this file is the documented fallback when only
//!      the raw key material is available.
//!
//! It never writes a private key anywhere except an explicit `keygen --out`
//! 0600 file, and it never prints private key material.
//!
//! Usage:
//!   sparkle_sign.swift keygen --out <dir>            # prints public key (base64)
//!   sparkle_sign.swift pubkey --key <file|->         # public key from a seed
//!   sparkle_sign.swift sign <file> [--key <file|->]  # base64 signature
//!   sparkle_sign.swift verify <file> <sig-b64> <pub-b64>

import CryptoKit
import Foundation

func fail(_ message: String) -> Never {
    FileHandle.standardError.write(("sparkle_sign: " + message + "\n").data(using: .utf8)!)
    exit(1)
}

func readKey(from path: String) -> Data {
    let raw: String
    if path == "-" {
        raw = String(data: FileHandle.standardInput.readDataToEndOfFile(), encoding: .utf8) ?? ""
    } else {
        guard let text = try? String(contentsOfFile: path, encoding: .utf8) else {
            fail("cannot read key file: \(path)")
        }
        raw = text
    }
    let trimmed = raw.trimmingCharacters(in: .whitespacesAndNewlines)
    guard let data = Data(base64Encoded: trimmed), data.count == 32 else {
        fail("private key must be base64 of a 32-byte Ed25519 seed")
    }
    return data
}

func privateKey(from path: String) -> Curve25519.Signing.PrivateKey {
    do {
        return try Curve25519.Signing.PrivateKey(rawRepresentation: readKey(from: path))
    } catch {
        fail("invalid private key: \(error)")
    }
}

let args = Array(CommandLine.arguments.dropFirst())
guard let command = args.first else {
    fail("usage: sparkle_sign.swift keygen|pubkey|sign|verify ...")
}

func flagValue(_ name: String) -> String? {
    guard let i = args.firstIndex(of: name), i + 1 < args.count else { return nil }
    return args[i + 1]
}

switch command {
case "keygen":
    guard let out = flagValue("--out") else { fail("keygen needs --out <dir>") }
    let dir = URL(fileURLWithPath: out, isDirectory: true)
    try? FileManager.default.createDirectory(at: dir, withIntermediateDirectories: true)
    let key = Curve25519.Signing.PrivateKey()
    let keyURL = dir.appendingPathComponent("ed25519_private.key")
    do {
        try key.rawRepresentation.base64EncodedString().write(to: keyURL, atomically: true, encoding: .utf8)
        try FileManager.default.setAttributes([.posixPermissions: 0o600], ofItemAtPath: keyURL.path)
    } catch {
        fail("cannot write key file: \(error)")
    }
    // The public key goes to stdout; the private key never does.
    print(key.publicKey.rawRepresentation.base64EncodedString())

case "pubkey":
    guard let keyPath = flagValue("--key") else { fail("pubkey needs --key <file|->") }
    let key = privateKey(from: keyPath)
    print(key.publicKey.rawRepresentation.base64EncodedString())

case "sign":
    guard let file = args.dropFirst().first(where: { !$0.hasPrefix("--") }) else {
        fail("sign needs a file")
    }
    guard let keyPath = flagValue("--key") ?? ProcessInfo.processInfo.environment["SU_PRIVATE_KEY_FILE"] else {
        fail("sign needs --key <file|-> or SU_PRIVATE_KEY_FILE")
    }
    guard let data = FileManager.default.contents(atPath: file) else { fail("cannot read \(file)") }
    let key = privateKey(from: keyPath)
    do {
        let signature = try key.signature(for: data)
        print(signature.base64EncodedString())
    } catch {
        fail("signing failed: \(error)")
    }

case "verify":
    let positional = args.dropFirst().filter { !$0.hasPrefix("--") }
    guard positional.count == 3 else { fail("verify needs <file> <sig-b64> <pub-b64>") }
    let file = positional[positional.startIndex]
    let sigB64 = positional[positional.startIndex + 1]
    let pubB64 = positional[positional.startIndex + 2]
    guard let data = FileManager.default.contents(atPath: file) else { fail("cannot read \(file)") }
    guard let sig = Data(base64Encoded: sigB64), sig.count == 64 else { fail("signature is not base64 of 64 bytes") }
    guard let pub = Data(base64Encoded: pubB64), pub.count == 32 else { fail("public key is not base64 of 32 bytes") }
    do {
        let publicKey = try Curve25519.Signing.PublicKey(rawRepresentation: pub)
        if publicKey.isValidSignature(sig, for: data) {
            print("ok")
            exit(0)
        }
        fail("signature does not verify")
    } catch {
        fail("verification error: \(error)")
    }

default:
    fail("unknown command: \(command)")
}
