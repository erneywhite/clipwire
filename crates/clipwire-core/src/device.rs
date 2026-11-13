//! Устройства группы и смена ключа группы.
//!
//! **Карточка устройства.** Каждое устройство публикует на сервере карточку:
//! имя, ОС и открытый ключ для получения нового ключа группы ([`DeviceCard`]).
//! Карточка подписана ключом устройства (Ed25519 — тот же ключ, что id
//! эндпоинта iroh, то есть `DeviceId`) и зашифрована ключом группы. Сервер
//! содержимого не видит, а подменить чужую карточку не может ни сервер, ни
//! другой участник: подпись проверяется по id устройства.
//!
//! **Смена ключа.** Чтобы исключить устройство (украли, продали), любое
//! оставшееся создаёт новый ключ группы и упаковывает его отдельно для каждого
//! остающегося устройства: X25519 эфемерного ключа с ключом из карточки →
//! HKDF → XChaCha20-Poly1305 ([`seal_rekey`]). Исключённое устройство пакета
//! не получает. Старая группа на сервере переходит в режим «ключ сменён»:
//! элементы и файлы удаляются, входящему устройству отдаётся только его пакет.
//!
//! Ключ для приёма пакетов выводится из ключа устройства через HKDF
//! ([`rekey_secret`]), отдельно хранить его не нужно.

use chacha20poly1305::aead::{Aead, KeyInit, Payload};
use chacha20poly1305::{XChaCha20Poly1305, XNonce};
use curve25519_dalek::montgomery::MontgomeryPoint;
use ed25519_dalek::{Signer, SigningKey, VerifyingKey};
use hkdf::Hkdf;
use serde::{Deserialize, Serialize};
use sha2::Sha256;

use crate::crypto::{GroupId, GroupKey};
use crate::item::DeviceId;

const INFO_CARD_KEY: &[u8] = b"clipwire/v1/card-key";
const INFO_REKEY_SECRET: &[u8] = b"clipwire/v1/rekey-secret";
const INFO_REKEY_WRAP: &[u8] = b"clipwire/v1/rekey-wrap";
const SIGN_CARD: &[u8] = b"clipwire/v1/device-card\0";
const SIGN_REKEY: &[u8] = b"clipwire/v1/rekey\0";
const INFO_REKEY_BIND: &[u8] = b"clipwire/v1/rekey-bind";

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum DeviceError {
    #[error("не удалось получить случайные байты от ОС")]
    Random,
    #[error("не удалось зашифровать")]
    Seal,
    #[error("не расшифровалось: чужой ключ или данные подменены")]
    Open,
    #[error("подпись карточки не сходится с id устройства")]
    BadSignature,
    #[error("данные не разобрались")]
    Decode,
}

/// Сведения об устройстве, которые видят остальные участники группы.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeviceCard {
    pub device: DeviceId,
    /// Имя устройства из настроек.
    pub name: String,
    /// «Windows», «macOS», «Linux».
    pub os: String,
    /// Открытый ключ X25519 для получения нового ключа группы.
    pub rekey_pub: [u8; 32],
    /// Когда карточка выпущена (мс Unix): новая заменяет старую.
    pub issued_ms: u64,
}

/// Карточка с подписью устройства.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct SignedCard {
    card: Vec<u8>,
    sig: Vec<u8>,
}

fn derive(key: &[u8; 32], info: &[u8]) -> [u8; 32] {
    let hk = Hkdf::<Sha256>::new(None, key);
    let mut out = [0u8; 32];
    hk.expand(info, &mut out)
        .expect("32 байта — допустимая длина для HKDF-SHA256");
    out
}

fn random<const N: usize>() -> Result<[u8; N], DeviceError> {
    let mut b = [0u8; N];
    getrandom::fill(&mut b).map_err(|_| DeviceError::Random)?;
    Ok(b)
}

fn seal_with(key: &[u8; 32], aad: &[u8], plain: &[u8]) -> Result<Vec<u8>, DeviceError> {
    let nonce: [u8; 24] = random()?;
    let ct = XChaCha20Poly1305::new_from_slice(key)
        .expect("ключ ровно 32 байта")
        .encrypt(&XNonce::from(nonce), Payload { msg: plain, aad })
        .map_err(|_| DeviceError::Seal)?;
    let mut out = nonce.to_vec();
    out.extend_from_slice(&ct);
    Ok(out)
}

fn open_with(key: &[u8; 32], aad: &[u8], sealed: &[u8]) -> Result<Vec<u8>, DeviceError> {
    if sealed.len() < 24 + 16 {
        return Err(DeviceError::Open);
    }
    let (nonce, ct) = sealed.split_at(24);
    let nonce: [u8; 24] = nonce.try_into().expect("ровно 24 байта");
    XChaCha20Poly1305::new_from_slice(key)
        .expect("ключ ровно 32 байта")
        .decrypt(&XNonce::from(nonce), Payload { msg: ct, aad })
        .map_err(|_| DeviceError::Open)
}

/// Секрет X25519 этого устройства для приёма нового ключа группы.
/// `device_key` — секретный ключ устройства (тот же, что у эндпоинта iroh).
pub fn rekey_secret(device_key: &[u8; 32]) -> [u8; 32] {
    derive(device_key, INFO_REKEY_SECRET)
}

/// Открытый ключ к [`rekey_secret`] — для карточки.
pub fn rekey_public(device_key: &[u8; 32]) -> [u8; 32] {
    MontgomeryPoint::mul_base_clamped(rekey_secret(device_key)).to_bytes()
}

/// Id устройства по его секретному ключу (открытый ключ Ed25519).
pub fn device_id(device_key: &[u8; 32]) -> DeviceId {
    DeviceId(
        SigningKey::from_bytes(device_key)
            .verifying_key()
            .to_bytes(),
    )
}

impl GroupKey {
    fn card_key(&self) -> [u8; 32] {
        derive(&self.0, INFO_CARD_KEY)
    }

    /// Подписать карточку ключом устройства и зашифровать ключом группы.
    pub fn seal_card(
        &self,
        card: &DeviceCard,
        device_key: &[u8; 32],
    ) -> Result<Vec<u8>, DeviceError> {
        let bytes = postcard::to_stdvec(card).map_err(|_| DeviceError::Seal)?;
        let mut msg = SIGN_CARD.to_vec();
        msg.extend_from_slice(&bytes);
        let sig = SigningKey::from_bytes(device_key)
            .sign(&msg)
            .to_bytes()
            .to_vec();
        let signed =
            postcard::to_stdvec(&SignedCard { card: bytes, sig }).map_err(|_| DeviceError::Seal)?;
        seal_with(&self.card_key(), &card.device.0, &signed)
    }

    /// Расшифровать карточку устройства `device` (id из сервера) и проверить,
    /// что её подписало именно оно.
    pub fn open_card(&self, device: DeviceId, sealed: &[u8]) -> Result<DeviceCard, DeviceError> {
        let signed = open_with(&self.card_key(), &device.0, sealed)?;
        let signed: SignedCard = postcard::from_bytes(&signed).map_err(|_| DeviceError::Decode)?;
        let vk = VerifyingKey::from_bytes(&device.0).map_err(|_| DeviceError::BadSignature)?;
        let sig: [u8; 64] = signed
            .sig
            .as_slice()
            .try_into()
            .map_err(|_| DeviceError::BadSignature)?;
        let mut msg = SIGN_CARD.to_vec();
        msg.extend_from_slice(&signed.card);
        vk.verify_strict(&msg, &ed25519_dalek::Signature::from_bytes(&sig))
            .map_err(|_| DeviceError::BadSignature)?;
        let card: DeviceCard =
            postcard::from_bytes(&signed.card).map_err(|_| DeviceError::Decode)?;
        if card.device != device {
            return Err(DeviceError::BadSignature);
        }
        Ok(card)
    }
}

/// Секрет текущей группы, который замешивается в ключ обёртки пакета: без
/// текущего ключа группы пакет не собрать. Так исключённое устройство, не
/// знающее нового ключа, не подделает следующую смену ключа даже в сговоре
/// с сервером.
fn rekey_bind(old_key: &GroupKey) -> [u8; 32] {
    derive(&old_key.0, INFO_REKEY_BIND)
}

fn wrap_key(shared: &[u8; 32], eph: &[u8; 32], old_key: &GroupKey, ctx: &[u8]) -> [u8; 32] {
    let mut ikm = shared.to_vec();
    ikm.extend_from_slice(&rekey_bind(old_key));
    let hk = Hkdf::<Sha256>::new(Some(eph), &ikm);
    let mut info = INFO_REKEY_WRAP.to_vec();
    info.extend_from_slice(ctx);
    let mut k = [0u8; 32];
    hk.expand(&info, &mut k)
        .expect("32 байта — допустимая длина");
    k
}

/// Контекст пакета: кому, из какой группы, в какую, кто меняет ключ.
fn rekey_ctx(to: DeviceId, old: GroupId, new: GroupId, initiator: DeviceId) -> Vec<u8> {
    let mut v = Vec::with_capacity(128);
    v.extend_from_slice(&to.0);
    v.extend_from_slice(&old.0);
    v.extend_from_slice(&new.0);
    v.extend_from_slice(&initiator.0);
    v
}

/// Длина заголовка пакета: эфемерный ключ, id новой группы, инициатор, подпись.
const REKEY_HEAD: usize = 32 + 32 + 32 + 64;

/// Новый ключ группы из пакета и устройство, которое его выпустило.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rekey {
    pub key: GroupKey,
    /// Кто сменил ключ. Подпись пакета проверена этим id; получатель должен
    /// ещё убедиться, что это устройство его группы (по карточкам), и
    /// показать человеку, кто сменил ключ.
    pub initiator: DeviceId,
}

/// Упаковать новый ключ группы для устройства `to` по его ключу из карточки.
///
/// Пакет привязан к текущему ключу группы `old_key` (без него ключ обёртки не
/// получить), к старой и новой группе, к адресату и подписан ключом
/// устройства-инициатора `initiator_key`.
///
/// Формат: `eph_pub (32) | id новой группы (32) | инициатор (32) | подпись (64) | шифротекст`.
pub fn seal_rekey(
    new_key: &GroupKey,
    old_key: &GroupKey,
    initiator_key: &[u8; 32],
    to: DeviceId,
    to_rekey_pub: &[u8; 32],
) -> Result<Vec<u8>, DeviceError> {
    let eph_secret: [u8; 32] = random()?;
    let eph_pub = MontgomeryPoint::mul_base_clamped(eph_secret).to_bytes();
    let shared = MontgomeryPoint(*to_rekey_pub)
        .mul_clamped(eph_secret)
        .to_bytes();
    if shared == [0u8; 32] {
        return Err(DeviceError::Seal);
    }
    let (old, new) = (old_key.group_id(), new_key.group_id());
    let initiator = device_id(initiator_key);
    let ctx = rekey_ctx(to, old, new, initiator);
    let key = wrap_key(&shared, &eph_pub, old_key, &ctx);
    let sealed = seal_with(&key, &ctx, &new_key.0)?;
    let mut signed = SIGN_REKEY.to_vec();
    signed.extend_from_slice(&ctx);
    signed.extend_from_slice(&eph_pub);
    signed.extend_from_slice(&sealed);
    let sig = SigningKey::from_bytes(initiator_key)
        .sign(&signed)
        .to_bytes();
    let mut out = Vec::with_capacity(REKEY_HEAD + sealed.len());
    out.extend_from_slice(&eph_pub);
    out.extend_from_slice(&new.0);
    out.extend_from_slice(&initiator.0);
    out.extend_from_slice(&sig);
    out.extend_from_slice(&sealed);
    Ok(out)
}

/// Открыть пакет, адресованный этому устройству: нужен свой ключ устройства и
/// текущий ключ группы. Проверяет подпись инициатора и то, что `group_id`
/// нового ключа совпадает с указанным в пакете.
pub fn open_rekey(
    device_key: &[u8; 32],
    old_key: &GroupKey,
    envelope: &[u8],
) -> Result<Rekey, DeviceError> {
    if envelope.len() < REKEY_HEAD + 24 + 16 {
        return Err(DeviceError::Open);
    }
    let eph_pub: [u8; 32] = envelope[..32].try_into().expect("32 байта");
    let new = GroupId(envelope[32..64].try_into().expect("32 байта"));
    let initiator = DeviceId(envelope[64..96].try_into().expect("32 байта"));
    let sig: [u8; 64] = envelope[96..160].try_into().expect("64 байта");
    let sealed = &envelope[REKEY_HEAD..];
    let me = device_id(device_key);
    let ctx = rekey_ctx(me, old_key.group_id(), new, initiator);

    let mut signed = SIGN_REKEY.to_vec();
    signed.extend_from_slice(&ctx);
    signed.extend_from_slice(&eph_pub);
    signed.extend_from_slice(sealed);
    VerifyingKey::from_bytes(&initiator.0)
        .map_err(|_| DeviceError::BadSignature)?
        .verify_strict(&signed, &ed25519_dalek::Signature::from_bytes(&sig))
        .map_err(|_| DeviceError::BadSignature)?;

    let shared = MontgomeryPoint(eph_pub)
        .mul_clamped(rekey_secret(device_key))
        .to_bytes();
    if shared == [0u8; 32] {
        return Err(DeviceError::Open);
    }
    let key = wrap_key(&shared, &eph_pub, old_key, &ctx);
    let raw = open_with(&key, &ctx, sealed)?;
    let k: [u8; 32] = raw.as_slice().try_into().map_err(|_| DeviceError::Decode)?;
    let gk = GroupKey(k);
    if gk.group_id() != new {
        return Err(DeviceError::Open);
    }
    Ok(Rekey { key: gk, initiator })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn card(dk: &[u8; 32], name: &str) -> DeviceCard {
        DeviceCard {
            device: device_id(dk),
            name: name.into(),
            os: "Windows".into(),
            rekey_pub: rekey_public(dk),
            issued_ms: 1,
        }
    }

    #[test]
    fn card_roundtrip_and_forgery_is_caught() {
        let g = GroupKey([3; 32]);
        let (a, b) = ([1u8; 32], [2u8; 32]);
        let sealed = g.seal_card(&card(&a, "Ноутбук"), &a).unwrap();
        assert_eq!(g.open_card(device_id(&a), &sealed).unwrap().name, "Ноутбук");
        // Сервер выдаёт карточку A за карточку B — не сходится.
        assert!(g.open_card(device_id(&b), &sealed).is_err());
        // Участник B подписывает карточку от имени A своим ключом — подпись не та.
        let forged = g.seal_card(&card(&a, "подделка"), &b).unwrap();
        assert_eq!(
            g.open_card(device_id(&a), &forged),
            Err(DeviceError::BadSignature)
        );
        // Чужая группа карточку не открывает.
        assert!(GroupKey([4; 32]).open_card(device_id(&a), &sealed).is_err());
    }

    #[test]
    fn rekey_reaches_only_addressee() {
        let old_key = GroupKey([3; 32]);
        let new_key = GroupKey::generate().unwrap();
        let (a, b, r) = ([1u8; 32], [2u8; 32], [5u8; 32]);
        let env = seal_rekey(&new_key, &old_key, &r, device_id(&a), &rekey_public(&a)).unwrap();
        let got = open_rekey(&a, &old_key, &env).unwrap();
        assert_eq!(got.key, new_key);
        assert_eq!(got.initiator, device_id(&r));
        // Чужое устройство пакет не откроет.
        assert!(open_rekey(&b, &old_key, &env).is_err());
        // С другим текущим ключом группы пакет не открывается.
        assert!(open_rekey(&a, &GroupKey([9; 32]), &env).is_err());
        // Подмена id новой группы, инициатора, подписи или шифротекста ломает пакет.
        for at in [40usize, 70, 120, 200] {
            let mut bad = env.clone();
            bad[at] ^= 1;
            assert!(open_rekey(&a, &old_key, &bad).is_err(), "байт {at}");
        }
        assert!(open_rekey(&a, &old_key, &env[..150]).is_err());
    }

    #[test]
    fn rekey_without_current_group_key_is_rejected() {
        // Исключённое устройство X знает rekey_pub получателя по старым
        // карточкам, но не знает текущего ключа группы: его пакет не откроется,
        // даже если подписан честно его ключом.
        let current = GroupKey([3; 32]);
        let stale = GroupKey([4; 32]);
        let x_new = GroupKey::generate().unwrap();
        let (a, x) = ([1u8; 32], [6u8; 32]);
        let forged = seal_rekey(&x_new, &stale, &x, device_id(&a), &rekey_public(&a)).unwrap();
        assert!(open_rekey(&a, &current, &forged).is_err());
    }

    #[test]
    fn rekey_keys_are_stable_and_distinct() {
        let a = [1u8; 32];
        assert_eq!(rekey_public(&a), rekey_public(&a));
        assert_ne!(rekey_public(&a), rekey_public(&[2u8; 32]));
        assert_ne!(rekey_secret(&a), a);
    }
}
