//! Клиент Clipwire: движок синхронизации, работа с буфером ОС и состояние.
//! Используется приложением (Tauri) и консольным `clipwire-agent`.

pub mod activity;
pub mod admin;
pub mod blobs;
pub mod cache;
pub mod clipboard;
pub mod devices;
pub mod folder;
pub mod group_config;
pub mod history;
pub mod i18n;
pub mod i18n_en;
pub mod image;
pub mod loopguard;
pub mod p2p;
#[cfg(target_os = "macos")]
pub mod paste_import;
pub mod pins;
pub mod push_mark;
pub mod read_budget;
pub mod secret;
pub mod service;
pub mod settings;
pub mod source_app;
pub mod state;
pub mod storage;
pub mod sync;
pub mod uploaded;
#[cfg(windows)]
pub mod virtual_files;
