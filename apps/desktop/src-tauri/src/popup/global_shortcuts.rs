//! Сочетание окна вставки через портал глобальных сочетаний
//! (`org.freedesktop.portal.GlobalShortcuts`).
//!
//! На Wayland глобальная клавиша программы (захват через XWayland) видит
//! нажатия только над окнами XWayland. Портал регистрирует сочетание в самой
//! оболочке: KDE при первом разе показывает окно подтверждения, дальше
//! сочетание живёт в «Комбинациях клавиш» и его можно поменять там. Сочетание
//! из настроек Clipwire передаётся как предпочтительное. Здесь — Hyprland,
//! GNOME 48+ без своего пути и другие оболочки с порталом; GNOME — через
//! `gsettings` (`gnome_shortcut`), KDE — через KGlobalAccel (`kde_shortcut`).

use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};

use ashpd::desktop::global_shortcuts::{GlobalShortcuts, NewShortcut};
use futures_util::StreamExt;
use tauri::AppHandle;

const SHORTCUT_ID: &str = "popup";

/// Сочетание зарегистрировано в оболочке.
static BOUND: AtomicBool = AtomicBool::new(false);
/// Задача, которая держит сеанс портала и слушает нажатия.
static TASK: Mutex<Option<(String, tauri::async_runtime::JoinHandle<()>)>> = Mutex::new(None);

/// Wayland, кроме GNOME (своё сочетание через `gsettings`) и KDE (через
/// KGlobalAccel: портал в Plasma 5.27 сломан, см. `kde_shortcut`).
pub fn applies() -> bool {
    std::env::var_os("WAYLAND_DISPLAY").is_some()
        && !super::gnome_shortcut::applies()
        && !super::kde_shortcut::applies()
}

/// Сочетание окна вставки работает через оболочку.
pub fn bound() -> bool {
    BOUND.load(Ordering::Relaxed)
}

/// Зарегистрировать (или перерегистрировать при смене) сочетание `accel`.
pub fn sync(app: &AppHandle, accel: &str) {
    if !applies() {
        return;
    }
    let mut task = TASK.lock().unwrap_or_else(|e| e.into_inner());
    if task.as_ref().is_some_and(|(a, _)| a == accel) {
        return;
    }
    // Сеанс закрывается вместе с задачей: новое сочетание — новый сеанс.
    if let Some((_, old)) = task.take() {
        old.abort();
        BOUND.store(false, Ordering::Relaxed);
    }
    let (app, accel_owned) = (app.clone(), accel.to_string());
    let handle = tauri::async_runtime::spawn(async move {
        if let Err(e) = run(&app, &accel_owned).await {
            tracing::info!("портал глобальных сочетаний недоступен, сочетание — вручную: {e}");
        }
        BOUND.store(false, Ordering::Relaxed);
    });
    *task = Some((accel.to_string(), handle));
}

async fn run(app: &AppHandle, accel: &str) -> Result<(), String> {
    let proxy = GlobalShortcuts::new().await.map_err(|e| e.to_string())?;
    let session = proxy
        .create_session(Default::default())
        .await
        .map_err(|e| e.to_string())?;
    let shortcut = NewShortcut::new(
        SHORTCUT_ID,
        clipwire_agent::i18n::tr("Открыть окно вставки Clipwire"),
    )
    .preferred_trigger(to_xdg(accel).as_deref());
    let bound = proxy
        .bind_shortcuts(&session, &[shortcut], None, Default::default())
        .await
        .map_err(|e| e.to_string())?
        .response()
        .map_err(|e| e.to_string())?;
    let trigger = bound
        .shortcuts()
        .iter()
        .find(|s| s.id() == SHORTCUT_ID)
        .map(|s| s.trigger_description().to_string());
    let Some(trigger) = trigger else {
        return Err("оболочка не приняла сочетание".into());
    };
    tracing::info!("сочетание окна вставки зарегистрировано в оболочке: {trigger}");
    BOUND.store(true, Ordering::Relaxed);
    let mut activated = proxy.receive_activated().await.map_err(|e| e.to_string())?;
    while let Some(ev) = activated.next().await {
        if ev.shortcut_id() == SHORTCUT_ID {
            let app2 = app.clone();
            let _ = app.run_on_main_thread(move || super::toggle(&app2));
        }
    }
    // Поток сигналов кончился — портал ушёл (перезапуск оболочки).
    drop(session);
    Err("портал закрыл сеанс".into())
}

/// Сочетание плагина global-shortcut → запись спецификации XDG shortcuts:
/// `CommandOrControl+Alt+V` → `CTRL+ALT+v`.
pub fn to_xdg(accel: &str) -> Option<String> {
    let mut parts = Vec::new();
    let mut key = None;
    for part in accel.split('+').map(str::trim) {
        match part {
            "CommandOrControl" | "CmdOrCtrl" | "Control" | "Ctrl" => parts.push("CTRL".to_string()),
            "Alt" | "Option" => parts.push("ALT".to_string()),
            "Shift" => parts.push("SHIFT".to_string()),
            "Super" | "Meta" | "Command" | "Cmd" => parts.push("LOGO".to_string()),
            k => key = Some(k),
        }
    }
    let key = match key? {
        k if k.len() == 1 && k.chars().all(|c| c.is_ascii_alphanumeric()) => k.to_lowercase(),
        k if k.starts_with('F') && k[1..].parse::<u8>().is_ok_and(|n| (1..=24).contains(&n)) => {
            k.to_string()
        }
        "Space" => "space".into(),
        "`" => "grave".into(),
        "Insert" => "Insert".into(),
        "Home" => "Home".into(),
        "End" => "End".into(),
        "PageUp" => "Page_Up".into(),
        "PageDown" => "Page_Down".into(),
        _ => return None,
    };
    parts.push(key);
    Some(parts.join("+"))
}

#[cfg(test)]
mod tests {
    use super::to_xdg;

    #[test]
    fn xdg_triggers() {
        assert_eq!(
            to_xdg("CommandOrControl+Alt+V").as_deref(),
            Some("CTRL+ALT+v")
        );
        assert_eq!(to_xdg("Super+Shift+F5").as_deref(), Some("LOGO+SHIFT+F5"));
        assert_eq!(to_xdg("Alt+Space").as_deref(), Some("ALT+space"));
        assert_eq!(to_xdg("CommandOrControl+Alt"), None);
    }
}
