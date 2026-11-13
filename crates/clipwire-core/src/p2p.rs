//! Прямая передача файлов между устройствами группы (`clipwire-p2p/1`).
//!
//! Файл больше порога на сервер не загружается: получатель при вставке сам
//! подключается к устройству-источнику через iroh (напрямую или транзитом
//! через relay, на диске сервера ничего не остаётся). Каждое устройство
//! группы принимает такие соединения, но обслуживает только участников.
//!
//! Проверка участника — взаимная, по ключу, выведенному из ключа группы:
//!
//! 1. запрашивающий: [`P2pMsg::Hello`] с id группы и своим случайным вызовом;
//! 2. источник: [`P2pMsg::Challenge`] — свой вызов и доказательство знания ключа;
//! 3. запрашивающий проверяет доказательство и отвечает [`P2pMsg::Proof`];
//! 4. источник проверяет и отвечает [`P2pMsg::Accepted`] или [`P2pMsg::Denied`].
//!
//! В доказательства входят оба вызова и id обоих устройств. Эти id iroh уже
//! подтвердил по TLS, поэтому чужой ответ не переслать через третье
//! устройство. После `Accepted` запрашивающий открывает на том же соединении
//! потоки файлов с теми же кадрами, что у сервера: `BlobRequest::Get` →
//! `BlobReply::GetReady` и `BlobReply::Chunk` (файл у источника целиком,
//! поэтому `complete = true`). Части шифруются ключом файла так же, как для
//! сервера: это защита от ошибок и от relay, транспорт и так зашифрован.

use hkdf::Hkdf;
use hmac::{Hmac, KeyInit, Mac};
use serde::{Deserialize, Serialize};
use sha2::Sha256;

use crate::crypto::{GroupId, GroupKey};
use crate::item::DeviceId;

/// ALPN прямых соединений между устройствами.
pub const P2P_ALPN: &[u8] = b"clipwire-p2p/1";

const INFO_P2P_KEY: &[u8] = b"clipwire/v1/p2p-auth";
const ROLE_SOURCE: &[u8] = b"clipwire-p2p source\0";
const ROLE_REQUESTER: &[u8] = b"clipwire-p2p requester\0";

/// Кадры проверки участника (первый поток соединения).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum P2pMsg {
    Hello { group: GroupId, nonce: [u8; 32] },
    Challenge { nonce: [u8; 32], proof: [u8; 32] },
    Proof { proof: [u8; 32] },
    Accepted,
    Denied,
}

/// Ключ проверки участника. Отдельный от ключа элементов и файлов.
pub struct P2pKey([u8; 32]);

impl GroupKey {
    pub fn p2p_key(&self) -> P2pKey {
        let hk = Hkdf::<Sha256>::new(None, &self.0);
        let mut k = [0u8; 32];
        hk.expand(INFO_P2P_KEY, &mut k)
            .expect("32 байта — допустимая длина для HKDF-SHA256");
        P2pKey(k)
    }
}

/// Участники одного соединения: кто спрашивает и у кого.
#[derive(Debug, Clone, Copy)]
pub struct Peers {
    pub requester: DeviceId,
    pub source: DeviceId,
}

impl P2pKey {
    fn mac(&self, role: &[u8], first: &[u8; 32], second: &[u8; 32], peers: Peers) -> [u8; 32] {
        let mut m =
            Hmac::<Sha256>::new_from_slice(&self.0).expect("HMAC принимает ключ любой длины");
        m.update(role);
        m.update(first);
        m.update(second);
        m.update(&peers.requester.0);
        m.update(&peers.source.0);
        m.finalize().into_bytes().into()
    }

    /// Доказательство источника: отвечает на вызов запрашивающего.
    pub fn source_proof(
        &self,
        requester_nonce: &[u8; 32],
        source_nonce: &[u8; 32],
        peers: Peers,
    ) -> [u8; 32] {
        self.mac(ROLE_SOURCE, requester_nonce, source_nonce, peers)
    }

    /// Доказательство запрашивающего: отвечает на вызов источника.
    pub fn requester_proof(
        &self,
        source_nonce: &[u8; 32],
        requester_nonce: &[u8; 32],
        peers: Peers,
    ) -> [u8; 32] {
        self.mac(ROLE_REQUESTER, source_nonce, requester_nonce, peers)
    }
}

/// Сравнение доказательств за постоянное время.
pub fn proofs_equal(a: &[u8; 32], b: &[u8; 32]) -> bool {
    a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// Случайный вызов.
pub fn nonce() -> Result<[u8; 32], getrandom::Error> {
    let mut n = [0u8; 32];
    getrandom::fill(&mut n)?;
    Ok(n)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn peers() -> Peers {
        Peers {
            requester: DeviceId([1; 32]),
            source: DeviceId([2; 32]),
        }
    }

    #[test]
    fn handshake_proofs_check_out_only_for_the_same_group_and_peers() {
        let g = GroupKey([7; 32]);
        let (rn, sn) = (nonce().unwrap(), nonce().unwrap());
        // Источник и запрашивающий считают одинаково.
        let k = g.p2p_key();
        assert!(proofs_equal(
            &k.source_proof(&rn, &sn, peers()),
            &g.p2p_key().source_proof(&rn, &sn, peers())
        ));
        // Роли не взаимозаменяемы: ответ источника не годится как ответ запрашивающего.
        assert!(!proofs_equal(
            &k.source_proof(&rn, &sn, peers()),
            &k.requester_proof(&sn, &rn, peers())
        ));
        // Другая группа, другие вызовы, другая пара устройств — не сходится.
        let other = GroupKey([8; 32]).p2p_key();
        assert!(!proofs_equal(
            &k.requester_proof(&sn, &rn, peers()),
            &other.requester_proof(&sn, &rn, peers())
        ));
        assert!(!proofs_equal(
            &k.requester_proof(&sn, &rn, peers()),
            &k.requester_proof(&sn, &nonce().unwrap(), peers())
        ));
        let relayed = Peers {
            requester: DeviceId([3; 32]),
            ..peers()
        };
        assert!(!proofs_equal(
            &k.requester_proof(&sn, &rn, peers()),
            &k.requester_proof(&sn, &rn, relayed)
        ));
    }

    #[test]
    fn p2p_key_differs_from_group_ids() {
        let g = GroupKey([7; 32]);
        assert_ne!(g.p2p_key().0, g.group_id().0);
        assert_ne!(g.p2p_key().0, g.join_secret().0);
    }
}
