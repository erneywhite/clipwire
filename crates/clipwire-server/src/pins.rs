//! Правка закрепа от устройства (`ClientMsg::PinSet`, `clipwire/8`):
//! проверка формы и размеров, запись (`store/pins.rs`), ответ отправителю и
//! рассылка `Pinned` остальным соединениям группы по `clipwire/8`.

use anyhow::Result;
use clipwire_core::GroupId;
use clipwire_core::pin::{MAX_PIN_CONTENT_BYTES, MAX_PIN_STATE_BYTES, PIN_CONTENT_SLACK};
use clipwire_core::proto::{PinFail, ServerMsg};

use crate::session::{Shared, now_ms};
use crate::store::{PinOutcome, PinWrite};

/// Сколько файлов может держать один закреп. Запись закрепа — одна запись
/// истории, а в ней столько файлов, сколько скопировали разом.
pub const MAX_PIN_BLOBS: usize = 4096;

/// Предел запечатанного содержимого: элемент, который сервер принимает в
/// историю (`CLIPWIRE_MAX_ITEM_BYTES`), с запасом на заголовок.
pub fn max_content(max_item_bytes: usize) -> usize {
    max_item_bytes
        .saturating_add(PIN_CONTENT_SLACK)
        .min(MAX_PIN_CONTENT_BYTES)
}

/// Не по правилам ли `PinSet`: сочетание полей и размеры.
fn check(w: &PinWrite<'_>, max_content: usize) -> Option<PinFail> {
    let invalid = |s: &str| Some(PinFail::Invalid(s.into()));
    if w.state.is_empty() {
        return invalid("у закрепа нет состояния");
    }
    if w.state.len() > MAX_PIN_STATE_BYTES {
        return Some(PinFail::TooLarge {
            max: MAX_PIN_STATE_BYTES as u64,
        });
    }
    if !w.pinned && (w.content.is_some() || !w.blobs.is_empty()) {
        return invalid("у снятого закрепа нет содержимого и файлов");
    }
    if w.content.is_none() && !w.blobs.is_empty() {
        return invalid("файлы закрепа задаются только вместе с содержимым");
    }
    if w.content.is_some_and(|c| c.is_empty()) {
        return invalid("пустое содержимое закрепа");
    }
    if w.content.is_some_and(|c| c.len() > max_content) {
        return Some(PinFail::TooLarge {
            max: max_content as u64,
        });
    }
    if w.blobs.len() > MAX_PIN_BLOBS {
        return invalid("у закрепа слишком много файлов");
    }
    None
}

/// Обработать `PinSet`. Возвращает ответ отправителю (`None` — ключ группы
/// сменён, соединение и так закроется по `kick`).
pub fn set(
    shared: &Shared,
    group: &GroupId,
    conn_id: u64,
    w: &PinWrite<'_>,
) -> Result<Option<ServerMsg>> {
    let pin_id = w.pin_id;
    let limits = &shared.limits;
    if let Some(reason) = check(w, max_content(limits.max_item_bytes)) {
        return Ok(Some(ServerMsg::PinFailed { pin_id, reason }));
    }
    let failed = |reason| Ok(Some(ServerMsg::PinFailed { pin_id, reason }));
    let q = limits.pin_quota;
    match shared.store.set_pin(group, w, now_ms(), q)? {
        PinOutcome::Accepted { rev } => {
            // Рассылка той же записью, что отдаёт `PinsSince`: большое
            // содержимое получатель спросит сам.
            if let Some(rec) = shared.store.pin(group, &pin_id)? {
                let n = shared
                    .hub
                    .broadcast_pins(group, conn_id, &ServerMsg::Pinned(rec));
                tracing::debug!(receivers = n, rev, "закреп сохранён");
            }
            Ok(Some(ServerMsg::PinAccepted { pin_id, rev }))
        }
        PinOutcome::Stale(current) => Ok(Some(ServerMsg::PinRejected { pin_id, current })),
        PinOutcome::TooMany => failed(PinFail::TooMany { max: q.max_pins }),
        PinOutcome::NoSpace => failed(PinFail::NoSpace { max: q.max_bytes }),
        PinOutcome::NoContent => failed(PinFail::Invalid("новый закреп без содержимого".into())),
        PinOutcome::Rotated => Ok(None),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clipwire_core::{BlobId, PinId};

    fn w<'a>(
        pinned: bool,
        state: &'a [u8],
        content: Option<&'a [u8]>,
        blobs: &'a [BlobId],
    ) -> PinWrite<'a> {
        PinWrite {
            pin_id: PinId([1; 16]),
            base_rev: 0,
            pinned,
            state,
            content,
            blobs,
        }
    }

    #[test]
    fn shape_and_size_checks() {
        let b = [BlobId([1; 16])];
        assert_eq!(check(&w(true, b"s", Some(b"c"), &b), 10), None);
        assert_eq!(check(&w(true, b"s", None, &[]), 10), None);
        assert_eq!(check(&w(false, b"s", None, &[]), 10), None);
        assert!(matches!(
            check(&w(false, b"s", Some(b"c"), &[]), 10),
            Some(PinFail::Invalid(_))
        ));
        assert!(matches!(
            check(&w(false, b"s", None, &b), 10),
            Some(PinFail::Invalid(_))
        ));
        assert!(matches!(
            check(&w(true, b"s", None, &b), 10),
            Some(PinFail::Invalid(_))
        ));
        assert!(matches!(
            check(&w(true, b"", Some(b"c"), &[]), 10),
            Some(PinFail::Invalid(_))
        ));
        assert!(matches!(
            check(&w(true, b"s", Some(b""), &[]), 10),
            Some(PinFail::Invalid(_))
        ));
        assert_eq!(
            check(&w(true, b"s", Some(&[0; 11]), &[]), 10),
            Some(PinFail::TooLarge { max: 10 })
        );
        let big_state = vec![0u8; MAX_PIN_STATE_BYTES + 1];
        assert!(matches!(
            check(&w(true, &big_state, Some(b"c"), &[]), 10),
            Some(PinFail::TooLarge { .. })
        ));
        let many = vec![BlobId([2; 16]); MAX_PIN_BLOBS + 1];
        assert!(matches!(
            check(&w(true, b"s", Some(b"c"), &many), 10),
            Some(PinFail::Invalid(_))
        ));
        assert_eq!(max_content(4096), 4096 + PIN_CONTENT_SLACK);
        assert_eq!(max_content(usize::MAX), MAX_PIN_CONTENT_BYTES);
    }
}
