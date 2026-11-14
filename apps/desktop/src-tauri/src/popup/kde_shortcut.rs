//! Сочетание окна вставки в KDE Plasma на Wayland — через службу сочетаний KDE
//! (KGlobalAccel, D-Bus `org.kde.kglobalaccel`), как у программ самого KDE.
//!
//! Глобальная клавиша программы на Wayland видит нажатия только над окнами
//! XWayland. Портал глобальных сочетаний в Plasma 5.27 сломан (служба портала
//! не передаёт сочетания оболочке), а KGlobalAccel одинаков в Plasma 5 и 6 и
//! работает без диалогов. Сочетание видно в «Параметрах системы» →
//! «Комбинации клавиш» → Clipwire; источник истины — настройки Clipwire.

use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};

use ashpd::zbus;
use futures_util::StreamExt;
use tauri::AppHandle;

const COMPONENT: &str = "Clipwire";
const ACTION: &str = "popup";
/// `SetPresent | NoAutoloading`: сочетание действует сразу и берётся из
/// настроек Clipwire, а не из сохранённого у KDE.
const FLAGS: u32 = 0x2 | 0x4;

static BOUND: AtomicBool = AtomicBool::new(false);
static TASK: Mutex<Option<(String, tauri::async_runtime::JoinHandle<()>)>> = Mutex::new(None);

/// KDE Plasma на Wayland.
pub fn applies() -> bool {
    std::env::var_os("WAYLAND_DISPLAY").is_some()
        && std::env::var("XDG_CURRENT_DESKTOP")
            .unwrap_or_default()
            .to_uppercase()
            .split(':')
            .any(|d| d == "KDE")
}

/// Сочетание зарегистрировано в KDE.
pub fn bound() -> bool {
    BOUND.load(Ordering::Relaxed)
}

/// Зарегистрировать (или заменить при смене) сочетание `accel`.
pub fn sync(app: &AppHandle, accel: &str) {
    if !applies() {
        return;
    }
    let mut task = TASK.lock().unwrap_or_else(|e| e.into_inner());
    if task.as_ref().is_some_and(|(a, _)| a == accel) {
        return;
    }
    if let Some((_, old)) = task.take() {
        old.abort();
        BOUND.store(false, Ordering::Relaxed);
    }
    let (app, accel_owned) = (app.clone(), accel.to_string());
    let handle = tauri::async_runtime::spawn(async move {
        if let Err(e) = run(&app, &accel_owned).await {
            tracing::warn!("сочетание в KDE не зарегистрировалось: {e}");
        }
        BOUND.store(false, Ordering::Relaxed);
    });
    *task = Some((accel.to_string(), handle));
}

async fn run(app: &AppHandle, accel: &str) -> Result<(), String> {
    let key = to_qt(accel).ok_or_else(|| format!("сочетание не переводится: {accel}"))?;
    let conn = zbus::Connection::session()
        .await
        .map_err(|e| e.to_string())?;
    let kga = zbus::Proxy::new(
        &conn,
        "org.kde.kglobalaccel",
        "/kglobalaccel",
        "org.kde.KGlobalAccel",
    )
    .await
    .map_err(|e| e.to_string())?;
    let action = vec![
        COMPONENT.to_string(),
        ACTION.to_string(),
        "Clipwire".to_string(),
        clipwire_agent::i18n::tr("Открыть окно вставки Clipwire").to_string(),
    ];
    kga.call::<_, _, ()>("doRegister", &(action.clone(),))
        .await
        .map_err(|e| e.to_string())?;
    let keys: Vec<i32> = kga
        .call("setShortcut", &(action.clone(), vec![key], FLAGS))
        .await
        .map_err(|e| e.to_string())?;
    if !keys.contains(&key) {
        return Err(format!(
            "{accel} уже занято другим сочетанием KDE (например, своим на «clipwire --popup»)"
        ));
    }
    let component: zbus::OwnedObjectPath = kga
        .call("getComponent", &(COMPONENT,))
        .await
        .map_err(|e| e.to_string())?;
    let comp = zbus::Proxy::new(
        &conn,
        "org.kde.kglobalaccel",
        component,
        "org.kde.kglobalaccel.Component",
    )
    .await
    .map_err(|e| e.to_string())?;
    let mut pressed = comp
        .receive_signal("globalShortcutPressed")
        .await
        .map_err(|e| e.to_string())?;
    tracing::info!("сочетание окна вставки зарегистрировано в KDE: {accel}");
    BOUND.store(true, Ordering::Relaxed);
    while let Some(msg) = pressed.next().await {
        let Ok((_, action, _)) = msg.body().deserialize::<(String, String, i64)>() else {
            continue;
        };
        if action == ACTION {
            let app2 = app.clone();
            let _ = app.run_on_main_thread(move || super::toggle(&app2));
        }
    }
    Err("служба сочетаний KDE закрыла соединение".into())
}

/// Сочетание плагина global-shortcut → код клавиши Qt (модификаторы — биты):
/// `CommandOrControl+Alt+V` → `Qt::CTRL | Qt::ALT | Qt::Key_V`.
pub fn to_qt(accel: &str) -> Option<i32> {
    const SHIFT: i32 = 0x0200_0000;
    const CTRL: i32 = 0x0400_0000;
    const ALT: i32 = 0x0800_0000;
    const META: i32 = 0x1000_0000;
    let mut mods = 0;
    let mut key = None;
    for part in accel.split('+').map(str::trim) {
        match part {
            "CommandOrControl" | "CmdOrCtrl" | "Control" | "Ctrl" => mods |= CTRL,
            "Alt" | "Option" => mods |= ALT,
            "Shift" => mods |= SHIFT,
            "Super" | "Meta" | "Command" | "Cmd" => mods |= META,
            k => key = Some(k),
        }
    }
    let code = match key? {
        k if k.len() == 1 && k.chars().all(|c| c.is_ascii_alphanumeric()) => {
            k.to_ascii_uppercase().as_bytes()[0] as i32
        }
        k if k.starts_with('F') => {
            let n: i32 = k[1..].parse().ok().filter(|n| (1..=24).contains(n))?;
            0x0100_0030 + n - 1
        }
        "Space" => 0x20,
        "`" => 0x60,
        "Insert" => 0x0100_0006,
        "Home" => 0x0100_0010,
        "End" => 0x0100_0011,
        "PageUp" => 0x0100_0016,
        "PageDown" => 0x0100_0017,
        _ => return None,
    };
    Some(mods | code)
}

#[cfg(test)]
mod tests {
    use super::to_qt;

    #[test]
    fn qt_keys() {
        assert_eq!(to_qt("CommandOrControl+Alt+V"), Some(0x0C00_0056));
        assert_eq!(to_qt("Super+Shift+F5"), Some(0x1200_0000 | 0x0100_0034));
        assert_eq!(to_qt("Alt+1"), Some(0x0800_0031));
        assert_eq!(to_qt("CommandOrControl+Alt"), None);
    }
}
