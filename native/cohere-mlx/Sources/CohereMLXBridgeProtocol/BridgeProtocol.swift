import Foundation

/// One line on stdin. Audio is little-endian IEEE-754 f32 PCM, base64 encoded.
public struct BridgeRequest: Codable, Sendable {
    public var op: String
    public var requestID: String?
    public var sampleRate: Int?
    public var channels: Int?
    public var audioF32Base64: String?
    public var language: String?
    public var maxTokens: Int?

    public init(op: String, requestID: String? = nil, sampleRate: Int? = nil,
                channels: Int? = nil, audioF32Base64: String? = nil,
                language: String? = nil, maxTokens: Int? = nil) {
        self.op = op
        self.requestID = requestID
        self.sampleRate = sampleRate
        self.channels = channels
        self.audioF32Base64 = audioF32Base64
        self.language = language
        self.maxTokens = maxTokens
    }

    enum CodingKeys: String, CodingKey {
        case op
        case requestID = "request_id"
        case sampleRate = "sample_rate"
        case channels
        case audioF32Base64 = "audio_f32_base64"
        case language
        case maxTokens = "max_tokens"
    }
}

public struct BridgeResponse: Codable, Sendable {
    public var ok: Bool
    public var event: String?
    public var requestID: String?
    public var text: String?
    public var model: String?
    public var sampleRate: Int?
    public var samples: Int?
    public var timingsMS: [String: Double]?
    public var errorCode: String?
    public var errorMessage: String?

    public init(ok: Bool, event: String? = nil, requestID: String? = nil,
                text: String? = nil, model: String? = nil, sampleRate: Int? = nil,
                samples: Int? = nil, timingsMS: [String: Double]? = nil,
                errorCode: String? = nil, errorMessage: String? = nil) {
        self.ok = ok
        self.event = event
        self.requestID = requestID
        self.text = text
        self.model = model
        self.sampleRate = sampleRate
        self.samples = samples
        self.timingsMS = timingsMS
        self.errorCode = errorCode
        self.errorMessage = errorMessage
    }

    enum CodingKeys: String, CodingKey {
        case ok, event, text, model, samples
        case requestID = "request_id"
        case sampleRate = "sample_rate"
        case timingsMS = "timings_ms"
        case errorCode = "error_code"
        case errorMessage = "error_message"
    }
}

public enum BridgeProtocol {
    public static let protocolVersion = 1

    public static func encode(_ response: BridgeResponse) throws -> Data {
        let encoder = JSONEncoder()
        encoder.outputFormatting = [.sortedKeys]
        return try encoder.encode(response)
    }
}
