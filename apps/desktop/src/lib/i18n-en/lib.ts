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
  '{server} · обновите образ до {client}': '{server} · update the image to {client}',
  'старше 0.4.0 · обновите образ до {client}': 'older than 0.4.0 · update the image to {client}',
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
  '{used} из {max}': '{used} of {max}',

  // pins.ts, NoteEdit.svelte
  'Снять закреп только здесь': 'Unpin on this device only',
  'на других устройствах останется': 'stays pinned on other devices',
  'Снять закреп везде': 'Unpin everywhere',
  'запись останется в истории': 'the item stays in history',
  'Удалить везде': 'Delete everywhere',
  'со всех устройств группы': 'from every device in the group',
  'Подпись: Enter — сохранить, Esc — отмена': 'Note: Enter to save, Esc to cancel',
  'Подпись закреплённой записи': 'Note of the pinned item',

  'Переход на новый ключ': 'Moving to the new key',
  'Ключ группы сменён только что': 'Group key rotated just now',
  'Ключ группы сменён: {a}': 'Group key rotated {a}',
  'Ключ группы сменён: {a} назад': 'Group key rotated {a} ago',
  'перешло на новый ключ': 'moved to the new key',
  'ещё не перешло': 'not moved yet',
  'Выделенные устройства за 10 минут не перешли на новый ключ. Откройте на них Clipwire: если там вопрос о новом ключе, сверьте отпечаток и переходите. Если устройство в сети, а вопроса нет, сервер мог оставить его в старой группе. Тогда подключите его заново по новому коду приглашения.':
    'The highlighted devices have not moved to the new key in 10 minutes. Open Clipwire on them: if it asks about a new key, check the fingerprint and accept. If a device is online and there is no question, the server may have kept it in the old group. Then connect it again with the new invite code.',
  'в группе давно': 'in the group for a long time',
  'появилось только что': 'first seen just now',
  'появилось: {a}': 'first seen {a}',
  'появилось: {a} назад': 'first seen {a} ago',
  'Внимание: выделенные устройства появились в группе меньше суток назад. Если вы их не подключали, снимите с них галочку: новый ключ они не получат.':
    'Warning: the highlighted devices joined the group less than a day ago. If you did not add them, untick them: they will not get the new key.',
  'Внимание: в группе есть устройства с одинаковым именем. Сверьте отпечатки и снимите галочку с того, которое не ваше.':
    'Warning: some devices in the group share a name. Compare the fingerprints and untick the one that is not yours.',
  'Без галочки: {names}. Они тоже потеряют доступ к группе.': 'Unticked: {names}. They will lose access to the group too.',
  'Устройство «{name}» ({os}, отпечаток {fp}) сменило ключ группы. Новый ключ пришёл сюда {when}. Перейти на него?':
    '“{name}” ({os}, fingerprint {fp}) rotated the group key. The new key arrived here on {when}. Switch to it?',
  'Пока вы не ответили, синхронизация стоит: скопированное в группу не уходит. Если ключ меняли вы, сверьте отпечаток с тем, что показывает то устройство в разделе «Устройства группы», и переходите.':
    'Sync is paused until you answer: nothing you copy goes to the group. If you rotated the key yourself, compare the fingerprint with the one that device shows under “Group devices”, then switch.',
  'Если вы ключ не меняли, не переходите. Новый ключ мог прислать тот, кого удалили из группы, и тогда всё скопированное уйдёт к нему.':
    'If you did not rotate the key, do not switch. The new key may have come from someone who was removed from the group, and then everything you copy would go to them.',
  // hotkey.ts
  'Пробел': 'Space',
}

export default en
