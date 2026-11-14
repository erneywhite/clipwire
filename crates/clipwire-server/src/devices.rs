//! Устройства группы и смена ключа на стороне соединения (`docs/devices.md`):
//! `PublishCard`, `ListDevices`, `Rekey` и вход в группу со сменённым ключом.

use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use clipwire_core::proto::{DeviceEntry, MAX_FRAME, ServerMsg};
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
    let mut entries = shared.store.list_devices(group, &online)?;
    fit_frame(&mut entries);
    Ok(ServerMsg::Devices(entries))
}

/// Запас под служебные поля одной записи списка устройств (id, время, флаги
/// и длины в сериализации) сверх карточки.
const ENTRY_OVERHEAD: usize = 64;

/// Урезать список устройств, чтобы ответ влез в кадр: иначе `write_msg`
/// вернул бы ошибку и оборвал соединение. Предел устройств при входе
/// (`CLIPWIRE_MAX_DEVICES`) такого не допускает, но группа с устройствами,
/// набранными до него, могла перерасти кадр. Лишние отбрасываются с конца
/// списка.
/// shortcut: порядок по device_id, так что урезанными окажутся одни и те же
/// устройства; переделать, если такие группы встретятся на практике.
fn fit_frame(entries: &mut Vec<DeviceEntry>) {
    let budget = MAX_FRAME - 1024;
    let mut used = 0usize;
    let mut keep = entries.len();
    for (i, e) in entries.iter().enumerate() {
        used = used.saturating_add(e.card.len() + ENTRY_OVERHEAD);
        if used > budget {
            keep = i;
            break;
        }
    }
    if keep < entries.len() {
        tracing::warn!(
            total = entries.len(),
            kept = keep,
            "список устройств не влез в кадр, отдан урезанным"
        );
        entries.truncate(keep);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clipwire_core::proto::encode_frame;

    fn entry(n: u8, card: usize) -> DeviceEntry {
        DeviceEntry {
            device: DeviceId([n; 32]),
            card: vec![0; card],
            last_seen_ms: 0,
            online: false,
        }
    }

    #[test]
    fn oversized_device_list_is_truncated_to_fit_frame() {
        // 20 карточек по ~1 МиБ: целиком больше кадра в 16 МиБ.
        let mut entries: Vec<_> = (0..20).map(|n| entry(n, 1024 * 1024)).collect();
        assert!(encode_frame(&ServerMsg::Devices(entries.clone())).is_err());
        fit_frame(&mut entries);
        assert!(!entries.is_empty() && entries.len() < 20);
        assert!(encode_frame(&ServerMsg::Devices(entries)).is_ok());
    }

    #[test]
    fn normal_device_list_is_untouched() {
        let mut entries: Vec<_> = (0..64).map(|n| entry(n, 4096)).collect();
        fit_frame(&mut entries);
        assert_eq!(entries.len(), 64);
    }
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

/// Чистка: у групп, ключ которых сменён раньше `rotated_keep`
/// (`config::ROTATED_KEEP`, 30 дней), удаляются пакеты, карточки и
/// устройства, а строка группы остаётся надгробием. Возвращает, сколько групп
/// стало надгробиями.
pub fn purge_rotated(shared: &Shared) -> Result<usize> {
    let now = now_ms();
    let before = now - shared.limits.rotated_keep.as_millis() as i64;
    shared.store.purge_rotated(before, now)
}
