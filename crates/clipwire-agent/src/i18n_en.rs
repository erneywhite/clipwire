//! Английский перевод для `i18n::tr`: (русский, английский). Ключ — ровно
//! строка из вызова, включая `{подстановки}`.

pub static EN: &[(&str, &str)] = &[
    // Трей.
    ("Синхронизация не запустилась", "Sync did not start"),
    ("Только это устройство", "This device only"),
    ("Подключение…", "Connecting…"),
    ("Синхронизация включена", "Sync is on"),
    ("Нет связи: {reason}", "Offline: {reason}"),
    ("На паузе", "Paused"),
    (
        "Сервер исключил устройство из группы",
        "The server removed this device from the group",
    ),
    ("Пауза", "Pause"),
    ("Настройки", "Settings"),
    ("Обновлений нет", "No updates"),
    ("Выход", "Quit"),
    ("Обновить до {version}", "Update to {version}"),
];
