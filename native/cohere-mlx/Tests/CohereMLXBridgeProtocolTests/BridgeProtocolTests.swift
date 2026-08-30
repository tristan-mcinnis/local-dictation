import XCTest
@testable import CohereMLXBridgeProtocol

final class BridgeProtocolTests: XCTestCase {
    func testRequestUsesRustContractKeys() throws {
        let request = BridgeRequest(op: "transcribe", requestID: "42", sampleRate: 16_000,
                                    channels: 1, audioF32Base64: "AAAA", language: "en", maxTokens: 256)
        let data = try JSONEncoder().encode(request)
        let object = try XCTUnwrap(JSONSerialization.jsonObject(with: data) as? [String: Any])
        XCTAssertEqual(object["request_id"] as? String, "42")
        XCTAssertEqual(object["sample_rate"] as? Int, 16_000)
        XCTAssertEqual(object["audio_f32_base64"] as? String, "AAAA")
        XCTAssertNil(object["requestID"])
    }

    func testResponseRoundTripsTimings() throws {
        let response = BridgeResponse(ok: true, requestID: "x", text: "hello",
                                      timingsMS: ["inference": 12.5])
        let data = try BridgeProtocol.encode(response)
        let decoded = try JSONDecoder().decode(BridgeResponse.self, from: data)
        XCTAssertEqual(decoded.text, "hello")
        XCTAssertEqual(decoded.timingsMS?["inference"], 12.5)
    }
}
