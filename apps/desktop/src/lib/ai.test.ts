/// <reference types="node" />
// Проверка команд подключения к ИИ-клиентам. Запуск: node src/lib/ai.test.ts
// shortcut: тесты без фреймворка, как остальные *.test.ts; переделать при появлении vitest.
import { bridgeSnippets, shellArg } from './ai.ts'

function check(cond: boolean, msg: string) {
  if (!cond) throw new Error(`не прошло: ${msg}`)
}

const win = bridgeSnippets({ exe: 'C:\Program Files\Clipwire\clipwire-desktop.exe', args: ['--mcp'] }, true)
check(win[0].text === 'claude mcp add clipwire -- "C:\Program Files\Clipwire\clipwire-desktop.exe" --mcp', `claude: ${win[0].text}`)
check(win[1].text === 'codex mcp add clipwire -- "C:\Program Files\Clipwire\clipwire-desktop.exe" --mcp', `codex: ${win[1].text}`)
const parsed = JSON.parse(win[2].text)
check(parsed.mcpServers.clipwire.command === 'C:\Program Files\Clipwire\clipwire-desktop.exe', 'json: путь как есть')
check(JSON.stringify(parsed.mcpServers.clipwire.args) === '["--mcp"]', 'json: аргументы')
const vs = JSON.parse(win[3].text)
check(vs.servers.clipwire.type === 'stdio', 'vscode: type stdio')
check(vs.servers.clipwire.command === 'C:\Program Files\Clipwire\clipwire-desktop.exe', 'vscode: путь как есть')

// Путь Windows с пробелом в --home: обратные слэши не удваиваются.
const winHome = bridgeSnippets({ exe: 'C:\Clipwire\clipwire.exe', args: ['--mcp', '--home', 'C:\Users\me\my data'] }, true)
check(
  winHome[0].text === 'claude mcp add clipwire -- "C:\Clipwire\clipwire.exe" --mcp --home "C:\Users\me\my data"',
  `windows --home: ${winHome[0].text}`,
)

const home = bridgeSnippets({ exe: '/opt/Clipwire.AppImage', args: ['--mcp', '--home', '/home/me/my data'] }, false)
check(home[0].text === "claude mcp add clipwire -- '/opt/Clipwire.AppImage' --mcp --home '/home/me/my data'", `пробел в аргументе: ${home[0].text}`)

// На Unix $, обратная кавычка и " в пути не раскрываются, ' экранируется.
const odd = bridgeSnippets({ exe: '/opt/a $b`c"d\'e/clipwire', args: ['--mcp'] }, false)
check(odd[0].text === `claude mcp add clipwire -- '/opt/a $b\`c"d'\''e/clipwire' --mcp`, `спецсимволы в пути: ${odd[0].text}`)

check(shellArg('--mcp', false) === '--mcp', 'простой аргумент без кавычек')
check(shellArg("a'b", false) === "'a'\''b'", 'одинарная кавычка внутри на Unix')
check(shellArg('', false) === "''", 'пустой аргумент на Unix')
check(shellArg('', true) === '""', 'пустой аргумент на Windows')
check(shellArg('C:\a b\c', true) === '"C:\a b\c"', 'слэши на Windows как есть')

console.log('ai: ok')
