//! Общая часть Clipwire: модель элемента буфера, шифрование ключом группы,
//! код приглашения и формат сообщений между агентом и сервером.
//!
//! Сетевого кода здесь нет: крейт не зависит от iroh и tokio, чтобы его можно
//! было тестировать отдельно и переиспользовать в любом клиенте.

pub mod admin;
pub mod ai;
pub mod blob;
pub mod crypto;
pub mod delete;
pub mod device;
pub mod invite;
pub mod item;
pub mod p2p;
pub mod pin;
pub mod proto;
pub mod storage;

pub use admin::AdminKey;
pub use blob::BlobId;
pub use crypto::{GroupId, GroupKey, JoinSecret, SealedItem};
pub use invite::{AdminServerInfo, GroupInviteCode, Invite, ServerCode, ServerInfo};
pub use item::{AiOrigin, Content, DeviceId, Ext, ExtList, FileRef, Item, ItemId, SourceApp};
pub use pin::{PinId, PinState};
pub use storage::{GroupConfig, StorageConfig};
