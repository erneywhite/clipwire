//! Устройства группы и смена ключа на стороне соединения (`docs/devices.md`):
//! `PublishCard`, `ListDevices`, `Rekey` и вход в группу со сменённым ключом.

use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use clipwire_core::proto::ServerMsg;
use clipwire_core::proto::io::write_msg;
use clipwire_core::{BlobId, DeviceId, GroupId};
use iroh::endpoint::SendStream;

use crate::session::{Shared, now_ms};
use crate::store::{RekeyLimits, RekeyOutcome, Rotation};

/// Предел зашифрованной карточки устройства. Настоящая занимает пару сотен
/// байт: имя, ОС, ключ и подпись.
pub const MAX_CARD_BYTES: usize = 4096;
/// Предел одного пакета нового ключа. Настоящий — 32 + 32 + 24 + 32 + 16 байт.
pub const MAX_ENVELOPE_BYTES: usize = 512;
/// Сколько хранить группу со сменённым ключом и её пакеты: за это время
/// выключенные устройства успеют включиться и забрать свой новый ключ.
pub const ROTATED_KEEP: Duration = Duration::from_secs(30 * 24 * 60 * 60);

/// Что сказать устройству, которое пришло в группу со сменённым ключом.
pub fn rotation_msg(r: Rotation) -> ServerMsg {
    match r {
        Rotation::Rekeyed(envelope) => ServerMsg::Rekeyed { envelope },
        Rotation::Excluded => ServerMsg::Excluded,
    }
}

/// Отправить последнее сообщение и закрыть поток. Ждём, пока клиент
/// дочитает, иначе соединение закроется раньше, чем кадр дойдёт
/// (подробнее у `session::refuse`).
pub async fn close_with(send: &mut SendStream, msg: Option<&ServerMsg>) {
    if let Some(msg) = msg {
        let _ = write_msg(send, msg).await;
    }
    let _ = send.finish();
    let _ = tokio::time::timeout(Duration::from_secs(2), send.stopped()).await;
}

/// `PublishCard`: сохранить карточку под `DeviceId` соединения, что бы ни
/// было написано внутри. Слишком большая — ответ `Error`, соединение живёт.
pub fn publish_card(
    shared: &Shared,
    group: &GroupId,
    device: &DeviceId,
    card: &[u8],
) -> Result<Option<ServerMsg>> {
    if card.len() > MAX_CARD_BYTES {
        tracing::warn!(len = card.len(), "карточка устройства больше предела");
        return Ok(Some(ServerMsg::Error(format!(
            "карточка устройства больше {MAX_CARD_BYTES} байт"
        ))));
    }
    shared.store.publish_card(group, device, card, now_ms())?;
    Ok(None)
}

/// `ListDevices`: все устройства группы с карточками и кто сейчас на связи.
pub fn list_devices(shared: &Shared, group: &GroupId) -> Result<ServerMsg> {
    let online = shared.hub.online_devices(group);
    Ok(ServerMsg::Devices(
        shared.store.list_devices(group, &online)?,
    ))
}

/// `Rekey`: перевести группу в режим «ключ сменён». При успехе остальные
/// соединения группы будятся (`Hub::kick`) и сами отдают своим устройствам
/// `Rekeyed` или `Excluded`, файлы группы убираются с диска в фоне.
/// `Err(причина)` — отказ, его надо отправить как `Error`.
pub fn rekey(
    shared: &Arc<Shared>,
    group: &GroupId,
    sender: &DeviceId,
    conn_id: u64,
    new_group: &GroupId,
    envelopes: &[(DeviceId, Vec<u8>)],
) -> Result<Result<(), &'static str>> {
    let limits = RekeyLimits {
        max_envelope: MAX_ENVELOPE_BYTES,
    };
    let blobs = match shared
        .store
        .rekey(group, sender, new_group, envelopes, now_ms(), &limits)?
    {
        RekeyOutcome::Done { blobs } => blobs,
        RekeyOutcome::Refused(reason) => return Ok(Err(reason)),
    };
    let woken = shared.hub.kick(group, conn_id);
    tracing::info!(
        envelopes = envelopes.len(),
        woken,
        files = blobs.len(),
        "ключ группы сменён"
    );
    if !blobs.is_empty() {
        // В фоне: под замком записи файла может сидеть загрузка этого же
        // соединения или соседнего. Она прервётся вместе с соединением, и
        // тогда файл удалится.
        tokio::spawn(remove_files(shared.clone(), *group, blobs));
    }
    Ok(Ok(()))
}

/// Убрать файлы группы с диска так же, как при отмене: под замком записи,
/// ждущие `Get` получают `Cancelled`. Строк в базе уже нет.
async fn remove_files(shared: Arc<Shared>, group: GroupId, blobs: Vec<BlobId>) {
    for blob in &blobs {
        let live = shared.blobs.live(&group, blob, 0);
        let _writer = live.writer.lock().await;
        shared.blobs.remove(&group, blob);
        live.state.send_modify(|p| p.cancelled = true);
    }
}

/// Чистка: группы со сменённым ключом старше [`ROTATED_KEEP`] удаляются
/// вместе с пакетами. Возвращает, сколько групп удалено.
pub fn purge_rotated(shared: &Shared) -> Result<usize> {
    let before = now_ms() - ROTATED_KEEP.as_millis() as i64;
    shared.store.purge_rotated(before)
}
