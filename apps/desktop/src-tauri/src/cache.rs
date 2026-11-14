//! Кэш скачанных файлов: чистка ненужных копий, размер и «Очистить кэш».
//!
//! Сама чистка — в `clipwire_agent::cache`; здесь — что оставить (файлы
//! записей истории и то, что качается сейчас) и запуск в фоне: удаление
//! больших копий не должно задерживать окно.

use clipwire_agent::i18n::trf;
use std::collections::HashSet;

use clipwire_core::BlobId;
use serde::Serialize;
use tauri::{AppHandle, Emitter, Manager};

use crate::AppState;

/// Что сейчас качается в кэш: это не трогаем ни при какой чистке.
fn busy(state: &AppState) -> HashSet<BlobId> {
    state
        .service
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .as_ref()
        .and_then(|s| s.blobs())
        .map(|b| b.downloading())
        .unwrap_or_default()
}

/// Удалить в фоне копии и забранные виртуальные файлы, на которые не
/// ссылается ни одна запись истории.
/// Зовётся после удаления записи, очистки истории, чистки по сроку и на старте.
pub fn prune_later(app: &AppHandle) {
    let state = app.state::<AppState>();
    let busy = busy(&state);
    let (keep, paths) = {
        let h = state.history.lock().unwrap_or_else(|e| e.into_inner());
        (h.blobs(), h.file_paths())
    };
    let dir = clipwire_agent::blobs::files_dir(&state.paths.dir);
    let virtual_dir = clipwire_agent::clipboard::virtual_dir(&state.paths.dir);
    std::thread::spawn(move || {
        let r = clipwire_agent::cache::prune(&dir, &keep, &busy);
        if r.removed > 0 {
            tracing::info!(removed = r.removed, "из кэша удалены ненужные копии");
        }
        // Забранные вложения Outlook: держатся, пока на них ссылается запись.
        let r = clipwire_agent::cache::prune_virtual(
            &virtual_dir,
            &paths,
            clipwire_agent::cache::VIRTUAL_GRACE,
        );
        if r.removed > 0 {
            tracing::info!(removed = r.removed, "удалены ненужные виртуальные файлы");
        }
    });
}

/// Сколько места занимает кэш скачанного, байт.
#[tauri::command]
pub async fn cache_size(app: AppHandle) -> u64 {
    let dir = clipwire_agent::blobs::files_dir(&app.state::<AppState>().paths.dir);
    tauri::async_runtime::spawn_blocking(move || clipwire_agent::cache::size(&dir))
        .await
        .unwrap_or(0)
}

#[derive(Debug, Clone, Serialize)]
pub struct CacheCleared {
    /// Сколько копий удалено.
    pub removed: usize,
    /// Сколько не удалилось: файл открыт в другой программе.
    pub failed: usize,
    /// Сколько места кэш занимает после чистки, байт.
    pub size: u64,
}

/// «Очистить кэш скачанного»: удалить всё, кроме того, что качается сейчас.
/// Записи истории остаются, их файлы скачаются заново при вставке.
#[tauri::command]
pub async fn cache_clear(app: AppHandle) -> Result<CacheCleared, String> {
    let state = app.state::<AppState>();
    let busy = busy(&state);
    let dir = clipwire_agent::blobs::files_dir(&state.paths.dir);
    let res = tauri::async_runtime::spawn_blocking(move || {
        let r = clipwire_agent::cache::clear(&dir, &busy);
        CacheCleared {
            removed: r.removed,
            failed: r.failed,
            size: clipwire_agent::cache::size(&dir),
        }
    })
    .await
    .map_err(|e| trf("Кэш не очистился: {e}", &[("e", &e)]))?;
    // Файлы записей больше не «здесь»: окно вставки перечитает список.
    let _ = app.emit("clipwire://history-changed", ());
    Ok(res)
}
