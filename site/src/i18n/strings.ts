// Тексты сайта на двух языках. Английский основной (корень), русский в /ru/.

export type Lang = 'en' | 'ru';

export interface Row {
  kind: 'text' | 'code' | 'link' | 'image' | 'file' | 'folder';
  text: string;
  meta: string;
  /** Для формата: метка HTML/RTF в мете. */
  tag?: string;
}

const en = {
  htmlLang: 'en',
  title: 'Clipwire · a shared clipboard through your own server',
  description:
    'Free shared clipboard for Windows, macOS and Linux. Copy on one computer, paste on another. Text, images and files go through your own server, end-to-end encrypted.',
  skip: 'Skip to content',
  nav: { how: 'How it works', security: 'Encryption', server: 'Self-hosting', download: 'Download', support: 'Support' },
  langSwitch: { label: 'Русский', href: '/ru/', short: 'RU' },

  hero: {
    a: 'Copy here.',
    b: 'Paste there.',
    lead:
      'A shared clipboard for Windows, macOS and Linux. Text, images and files travel through a server you run yourself and are encrypted before they leave the device. The server only ever sees ciphertext.',
    ctaFor: 'Download for',
    ctaAny: 'Download',
    ctaMobile: 'Get it for your computer',
    other: 'All platforms',
    meta: ['free', 'open source, AGPL-3.0'],
    sceneLabel:
      'Animation: text copied on a Windows PC travels encrypted along a wire and appears in the paste window on a MacBook.',
  },

  scene: {
    devA: 'work-pc · Windows',
    devB: 'macbook · macOS',
    copied: 'copied',
    payloads: [
      {
        doc: 'Bringing the server up is one command: |docker compose up -d|, then check the log.',
        row: { kind: 'code', text: 'docker compose up -d', meta: 'work-pc · Notepad · now · 20 chars' },
      },
      {
        doc: 'Repo for the review: |https://github.com/erneywhite/clipwire| (main branch).',
        row: { kind: 'link', text: 'https://github.com/erneywhite/clipwire', meta: 'work-pc · Firefox · now' },
      },
      {
        doc: '#chart',
        row: { kind: 'image', text: 'Weekly active devices', meta: 'work-pc · Paint · now · 1200×739 · 42 KB' },
      },
      {
        doc: '#file',
        row: { kind: 'file', text: 'report-q3.pdf', meta: 'work-pc · Explorer · now · 2.4 MB' },
      },
    ] as { doc: string; row: Row }[],
    fileName: 'report-q3.pdf',
    fileSize: '2.4 MB',
    chartTitle: 'Weekly active devices',
    rows: [
      { kind: 'text', text: 'Meeting moved to Thursday, 15:00. Room 4B', meta: 'macbook · Mail · 12 min' },
      { kind: 'link', text: 'https://docs.rs/iroh/latest/iroh/', meta: 'macbook · Safari · 40 min' },
      { kind: 'text', text: 'Thanks! I will send the slides before lunch.', meta: 'work-pc · Teams · 1 h' },
    ] as Row[],
  },

  paste: {
    search: 'Search clipboard',
    history: 'History',
    pinned: 'Pinned',
    hints: ['↵ paste', '␣ preview', 'Ctrl P pin', 'Del delete'],
  },

  how: {
    title: 'What travels over the wire',
    lead: 'Whatever you copy shows up on your other devices a moment later, in a history you can search.',
    steps: [
      {
        key: 'rich',
        title: 'Text, with formatting',
        body: 'Copying from a browser, Word or a mail client keeps the HTML and RTF. Enter pastes it formatted, Shift+Enter pastes plain text.',
      },
      {
        key: 'image',
        title: 'Images',
        body: 'Screenshots and pictures arrive as lossless PNG with a large thumbnail. Space opens the full image, Ctrl+E opens the editor.',
      },
      {
        key: 'files',
        title: 'Files and folders',
        body: 'A card with the name and size appears everywhere at once. Files up to 100 MB (adjustable) are uploaded encrypted right away. Larger ones come straight from the source computer when you paste.',
      },
      {
        key: 'links',
        title: 'Links',
        body: "Space shows a card built from the page's og: tags. The page itself never runs, and links that look single-use (reset, token, magic login) are not fetched at all.",
      },
    ],
    rich: {
      heading: 'Release notes',
      items: ['Paste window opens next to the cursor', 'Pins sync across devices', 'Exclusions for files and folders'],
      plain: 'plain text',
      formatted: 'formatted',
    },
    files: {
      card: 'clipwire-demo.mp4',
      size: '186 MB',
      waiting: 'card arrived, waiting for paste',
      pulling: 'pasting: pulled from work-pc directly',
      done: 'pasted',
    },
    links: {
      okUrl: 'https://github.com/erneywhite/clipwire',
      okTitle: 'erneywhite/clipwire',
      okDesc: 'Shared clipboard for your computers, through your own server.',
      badUrl: 'https://accounts.example.com/reset?token=8f2c…',
      badNote: 'looks single-use, not fetched',
    },
  },

  security: {
    title: 'The server only sees noise',
    lead: 'Each entry is encrypted on the device with the group key. The key lives in the invitation code and on your devices, never on the server.',
    secret: 'DB_PASSWORD=sunflower-orbit-42',
    server: 'your server',
    stages: ['on work-pc', 'on the server', 'on macbook'],
    points: [
      'The local history is encrypted too: DPAPI on Windows, Keychain on macOS, Secret Service on Linux.',
      'Content that password managers mark as private never leaves the device.',
      'Lost a laptop? Remove it from the group: the key is replaced and the other devices get the new one.',
    ],
  },

  exclusions: {
    title: 'Not everything should travel',
    lead: 'Rules decide what stays on this computer: by extension, name, mask or size. System clutter is dropped out of the box, and password managers are excluded by default.',
    note: 'With “Honour .gitignore” on, a copied project folder leaves behind whatever its .gitignore lists, such as .env and build output.',
    gate: 'exclusions',
    sent: 'sent',
    items: [
      { label: 'notes.md', pass: true },
      { label: 'Thumbs.db', pass: false, why: 'system file' },
      { label: 'screenshot.png', pass: true },
      { label: '.env', pass: false, why: '.gitignore' },
      { label: 'Meeting at 15:00', pass: true },
      { label: 'KeePassXC', pass: false, why: 'password manager' },
      { label: 'invoice.pdf', pass: true },
      { label: 'node_modules/', pass: false, why: '.gitignore' },
      { label: '~$report.docx', pass: false, why: 'lock file' },
      { label: 'deploy.sh', pass: true },
      { label: '.DS_Store', pass: false, why: 'system file' },
    ],
  },

  server: {
    title: 'One container. Your server.',
    lead: 'The server is a single Docker container with an iroh relay built in. It is set up from a Docker panel, no SSH needed.',
    facts: [
      'On the same network devices connect directly. Everywhere else traffic goes through your relay.',
      'Files are kept on the server disk, in S3-compatible storage or on SFTP.',
      'Devices that were off pick up what they missed from a short mailbox with a retention period.',
      'No server yet? The app works as a local clipboard history on one computer.',
    ],
    guide: 'Server setup guide',
    guideHref: 'https://github.com/erneywhite/clipwire#self-hosted-server',
    file: 'docker-compose.yml',
  },

  try: {
    title: 'Try the paste window',
    lead: 'Ctrl+Shift+V opens it next to the mouse cursor (Cmd+Shift+V on macOS). This copy works right here: arrows, Enter, Space for preview, or just start typing.',
    target: 'Your document',
    targetEmpty: 'Select an entry and press Enter. It lands here.',
    focusHint: 'Click the window, then use the keyboard',
    noResults: 'Nothing found',
    shots: [
      { src: '/img/preview.png', alt: 'Paste window with an image preview', caption: 'Preview opens next to the list' },
      { src: '/img/settings-sync.png', alt: 'Sync settings: create a group or join one', caption: 'Join a group with an invitation code' },
      { src: '/img/settings-exclusions.png', alt: 'Exclusions settings', caption: 'Exclusions for files, folders and programs' },
    ],
    rows: [
      { kind: 'code', text: 'docker compose pull && docker compose up -d', meta: 'work-pc · Terminal · now · 44 chars' },
      { kind: 'link', text: 'https://github.com/erneywhite/clipwire', meta: 'This device · Firefox · 2 min' },
      { kind: 'text', text: 'Thanks! I will send the slides before lunch.', meta: 'macbook · Mail · 9 min · 45 chars' },
      { kind: 'text', text: 'Meeting moved to Thursday, 15:00. Room 4B', meta: 'work-pc · Outlook · 14 min', tag: 'HTML' },
      { kind: 'image', text: 'Weekly active devices', meta: 'This device · Paint · 20 min · 1200×739' },
      { kind: 'file', text: 'report-q3.pdf', meta: 'linux-box · Files · 1 h · 2.4 MB' },
      { kind: 'folder', text: 'clipwire-site', meta: 'work-pc · Explorer · 2 h · 38 files' },
      { kind: 'text', text: 'Riga, Brīvības iela 40', meta: 'macbook · Maps · yesterday' },
    ] as Row[],
  },

  download: {
    title: 'Download Clipwire',
    lead: 'One installer per system. The app updates itself when a new version is out.',
    latest: 'Latest release',
    systems: [
      { os: 'windows', name: 'Windows', req: 'x64', files: [{ label: 'Installer (.exe)', suffix: '_windows_x64-setup.exe' }] },
      { os: 'macos', name: 'macOS', req: '13 Ventura or newer, Apple Silicon and Intel', files: [{ label: 'Disk image (.dmg)', suffix: '_macos_universal.dmg' }] },
      {
        os: 'linux',
        name: 'Linux',
        req: 'x86-64, X11 and Wayland',
        files: [
          { label: 'Debian, Ubuntu (.deb)', suffix: '_linux_amd64.deb' },
          { label: 'AppImage', suffix: '_linux_amd64.AppImage' },
        ],
      },
    ],
    serverLine: 'Server image',
    allReleases: 'All releases and changelog',
    yours: 'your system',
  },

  support: {
    title: 'Free, made by one person',
    body: 'Clipwire is free and has no paid tier. If it saves you time, you can support the author on Ko-fi.',
    kofi: 'Support on Ko-fi',
    issues: 'Found a bug or have an idea? Tell about it in Issues.',
    issuesLink: 'Open Issues',
  },

  footer: { license: 'AGPL-3.0', source: 'Source code' },
};

export type Strings = typeof en;

const ru: Strings = {
  htmlLang: 'ru',
  title: 'Clipwire · общий буфер обмена через свой сервер',
  description:
    'Бесплатный общий буфер обмена для Windows, macOS и Linux. Копируешь на одном компьютере, вставляешь на другом. Текст, картинки и файлы идут через собственный сервер с шифрованием от устройства до устройства.',
  skip: 'К содержимому',
  nav: { how: 'Как работает', security: 'Шифрование', server: 'Свой сервер', download: 'Скачать', support: 'Поддержать' },
  langSwitch: { label: 'English', href: '/', short: 'EN' },

  hero: {
    a: 'Копируешь здесь.',
    b: 'Вставляешь там.',
    lead:
      'Общий буфер обмена для Windows, macOS и Linux. Текст, картинки и файлы идут через собственный сервер и шифруются ещё на устройстве. Сервер видит только шифротекст.',
    ctaFor: 'Скачать для',
    ctaAny: 'Скачать',
    ctaMobile: 'Скачать на компьютер',
    other: 'Все системы',
    meta: ['бесплатно', 'открытый код, AGPL-3.0'],
    sceneLabel:
      'Анимация: текст, скопированный на компьютере с Windows, зашифрованным уходит по проводу и появляется в окне вставки на MacBook.',
  },

  scene: {
    devA: 'work-pc · Windows',
    devB: 'macbook · macOS',
    copied: 'скопировано',
    payloads: [
      {
        doc: 'Сервер поднимается одной командой: |docker compose up -d|, дальше смотрим лог.',
        row: { kind: 'code', text: 'docker compose up -d', meta: 'work-pc · Блокнот · сейчас · 20 симв.' },
      },
      {
        doc: 'Репозиторий для ревью: |https://github.com/erneywhite/clipwire| (ветка main).',
        row: { kind: 'link', text: 'https://github.com/erneywhite/clipwire', meta: 'work-pc · Firefox · сейчас' },
      },
      {
        doc: '#chart',
        row: { kind: 'image', text: 'Устройства за неделю', meta: 'work-pc · Paint · сейчас · 1200×739 · 42 КБ' },
      },
      {
        doc: '#file',
        row: { kind: 'file', text: 'report-q3.pdf', meta: 'work-pc · Проводник · сейчас · 2,4 МБ' },
      },
    ],
    fileName: 'report-q3.pdf',
    fileSize: '2,4 МБ',
    chartTitle: 'Устройства за неделю',
    rows: [
      { kind: 'text', text: 'Встреча переносится на четверг, 15:00. Переговорная 4B', meta: 'macbook · Почта · 12 мин' },
      { kind: 'link', text: 'https://docs.rs/iroh/latest/iroh/', meta: 'macbook · Safari · 40 мин' },
      { kind: 'text', text: 'Спасибо! Слайды пришлю до обеда.', meta: 'work-pc · Teams · 1 ч' },
    ],
  },

  paste: {
    search: 'Поиск по буферу',
    history: 'История',
    pinned: 'Закреплённые',
    hints: ['↵ вставить', '␣ предпросмотр', 'Ctrl P закрепить', 'Del удалить'],
  },

  how: {
    title: 'Что едет по проводу',
    lead: 'Всё скопированное через мгновение появляется на остальных устройствах, в истории с поиском.',
    steps: [
      {
        key: 'rich',
        title: 'Текст с оформлением',
        body: 'Из браузера, Word или почты текст приходит вместе с HTML и RTF. Enter вставляет его с оформлением, Shift+Enter без него.',
      },
      {
        key: 'image',
        title: 'Картинки',
        body: 'Скриншоты и картинки приходят в PNG без потерь, с крупной миниатюрой. Пробел открывает картинку целиком, Ctrl+E открывает редактор.',
      },
      {
        key: 'files',
        title: 'Файлы и папки',
        body: 'Карточка с именем и размером сразу появляется на всех устройствах. Файлы до 100 МБ (порог настраивается) сразу заливаются зашифрованными. Крупные приходят прямо с исходного компьютера в момент вставки.',
      },
      {
        key: 'links',
        title: 'Ссылки',
        body: 'По пробелу открывается карточка из og:-тегов страницы. Сама страница не исполняется, а ссылки, похожие на одноразовые (reset, token, вход по ссылке), вообще не загружаются.',
      },
    ],
    rich: {
      heading: 'Что нового',
      items: ['Окно вставки открывается у курсора', 'Закрепы синхронизируются', 'Исключения для файлов и папок'],
      plain: 'без оформления',
      formatted: 'с оформлением',
    },
    files: {
      card: 'clipwire-demo.mp4',
      size: '186 МБ',
      waiting: 'карточка пришла, файл ждёт вставки',
      pulling: 'вставка: файл идёт напрямую с work-pc',
      done: 'вставлено',
    },
    links: {
      okUrl: 'https://github.com/erneywhite/clipwire',
      okTitle: 'erneywhite/clipwire',
      okDesc: 'Общий буфер обмена для твоих компьютеров через свой сервер.',
      badUrl: 'https://accounts.example.com/reset?token=8f2c…',
      badNote: 'похоже на одноразовую, не загружается',
    },
  },

  security: {
    title: 'Сервер видит только шум',
    lead: 'Каждая запись шифруется на устройстве ключом группы. Ключ есть в коде приглашения и на устройствах, на сервере его нет.',
    secret: 'DB_PASSWORD=sunflower-orbit-42',
    server: 'свой сервер',
    stages: ['на work-pc', 'на сервере', 'на macbook'],
    points: [
      'Локальная история тоже зашифрована: DPAPI на Windows, связка ключей на macOS, Secret Service на Linux.',
      'То, что менеджеры паролей помечают как секретное, с устройства не уходит.',
      'Ноутбук потерян? Его можно удалить из группы: ключ сменится, остальные устройства получат новый.',
    ],
  },

  exclusions: {
    title: 'Не всё нужно отправлять',
    lead: 'Правила решают, что остаётся на этом компьютере: по расширению, имени, маске или размеру. Системный мусор отсекается сразу, менеджеры паролей исключены по умолчанию.',
    note: 'С включённым учётом .gitignore папка проекта уезжает без того, что в нём перечислено: без .env, сборок и прочего.',
    gate: 'исключения',
    sent: 'отправлено',
    items: [
      { label: 'notes.md', pass: true },
      { label: 'Thumbs.db', pass: false, why: 'системный файл' },
      { label: 'screenshot.png', pass: true },
      { label: '.env', pass: false, why: '.gitignore' },
      { label: 'Встреча в 15:00', pass: true },
      { label: 'KeePassXC', pass: false, why: 'менеджер паролей' },
      { label: 'invoice.pdf', pass: true },
      { label: 'node_modules/', pass: false, why: '.gitignore' },
      { label: '~$report.docx', pass: false, why: 'файл блокировки' },
      { label: 'deploy.sh', pass: true },
      { label: '.DS_Store', pass: false, why: 'системный файл' },
    ],
  },

  server: {
    title: 'Один контейнер. Свой сервер.',
    lead: 'Весь сервер помещается в один Docker-контейнер со встроенным relay iroh. Ставится из Docker-панели, SSH не нужен.',
    facts: [
      'В одной сети устройства соединяются напрямую, в остальных случаях идут через свой relay.',
      'Файлы хранятся на диске сервера, в S3-совместимом хранилище или на SFTP.',
      'Выключенные устройства забирают пропущенное из короткого почтового ящика со сроком хранения.',
      'Сервера пока нет? Программа работает как локальная история буфера на одном компьютере.',
    ],
    guide: 'Как поставить сервер',
    guideHref: 'https://github.com/erneywhite/clipwire/blob/main/README.ru.md#свой-сервер',
    file: 'docker-compose.yml',
  },

  try: {
    title: 'Попробуй окно вставки',
    lead: 'Ctrl+Shift+V открывает его рядом с курсором мыши (на macOS Cmd+Shift+V). Эта копия работает прямо здесь: стрелки, Enter, пробел для предпросмотра или просто начни печатать.',
    target: 'Твой документ',
    targetEmpty: 'Выбери запись и нажми Enter. Она окажется здесь.',
    focusHint: 'Щёлкни по окну и управляй с клавиатуры',
    noResults: 'Ничего не нашлось',
    shots: [
      { src: '/img/preview.png', alt: 'Окно вставки с предпросмотром картинки', caption: 'Предпросмотр открывается рядом со списком' },
      { src: '/img/settings-sync.png', alt: 'Настройки синхронизации: создать группу или войти в неё', caption: 'Вход в группу по коду приглашения' },
      { src: '/img/settings-exclusions.png', alt: 'Настройки исключений', caption: 'Исключения для файлов, папок и программ' },
    ],
    rows: [
      { kind: 'code', text: 'docker compose pull && docker compose up -d', meta: 'work-pc · Терминал · сейчас · 44 симв.' },
      { kind: 'link', text: 'https://github.com/erneywhite/clipwire', meta: 'Это устройство · Firefox · 2 мин' },
      { kind: 'text', text: 'Спасибо! Слайды пришлю до обеда.', meta: 'macbook · Почта · 9 мин · 32 симв.' },
      { kind: 'text', text: 'Встреча переносится на четверг, 15:00. Переговорная 4B', meta: 'work-pc · Outlook · 14 мин', tag: 'HTML' },
      { kind: 'image', text: 'Устройства за неделю', meta: 'Это устройство · Paint · 20 мин · 1200×739' },
      { kind: 'file', text: 'report-q3.pdf', meta: 'linux-box · Файлы · 1 ч · 2,4 МБ' },
      { kind: 'folder', text: 'clipwire-site', meta: 'work-pc · Проводник · 2 ч · 38 файлов' },
      { kind: 'text', text: 'Рига, Brīvības iela 40', meta: 'macbook · Карты · вчера' },
    ],
  },

  download: {
    title: 'Скачать Clipwire',
    lead: 'Один установщик на систему. Новые версии программа ставит сама.',
    latest: 'Последний выпуск',
    systems: [
      { os: 'windows', name: 'Windows', req: 'x64', files: [{ label: 'Установщик (.exe)', suffix: '_windows_x64-setup.exe' }] },
      { os: 'macos', name: 'macOS', req: '13 Ventura и новее, Apple Silicon и Intel', files: [{ label: 'Образ диска (.dmg)', suffix: '_macos_universal.dmg' }] },
      {
        os: 'linux',
        name: 'Linux',
        req: 'x86-64, X11 и Wayland',
        files: [
          { label: 'Debian, Ubuntu (.deb)', suffix: '_linux_amd64.deb' },
          { label: 'AppImage', suffix: '_linux_amd64.AppImage' },
        ],
      },
    ],
    serverLine: 'Образ сервера',
    allReleases: 'Все выпуски и список изменений',
    yours: 'твоя система',
  },

  support: {
    title: 'Бесплатно, делает один человек',
    body: 'Clipwire бесплатный, платной версии нет. Если он экономит тебе время, можно поддержать автора на Ko-fi.',
    kofi: 'Поддержать на Ko-fi',
    issues: 'Нашёл ошибку или есть идея? Расскажи о ней в Issues.',
    issuesLink: 'Открыть Issues',
  },

  footer: { license: 'AGPL-3.0', source: 'Исходный код' },
};

export const strings: Record<Lang, Strings> = { en, ru };
