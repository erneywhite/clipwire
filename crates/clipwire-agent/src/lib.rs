//! Клиент Clipwire: движок синхронизации, работа с буфером ОС и состояние.
//! Используется приложением (Tauri) и консольным `clipwire-agent`.

pub mod clipboard;
pub mod loopguard;
pub mod secret;
pub mod service;
pub mod state;
pub mod sync;
