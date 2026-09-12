//! Shutdown-ordering probe for the Metal-context release path.
//!
//! Reproduces (and then guards) the quit abort: ggml keeps its Metal devices in
//! a function-local `static std::vector<ggml_metal_device_ptr>` inside
//! `ggml_metal_device_get`. At process exit `__cxa_finalize_ranges` destroys
//! that vector, and `ggml_metal_device_free` → `ggml_metal_rsets_free` asserts
//! `[rsets->data count] == 0` — i.e. every Metal buffer must already be freed.
//! A live `LlamaModel` / `LlamaContext` at exit therefore aborts the process.
//!
//! ```bash
//! cargo run --release --features full --example shutdown_probe -- drop   # exits 0
//! cargo run --release --features full --example shutdown_probe -- leak   # SIGABRT
//! ```
//!
//! `tests/shutdown_release.rs` runs the `drop` mode and asserts a clean exit.

#[cfg(not(feature = "cleaner"))]
fn main() {
    eprintln!("shutdown_probe needs the `cleaner` feature: cargo run --release --features full --example shutdown_probe");
    std::process::exit(1);
}

#[cfg(feature = "cleaner")]
fn main() {
    use fast_dictate_backend::cleaner::TextCleanupEngine;

    let mode = std::env::args().nth(1).unwrap_or_else(|| "drop".into());
    let model = fast_dictate_backend::settings::Settings::load()
        .resolve_llm_model(&fast_dictate_backend::app_paths::llm_default_path());

    let engine = match TextCleanupEngine::initialize(&model) {
        Ok(e) => e,
        Err(e) => {
            eprintln!("[probe] init failed ({model}): {e}");
            std::process::exit(2);
        }
    };

    match mode.as_str() {
        // The pre-fix quit path: the engine is still alive when exit() runs the
        // C++ static destructors. Expected to abort.
        "leak" => {
            std::mem::forget(engine);
            eprintln!("[probe] leaked engine — exiting with Metal buffers still live");
        }
        // The fixed quit path: release the context/model first, while Metal is
        // still alive, so the static destructor finds an empty residency set.
        _ => {
            drop(engine);
            eprintln!("[probe] released engine — exiting clean");
        }
    }

    std::process::exit(0);
}
