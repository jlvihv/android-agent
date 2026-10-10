//! 生成 Kotlin 绑定的 CLI。
//!
//! 用法（见 justfile 的 `android-bindings`）：
//!
//! ```sh
//! cargo run --features uniffi-bindgen --bin uniffi-bindgen -- \
//!   generate --library target/debug/libagent_core.so \
//!   --language kotlin --out-dir ../android/app/src/main/java
//! ```
//!
//! `--library` 模式会 dlopen 宿主版本的 `.so`，从中读出 uniffi 元数据，
//! 所以**不需要**维护一份 .udl 文件。

fn main() {
    uniffi::uniffi_bindgen_main()
}
