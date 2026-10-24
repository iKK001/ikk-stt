# ikk_core_flutter

Flutter plugin wrapping `crates/ikk-ffi` via flutter_rust_bridge 2 and
cargokit. Consumed by `apps/reference_app` and any other Flutter shell.

Regenerate the Dart bindings after changing `rust/src/api/*`:

    flutter_rust_bridge_codegen generate

Events from the engine arrive as JSON strings on `IkkEngine.events()`; the
schema is `ikk_engine::EngineEvent` (serde, externally tagged).
