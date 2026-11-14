// Английский перевод: часть «popup». Ключ — русский текст из t('…').
const en: Record<string, string> = {
  // AppTag
  'Скопировано в «{app}»': 'Copied from “{app}”',

  // LinkCard
  'Загружаю карточку…': 'Loading card…',
  'Похоже на одноразовую ссылку: вход, сброс пароля или подтверждение. Карточку не загружаю, чтобы переход не сжёг её.':
    'This looks like a one-time link (sign-in, password reset or confirmation). The card is not loaded, so that opening it does not use the link up.',
  'Карточки ссылок выключены в настройках, в разделе «Приватность».':
    'Link cards are turned off in Settings, under “Privacy”.',
  'Карточка не загрузилась: {error}': 'Could not load the card: {error}',

  // FilePreview
  'Папка · напрямую с устройства «{device}» · {size}': 'Folder · directly from “{device}” · {size}',
  'напрямую с устройства «{device}» · {size}': 'Directly from “{device}” · {size}',
  'Папка на устройстве «{device}» · {size}': 'Folder on “{device}” · {size}',
  'Файл на устройстве «{device}» · {size}': 'File on “{device}” · {size}',
  'Файла нет на месте, копия есть на сервере · {size}': 'The file is no longer in place, a copy is on the server · {size}',
  'Скачивается…': 'Downloading…',
  'Скачать ещё раз': 'Download again',
  'Скачать': 'Download',
  'Папка': 'Folder',
  'есть только на устройстве «{device}»': 'only on “{device}”',
  'Папки нет на месте: её переместили или удалили.': 'The folder is missing: it was moved or deleted.',
  'Файла нет на месте: его переместили или удалили.': 'The file is missing: it was moved or deleted.',
  'Открываю…': 'Opening…',
  '… показано начало, весь файл — {size}': '… showing the beginning, the whole file is {size}',
  'Этот формат встроенный плеер не воспроизводит. Откройте файл в своей программе через «Показать в папке».':
    'The built-in player cannot play this format. Open the file in your own app via “Show in folder”.',
  'Для этого типа файла предпросмотра нет.': 'No preview for this file type.',

  // EntryRow
  'Это устройство': 'This device',
  'Другое устройство': 'Another device',
  '↑ на сервере': '↑ on the server',
  'напрямую с устройства «{device}»': 'directly from “{device}”',
  'Картинка {w}×{h}': 'Image {w}×{h}',
  'Картинка': 'Image',
  'Удалить из истории (Delete)': 'Remove from history (Delete)',
  'Удалить из истории': 'Remove from history',
  'Открепить (Ctrl+P)': 'Unpin (Ctrl+P)',
  'Закрепить (Ctrl+P)': 'Pin (Ctrl+P)',
  'Открепить': 'Unpin',
  'Закрепить': 'Pin',

  // Popup
  'Текст': 'Text',
  'Ссылки': 'Links',
  'Картинки': 'Images',
  'Файлы': 'Files',
  'Файлы ещё скачиваются': 'Files are still downloading',
  'Открыть настройки': 'Open Settings',
  'Скопировано в буфер — вставьте сами (Cmd+V). Чтобы Clipwire вставлял сразу, разрешите ему «Универсальный доступ» в Системных настройках.':
    'Copied to the clipboard — paste it yourself (Cmd+V). To let Clipwire paste right away, allow it under Accessibility in System Settings.',
  'Сначала дождитесь, пока скачаются файлы другой записи': 'Wait until the files of the other item finish downloading',
  'Поиск по истории': 'Search history',
  'История': 'History',
  'Закреплённое': 'Pinned',
  'Тип записей': 'Item type',
  '{kind} (Alt + ← →)': '{kind} (Alt + ← →)',
  'Ничего не нашлось по «{query}»': 'Nothing found for “{query}”',
  'Здесь нет записей этого типа': 'No items of this type',
  'Закрепите нужное сочетанием Ctrl+P, и оно не пропадёт из истории':
    'Pin items with Ctrl+P and they stay in history',
  'Здесь появится всё, что копируется на ваших устройствах': 'Everything you copy on your devices will show up here',
  'История буфера': 'Clipboard history',
  'вставить': 'paste',
  'просмотр': 'preview',
  'изменить': 'edit',
  'закрепить': 'pin',
  'удалить': 'delete',
  'Предпросмотр': 'Preview',
  'Вставить': 'Paste',
  'Изменить': 'Edit',
  'Удалить': 'Delete',
  'Вставить «{name}»': 'Paste “{name}”',
  'папка · {size}': 'folder · {size}',
  'папка': 'folder',
  'папки нет на месте': 'folder is missing',
  'файла нет на месте': 'file is missing',
  'Скачивается {percent}%': 'Downloading {percent}%',
  'Вставить выбранные ({n})': 'Paste selected ({n})',
  'Показать в папке': 'Show in folder',
  'Вид': 'View',
  'Карточка': 'Card',
  'Форматирование': 'Formatting',
  'Текст с форматированием': 'Formatted text',
  'Текст элемента, можно поправить перед вставкой': 'Item text, you can edit it before pasting',
  'изменено · Ctrl+Enter вставить · Ctrl+S сохранить': 'edited · Ctrl+Enter paste · Ctrl+S save',
  'Сохранить в истории': 'Save to history',
  'Отменить правку': 'Discard changes',
  'Открыть в браузере': 'Open in browser',
  'Без форматирования': 'Paste as plain text',
  'Просмотр': 'Preview',
  'Пробел': 'Space',
}

export default en
