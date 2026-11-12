//! Фоновая проверка обновлений. Установку запускает пользователь — из меню
//! трея или из «О программе»; сами по себе обновления не ставятся.

use std::time::Duration;

use tauri::{AppHandle, Emitter};
use tauri_plugin_updater::UpdaterExt;

const FIRST_CHECK: Duration = Duration::from_secs(20);
const EVERY: Duration = Duration::from_secs(6 * 3600);

pub fn spawn(app: AppHandle) {
    tauri::async_runtime::spawn(async move {
        tokio::time::sleep(FIRST_CHECK).await;
        loop {
            match check(&app).await {
                Ok(Some(v)) => {
                    tracing::info!("доступно обновление {v}");
                    crate::tray::show_update(&app, &v);
                    let _ = app.emit("clipwire://update-available", &v);
                }
                Ok(None) => {}
                // Нет сети или GitHub недоступен — тихо, попробуем позже.
                Err(e) => tracing::debug!("проверка обновлений не удалась: {e}"),
            }
            tokio::time::sleep(EVERY).await;
        }
    });
}

async fn check(app: &AppHandle) -> Result<Option<String>, String> {
    let updater = app.updater().map_err(|e| e.to_string())?;
    let update = updater.check().await.map_err(|e| e.to_string())?;
    Ok(update.map(|u| u.version))
}

/// Скачать, проверить подпись, установить и перезапуститься.
pub async fn install(app: AppHandle) -> Result<(), String> {
    let updater = app.updater().map_err(|e| e.to_string())?;
    let Some(update) = updater.check().await.map_err(|e| e.to_string())? else {
        return Err("Обновлений нет".into());
    };
    update
        .download_and_install(|_, _| {}, || {})
        .await
        .map_err(|e| e.to_string())?;
    app.restart();
}
