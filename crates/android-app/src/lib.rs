//! Android entry point: Makepad UI, JNI glue (`JNI_OnLoad` + `RegisterNatives`), AAudio,
//! playback capture over `AudioRecord`.
//!
//! * Stub. Implemented in session S8 (PLAN.md §6.3, §11). Java lives only in
//!   `resources/android/java/` and every method there forwards to a Rust `native` fn.
