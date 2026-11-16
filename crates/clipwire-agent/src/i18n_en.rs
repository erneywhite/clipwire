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
    // Окно, уведомления, ошибки.
    ("Кэш не очистился: {e}", "Could not clear the cache: {e}"),
    (
        "Устройство ещё не в группе",
        "This device is not in a group yet",
    ),
    (
        "Устройство уже в группе",
        "This device is already in a group",
    ),
    ("Неверный id элемента", "Invalid item id"),
    (
        "Элемента уже нет в истории",
        "This item is no longer in the history",
    ),
    ("Неверный id устройства", "Invalid device id"),
    ("Устройство не в группе", "This device is not in a group"),
    (
        "Ключ группы уже сменён",
        "The group key has already been rotated",
    ),
    (
        "Ключ сменён, но не сохранился: {e}",
        "The key was rotated but could not be saved: {e}",
    ),
    (
        "Ключ сменён, но синхронизация не перезапустилась: {e}",
        "The key was rotated but sync failed to restart: {e}",
    ),
    (
        "Синхронизация не запустилась: {e}",
        "Sync failed to start: {e}",
    ),
    (
        "Окно редактора не создано",
        "The editor window was not created",
    ),
    (
        "Картинка пришла повреждённой",
        "The image arrived corrupted",
    ),
    (
        "Этого файла нет в истории",
        "This file is not in the history",
    ),
    (
        "Файл не открылся для просмотра: {e}",
        "Could not open the file for preview: {e}",
    ),
    (
        "Есть только на устройстве «{device}»: {files}",
        "Only available on the device “{device}”: {files}",
    ),
    (
        "Файла нет на месте, его переместили или удалили: {files}",
        "The file is missing, it was moved or deleted: {files}",
    ),
    (
        "Устройство не в группе: файлы с других устройств не скачать",
        "This device is not in a group: files from other devices cannot be downloaded",
    ),
    (
        "{err}: устройство «{device}» не в сети",
        "{err}: device “{device}” is offline",
    ),
    (
        "{err}: устройство «{device}» не пустило, у него другой ключ группы",
        "{err}: device “{device}” refused the connection, it has a different group key",
    ),
    ("В записи нет файлов", "The item has no files"),
    ("Файл не скачался", "The file was not downloaded"),
    (
        "Этой папки нет в истории",
        "This folder is not in the history",
    ),
    (
        "Папки нет на месте: её переместили или удалили",
        "The folder is missing: it was moved or deleted",
    ),
    ("Папка не открылась: {e}", "Could not open the folder: {e}"),
    ("Это не веб-ссылка", "This is not a web link"),
    ("слишком много редиректов", "too many redirects"),
    (
        "Ссылка ведёт на этот компьютер или в локальную сеть",
        "The link points to this computer or the local network",
    ),
    (
        "имя указывает на этот компьютер",
        "the name points to this computer",
    ),
    (
        "у {host} нет публичных адресов",
        "{host} has no public addresses",
    ),
    ("сайт ответил {status}", "the site responded with {status}"),
    (
        "это не страница ({ctype})",
        "this is not a web page ({ctype})",
    ),
    ("картинка: {status}", "image: {status}"),
    (
        "картинка неподходящего типа {ctype}",
        "unsuitable image type {ctype}",
    ),
    ("картинка больше 2 МБ", "the image is larger than 2 MB"),
    (
        "Открыть окно вставки Clipwire",
        "Open the Clipwire paste window",
    ),
    (
        "Система не дала разрешение. Окно вставки будет только класть элемент в буфер.",
        "The system didn't grant permission. The paste window will only put the item on the clipboard.",
    ),
    (
        "Сочетание {hotkey} не зарегистрировалось: {e}",
        "Could not register the shortcut {hotkey}: {e}",
    ),
    ("Не выбрано ни одного файла", "No files selected"),
    ("В записи нет файла №{n}", "The item has no file #{n}"),
    (
        "Хранилище выбирается, когда устройство в группе",
        "File storage can be chosen once the device is in a group",
    ),
    ("Введите секретный ключ", "Enter the secret key"),
    (
        "Порт — число от 1 до 65535",
        "The port must be a number from 1 to 65535",
    ),
    (
        "Ключ сервера изменился: введите пароль заново",
        "The server key has changed: enter the password again",
    ),
    ("Введите пароль", "Enter the password"),
    (
        "Ключ сервера изменился: вставьте закрытый ключ заново",
        "The server key has changed: paste the private key again",
    ),
    (
        "Вставьте закрытый ключ или выберите файл",
        "Paste the private key or choose a file",
    ),
    ("Неизвестный способ входа", "Unknown sign-in method"),
    (
        "Неизвестное хранилище «{other}»",
        "Unknown file storage “{other}”",
    ),
    (
        "Обновление до {version} не установилось. Поставить его можно из меню в трее.",
        "The update to {version} failed to install. You can install it from the tray menu.",
    ),
    (
        "Clipwire обновлён до {current}",
        "Clipwire updated to {current}",
    ),
    (
        "Сервер Clipwire {server} старше приложения {client}: обновите образ сервера",
        "Clipwire server {server} is older than the app {client}: update the server image",
    ),
    (
        "Сервер Clipwire старше приложения {client}: обновите образ сервера",
        "The Clipwire server is older than the app {client}: update the server image",
    ),
    (
        "Сервер Clipwire устарел и не поддерживает эту версию: обновите образ сервера",
        "The Clipwire server is too old for this version: update the server image",
    ),
    (
        "Обновление уже устанавливается",
        "An update is already being installed",
    ),
    (
        "Скачанное обновление не прочиталось: {e}",
        "Could not read the downloaded update: {e}",
    ),
    (
        "файл изменился после копирования",
        "the file changed after it was copied",
    ),
    ("файла уже нет на месте", "the file is no longer there"),
    (
        "нет связи с сервером: {e}",
        "no connection to the server: {e}",
    ),
    ("внешнее хранилище: {e}", "external storage: {e}"),
    ("сервер закрыл поток", "the server closed the stream"),
    (
        "сервер не подтвердил загрузку",
        "the server did not confirm the upload",
    ),
    (
        "сервер закрыл поток без ответа",
        "the server closed the stream without a reply",
    ),
    (
        "сервер не ответил за {timeout}",
        "the server did not reply within {timeout}",
    ),
    (
        "лишняя часть файла {index}",
        "unexpected extra file chunk {index}",
    ),
    (
        "часть {index} пришла не по порядку, ждали {next}",
        "chunk {index} arrived out of order, expected {next}",
    ),
    (
        "часть {index} не той длины: {len} байт",
        "chunk {index} has the wrong length: {len} bytes",
    ),
    (
        "файл скачан не до конца",
        "the file was not fully downloaded",
    ),
    ("не переименовался {path}", "could not rename {path}"),
    ("не открылся {path}", "could not open {path}"),
    (
        "источник не ответил за {timeout}",
        "the source did not reply within {timeout}",
    ),
    (
        "на сервере файл другого размера: {s} байт вместо {size}",
        "the file on the server has a different size: {s} bytes instead of {size}",
    ),
    (
        "сервер оборвал передачу файла",
        "the server interrupted the file transfer",
    ),
    (
        "папка изменилась после копирования",
        "the folder changed after it was copied",
    ),
    (
        "сеть ещё не поднялась, попробуйте позже",
        "the network is not up yet, try again later",
    ),
    (
        "на устройстве-источнике этого файла уже нет в истории",
        "the file is no longer in the history on the source device",
    ),
    (
        "файл на устройстве-источнике изменился или пропал после копирования",
        "the file on the source device changed or disappeared after it was copied",
    ),
    (
        "файл «{name}» не передавался с устройства",
        "the file “{name}” was not sent from the device",
    ),
    (
        "настройки хранилища файлов группы ещё не пришли на это устройство, попробуйте вставить чуть позже",
        "the group's file storage settings have not reached this device yet, try pasting a bit later",
    ),
    ("нет связи с сервером", "no connection to the server"),
    ("«{name}» не скачался", "“{name}” was not downloaded"),
    (
        "Внешнее хранилище недоступно ({e}), «{name}» отправлен через сервер Clipwire",
        "External storage is unavailable ({e}), “{name}” was sent through the Clipwire server",
    ),
    (
        "Внешнее хранилище отказало ({e}), файл отправляется через сервер Clipwire",
        "External storage refused ({e}), the file is being sent through the Clipwire server",
    ),
    (
        "{e}. Отправка продолжится после переподключения",
        "{e}. Sending will resume after reconnecting",
    ),
    (
        "не удалось записать в буфер",
        "could not write to the clipboard",
    ),
    (
        "не удалось записать картинку в буфер",
        "could not write the image to the clipboard",
    ),
    (
        "буфер обмена недоступен: нет ни X11, ни Wayland с data-control",
        "the clipboard is unavailable: neither X11 nor Wayland with data-control",
    ),
    (
        "поток буфера обмена остановился",
        "the clipboard thread has stopped",
    ),
    ("буфер обмена не ответил", "the clipboard did not respond"),
    (
        "не удалось занять буфер обмена",
        "could not take over the clipboard",
    ),
    (
        "этот тип пока не записывается в буфер",
        "this kind of content cannot be written to the clipboard yet",
    ),
    (
        "файлов уже нет на месте: {files}",
        "the files are no longer there: {files}",
    ),
    (
        "буфер не открылся: {e}",
        "could not open the clipboard: {e}",
    ),
    (
        "файлы не записались в буфер: {e}",
        "could not write the files to the clipboard: {e}",
    ),
    (
        "файлы не записались в буфер",
        "could not write the files to the clipboard",
    ),
    (
        "ключ Clipwire в связке ключей повреждён",
        "the Clipwire key in the keychain is damaged",
    ),
    (
        "ключ не записался в связку ключей",
        "could not save the key to the keychain",
    ),
    (
        "нет доступа к ключу Clipwire в связке ключей",
        "no access to the Clipwire key in the keychain",
    ),
    ("файл с секретами повреждён", "the secrets file is damaged"),
    (
        "файл с секретами не расшифровался: ключ в хранилище паролей другой",
        "the secrets file could not be decrypted: the key in the password store is different",
    ),
    (
        "файл с секретами зашифрован, а ключа Clipwire в хранилище паролей нет",
        "the secrets file is encrypted, but the Clipwire key is missing from the password store",
    ),
    (
        "ключ Clipwire в хранилище паролей повреждён",
        "the Clipwire key in the password store is damaged",
    ),
    (
        "хранилище паролей заперто, ключи Clipwire недоступны. Разблокируйте его паролем входа в систему — Clipwire подхватит ключи сам",
        "the password store is locked, so Clipwire cannot get its keys. Unlock it with your login password and Clipwire will pick the keys up by itself",
    ),
    (
        "хранилище паролей не ответило",
        "the password store did not respond",
    ),
    ("ошибка хранилища паролей", "password store error"),
    (
        "хранилище паролей недоступно",
        "the password store is unavailable",
    ),
    (
        "файл с секретами не расшифровался: ключ в связке ключей другой",
        "could not decrypt the secrets file: the key in the keychain is different",
    ),
    ("буфер: {what}: {e}", "clipboard: {what}: {e}"),
    ("не открылся", "could not open"),
    ("текст", "text"),
    (
        "Это устройство так не удалить. Чтобы выйти, отключитесь от группы",
        "This device cannot be removed this way. To leave, disconnect from the group",
    ),
    (
        "Такого устройства нет в группе",
        "There is no such device in the group",
    ),
    (
        "Нет связи с сервером. Попробуйте, когда устройство будет в сети",
        "No connection to the server. Try again when the device is online",
    ),
    (
        "Сервер не сообщает эти цифры: обновите сервер Clipwire",
        "The server does not report these numbers: update the Clipwire server",
    ),
    (
        "в папке больше {max} файлов и папок",
        "the folder has more than {max} files and folders",
    ),
    (
        "папка вложена глубже {max} уровней",
        "the folder is nested deeper than {max} levels",
    ),
    ("папка не читается: {e}", "the folder cannot be read: {e}"),
    ("это не папка", "this is not a folder"),
    (
        "файл стал короче после копирования",
        "the file became shorter after it was copied",
    ),
    (
        "файл {name} изменился после копирования",
        "the file {name} changed after it was copied",
    ),
    ("не создался {path}", "could not create {path}"),
    (
        "в архиве запись с недопустимым путём: {path}",
        "the archive has an entry with an invalid path: {path}",
    ),
    (
        "в архиве запись с пустым путём",
        "the archive has an entry with an empty path",
    ),
    (
        "в архиве слишком глубокая запись",
        "the archive has an entry nested too deep",
    ),
    (
        "не удалилась старая {path}",
        "could not delete the old {path}",
    ),
    ("не переименовалась {path}", "could not rename {path}"),
    (
        "в архиве больше {max} записей",
        "the archive has more than {max} entries",
    ),
    ("не распаковался {path}", "could not extract {path}"),
    (
        "в архиве недопустимая запись ({kind}): ссылка или устройство",
        "the archive has an invalid entry ({kind}): a link or a device",
    ),
    (
        "Сначала проверьте подключение и подтвердите отпечаток ключа сервера",
        "First check the connection and confirm the server key fingerprint",
    ),
    (
        "Настройки не сохранились: {e}",
        "Settings were not saved: {e}",
    ),
    (
        "Версия настроек группы на пределе, новее сохранить нельзя",
        "The group settings version is at its limit, a newer one cannot be saved",
    ),
    (
        "Настройки слишком большие (длинный ключ SSH?)",
        "The settings are too large (a long SSH key?)",
    ),
    (
        "картинка {width}×{height} слишком большая",
        "the image {width}×{height} is too large",
    ),
    ("не удалось закодировать PNG", "could not encode the PNG"),
    (
        "заголовок картинки не читается",
        "the image header cannot be read",
    ),
    ("картинка не читается", "the image cannot be read"),
    ("PNG повреждён", "the PNG is corrupted"),
    (
        "размеры картинки {w}×{h} не совпадают с заявленными {width}×{height}",
        "the image size {w}×{h} does not match the declared {width}×{height}",
    ),
    ("файл недоступен", "the file is not accessible"),
    (
        "файл-картинка слишком большой",
        "the image file is too large",
    ),
    (
        "не удалось сохранить миниатюру",
        "could not save the thumbnail",
    ),
    ("устройство не в сети", "the device is offline"),
    (
        "устройство-источник не пустило: у него другой ключ группы",
        "the source device refused the connection: it has a different group key",
    ),
    (
        "устройство-источник не подтвердило ключ группы",
        "the source device did not confirm the group key",
    ),
    (
        "файла нет на месте, его переместили или удалили",
        "the file is missing, it was moved or deleted",
    ),
    (
        "устройство-источник не ответило на проверку",
        "the source device did not respond to the check",
    ),
    ("«{name}»: {reason}", "“{name}”: {reason}"),
    (
        "архив папки собрался не того размера",
        "the folder archive came out the wrong size",
    ),
    ("Это устройство", "This device"),
    (
        "Связь с сервером оборвалась",
        "The connection to the server was lost",
    ),
    (
        "Ключ группы сменён, устройство переподключается",
        "The group key was rotated, the device is reconnecting",
    ),
    ("ключ группы сменён", "the group key was rotated"),
    (
        "ключ группы сменён, нужно подтверждение",
        "the group key was rotated, confirmation needed",
    ),
    (
        "В группе появилось новое устройство. Проверьте список получателей и подтвердите смену ключа заново",
        "A new device has joined the group. Check the list of recipients and confirm the key rotation again",
    ),
    (
        "Ключ группы сменён: подтвердите в настройках",
        "Group key rotated: confirm in settings",
    ),
    (
        "Устройство «{name}» сменило ключ группы. Синхронизация остановлена: подтвердите в настройках Clipwire",
        "“{name}” rotated the group key. Sync is paused: confirm in Clipwire settings",
    ),
    (
        "устройство исключили из группы",
        "the device was removed from the group",
    ),
    ("сервер отказал: {e}", "the server refused: {e}"),
    (
        "Ключ не сменился: сервер не получил запрос. Попробуйте ещё раз",
        "The key was not rotated: the server did not receive the request. Try again",
    ),
    (
        "сервер не дослал историю",
        "the server did not finish sending the history",
    ),
    ("сервер: {e}", "server: {e}"),
    (
        "сервер закрыл соединение",
        "the server closed the connection",
    ),
    (
        "Сервер не поддерживает настройки группы: обновите сервер Clipwire",
        "The server does not support group settings: update the Clipwire server",
    ),
    (
        "ключ группы сменён на этом устройстве",
        "the group key was rotated on this device",
    ),
    (
        "Ответ сервера не пришёл, проверяю…",
        "No reply from the server, checking…",
    ),
    ("Ключ не сменился: {e}", "The key was not rotated: {e}"),
    (
        "сервер не прислал список устройств",
        "the server did not send the device list",
    ),
    (
        "не удалось создать новую группу: {e}",
        "could not create the new group: {e}",
    ),
    ("новая группа не ответила", "the new group did not respond"),
    ("новая группа: {e}", "new group: {e}"),
    (
        "сервер закрыл соединение с новой группой",
        "the server closed the connection to the new group",
    ),
    (
        "сервер не ответил на смену ключа",
        "the server did not reply to the key rotation",
    ),
    (
        "файлы не зарезервировались: {error}",
        "could not reserve space for the files: {error}",
    ),
    ("таймаут подключения{hint}", "connection timed out{hint}"),
    (
        "relay отказал в доступе: {reason}",
        "the relay denied access: {reason}",
    ),
    (" (relay: {e})", " (relay: {e})"),
    (
        "сервер закрыл соединение: {e}",
        "the server closed the connection: {e}",
    ),
    (
        "сервер не ответил на вход",
        "the server did not reply to the sign-in",
    ),
    ("без ответа", "no reply"),
    ("кривой адрес relay {url}", "invalid relay address {url}"),
    (
        "не поднялся iroh-эндпоинт",
        "the network endpoint did not start",
    ),
    (
        "неверный ключ доступа или пароль ({d})",
        "wrong access key or password ({d})",
    ),
    (
        "бакета или папки нет ({d})",
        "the bucket or folder does not exist ({d})",
    ),
    (
        "нет прав на запись или чтение ({d})",
        "no permission to write or read ({d})",
    ),
    (
        "не тот ключ хоста: сервер прислал {got}, а подтверждён {expected}",
        "wrong host key: the server sent {got}, but {expected} was confirmed",
    ),
    (
        "хранилище не ответило вовремя",
        "the storage did not respond in time",
    ),
    (
        "нет связи с хранилищем ({d})",
        "no connection to the storage ({d})",
    ),
    (
        "настройки хранилища не годятся: {d}",
        "the storage settings are not valid: {d}",
    ),
    (
        "хранилище отдало испорченные данные: {d}",
        "the storage returned corrupted data: {d}",
    ),
    ("ошибка хранилища: {d}", "storage error: {d}"),
    (
        "объект {name} больше {max} байт",
        "the object {name} is larger than {max} bytes",
    ),
    (
        "недопустимое имя объекта {name}",
        "invalid object name {name}",
    ),
    ("адрес «{endpoint}»: {e}", "address “{endpoint}”: {e}"),
    ("в адресе нет хоста", "the address has no host"),
    (
        "адрес link-local (169.254.x.x, fe80::) запрещён: там метаданные облака",
        "link-local addresses (169.254.x.x, fe80::) are not allowed: cloud metadata lives there",
    ),
    (
        "по HTTP можно только к localhost и адресам локальной сети, нужен HTTPS",
        "plain HTTP only works with localhost and local network addresses, HTTPS is required",
    ),
    (
        "схема «{s}» не поддерживается",
        "the scheme “{s}” is not supported",
    ),
    ("не указан бакет", "no bucket specified"),
    ("не указан ключ доступа", "no access key specified"),
    (
        "бакет с точкой в имени работает только с адресацией endpoint/bucket",
        "a bucket with a dot in its name only works with endpoint/bucket addressing",
    ),
    (
        "HTTP-клиент не собрался: {e}",
        "could not build the HTTP client: {e}",
    ),
    ("список", "listing"),
    (
        "{detail}: проверьте адрес, регион и имя бакета",
        "{detail}: check the address, region and bucket name",
    ),
    (
        "в {dir} больше {max} объектов",
        "{dir} has more than {max} objects",
    ),
    (
        "список объектов пошёл по кругу",
        "the object listing went in a loop",
    ),
    (
        "список {dir} длиннее {pages} страниц",
        "the listing of {dir} is longer than {pages} pages",
    ),
    ("не указан сервер", "no server specified"),
    ("не указан пользователь", "no user specified"),
    (
        "закрытый ключ не прочитался: {e}",
        "could not read the private key: {e}",
    ),
    (
        "сервер не принял вход пользователя «{user}»",
        "the server did not accept the sign-in of the user “{user}”",
    ),
    (
        "сервер не дал SFTP: {e}",
        "the server did not provide SFTP: {e}",
    ),
    (
        "сервер не прислал ключ хоста",
        "the server did not send a host key",
    ),
    (
        "в папке {dir} больше {max} записей",
        "the folder {dir} has more than {max} entries",
    ),
    ("meta файла: {e}", "file metadata: {e}"),
    (
        "во внешнем хранилище файл другого размера: {actual} байт вместо {size}",
        "the file in external storage has a different size: {actual} bytes instead of {size}",
    ),
    ("не открылся {path}: {e}", "could not open {path}: {e}"),
    (
        "часть {index} пропала из внешнего хранилища (удалена по сроку?)",
        "chunk {index} is gone from external storage (expired?)",
    ),
    (
        "часть {index} есть во внешнем хранилище, но не читается",
        "chunk {index} exists in external storage but cannot be read",
    ),
    (
        "загрузка файла остановилась на источнике: за {min} мин не появилось ни одной части",
        "the upload stalled on the source: no chunk appeared in {min} min",
    ),
    (
        "прочитано не то, что записано",
        "what was read is not what was written",
    ),
    (
        "записанный объект не читается обратно",
        "the written object cannot be read back",
    ),
    ("объект не удалился", "the object was not deleted"),
    // Закрепы группы.
    (
        "Подписать можно только закреплённую запись",
        "Only a pinned item can have a note",
    ),
    (
        "На сервере уже {max} закрепов группы, больше нельзя: запись закреплена только здесь",
        "The group already has {max} pins on the server, the limit: the item is pinned on this device only",
    ),
    (
        "Закрепы группы заняли всё место на сервере ({mb} МБ): запись закреплена только здесь",
        "The group's pins have used all their space on the server ({mb} MB): the item is pinned on this device only",
    ),
    (
        "Запись больше предела сервера ({mb} МБ): закреплена только здесь",
        "The item is larger than the server allows ({mb} MB): pinned on this device only",
    ),
    (
        "Сервер не принял закреп: {e}",
        "The server refused the pin: {e}",
    ),
    (
        "Запись слишком большая для закрепа группы: закреплена только здесь",
        "The item is too large for a group pin: pinned on this device only",
    ),
    (
        "После смены ключа сервер не принял закрепов: {n}. {why}",
        "After the key change the server refused {n} pins. {why}",
    ),
    // Администратор сервера и админ группы.
    (
        "Его печатает сервер при запуске (лог контейнера) и команда clipwire-server info, строка начинается с cwserver2-.",
        "The server prints it on startup (container log) and via clipwire-server info; it starts with cwserver2-.",
    ),
    (
        "С кодом cwserver1- группу не создать: эта версия Clipwire создаёт группы только на сервере 0.5.0 или новее и только с его кодом администратора cwserver2-. Если сервер старее, обновите его образ, и он напечатает новый код.",
        "A cwserver1- code cannot create a group: this version of Clipwire creates groups only on server 0.5.0 or newer and only with its administrator code cwserver2-. If the server is older, update its image, and it will print the new code.",
    ),
    ("Код не сохранился: {e}", "The code was not saved: {e}"),
    (
        "Код приглашения не разобрался ({e}). Он начинается с clipwire1-.",
        "Could not read the invite code ({e}). It starts with clipwire1-.",
    ),
    ("сервер не ответил", "the server did not reply"),
    (
        "сервер не принял ключ администратора",
        "the server did not accept the administrator key",
    ),
    (
        "Это доступно только в окне настроек",
        "This is available only in the settings window",
    ),
    (
        "Это приглашение в другую группу. Чтобы перейти в неё, выйдите из этой группы и войдите по коду.",
        "This invite is for another group. To move to it, leave this group and join with the code.",
    ),
    (
        "Это приглашение на другой сервер. Чтобы перейти на него, выйдите из группы и войдите по коду.",
        "This invite is for another server. To move to it, leave the group and join with the code.",
    ),
    (
        "В коде cwserver1- нет ключа администратора.",
        "A cwserver1- code has no administrator key.",
    ),
    (
        "Это код другого сервера: устройство подключено не к нему.",
        "This code is for a different server: this device is not connected to it.",
    ),
    (
        "Ключ администратора не подошёл: его сменили, или это код другого сервера. Введите текущий код сервера администратора.",
        "The administrator key did not match: it was changed, or the code is for another server. Enter the current server admin code.",
    ),
    (
        "Сервер не дал прав администратора: у этого устройства нет действующего ключа администратора (его могли сменить). Введите текущий код сервера администратора.",
        "The server did not grant administrator rights: this device has no valid administrator key (it may have been changed). Enter the current server admin code.",
    ),
    (
        "Это может только админ группы",
        "Only the group admin can do this",
    ),
    (
        "Сервер не поддерживает администрирование: обновите сервер Clipwire до 0.5.0",
        "The server does not support administration: update the Clipwire server to 0.5.0",
    ),
    (
        "Сервер ответил не то, что ожидалось",
        "The server sent an unexpected reply",
    ),
    (
        "Код сервера не разобрался ({e}). Код сервера начинается с cwserver2-.",
        "Could not read the server code ({e}). A server code starts with cwserver2-.",
    ),
    (
        "Сервер не знает настройку {key}: обновите сервер Clipwire",
        "The server does not know the setting {key}: update the Clipwire server",
    ),
    (
        "Значение {key} должно быть от {min} до {max}",
        "{key} must be between {min} and {max}",
    ),
    ("Ключ не сохранился: {e}", "The key was not saved: {e}"),
    (
        "Токен сменён, но новый код сервера не сохранился: {e}. Запишите код: {code}",
        "The token was changed, but the new server code was not saved: {e}. Write down the code: {code}",
    ),
    (
        "Ключ сменён, но не сохранился на этом устройстве: {e}. Запишите код: {code}",
        "The key was changed, but it was not saved on this device: {e}. Write down the code: {code}",
    ),
    // Группы сервера (`clipwire/10`).
    (
        "Такой группы на сервере уже нет",
        "This group is no longer on the server",
    ),
    (
        "Это группа этого устройства. Чтобы удалить её, сначала выйдите из неё (или удалите её с другого устройства администратора).",
        "This is this device's own group. To delete it, leave it first (or delete it from another administrator device).",
    ),
    (
        "Сервер не умеет показывать группы: обновите образ сервера Clipwire",
        "The server cannot list groups: update the Clipwire server image",
    ),
    (
        "группу удалил администратор сервера",
        "the server administrator deleted the group",
    ),
    (
        "Группу удалил администратор сервера",
        "The server administrator deleted the group",
    ),
    ("Неверный id группы", "Invalid group id"),
    // Коды создания группы (`clipwire/11`).
    (
        "Код создания группы не разобрался ({e}). Скопируйте его целиком, он начинается с cwgroup1-.",
        "The group creation code could not be read ({e}). Copy it whole, it starts with cwgroup1-.",
    ),
    (
        "Код не разобрался ({e}). Группу создают по коду администратора сервера (cwserver2-…) или по одноразовому коду создания группы (cwgroup1-…).",
        "The code could not be read ({e}). A group is created with the server administrator code (cwserver2-…) or with a one-time group creation code (cwgroup1-…).",
    ),
    (
        "Сервер не знает этот код создания группы. Проверьте, что код скопирован целиком и выпущен на этом сервере.",
        "The server does not know this group creation code. Check that the code is copied whole and was issued on this server.",
    ),
    (
        "По этому коду уже создали группу. Код одноразовый: попросите администратора сервера выпустить новый.",
        "A group has already been created with this code. The code works once: ask the server administrator for a new one.",
    ),
    (
        "Срок действия кода истёк. Попросите администратора сервера выпустить новый.",
        "The code has expired. Ask the server administrator for a new one.",
    ),
    (
        "Администратор сервера отозвал этот код. Попросите у него новый.",
        "The server administrator revoked this code. Ask them for a new one.",
    ),
    (
        "Действующих кодов уже {max}. Отзовите ненужные или дождитесь, пока их используют.",
        "There are already {max} active codes. Revoke the ones you no longer need or wait until they are used.",
    ),
    (
        "Этот код уже использован или отозван",
        "This code has already been used or revoked",
    ),
    (
        "Сервер не знает одноразовых кодов создания группы: попросите администратора обновить образ сервера Clipwire",
        "The server does not support one-time group creation codes: ask the administrator to update the Clipwire server image",
    ),
    (
        "Срок действия — от 1 до {max} часов",
        "The validity period must be from 1 to {max} hours",
    ),
    // Картинка как файл (image_file.rs, окно вставки).
    ("Снимок {stamp}.png", "Image {stamp}.png"),
    (
        "Картинка не записалась файлом: {e}",
        "Could not save the image as a file: {e}",
    ),
];
