// Английский перевод: часть «lib». Ключ — русский текст из t('…').
const en: Record<string, string> = {
  // format.ts
  'сейчас': 'now',
  '{m} мин': '{m} min',
  '{h} ч': '{h} h',
  '{n} ч': '{n} h',
  'вчера': 'yesterday',
  '{n} Б': '{n} B',
  '{n} КБ': '{n} KB',
  '{n} МБ': '{n} MB',
  '{n} ГБ': '{n} GB',
  'папка': 'folder',

  // devices.ts
  'Ответ сервера не пришёл, проверяю…': 'No reply from the server, checking…',
  'Ключ группы сменило устройство «{n}»': 'The group key was changed by the device “{n}”',
  'Ключ группы сменён на другом устройстве': 'The group key was changed on another device',
  'Неизвестное устройство': 'Unknown device',
  'в сети': 'online',
  'не в сети': 'offline',
  'был в сети: {a}': 'last seen {a}',
  'был в сети: {a} назад': 'last seen {a} ago',
  'Смените ключ S3 у поставщика хранилища и сохраните новый в настройках «Хранилище файлов».':
    'Change the S3 key with your storage provider and save the new one in the “File storage” settings.',
  'Смените пароль или ключ SSH на сервере и сохраните новые в настройках «Хранилище файлов».':
    'Change the password or SSH key on the server and save the new one in the “File storage” settings.',
  'у устройства без карточки': 'a device without a profile',
  'у устройств без карточки': 'devices without a profile',
  'у исключённого устройства и {unknown}': 'the removed device and {unknown}',
  'у исключённого устройства': 'the removed device',
  'Внимание: {who} остались доступы к хранилищу файлов. {fix}':
    'Warning: {who} still have access to the file storage. {fix}',
  '«{name}» потеряет доступ к группе: не получит новые элементы и файлы и не сможет вернуться по старому коду приглашения. То, что уже есть на нём, останется.':
    '“{name}” will lose access to the group: it will not receive new items and files and cannot rejoin with the old invite code. What is already on it will stay.',
  'Внимание: {count} без карточки ({ids}) новый ключ не получат. Их придётся подключить заново по новому коду.':
    'Warning: {count} without a profile ({ids}) will not get the new key. You will need to connect them again with the new code.',
  'История группы на сервере удалится, локальная история на устройствах останется. Код приглашения сменится, старый перестанет работать.':
    'The group history on the server will be deleted; local history on devices will stay. The invite code will change and the old one will stop working.',

  // storage.ts
  '6 часов': '6 hours',
  '12 часов': '12 hours',
  '1 день': '1 day',
  '3 дня': '3 days',
  '30 дней': '30 days',
  'Не удалять': 'Never delete',
  '{n} дн.': '{n} days',
  'Укажите адрес API': 'Enter the API address',
  'Адрес должен начинаться с https://': 'The address must start with https://',
  'По HTTP можно только к своей локальной сети, нужен https://':
    'HTTP is only allowed for your local network, use https://',
  'Укажите бакет': 'Enter the bucket',
  'Бакет с точкой в имени работает только с адресацией через путь':
    'A bucket with a dot in its name only works with path-style addressing',
  'Укажите ключ доступа': 'Enter the access key',
  'Укажите секретный ключ': 'Enter the secret key',
  'Укажите сервер': 'Enter the server',
  'Только имя или адрес сервера, без пользователя и пути': 'Server name or address only, without a user or path',
  'Порт — число от 1 до 65535': 'The port is a number from 1 to 65535',
  'Укажите пользователя': 'Enter the user',
  'Ключ сервера изменился: введите пароль заново': 'The server key has changed: enter the password again',
  'Введите пароль': 'Enter the password',
  'Ключ сервера изменился: вставьте закрытый ключ заново': 'The server key has changed: paste the private key again',
  'Вставьте закрытый ключ или выберите файл': 'Paste the private key or choose a file',
  'Это открытый ключ (.pub), нужен закрытый': 'This is a public key (.pub), a private one is needed',
  'Ключ PuTTY (.ppk) не подходит, сохраните его в формате OpenSSH':
    'A PuTTY key (.ppk) will not work, save it in OpenSSH format',
  'Не похоже на закрытый ключ: нужен текст с -----BEGIN … PRIVATE KEY-----':
    'This does not look like a private key: it should contain -----BEGIN … PRIVATE KEY-----',

  // transfer.ts
  'неизвестная ошибка': 'unknown error',
  'не отправлено: {error}': 'not sent: {error}',
  'не скачано: {error}': 'not downloaded: {error}',
  'не скачан': 'not downloaded',
  'нет на месте, копия на сервере': 'missing, copy on the server',

  // overview.ts
  'Пусто': 'Empty',
  'Нет': 'None',
  '{used} из {quota} ({pct} %)': '{used} of {quota} ({pct}%)',
  'Напрямую': 'Direct',
  'Через relay': 'Via relay',
  '{ms} мс': '{ms} ms',
  'только что': 'just now',
  '{a} назад': '{a} ago',
  'отправлено {when}': 'sent {when}',
  'получено {when}': 'received {when}',
  'С запуска не было': 'Nothing since launch',
  '{total}, в сети {online}': '{total}, {online} online',
  'Сервер Clipwire': 'Clipwire server',
  '{where}: не подключается': '{where}: cannot connect',
  '{where}: были сбои, файлы шли через сервер': '{where}: had failures, files went through the server',

  // hotkey.ts
  'Пробел': 'Space',
}

export default en
