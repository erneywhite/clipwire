# Логотип Clipwire: задание для генерации

Логотип нужен в трёх местах: иконка приложения (ярлык, панель задач, установщик), значок в трее
и превью репозитория. Генерируем варианты в ChatGPT, выбранный перерисовываем в SVG вручную:
нейросеть не даёт чистых контуров, а на 16 px каждый пиксель важен.

## Что это за продукт

Clipwire — общий буфер обмена для Windows, macOS и Linux. Скопировал на одном компьютере,
вставил на другом. Работает через собственный сервер владельца, всё шифруется на устройствах.
Характер: утилита для тех, кто много копирует (админы, разработчики), спокойная и точная, без
игривости.

## Требования

- **Читается на 16×16.** Это значок в трее Windows, на светлой и тёмной панели задач. Один
  простой силуэт, толстые линии, никаких мелких деталей и тонких штрихов.
- **Без текста и букв**, кроме случая, когда сама форма — буква «C».
- **Плоский и геометрический:** 2–3 цвета, без 3D, бликов, фотореализма и сложных градиентов.
- **Без канцелярской скрепки.** Слово «clip» тянет к ней, но это штамп, и скрепка не говорит
  о синхронизации.
- **Не похож** на иконку Paste (стопка цветных квадратов), на системный буфер Windows и на
  иконки Apple.
- **Правый нижний угол свободен.** В трее туда ляжет маленькая цветная точка статуса
  (на связи, нет связи, пауза).

## Идея

Буфер обмена плюс провод или связь между устройствами. Варианты, которые стоит попробовать:

1. Силуэт планшета-буфера (clipboard), где зажим наверху — узел или петля из провода.
2. Два листа со сдвигом, соединённые одной непрерывной линией.
3. Буква «C», собранная из провода, со штекером или точкой соединения на конце.

## Цвета

Базовая тема приложения «Графит»: фон `#1c1e22`, акцент янтарный `#e2a63b`, светлый
`#e9e6df`. Иконка приложения — на скруглённом квадрате графитового цвета. Значок для трея —
одноцветный силуэт, должен работать и светлым на тёмном, и тёмным на светлом.

## Что получить от ChatGPT

1. Сетку из 3–4 концептов на одном изображении, по одному на каждую идею выше.
2. После выбора — выбранный концепт крупно, 1024×1024, на скруглённом квадрате.
3. Тот же знак одним цветом без фона (для трея), прозрачный PNG.

## Промпт (английский, на нём генерация точнее)

```
Design an app icon for "Clipwire", a cross-platform clipboard sync utility: copy on one
computer, paste on another, end-to-end encrypted, self-hosted. Calm, precise, technical tool
for sysadmins and developers.

Show a grid of 4 distinct concepts on one image:
1) a clipboard silhouette whose top clip is formed by a loop or knot of a single wire;
2) two offset sheets connected by one continuous line;
3) a letter "C" built from a cable, ending in a small plug or connection dot;
4) your own idea along the same lines.

Rules: flat geometric vector style, 2-3 colors, bold strokes, no text, no 3D, no gloss,
no photorealism, no paperclip. Must stay recognizable at 16x16 pixels as a tray icon.
Keep the bottom-right corner visually empty (a small status dot will go there).
Palette: graphite #1c1e22 rounded-square background, amber #e2a63b accent, off-white #e9e6df.
Each concept on its own rounded-square tile, plain neutral background around the tiles.
```

Для второго шага, когда концепт выбран:

```
Take concept N and render it as a single app icon, 1024x1024, centered on a graphite #1c1e22
rounded square (iOS/macOS-style corner radius), amber #e2a63b and off-white #e9e6df only.
Then render the same mark alone as a single-color white silhouette on a transparent
background, for a 16x16 tray icon: simplify details, thicken strokes.
```

## Дальше

Выбранную картинку перерисовываю в SVG и генерирую из неё все размеры для Windows, macOS
и Linux командой `tauri icon`. Превью репозитория (1280×640) собираю из логотипа и настоящих
скриншотов программы.
