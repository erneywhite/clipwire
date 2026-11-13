//! Хранилище в локальной папке. В настройках группы его нет (папка у
//! каждого устройства своя), оно нужно для тестов и как образец
//! [`ObjectStore`]: та же раскладка, что у S3 и SFTP.

use std::path::{Path, PathBuf};

use super::{ObjectInfo, ObjectStore, StoreError, StoreFuture, check_name};

pub struct DirStore {
    root: PathBuf,
}

impl DirStore {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    fn path(&self, name: &str) -> Result<PathBuf, StoreError> {
        check_name(name)?;
        Ok(self.root.join(name))
    }
}

fn io(e: std::io::Error) -> StoreError {
    match e.kind() {
        std::io::ErrorKind::PermissionDenied => StoreError::Forbidden(e.to_string()),
        _ => StoreError::Other(e.to_string()),
    }
}

fn modified_ms(m: &std::fs::Metadata) -> u64 {
    m.modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map_or(0, |d| d.as_millis() as u64)
}

fn walk(root: &Path, dir: &Path, out: &mut Vec<ObjectInfo>) -> std::io::Result<()> {
    let rd = match std::fs::read_dir(dir) {
        Ok(rd) => rd,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e),
    };
    for e in rd {
        let e = e?;
        let m = e.metadata()?;
        if m.is_dir() {
            walk(root, &e.path(), out)?;
        } else if !e.file_name().to_string_lossy().contains(".tmp-") {
            let rel = e.path();
            let rel = rel.strip_prefix(root).unwrap_or(&rel);
            let name = rel
                .components()
                .map(|c| c.as_os_str().to_string_lossy().into_owned())
                .collect::<Vec<_>>()
                .join("/");
            out.push(ObjectInfo {
                name,
                size: m.len(),
                modified_ms: modified_ms(&m),
            });
        }
    }
    Ok(())
}

impl ObjectStore for DirStore {
    fn put<'a>(&'a self, name: &'a str, data: Vec<u8>) -> StoreFuture<'a, ()> {
        Box::pin(async move {
            let path = self.path(name)?;
            if let Some(dir) = path.parent() {
                tokio::fs::create_dir_all(dir).await.map_err(io)?;
            }
            // Через временный файл: читающий не увидит полуобъекта.
            let tmp = path.with_file_name(format!(
                "{}.tmp-{}",
                path.file_name().unwrap_or_default().to_string_lossy(),
                uuid::Uuid::now_v7().simple()
            ));
            tokio::fs::write(&tmp, &data).await.map_err(io)?;
            tokio::fs::rename(&tmp, &path).await.map_err(|e| {
                let _ = std::fs::remove_file(&tmp);
                io(e)
            })
        })
    }

    fn get<'a>(&'a self, name: &'a str, max: u64) -> StoreFuture<'a, Option<Vec<u8>>> {
        Box::pin(async move {
            match tokio::fs::read(self.path(name)?).await {
                Ok(d) if d.len() as u64 > max => Err(super::too_large(name, max)),
                Ok(d) => Ok(Some(d)),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
                Err(e) => Err(io(e)),
            }
        })
    }

    fn head<'a>(&'a self, name: &'a str) -> StoreFuture<'a, Option<ObjectInfo>> {
        Box::pin(async move {
            match tokio::fs::metadata(self.path(name)?).await {
                Ok(m) => Ok(Some(ObjectInfo {
                    name: name.to_string(),
                    size: m.len(),
                    modified_ms: modified_ms(&m),
                })),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
                Err(e) => Err(io(e)),
            }
        })
    }

    fn delete<'a>(&'a self, name: &'a str) -> StoreFuture<'a, ()> {
        Box::pin(async move {
            match tokio::fs::remove_file(self.path(name)?).await {
                Ok(()) => Ok(()),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
                Err(e) => Err(io(e)),
            }
        })
    }

    fn list<'a>(&'a self, dir: &'a str) -> StoreFuture<'a, Vec<ObjectInfo>> {
        Box::pin(async move {
            let path = self.path(dir)?;
            let root = self.root.clone();
            tokio::task::spawn_blocking(move || {
                let mut out = Vec::new();
                walk(&root, &path, &mut out).map(|()| out)
            })
            .await
            .map_err(|e| StoreError::Other(e.to_string()))?
            .map_err(io)
        })
    }

    fn delete_dir<'a>(&'a self, dir: &'a str) -> StoreFuture<'a, ()> {
        Box::pin(async move {
            match tokio::fs::remove_dir_all(self.path(dir)?).await {
                Ok(()) => Ok(()),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
                Err(e) => Err(io(e)),
            }
        })
    }
}
