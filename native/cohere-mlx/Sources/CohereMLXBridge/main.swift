import Foundation
import MLX
import MLXAudioCore
import MLXAudioSTT
import CohereMLXBridgeProtocol

private struct Arguments {
    var modelDir: String
    var language: String
    var maxTokens: Int

    static func parse(_ argv: [String]) throws -> Arguments {
        var modelDir = ProcessInfo.processInfo.environment["DICTATE_COHERE_MLX_MODEL_DIR"]
        var language = ProcessInfo.processInfo.environment["DICTATE_COHERE_LANG"] ?? "en"
        var maxTokens = 256
        var i = 1
        while i < argv.count {
            switch argv[i] {
            case "--model-dir":
                i += 1; guard i < argv.count else { throw BridgeError.usage("--model-dir needs a path") }
                modelDir = argv[i]
            case "--language":
                i += 1; guard i < argv.count else { throw BridgeError.usage("--language needs a code") }
                language = argv[i]
            case "--max-tokens":
                i += 1; guard i < argv.count, let value = Int(argv[i]), value > 0 else {
                    throw BridgeError.usage("--max-tokens needs a positive integer")
                }
                maxTokens = value
            case "--help", "-h":
                print("cohere-mlx-bridge --model-dir <MLX safetensors directory> [--language en] [--max-tokens 256]")
                exit(0)
            default:
                throw BridgeError.usage("unknown argument: \(argv[i])")
            }
            i += 1
        }
        guard let modelDir, !modelDir.isEmpty else {
            throw BridgeError.usage("model directory is required (or DICTATE_COHERE_MLX_MODEL_DIR)")
        }
        return Arguments(modelDir: modelDir, language: language, maxTokens: maxTokens)
    }
}

private enum BridgeError: Error, CustomStringConvertible {
    case usage(String)
    case protocolError(String)
    case modelError(String)

    var description: String {
        switch self {
        case .usage(let value), .protocolError(let value), .modelError(let value): return value
        }
    }
}

private final class CohereRunner {
    let model: CohereTranscribeModel
    let modelName: String
    let defaultLanguage: String
    let defaultMaxTokens: Int

    init(arguments: Arguments) throws {
        let url = URL(fileURLWithPath: arguments.modelDir, isDirectory: true)
        let started = Date()
        model = try CohereTranscribeModel.fromDirectory(url)
        modelName = url.lastPathComponent
        defaultLanguage = arguments.language
        defaultMaxTokens = arguments.maxTokens
        let loadMS = Date().timeIntervalSince(started) * 1000
        FileHandle.standardError.write(Data("cohere-mlx: model loaded in \(String(format: "%.1f", loadMS)) ms\n".utf8))
    }

    func transcribe(_ request: BridgeRequest) throws -> BridgeResponse {
        let started = Date()
        guard request.sampleRate ?? 16_000 == 16_000 else {
            throw BridgeError.protocolError("sample_rate must be 16000; resample in Rust before sending")
        }
        guard request.channels ?? 1 == 1 else {
            throw BridgeError.protocolError("channels must be 1; downmix in Rust before sending")
        }
        guard let encoded = request.audioF32Base64,
              let bytes = Data(base64Encoded: encoded, options: [.ignoreUnknownCharacters]),
              bytes.count > 0, bytes.count % MemoryLayout<Float>.size == 0 else {
            throw BridgeError.protocolError("audio_f32_base64 must be non-empty little-endian f32 PCM")
        }
        let audio = bytes.withUnsafeBytes { raw -> [Float] in
            let words = raw.bindMemory(to: UInt32.self)
            return words.map { Float(bitPattern: UInt32(littleEndian: $0)) }
        }
        guard audio.allSatisfy({ $0.isFinite }) else {
            throw BridgeError.protocolError("audio contains NaN or infinity")
        }
        let audioArray = MLXArray(audio)
        let decodeInputMS = Date().timeIntervalSince(started) * 1000
        let params = STTGenerateParameters(
            maxTokens: min(max(request.maxTokens ?? defaultMaxTokens, 1), 4096),
            temperature: 0,
            topP: 1,
            topK: 0,
            verbose: false,
            language: (request.language?.isEmpty == false ? request.language : defaultLanguage),
            chunkDuration: 1200,
            minChunkDuration: 1
        )
        let inferenceStarted = Date()
        let output = model.generate(audio: audioArray, generationParameters: params)
        let inferenceMS = Date().timeIntervalSince(inferenceStarted) * 1000
        let totalMS = Date().timeIntervalSince(started) * 1000
        return BridgeResponse(
            ok: true,
            requestID: request.requestID,
            text: output.text,
            model: modelName,
            sampleRate: 16_000,
            samples: audio.count,
            timingsMS: [
                "decode_input": decodeInputMS,
                "inference": inferenceMS,
                "total": totalMS,
            ]
        )
    }
}

private func write(_ response: BridgeResponse) {
    do {
        var data = try BridgeProtocol.encode(response)
        data.append(0x0A)
        FileHandle.standardOutput.write(data)
    } catch {
        FileHandle.standardError.write(Data("cohere-mlx: cannot encode response: \(error)\n".utf8))
    }
}

private func failure(_ error: Error, requestID: String? = nil) -> BridgeResponse {
    let code: String
    switch error {
    case BridgeError.usage: code = "usage"
    case BridgeError.protocolError: code = "protocol"
    case BridgeError.modelError: code = "model"
    default: code = "internal"
    }
    return BridgeResponse(ok: false, requestID: requestID, errorCode: code, errorMessage: String(describing: error))
}

struct CohereMLXBridgeMain {
    static func main() {
        do {
            let arguments = try Arguments.parse(CommandLine.arguments)
            let runner = try CohereRunner(arguments: arguments)
            write(BridgeResponse(ok: true, event: "ready", model: runner.modelName,
                                 sampleRate: 16_000))
            while let line = readLine(strippingNewline: true) {
                guard !line.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty else { continue }
                // Read the id before anything can throw, so every error reply
                // names the request it answers.
                let requestID = BridgeProtocol.requestID(fromLine: line)
                do {
                    let request = try JSONDecoder().decode(BridgeRequest.self, from: Data(line.utf8))
                    if request.op == "health" {
                        write(BridgeResponse(ok: true, event: "health", requestID: request.requestID,
                                             model: runner.modelName, sampleRate: 16_000))
                    } else if request.op == "shutdown" {
                        write(BridgeResponse(ok: true, event: "shutdown", requestID: request.requestID))
                        return
                    } else if request.op == "transcribe" {
                        write(try runner.transcribe(request))
                    } else {
                        write(failure(BridgeError.protocolError("unknown op: \(request.op)"), requestID: request.requestID))
                    }
                } catch {
                    write(failure(error, requestID: requestID))
                }
            }
        } catch {
            FileHandle.standardError.write(Data("cohere-mlx: \(error)\n".utf8))
            exit(2)
        }
    }
}

// This target's source file is named `main.swift`, so SwiftPM and Xcode both
// treat top-level code as the executable entry point. Keeping the implementation
// in a type makes it testable while avoiding `@main`/top-level conflicts in
// Xcode's SwiftPM integration.
CohereMLXBridgeMain.main()
