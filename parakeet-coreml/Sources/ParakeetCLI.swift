import Foundation
import CoreML
import AVFoundation
import Accelerate

// MARK: - JSON Output Structures
struct TranscriptionResult: Codable {
    let text: String
    let processingTime: Double
    let audioLength: Double
    let model: String = "parakeet-coreml"
}

struct ErrorResult: Codable {
    let error: String
}

// MARK: - Audio Processing
class AudioProcessor {
    static func loadAudioFile(at path: String) throws -> (samples: [Float], sampleRate: Float) {
        let url = URL(fileURLWithPath: path)
        let file = try AVAudioFile(forReading: url)
        
        let format = file.processingFormat
        let frameCount = UInt32(file.length)
        
        guard let buffer = AVAudioPCMBuffer(pcmFormat: format, frameCapacity: frameCount) else {
            throw AudioError.bufferCreationFailed
        }
        
        try file.read(into: buffer)
        
        // Convert to mono if needed
        let channelCount = Int(format.channelCount)
        let sampleRate = Float(format.sampleRate)
        
        guard let floatData = buffer.floatChannelData else {
            throw AudioError.noFloatData
        }
        
        let frameLength = Int(buffer.frameLength)
        var monoSamples = [Float](repeating: 0, count: frameLength)
        
        if channelCount == 1 {
            // Already mono
            monoSamples = Array(UnsafeBufferPointer(start: floatData[0], count: frameLength))
        } else {
            // Mix down to mono
            for channel in 0..<channelCount {
                let channelData = floatData[channel]
                for i in 0..<frameLength {
                    monoSamples[i] += channelData[i] / Float(channelCount)
                }
            }
        }
        
        // Resample to 16kHz if needed
        if sampleRate != 16000 {
            monoSamples = resample(monoSamples, from: sampleRate, to: 16000)
        }
        
        return (monoSamples, 16000)
    }
    
    static func resample(_ samples: [Float], from sourceSR: Float, to targetSR: Float) -> [Float] {
        let ratio = targetSR / sourceSR
        let outputLength = Int(Float(samples.count) * ratio)
        var output = [Float](repeating: 0, count: outputLength)
        
        // Simple linear interpolation resampling
        for i in 0..<outputLength {
            let sourceIndex = Float(i) / ratio
            let index = Int(sourceIndex)
            let fraction = sourceIndex - Float(index)
            
            if index < samples.count - 1 {
                output[i] = samples[index] * (1 - fraction) + samples[index + 1] * fraction
            } else if index < samples.count {
                output[i] = samples[index]
            }
        }
        
        return output
    }
}

enum AudioError: Error, LocalizedError {
    case bufferCreationFailed
    case noFloatData
    case resamplingFailed
    
    var errorDescription: String? {
        switch self {
        case .bufferCreationFailed:
            return "Failed to create audio buffer"
        case .noFloatData:
            return "No float data in audio buffer"
        case .resamplingFailed:
            return "Failed to resample audio"
        }
    }
}

// MARK: - Parakeet Model Interface
class ParakeetModel {
    private var model: MLModel?
    private let modelPath: String
    
    init(modelPath: String) {
        self.modelPath = modelPath
    }
    
    func loadModel() throws {
        let url = URL(fileURLWithPath: modelPath)
        let compiledUrl = try MLModel.compileModel(at: url)
        self.model = try MLModel(contentsOf: compiledUrl)
    }
    
    func transcribe(audio: [Float]) throws -> String {
        guard let model = model else {
            throw ModelError.notLoaded
        }
        
        // Create MLMultiArray for input
        let inputArray = try MLMultiArray(shape: [1, NSNumber(value: audio.count)], dataType: .float32)
        for (index, sample) in audio.enumerated() {
            inputArray[index] = NSNumber(value: sample)
        }
        
        // Create input provider
        let input = ParakeetInput(audio: inputArray)
        
        // Run prediction
        let output = try model.prediction(from: input)
        
        // Extract transcription from output
        if let transcription = output.featureValue(for: "transcription")?.stringValue {
            return transcription
        } else {
            throw ModelError.noTranscriptionOutput
        }
    }
}

// MARK: - Model Input/Output
class ParakeetInput: MLFeatureProvider {
    let audio: MLMultiArray
    
    init(audio: MLMultiArray) {
        self.audio = audio
    }
    
    var featureNames: Set<String> {
        return ["audio"]
    }
    
    func featureValue(for featureName: String) -> MLFeatureValue? {
        if featureName == "audio" {
            return MLFeatureValue(multiArray: audio)
        }
        return nil
    }
}

enum ModelError: Error, LocalizedError {
    case notLoaded
    case noTranscriptionOutput
    case modelNotFound
    
    var errorDescription: String? {
        switch self {
        case .notLoaded:
            return "Model not loaded"
        case .noTranscriptionOutput:
            return "No transcription in model output"
        case .modelNotFound:
            return "CoreML model file not found"
        }
    }
}

// MARK: - Main CLI
@main
struct ParakeetCLI {
    static func main() {
        let args = CommandLine.arguments
        
        guard args.count >= 2 else {
            printError("Usage: parakeet-cli <audio_file> [model_path]")
            exit(1)
        }
        
        let audioPath = args[1]
        let modelPath = args.count > 2 ? args[2] : findModelPath()
        
        do {
            // Load audio
            let startTime = Date()
            let (samples, _) = try AudioProcessor.loadAudioFile(at: audioPath)
            let audioLength = Double(samples.count) / 16000.0
            
            // Initialize and load model
            let model = ParakeetModel(modelPath: modelPath)
            try model.loadModel()
            
            // Transcribe
            let transcription = try model.transcribe(audio: samples)
            
            let processingTime = Date().timeIntervalSince(startTime)
            
            // Output result as JSON
            let result = TranscriptionResult(
                text: transcription,
                processingTime: processingTime,
                audioLength: audioLength
            )
            
            let encoder = JSONEncoder()
            encoder.outputFormatting = .prettyPrinted
            let jsonData = try encoder.encode(result)
            print(String(data: jsonData, encoding: .utf8)!)
            
        } catch {
            printError(error.localizedDescription)
            exit(1)
        }
    }
    
    static func findModelPath() -> String {
        // Look for model in common locations
        let homeDir = FileManager.default.homeDirectoryForCurrentUser.path
        let possiblePaths = [
            "\(homeDir)/parakeet-tdt-0.6b-v3/ParakeetTDT.mlmodelc",
            "\(homeDir)/.cache/parakeet/ParakeetTDT.mlmodelc",
            "./ParakeetTDT.mlmodelc",
            "./parakeet-tdt-0.6b-v3/ParakeetTDT.mlmodelc"
        ]
        
        for path in possiblePaths {
            if FileManager.default.fileExists(atPath: path) {
                return path
            }
        }
        
        // Default path
        return "\(homeDir)/parakeet-tdt-0.6b-v3/ParakeetTDT.mlmodelc"
    }
    
    static func printError(_ message: String) {
        let error = ErrorResult(error: message)
        let encoder = JSONEncoder()
        encoder.outputFormatting = .prettyPrinted
        if let jsonData = try? encoder.encode(error) {
            FileHandle.standardError.write(jsonData)
            FileHandle.standardError.write("\n".data(using: .utf8)!)
        }
    }
}