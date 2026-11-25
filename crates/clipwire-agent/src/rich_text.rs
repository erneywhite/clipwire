//! Простой текст из HTML и RTF: что получит ИИ-инструмент по
//! форматированной записи без простого текста и по чему проверяется
//! «похоже на пароль» (`ai_access::ai_text`).

// shortcut: свой разбор без библиотеки (теги, комментарии, сущности,
// управляющие слова RTF); таблицы и списки без оформления. Переделать на
// готовую библиотеку, если ИИ понадобится текст точнее.

/// Простой текст из HTML: без тегов, скриптов и стилей, с переводами строк
/// на месте блоков и с раскрытыми сущностями.
pub fn html_to_text(html: &str) -> String {
    // CF_HTML на Windows: служебный заголовок до фрагмента.
    let html = match html.find("<!--StartFragment-->") {
        Some(i) => &html[i..],
        None => html,
    };
    let mut out = String::new();
    let mut rest = html;
    while let Some(lt) = rest.find('<') {
        out.push_str(&decode_entities(&rest[..lt]));
        rest = &rest[lt..];
        if let Some(after) = rest.strip_prefix("<!--") {
            rest = after.find("-->").map_or("", |i| &after[i + 3..]);
            continue;
        }
        let Some(gt) = rest.find('>') else {
            rest = "";
            break;
        };
        let name: String = rest[1..gt]
            .trim_start_matches('/')
            .chars()
            .take_while(|c| c.is_ascii_alphanumeric())
            .collect::<String>()
            .to_ascii_lowercase();
        let closing = rest[1..].starts_with('/');
        rest = &rest[gt + 1..];
        if !closing && matches!(name.as_str(), "script" | "style" | "head" | "title") {
            let close = format!("</{name}");
            rest = match rest.to_ascii_lowercase().find(&close) {
                Some(i) => rest[i..].find('>').map_or("", |j| &rest[i + j + 1..]),
                None => "",
            };
            continue;
        }
        match name.as_str() {
            "br" | "p" | "div" | "tr" | "li" | "h1" | "h2" | "h3" | "h4" | "h5" | "h6" | "pre"
            | "blockquote" | "table" => out.push('\n'),
            "td" | "th" => out.push('\t'),
            _ => {}
        }
    }
    out.push_str(&decode_entities(rest));
    out.lines()
        .map(|l| l.split_whitespace().collect::<Vec<_>>().join(" "))
        .filter(|l| !l.is_empty())
        .collect::<Vec<_>>()
        .join("\n")
}

/// Сущности HTML: основные именованные и числовые.
fn decode_entities(s: &str) -> String {
    if !s.contains('&') {
        return s.to_string();
    }
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(i) = rest.find('&') {
        out.push_str(&rest[..i]);
        rest = &rest[i..];
        let decoded = rest[1..]
            .find(';')
            .filter(|&e| e <= 10)
            .and_then(|e| entity(&rest[1..=e]).map(|c| (c, e + 2)));
        match decoded {
            Some((c, len)) => {
                out.push(c);
                rest = &rest[len..];
            }
            None => {
                out.push('&');
                rest = &rest[1..];
            }
        }
    }
    out.push_str(rest);
    out
}

fn entity(name: &str) -> Option<char> {
    match name {
        "amp" => Some('&'),
        "lt" => Some('<'),
        "gt" => Some('>'),
        "quot" => Some('"'),
        "apos" => Some('\''),
        "nbsp" => Some(' '),
        _ => {
            let n = name.strip_prefix('#')?;
            let code = match n.strip_prefix(['x', 'X']) {
                Some(h) => u32::from_str_radix(h, 16).ok()?,
                None => n.parse().ok()?,
            };
            char::from_u32(code)
        }
    }
}

/// Служебные группы RTF, текст которых не показывается.
const RTF_SKIP: &[&str] = &[
    "fonttbl",
    "colortbl",
    "stylesheet",
    "info",
    "pict",
    "object",
    "header",
    "footer",
    "listtable",
    "listoverridetable",
    "generator",
    "themedata",
    "datastore",
    "latentstyles",
];

/// Простой текст из RTF: без управляющих слов и служебных групп (шрифты,
/// цвета, картинки), `\par` — перевод строки, `\'hh` и `\uN` — символы.
// shortcut: байты `\'hh` читаются как Latin-1, без кодовой страницы
// `\ansicpg`; кириллица в RTF обычно идёт через `\uN`, его хватает.
pub fn rtf_to_text(rtf: &[u8]) -> String {
    let s = String::from_utf8_lossy(rtf);
    let b: Vec<char> = s.chars().collect();
    let mut out = String::new();
    let mut i = 0;
    // Глубина групп и глубина, с которой группа пропускается.
    let mut depth = 0usize;
    let mut skip_from: Option<usize> = None;
    // После `\uN` идёт замена для старых читателей — один символ.
    let mut skip_chars = 0usize;
    while i < b.len() {
        let c = b[i];
        match c {
            '{' => {
                depth += 1;
                i += 1;
                if skip_from.is_none() && b.get(i) == Some(&'\\') && b.get(i + 1) == Some(&'*') {
                    skip_from = Some(depth);
                }
            }
            '}' => {
                if skip_from == Some(depth) {
                    skip_from = None;
                }
                depth = depth.saturating_sub(1);
                i += 1;
            }
            '\\' => {
                i += 1;
                let Some(&n) = b.get(i) else { break };
                if n.is_ascii_alphabetic() {
                    let st = i;
                    while i < b.len() && b[i].is_ascii_alphabetic() {
                        i += 1;
                    }
                    let word: String = b[st..i].iter().collect();
                    let nst = i;
                    if i < b.len() && (b[i] == '-' || b[i].is_ascii_digit()) {
                        i += 1;
                        while i < b.len() && b[i].is_ascii_digit() {
                            i += 1;
                        }
                    }
                    let num: Option<i32> = b[nst..i].iter().collect::<String>().parse().ok();
                    if i < b.len() && b[i] == ' ' {
                        i += 1;
                    }
                    if skip_from.is_none() && RTF_SKIP.contains(&word.as_str()) {
                        skip_from = Some(depth);
                    }
                    if skip_from.is_some() {
                        continue;
                    }
                    match word.as_str() {
                        "par" | "line" | "row" => out.push('\n'),
                        "tab" | "cell" => out.push('\t'),
                        "u" => {
                            if let Some(n) = num {
                                let code = if n < 0 { n + 65536 } else { n } as u32;
                                out.extend(char::from_u32(code));
                                skip_chars = 1;
                            }
                        }
                        _ => {}
                    }
                } else if n == '\'' {
                    let hex: String = b.get(i + 1..i + 3).unwrap_or(&[]).iter().collect();
                    i += 3;
                    if skip_chars > 0 {
                        skip_chars -= 1;
                    } else if skip_from.is_none()
                        && let Ok(v) = u8::from_str_radix(&hex, 16)
                    {
                        out.push(char::from(v));
                    }
                } else {
                    // `\\`, `\{`, `\}`; прочие одиночные символы — служебные.
                    i += 1;
                    if skip_from.is_none() && matches!(n, '\\' | '{' | '}') {
                        out.push(n);
                    }
                }
            }
            '\r' | '\n' => i += 1,
            _ => {
                i += 1;
                if skip_chars > 0 {
                    skip_chars -= 1;
                } else if skip_from.is_none() {
                    out.push(c);
                }
            }
        }
    }
    out.trim().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn html_to_plain_text() {
        let html = "<html><head><style>td{color:red}</style></head><body>\
                    <!--StartFragment--><table><tr><td>Логин</td><td>admin</td></tr>\
                    <tr><td>Пароль</td><td>Tr0ub4dor&amp;3</td></tr></table>\
                    <script>alert(1)</script><p>a&nbsp;&lt;b&gt; &#1046; &#x41;</p>\
                    <!--EndFragment--></body></html>";
        assert_eq!(
            html_to_text(html),
            "Логин admin\nПароль Tr0ub4dor&3\na <b> Ж A"
        );
        assert_eq!(html_to_text("просто & текст"), "просто & текст");
    }

    #[test]
    fn rtf_to_plain_text() {
        let rtf = br"{\rtf1\ansi\ansicpg1251{\fonttbl{\f0 Arial;}}{\colortbl;\red0\green0\blue0;}{\*\generator Word;}\f0\fs20 \u1055?\u1072?\u1088?\u1086?\u1083?\u1100?: Tr0ub4dor&3\par caf\'e9 \{x\}}";
        assert_eq!(rtf_to_text(rtf), "Пароль: Tr0ub4dor&3\ncafé {x}");
    }
}
