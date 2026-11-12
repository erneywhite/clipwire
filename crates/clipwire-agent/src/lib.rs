//! Клиент Clipwire: движок синхронизации, работа с буфером ОС и состояние.
//! Используется приложением (Tauri) и консольным `clipwire-agent`.

pub mod clipboard;
pub mod history;
pub mod loopguard;
pub mod secret;
pub mod service;
pub mod settings;
pub mod state;
pub mod sync;
