// Фирменный цвет ИИ-клиента для метки «ИИ · клиент». Клиента узнаём по имени,
// которое он сам сообщает в MCP (clientInfo.name), поэтому сравниваем по
// подстроке без учёта регистра: «claude-code», «Claude Desktop» → Claude.
// У OpenAI (Codex, ChatGPT) и Cursor фирменный цвет чёрный или белый: в одной
// из тем он пропадёт, поэтому они, как и незнакомые клиенты, остаются с
// фиолетовым --c-ai из темы (null).

const BRANDS: [RegExp, string | null][] = [
  // Cursor называет себя «cursor-vscode»: проверяем раньше VS Code.
  [/cursor|codex|openai|chatgpt/i, null],
  [/claude|anthropic/i, '#d97757'],
  [/gemini|google/i, '#4285f4'],
  [/copilot|vs ?code|visual studio/i, '#2f80ed'],
  [/windsurf|codeium/i, '#0fb5a5'],
  [/zed/i, '#3b82f6'],
  [/cline|roo/i, '#a855f7'],
]

export function aiBrand(client: string): string | null {
  for (const [re, color] of BRANDS) if (re.test(client)) return color
  return null
}
