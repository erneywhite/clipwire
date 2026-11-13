//! Общая часть Clipwire: модель элемента буфера, шифрование ключом группы,
//! код приглашения и формат сообщений между агентом и сервером.
//!
//! Сетевого кода здесь нет: крейт не зависит от iroh и tokio, чтобы его можно
//! было тестировать отдельно и переиспользовать в любом клиенте.

pub mod blob;
pub mod crypto;
pub mod invite;
pub mod item;
pub mod proto;

pub use blob::BlobId;
pub use crypto::{GroupId, GroupKey, JoinSecret, SealedItem};
pub use invite::{Invite, ServerInfo};
pub use item::{Content, DeviceId, FileRef, Item, ItemId};
