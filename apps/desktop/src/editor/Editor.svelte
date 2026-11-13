<script lang="ts">
  // Редактор картинки из истории: стрелки, перо, маркер, рамки, текст в плашке,
  // размытие и обрезка. Рисуем в пикселях картинки на canvas её размера; на
  // экране canvas вписан в окно. Результат уходит в Rust готовым PNG.
  import { onMount, tick } from 'svelte'
  import { invoke } from '@tauri-apps/api/core'
  import { listen } from '@tauri-apps/api/event'
  import { getCurrentWindow } from '@tauri-apps/api/window'
  import { api, type Settings } from '../lib/api'
  import { applyTheme } from '../lib/theme'
  import TitleBar from '../settings/TitleBar.svelte'
  import {
    COLORS,
    arrowHead,
    blurBlock,
    bounds,
    clampCrop,
    constrain,
    distToSegment,
    fontSize,
    inside,
    moved,
    norm,
    strokeWidth,
    textOn,
    unit,
    type Pt,
    type Rect,
    type Shape,
    type Tool,
  } from './shapes'

  const win = getCurrentWindow()

  const TOOLS: { tool: Tool; key: string; label: string; icon: string }[] = [
    { tool: 'select', key: 'V', label: 'Выбор и перемещение', icon: 'M6 3l12 9-5.5 1L10 19z' },
    { tool: 'arrow', key: 'A', label: 'Стрелка', icon: 'M5 19L19 5M10 5h9v9' },
    { tool: 'pen', key: 'P', label: 'Перо', icon: 'M4 20l4.5-1L19 8.5 15.5 5 5 15.5zM13.5 7l3.5 3.5' },
    { tool: 'marker', key: 'M', label: 'Маркер', icon: 'M8 16l-3 4h7l1.5-2M8 16l8-11 4 3-8 10z' },
    { tool: 'rect', key: 'R', label: 'Рамка', icon: 'M4 6h16v12H4z' },
    { tool: 'ellipse', key: 'E', label: 'Эллипс', icon: 'M12 6c4.4 0 8 2.7 8 6s-3.6 6-8 6-8-2.7-8-6 3.6-6 8-6z' },
    { tool: 'text', key: 'T', label: 'Текст', icon: 'M5 6h14M12 6v13M9 19h6' },
    { tool: 'blur', key: 'B', label: 'Размыть', icon: 'M4 4h5v5H4zM15 4h5v5h-5zM9.5 9.5h5v5h-5zM4 15h5v5H4zM15 15h5v5h-5z' },
    { tool: 'crop', key: 'C', label: 'Обрезать', icon: 'M7 3v14h14M3 7h14v14' },
  ]
  const SIZES = ['S', 'M', 'L']

  let canvas: HTMLCanvasElement | undefined = $state()
  let stage: HTMLDivElement | undefined = $state()
  let textarea: HTMLTextAreaElement | undefined = $state()

  let image: HTMLImageElement | null = null
  let W = $state(0)
  let H = $state(0)
  let tool = $state<Tool>('arrow')
  let color = $state(COLORS[0])
  let size = $state(1)
  let shapes = $state<Shape[]>([])
  let crop = $state<Rect | null>(null)
  let selected = $state<number | null>(null)
  let undoStack = $state<string[]>([])
  let redoStack = $state<string[]>([])
  let busy = $state(false)
  let error = $state('')
  let escArmed = $state(false)
  let nextId = 1

  // Что сейчас рисуется мышью.
  let draft: Shape | null = null
  let cropDraft: { a: Pt; b: Pt } | null = null
  let drag: { start: Pt; orig: Shape; moved: boolean } | null = null
  // Редактируемый текст: новая плашка или существующая (id).
  let editing = $state<{ at: Pt; id: number | null; value: string } | null>(null)
  let font = 'system-ui, sans-serif'
  // Клик мимо плашки завершает ввод; тот же клик не должен начинать новую.
  let committedAt = 0

  const u = $derived(unit(W, H))
  const dirty = $derived(undoStack.length > 0)

  // ---- Состояние и отмена ----

  function snapshot(): string {
    return JSON.stringify({ shapes, crop })
  }
  function restore(s: string) {
    const v = JSON.parse(s)
    shapes = v.shapes
    crop = v.crop
    selected = null
    render()
  }
  /** Вызывать перед каждым изменением. */
  function checkpoint() {
    undoStack = [...undoStack, snapshot()]
    redoStack = []
    escArmed = false
  }
  function undo() {
    if (!undoStack.length) return
    redoStack = [...redoStack, snapshot()]
    restore(undoStack[undoStack.length - 1])
    undoStack = undoStack.slice(0, -1)
  }
  function redo() {
    if (!redoStack.length) return
    undoStack = [...undoStack, snapshot()]
    restore(redoStack[redoStack.length - 1])
    redoStack = redoStack.slice(0, -1)
  }

  // ---- Рисование ----

  function textLines(s: Shape): string[] {
    return (s.text ?? '').split('\n')
  }

  /** Размер плашки с текстом. */
  function textBox(ctx: CanvasRenderingContext2D, s: Shape): { w: number; h: number; fs: number; pad: number } {
    const fs = fontSize(u, s.size)
    ctx.font = `500 ${fs}px ${font}`
    const pad = fs * 0.4
    const lines = textLines(s)
    const tw = Math.max(...lines.map((l) => ctx.measureText(l || ' ').width))
    return { w: tw + pad * 2, h: lines.length * fs * 1.25 + pad * 2 - fs * 0.25, fs, pad }
  }

  function smoothPath(ctx: CanvasRenderingContext2D, pts: Pt[]) {
    ctx.beginPath()
    ctx.moveTo(pts[0].x, pts[0].y)
    if (pts.length === 1) {
      ctx.lineTo(pts[0].x + 0.01, pts[0].y)
      return
    }
    for (let i = 1; i < pts.length - 1; i++) {
      const mx = (pts[i].x + pts[i + 1].x) / 2
      const my = (pts[i].y + pts[i + 1].y) / 2
      ctx.quadraticCurveTo(pts[i].x, pts[i].y, mx, my)
    }
    const last = pts[pts.length - 1]
    ctx.lineTo(last.x, last.y)
  }

  function pixelate(ctx: CanvasRenderingContext2D, r: Rect, block: number) {
    if (!image || r.w < 1 || r.h < 1) return
    const sw = Math.max(1, Math.ceil(r.w / block))
    const sh = Math.max(1, Math.ceil(r.h / block))
    const off = document.createElement('canvas')
    off.width = sw
    off.height = sh
    off.getContext('2d')!.drawImage(image, r.x, r.y, r.w, r.h, 0, 0, sw, sh)
    ctx.save()
    ctx.imageSmoothingEnabled = false
    ctx.drawImage(off, 0, 0, sw, sh, r.x, r.y, r.w, r.h)
    ctx.restore()
  }

  function drawShape(ctx: CanvasRenderingContext2D, s: Shape) {
    const w = strokeWidth(u, s.size)
    ctx.save()
    ctx.strokeStyle = s.color
    ctx.fillStyle = s.color
    ctx.lineWidth = w
    ctx.lineCap = 'round'
    ctx.lineJoin = 'round'
    switch (s.kind) {
      case 'arrow': {
        const [tip, l, r] = arrowHead(s.a, s.b, w)
        // Линия до основания наконечника, чтобы круглый край не торчал из острия.
        const base = { x: (l.x + r.x) / 2, y: (l.y + r.y) / 2 }
        ctx.beginPath()
        ctx.moveTo(s.a.x, s.a.y)
        ctx.lineTo(base.x, base.y)
        ctx.stroke()
        ctx.beginPath()
        ctx.moveTo(tip.x, tip.y)
        ctx.lineTo(l.x, l.y)
        ctx.lineTo(r.x, r.y)
        ctx.closePath()
        ctx.lineWidth = w * 0.6
        ctx.fill()
        ctx.stroke()
        break
      }
      case 'rect': {
        const r = norm(s.a, s.b)
        ctx.strokeRect(r.x, r.y, r.w, r.h)
        break
      }
      case 'ellipse': {
        const r = norm(s.a, s.b)
        ctx.beginPath()
        ctx.ellipse(r.x + r.w / 2, r.y + r.h / 2, r.w / 2, r.h / 2, 0, 0, Math.PI * 2)
        ctx.stroke()
        break
      }
      case 'pen':
        smoothPath(ctx, s.pts ?? [s.a])
        ctx.stroke()
        break
      case 'marker':
        ctx.globalAlpha = 0.4
        ctx.lineWidth = w * 4
        smoothPath(ctx, s.pts ?? [s.a])
        ctx.stroke()
        break
      case 'blur':
        pixelate(ctx, norm(s.a, s.b), blurBlock(u, s.size))
        break
      case 'text': {
        const box = textBox(ctx, s)
        ctx.beginPath()
        ctx.roundRect(s.a.x, s.a.y, box.w, box.h, box.fs * 0.3)
        ctx.fill()
        ctx.fillStyle = textOn(s.color)
        ctx.textBaseline = 'top'
        textLines(s).forEach((line, i) => ctx.fillText(line, s.a.x + box.pad, s.a.y + box.pad + i * box.fs * 1.25))
        break
      }
    }
    ctx.restore()
  }

  /** Всё на canvas. `forExport` — без выделения и затемнения обрезки. */
  function paint(ctx: CanvasRenderingContext2D, forExport: boolean) {
    if (!image) return
    ctx.clearRect(0, 0, W, H)
    ctx.drawImage(image, 0, 0)
    for (const s of shapes) {
      if (editing && editing.id === s.id) continue
      drawShape(ctx, s)
    }
    if (draft) drawShape(ctx, draft)
    if (forExport) return

    const px = screenPx()
    const shown = cropDraft ? norm(cropDraft.a, cropDraft.b) : crop
    if (shown) {
      ctx.save()
      ctx.fillStyle = 'rgba(0, 0, 0, 0.55)'
      ctx.beginPath()
      ctx.rect(0, 0, W, H)
      ctx.rect(shown.x, shown.y, shown.w, shown.h)
      ctx.fill('evenodd')
      ctx.strokeStyle = '#ffffff'
      ctx.lineWidth = px * 1.5
      ctx.setLineDash([px * 6, px * 4])
      ctx.strokeRect(shown.x, shown.y, shown.w, shown.h)
      ctx.restore()
    }
    const sel = shapes.find((s) => s.id === selected)
    if (sel) {
      const r = shapeBounds(ctx, sel)
      const pad = strokeWidth(u, sel.size) + px * 4
      ctx.save()
      ctx.strokeStyle = accent()
      ctx.lineWidth = px * 1.5
      ctx.setLineDash([px * 5, px * 4])
      ctx.strokeRect(r.x - pad, r.y - pad, r.w + pad * 2, r.h + pad * 2)
      ctx.restore()
    }
  }

  function render() {
    const ctx = canvas?.getContext('2d')
    if (ctx) paint(ctx, false)
  }

  /** Вписать картинку в окно: крупные уменьшаются, мелкие увеличиваются.
   *  Это только показ, рисуем и сохраняем в исходном разрешении. */
  function layout() {
    if (!canvas || !stage || !W) return
    const cs = getComputedStyle(stage)
    const aw = stage.clientWidth - parseFloat(cs.paddingLeft) - parseFloat(cs.paddingRight)
    const ah = stage.clientHeight - parseFloat(cs.paddingTop) - parseFloat(cs.paddingBottom)
    const k = Math.max(0.01, Math.min(aw / W, ah / H))
    canvas.style.width = `${Math.floor(W * k)}px`
    canvas.style.height = `${Math.floor(H * k)}px`
  }

  function accent(): string {
    return getComputedStyle(document.documentElement).getPropertyValue('--c-accent').trim() || '#e2a63b'
  }

  /** Сколько пикселей картинки в одном пикселе экрана. */
  function screenPx(): number {
    if (!canvas) return 1
    const r = canvas.getBoundingClientRect()
    return r.width ? W / r.width : 1
  }

  function shapeBounds(ctx: CanvasRenderingContext2D, s: Shape): Rect {
    return s.kind === 'text' ? bounds(s, textBox(ctx, s)) : bounds(s)
  }

  function hit(p: Pt): Shape | null {
    const ctx = canvas?.getContext('2d')
    if (!ctx) return null
    const pad = screenPx() * 6
    for (let i = shapes.length - 1; i >= 0; i--) {
      const s = shapes[i]
      const w = strokeWidth(u, s.size)
      if (s.kind === 'arrow') {
        if (distToSegment(p, s.a, s.b) <= w + pad) return s
      } else if (inside(p, shapeBounds(ctx, s), w + pad)) {
        return s
      }
    }
    return null
  }

  // ---- Мышь ----

  function toImage(e: PointerEvent | MouseEvent): Pt {
    const r = canvas!.getBoundingClientRect()
    return {
      x: Math.max(0, Math.min(W, ((e.clientX - r.left) * W) / r.width)),
      y: Math.max(0, Math.min(H, ((e.clientY - r.top) * H) / r.height)),
    }
  }

  function onDown(e: PointerEvent) {
    if (e.button !== 0 || !image) return
    if (editing) {
      commitText()
      return
    }
    const p = toImage(e)
    canvas!.setPointerCapture(e.pointerId)
    escArmed = false

    if (tool === 'select') {
      const s = hit(p)
      selected = s?.id ?? null
      if (s) drag = { start: p, orig: s, moved: false }
      render()
      return
    }
    if (tool === 'text') {
      if (performance.now() - committedAt < 250) return
      // Клик по готовой плашке — правка её, иначе новая.
      const s = hit(p)
      if (s?.kind === 'text') startEdit(s)
      else editing = { at: p, id: null, value: '' }
      focusEditor()
      return
    }
    if (tool === 'crop') {
      cropDraft = { a: p, b: p }
      return
    }
    selected = null
    draft = { id: nextId++, kind: tool, color, size, a: p, b: p, pts: tool === 'pen' || tool === 'marker' ? [p] : undefined }
  }

  function onMove(e: PointerEvent) {
    if (!image) return
    const p = toImage(e)
    if (drag) {
      const dx = p.x - drag.start.x
      const dy = p.y - drag.start.y
      if (!drag.moved && Math.hypot(dx, dy) < screenPx() * 2) return
      if (!drag.moved) checkpoint()
      drag.moved = true
      shapes = shapes.map((s) => (s.id === drag!.orig.id ? moved(drag!.orig, dx, dy) : s))
      render()
      return
    }
    if (cropDraft) {
      cropDraft.b = e.shiftKey ? constrain('crop', cropDraft.a, p) : p
      render()
      return
    }
    if (draft) {
      if (draft.pts) draft.pts.push(p)
      draft.b = e.shiftKey && (draft.kind === 'arrow' || draft.kind === 'rect' || draft.kind === 'ellipse' || draft.kind === 'blur')
        ? constrain(draft.kind, draft.a, p)
        : p
      render()
      return
    }
    if (tool === 'select' && canvas) canvas.style.cursor = hit(p) ? 'move' : 'default'
  }

  function onUp() {
    if (drag) {
      drag = null
      return
    }
    if (cropDraft) {
      const r = clampCrop(norm(cropDraft.a, cropDraft.b), W, H)
      cropDraft = null
      if (r) {
        checkpoint()
        crop = r
      }
      render()
      return
    }
    if (draft) {
      const d = draft
      draft = null
      // Случайный клик без движения не оставляет фигуру (кроме точки пером).
      const tiny = !d.pts && Math.hypot(d.b.x - d.a.x, d.b.y - d.a.y) < screenPx() * 3
      if (!tiny) {
        checkpoint()
        shapes = [...shapes, d]
      }
      render()
    }
  }

  function onDouble(e: MouseEvent) {
    const s = hit(toImage(e))
    if (s?.kind === 'text') {
      startEdit(s)
      focusEditor()
    }
  }

  // ---- Текст ----

  function startEdit(s: Shape) {
    editing = { at: s.a, id: s.id, value: s.text ?? '' }
    color = s.color
    size = s.size
    render()
  }

  async function focusEditor() {
    await tick()
    textarea?.focus()
    textarea?.select()
  }

  function commitText() {
    if (!editing) return
    const { at, id, value } = editing
    editing = null
    committedAt = performance.now()
    const text = value.replace(/\s+$/, '')
    const old = shapes.find((s) => s.id === id)
    if (old && old.text === text && old.color === color && old.size === size) {
      render()
      return
    }
    if (old || text) checkpoint()
    if (old) {
      shapes = text ? shapes.map((s) => (s.id === id ? { ...s, text, color, size } : s)) : shapes.filter((s) => s.id !== id)
    } else if (text) {
      shapes = [...shapes, { id: nextId++, kind: 'text', color, size, a: at, b: at, text }]
    }
    render()
  }

  function cancelText() {
    editing = null
    render()
  }

  /** Где на экране стоит поле ввода текста. */
  const editorStyle = $derived.by(() => {
    if (!editing || !canvas || !stage) return ''
    const r = canvas.getBoundingClientRect()
    const st = stage.getBoundingClientRect()
    const k = r.width / W
    const fs = fontSize(u, size) * k
    return [
      `left: ${r.left - st.left + editing.at.x * k}px`,
      `top: ${r.top - st.top + editing.at.y * k}px`,
      `font-size: ${fs}px`,
      `padding: ${fs * 0.4}px`,
      `border-radius: ${fs * 0.3}px`,
      `background: ${color}`,
      `color: ${textOn(color)}`,
      `font-family: ${font}`,
    ].join(';')
  })

  // ---- Изменения выбранного ----

  function setColor(c: string) {
    color = c
    restyleSelected({ color: c })
  }
  function setSize(n: number) {
    size = n
    restyleSelected({ size: n })
  }
  function restyleSelected(patch: Partial<Shape>) {
    const s = shapes.find((x) => x.id === selected)
    if (!s) return
    checkpoint()
    shapes = shapes.map((x) => (x.id === selected ? { ...x, ...patch } : x))
    render()
  }
  function removeSelected() {
    if (selected === null) return
    checkpoint()
    shapes = shapes.filter((s) => s.id !== selected)
    selected = null
    render()
  }
  function pickTool(t: Tool) {
    if (editing) commitText()
    tool = t
    if (t !== 'select') selected = null
    if (canvas) canvas.style.cursor = t === 'select' ? 'default' : t === 'text' ? 'text' : 'crosshair'
    render()
  }

  // ---- Открыть, сохранить, закрыть ----

  async function open(id: string) {
    error = ''
    busy = false
    try {
      const entry = await api.get(id)
      if (!entry.image) throw new Error('У элемента нет картинки')
      const img = new Image()
      img.src = entry.image
      await img.decode()
      image = img
      W = img.naturalWidth
      H = img.naturalHeight
      shapes = []
      crop = null
      selected = null
      undoStack = []
      redoStack = []
      editing = null
      escArmed = false
      font = getComputedStyle(document.documentElement).getPropertyValue('--font-ui').trim() || font
      await tick()
      if (canvas) {
        canvas.width = W
        canvas.height = H
      }
      layout()
      pickTool(tool)
      render()
    } catch (e) {
      error = String(e)
    }
  }

  async function save(paste: boolean) {
    if (!image || busy) return
    if (editing) commitText()
    busy = true
    error = ''
    try {
      const full = document.createElement('canvas')
      full.width = W
      full.height = H
      paint(full.getContext('2d')!, true)
      let out = full
      if (crop) {
        out = document.createElement('canvas')
        out.width = crop.w
        out.height = crop.h
        out.getContext('2d')!.drawImage(full, crop.x, crop.y, crop.w, crop.h, 0, 0, crop.w, crop.h)
      }
      await invoke('editor_save', { png: out.toDataURL('image/png'), paste })
      undoStack = []
      redoStack = []
    } catch (e) {
      error = String(e)
    } finally {
      busy = false
    }
  }

  function close() {
    editing = null
    escArmed = false
    win.hide()
  }

  function onKey(e: KeyboardEvent) {
    if (editing) {
      // В поле текста: Enter — готово, Shift+Enter — новая строка, Esc — отмена.
      if (e.key === 'Enter' && !e.shiftKey && !e.ctrlKey) {
        e.preventDefault()
        commitText()
      } else if (e.key === 'Escape') {
        e.preventDefault()
        cancelText()
      }
      return
    }
    const ctrl = e.ctrlKey || e.metaKey
    if (ctrl && e.key === 'Enter') {
      e.preventDefault()
      save(true)
    } else if (ctrl && e.code === 'KeyS') {
      e.preventDefault()
      save(false)
    } else if (ctrl && e.code === 'KeyZ') {
      e.preventDefault()
      if (e.shiftKey) redo()
      else undo()
    } else if (ctrl && e.code === 'KeyY') {
      e.preventDefault()
      redo()
    } else if (e.key === 'Escape') {
      e.preventDefault()
      if (selected !== null) {
        selected = null
        render()
      } else if (dirty && !escArmed) {
        escArmed = true
      } else {
        close()
      }
    } else if (e.key === 'Delete' || e.key === 'Backspace') {
      removeSelected()
    } else if (!ctrl && !e.altKey) {
      const t = TOOLS.find((x) => e.code === `Key${x.key}`)
      if (t) pickTool(t.tool)
      const n = /^Digit([1-6])$/.exec(e.code)
      if (n) setColor(COLORS[Number(n[1]) - 1])
      if (e.key === '[') setSize(Math.max(0, size - 1))
      if (e.key === ']') setSize(Math.min(2, size + 1))
    }
  }

  function applySettings(s: Settings) {
    applyTheme(s.theme, s.mode)
  }

  onMount(() => {
    api.settings().then(applySettings)
    const ro = new ResizeObserver(() => {
      layout()
      render()
    })
    if (stage) ro.observe(stage)
    const unlisten = [
      listen<string>('editor://open', (e) => open(e.payload)),
      listen<Settings>('clipwire://settings-changed', (e) => applySettings(e.payload)),
    ]
    return () => {
      ro.disconnect()
      unlisten.forEach((p) => p.then((f) => f()))
    }
  })
</script>

<svelte:window onkeydown={onKey} />

<div class="editor">
  <TitleBar title={W ? `Изменить картинку · ${W}×${H}` : 'Изменить картинку'} />

  <div class="toolbar" role="toolbar" aria-label="Инструменты">
    {#each TOOLS as t (t.tool)}
      <button
        class="tool"
        class:on={tool === t.tool}
        title="{t.label} ({t.key})"
        aria-label={t.label}
        aria-pressed={tool === t.tool}
        onclick={() => pickTool(t.tool)}
      >
        <svg viewBox="0 0 24 24" aria-hidden="true"><path d={t.icon} /></svg>
      </button>
    {/each}
    <span class="sep"></span>
    {#each COLORS as c, i (c)}
      <button
        class="swatch"
        class:on={color === c}
        style:--swatch={c}
        title="Цвет ({i + 1})"
        aria-label="Цвет {i + 1}"
        aria-pressed={color === c}
        onclick={() => setColor(c)}
      ></button>
    {/each}
    <span class="sep"></span>
    {#each SIZES as s, i (s)}
      <button class="size" class:on={size === i} title="Толщина ([ и ])" aria-pressed={size === i} onclick={() => setSize(i)}>{s}</button>
    {/each}
    <span class="grow"></span>
    <button class="tool" title="Отменить (Ctrl+Z)" aria-label="Отменить" disabled={!undoStack.length} onclick={undo}>
      <svg viewBox="0 0 24 24" aria-hidden="true"><path d="M9 14L4 9l5-5M4 9h10a6 6 0 0 1 0 12h-3" /></svg>
    </button>
    <button class="tool" title="Повторить (Ctrl+Y)" aria-label="Повторить" disabled={!redoStack.length} onclick={redo}>
      <svg viewBox="0 0 24 24" aria-hidden="true"><path d="M15 14l5-5-5-5M20 9H10a6 6 0 0 0 0 12h3" /></svg>
    </button>
    <button class="action" disabled={busy} onclick={() => save(false)}>Сохранить</button>
    <button class="action primary" disabled={busy} onclick={() => save(true)}>Вставить</button>
  </div>

  <div class="stage" bind:this={stage}>
    <canvas
      bind:this={canvas}
      onpointerdown={onDown}
      onpointermove={onMove}
      onpointerup={onUp}
      onpointercancel={onUp}
      ondblclick={onDouble}
    ></canvas>
    {#if editing}
      <textarea
        bind:this={textarea}
        bind:value={editing.value}
        class="text-input"
        style={editorStyle}
        rows={Math.max(1, editing.value.split('\n').length)}
        spellcheck="false"
        placeholder="Текст"
        onblur={commitText}
      ></textarea>
    {/if}
  </div>

  <footer class="hints">
    {#if error}
      <span class="error">{error}</span>
    {:else if escArmed}
      <span class="warn">Изменения не сохранены. Esc ещё раз — закрыть без сохранения</span>
    {:else}
      <span>V выбор</span><span>A стрелка</span><span>P перо</span><span>R рамка</span><span>T текст</span>
      <span>B размыть</span><span>C обрезать</span><span>1–6 цвет</span><span>Ctrl Z отменить</span>
      <span>Ctrl ↵ вставить</span><span>Esc закрыть</span>
    {/if}
  </footer>
</div>

<style>
  .editor {
    height: 100vh;
    display: flex;
    flex-direction: column;
    background: var(--c-bg);
    color: var(--c-text);
  }
  .toolbar {
    flex: none;
    display: flex;
    align-items: center;
    gap: 2px;
    padding: var(--space-2) var(--space-3);
    border-bottom: 1px solid var(--c-border);
    background: var(--c-surface);
    flex-wrap: wrap;
  }
  button {
    border: 0;
    background: transparent;
    color: var(--c-muted);
    cursor: default;
    font: inherit;
  }
  .tool {
    width: 32px;
    height: 32px;
    display: grid;
    place-items: center;
    border-radius: var(--radius-s);
  }
  .tool svg {
    width: 18px;
    height: 18px;
    fill: none;
    stroke: currentColor;
    stroke-width: 1.8;
    stroke-linecap: round;
    stroke-linejoin: round;
  }
  .tool:hover:not(:disabled),
  .size:hover {
    background: var(--c-hover);
    color: var(--c-text);
  }
  .tool.on {
    background: var(--c-sel);
    color: var(--c-accent);
    box-shadow: inset 0 0 0 1px var(--c-accent);
  }
  .tool:disabled {
    opacity: 0.35;
  }
  .sep {
    width: 1px;
    height: 20px;
    background: var(--c-border);
    margin: 0 var(--space-2);
  }
  .grow {
    flex: 1;
  }
  .swatch {
    width: 20px;
    height: 20px;
    margin: 0 2px;
    border-radius: 50%;
    background: var(--swatch);
    box-shadow: inset 0 0 0 1px color-mix(in oklch, var(--c-text) 25%, transparent);
  }
  .swatch.on {
    box-shadow:
      0 0 0 2px var(--c-surface),
      0 0 0 3.5px var(--c-text);
  }
  .size {
    min-width: 26px;
    height: 28px;
    border-radius: var(--radius-s);
    font-family: var(--font-mono);
    font-size: var(--text-xs);
  }
  .size.on {
    color: var(--c-accent);
    background: var(--c-sel);
  }
  .action {
    height: 30px;
    padding: 0 var(--space-3);
    margin-left: var(--space-2);
    border-radius: var(--radius-m);
    border: 1px solid var(--c-border);
    color: var(--c-text);
    font-size: var(--text-s);
  }
  .action:hover:not(:disabled) {
    background: var(--c-hover);
  }
  .action.primary {
    background: var(--c-accent);
    border-color: var(--c-accent);
    color: var(--c-on-accent);
  }
  .action.primary:hover:not(:disabled) {
    background: color-mix(in oklch, var(--c-accent) 88%, var(--c-text));
  }
  .action:disabled {
    opacity: 0.6;
  }
  .stage {
    flex: 1;
    min-height: 0;
    position: relative;
    display: grid;
    place-items: center;
    padding: var(--space-4);
    background: color-mix(in oklch, var(--c-bg) 80%, black);
    overflow: hidden;
  }
  canvas {
    display: block;
    touch-action: none;
    box-shadow: 0 0 0 1px var(--c-border);
  }
  .text-input {
    position: absolute;
    margin: 0;
    border: 0;
    outline: 2px dashed var(--c-accent);
    outline-offset: 3px;
    resize: none;
    overflow: hidden;
    line-height: 1.25;
    font-weight: 500;
    min-width: 4ch;
    field-sizing: content;
  }
  .text-input::placeholder {
    color: inherit;
    opacity: 0.55;
  }
  .hints {
    flex: none;
    display: flex;
    flex-wrap: wrap;
    gap: var(--space-3);
    padding: var(--space-2) var(--space-3);
    border-top: 1px solid var(--c-border);
    font-family: var(--font-mono);
    font-size: var(--text-xs);
    color: var(--c-muted);
  }
  .warn {
    color: var(--c-accent);
  }
  .error {
    color: var(--c-danger);
  }
</style>
