//! Клиент Clipwire. Платформенный слой (буфер ОС, трей) — в `platform`,
//! всё остальное общее для всех ОС.

pub mod loopguard;
pub mod secret;
pub mod state;
pub mod sync;

#[cfg(windows)]
pub mod platform;
