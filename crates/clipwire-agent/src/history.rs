//! Локальная история буфера.
//!
//! SQLite-файл `history.db`; каждая запись зашифрована локальным ключом
//! (XChaCha20-Poly1305), ключ лежит в `history.key` под [`crate::secret`].
//! Открыто хранится только время для очистки по сроку и флаг закрепления.
//! В памяти — расшифрованный список для мгновенного поиска (история ограничена
//! сроком, так что это килобайты-мегабайты текста).

use std::collections::HashMap;
use std::path::Path;

use anyhow::{Context, Result, bail};
use chacha20poly1305::aead::{Aead, KeyInit};
use chacha20poly1305::{XChaCha20Poly1305, XNonce};
use clipwire_core::{Content, DeviceId, ItemId};
use rusqlite::{Connection, params};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::settings::Retention;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Entry {
    pub id: ItemId,
    pub origin: DeviceId,
    pub device_name: String,
    /// Скопировано на этом устройстве.
    pub local: bool,
    pub content: Content,
    /// Когда элемент последний раз появился в буфере (мс Unix).
    pub seen_ms: i64,
    pub pinned: bool,
}

pub struct History {
    conn: Connection,
    cipher: XChaCha20Poly1305,
    /// Новые первыми.
    entries: Vec<Entry>,
    by_hash: HashMap<[u8; 32], ItemId>,
}

pub fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

fn load_key(dir: &Path) -> Result<[u8; 32]> {
    let path = dir.join("history.key");
    match std::fs::read(&path) {
        Ok(raw) => {
            let k = crate::secret::unprotect(&raw)?;
            k.try_into()
                .map_err(|_| anyhow::anyhow!("history.key повреждён"))
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            std::fs::create_dir_all(dir)?;
            let k = crate::state::random_key()?;
            std::fs::write(&path, crate::secret::protect(&k)?)
                .context("не записался history.key")?;
            Ok(k)
        }
        Err(e) => Err(e).context("не читается history.key"),
    }
}

impl History {
    pub fn open(dir: &Path) -> Result<Self> {
        let key = load_key(dir)?;
        let conn = Connection::open(dir.join("history.db")).context("не открылась history.db")?;
        Self::init(conn, key)
    }

    #[cfg(test)]
    fn in_memory() -> Self {
        Self::init(Connection::open_in_memory().unwrap(), [7; 32]).unwrap()
    }

    fn init(conn: Connection, key: [u8; 32]) -> Result<Self> {
        conn.execute_batch(
            "PRAGMA journal_mode = WAL;
             CREATE TABLE IF NOT EXISTS entries (
                 id      BLOB PRIMARY KEY,
                 seen_ms INTEGER NOT NULL,
                 pinned  INTEGER NOT NULL,
                 nonce   BLOB NOT NULL,
                 data    BLOB NOT NULL
             );",
        )?;
        let cipher = XChaCha20Poly1305::new_from_slice(&key).expect("ключ 32 байта");
        let mut h = Self {
            conn,
            cipher,
            entries: Vec::new(),
            by_hash: HashMap::new(),
        };
        h.load_all()?;
        Ok(h)
    }

    fn load_all(&mut self) -> Result<()> {
        let mut stmt = self
            .conn
            .prepare("SELECT nonce, data FROM entries ORDER BY seen_ms DESC")?;
        let rows: Vec<(Vec<u8>, Vec<u8>)> = stmt
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
            .collect::<Result<_, _>>()?;
        drop(stmt);
        for (nonce, data) in rows {
            match self.decrypt(&nonce, &data) {
                Ok(e) => {
                    self.by_hash.insert(e.content.hash(), e.id);
                    self.entries.push(e);
                }
                // Не убиваем всю историю из-за одной битой записи.
                Err(err) => tracing::warn!("запись истории не расшифровалась, пропускаю: {err:#}"),
            }
        }
        Ok(())
    }

    fn decrypt(&self, nonce: &[u8], data: &[u8]) -> Result<Entry> {
        let nonce: [u8; 24] = nonce
            .try_into()
            .map_err(|_| anyhow::anyhow!("битый nonce"))?;
        let plain = self
            .cipher
            .decrypt(&XNonce::from(nonce), data)
            .map_err(|_| anyhow::anyhow!("чужой ключ или данные повреждены"))?;
        Ok(postcard::from_bytes(&plain)?)
    }

    fn write(&self, e: &Entry) -> Result<()> {
        let mut nonce = [0u8; 24];
        getrandom::fill(&mut nonce).map_err(|e| anyhow::anyhow!("нет случайных байтов: {e}"))?;
        let plain = postcard::to_stdvec(e)?;
        let data = self
            .cipher
            .encrypt(&XNonce::from(nonce), plain.as_slice())
            .map_err(|_| anyhow::anyhow!("не удалось зашифровать запись"))?;
        self.conn.execute(
            "INSERT INTO entries (id, seen_ms, pinned, nonce, data) VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT (id) DO UPDATE SET seen_ms = excluded.seen_ms, pinned = excluded.pinned,
                 nonce = excluded.nonce, data = excluded.data",
            params![&e.id.as_bytes()[..], e.seen_ms, e.pinned, &nonce[..], data],
        )?;
        Ok(())
    }

    /// Добавить элемент. То же содержимое не дублируется: существующая запись
    /// поднимается наверх с новым временем (закрепление сохраняется).
    pub fn add(&mut self, mut entry: Entry) -> Result<()> {
        let hash = entry.content.hash();
        if let Some(&old) = self.by_hash.get(&hash)
            && let Some(pos) = self.entries.iter().position(|e| e.id == old)
        {
            let mut existing = self.entries.remove(pos);
            existing.seen_ms = entry.seen_ms;
            existing.local = entry.local;
            existing.origin = entry.origin;
            existing.device_name = std::mem::take(&mut entry.device_name);
            self.write(&existing)?;
            self.entries.insert(0, existing);
            return Ok(());
        }
        self.write(&entry)?;
        self.by_hash.insert(hash, entry.id);
        self.entries.insert(0, entry);
        Ok(())
    }

    pub fn get(&self, id: ItemId) -> Option<&Entry> {
        self.entries.iter().find(|e| e.id == id)
    }

    /// Список для окна вставки: новые первыми, поиск без учёта регистра.
    pub fn list(&self, query: &str, pinned_only: bool, limit: usize) -> Vec<&Entry> {
        let q = query.trim().to_lowercase();
        self.entries
            .iter()
            .filter(|e| !pinned_only || e.pinned)
            .filter(|e| {
                q.is_empty()
                    || match &e.content {
                        Content::Text(t) => t.to_lowercase().contains(&q),
                        #[allow(unreachable_patterns)]
                        _ => false,
                    }
                    || e.device_name.to_lowercase().contains(&q)
            })
            .take(limit)
            .collect()
    }

    pub fn set_pinned(&mut self, id: ItemId, pinned: bool) -> Result<()> {
        let Some(e) = self.entries.iter_mut().find(|e| e.id == id) else {
            bail!("элемента уже нет в истории");
        };
        e.pinned = pinned;
        let e = e.clone();
        self.write(&e)
    }

    /// Заменить текст элемента (правка в предпросмотре). Если такой текст уже
    /// есть в истории отдельным элементом, тот удаляется, чтобы не было дублей.
    pub fn update_text(&mut self, id: ItemId, text: String) -> Result<()> {
        let content = Content::Text(text);
        let hash = content.hash();
        if let Some(&other) = self.by_hash.get(&hash)
            && other != id
        {
            self.delete(other)?;
        }
        let Some(e) = self.entries.iter_mut().find(|e| e.id == id) else {
            bail!("элемента уже нет в истории");
        };
        let old = e.content.hash();
        e.content = content;
        let e = e.clone();
        self.write(&e)?;
        self.by_hash.remove(&old);
        self.by_hash.insert(hash, id);
        Ok(())
    }

    pub fn delete(&mut self, id: ItemId) -> Result<()> {
        self.conn.execute(
            "DELETE FROM entries WHERE id = ?1",
            params![&id.as_bytes()[..]],
        )?;
        if let Some(pos) = self.entries.iter().position(|e| e.id == id) {
            let e = self.entries.remove(pos);
            self.by_hash.remove(&e.content.hash());
        }
        Ok(())
    }

    /// Очистить историю. Закреплённое остаётся, если `keep_pinned`.
    pub fn clear(&mut self, keep_pinned: bool) -> Result<()> {
        let ids: Vec<ItemId> = self
            .entries
            .iter()
            .filter(|e| !(keep_pinned && e.pinned))
            .map(|e| e.id)
            .collect();
        for id in ids {
            self.delete(id)?;
        }
        Ok(())
    }

    /// Удалить старше срока хранения (кроме закреплённого). Возвращает число удалённых.
    pub fn prune(&mut self, retention: Retention, now: i64) -> Result<usize> {
        let Some(max) = retention.max_age_ms() else {
            return Ok(0);
        };
        let ids: Vec<ItemId> = self
            .entries
            .iter()
            .filter(|e| !e.pinned && now - e.seen_ms > max)
            .map(|e| e.id)
            .collect();
        for id in &ids {
            self.delete(*id)?;
        }
        Ok(ids.len())
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

/// Новая запись для локального копирования.
pub fn local_entry(device: DeviceId, device_name: &str, content: Content) -> Entry {
    Entry {
        id: ItemId(Uuid::now_v7()),
        origin: device,
        device_name: device_name.to_string(),
        local: true,
        content,
        seen_ms: now_ms(),
        pinned: false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(h: &mut History, s: &str, at: i64) -> ItemId {
        let mut e = local_entry(DeviceId([1; 32]), "ПК", Content::Text(s.into()));
        e.seen_ms = at;
        let id = e.id;
        h.add(e).unwrap();
        id
    }

    fn texts(v: Vec<&Entry>) -> Vec<String> {
        v.into_iter()
            .map(|e| match &e.content {
                Content::Text(t) => t.clone(),
                #[allow(unreachable_patterns)]
                _ => String::new(),
            })
            .collect()
    }

    #[test]
    fn newest_first_and_dedup_bumps() {
        let mut h = History::in_memory();
        text(&mut h, "a", 1);
        text(&mut h, "b", 2);
        text(&mut h, "a", 3);
        assert_eq!(texts(h.list("", false, 10)), ["a", "b"]);
        assert_eq!(h.len(), 2);
    }

    #[test]
    fn search_is_case_insensitive() {
        let mut h = History::in_memory();
        text(&mut h, "Docker compose up", 1);
        text(&mut h, "пароль от wifi", 2);
        assert_eq!(texts(h.list("DOCKER", false, 10)), ["Docker compose up"]);
        assert_eq!(texts(h.list("Пароль", false, 10)), ["пароль от wifi"]);
    }

    #[test]
    fn prune_keeps_pinned_and_recent() {
        let mut h = History::in_memory();
        const DAY: i64 = 24 * 3600 * 1000;
        let old = text(&mut h, "старое", 0);
        let pinned = text(&mut h, "закреплённое", 0);
        text(&mut h, "свежее", 10 * DAY);
        h.set_pinned(pinned, true).unwrap();
        assert_eq!(h.prune(Retention::Week, 10 * DAY).unwrap(), 1);
        assert!(h.get(old).is_none());
        assert!(h.get(pinned).is_some());
        assert_eq!(h.prune(Retention::Forever, 1000 * DAY).unwrap(), 0);
    }

    #[test]
    fn persisted_encrypted_and_reloaded() {
        let dir = std::env::temp_dir().join(format!("clipwire-hist-{}", Uuid::now_v7()));
        {
            let mut h = History::open(&dir).unwrap();
            let id = text(&mut h, "секретный токен", 5);
            h.set_pinned(id, true).unwrap();
        }
        // На диске текста нет в открытом виде.
        let raw = std::fs::read(dir.join("history.db")).unwrap();
        let wal = std::fs::read(dir.join("history.db-wal")).unwrap_or_default();
        let needle = "секретный".as_bytes();
        assert!(!raw.windows(needle.len()).any(|w| w == needle));
        assert!(!wal.windows(needle.len()).any(|w| w == needle));

        let h = History::open(&dir).unwrap();
        assert_eq!(texts(h.list("", false, 10)), ["секретный токен"]);
        assert!(h.list("", true, 10)[0].pinned);
        drop(h);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn update_text_replaces_and_dedups() {
        let mut h = History::in_memory();
        let a = text(&mut h, "черновик", 1);
        text(&mut h, "итог", 2);
        h.update_text(a, "итог".into()).unwrap();
        assert_eq!(texts(h.list("", false, 10)), ["итог"]);
        assert!(h.get(a).is_some());
        // Повторное копирование того же текста поднимает отредактированный элемент.
        text(&mut h, "итог", 3);
        assert_eq!(h.len(), 1);
    }

    #[test]
    fn clear_keeps_pinned_when_asked() {
        let mut h = History::in_memory();
        let p = text(&mut h, "p", 1);
        text(&mut h, "x", 2);
        h.set_pinned(p, true).unwrap();
        h.clear(true).unwrap();
        assert_eq!(texts(h.list("", false, 10)), ["p"]);
        h.clear(false).unwrap();
        assert!(h.is_empty());
    }
}
