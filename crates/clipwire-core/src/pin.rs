//! Закрепы, общие для всех устройств группы (`clipwire/8`).
//!
//! Закреп на сервере — запись под случайным [`PinId`] из двух запечатанных
//! частей: состояния ([`PinState`]: какое содержимое, закреплено ли, подпись)
//! и самого содержимого (элемент истории целиком, [`Item`]). Части разделены,
//! чтобы правка подписи не гоняла по сети мегабайты картинки.
//!
//! Обе части шифруются подключом HKDF `clipwire/v1/pin`. В AAD входят id
//! группы, `pin_id` и вид части: сервер не может подставить состояние или
//! содержимое одного закрепа другому, состояние вместо содержимого или
//! запись из другой группы. Номер правки (`rev`) ставит сервер, в AAD его
//! нет: старое состояние того же закрепа сервер вернуть может, от этого
//! защищает счётчик правок `seq` внутри состояния (часы устройств в этом не
//! участвуют).
//!
//! Устройства узнают одно и то же содержимое по `Content::hash` (id записи на
//! разных устройствах разные, см. `delete.rs`), поэтому в состоянии лежит
//! хэш, а не id.

use chacha20poly1305::aead::{Aead, KeyInit, Payload};
use chacha20poly1305::{XChaCha20Poly1305, XNonce};
use hkdf::Hkdf;
use serde::{Deserialize, Serialize};
use sha2::Sha256;

use crate::crypto::{GroupId, GroupKey};
use crate::item::{DeviceId, Item};

const INFO_PIN_KEY: &[u8] = b"clipwire/v1/pin";
const AAD_PIN: &[u8] = b"clipwire/v1/pin\0";

/// Длина подписи закрепа в символах.
pub const MAX_NOTE_CHARS: usize = 80;

/// Предел запечатанного состояния. Само оно — пара сотен байт: хэш, id
/// устройства, время и подпись до 80 символов (до 320 байт UTF-8).
pub const MAX_PIN_STATE_BYTES: usize = 1024;

/// Запас запечатанного содержимого закрепа сверх предела элемента истории
/// (`CLIPWIRE_MAX_ITEM_BYTES`): nonce и заголовок. Элемент, который сервер
/// принимает в историю, влезает и в закреп.
pub const PIN_CONTENT_SLACK: usize = 1024;

/// Предел запечатанного содержимого закрепа: элемент до 8 МиБ, как в
/// истории. Влезает в кадр вместе с заголовком сообщения.
pub const MAX_PIN_CONTENT_BYTES: usize = 8 * 1024 * 1024 + PIN_CONTENT_SLACK;

const NONCE: usize = 24;
const TAG: usize = 16;

/// Случайный id закрепа. С содержимым не связан: одинаковое содержимое в
/// разных группах (и после смены ключа) по id не сопоставить.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct PinId(pub [u8; 16]);

impl PinId {
    pub fn random() -> Result<Self, PinError> {
        let mut b = [0u8; 16];
        getrandom::fill(&mut b).map_err(|_| PinError::Random)?;
        Ok(Self(b))
    }

    pub fn hex(&self) -> String {
        self.0.iter().map(|b| format!("{b:02x}")).collect()
    }
}

impl std::fmt::Debug for PinId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "PinId({})", self.hex())
    }
}

/// Состояние закрепа.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PinState {
    /// `Content::hash` закреплённого содержимого.
    pub hash: [u8; 32],
    /// `false` — закреп сняли; запись на сервере остаётся без содержимого,
    /// чтобы долго выключенное устройство не вернуло закреп.
    pub pinned: bool,
    /// Подпись, до [`MAX_NOTE_CHARS`] символов; пустая — без подписи.
    pub note: String,
    /// Когда правили, мс Unix (часы правившего устройства). Только
    /// подсказка: какую подпись оставить при схлопывании дублей. Порядок
    /// правок держат `rev` сервера и [`PinState::seq`].
    pub at_ms: i64,
    /// Номер правки этого закрепа: отправитель ставит известный `seq` + 1.
    /// Сервер принимает правку только поверх текущей записи (CAS по `rev`),
    /// поэтому у честного сервера `seq` только растёт. Состояние с меньшим
    /// `seq`, чем уже известное, — возврат старого: устройство его не
    /// применяет.
    pub seq: u64,
    /// Какое устройство правило.
    pub device: DeviceId,
}

impl PinState {
    /// Это ли содержимое закреплено: хэш в состоянии совпадает с хэшем
    /// элемента. Сервер мог отдать содержимое одного закрепа вместе с
    /// состоянием другого только сломав AAD, но устройство с ошибкой в коде
    /// могло запечатать не то — проверка дешёвая.
    pub fn matches(&self, item: &Item) -> bool {
        item.content.hash() == self.hash
    }
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum PinError {
    #[error("не удалось получить случайные байты от ОС")]
    Random,
    #[error("закреп не зашифровался")]
    Seal,
    #[error("закреп не расшифровался: чужой ключ или данные подменены")]
    Open,
    #[error("закреп расшифровался, но не разобрался")]
    Decode,
    #[error("подпись длиннее {MAX_NOTE_CHARS} символов")]
    NoteTooLong,
    #[error("закреп больше допустимого размера")]
    TooLarge,
}

/// Какая часть закрепа запечатана: входит в AAD, чтобы части не путались.
#[derive(Clone, Copy)]
enum Part {
    State,
    Content,
}

impl Part {
    fn tag(self) -> &'static [u8] {
        match self {
            Part::State => b"state",
            Part::Content => b"content",
        }
    }

    fn max(self) -> usize {
        match self {
            Part::State => MAX_PIN_STATE_BYTES,
            Part::Content => MAX_PIN_CONTENT_BYTES,
        }
    }
}

fn aad(group: &GroupId, pin: &PinId, part: Part) -> Vec<u8> {
    let tag = part.tag();
    let mut v = Vec::with_capacity(AAD_PIN.len() + tag.len() + 1 + 32 + 16);
    v.extend_from_slice(AAD_PIN);
    v.extend_from_slice(tag);
    v.push(0);
    v.extend_from_slice(&group.0);
    v.extend_from_slice(&pin.0);
    v
}

fn check_note(note: &str) -> Result<(), PinError> {
    if note.chars().count() > MAX_NOTE_CHARS {
        return Err(PinError::NoteTooLong);
    }
    Ok(())
}

impl GroupKey {
    fn pin_key(&self) -> XChaCha20Poly1305 {
        let hk = Hkdf::<Sha256>::new(None, &self.0);
        let mut k = [0u8; 32];
        hk.expand(INFO_PIN_KEY, &mut k)
            .expect("32 байта — допустимая длина для HKDF-SHA256");
        XChaCha20Poly1305::new_from_slice(&k).expect("ключ ровно 32 байта")
    }

    fn seal_pin_part(
        &self,
        group: &GroupId,
        pin: &PinId,
        part: Part,
        plain: &[u8],
    ) -> Result<Vec<u8>, PinError> {
        if plain.len() + NONCE + TAG > part.max() {
            return Err(PinError::TooLarge);
        }
        let mut nonce = [0u8; NONCE];
        getrandom::fill(&mut nonce).map_err(|_| PinError::Random)?;
        let ct = self
            .pin_key()
            .encrypt(
                &XNonce::from(nonce),
                Payload {
                    msg: plain,
                    aad: &aad(group, pin, part),
                },
            )
            .map_err(|_| PinError::Seal)?;
        let mut out = nonce.to_vec();
        out.extend_from_slice(&ct);
        Ok(out)
    }

    fn open_pin_part(
        &self,
        group: &GroupId,
        pin: &PinId,
        part: Part,
        sealed: &[u8],
    ) -> Result<Vec<u8>, PinError> {
        if sealed.len() > part.max() {
            return Err(PinError::TooLarge);
        }
        if sealed.len() < NONCE + TAG {
            return Err(PinError::Open);
        }
        let (nonce, ct) = sealed.split_at(NONCE);
        let nonce: [u8; NONCE] = nonce.try_into().expect("ровно 24 байта");
        self.pin_key()
            .decrypt(
                &XNonce::from(nonce),
                Payload {
                    msg: ct,
                    aad: &aad(group, pin, part),
                },
            )
            .map_err(|_| PinError::Open)
    }

    fn seal_pin_state_in(
        &self,
        group: &GroupId,
        pin: &PinId,
        state: &PinState,
    ) -> Result<Vec<u8>, PinError> {
        check_note(&state.note)?;
        let plain = postcard::to_stdvec(state).map_err(|_| PinError::Seal)?;
        self.seal_pin_part(group, pin, Part::State, &plain)
    }

    fn open_pin_state_in(
        &self,
        group: &GroupId,
        pin: &PinId,
        sealed: &[u8],
    ) -> Result<PinState, PinError> {
        let plain = self.open_pin_part(group, pin, Part::State, sealed)?;
        let state: PinState = postcard::from_bytes(&plain).map_err(|_| PinError::Decode)?;
        check_note(&state.note)?;
        Ok(state)
    }

    fn seal_pin_content_in(
        &self,
        group: &GroupId,
        pin: &PinId,
        item: &Item,
    ) -> Result<Vec<u8>, PinError> {
        let plain = postcard::to_stdvec(item).map_err(|_| PinError::Seal)?;
        self.seal_pin_part(group, pin, Part::Content, &plain)
    }

    fn open_pin_content_in(
        &self,
        group: &GroupId,
        pin: &PinId,
        sealed: &[u8],
    ) -> Result<Item, PinError> {
        let plain = self.open_pin_part(group, pin, Part::Content, sealed)?;
        // Закрепы появились позже поля `ext`, старых видов `Item` здесь нет.
        match postcard::take_from_bytes::<Item>(&plain) {
            Ok((item, [])) => Ok(item),
            _ => Err(PinError::Decode),
        }
    }

    /// Запечатать состояние закрепа `pin` для сервера.
    pub fn seal_pin_state(&self, pin: &PinId, state: &PinState) -> Result<Vec<u8>, PinError> {
        self.seal_pin_state_in(&self.group_id(), pin, state)
    }

    /// Открыть состояние закрепа `pin`, пришедшее с сервера.
    pub fn open_pin_state(&self, pin: &PinId, sealed: &[u8]) -> Result<PinState, PinError> {
        self.open_pin_state_in(&self.group_id(), pin, sealed)
    }

    /// Запечатать содержимое закрепа `pin`: элемент истории целиком.
    pub fn seal_pin_content(&self, pin: &PinId, item: &Item) -> Result<Vec<u8>, PinError> {
        self.seal_pin_content_in(&self.group_id(), pin, item)
    }

    /// Открыть содержимое закрепа `pin`.
    pub fn open_pin_content(&self, pin: &PinId, sealed: &[u8]) -> Result<Item, PinError> {
        self.open_pin_content_in(&self.group_id(), pin, sealed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::item::Content;

    fn state(item: &Item, note: &str) -> PinState {
        PinState {
            hash: item.content.hash(),
            pinned: true,
            note: note.into(),
            at_ms: 1_763_000_000_000,
            seq: u64::MAX,
            device: DeviceId([3; 32]),
        }
    }

    #[test]
    fn roundtrip_state_and_content() {
        let key = GroupKey::generate().unwrap();
        let pin = PinId::random().unwrap();
        let item = Item::new(DeviceId([1; 32]), Content::Text("пароль от роутера".into()));
        let st = state(&item, "роутер на даче");
        let s = key.seal_pin_state(&pin, &st).unwrap();
        let c = key.seal_pin_content(&pin, &item).unwrap();
        assert!(s.len() <= MAX_PIN_STATE_BYTES);
        let back = key.open_pin_state(&pin, &s).unwrap();
        assert_eq!(back, st);
        let item_back = key.open_pin_content(&pin, &c).unwrap();
        assert_eq!(item_back, item);
        assert!(back.matches(&item_back));
        let other = Item::new(DeviceId([1; 32]), Content::Text("другое".into()));
        assert!(!back.matches(&other));
        // Текст не виден в шифротексте.
        let needle = "роутер".as_bytes();
        assert!(!s.windows(needle.len()).any(|w| w == needle));
        assert!(!c.windows(needle.len()).any(|w| w == needle));
    }

    #[test]
    fn swapped_pin_group_or_part_fails() {
        let key = GroupKey::generate().unwrap();
        let (pin, other_pin) = (PinId::random().unwrap(), PinId::random().unwrap());
        let item = Item::new(DeviceId([1; 32]), Content::Text("x".into()));
        let s = key.seal_pin_state(&pin, &state(&item, "")).unwrap();
        let c = key.seal_pin_content(&pin, &item).unwrap();

        // Другой закреп.
        assert_eq!(key.open_pin_state(&other_pin, &s), Err(PinError::Open));
        assert_eq!(key.open_pin_content(&other_pin, &c), Err(PinError::Open));
        // Состояние вместо содержимого и наоборот.
        assert_eq!(key.open_pin_content(&pin, &s), Err(PinError::Open));
        assert_eq!(key.open_pin_state(&pin, &c), Err(PinError::Open));
        // Тот же ключ, но запись из другой группы (id группы в AAD).
        let foreign = GroupId([9; 32]);
        let s2 = key
            .seal_pin_state_in(&foreign, &pin, &state(&item, ""))
            .unwrap();
        assert_eq!(key.open_pin_state(&pin, &s2), Err(PinError::Open));
        assert!(key.open_pin_state_in(&foreign, &pin, &s2).is_ok());
        // Чужой ключ и свой подключ: записка об удалении и настройки не
        // открываются ключом закрепов и наоборот.
        let other = GroupKey::generate().unwrap();
        assert_eq!(other.open_pin_state(&pin, &s), Err(PinError::Open));
        assert!(key.open_delete(&s).is_err());
        // Испорченный шифротекст и обрубок.
        let mut bad = s.clone();
        *bad.last_mut().unwrap() ^= 1;
        assert_eq!(key.open_pin_state(&pin, &bad), Err(PinError::Open));
        assert_eq!(key.open_pin_state(&pin, &s[..10]), Err(PinError::Open));
    }

    #[test]
    fn note_and_size_limits() {
        let key = GroupKey::generate().unwrap();
        let pin = PinId::random().unwrap();
        let item = Item::new(DeviceId([1; 32]), Content::Text("x".into()));
        // 80 символов кириллицей (160 байт) можно, 81 — нет.
        let max: String = "я".repeat(MAX_NOTE_CHARS);
        let s = key.seal_pin_state(&pin, &state(&item, &max)).unwrap();
        assert!(s.len() <= MAX_PIN_STATE_BYTES, "{}", s.len());
        assert_eq!(key.open_pin_state(&pin, &s).unwrap().note, max);
        let long = "я".repeat(MAX_NOTE_CHARS + 1);
        assert_eq!(
            key.seal_pin_state(&pin, &state(&item, &long)),
            Err(PinError::NoteTooLong)
        );
        // Даже 80 четырёхбайтовых символов влезают в предел состояния.
        let emoji = "😀".repeat(MAX_NOTE_CHARS);
        let s = key.seal_pin_state(&pin, &state(&item, &emoji)).unwrap();
        assert!(s.len() <= MAX_PIN_STATE_BYTES, "{}", s.len());
        // Чужое устройство запечатало длинную подпись в обход проверки:
        // открытие её не пропускает.
        let mut st = state(&item, "");
        st.note = long;
        let plain = postcard::to_stdvec(&st).unwrap();
        let sealed = key
            .seal_pin_part(&key.group_id(), &pin, Part::State, &plain)
            .unwrap();
        assert_eq!(
            key.open_pin_state(&pin, &sealed),
            Err(PinError::NoteTooLong)
        );

        // Содержимое больше предела не запечатывается и не открывается.
        let big = Item::new(
            DeviceId([1; 32]),
            Content::Text("x".repeat(MAX_PIN_CONTENT_BYTES)),
        );
        assert_eq!(key.seal_pin_content(&pin, &big), Err(PinError::TooLarge));
        let huge = vec![0u8; MAX_PIN_CONTENT_BYTES + 1];
        assert_eq!(key.open_pin_content(&pin, &huge), Err(PinError::TooLarge));
        // Элемент около 8 МиБ влезает.
        let ok = Item::new(
            DeviceId([1; 32]),
            Content::Text("x".repeat(8 * 1024 * 1024 - 200)),
        );
        let c = key.seal_pin_content(&pin, &ok).unwrap();
        assert_eq!(key.open_pin_content(&pin, &c).unwrap(), ok);
    }

    #[test]
    fn pin_ids_are_random() {
        let a = PinId::random().unwrap();
        assert_ne!(a, PinId::random().unwrap());
        assert_eq!(a.hex().len(), 32);
    }
}
