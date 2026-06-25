// Build-time feature checks. The only hard constraint in this crate: `whisper`
// (whisper.cpp via whisper-rs) and `cleaner` (llama.cpp via llama-cpp-2) BOTH
// vendor ggml, so linking both in one binary produces 533+ duplicate-symbol
// errors. Catch it here with a clear message instead of letting the user stare
// at a wall of linker errors.
fn main() {
    if cfg!(feature = "whisper") && cfg!(feature = "cleaner") {
        panic!(
            "The `whisper` and `cleaner` Cargo features are mutually exclusive: both vendor \
             ggml (whisper.cpp via whisper-rs, llama.cpp via llama-cpp-2) and cannot be linked \
             into the same binary (533+ duplicate symbols). Build with `--features \
             whisper-stack` (Parakeet + Whisper + inject, no cleaner) to use Whisper, or \
             `--features full` (Parakeet + Cohere + Moonshine + cleaner + inject, no Whisper) \
             for the cleanup-enabled stack."
        );
    }
}
