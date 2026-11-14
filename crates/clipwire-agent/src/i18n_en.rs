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
        "Обновление уже устанавливается",
        "An update is already being installed",
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
];
