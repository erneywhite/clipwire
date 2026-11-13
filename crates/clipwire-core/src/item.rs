//! Элемент буфера обмена в открытом виде. До сервера он доходит только
//! зашифрованным (см. [`crate::crypto::SealedItem`]).

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use uuid::Uuid;

/// Идентификатор элемента. UUID v7 сортируется по времени создания,
/// поэтому им же упорядочивается история.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct ItemId(pub Uuid);

impl ItemId {
    pub fn new() -> Self {
        Self(Uuid::now_v7())
    }

    /// Id с заданным временем: для элементов истории, скопированных раньше,
    /// чтобы на сервере и у других устройств они встали на своё место по времени.
    pub fn at_ms(ms: u64) -> Self {
        let ts = uuid::Timestamp::from_unix(
            uuid::NoContext,
            ms / 1000,
            ((ms % 1000) * 1_000_000) as u32,
        );
        Self(Uuid::new_v7(ts))
    }

    /// Время из UUID v7, мс Unix.
    pub fn ms(&self) -> Option<u64> {
        let (s, ns) = self.0.get_timestamp()?.to_unix();
        Some(s * 1000 + u64::from(ns) / 1_000_000)
    }

    pub fn as_bytes(&self) -> &[u8; 16] {
        self.0.as_bytes()
    }
}

impl Default for ItemId {
    fn default() -> Self {
        Self::new()
    }
}

/// Устройство-источник: публичный ключ iroh-эндпоинта (Ed25519, 32 байта).
/// Хранится как массив, чтобы core не зависел от iroh.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct DeviceId(pub [u8; 32]);

/// Содержимое элемента. Файлы добавятся на этапе файлов.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub enum Content {
    Text(String),
    /// Картинка в PNG (без потерь) и её размеры в пикселях.
    Image {
        width: u32,
        height: u32,
        png: Vec<u8>,
    },
    /// Текст с форматированием: из браузера, Word, почты. `text` — тот же
    /// текст без форматирования, он вставляется по Shift+Enter и в программы,
    /// которые форматирование не понимают. `html` — фрагмент из CF_HTML,
    /// `rtf` — байты RTF как есть (там бывает 8-битная кодировка).
    Rich {
        text: String,
        html: Option<String>,
        rtf: Option<Vec<u8>>,
    },
    /// Скопированные файлы и папки. Файл с `blob` передаётся через хранилище
    /// сервера (см. `docs/files-transfer.md`), без него — только ссылка на путь
    /// устройства-источника.
    Files(Vec<FileRef>),
}

/// Файл или папка из буфера.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileRef {
    pub name: String,
    /// Полный путь на устройстве-источнике.
    pub path: String,
    /// Байты; у папки 0.
    pub size: u64,
    pub is_dir: bool,
    /// Файл-картинка: размеры в пикселях, иначе 0.
    pub width: u32,
    pub height: u32,
    /// Время изменения файла (мс Unix): загрузчик сверяет его, чтобы не
    /// отправить файл, изменившийся после копирования.
    pub modified_ms: u64,
    /// Как файл передаётся. `None` — никак (локальный режим или передача
    /// отключена): есть только путь на устройстве-источнике. Иначе — id
    /// потока частей: через хранилище сервера или напрямую (`p2p`).
    pub blob: Option<crate::blob::BlobId>,
    /// Размер передаваемого потока (открытый текст): у файла равен `size`,
    /// у папки — размер её архива (tar без сжатия). 0, если `blob` нет.
    pub blob_size: u64,
    /// Файл больше порога: на сервер не загружается, его берут напрямую с
    /// устройства-источника по `clipwire-p2p/1`, пока оно в сети.
    pub p2p: bool,
}

/// Форматы прошлых версий: история на диске хранит элементы в postcard, а он
/// не умеет пропускать недостающие поля. Чтобы записи пережили обновление,
/// история пробует прочитать запись в прежнем виде и переводит её в новый.
pub mod legacy {
    use serde::Deserialize;

    use super::{Content, FileRef};

    /// `FileRef` до полей `blob_size` и `p2p` (передача через сервер, ноябрь 2026).
    #[derive(Deserialize)]
    pub struct FileRefV2 {
        pub name: String,
        pub path: String,
        pub size: u64,
        pub is_dir: bool,
        pub width: u32,
        pub height: u32,
        pub modified_ms: u64,
        pub blob: Option<crate::blob::BlobId>,
    }

    /// `Content` с `FileRefV2`. Порядок вариантов совпадает с `Content`.
    #[derive(Deserialize)]
    pub enum ContentV2 {
        Text(String),
        Image {
            width: u32,
            height: u32,
            png: Vec<u8>,
        },
        Rich {
            text: String,
            html: Option<String>,
            rtf: Option<Vec<u8>>,
        },
        Files(Vec<FileRefV2>),
    }

    impl From<ContentV2> for Content {
        fn from(c: ContentV2) -> Self {
            match c {
                ContentV2::Text(t) => Content::Text(t),
                ContentV2::Image { width, height, png } => Content::Image { width, height, png },
                ContentV2::Rich { text, html, rtf } => Content::Rich { text, html, rtf },
                ContentV2::Files(f) => Content::Files(
                    f.into_iter()
                        .map(|f| FileRef {
                            blob_size: if f.blob.is_some() { f.size } else { 0 },
                            name: f.name,
                            path: f.path,
                            size: f.size,
                            is_dir: f.is_dir,
                            width: f.width,
                            height: f.height,
                            modified_ms: f.modified_ms,
                            blob: f.blob,
                            p2p: false,
                        })
                        .collect(),
                ),
            }
        }
    }
}

// Картинку не печатаем байтами в отладочный вывод: это мегабайты в логе.
impl std::fmt::Debug for Content {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Content::Text(s) => f.debug_tuple("Text").field(s).finish(),
            Content::Image { width, height, png } => {
                write!(f, "Image({width}×{height}, {} байт PNG)", png.len())
            }
            Content::Rich { text, html, rtf } => write!(
                f,
                "Rich({text:?}, html {} байт, rtf {} байт)",
                html.as_ref().map_or(0, String::len),
                rtf.as_ref().map_or(0, Vec::len)
            ),
            Content::Files(files) => f.debug_tuple("Files").field(files).finish(),
        }
    }
}

impl Content {
    /// Хэш содержимого для защиты от петель: агент запоминает хэш того, что
    /// сам записал в буфер, и не отправляет это изменение обратно.
    /// В хэш входит тип, чтобы текст и, например, путь к файлу с теми же
    /// байтами не совпали. Форматированный текст хэшируется по самому тексту:
    /// это то же самое копирование, а HTML программы при обратном чтении
    /// иногда переписывают — защита от петель на нём бы промахивалась.
    pub fn hash(&self) -> [u8; 32] {
        let mut h = Sha256::new();
        match self {
            Content::Text(s) | Content::Rich { text: s, .. } => {
                h.update(b"text\0");
                h.update(s.as_bytes());
            }
            Content::Image { width, height, png } => {
                h.update(b"image\0");
                h.update(width.to_le_bytes());
                h.update(height.to_le_bytes());
                h.update(png);
            }
            Content::Files(files) => {
                h.update(b"files\0");
                for f in files {
                    h.update(f.path.as_bytes());
                    h.update(b"\0");
                    h.update(f.size.to_le_bytes());
                }
            }
        }
        h.finalize().into()
    }

    /// Размер полезной нагрузки в байтах, для проверки лимитов до шифрования.
    pub fn len(&self) -> usize {
        match self {
            Content::Text(s) => s.len(),
            Content::Image { png, .. } => png.len(),
            Content::Rich { text, html, rtf } => {
                text.len() + html.as_ref().map_or(0, String::len) + rtf.as_ref().map_or(0, Vec::len)
            }
            // Сам элемент — только имена и пути; размер файлов тут ни при чём.
            Content::Files(files) => files.iter().map(|f| f.name.len() + f.path.len() + 24).sum(),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Текст элемента (обычный или форматированный), если он текстовый.
    pub fn text(&self) -> Option<&str> {
        match self {
            Content::Text(t) | Content::Rich { text: t, .. } => Some(t),
            Content::Image { .. } | Content::Files(_) => None,
        }
    }

    /// Без форматирования: форматированный текст становится обычным.
    pub fn plain(self) -> Content {
        match self {
            Content::Rich { text, .. } => Content::Text(text),
            other => other,
        }
    }

    /// Ужать до `max` байт, жертвуя форматированием: сначала RTF (у Word он
    /// раздут таблицами шрифтов), потом HTML. Текст и картинки не трогает.
    pub fn fit(self, max: usize) -> Content {
        if self.len() <= max {
            return self;
        }
        match self {
            Content::Rich {
                text,
                html,
                rtf: Some(_),
            } => Content::Rich {
                text,
                html,
                rtf: None,
            }
            .fit(max),
            Content::Rich { text, .. } => Content::Text(text),
            other => other,
        }
    }
}

/// Элемент буфера целиком. Время — миллисекунды Unix-эпохи на устройстве-источнике.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Item {
    pub id: ItemId,
    pub origin: DeviceId,
    /// Человекочитаемое имя устройства-источника («MacBook», «Рабочий ПК»).
    /// Лежит внутри шифротекста, сервер его не видит.
    pub device_name: String,
    pub created_at_ms: u64,
    pub content: Content,
    /// Дополнительные сведения по меткам ([`Ext`]). Новое добавляется сюда
    /// новой меткой, формат элемента при этом не меняется: версии, не знающие
    /// метку, её пропускают.
    pub ext: Vec<Ext>,
}

/// Дополнительное сведение элемента: метка и байты (обычно postcard).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Ext {
    pub tag: u16,
    pub data: Vec<u8>,
}

/// Метки [`Ext`].
pub mod ext {
    /// Приложение, из которого скопировано: [`super::SourceApp`].
    pub const SOURCE_APP: u16 = 1;
}

/// Из какого приложения скопировано.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SourceApp {
    /// Понятное имя («Telegram»), из описания exe; если нет — имя файла.
    pub name: String,
    /// Имя exe («Telegram.exe»): по нему список исключений и группировка.
    pub exe: String,
    /// Маленькая иконка (PNG 32×32, до ~8 КБ); пусто, если не удалось.
    pub icon_png: Vec<u8>,
}

/// Поиск и запись сведений по метке — для `Item` и записей истории.
pub trait ExtList {
    fn ext_list(&self) -> &[Ext];
    fn ext_list_mut(&mut self) -> &mut Vec<Ext>;

    fn ext_get<T: serde::de::DeserializeOwned>(&self, tag: u16) -> Option<T> {
        let e = self.ext_list().iter().find(|e| e.tag == tag)?;
        postcard::from_bytes(&e.data).ok()
    }

    fn ext_set<T: Serialize>(&mut self, tag: u16, value: &T) {
        let Ok(data) = postcard::to_stdvec(value) else {
            return;
        };
        let list = self.ext_list_mut();
        list.retain(|e| e.tag != tag);
        list.push(Ext { tag, data });
    }
}

impl ExtList for Item {
    fn ext_list(&self) -> &[Ext] {
        &self.ext
    }
    fn ext_list_mut(&mut self) -> &mut Vec<Ext> {
        &mut self.ext
    }
}

impl Item {
    /// Приложение-источник, если известно.
    pub fn source_app(&self) -> Option<SourceApp> {
        self.ext_get(ext::SOURCE_APP)
    }

    pub fn new(origin: DeviceId, content: Content) -> Self {
        let created_at_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0);
        Self {
            id: ItemId::new(),
            origin,
            device_name: String::new(),
            created_at_ms,
            content,
            ext: Vec::new(),
        }
    }

    pub fn with_device_name(mut self, name: impl Into<String>) -> Self {
        self.device_name = name.into();
        self
    }

    /// Элемент, скопированный раньше (`ms` — когда), например из локальной
    /// истории при подключении к группе.
    pub fn backdated(origin: DeviceId, content: Content, ms: u64) -> Self {
        Self {
            id: ItemId::at_ms(ms),
            origin,
            device_name: String::new(),
            created_at_ms: ms,
            content,
            ext: Vec::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_are_time_ordered() {
        let a = ItemId::new();
        let b = ItemId::new();
        assert!(a < b);
    }

    #[test]
    fn backdated_ids_keep_time_and_order() {
        let ms = 1_763_000_000_123;
        let old = ItemId::at_ms(ms);
        assert_eq!(old.ms(), Some(ms));
        assert!(old < ItemId::new());
        assert!(ItemId::at_ms(ms - 1) < old);
    }

    #[test]
    fn image_hash_differs_by_size_and_debug_is_short() {
        let a = Content::Image {
            width: 2,
            height: 1,
            png: vec![1, 2, 3],
        };
        let b = Content::Image {
            width: 1,
            height: 2,
            png: vec![1, 2, 3],
        };
        assert_ne!(a.hash(), b.hash());
        assert_eq!(format!("{a:?}"), "Image(2×1, 3 байт PNG)");
    }

    #[test]
    fn rich_hashes_as_its_text_and_sheds_formatting_to_fit() {
        let rich = Content::Rich {
            text: "привет".into(),
            html: Some("<b>привет</b>".into()),
            rtf: Some(vec![b'x'; 100]),
        };
        assert_eq!(rich.hash(), Content::Text("привет".into()).hash());
        assert_eq!(rich.text(), Some("привет"));
        // Влезает целиком — не трогаем.
        assert_eq!(rich.clone().fit(10_000), rich);
        // Без RTF влезает — остаётся HTML.
        let no_rtf = rich.clone().fit(40);
        assert!(matches!(
            &no_rtf,
            Content::Rich {
                html: Some(_),
                rtf: None,
                ..
            }
        ));
        // Не влезает и HTML — остаётся текст.
        assert_eq!(rich.clone().fit(15), Content::Text("привет".into()));
        assert_eq!(rich.plain(), Content::Text("привет".into()));
    }

    #[test]
    fn hash_depends_on_content() {
        let a = Content::Text("пароль".into());
        let b = Content::Text("пароль".into());
        let c = Content::Text("пароль!".into());
        assert_eq!(a.hash(), b.hash());
        assert_ne!(a.hash(), c.hash());
    }
}
