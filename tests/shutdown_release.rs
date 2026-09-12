//! Regression guard for the quit crash: the cleanup engine must free its Metal
//! buffers when it is dropped, so ggml's static Metal-device destructor finds an
//! empty residency set at exit instead of aborting
//! (`GGML_ASSERT([rsets->data count] == 0)`, ggml-metal-device.m:612).
//!
//! The assertion is the test binary's own exit status: if `drop` left a Metal
//! buffer alive, this process would SIGABRT on the way out and `cargo test`
//! would report the failure.
//!
//! Needs the real model, so it skips when the cleaner feature is off or the
//! GGUF is not on this machine. `examples/shutdown_probe.rs` exercises the same
//! path in both directions (drop → exit 0, leak → SIGABRT).

#[cfg(feature = "cleaner")]
#[test]
fn dropping_the_cleanup_engine_frees_its_metal_buffers() {
    use fast_dictate_backend::cleaner::TextCleanupEngine;

    let model = fast_dictate_backend::settings::Settings::load()
        .resolve_llm_model(&fast_dictate_backend::app_paths::llm_default_path());
    if !std::path::Path::new(&model).exists() {
        eprintln!("[skip] cleanup model not present at {model}");
        return;
    }

    let engine = TextCleanupEngine::initialize(&model).expect("cleanup engine should load");
    drop(engine);
    // Reaching here means the worker thread joined and its LlamaContext is gone.
    // The exit-time assertion does the rest.
}
