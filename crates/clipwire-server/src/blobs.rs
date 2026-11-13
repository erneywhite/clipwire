//! Части файлов на диске и уведомления о новых частях в памяти.
//!
//! Файл лежит в `<data_dir>/blobs/<group hex>/<blob hex>`: части подряд, часть
//! `i` со смещения `i * (CHUNK + OVERHEAD)`. Последняя часть короче, поэтому
//! смещения у всех частей одинаковые и докачка просто дописывает с нужного
//! места. Имена путей собираются только из разобранных id.

use std::collections::HashMap;
use std::io::SeekFrom;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, Weak};

use anyhow::{Context, Result};
use clipwire_core::blob::{CHUNK, OVERHEAD, chunk_len};
use clipwire_core::{BlobId, GroupId};
use tokio::io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt};
use tokio::sync::{Mutex as AsyncMutex, watch};

/// Место части `index` в файле на диске.
pub fn offset(index: u64) -> u64 {
    index * (CHUNK + OVERHEAD) as u64
}

/// Длина части `index` в зашифрованном виде.
pub fn sealed_chunk_len(size: u64, index: u64) -> usize {
    chunk_len(size, index) + OVERHEAD
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Состояние файла, пока с ним кто-то работает: сколько частей готово и
/// замок, чтобы файл не писали два потока сразу (например, старое соединение
/// ещё не умерло, а агент уже переподключился и докачивает).
pub struct Live {
    pub have: watch::Sender<u64>,
    pub writer: AsyncMutex<()>,
}

pub struct Blobs {
    root: PathBuf,
    live: Mutex<HashMap<(GroupId, BlobId), Weak<Live>>>,
}

impl Blobs {
    pub fn new(root: PathBuf) -> Self {
        Self {
            root,
            live: Mutex::new(HashMap::new()),
        }
    }

    pub fn path(&self, group: &GroupId, blob: &BlobId) -> PathBuf {
        self.root.join(hex(&group.0)).join(blob.hex())
    }

    /// Общее на все потоки состояние файла. Живёт, пока кто-то держит `Arc`;
    /// потом запись в карте умирает и при следующем обращении создаётся
    /// заново. `have` — сколько частей сейчас в базе: оно нужно, только если
    /// состояния ещё не было.
    pub fn live(&self, group: &GroupId, blob: &BlobId, have: u64) -> Arc<Live> {
        let mut map = self.live.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(l) = map.get(&(*group, *blob)).and_then(Weak::upgrade) {
            return l;
        }
        map.retain(|_, w| w.strong_count() > 0);
        let l = Arc::new(Live {
            have: watch::Sender::new(have),
            writer: AsyncMutex::new(()),
        });
        map.insert((*group, *blob), Arc::downgrade(&l));
        l
    }

    /// Открыть файл для записи частей. `fresh` — файл новый: старое
    /// содержимое с тем же именем (истёкший файл) выбрасывается.
    pub async fn open_write(
        &self,
        group: &GroupId,
        blob: &BlobId,
        fresh: bool,
    ) -> Result<tokio::fs::File> {
        let path = self.path(group, blob);
        let dir = path.parent().expect("у пути файла есть папка группы");
        tokio::fs::create_dir_all(dir)
            .await
            .with_context(|| format!("не создалась папка {}", dir.display()))?;
        tokio::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(fresh)
            .open(&path)
            .await
            .with_context(|| format!("не открылся на запись {}", path.display()))
    }

    // shortcut: без fsync после каждой части. Если упадёт сама ОС, хвост файла
    // может не дописаться, а have уже учтёт его, и агент получит ошибку
    // расшифровки. Переделать, если такие случаи будут на деле.
    pub async fn write_chunk(file: &mut tokio::fs::File, index: u64, data: &[u8]) -> Result<()> {
        file.seek(SeekFrom::Start(offset(index))).await?;
        file.write_all(data).await?;
        file.flush().await?;
        Ok(())
    }

    pub async fn open_read(&self, group: &GroupId, blob: &BlobId) -> Result<tokio::fs::File> {
        let path = self.path(group, blob);
        tokio::fs::File::open(&path)
            .await
            .with_context(|| format!("не открылся {}", path.display()))
    }

    pub async fn read_chunk(file: &mut tokio::fs::File, size: u64, index: u64) -> Result<Vec<u8>> {
        let mut buf = vec![0u8; sealed_chunk_len(size, index)];
        file.seek(SeekFrom::Start(offset(index))).await?;
        file.read_exact(&mut buf)
            .await
            .with_context(|| format!("часть {index} не прочиталась"))?;
        Ok(buf)
    }

    /// Убрать файл с диска (после чистки по сроку). Пустую папку группы тоже.
    pub fn remove(&self, group: &GroupId, blob: &BlobId) {
        let path = self.path(group, blob);
        if let Err(e) = std::fs::remove_file(&path)
            && e.kind() != std::io::ErrorKind::NotFound
        {
            tracing::warn!("не удалился {}: {e}", path.display());
        }
        if let Some(dir) = path.parent() {
            remove_dir_if_empty(dir);
        }
    }
}

fn remove_dir_if_empty(dir: &Path) {
    let empty = std::fs::read_dir(dir)
        .map(|mut d| d.next().is_none())
        .unwrap_or(false);
    if empty {
        let _ = std::fs::remove_dir(dir);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_root() -> PathBuf {
        std::env::temp_dir().join(format!("clipwire-blobs-{}", uuid::Uuid::now_v7()))
    }

    #[test]
    fn path_is_built_from_hex_ids() {
        let b = Blobs::new(PathBuf::from("root"));
        let p = b.path(&GroupId([0xab; 32]), &BlobId([0x01; 16]));
        assert_eq!(
            p,
            PathBuf::from("root")
                .join("ab".repeat(32))
                .join("01".repeat(16))
        );
    }

    #[test]
    fn live_state_is_shared_while_held() {
        let b = Blobs::new(temp_root());
        let (g, id) = (GroupId([1; 32]), BlobId([2; 16]));
        let a = b.live(&g, &id, 3);
        let c = b.live(&g, &id, 0);
        assert!(Arc::ptr_eq(&a, &c));
        assert_eq!(*c.have.borrow(), 3);
        drop((a, c));
        // Никто не держит — новое состояние берёт свежее значение из базы.
        assert_eq!(*b.live(&g, &id, 7).have.borrow(), 7);
    }

    #[tokio::test]
    async fn chunks_land_at_fixed_offsets_and_are_removed() {
        let root = temp_root();
        let b = Blobs::new(root.clone());
        let (g, id) = (GroupId([1; 32]), BlobId([2; 16]));
        let size = CHUNK as u64 + 10;
        let first = vec![7u8; sealed_chunk_len(size, 0)];
        let last = vec![9u8; sealed_chunk_len(size, 1)];
        assert_eq!(last.len(), 10 + OVERHEAD);

        // Пишем сначала вторую часть: место первой остаётся за ней.
        let mut f = b.open_write(&g, &id, true).await.unwrap();
        Blobs::write_chunk(&mut f, 1, &last).await.unwrap();
        Blobs::write_chunk(&mut f, 0, &first).await.unwrap();
        drop(f);

        let mut r = b.open_read(&g, &id).await.unwrap();
        assert_eq!(Blobs::read_chunk(&mut r, size, 1).await.unwrap(), last);
        assert_eq!(Blobs::read_chunk(&mut r, size, 0).await.unwrap(), first);
        drop(r);

        // Новый файл с тем же именем начинается с нуля.
        drop(b.open_write(&g, &id, true).await.unwrap());
        let mut r = b.open_read(&g, &id).await.unwrap();
        assert!(Blobs::read_chunk(&mut r, size, 0).await.is_err());
        drop(r);

        b.remove(&g, &id);
        assert!(!b.path(&g, &id).exists());
        assert!(!b.path(&g, &id).parent().unwrap().exists());
        let _ = std::fs::remove_dir_all(root);
    }
}
