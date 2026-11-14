// Передачи файлов (через сервер и напрямую): что показывать в строке и в предпросмотре.
// Rust шлёт событие clipwire://transfer на каждый файл (blob), здесь из них
// собирается одна сводка на запись.
import type { FileInfo, Transfer } from './api'
import { t } from './i18n.ts'

/** Передачи по blob: последнее известное состояние каждого файла. */
export type TransferMap = Record<string, Transfer>

export interface Progress {
  direction: 'up' | 'down'
  done: number
  total: number
  /** 0…100, целое. */
  percent: number
  /** Есть файлы, которые ещё передаются. */
  active: boolean
  /** Текст первой ошибки, если что-то не передалось. */
  error: string | null
}

/** Целый процент с округлением вниз: «100%» только когда передано всё. */
export function percentOf(done: number, total: number): number {
  if (total <= 0) return 0
  return Math.max(0, Math.min(100, Math.floor((done * 100) / total)))
}

/**
 * Суммарный прогресс по файлам записи. Возвращает null, если показывать
 * нечего: передач нет или все закончились успешно. Если идут и загрузка, и
 * скачивание (бывает редко), показывается скачивание: его ждёт пользователь.
 * Ошибка важнее прогресса, но не прячет его: полоса остаётся на месте сбоя.
 */
export function entryProgress(files: Pick<FileInfo, 'blob'>[], transfers: TransferMap): Progress | null {
  const list = files.flatMap((f) => (f.blob && transfers[f.blob] ? [transfers[f.blob]] : []))
  if (list.length === 0) return null
  const failed = list.find((x) => x.state === 'failed')
  const active = list.filter((x) => x.state === 'active')
  if (!failed && active.length === 0) return null
  const direction = (failed ?? active.find((x) => x.direction === 'down') ?? active[0]).direction
  const same = list.filter((x) => x.direction === direction)
  const total = same.reduce((n, x) => n + x.total, 0)
  // Готовый файл засчитывается целиком, даже если последнее событие о нём
  // пришло с done чуть меньше total.
  const done = same.reduce((n, x) => n + (x.state === 'done' ? x.total : Math.min(x.done, x.total)), 0)
  return {
    direction,
    done,
    total,
    percent: percentOf(done, total),
    active: same.some((x) => x.state === 'active'),
    error: failed?.error || (failed ? t('неизвестная ошибка') : null),
  }
}

/**
 * Все файлы записи, которые отправлялись, уже на сервере. Полоса прогресса
 * видна только пока файл идёт, а это секунды: без этой пометки открывший окно
 * позже не узнает, ушёл ли файл. Сведения двух видов: события передач этого
 * запуска и поле uploaded (журнал загрузок за сутки, переживает перезапуск).
 */
export function allUploaded(files: Pick<FileInfo, 'blob' | 'uploaded'>[], transfers: TransferMap): boolean {
  const sent = files.filter((f) => f.blob && (f.uploaded || transfers[f.blob]))
  return (
    sent.length > 0 &&
    sent.every((f) => {
      const x = transfers[f.blob!]
      return f.uploaded || (x.direction === 'up' && x.state === 'done')
    })
  )
}

/** Подпись для меты строки: «↑ 45%», «↓ 12%», «не отправлено: причина». */
export function progressLabel(p: Progress): string {
  if (p.error) return p.direction === 'up' ? t('не отправлено: {error}', { error: p.error }) : t('не скачано: {error}', { error: p.error })
  return `${p.direction === 'up' ? '↑' : '↓'} ${p.percent}%`
}

/** Файл можно показать и вставить с этого компьютера. */
export function isHere(f: FileInfo): boolean {
  return f.local_path !== null && f.exists
}

/**
 * Файла или папки нет на этом компьютере, но их можно скачать: с сервера или
 * напрямую с устройства-источника (p2p). Чужие — если передавались (есть
 * blob). Свои пропавшие — только если копия на сервере: свой p2p-файл
 * брать неоткуда, источник — это мы сами.
 */
export function canDownload(f: FileInfo): boolean {
  return f.blob !== null && !isHere(f) && (f.remote || !f.p2p)
}

/** Скачивается напрямую с устройства-источника: оно должно быть в сети. */
export function isDirect(f: FileInfo): boolean {
  return f.remote && f.p2p && canDownload(f)
}

/** Файл с другого устройства, который не передавался вовсе (порог 0, обход папки не прошёл). */
export function onlyThere(f: FileInfo): boolean {
  return f.remote && !isHere(f) && f.blob === null
}

/**
 * Откуда возьмётся не скачанный файл — для меты строки и предпросмотра:
 * «напрямую с устройства «X»», «не скачан», «нет на месте, копия на сервере».
 * Для файла, который скачивать не надо или нельзя, — null.
 */
export function fetchLabel(f: FileInfo, device: string): string | null {
  if (!canDownload(f)) return null
  if (isDirect(f)) return t('напрямую с устройства «{device}»', { device })
  return f.remote ? t('не скачан') : t('нет на месте, копия на сервере')
}

/** Свой файл пропал с места, и копии на сервере нет: вставить нечего. */
export function isGone(f: FileInfo): boolean {
  return !f.remote && !isHere(f) && !canDownload(f)
}
