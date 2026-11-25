//! Что из истории видят ИИ-инструменты через MCP (`settings.ai`).
//!
//! Здесь только чистые проверки без ввода-вывода: виды записей
//! ([`kinds_of`]), видимость записи ([`AiPolicy`]) и эвристика «похоже на
//! пароль или ключ» ([`looks_secret`]). Канал MCP в приложении берёт отсюда
//! предикат для [`crate::history::History::scan_visible`] и повторно
//! проверяет полную запись перед выдачей ([`AiPolicy::check_content`]).

use clipwire_core::ai::{AiKind, AiSummary, PREVIEW_CHARS};
use clipwire_core::{Content, ExtList, FileRef};

use crate::history::{Card, CardKind, HEAD_CHARS, Row, is_url};
use crate::settings::{AiKinds, AiSettings};

/// Главный вид записи для [`AiSummary::kind`]. У записи из файлов и папок —
/// `File`. `None` — содержимое, которого эта версия не знает.
pub fn ai_kind(card: &Card) -> Option<AiKind> {
    kinds_of(card).first().copied()
}

/// Все виды записи по порядку [`AiKind`] (без повторов). Файл-картинка —
/// файл, а не картинка: её путь даёт агенту с доступом к диску сам файл.
pub fn kinds_of(card: &Card) -> Vec<AiKind> {
    match card.kind {
        CardKind::Text if card.link => vec![AiKind::Link],
        CardKind::Rich if card.link => vec![AiKind::Link],
        CardKind::Text => vec![AiKind::Text],
        CardKind::Rich => vec![AiKind::Rich],
        CardKind::Image => vec![AiKind::Image],
        CardKind::Files => file_kinds(&card.files),
        CardKind::Other => Vec::new(),
    }
}

/// То же по полному содержимому (для проверки перед выдачей записи).
pub fn content_kinds(content: &Content) -> Vec<AiKind> {
    match content {
        Content::Text(t) | Content::Rich { text: t, .. } if is_url(t) => vec![AiKind::Link],
        Content::Text(_) => vec![AiKind::Text],
        Content::Rich { .. } => vec![AiKind::Rich],
        Content::Image { .. } => vec![AiKind::Image],
        Content::Files(files) => file_kinds(files),
        _ => Vec::new(),
    }
}

fn file_kinds(files: &[FileRef]) -> Vec<AiKind> {
    let mut v = Vec::new();
    if files.iter().any(|f| !f.is_dir) {
        v.push(AiKind::File);
    }
    if files.iter().any(|f| f.is_dir) {
        v.push(AiKind::Folder);
    }
    v
}

/// Почему запись не видна ИИ (для журнала и ответа `Hidden`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HiddenBy {
    /// Вид записи (или один из видов) не разрешён, либо вид неизвестен.
    Kind,
    /// Старше окна «ИИ видит историю за».
    Age,
    /// Скопировано в программе из списка исключений.
    App,
    /// Файлы подпадают под правило «не отправлять» во вкладке «Исключения».
    Exclusion,
    /// Похоже на пароль или ключ.
    Secret,
}

/// Правила видимости, собранные из настроек на один запрос: настройки
/// читаются заново на каждый запрос, поэтому правка действует сразу.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AiPolicy {
    pub kinds: AiKinds,
    /// С какого времени (мс Unix) видны записи; `None` — вся история.
    pub since_ms: Option<i64>,
    pub hide_secrets: bool,
    /// Программы-исключения (`settings.ignored_apps`).
    pub ignored_apps: Vec<String>,
}

impl AiPolicy {
    pub fn new(ai: &AiSettings, ignored_apps: &[String], now_ms: i64) -> Self {
        Self {
            kinds: ai.kinds,
            since_ms: ai.visible_since(now_ms),
            hide_secrets: ai.hide_secrets,
            ignored_apps: ignored_apps.to_vec(),
        }
    }

    /// Граница времени для обхода истории (`cutoff_ms` у `scan_visible`).
    /// `ignore_window` — список закрепов или поиск только по закрепам: для
    /// них окно не действует.
    pub fn cutoff(&self, ignore_window: bool) -> Option<i64> {
        if ignore_window { None } else { self.since_ms }
    }

    /// Видна ли запись по карточке. Секрет проверяется по началу текста
    /// (`card.head`, регистр сохранён), по `card.search` (первые 8 КБ в
    /// нижнем регистре) и по подписи закрепа. Части правил нужен регистр,
    /// поэтому длинный и форматированный текст надо досмотреть по телу:
    /// [`AiPolicy::check_row`]. Одной этой проверки для выдачи мало.
    pub fn check_card(
        &self,
        seen_ms: i64,
        card: &Card,
        ignore_window: bool,
    ) -> Result<(), HiddenBy> {
        self.check_kinds(&kinds_of(card))?;
        if let Some(since) = self.cutoff(ignore_window)
            && seen_ms < since
        {
            return Err(HiddenBy::Age);
        }
        if let Some(app) = card.source_app()
            && app_ignored(&self.ignored_apps, &app.exe, &app.name)
        {
            return Err(HiddenBy::App);
        }
        if card.kind == CardKind::Files
            && crate::exclusions::blocks_send(&Content::Files(card.files.clone()))
        {
            return Err(HiddenBy::Exclusion);
        }
        if self.hide_secrets {
            if matches!(card.kind, CardKind::Text | CardKind::Rich)
                && (looks_secret(&card.head)
                    || (card.chars > HEAD_CHARS as u64 && looks_secret(&card.search)))
            {
                return Err(HiddenBy::Secret);
            }
            // Подпись закрепа видна в каждом списке и ищется поиском.
            if card.note().is_some_and(|n| looks_secret(&n)) {
                return Err(HiddenBy::Secret);
            }
        }
        Ok(())
    }

    /// Нужно ли досмотреть тело записи, чтобы решить про секрет: текст
    /// длиннее начала в карточке или форматированный (ИИ получит текст, а
    /// при пустом тексте — текст из HTML/RTF, которого в карточке нет).
    pub fn needs_body(&self, card: &Card) -> bool {
        self.hide_secrets
            && match card.kind {
                CardKind::Text => card.chars > HEAD_CHARS as u64,
                CardKind::Rich => true,
                _ => false,
            }
    }

    /// Полная проверка строки истории для поиска и списка закрепов: карточка
    /// и, если нужно ([`AiPolicy::needs_body`]), тело через `body`. Так
    /// поиск видит ровно то, что потом отдаст `Get`, и не служит оракулом
    /// для подбора скрытого текста по подстроке. Тело не читается — запись
    /// скрыта.
    pub fn check_row(
        &self,
        seen_ms: i64,
        card: &Card,
        ignore_window: bool,
        body: impl FnOnce() -> Option<Content>,
    ) -> Result<(), HiddenBy> {
        self.check_card(seen_ms, card, ignore_window)?;
        if self.needs_body(card) {
            match body() {
                Some(content) => self.check_content(&content)?,
                None => return Err(HiddenBy::Secret),
            }
        }
        Ok(())
    }

    /// Повторная проверка полной записи перед выдачей (`Get`): виды по
    /// содержимому, правила файлов и секрет по ВСЕМУ тексту, который получит
    /// ИИ ([`ai_text`]: у форматированного без текста — текст из HTML/RTF).
    /// Окно времени и программу-источник проверяет [`AiPolicy::check_card`]
    /// по карточке.
    pub fn check_content(&self, content: &Content) -> Result<(), HiddenBy> {
        self.check_kinds(&content_kinds(content))?;
        if crate::exclusions::blocks_send(content) {
            return Err(HiddenBy::Exclusion);
        }
        if self.hide_secrets
            && let Some((text, _)) = ai_text(content)
            && looks_secret(&text)
        {
            return Err(HiddenBy::Secret);
        }
        Ok(())
    }

    fn check_kinds(&self, kinds: &[AiKind]) -> Result<(), HiddenBy> {
        if kinds.is_empty() || !kinds.iter().all(|k| self.kinds.allows(*k)) {
            return Err(HiddenBy::Kind);
        }
        Ok(())
    }
}

/// Программа-источник в списке исключений. Кроме exe и имени, как при
/// копировании, понимает записи вида bundle id / .desktop id
/// (`org.keepassxc.KeePassXC`, `com.1password.1password`) по последней
/// части: сам id в карточке не хранится, а у записей с других устройств его
/// и не узнать. Совпадений выходит больше, чем при копировании, но лишнее
/// совпадение только прячет запись от ИИ.
pub fn app_ignored(list: &[String], exe: &str, name: &str) -> bool {
    use crate::service::is_ignored;
    if is_ignored(list, exe) || is_ignored(list, name) {
        return true;
    }
    let tails: Vec<String> = list
        .iter()
        .filter_map(|x| {
            let x = x.trim();
            let x = x.strip_suffix(".desktop").unwrap_or(x);
            let parts: Vec<&str> = x.split('.').collect();
            let id_like = parts.len() >= 3
                && parts.iter().all(|p| {
                    !p.is_empty()
                        && p.chars()
                            .all(|c| c.is_alphanumeric() || c == '-' || c == '_')
                });
            id_like.then(|| parts[parts.len() - 1].to_string())
        })
        .collect();
    !tails.is_empty() && (is_ignored(&tails, exe) || is_ignored(&tails, name))
}

/// Текст, который ИИ получит по записи, и взят ли он из разметки
/// (`AiItem::html_as_text`). У форматированного — его простой текст, а если
/// тот пуст — текст из HTML, иначе из RTF. `None` — у записи нет текста
/// (картинка, файлы). Секрет проверяется по этому же тексту
/// ([`AiPolicy::check_content`]), поэтому `Get` должен отдавать именно его.
pub fn ai_text(content: &Content) -> Option<(String, bool)> {
    use crate::rich_text::{html_to_text, rtf_to_text};
    match content {
        Content::Text(t) => Some((t.clone(), false)),
        Content::Rich { text, .. } if !text.trim().is_empty() => Some((text.clone(), false)),
        Content::Rich { html: Some(h), .. } => Some((html_to_text(h), true)),
        Content::Rich { rtf: Some(r), .. } => Some((rtf_to_text(r), true)),
        Content::Rich { text, .. } => Some((text.clone(), false)),
        _ => None,
    }
}

/// Краткие сведения о записи для ответа ИИ. Превью — начало текста или
/// имена файлов в одну строку, до [`PREVIEW_CHARS`] символов.
pub fn summary(row: &Row) -> AiSummary {
    let card = &row.card;
    let preview = match card.kind {
        CardKind::Files => card
            .files
            .iter()
            .map(|f| f.name.as_str())
            .collect::<Vec<_>>()
            .join(", "),
        CardKind::Image => format!("image {}×{}", card.width, card.height),
        _ => card.head.clone(),
    };
    let mut preview: String = preview
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .take(PREVIEW_CHARS)
        .collect();
    preview = preview.trim().to_string();
    let text = matches!(card.kind, CardKind::Text | CardKind::Rich);
    AiSummary {
        id: row.id.0.to_string(),
        kind: ai_kind(card).unwrap_or(AiKind::Text),
        preview,
        chars: if text {
            u32::try_from(card.chars).unwrap_or(u32::MAX)
        } else {
            0
        },
        device: card.device_name.clone(),
        app: card.source_app().map(|a| a.name),
        seen_ms: row.seen_ms,
        pinned: row.pinned,
        note: row.note(),
        by_ai: card.ai_origin().map(|o| o.client),
    }
}

// ---------------------------------------------------------------------------
// Похоже ли на пароль или ключ
// ---------------------------------------------------------------------------

/// Похож ли текст на пароль, токен или ключ. Эвристика с запасом в сторону
/// «скрыть»: ложное срабатывание только прячет запись от ИИ.
///
/// Срабатывает, если в тексте есть:
/// - известный префикс токена (`sk-`, `ghp_`, `github_pat_`, `glpat-`,
///   `xoxb-`, `AKIA…`, `AIza…`, `ya29.`, `hf_`, `npm_`, `SG.` и др.) или JWT;
/// - код Clipwire (`clipwire1-…`, `cwserver1-…`, `cwserver2-…`,
///   `cwgroup1-…`) любой длины: в нём ключ группы или ключ админа сервера;
/// - закрытый ключ PEM / OpenSSH / PGP или файл ключа PuTTY;
/// - номер платёжной карты (13–19 цифр, можно через пробел или дефис,
///   проходит проверку Луна);
/// - `password=`, `secret:`, `token=`, `pwd=`, `пароль:` и т. п. со
///   значением (и в прозе: «password is …», «пароль от роутера: …»), строка
///   `.env` вида `KEY=значение`, где значение похоже на случайное или имя
///   переменной говорит о секрете, заголовок `Authorization: Bearer …`,
///   `curl -u user:pass`;
/// - пароль в адресе (`scheme://user:pass@host`, `redis://:pass@host`,
///   `user:pass@host`), адрес-ключ (вебхуки Slack, Discord, Teams, бот
///   Telegram, DSN Sentry) или адрес с подписью или одноразовым кодом в
///   параметрах (`X-Amz-Signature`, `sig`, `code`, `token`…);
/// - отдельное слово 16–256 символов без пробелов, из ≥3 классов символов, с
///   энтропией ≥3,5 бита на символ, если это не адрес, не путь, не UUID, не
///   хэш (hex длиной 32/40/64), не почта, не версия и не слова через дефис;
/// - или весь текст — короткое (8–64 символа) слово без пробелов с
///   прописными и строчными буквами и цифрой или знаком (так выглядят
///   пароли), если это не что-то из безобидного выше, не имя продукта с
///   номером (`Windows11`, `Shift+Enter`) и не код;
/// - или весь текст — 64 hex-символа (можно с `0x`): закрытый ключ ETH/BTC;
/// - или весь текст — сид-фраза: 12/15/18/21/24 слова из строчных латинских
///   букв по 3–8 в каждом;
/// - или весь текст — одноразовый код из 6–8 цифр.
pub fn looks_secret(text: &str) -> bool {
    let t = text.trim();
    if t.is_empty() {
        return false;
    }
    if clipwire_code(t)
        || private_key(t)
        || short_password(t)
        || raw_hex_key(t)
        || seed_phrase(t)
        || one_time_code(t)
        || card_number(t)
    {
        return true;
    }
    if t.lines()
        .any(|l| assignment_secret(l) || env_secret(l) || auth_header(l) || cli_credentials(l))
    {
        return true;
    }
    tokens(t).any(|w| {
        known_prefix(w)
            || jwt(w)
            || url_password(w)
            || url_secret(w)
            || bare_userinfo(w)
            || generic_token(w)
    })
}

/// Начала кодов Clipwire: код приглашения и код группы несут ключ группы,
/// коды сервера — ключ доступа к серверу или ключ админа.
const CLIPWIRE_CODES: &[&str] = &["clipwire1-", "cwserver1-", "cwserver2-", "cwgroup1-"];

/// Код Clipwire где угодно в тексте, без учёта регистра. Отдельное правило,
/// а не `generic_token`: с длинным адресом сервера код выходит за 256
/// символов, и общее правило его уже не видит. Хвост — base32 (`a-z`, `2-7`)
/// от 16 символов, чтобы упоминание вида «код начинается с clipwire1-» не
/// пряталось.
fn clipwire_code(t: &str) -> bool {
    let lower = t.to_ascii_lowercase();
    CLIPWIRE_CODES.iter().any(|p| {
        lower.match_indices(p).any(|(i, _)| {
            lower[i + p.len()..]
                .chars()
                .take_while(|c| c.is_ascii_lowercase() || ('2'..='7').contains(c))
                .count()
                >= 16
        })
    })
}

/// Весь текст — 64 hex-символа, можно с `0x`: так выглядит закрытый ключ
/// Ethereum или Bitcoin. Тот же вид у sha256 и у id коммита в репозитории
/// на sha256: такие, вставленные отдельно, тоже прячутся — ошибка в другую
/// сторону дороже. Хэш внутри текста или с меткой (`sha256:…`) не трогается.
fn raw_hex_key(t: &str) -> bool {
    let h = t
        .strip_prefix("0x")
        .or_else(|| t.strip_prefix("0X"))
        .unwrap_or(t);
    h.len() == 64 && h.chars().all(|c| c.is_ascii_hexdigit())
}

/// Весь текст — сид-фраза кошелька (BIP39): 12, 15, 18, 21 или 24 слова из
/// строчных латинских букв, по 3–8 в слове.
fn seed_phrase(t: &str) -> bool {
    let words: Vec<&str> = t.split_whitespace().collect();
    matches!(words.len(), 12 | 15 | 18 | 21 | 24)
        && words
            .iter()
            .all(|w| (3..=8).contains(&w.len()) && w.chars().all(|c| c.is_ascii_lowercase()))
}

/// Весь текст — одноразовый код: 6–8 цифр. Пятизначные номера заказов,
/// годы и цены остаются видны; шестизначное число отдельно прячется.
fn one_time_code(t: &str) -> bool {
    (6..=8).contains(&t.len()) && t.chars().all(|c| c.is_ascii_digit())
}

/// Номер карты в тексте: 13–19 цифр подряд, между цифрами можно по одному
/// пробелу или дефису, первая цифра 2–6 (Visa, Mastercard, Amex, Discover,
/// «Мир»), сумма Луна сходится. Первая цифра отсекает метки времени в мс
/// (`1764…`), граница — числа внутри дробей, версий и слов: рядом с
/// номером не буква и не цифра, а точка, запятая, `/` или `:` — только если
/// за ними не цифра («карта 4111…1111, срок 12/29» — карта, `3.1411…` — нет).
fn card_number(t: &str) -> bool {
    let chars: Vec<char> = t.chars().collect();
    // `n` — соседний символ, `beyond` — следующий за ним.
    let edge = |n: Option<&char>, beyond: Option<&char>| {
        n.is_none_or(|&n| {
            !n.is_alphanumeric()
                && n != '_'
                && !(matches!(n, '.' | ',' | '/' | ':') && beyond.is_some_and(char::is_ascii_digit))
        })
    };
    let mut i = 0;
    while i < chars.len() {
        let before = i.checked_sub(1).and_then(|k| chars.get(k));
        let beyond = i.checked_sub(2).and_then(|k| chars.get(k));
        if !chars[i].is_ascii_digit() || !edge(before, beyond) {
            i += 1;
            continue;
        }
        // Число от `i`: цифры и одиночные разделители между ними.
        let mut digits = Vec::new();
        let mut j = i;
        while j < chars.len() {
            if chars[j].is_ascii_digit() {
                digits.push(chars[j] as u8 - b'0');
                j += 1;
            } else if matches!(chars[j], ' ' | '-')
                && chars.get(j + 1).is_some_and(char::is_ascii_digit)
            {
                j += 1;
            } else {
                break;
            }
        }
        if edge(chars.get(j), chars.get(j + 1))
            && (13..=19).contains(&digits.len())
            && (2..=6).contains(&digits[0])
            && luhn(&digits)
        {
            return true;
        }
        i = j.max(i + 1);
    }
    false
}

/// Проверка Луна: каждая вторая цифра справа удваивается, сумма делится
/// на 10.
fn luhn(digits: &[u8]) -> bool {
    let sum: u32 = digits
        .iter()
        .rev()
        .enumerate()
        .map(|(k, &d)| {
            let d = u32::from(d);
            match (k % 2, d * 2) {
                (1, x) if x > 9 => x - 9,
                (1, x) => x,
                _ => d,
            }
        })
        .sum();
    sum.is_multiple_of(10)
}

/// Закрытый ключ: PEM (`-----BEGIN … PRIVATE KEY-----`, в том числе
/// `OPENSSH` и `PGP PRIVATE KEY BLOCK`) или файл ключа PuTTY.
fn private_key(t: &str) -> bool {
    let lower = t.to_lowercase();
    lower
        .lines()
        .any(|l| l.contains("-----begin") && l.contains("private key"))
        || lower.contains("putty-user-key-file-")
        || lower.contains("private-lines:")
}

/// Классы символов: строчные, прописные, цифры, прочие (кроме пробелов).
fn classes(s: &str) -> u32 {
    let (mut lo, mut up, mut dg, mut sy) = (false, false, false, false);
    for c in s.chars() {
        if c.is_lowercase() {
            lo = true;
        } else if c.is_uppercase() {
            up = true;
        } else if c.is_numeric() {
            dg = true;
        } else if !c.is_whitespace() {
            sy = true;
        }
    }
    [lo, up, dg, sy].into_iter().filter(|b| *b).count() as u32
}

/// Энтропия Шеннона по символам, бит на символ.
fn entropy(s: &str) -> f64 {
    let mut counts = std::collections::HashMap::<char, u32>::new();
    let mut n = 0u32;
    for c in s.chars() {
        *counts.entry(c).or_default() += 1;
        n += 1;
    }
    if n == 0 {
        return 0.0;
    }
    let n = f64::from(n);
    counts
        .values()
        .map(|&k| {
            let p = f64::from(k) / n;
            -p * p.log2()
        })
        .sum()
}

/// Слова текста: по пробелам и по кавычкам, скобкам, запятым, точкам с
/// запятой. Адреса `data:` (картинка в base64) пропускаются целиком.
fn tokens(t: &str) -> impl Iterator<Item = &str> {
    let sep = |c: char| {
        matches!(
            c,
            '"' | '\'' | '`' | ',' | ';' | '(' | ')' | '[' | ']' | '{' | '}' | '<' | '>'
        ) || QUOTES.contains(&c)
    };
    t.split_whitespace()
        .filter(move |w| {
            !w.trim_start_matches(sep)
                .get(..5)
                .is_some_and(|p| p.eq_ignore_ascii_case("data:"))
        })
        .flat_map(move |w| w.split(sep))
        .map(|w| w.trim_matches(|c: char| matches!(c, '.' | ':' | '!' | '?')))
        .filter(|w| !w.is_empty())
}

fn is_b64url(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '-' || c == '_'
}

/// Известные префиксы токенов и минимальная длина хвоста после префикса.
const PREFIXES: &[(&str, usize)] = &[
    ("sk_live_", 16),
    ("sk_test_", 16),
    ("pk_live_", 16),
    ("rk_live_", 16),
    ("sk-", 20),
    ("ghp_", 20),
    ("gho_", 20),
    ("ghu_", 20),
    ("ghs_", 20),
    ("ghr_", 20),
    ("github_pat_", 20),
    ("glpat-", 16),
    ("xoxb-", 10),
    ("xoxp-", 10),
    ("xapp-", 10),
    ("AIza", 30),
    ("ya29.", 20),
    ("hf_", 20),
    ("npm_", 20),
    ("dop_v1_", 20),
    ("SG.", 20),
];

fn known_prefix(w: &str) -> bool {
    for (p, min) in PREFIXES {
        if let Some(tail) = w.strip_prefix(p) {
            let tail_ok = tail.chars().all(|c| is_b64url(c) || c == '.');
            if tail_ok && tail.chars().count() >= *min && classes(tail) >= 2 {
                return true;
            }
        }
    }
    // Ключ доступа AWS: AKIA/ASIA и ровно 16 прописных букв и цифр.
    if let Some(tail) = w.strip_prefix("AKIA").or_else(|| w.strip_prefix("ASIA")) {
        return tail.len() == 16
            && tail
                .chars()
                .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit());
    }
    false
}

/// JWT: `eyJ…` и ровно три части base64url через точку.
fn jwt(w: &str) -> bool {
    if !w.starts_with("eyJ") {
        return false;
    }
    let parts: Vec<&str> = w.split('.').collect();
    parts.len() == 3
        && parts[0].len() >= 10
        && parts[1].len() >= 10
        && parts[2].len() >= 16
        && parts.iter().all(|p| p.chars().all(is_b64url))
}

/// Пароль прямо в адресе: `scheme://user:pass@host`, у Redis и без
/// пользователя (`redis://:pass@host`).
fn url_password(w: &str) -> bool {
    let Some((_, rest)) = w.split_once("://") else {
        return false;
    };
    let auth = rest.split(['/', '?', '#']).next().unwrap_or("");
    let Some((userinfo, _host)) = auth.rsplit_once('@') else {
        return false;
    };
    matches!(userinfo.split_once(':'), Some((_, p)) if !p.is_empty())
}

/// Адреса, которые сами по себе ключ: кто знает адрес, тот и пишет.
const SECRET_URLS: &[&str] = &[
    "hooks.slack.com/services/",
    "discord.com/api/webhooks/",
    "discordapp.com/api/webhooks/",
    ".webhook.office.com/",
    "api.telegram.org/bot",
];

/// Параметры адреса, в которых бывает подпись, ключ или одноразовый код.
const SECRET_PARAMS: &[&str] = &[
    "sig",
    "signature",
    "x-amz-signature",
    "x-amz-credential",
    "x-amz-security-token",
    "x-goog-signature",
    "x-goog-credential",
    "code",
    "key",
    "api_key",
    "apikey",
    "auth",
    "access_token",
    "refresh_token",
    "id_token",
    "token",
    "otp",
    "reset",
    "magic",
    "password",
    "secret",
    "client_secret",
];

/// Адрес-ключ: вебхук, бот Telegram, ключ в имени пользователя (DSN
/// Sentry) или подпись и одноразовый код в параметрах.
fn url_secret(w: &str) -> bool {
    let Some((_, rest)) = w.split_once("://") else {
        return false;
    };
    let lower = rest.to_ascii_lowercase();
    if SECRET_URLS.iter().any(|h| {
        lower
            .find(h)
            .is_some_and(|i| lower.len() >= i + h.len() + 8)
    }) {
        return true;
    }
    let auth = rest.split(['/', '?', '#']).next().unwrap_or("");
    if let Some((user, _)) = auth.rsplit_once('@')
        && !user.contains(':')
        && user.chars().count() >= 16
        && user.chars().all(is_b64url)
        && classes(user) >= 2
    {
        return true;
    }
    if one_time_link(rest) {
        return true;
    }
    let Some((_, query)) = rest.split_once('?') else {
        return false;
    };
    let query = query.split('#').next().unwrap_or("");
    query.split('&').any(|kv| {
        let Some((k, v)) = kv.split_once('=') else {
            return false;
        };
        let n = v.chars().count();
        SECRET_PARAMS.contains(&k.to_ascii_lowercase().as_str())
            && n >= 8
            && (classes(v) >= 2 || n >= 16)
    })
}

/// Слова пути, по которым ссылка похожа на вход, сброс пароля или
/// приглашение (как `ONE_TIME` у карточек ссылок в link.rs).
const ONE_TIME_WORDS: &[&str] = &[
    "magic",
    "login",
    "signin",
    "auth",
    "oauth",
    "sso",
    "reset",
    "verify",
    "verification",
    "confirm",
    "activate",
    "invite",
    "token",
    "otp",
    "session",
    "password",
];

/// Ссылка входа или сброса пароля с токеном: в пути есть слово из
/// [`ONE_TIME_WORDS`], а в пути, параметрах или после `#` — случайная
/// строка от 16 знаков. Так выглядят magic-link входа
/// (`claude.ai/magic-link#…`), ссылки сброса пароля и приглашений.
fn one_time_link(rest: &str) -> bool {
    let (before_frag, frag) = rest.split_once('#').unwrap_or((rest, ""));
    let (path, query) = before_frag.split_once('?').unwrap_or((before_frag, ""));
    let path = path.split_once('/').map_or("", |(_, p)| p);
    let marked = path
        .to_ascii_lowercase()
        .split(|c: char| !c.is_ascii_alphanumeric())
        .any(|w| ONE_TIME_WORDS.contains(&w));
    if !marked {
        return false;
    }
    let token = |s: &str| {
        let n = s.chars().count();
        n >= 16 && (hex_hash(s) || classes(s) >= 2 && entropy(s) >= 3.0)
    };
    path.split('/')
        .chain(query.split(['&', '=']))
        .chain(frag.split([':', '&', '=', '/']))
        .any(token)
}

/// `user:pass@host` без схемы (`admin:S3cret@10.0.0.5`, строка `rsync`).
fn bare_userinfo(w: &str) -> bool {
    if w.contains("://") {
        return false;
    }
    let Some((userinfo, host)) = w.rsplit_once('@') else {
        return false;
    };
    let Some((user, pass)) = userinfo.split_once(':') else {
        return false;
    };
    let host = host.split('/').next().unwrap_or("");
    !user.is_empty()
        && !pass.is_empty()
        && !user.eq_ignore_ascii_case("mailto")
        && user
            .chars()
            .all(|c| c.is_alphanumeric() || matches!(c, '.' | '_' | '-'))
        && !host.is_empty()
        && host
            .chars()
            .all(|c| c.is_alphanumeric() || matches!(c, '.' | '-' | ':'))
}

/// `Authorization: Bearer …`, `Basic …`, `Authorization: token …`.
fn auth_header(line: &str) -> bool {
    let words: Vec<&str> = line.split_whitespace().collect();
    (0..words.len().saturating_sub(1)).any(|i| {
        let scheme = words[i]
            .trim_matches(|c: char| matches!(c, '"' | '\''))
            .to_lowercase();
        let after_header = i > 0
            && words[i - 1]
                .trim_matches(|c: char| matches!(c, '"' | '\''))
                .to_lowercase()
                .ends_with("authorization:");
        let ok =
            matches!(scheme.as_str(), "bearer" | "basic") || (after_header && scheme == "token");
        let v = words[i + 1].trim_matches(|c: char| matches!(c, '"' | '\'' | ',' | ';'));
        ok && v.chars().count() >= 12 && classes(v) >= 2 && !v.contains("://")
    })
}

/// `curl -u user:pass`, `--user user:pass`, `--user=user:pass`.
fn cli_credentials(line: &str) -> bool {
    let words: Vec<&str> = line.split_whitespace().collect();
    let pair = |v: &str| {
        let v = v.trim_matches(|c: char| matches!(c, '"' | '\''));
        matches!(v.split_once(':'), Some((u, p)) if !u.is_empty() && !p.is_empty())
            && !v.contains("://")
    };
    words.iter().enumerate().any(|(i, w)| match *w {
        "-u" | "--user" => words.get(i + 1).is_some_and(|v| pair(v)),
        _ => w.strip_prefix("--user=").is_some_and(pair),
    })
}

/// Слово-ключ, за которым идёт секрет, и как его искать.
struct SecretKey {
    word: &'static str,
    /// Только отдельным словом: `pass` — да, `bypass` и `passport` — нет.
    /// Нестрогое ищется и внутри имени (`DB_PASSWORD`, `SECRET_KEY_BASE`);
    /// с хвостом после слова значение оценивается строже.
    strict: bool,
    /// Значение может идти после «is», «это», пробела или нескольких слов
    /// до двоеточия: «password is …», «пароль от роутера: …».
    prose: bool,
    /// PIN и коды: значение из одних цифр тоже секрет (от 4 цифр).
    digits: bool,
}

const fn key(word: &'static str, strict: bool, prose: bool, digits: bool) -> SecretKey {
    SecretKey {
        word,
        strict,
        prose,
        digits,
    }
}

const SECRET_KEYS: &[SecretKey] = &[
    key("password", false, true, false),
    key("passwd", false, false, false),
    key("passphrase", false, true, false),
    key("passcode", false, true, true),
    key("secret", false, false, false),
    key("token", false, false, false),
    key("api_key", false, false, false),
    key("apikey", false, false, false),
    key("api-key", false, false, false),
    key("access_key", false, false, false),
    key("private_key", false, false, false),
    key("auth_key", false, false, false),
    key("credential", false, false, false),
    key("pwd", true, false, false),
    key("pass", true, false, false),
    key("pin", true, true, true),
    key("auth", true, false, false),
    key("wifi", true, true, false),
    key("wi-fi", true, true, false),
    key("пароль", false, true, false),
    key("пасс", true, false, false),
    key("ключ", true, true, false),
    key("токен", true, true, false),
    key("код доступа", false, true, true),
];

/// Символ имени: после ключа такие идут хвостом (`password_hash`).
fn ident_char(c: char) -> bool {
    c.is_alphanumeric() || c == '_' || c == '-'
}

/// `password=…`, `"secret": "…"`, `DB_PASSWORD: …`, `Pwd=…`, «Пароль: …»,
/// «password is …», «пароль от вайфая: …» со значением.
fn assignment_secret(line: &str) -> bool {
    // Нижний регистр посимвольно с той же длиной в байтах: смещения в
    // `lower` совпадают со смещениями в `line`, а значение берётся из
    // `line` (регистр важен для оценки).
    let lower: String = line
        .chars()
        .map(|c| {
            let mut l = c.to_lowercase();
            match (l.next(), l.next()) {
                (Some(x), None) if x.len_utf8() == c.len_utf8() => x,
                _ => c,
            }
        })
        .collect();
    for k in SECRET_KEYS {
        let mut from = 0;
        while let Some(pos) = lower[from..].find(k.word) {
            let start = from + pos;
            let mut end = start + k.word.len();
            from = end;
            if k.strict
                && lower[..start]
                    .chars()
                    .next_back()
                    .is_some_and(char::is_alphanumeric)
            {
                continue;
            }
            let tail_len = lower[end..]
                .chars()
                .take_while(|c| ident_char(*c))
                .map(char::len_utf8)
                .sum::<usize>();
            if tail_len > 0 && k.strict {
                continue;
            }
            end += tail_len;
            if key_value_secret(k, tail_len > 0, &line[end..]) {
                return true;
            }
        }
    }
    false
}

/// Как значение отделено от ключа.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Sep {
    /// `=`, `:`, тире.
    Assign,
    /// «is», «это», слова до двоеточия.
    Prose,
    /// Просто пробел: «пароль Tr0ub4dor&3».
    Bare,
}

/// Есть ли после ключа `k` (с хвостом имени `tail` или без) секретное
/// значение. `rest` — строка сразу после ключа и хвоста.
fn key_value_secret(k: &SecretKey, tail: bool, rest: &str) -> bool {
    let spaced = rest.starts_with(char::is_whitespace);
    let r = rest.trim_start_matches(['"', '\'']).trim_start();
    let (sep, after) = if let Some(a) = r.strip_prefix(['=', ':', '—', '–']) {
        (Sep::Assign, a)
    } else if !k.prose || tail {
        return false;
    } else if let Some(a) = ["is ", "это ", "- "].iter().find_map(|p| {
        r.get(..p.len())
            .filter(|h| h.eq_ignore_ascii_case(p))
            .map(|_| &r[p.len()..])
    }) {
        (Sep::Prose, a)
    } else if let Some(i) = r.find(':').filter(|&i| {
        let head = &r[..i];
        i <= 40 && !head.contains(['=', '\n']) && head.split_whitespace().count() <= 4
    }) {
        (Sep::Prose, &r[i + 1..])
    } else if spaced {
        (Sep::Bare, r)
    } else {
        return false;
    };
    let Some(v) = value_of(after) else {
        return false;
    };
    let n = v.chars().count();
    let pin = k.digits && n >= 4 && v.chars().all(|c| c.is_ascii_digit());
    match sep {
        _ if pin => true,
        Sep::Assign if tail => strict_secret_value(v),
        Sep::Assign => !harmless_value(v),
        Sep::Prose => !harmless_value(v) && n >= 6 && classes(v) >= 2,
        Sep::Bare => n >= 6 && classes(v) >= 3 && !harmless_token(v),
    }
}

/// Значение после `=` или `:`: в кавычках — целиком, иначе первое слово.
fn value_of(s: &str) -> Option<&str> {
    let s = s.trim_start_matches(['=', ' ', '\t']);
    let v = if let Some(q) = s.chars().next().filter(|c| matches!(c, '"' | '\'' | '`')) {
        let inner = &s[1..];
        &inner[..inner.find(q).unwrap_or(inner.len())]
    } else {
        s.split(char::is_whitespace).next().unwrap_or("")
    };
    let v = v.trim_end_matches([',', ';']);
    (!v.is_empty()).then_some(v)
}

/// Значение, которое не секрет: тип в коде, заглушка, переменная, вызов,
/// обычное слово.
fn harmless_value(v: &str) -> bool {
    const WORDS: &[&str] = &[
        "string",
        "str",
        "number",
        "int",
        "bool",
        "boolean",
        "null",
        "none",
        "nil",
        "undefined",
        "true",
        "false",
        "any",
        "required",
        "optional",
        "changeme",
        "secret",
        "password",
        "token",
    ];
    let lower = v.to_lowercase();
    let n = v.chars().count();
    let letters = v.chars().all(char::is_alphabetic);
    n < 4
        || WORDS.contains(&lower.as_str())
        || v.starts_with(['$', '{', '<', '%', '&', '*'])
        || v.contains('(')
        || v.chars()
            .all(|c| c == '*' || c == 'x' || c == 'X' || c == '.')
        || (v.chars().all(|c| c.is_ascii_digit()) && v.len() < 6)
        // Короткое слово («Token=Value») и русское слово: пароли кириллицей
        // почти не бывают, а «Пароль: введите ниже» — обычная фраза.
        || (letters && n < 6)
        || (letters && v.chars().any(|c| matches!(c, 'а'..='я' | 'А'..='Я' | 'ё' | 'Ё')))
        || code_like(v)
}

/// Имя переменной говорит о секрете: часть через `_` — `SECRET`, `KEY`,
/// `TOKEN`, `PASS`, `PWD`, `AUTH`, `PRIVATE` и т. п.
fn secret_name(key: &str) -> bool {
    key.split(['_', '-']).any(|seg| {
        matches!(
            seg.to_ascii_uppercase().as_str(),
            "SECRET"
                | "SECRETS"
                | "KEY"
                | "APIKEY"
                | "TOKEN"
                | "PASS"
                | "PASSWORD"
                | "PASSWD"
                | "PWD"
                | "AUTH"
                | "CREDENTIAL"
                | "CREDENTIALS"
                | "PRIVATE"
                | "SIGNATURE"
                | "SALT"
                | "DSN"
                | "SEED"
                | "OTP"
                | "TOTP"
        )
    })
}

/// Значение переменной с «секретным» именем: хэш и UUID тут тоже секрет
/// (`SECRET_KEY_BASE=<hex>`, `HEROKU_API_KEY=<uuid>`), а адрес, путь и
/// версия — нет.
fn strict_secret_value(v: &str) -> bool {
    let n = v.chars().count();
    n >= 12
        && !v.contains(char::is_whitespace)
        && classes(v) >= 2
        && (v.chars().any(|c| c.is_ascii_digit()) || classes(v) >= 3)
        && entropy(v) >= 3.0
        && !(url(v)
            || path(v)
            || email(v)
            || file_name(v)
            || version(v)
            || code_like(v)
            || cli_flag(v))
}

/// Строка `.env`: `KEY=значение`, где значение похоже на случайное или имя
/// переменной говорит о секрете.
fn env_secret(line: &str) -> bool {
    let line = line.trim().trim_start_matches("export ").trim();
    let Some((key, value)) = line.split_once('=') else {
        return false;
    };
    let key_ok = key.chars().next().is_some_and(|c| c.is_ascii_uppercase())
        && key
            .chars()
            .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_');
    let v = value.trim().trim_matches(['"', '\'']);
    if !key_ok || v.chars().count() < 12 || v.contains(char::is_whitespace) {
        return false;
    }
    if secret_name(key) {
        return strict_secret_value(v);
    }
    // Флаги (`--max-old-space-size=4096`) и списки `имя=значение`
    // (`RUST_LOG=a=debug,b=warn`) — настройки, не ключи.
    !v.starts_with('-')
        && !v.trim_end_matches('=').contains('=')
        && classes(v) >= 3
        && entropy(v) >= 3.5
        && !harmless_token(v)
}

/// Отдельное длинное случайное слово.
fn generic_token(w: &str) -> bool {
    // У `ИМЯ=значение` оценивается значение: имя переменной не случайно.
    let w = if key_value(w) {
        w.split_once(['=', ':']).map_or(w, |(_, v)| v)
    } else {
        w
    };
    let n = w.chars().count();
    (16..=256).contains(&n) && classes(w) >= 3 && entropy(w) >= 3.5 && !harmless_token(w)
}

/// Весь текст — короткое слово, похожее на пароль: есть прописные и
/// строчные буквы и цифра или знак.
fn short_password(t: &str) -> bool {
    // Кавычки и скобки вокруг слова — оформление, а не часть пароля:
    // «ИИ-инструменты», "Shift+Enter", (Windows11).
    let t = t.trim_matches(|c| QUOTES.contains(&c));
    let n = t.chars().count();
    let (up, lo) = (
        t.chars().any(char::is_uppercase),
        t.chars().any(char::is_lowercase),
    );
    let other = t.chars().any(|c| !c.is_alphabetic());
    (8..=64).contains(&n)
        && !t.contains(char::is_whitespace)
        && up
        && lo
        && other
        && !harmless_short(t)
        && !datetime(t)
        && !key_value(t)
}

/// Кавычки и скобки, которыми обрамляют слово в тексте.
const QUOTES: &[char] = &[
    '«', '»', '„', '“', '”', '‘', '’', '"', '\'', '`', '(', ')', '[', ']', '{', '}',
];

/// Безобидное для правила коротких паролей. Код с одними точками
/// (`self.token2`) тут не в счёт, если в нём есть цифры: `My.Pass99` —
/// пароль.
fn harmless_short(t: &str) -> bool {
    let strong_code = ["::", "->", "=>", "()"].iter().any(|s| t.contains(s));
    url(t)
        || path(t)
        || uuid(t)
        || hex_hash(t)
        || email(t)
        || css_color(t)
        || version(t)
        || file_name(t)
        || product_version(t)
        || cli_flag(t)
        || word_segments(t)
        || (code_like(t) && (strong_code || !t.chars().any(|c| c.is_ascii_digit())))
}

/// `ИМЯ=значение` или `имя:значение`: такое оценивают правила присваиваний
/// и `.env`, а не правило коротких паролей.
fn key_value(t: &str) -> bool {
    t.split_once(['=', ':']).is_some_and(|(k, v)| {
        !k.is_empty()
            && !v.is_empty()
            && k.chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
    })
}

/// Безобидное слово: адрес, путь, UUID, хэш, почта, цвет CSS, код, версия,
/// имя файла, продукт с версией, флаг командной строки, слова через дефис.
fn harmless_token(w: &str) -> bool {
    url(w)
        || path(w)
        || uuid(w)
        || hex_hash(w)
        || email(w)
        || css_color(w)
        || code_like(w)
        || version(w)
        || file_name(w)
        || product_version(w)
        || cli_flag(w)
        || word_segments(w)
}

fn url(w: &str) -> bool {
    let l = w.to_ascii_lowercase();
    (w.contains("://") && !url_password(w) && !url_secret(w))
        || l.starts_with("www.")
        || l.starts_with("data:")
        || l.starts_with("mailto:")
}

fn path(w: &str) -> bool {
    let b = w.as_bytes();
    let drive =
        b.len() > 2 && b[0].is_ascii_alphabetic() && b[1] == b':' && matches!(b[2], b'\\' | b'/');
    if drive || w.starts_with(['/', '~', '\\']) || w.starts_with("./") || w.starts_with("../") {
        return true;
    }
    if w.contains('\\') {
        return true;
    }
    if !w.contains('/') {
        return false;
    }
    // Относительный путь: последний кусок с расширением, все куски из
    // букв, цифр и `._-`, или все куски — слова без цифр (base64 почти
    // всегда перемешан с цифрами).
    let segs: Vec<&str> = w.split('/').collect();
    let plain = |s: &str| {
        !s.is_empty()
            && s.chars()
                .all(|c| c.is_alphanumeric() || matches!(c, '.' | '_' | '-' | '@' | ':'))
    };
    let last = segs.last().copied().unwrap_or("");
    (segs.iter().all(|s| plain(s)) && file_name(last))
        || segs
            .iter()
            .all(|s| !s.is_empty() && s.chars().all(|c| c.is_alphabetic() || c == '_' || c == '-'))
        // Без прописных — не base64 (`ghcr.io/owner/name:1.2`).
        || (segs.iter().all(|s| plain(s)) && !w.chars().any(char::is_uppercase))
}

fn uuid(w: &str) -> bool {
    let parts: Vec<&str> = w.split('-').collect();
    parts.len() == 5
        && parts
            .iter()
            .zip([8, 4, 4, 4, 12])
            .all(|(p, n)| p.len() == n && p.chars().all(|c| c.is_ascii_hexdigit()))
}

/// Хэш git/sha: только hex длиной 32, 40 или 64, можно с меткой
/// (`sha256:…`).
fn hex_hash(w: &str) -> bool {
    let h = match w.split_once([':', '=']) {
        Some((label, h)) if label.chars().all(|c| c.is_ascii_alphanumeric()) => h,
        _ => w,
    };
    matches!(h.len(), 32 | 40 | 64) && h.chars().all(|c| c.is_ascii_hexdigit())
}

fn email(w: &str) -> bool {
    let Some((local, domain)) = w.split_once('@') else {
        return false;
    };
    !local.is_empty()
        && !domain.contains('@')
        && domain.contains('.')
        && !domain.starts_with('.')
        && !domain.ends_with('.')
        && local
            .chars()
            .all(|c| c.is_alphanumeric() || matches!(c, '.' | '_' | '-' | '+'))
        && domain
            .chars()
            .all(|c| c.is_alphanumeric() || matches!(c, '.' | '-'))
}

fn css_color(w: &str) -> bool {
    let l = w.to_ascii_lowercase();
    let l = l.trim_end_matches(';');
    (l.starts_with('#')
        && matches!(l.len(), 4 | 5 | 7 | 9)
        && l[1..].chars().all(|c| c.is_ascii_hexdigit()))
        || [
            "rgb(",
            "rgba(",
            "hsl(",
            "hsla(",
            "oklch(",
            "oklab(",
            "color-mix(",
        ]
        .iter()
        .any(|p| l.starts_with(p))
}

/// Версия: `1.2.3`, `v0.9.7-beta.1`, `2.0.0+build5`.
fn version(w: &str) -> bool {
    let w = w.strip_prefix(['v', 'V']).unwrap_or(w);
    let (core, suffix) = match w.find(['-', '+']) {
        Some(i) => (&w[..i], &w[i + 1..]),
        None => (w, ""),
    };
    let nums: Vec<&str> = core.split('.').collect();
    nums.len() >= 2
        && nums
            .iter()
            .all(|n| !n.is_empty() && n.chars().all(|c| c.is_ascii_digit()))
        && suffix
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '+'))
}

/// Продукт с версией, как в User-Agent: `AppleWebKit/537.36`, `Mozilla/5.0`.
fn product_version(w: &str) -> bool {
    let Some((name, ver)) = w.split_once('/') else {
        return false;
    };
    !name.is_empty()
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
        && (version(ver) || (!ver.is_empty() && ver.chars().all(|c| c.is_ascii_digit())))
}

/// Флаг командной строки: `--max-old-space-size=4096`, `-Xmx512m`.
fn cli_flag(w: &str) -> bool {
    let Some(rest) = w.strip_prefix('-') else {
        return false;
    };
    let rest = rest.trim_start_matches('-');
    !rest.is_empty() && rest.split(['-', '=', '_', '.']).all(word_or_number)
}

/// Слово, возможно с номером до 4 цифр в конце (`Windows11`, `x86`), или
/// число.
fn word_or_number(s: &str) -> bool {
    if s.is_empty() {
        return true;
    }
    let letters = s.trim_end_matches(|c: char| c.is_ascii_digit());
    let digits = s.len() - letters.len();
    letters.is_empty() || (digits <= 4 && letters.chars().all(char::is_alphabetic))
}

/// Слова и номера через `-`, `+`, `_`, `.` и угловые скобки:
/// `Monday-Friday`, `Shift+Enter`, `ru_RU.UTF-8`, `Python3.12`,
/// `Vec<Option<String>>`, `iPhone15`.
fn word_segments(w: &str) -> bool {
    w.chars()
        .all(|c| c.is_alphanumeric() || matches!(c, '-' | '+' | '_' | '.' | '<' | '>'))
        && w.chars().any(char::is_alphabetic)
        && w.split(['-', '+', '_', '.', '<', '>']).all(word_or_number)
}

/// Имя файла с расширением: `README.md`, `Cargo.toml`, `archive.tar.gz`.
/// Расширение без смеси регистров: `My.Pass99` — не файл.
fn file_name(w: &str) -> bool {
    let Some((name, ext)) = w.rsplit_once('.') else {
        return false;
    };
    let mixed =
        ext.chars().any(|c| c.is_ascii_uppercase()) && ext.chars().any(|c| c.is_ascii_lowercase());
    !name.is_empty()
        && (1..=6).contains(&ext.len())
        && !mixed
        && ext.chars().all(|c| c.is_ascii_alphanumeric())
        && ext.chars().any(|c| c.is_ascii_alphabetic())
        && name
            .chars()
            .all(|c| c.is_alphanumeric() || matches!(c, '.' | '_' | '-' | ' '))
}

/// Похоже на код: путь к имени (`std::io::Result`, `console.log()`,
/// `self.token`, `a->b`), где каждый кусок — слово, возможно с цифрой-двумя
/// в конце.
fn code_like(w: &str) -> bool {
    let has_sep = w.contains("::")
        || w.contains("->")
        || w.contains("=>")
        || w.contains("()")
        || w.contains('.');
    if !has_sep {
        return false;
    }
    let word = |p: &str| {
        let letters = p.trim_end_matches(|c: char| c.is_ascii_digit());
        let digits = p.len() - letters.len();
        !letters.is_empty()
            && digits <= 2
            && letters
                .chars()
                .all(|c| c.is_ascii_alphabetic() || c == '_' || c == '$')
    };
    w.split(|c: char| !(c.is_ascii_alphanumeric() || c == '_' || c == '$'))
        .filter(|p| !p.is_empty())
        .all(word)
        && w.chars()
            .all(|c| c.is_ascii_alphanumeric() || "_$.:->=()<&!?;".contains(c))
}

/// Дата и время ISO: только цифры, знаки и буквы `T`, `Z`.
fn datetime(w: &str) -> bool {
    w.chars().any(|c| c.is_ascii_digit())
        && w.chars()
            .all(|c| c.is_ascii_digit() || matches!(c, 'T' | 'Z' | ':' | '-' | '.' | '+' | '/'))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::history::{Entry, History, local_entry};
    use clipwire_core::{DeviceId, ItemId, SourceApp};

    const SECRETS: &[&str] = &[
        concat!("sk-p", "roj-AbC123dEf456GhI789jKl012MnO345"),
        concat!("sk_l", "ive_51HcXyZAbCdEfGhIjKlMnOp"),
        concat!("pk_l", "ive_51HcXyZAbCdEfGhIjKlMnOp"),
        concat!("rk_l", "ive_51HcXyZAbCdEfGhIjKlMnOp"),
        concat!("gh", "p_aB3dE5fG7hI9jK1lM3nO5pQ7rS9tU1vW3xY5"),
        concat!("gh", "o_aB3dE5fG7hI9jK1lM3nO5pQ7rS9tU1vW3xY5"),
        concat!(
            "githu",
            "b_pat_11ABCDEFG0abcdefghijkl_MNOPQRSTUVWXyz0123456789"
        ),
        concat!("glp", "at-xY12aB34cD56eF78gH90"),
        concat!(
            "xo",
            "xb-123456789012-1234567890123-AbCdEfGhIjKlMnOpQrStUvWx"
        ),
        concat!("AK", "IAIOSFODNN7EXAMPLE"),
        concat!("AS", "IAIOSFODNN7EXAMPLE"),
        concat!("AI", "zaSyD-1234567890abcdefGHIJKLMNOPqrstu"),
        concat!("ya", "29.a0AfH6SMBx1234567890abcdefGHIJKL"),
        concat!("hf", "_AbCdEfGhIjKlMnOpQrStUvWxYz012345"),
        concat!("np", "m_AbCdEfGhIjKlMnOpQrStUvWxYz0123456789"),
        concat!("SG", ".AbCdEfGhIjKlMnOpQrSt.UvWxYz0123456789AbCdEfGhIjKl"),
        concat!(
            "ey",
            "JhbGciOiJIUzI1NiIsInR5cCI6IkpXVCJ9.eyJzdWIiOiIxMjM0NTY3ODkwIn0.dozjgNryP4J3jVmNHl0w5N_XgL0n3I9PlFUP0THsR8U"
        ),
        "-----BEGIN RSA PRIVATE KEY-----\nMIIEpAIBAAKCAQEA\n-----END RSA PRIVATE KEY-----",
        "-----BEGIN OPENSSH PRIVATE KEY-----\nb3BlbnNzaC1rZXktdjEAAAAA\n-----END OPENSSH PRIVATE KEY-----",
        "password=hunter22",
        "DB_PASSWORD: s3cr3tValue",
        r#"{"api_key": "a8F3kL9qPz"}"#,
        "export SECRET=abcd1234efgh",
        "STRIPE_KEY=Xk9fL2mQp7RtZ4vB8nW1",
        "postgres://admin:Sup3rPass@db.example.com:5432/app",
        "Tr0ub4dor&3",
        "P@ssw0rd!",
        "Summer2024!",
        "Authorization: Bearer Zx8Kq2Lp9Vt4Rw7Ny1Mb6Hc3",
        "мой пароль: password: Qwerty12345",
        "token=7fG2kP9xQ1zL",
        // Адреса-ключи и подписи.
        concat!(
            "https://hook",
            "s.slack.com/services/T024BE7LD/B01ABCDEFGH/aBcDeFgHiJkLmNoPqRsTuVwX"
        ),
        concat!(
            "https://discord.",
            "com/api/webhooks/123456789012345678/AbCdEfGhIjKlMnOpQrStUv"
        ),
        concat!(
            "https://bucket.s3.amazonaws.com/f.zip?X-Amz-Credential=AK",
            "IAIOSFODNN7EXAMPLE%2F20261124&X-Amz-Signature=3f1a9c0e7b2d4f6a8c0e2b4d6f8a0c2e"
        ),
        "https://example.com/magic?code=Xy7kP2qL9mN4",
        "SENTRY_DSN=https://0123456789abcdef0123456789abcdef@o123.ingest.sentry.io/123",
        concat!(
            "https://api.te",
            "legram.org/bot123456789:AAHdqTcvCH1vG",
            "WJxfSeofSAs0K5PALDsaw/getMe"
        ),
        "redis://:p4ssw0rd@redis.example.com:6379",
        // Ссылки входа и сброса пароля: токен в пути или после `#`.
        "https://example.com/magic-link?client=desktop_app#0123456789abcdef0123456789abcdef:dXNlckBleGFtcGxlLmNvbQ==",
        "https://app.example.com/reset-password/Xk9fL2mQp7RtZ4vB8nW1",
        "https://example.com/invite/AbCdEf123456GhIjKlMn",
        "https://example.com/auth/verify#Zx8Kq2Lp9Vt4Rw7Ny1Mb6Hc3",
        "REDIS_URL=redis://:p4ssw0rd@redis.example.com:6379/0",
        // Ключи на других языках и в прозе.
        "Пароль: Tr0ub4dor&3",
        "пароль от вайфая: Kx7mP2qL9",
        "my password is Tr0ub4dor&3",
        "wifi: Kx7mP2qL9z",
        "pwd=Hunter2Pass",
        "user=admin pass=hunter2pass",
        "Data Source=.;Initial Catalog=app;User ID=sa;Pwd=Hunter2Pass!",
        "DB_PASS=hunter2pass",
        "curl -u admin:S3cr3tPass https://api.example.com/v1",
        "Authorization: Bearer abc123def456ghi789jkl012mno345",
        "PIN: 4821",
        "пароль Tr0ub4dor&3",
        // Хэш и UUID под «секретным» именем.
        "SECRET_KEY_BASE=e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855",
        "HEROKU_API_KEY=550e8400-e29b-41d4-a716-446655440000",
        "JWT_SECRET=d41d8cd98f00b204e9800998ecf8427e",
        "TOTP_SECRET=JBSWY3DPEHPK3PXP",
        // Закрытые ключи PGP и PuTTY.
        "-----BEGIN PGP PRIVATE KEY BLOCK-----\nlQOYBF0123456\n-----END PGP PRIVATE KEY BLOCK-----",
        "PuTTY-User-Key-File-3: ssh-ed25519\nEncryption: none\nPrivate-Lines: 1\nAAAAIAbc",
        "admin:S3cr3tPass@192.168.1.10",
        // Закрытый ключ ETH/BTC целиком (и sha256 отдельно — так же).
        "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855",
        "0x4c0883a69102937d6231471b5dbb6204fe5129617082792ae468d01a3f362318",
        // Сид-фразы кошелька.
        "abandon ability able about above absent absorb abstract absurd abuse access accident",
        "legal winner thank year wave sausage worth useful legal winner thank yellow \
         legal winner thank year wave sausage worth useful legal winner thank yellow",
        // Номера карт: слитно, через пробел, через дефис, внутри текста.
        "4111111111111111",
        "4111 1111 1111 1111",
        "5555-5555-5555-4444",
        "378282246310005",
        "карта 2200 0000 0000 0004, срок 12/29",
        // Одноразовые коды.
        "482913",
        " 4829135 ",
        "48291357",
    ];

    const HARMLESS: &[&str] = &[
        "550e8400-e29b-41d4-a716-446655440000",
        "da39a3ee5e6b4b0d3255bfef95601890afd80709",
        // 64 hex внутри текста — хэш, а не ключ.
        "образ e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855 собран",
        "sha256:e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855",
        "d41d8cd98f00b204e9800998ecf8427e",
        "C:\\Users\\Erney\\Documents\\ClaudeCode\\Clipwire\\Cargo.toml",
        "/home/user/Projects/MyApp/src/main.rs",
        "src/lib/AppTag.svelte",
        "https://github.com/erneywhite/clipwire/releases/tag/v0.9.7",
        "https://example.com/search?q=Hello%20World&page=2",
        "Привет! Как дела? Встречаемся завтра в 10:30.",
        "«ИИ-инструменты»",
        "https://example.com/login",
        "https://example.com/auth/settings/profile",
        "https://www.ss.lv/msg/ru/electronics/computers/hubs-switch-routers/bjxkxc.html#photo-2",
        "https://aio.lv/ru/product--hp-enterprise-rp001231596--12651497",
        "https://docs.rs/iroh/latest/iroh/endpoint/struct.Endpoint.html#method.connect",
        "\"Shift+Enter\"",
        "(Windows11)",
        "The quick brown fox jumps over the lazy dog.",
        "fn main() { println!(\"Hello, world!\"); }",
        "let token = getToken();",
        "password: string;",
        "const password = process.env.DB_PASSWORD;",
        "data:image/png;base64,iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mNkYPhfDwAChwGA60e6kgAAAABJRU5ErkJggg==",
        "#FF00AA",
        "#ff00aa80",
        "rgba(255, 0, 0, 0.5)",
        "v0.9.7-beta.1",
        "1.2.3",
        "ivan.white@example.com",
        "README.md",
        "std::collections::HashMap",
        "console.log()",
        "2026-11-24T10:00:00Z",
        "ghcr.io/erneywhite/clipwire:0.9.7",
        "Cargo.toml",
        "NODE_ENV=production",
        "PORT=8080",
        "Секрет успеха — в простоте.",
        "max_tokens=4096",
        "hello world",
        "12345",
        "getUserById",
        "snake_case_name",
        "Hello, World!",
        "TODO: fix this later",
        "https://youtu.be/dQw4w9WgXcQ",
        "Error: ENOENT: no such file or directory",
        "SELECT * FROM users WHERE id = 1;",
        "npm install --save-dev typescript@5.6.3",
        "git commit -m \"Сайт: счётчики\"",
        "192.168.1.1:8080",
        "+371 2000 0000",
        "Clipwire-0.9.7-x64-setup.exe",
        "C:/Program Files/Clipwire/clipwire-desktop.exe",
        "@erneywhite",
        "Вставьте токен в поле ниже и нажмите «Сохранить».",
        "",
        // Имена продуктов, клавиши, кодировки, версии и код.
        "Windows11",
        "iPhone15",
        "COVID-19",
        "Monday-Friday",
        "Shift+Enter",
        "Ctrl+Shift+V",
        "WPA2-PSK",
        "ISO-8859-1",
        "ru_RU.UTF-8",
        "x86_64-pc-windows-msvc",
        "Rust2024",
        "Python3.12",
        "ruby-3.3.0",
        "Vec<Option<String>>",
        "items.map(x=>x*2)",
        "#FFAA00;",
        "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/120.0.0.0 Safari/537.36",
        "RUST_LOG=clipwire=debug,iroh=warn",
        "NODE_OPTIONS=--max-old-space-size=4096",
        "Note: Token=Value pairs are parsed",
        "Пароль: не меньше восьми символов",
        "This password is required",
        "pinned: true",
        "bypass: enabled",
        "KEYBOARD_LAYOUT=us-international",
        "git@github.com:erneywhite/clipwire.git",
        "mailto:ivan@example.com",
        "max_tokens: 4096",
        // Числа, похожие на карты и коды, но не они.
        "+1 (415) 555-2671",
        "+37120000000",
        "2026",
        "1999.99",
        "€ 1 299,00",
        "Заказ 12345",
        "123456789",
        "1764000000000",
        "4111 1111 1111 1112",
        "Код 4821 из 6 цифр: 48",
        "v1.4111111111111111",
        // Слова, похожие на сид-фразу.
        "abandon ability able about above absent absorb abstract absurd abuse access",
        "Abandon ability able about above absent absorb abstract absurd abuse access accident",
        "the quick brown fox jumps over the lazy dog and runs to the forest",
        // Упоминание кода Clipwire без самого кода.
        "Код приглашения не разобрался. Он начинается с clipwire1-.",
        "cwserver2-…",
    ];

    #[test]
    fn secrets_are_detected() {
        assert!(SECRETS.len() >= 25);
        for s in SECRETS {
            assert!(looks_secret(s), "должно быть секретом: {s}");
        }
    }

    #[test]
    fn harmless_text_is_not_secret() {
        assert!(HARMLESS.len() >= 25);
        for s in HARMLESS {
            assert!(!looks_secret(s), "не секрет: {s}");
        }
    }

    #[test]
    fn secret_inside_longer_text() {
        let text = format!(
            "{}\nOPENAI_API_KEY=sk-AbC123dEf456GhI789jKl012\n",
            "заметка ".repeat(80)
        );
        assert!(looks_secret(&text));
    }

    #[test]
    fn clipwire_codes_hidden_at_any_length() {
        // Хвост base32 длиннее 256 символов: общее правило длинных слов его
        // не видит, видит только правило кодов Clipwire.
        let tail = "abcdefghijklmnopqrstuvwxyz234567".repeat(10);
        for p in ["clipwire1-", "cwserver1-", "cwserver2-", "cwgroup1-"] {
            let code = format!("{p}{tail}");
            assert!(code.chars().count() > 256);
            assert!(!generic_token(&code), "{p}: проверка не по общему правилу");
            assert!(looks_secret(&code), "{p}");
            assert!(looks_secret(&code.to_uppercase()), "{p} прописными");
            // Код внутри фразы.
            let line = format!("Держи код: {code} — вставь в настройки.");
            assert!(looks_secret(&line), "{p} в тексте");
        }
        assert!(looks_secret(
            "вот приглашение clipwire1-mfrggzdfmztwq2lknnwg23tpobyxe43u, не теряй"
        ));
    }

    #[test]
    fn luhn_and_card_bounds() {
        assert!(luhn(&[7, 9, 9, 2, 7, 3, 9, 8, 7, 1, 3]));
        assert!(!luhn(&[7, 9, 9, 2, 7, 3, 9, 8, 7, 1, 4]));
        // Слишком длинная цепочка цифр — не карта, даже если сумма сходится.
        assert!(!card_number("41111111111111111111111"));
        // Дробная часть и цифры в слове не считаются.
        assert!(!card_number("3.141111111111111111"));
        assert!(!card_number("id4111111111111111"));
    }

    fn file(name: &str, is_dir: bool) -> FileRef {
        FileRef {
            name: name.into(),
            path: format!("C:/x/{name}"),
            size: 1,
            is_dir,
            width: 0,
            height: 0,
            modified_ms: 0,
            blob: None,
            blob_size: 0,
            p2p: false,
        }
    }

    fn card_of(h: &mut History, e: Entry) -> Card {
        let id = e.id;
        h.add(e).unwrap();
        h.row(id).unwrap().card
    }

    #[test]
    fn kinds_split_six_ways() {
        let mut h = History::in_memory();
        let me = DeviceId([1; 32]);
        let mut card = |c: Content| card_of(&mut h, local_entry(me, "ПК", c));
        assert_eq!(
            kinds_of(&card(Content::Text("просто текст".into()))),
            vec![AiKind::Text]
        );
        assert_eq!(
            kinds_of(&card(Content::Text("https://example.com/a".into()))),
            vec![AiKind::Link]
        );
        let rich = Content::Rich {
            text: "жирный".into(),
            html: Some("<b>жирный</b>".into()),
            rtf: None,
        };
        assert_eq!(kinds_of(&card(rich)), vec![AiKind::Rich]);
        let img = Content::Image {
            width: 1,
            height: 1,
            png: vec![1, 2, 3],
        };
        assert_eq!(kinds_of(&card(img)), vec![AiKind::Image]);
        // Файл-картинка — файл.
        let mut pic = file("a.png", false);
        pic.width = 10;
        pic.height = 10;
        assert_eq!(
            kinds_of(&card(Content::Files(vec![pic]))),
            vec![AiKind::File]
        );
        assert_eq!(
            kinds_of(&card(Content::Files(vec![file("dir", true)]))),
            vec![AiKind::Folder]
        );
        let mixed = card(Content::Files(vec![
            file("b.txt", false),
            file("dir2", true),
        ]));
        assert_eq!(kinds_of(&mixed), vec![AiKind::File, AiKind::Folder]);
        assert_eq!(ai_kind(&mixed), Some(AiKind::File));
        assert_eq!(
            content_kinds(&Content::Files(vec![file("b.txt", false), file("d", true)])),
            vec![AiKind::File, AiKind::Folder]
        );
    }

    #[test]
    fn policy_hides_by_kind_age_app_and_secret() {
        let mut h = History::in_memory();
        let me = DeviceId([1; 32]);
        let day = 86_400_000;
        let now = 100 * day;
        let mut ai = AiSettings {
            enabled: true,
            ..AiSettings::default()
        };
        let p = AiPolicy::new(&ai, &["KeePass.exe".into()], now);
        let text = card_of(
            &mut h,
            local_entry(me, "ПК", Content::Text("обычный текст".into())),
        );
        assert_eq!(p.check_card(now, &text, false), Ok(()));
        // Старше недели — не видно, но в закрепах окно не действует.
        assert_eq!(
            p.check_card(now - 8 * day, &text, false),
            Err(HiddenBy::Age)
        );
        assert_eq!(p.check_card(now - 8 * day, &text, true), Ok(()));
        // Папки по умолчанию выключены; смесь файлов и папок — только если
        // разрешено всё.
        let mixed = card_of(
            &mut h,
            local_entry(
                me,
                "ПК",
                Content::Files(vec![file("b.txt", false), file("d", true)]),
            ),
        );
        assert_eq!(p.check_card(now, &mixed, false), Err(HiddenBy::Kind));
        ai.kinds.files = true;
        let p2 = AiPolicy::new(&ai, &[], now);
        assert_eq!(p2.check_card(now, &mixed, false), Err(HiddenBy::Kind));
        ai.kinds.folders = true;
        let p3 = AiPolicy::new(&ai, &[], now);
        assert_eq!(p3.check_card(now, &mixed, false), Ok(()));
        // Программа-исключение — по exe и по имени.
        let mut from_keepass = local_entry(me, "ПК", Content::Text("что-то".into()));
        from_keepass.set_source_app(&SourceApp {
            name: "KeePass".into(),
            exe: "KeePass.exe".into(),
            icon_png: Vec::new(),
        });
        let kp = card_of(&mut h, from_keepass);
        assert_eq!(p.check_card(now, &kp, false), Err(HiddenBy::App));
        // Секрет.
        let sec = card_of(
            &mut h,
            local_entry(
                me,
                "ПК",
                Content::Text(concat!("gh", "p_aB3dE5fG7hI9jK1lM3nO5pQ7rS9tU1vW3xY5").into()),
            ),
        );
        assert_eq!(p.check_card(now, &sec, false), Err(HiddenBy::Secret));
        ai.hide_secrets = false;
        let open = AiPolicy::new(&ai, &[], now);
        assert_eq!(open.check_card(now, &sec, false), Ok(()));
        // Секрет глубже карточки ловит проверка полного текста.
        let long = format!("{}\npassword=hunter22", "а".repeat(9000));
        assert_eq!(p.check_content(&Content::Text(long)), Err(HiddenBy::Secret));
        // История целиком: окна нет.
        ai.history_days = 0;
        let all = AiPolicy::new(&ai, &[], now);
        assert_eq!(all.check_card(0, &text, false), Ok(()));
    }

    fn on_policy() -> AiPolicy {
        let ai = AiSettings {
            enabled: true,
            history_days: 0,
            ..AiSettings::default()
        };
        AiPolicy::new(&ai, &[], 0)
    }

    /// Секрет после первых `HEAD_CHARS` символов: поиск по истории не
    /// выдаёт запись (иначе по подстроке подбирали бы скрытый ключ), как и
    /// `Get`.
    #[test]
    fn search_hides_secret_after_head() {
        let p = on_policy();
        let mut h = History::in_memory();
        let me = DeviceId([1; 32]);
        let pad = "заметка ".repeat(60);
        for key in [
            concat!("AK", "IAIOSFODNN7EXAMPLE"),
            "STRIPE_KEY=Xk9fL2mQp7RtZ4vB8nW1",
        ] {
            let e = local_entry(me, "ПК", Content::Text(format!("{pad}\n{key}\n")));
            let id = e.id;
            h.add(e).unwrap();
            let row = h.row(id).unwrap();
            let body = || h.get(id).map(|e| e.content);
            assert_eq!(
                p.check_row(0, &row.card, false, body),
                Err(HiddenBy::Secret),
                "{key}"
            );
            // Тело не прочиталось — тоже скрыто.
            assert_eq!(
                p.check_row(0, &row.card, false, || None),
                Err(HiddenBy::Secret)
            );
        }
        let plain = local_entry(me, "ПК", Content::Text(format!("{pad} конец")));
        h.add(plain).unwrap();
        // Обход с предикатом из политики: по «akia» запись не находится,
        // обычная длинная заметка видна.
        let mut pred = |id: ItemId, seen: i64, _: bool, card: &Card| {
            p.check_row(seen, card, false, || h.get(id).map(|e| e.content))
                .is_ok()
        };
        let (rows, _) = h
            .scan_visible("akiaiosfodnn7", false, 0, 10, None, &|| false, &mut pred)
            .unwrap();
        assert!(rows.is_empty());
        let (rows, _) = h
            .scan_visible("заметка", false, 0, 10, None, &|| false, &mut pred)
            .unwrap();
        assert_eq!(rows.len(), 1);
        assert!(rows[0].card.head.ends_with("заметка "));
    }

    /// Форматированный текст без простого текста: ИИ получит текст из HTML,
    /// по нему и проверка.
    #[test]
    fn rich_without_text_is_checked_by_html() {
        let p = on_policy();
        let secret = Content::Rich {
            text: String::new(),
            html: Some("<table><tr><td>Tr0ub4dor&amp;3</td></tr></table>".into()),
            rtf: None,
        };
        assert_eq!(ai_text(&secret), Some(("Tr0ub4dor&3".to_string(), true)));
        assert_eq!(p.check_content(&secret), Err(HiddenBy::Secret));
        let mut h = History::in_memory();
        let e = local_entry(DeviceId([1; 32]), "ПК", secret);
        let id = e.id;
        h.add(e).unwrap();
        let card = h.row(id).unwrap().card;
        assert!(p.needs_body(&card));
        assert_eq!(
            p.check_row(0, &card, false, || h.get(id).map(|e| e.content)),
            Err(HiddenBy::Secret)
        );
        let fine = Content::Rich {
            text: "жирный".into(),
            html: Some("<b>жирный</b>".into()),
            rtf: None,
        };
        assert_eq!(ai_text(&fine), Some(("жирный".to_string(), false)));
        assert_eq!(p.check_content(&fine), Ok(()));
    }

    /// Подпись закрепа с паролем прячет запись: подпись видна в списках.
    #[test]
    fn secret_in_pin_note_hides_entry() {
        let p = on_policy();
        let mut h = History::in_memory();
        let e = local_entry(DeviceId([1; 32]), "ПК", Content::Text("роутер".into()));
        let id = e.id;
        h.add(e).unwrap();
        h.set_pinned(id, true).unwrap();
        h.set_note(id, "дача").unwrap();
        assert_eq!(p.check_card(0, &h.row(id).unwrap().card, true), Ok(()));
        h.set_note(id, "пароль Tr0ub4dor&3").unwrap();
        assert_eq!(
            p.check_card(0, &h.row(id).unwrap().card, true),
            Err(HiddenBy::Secret)
        );
    }

    #[test]
    fn ignored_app_by_bundle_or_desktop_id() {
        let list = |s: &str| vec![s.to_string()];
        assert!(app_ignored(&list("KeePass.exe"), "KeePass.exe", "KeePass"));
        assert!(app_ignored(
            &list("org.keepassxc.KeePassXC"),
            "keepassxc",
            "KeePassXC"
        ));
        assert!(app_ignored(
            &list("com.1password.1password"),
            "1Password",
            "1Password"
        ));
        assert!(app_ignored(
            &list("org.keepassxc.KeePassXC.desktop"),
            "keepassxc",
            "KeePassXC"
        ));
        assert!(!app_ignored(
            &list("org.keepassxc.KeePassXC"),
            "Telegram.exe",
            "Telegram"
        ));
        assert!(!app_ignored(&list("notepad"), "Telegram.exe", "Telegram"));
    }

    /// Файлы под правилом «не отправлять» не видны ИИ ни по карточке, ни по
    /// содержимому. Правило с особым расширением: другие тесты, идущие
    /// параллельно, таких файлов не копируют.
    #[test]
    fn exclusion_rules_hide_files() {
        use crate::exclusions::{self, Action, Rule, What};
        exclusions::set(&[Rule {
            enabled: true,
            what: What::Ext,
            value: "aiexcltest".into(),
            size: 0,
            action: Action::NoSend,
        }]);
        let mut ai = AiSettings {
            enabled: true,
            history_days: 0,
            ..AiSettings::default()
        };
        ai.kinds.files = true;
        let p = AiPolicy::new(&ai, &[], 0);
        let mut h = History::in_memory();
        let me = DeviceId([1; 32]);
        let hidden = Content::Files(vec![file("ключи.aiexcltest", false)]);
        let open = Content::Files(vec![file("отчёт.pdf", false)]);
        let hc = card_of(&mut h, local_entry(me, "ПК", hidden.clone()));
        let oc = card_of(&mut h, local_entry(me, "ПК", open.clone()));
        let res = (
            p.check_card(0, &hc, false),
            p.check_content(&hidden),
            p.check_card(0, &oc, false),
            p.check_content(&open),
        );
        exclusions::set(&[]);
        assert_eq!(
            res,
            (
                Err(HiddenBy::Exclusion),
                Err(HiddenBy::Exclusion),
                Ok(()),
                Ok(())
            )
        );
    }

    #[test]
    fn summary_carries_ai_origin_and_preview() {
        let mut h = History::in_memory();
        let mut e = local_entry(
            DeviceId([1; 32]),
            "ПК",
            Content::Text(format!("строка\n{}", "х".repeat(500))),
        );
        e.set_ai_origin(&clipwire_core::AiOrigin {
            client: "Cursor".into(),
        });
        let id = e.id;
        h.add(e).unwrap();
        let s = summary(&h.row(id).unwrap());
        assert_eq!(s.by_ai.as_deref(), Some("Cursor"));
        assert_eq!(s.kind, AiKind::Text);
        assert_eq!(s.chars, 507);
        assert!(s.preview.chars().count() <= PREVIEW_CHARS);
        assert!(!s.preview.contains('\n'));
        assert_eq!(s.id, id.0.to_string());
    }
}
