// Готовые команды и конфиги подключения Clipwire к ИИ-клиентам (вкладка
// «ИИ-инструменты»). Путь к программе приходит из ai_bridge_command.
import { t } from './i18n.ts'
import { isMac, isLinux } from './platform.ts'

export interface BridgeSnippet {
  id: 'claude' | 'codex' | 'json' | 'vscode'
  title: string
  text: string
}

/**
 * Аргумент для командной строки. Простой — как есть. Иначе на Windows в двойных
 * кавычках без экранирования обратных слэшей (cmd и PowerShell понимают путь
 * «C:\Users\me\my data» как есть; двойных кавычек в путях Windows не бывает),
 * на macOS и Linux в одинарных кавычках, где `$`, обратная кавычка и `"` ничего
 * не значат, а сама `'` пишется как `'\''`.
 */
export function shellArg(a: string, windows = !isMac && !isLinux): string {
  if (/^[\w@%+=:,./-]+$/.test(a)) return a
  if (windows) return `"${a}"`
  return `'${a.replace(/'/g, `'\''`)}'`
}

/**
 * Варианты подключения: команды Claude Code и Codex, JSON для клиентов с
 * конфигом mcpServers (Claude Desktop, Cursor) и JSON для VS Code, у которого
 * корневой ключ servers. Путь к программе в командах всегда в кавычках: в нём
 * бывают пробелы («Program Files»).
 */
export function bridgeSnippets(
  b: { exe: string; args: string[] },
  windows = !isMac && !isLinux,
): BridgeSnippet[] {
  const exe = shellArg(b.exe, windows)
  const quotedExe = exe.startsWith('"') || exe.startsWith("'") ? exe : windows ? `"${exe}"` : `'${exe}'`
  const tail = ['--', quotedExe, ...b.args.map((a) => shellArg(a, windows))].join(' ')
  const json = JSON.stringify({ mcpServers: { clipwire: { command: b.exe, args: b.args } } }, null, 2)
  const vscode = JSON.stringify({ servers: { clipwire: { type: 'stdio', command: b.exe, args: b.args } } }, null, 2)
  return [
    { id: 'claude', title: 'Claude Code', text: `claude mcp add clipwire ${tail}` },
    { id: 'codex', title: 'Codex', text: `codex mcp add clipwire ${tail}` },
    { id: 'json', title: t('Claude Desktop и Cursor (JSON)'), text: json },
    { id: 'vscode', title: t('VS Code (JSON, файл mcp.json)'), text: vscode },
  ]
}
