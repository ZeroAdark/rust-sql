;; The Gravity JIT host's stub for embedders without one (wasmtime's CLI e2e: --preload env=wasm/gravity-jit-stub.wat).
;; gravity_install answers -2 ("no JIT host"), so the server's Gravity programs run interpreted, silently
;; (crates/gravity/backend/src/wasm.rs); gravity_uninstall is then never called.
(module
  (func (export "gravity_install") (param i32 i32 i32 i32 i32) (result i32) i32.const -2)
  (func (export "gravity_uninstall") (param i32 i32))
  ;; no cancel channel: the server never polls for a cancel
  (func (export "pgrust_cancel_poll") (result i32) i32.const -1))
