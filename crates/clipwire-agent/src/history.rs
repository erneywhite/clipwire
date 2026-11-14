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
use clipwire_core::item::ext;
use clipwire_core::{Content, DeviceId, Ext, ExtList, ItemId, SourceApp};
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
    /// Дополнительные сведения по меткам, как у `Item` (приложение-источник).
    pub ext: Vec<Ext>,
}

impl ExtList for Entry {
    fn ext_list(&self) -> &[Ext] {
        &self.ext
    }
    fn ext_list_mut(&mut self) -> &mut Vec<Ext> {
        &mut self.ext
    }
}

impl Entry {
    /// Приложение, из которого скопировано, если известно.
    pub fn source_app(&self) -> Option<SourceApp> {
        self.ext_get(ext::SOURCE_APP)
    }

    pub fn set_source_app(&mut self, app: &SourceApp) {
        self.ext_set(ext::SOURCE_APP, app);
    }

    /// Только имя приложения-источника: для поиска не нужно разбирать иконку.
    fn source_name(&self) -> Option<String> {
        #[derive(Deserialize)]
        struct NameOnly {
            name: String,
        }
        let e = self.ext.iter().find(|e| e.tag == ext::SOURCE_APP)?;
        postcard::take_from_bytes::<NameOnly>(&e.data)
            .ok()
            .map(|(n, _)| n.name)
    }
}

/// `Entry` до появления `ext`.
#[derive(Deserialize)]
struct EntryV3 {
    id: ItemId,
    origin: DeviceId,
    device_name: String,
    local: bool,
    content: Content,
    seen_ms: i64,
    pinned: bool,
}

impl From<EntryV3> for Entry {
    fn from(e: EntryV3) -> Self {
        Entry {
            id: e.id,
            origin: e.origin,
            device_name: e.device_name,
            local: e.local,
            content: e.content,
            seen_ms: e.seen_ms,
            pinned: e.pinned,
            ext: Vec::new(),
        }
    }
}

/// `Entry` с содержимым прошлой версии (см. `clipwire_core::item::legacy`).
#[derive(Deserialize)]
struct EntryV2 {
    id: ItemId,
    origin: DeviceId,
    device_name: String,
    local: bool,
    content: clipwire_core::item::legacy::ContentV2,
    seen_ms: i64,
    pinned: bool,
}

impl From<EntryV2> for Entry {
    fn from(e: EntryV2) -> Self {
        Entry {
            id: e.id,
            origin: e.origin,
            device_name: e.device_name,
            local: e.local,
            content: e.content.into(),
            seen_ms: e.seen_ms,
            pinned: e.pinned,
            ext: Vec::new(),
        }
    }
}

/// Фильтр окна вставки по типу записи.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    #[default]
    All,
    /// Текст, кроме одиночных ссылок.
    Text,
    /// Текст из одной ссылки http(s).
    Link,
    /// Картинки и скопированные файлы-картинки.
    Image,
    /// Файлы и папки.
    Files,
}

/// Текст целиком — одна ссылка, как `isUrl` в окне.
pub fn is_url(text: &str) -> bool {
    let t = text.trim();
    let lower = t.to_ascii_lowercase();
    (lower.starts_with("http://") || lower.starts_with("https://"))
        && t.len() > "http://".len()
        && !t.chars().any(char::is_whitespace)
}

impl Kind {
    pub fn matches(self, c: &Content) -> bool {
        match (self, c) {
            (Kind::All, _) => true,
            (Kind::Text, Content::Text(t) | Content::Rich { text: t, .. }) => !is_url(t),
            (Kind::Link, Content::Text(t) | Content::Rich { text: t, .. }) => is_url(t),
            (Kind::Image, Content::Image { .. }) => true,
            // Файл-картинка считается картинкой (так же решено для превью).
            (Kind::Image, Content::Files(f)) => !f.is_empty() && f.iter().all(|f| f.width > 0),
            (Kind::Files, Content::Files(_)) => true,
            _ => false,
        }
    }
}

pub struct History {
    conn: Connection,
    cipher: XChaCha20Poly1305,
    /// Новые первыми.
    entries: Vec<Entry>,
    by_hash: HashMap<[u8; 32], ItemId>,
    /// Миниатюры картинок (PNG) для списка в окне вставки. В базе — отдельной
    /// зашифрованной колонкой, чтобы формат самих записей не менялся.
    thumbs: HashMap<ItemId, Vec<u8>>,
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
        // Колонки миниатюр появились вместе с картинками — в старых базах их нет.
        let has_thumb: bool = conn
            .prepare("SELECT 1 FROM pragma_table_info('entries') WHERE name = 'thumb'")?
            .exists([])?;
        if !has_thumb {
            conn.execute_batch(
                "ALTER TABLE entries ADD COLUMN thumb_nonce BLOB;
                 ALTER TABLE entries ADD COLUMN thumb BLOB;",
            )?;
        }
        let cipher = XChaCha20Poly1305::new_from_slice(&key).expect("ключ 32 байта");
        let mut h = Self {
            conn,
            cipher,
            entries: Vec::new(),
            by_hash: HashMap::new(),
            thumbs: HashMap::new(),
        };
        h.load_all()?;
        Ok(h)
    }

    fn load_all(&mut self) -> Result<()> {
        type Row = (Vec<u8>, Vec<u8>, Option<Vec<u8>>, Option<Vec<u8>>);
        let mut stmt = self
            .conn
            .prepare("SELECT nonce, data, thumb_nonce, thumb FROM entries ORDER BY seen_ms DESC")?;
        let rows: Vec<Row> = stmt
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?
            .collect::<Result<_, _>>()?;
        drop(stmt);
        for (nonce, data, tn, tdata) in rows {
            match self.decrypt(&nonce, &data) {
                Ok(e) => {
                    if let (Some(tn), Some(td)) = (tn, tdata)
                        && let Ok(t) = self.open_blob(&tn, &td)
                    {
                        self.thumbs.insert(e.id, t);
                    }
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
        let plain = self.open_blob(nonce, data)?;
        // Запись прошлой версии читается по-старому и переводится. Каждый вид
        // принимаем, только если он съел все байты: запись старого формата
        // иначе могла бы разобраться со сдвинутыми полями.
        let err = match postcard::take_from_bytes::<Entry>(&plain) {
            Ok((e, [])) => return Ok(e),
            Ok(_) => postcard::Error::DeserializeBadEncoding,
            Err(e) => e,
        };
        if let Ok((e, [])) = postcard::take_from_bytes::<EntryV3>(&plain) {
            return Ok(e.into());
        }
        match postcard::take_from_bytes::<EntryV2>(&plain) {
            Ok((e, [])) => Ok(e.into()),
            _ => Err(err.into()),
        }
    }

    fn open_blob(&self, nonce: &[u8], data: &[u8]) -> Result<Vec<u8>> {
        let nonce: [u8; 24] = nonce
            .try_into()
            .map_err(|_| anyhow::anyhow!("битый nonce"))?;
        self.cipher
            .decrypt(&XNonce::from(nonce), data)
            .map_err(|_| anyhow::anyhow!("чужой ключ или данные повреждены"))
    }

    fn seal_blob(&self, plain: &[u8]) -> Result<([u8; 24], Vec<u8>)> {
        let mut nonce = [0u8; 24];
        getrandom::fill(&mut nonce).map_err(|e| anyhow::anyhow!("нет случайных байтов: {e}"))?;
        let data = self
            .cipher
            .encrypt(&XNonce::from(nonce), plain)
            .map_err(|_| anyhow::anyhow!("не удалось зашифровать"))?;
        Ok((nonce, data))
    }

    /// Сделать и сохранить миниатюру картинки. Ошибка не фатальна: в списке
    /// тогда будет значок вместо превью.
    fn store_thumb(&mut self, e: &Entry) {
        let made = match &e.content {
            Content::Image { png, .. } => crate::image::thumbnail(png),
            // Свой файл-картинка: миниатюра из файла. У чужого путь с другого
            // устройства, его миниатюра делается после скачивания (`file_thumb`).
            Content::Files(f) if e.local && f.len() == 1 && f[0].width > 0 => {
                crate::image::thumbnail_file(std::path::Path::new(&f[0].path))
            }
            _ => return,
        };
        self.save_thumb(e.id, made);
    }

    /// Миниатюра записи из скачанного файла-картинки, если её ещё нет.
    pub fn file_thumb(&mut self, id: ItemId, path: &Path) {
        if self.thumbs.contains_key(&id) || self.get(id).is_none() {
            return;
        }
        self.save_thumb(id, crate::image::thumbnail_file(path));
    }

    fn save_thumb(&mut self, id: ItemId, made: Result<Vec<u8>>) {
        let res = made.and_then(|t| {
            let (nonce, data) = self.seal_blob(&t)?;
            self.conn.execute(
                "UPDATE entries SET thumb_nonce = ?1, thumb = ?2 WHERE id = ?3",
                params![&nonce[..], data, &id.as_bytes()[..]],
            )?;
            Ok(t)
        });
        match res {
            Ok(t) => {
                self.thumbs.insert(id, t);
            }
            Err(err) => tracing::warn!("миниатюра не сделалась: {err:#}"),
        }
    }

    /// Миниатюра картинки (PNG), если есть.
    pub fn thumb(&self, id: ItemId) -> Option<&[u8]> {
        self.thumbs.get(&id).map(Vec::as_slice)
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
    /// Возвращает `true`, если появилась новая запись, и `false`, если
    /// поднялась существующая.
    pub fn add(&mut self, mut entry: Entry) -> Result<bool> {
        let hash = entry.content.hash();
        if let Some(&old) = self.by_hash.get(&hash)
            && let Some(pos) = self.entries.iter().position(|e| e.id == old)
        {
            let mut existing = self.entries.remove(pos);
            existing.seen_ms = entry.seen_ms;
            existing.local = entry.local;
            existing.origin = entry.origin;
            existing.device_name = std::mem::take(&mut entry.device_name);
            // Тот же текст скопирован уже с форматированием — берём его. Обратно
            // (вставка без форматирования) форматирование у записи не отнимаем.
            // Файлы берём новые: у них могли смениться `blob` (файл изменился
            // или запись пришла с другого устройства с теми же путями).
            if matches!(entry.content, Content::Rich { .. } | Content::Files(_)) {
                existing.content = entry.content;
            }
            // Сведения нового копирования (другое приложение-источник)
            // заменяют старые по метке; чего в новом нет, остаётся.
            for x in entry.ext {
                existing.ext.retain(|e| e.tag != x.tag);
                existing.ext.push(x);
            }
            self.write(&existing)?;
            self.entries.insert(0, existing);
            return Ok(false);
        }
        self.write(&entry)?;
        self.store_thumb(&entry);
        self.by_hash.insert(hash, entry.id);
        self.entries.insert(0, entry);
        Ok(true)
    }

    /// Влить элемент из чужой истории: если такое содержимое уже есть, ничего
    /// не меняется; иначе запись встаёт на своё место по времени, а не наверх.
    /// Возвращает `true`, если добавилось.
    pub fn merge(&mut self, entry: Entry) -> Result<bool> {
        let hash = entry.content.hash();
        if self.by_hash.contains_key(&hash) || self.get(entry.id).is_some() {
            return Ok(false);
        }
        self.write(&entry)?;
        self.store_thumb(&entry);
        self.by_hash.insert(hash, entry.id);
        let pos = self
            .entries
            .iter()
            .position(|e| e.seen_ms < entry.seen_ms)
            .unwrap_or(self.entries.len());
        self.entries.insert(pos, entry);
        Ok(true)
    }

    /// Запись с тем же содержимым (по хэшу), если есть.
    pub fn find(&self, content: &Content) -> Option<&Entry> {
        let id = *self.by_hash.get(&content.hash())?;
        self.get(id)
    }

    pub fn get(&self, id: ItemId) -> Option<&Entry> {
        self.entries.iter().find(|e| e.id == id)
    }

    /// Файл или папка своей записи, который другие устройства берут напрямую
    /// (`p2p`) по этому `blob`. Чужие записи и файлы через сервер — `None`.
    pub fn local_p2p_file(&self, blob: clipwire_core::BlobId) -> Option<clipwire_core::FileRef> {
        self.entries
            .iter()
            .filter(|e| e.local)
            .find_map(|e| match &e.content {
                Content::Files(files) => files
                    .iter()
                    .find(|f| f.p2p && f.blob == Some(blob))
                    .cloned(),
                _ => None,
            })
    }

    /// Список для окна вставки: новые первыми, поиск без учёта регистра.
    pub fn list(&self, query: &str, pinned_only: bool, limit: usize) -> Vec<&Entry> {
        self.list_kind(query, pinned_only, Kind::All, limit)
    }

    /// То же с фильтром по типу записи.
    pub fn list_kind(
        &self,
        query: &str,
        pinned_only: bool,
        kind: Kind,
        limit: usize,
    ) -> Vec<&Entry> {
        let q = query.trim().to_lowercase();
        self.entries
            .iter()
            .filter(|e| !pinned_only || e.pinned)
            .filter(|e| kind.matches(&e.content))
            .filter(|e| {
                q.is_empty()
                    || match &e.content {
                        Content::Text(t) | Content::Rich { text: t, .. } => {
                            t.to_lowercase().contains(&q)
                        }
                        // Файлы — по именам и по словам «файл», «папка».
                        Content::Files(files) => {
                            files.iter().any(|f| f.name.to_lowercase().contains(&q))
                                || ["файл", "папк", "file", "folder"]
                                    .iter()
                                    .any(|w| w.starts_with(q.as_str()) || q.starts_with(w))
                        }
                        // Картинки находятся по словам «картинка», «скрин» и т. п.
                        Content::Image { .. } => {
                            ["картин", "изображ", "скрин", "снимок", "image", "фото"]
                                .iter()
                                .any(|w| w.starts_with(q.as_str()) || q.starts_with(w))
                        }
                        #[allow(unreachable_patterns)]
                        _ => false,
                    }
                    || e.device_name.to_lowercase().contains(&q)
                    || e.source_name()
                        .is_some_and(|n| n.to_lowercase().contains(&q))
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

    /// Удаление с другого устройства: убрать записи с этими хэшами, кроме
    /// закреплённых (закрепы у каждого устройства свои) и появившихся после
    /// удаления (то же содержимое скопировали снова). Возвращает, сколько
    /// удалено.
    pub fn delete_remote(&mut self, hashes: &[[u8; 32]], at_ms: i64) -> Result<usize> {
        // Часы устройств расходятся; запас, чтобы запись, удалённую сразу
        // после копирования на другом устройстве, всё же удалить.
        const SKEW_MS: i64 = 5_000;
        let mut ids = Vec::new();
        for h in hashes {
            if let Some(&id) = self.by_hash.get(h)
                && let Some(e) = self.get(id)
                && !e.pinned
                && e.seen_ms <= at_ms.saturating_add(SKEW_MS)
            {
                ids.push(id);
            }
        }
        for id in &ids {
            self.delete(*id)?;
        }
        Ok(ids.len())
    }

    pub fn delete(&mut self, id: ItemId) -> Result<()> {
        self.conn.execute(
            "DELETE FROM entries WHERE id = ?1",
            params![&id.as_bytes()[..]],
        )?;
        if let Some(pos) = self.entries.iter().position(|e| e.id == id) {
            let e = self.entries.remove(pos);
            self.by_hash.remove(&e.content.hash());
            self.thumbs.remove(&id);
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

    /// Сколько записей закреплено.
    pub fn pinned_len(&self) -> usize {
        self.entries.iter().filter(|e| e.pinned).count()
    }

    /// Блобы файлов всех записей: скачанное по ним в кэше ещё нужно.
    pub fn blobs(&self) -> std::collections::HashSet<clipwire_core::BlobId> {
        referenced_blobs(&self.entries)
    }

    /// Пути файлов всех записей: забранные виртуальные файлы по ним ещё
    /// нужны (см. `cache::prune_virtual`).
    pub fn file_paths(&self) -> std::collections::HashSet<std::path::PathBuf> {
        self.entries
            .iter()
            .filter_map(|e| match &e.content {
                Content::Files(f) => Some(f.iter().map(|f| std::path::PathBuf::from(&f.path))),
                _ => None,
            })
            .flatten()
            .collect()
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
        ext: Vec::new(),
    }
}

/// Блобы файлов, на которые ссылаются записи (для чистки кэша скачанного).
pub fn referenced_blobs<'a>(
    entries: impl IntoIterator<Item = &'a Entry>,
) -> std::collections::HashSet<clipwire_core::BlobId> {
    entries
        .into_iter()
        .filter_map(|e| match &e.content {
            Content::Files(f) => Some(f.iter().filter_map(|f| f.blob)),
            _ => None,
        })
        .flatten()
        .collect()
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
    fn merge_places_by_time_and_skips_known_content() {
        let mut h = History::in_memory();
        text(&mut h, "старое", 10);
        text(&mut h, "новое", 30);
        let mut e = local_entry(DeviceId([2; 32]), "Mac", Content::Text("с мака".into()));
        e.seen_ms = 20;
        e.local = false;
        assert!(h.merge(e).unwrap());
        assert_eq!(texts(h.list("", false, 10)), ["новое", "с мака", "старое"]);
        // То же содержимое не дублируется и не поднимается.
        let mut dup = local_entry(DeviceId([2; 32]), "Mac", Content::Text("старое".into()));
        dup.seen_ms = 99;
        assert!(!h.merge(dup).unwrap());
        assert_eq!(texts(h.list("", false, 10)), ["новое", "с мака", "старое"]);
    }

    #[test]
    fn rich_copy_upgrades_plain_entry_and_plain_does_not_downgrade() {
        let mut h = History::in_memory();
        text(&mut h, "жирный", 1);
        let rich = Content::Rich {
            text: "жирный".into(),
            html: Some("<b>жирный</b>".into()),
            rtf: None,
        };
        let mut e = local_entry(DeviceId([1; 32]), "ПК", rich.clone());
        e.seen_ms = 2;
        h.add(e).unwrap();
        assert_eq!(h.len(), 1);
        assert_eq!(h.list("", false, 1)[0].content, rich);
        text(&mut h, "жирный", 3);
        assert_eq!(h.list("", false, 1)[0].content, rich);
        assert_eq!(h.list("ЖИР", false, 5).len(), 1);
    }

    #[test]
    fn files_are_found_by_name_and_word() {
        let mut h = History::in_memory();
        let f = |name: &str| clipwire_core::FileRef {
            name: name.into(),
            path: format!("C:\\tmp\\{name}"),
            size: 10,
            is_dir: false,
            width: 0,
            height: 0,
            modified_ms: 0,
            blob: None,
            blob_size: 0,
            p2p: false,
        };
        let e = local_entry(
            DeviceId([1; 32]),
            "ПК",
            Content::Files(vec![f("Отчёт.pdf"), f("смета.xlsx")]),
        );
        h.add(e).unwrap();
        text(&mut h, "просто текст", 2);
        assert_eq!(h.list("СМЕТА", false, 5).len(), 1);
        assert_eq!(h.list("файл", false, 5).len(), 1);
        assert_eq!(h.list("текст", false, 5).len(), 1);
    }

    #[test]
    fn entry_of_previous_version_is_read_and_converted() {
        // Запись в виде части 2–3: у файла ещё нет blob_size и p2p.
        #[derive(Serialize)]
        struct FileV2 {
            name: String,
            path: String,
            size: u64,
            is_dir: bool,
            width: u32,
            height: u32,
            modified_ms: u64,
            blob: Option<clipwire_core::BlobId>,
        }
        #[derive(Serialize)]
        #[allow(dead_code)]
        enum ContentV2 {
            Text(String),
            Image {
                width: u32,
                height: u32,
                png: Vec<u8>,
            },
            Rich {
                text: String,
                html: Option<String>,
                rtf: Option<Vec<u8>>,
            },
            Files(Vec<FileV2>),
        }
        #[derive(Serialize)]
        struct EntryV2Out {
            id: ItemId,
            origin: DeviceId,
            device_name: String,
            local: bool,
            content: ContentV2,
            seen_ms: i64,
            pinned: bool,
        }
        let blob = clipwire_core::BlobId([5; 16]);
        let old = EntryV2Out {
            id: ItemId::new(),
            origin: DeviceId([1; 32]),
            device_name: "ПК".into(),
            local: true,
            content: ContentV2::Files(vec![FileV2 {
                name: "отчёт.pdf".into(),
                path: r"C:\отчёт.pdf".into(),
                size: 1234,
                is_dir: false,
                width: 0,
                height: 0,
                modified_ms: 7,
                blob: Some(blob),
            }]),
            seen_ms: 42,
            pinned: true,
        };
        let dir = std::env::temp_dir().join(format!("clipwire-legacy-{}", Uuid::now_v7()));
        {
            let h = History::open(&dir).unwrap();
            let (nonce, data) = h.seal_blob(&postcard::to_stdvec(&old).unwrap()).unwrap();
            h.conn
                .execute(
                    "INSERT INTO entries (id, seen_ms, pinned, nonce, data) VALUES (?1, ?2, ?3, ?4, ?5)",
                    params![&old.id.as_bytes()[..], 42i64, true, &nonce[..], data],
                )
                .unwrap();
        }
        let h = History::open(&dir).unwrap();
        let e = h.get(old.id).expect("старая запись прочиталась");
        assert!(e.pinned);
        assert_eq!(e.seen_ms, 42);
        match &e.content {
            Content::Files(f) => {
                assert_eq!(f[0].name, "отчёт.pdf");
                assert_eq!(f[0].blob, Some(blob));
                assert_eq!(f[0].blob_size, 1234);
                assert!(!f[0].p2p);
            }
            other => panic!("ждали файлы, пришло {other:?}"),
        }
        drop(h);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn entry_without_ext_is_read() {
        // Запись до появления `ext`: те же поля, кроме последнего.
        #[derive(Serialize)]
        struct EntryV3Out {
            id: ItemId,
            origin: DeviceId,
            device_name: String,
            local: bool,
            content: Content,
            seen_ms: i64,
            pinned: bool,
        }
        let old = EntryV3Out {
            id: ItemId::new(),
            origin: DeviceId([3; 32]),
            device_name: "Mac".into(),
            local: false,
            content: Content::Text("старая запись".into()),
            seen_ms: 77,
            pinned: true,
        };
        let h = History::in_memory();
        let (nonce, data) = h.seal_blob(&postcard::to_stdvec(&old).unwrap()).unwrap();
        let e = h.decrypt(&nonce, &data).expect("старая запись прочиталась");
        assert_eq!(e.id, old.id);
        assert_eq!(e.content, Content::Text("старая запись".into()));
        assert!(e.pinned && e.ext.is_empty() && e.source_app().is_none());

        // Новая запись с ext читается как новая, а не как старая с хвостом.
        let mut new = local_entry(DeviceId([1; 32]), "ПК", Content::Text("новая".into()));
        new.set_source_app(&app("Блокнот"));
        let (nonce, data) = h.seal_blob(&postcard::to_stdvec(&new).unwrap()).unwrap();
        assert_eq!(h.decrypt(&nonce, &data).unwrap(), new);
    }

    fn app(name: &str) -> SourceApp {
        SourceApp {
            name: name.into(),
            exe: format!("{name}.exe"),
            icon_png: vec![1, 2, 3],
        }
    }

    #[test]
    fn source_app_is_kept_found_and_replaced() {
        let dir = std::env::temp_dir().join(format!("clipwire-src-{}", Uuid::now_v7()));
        {
            let mut h = History::open(&dir).unwrap();
            let mut e = local_entry(DeviceId([1; 32]), "ПК", Content::Text("абв".into()));
            e.set_source_app(&app("Telegram"));
            assert!(h.add(e).unwrap(), "новая запись");
            text(&mut h, "другое", 5);
            assert_eq!(texts(h.list("telegr", false, 10)), ["абв"]);
            assert!(h.list("notepad", false, 10).is_empty());

            // Повтор того же содержимого — не новая запись; без источника
            // старый источник остаётся.
            let again = local_entry(DeviceId([1; 32]), "ПК", Content::Text("абв".into()));
            assert!(!h.add(again).unwrap());
            assert_eq!(h.len(), 2);
            assert_eq!(
                h.list("абв", false, 1)[0].source_app(),
                Some(app("Telegram"))
            );

            // Скопировали то же из другой программы — источник новый.
            let mut other = local_entry(DeviceId([1; 32]), "ПК", Content::Text("абв".into()));
            other.set_source_app(&app("Блокнот"));
            assert!(!h.add(other).unwrap());
        }
        let h = History::open(&dir).unwrap();
        let e = h.list("блокн", false, 10);
        assert_eq!(e.len(), 1);
        assert_eq!(e[0].source_app(), Some(app("Блокнот")));
        assert!(h.list("telegram", false, 10).is_empty());
        drop(h);
        let _ = std::fs::remove_dir_all(dir);
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
    fn image_gets_encrypted_thumbnail_that_survives_reopen() {
        let dir = std::env::temp_dir().join(format!("clipwire-thumb-{}", Uuid::now_v7()));
        let px: Vec<u8> = (0..800 * 600)
            .flat_map(|i| [(i % 251) as u8, 120, 200, 255])
            .collect();
        let content = crate::image::content_from_rgba(800, 600, &px).unwrap();
        let id;
        {
            let mut h = History::open(&dir).unwrap();
            let e = local_entry(DeviceId([1; 32]), "ПК", content.clone());
            id = e.id;
            h.add(e).unwrap();
            let t = crate::image::decode_png(h.thumb(id).unwrap()).unwrap();
            assert_eq!(t.dimensions(), (320, 240));
            // Картинка находится поиском по слову «скрин».
            assert_eq!(h.list("скрин", false, 10).len(), 1);
        }
        let h = History::open(&dir).unwrap();
        assert!(h.thumb(id).is_some());
        assert_eq!(h.get(id).unwrap().content, content);
        drop(h);
        let _ = std::fs::remove_dir_all(dir);
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

    #[test]
    fn kind_filter() {
        use clipwire_core::FileRef;
        let file = |name: &str, width: u32| FileRef {
            name: name.into(),
            path: format!("/tmp/{name}"),
            size: 1,
            is_dir: false,
            width,
            height: width,
            modified_ms: 0,
            blob: None,
            blob_size: 0,
            p2p: false,
        };
        let text = Content::Text("привет".into());
        let link = Content::Text(" https://example.com/a?b=1 ".into());
        let two_links = Content::Text("https://a.com https://b.com".into());
        let img = Content::Image {
            width: 1,
            height: 1,
            png: vec![1],
        };
        let pic = Content::Files(vec![file("a.png", 10)]);
        let doc = Content::Files(vec![file("a.pdf", 0)]);
        assert!(Kind::Text.matches(&text) && !Kind::Link.matches(&text));
        assert!(Kind::Link.matches(&link) && !Kind::Text.matches(&link));
        assert!(Kind::Text.matches(&two_links), "две ссылки — уже текст");
        assert!(Kind::Image.matches(&img) && Kind::Image.matches(&pic));
        assert!(!Kind::Image.matches(&doc));
        assert!(Kind::Files.matches(&pic) && Kind::Files.matches(&doc));
        assert!(!Kind::Files.matches(&img));
        for c in [&text, &link, &img, &pic, &doc] {
            assert!(Kind::All.matches(c));
        }
    }

    #[test]
    fn delete_remote_skips_pinned_and_newer() {
        let dir = std::env::temp_dir().join(format!("clipwire-delrem-{}", uuid::Uuid::now_v7()));
        let mut h = History::open(&dir).unwrap();
        let old = text(&mut h, "старое", 1_000);
        let pinned = text(&mut h, "закреп", 1_000);
        h.set_pinned(pinned, true).unwrap();
        let newer = text(&mut h, "снова", 100_000);
        let hashes: Vec<_> = ["старое", "закреп", "снова", "нет такого"]
            .iter()
            .map(|t| Content::Text((*t).into()).hash())
            .collect();
        assert_eq!(h.delete_remote(&hashes, 50_000).unwrap(), 1);
        assert!(h.get(old).is_none());
        assert!(h.get(pinned).is_some(), "закреплённое не трогаем");
        assert!(
            h.get(newer).is_some(),
            "скопированное после удаления остаётся"
        );
        let _ = std::fs::remove_dir_all(dir);
    }
}
