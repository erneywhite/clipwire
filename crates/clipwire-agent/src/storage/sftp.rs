//! Хранилище на SFTP-сервере (`russh` + `russh-sftp`).
//!
//! Вход по паролю или закрытому ключу OpenSSH (с паролем к ключу или без).
//! Ключ хоста сверяется с отпечатком SHA-256 из настроек; другой ключ —
//! отказ ([`StoreError::HostKey`]), без «всё равно продолжить». Без
//! подтверждённого отпечатка ([`SftpConfig::host_key_sha256`] пуст) вход не
//! выполняется вовсе: пароль неизвестному хосту не отправляется, а
//! отпечаток возвращается в ошибке, чтобы интерфейс показал его человеку.
//!
//! Одно SSH-соединение на хранилище, оно переоткрывается после обрыва.
//! Запись — во временный файл и переименование: читающий не увидит
//! полуобъекта. Папки создаются по мере надобности.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use clipwire_core::storage::{SftpAuth, SftpConfig};
use russh::client;
use russh::keys::{HashAlg, PrivateKeyWithHashAlg, PublicKeyOrCertificate};
use russh_sftp::client::error::Error as SftpError;
use russh_sftp::client::{RawSftpSession, SftpSession};
use russh_sftp::protocol::{FileAttributes, Name, StatusCode};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use super::{ObjectInfo, ObjectStore, StoreError, StoreFuture, check_name, join_prefix};

const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);
/// Тайм-аут одного запроса SFTP, секунды.
const OP_TIMEOUT_SECS: u64 = 30;

/// Привести отпечаток к виду `SHA256:<base64 без =>` для сравнения.
pub fn normalize_fingerprint(s: &str) -> String {
    let s = s.trim();
    let body = s
        .strip_prefix("SHA256:")
        .or_else(|| s.strip_prefix("sha256:"))
        .unwrap_or(s);
    format!("SHA256:{}", body.trim_end_matches('='))
}

struct HostCheck {
    expected: String,
    /// Какой отпечаток прислал сервер.
    seen: Arc<Mutex<Option<String>>>,
}

impl client::Handler for HostCheck {
    type Error = russh::Error;

    async fn check_server_key(
        &mut self,
        key: &PublicKeyOrCertificate,
    ) -> Result<bool, Self::Error> {
        let fp = match key {
            PublicKeyOrCertificate::PublicKey { key, .. } => key.fingerprint(HashAlg::Sha256),
            PublicKeyOrCertificate::Certificate(c) => c.public_key().fingerprint(HashAlg::Sha256),
        }
        .to_string();
        let fp = normalize_fingerprint(&fp);
        *self.seen.lock().unwrap_or_else(|e| e.into_inner()) = Some(fp.clone());
        Ok(!self.expected.trim().is_empty() && normalize_fingerprint(&self.expected) == fp)
    }
}

struct Conn {
    // Держим соединение, пока жива сессия SFTP.
    _handle: client::Handle<HostCheck>,
    link: Link,
}

/// Две сессии SFTP на одном SSH-соединении. Высокоуровневая — для файлов;
/// низкоуровневая — для чтения папок: `SftpSession::read_dir` копит ответы
/// READDIR до EOF без предела, а здесь записи считаются после каждого ответа.
/// Сервер с `MaxSessions 1` второй канал не даёт: тогда `raw` нет и папки
/// читаются высокоуровневой сессией.
#[derive(Clone)]
struct Link {
    sftp: Arc<SftpSession>,
    raw: Option<Arc<RawSftpSession>>,
}

pub struct SftpStore {
    cfg: SftpConfig,
    conn: tokio::sync::Mutex<Option<Conn>>,
}

fn sftp_err(e: SftpError) -> StoreError {
    match e {
        SftpError::Status(s) => match s.status_code {
            StatusCode::PermissionDenied => StoreError::Forbidden(s.error_message),
            StatusCode::NoSuchFile => StoreError::NoBucket(s.error_message),
            StatusCode::NoConnection | StatusCode::ConnectionLost => {
                StoreError::Network(s.error_message)
            }
            other => StoreError::Other(format!("{other}: {}", s.error_message)),
        },
        SftpError::Timeout => StoreError::Timeout,
        SftpError::IO(m) => StoreError::Network(m),
        other => StoreError::Other(other.to_string()),
    }
}

fn is_missing(e: &SftpError) -> bool {
    matches!(e, SftpError::Status(s) if s.status_code == StatusCode::NoSuchFile)
}

/// Ответил ли сервер отказом на операцию (а не пропала связь). Только после
/// такого ответа `rename` поверх файла стоит повторять через удаление: при
/// тайм-ауте первый `rename` мог пройти, и удаление снесло бы новый объект.
fn is_refusal(e: &SftpError) -> bool {
    matches!(e, SftpError::Status(s) if !matches!(
        s.status_code,
        StatusCode::NoConnection | StatusCode::ConnectionLost
    ))
}

impl SftpStore {
    pub fn new(cfg: SftpConfig) -> Result<Self, StoreError> {
        if cfg.host.trim().is_empty() {
            return Err(StoreError::Config("не указан сервер".into()));
        }
        if cfg.user.trim().is_empty() {
            return Err(StoreError::Config("не указан пользователь".into()));
        }
        if let SftpAuth::Key {
            private_key,
            passphrase,
        } = &cfg.auth
        {
            russh::keys::decode_secret_key(private_key, passphrase.as_deref())
                .map_err(|e| StoreError::Auth(format!("закрытый ключ не прочитался: {e}")))?;
        }
        Ok(Self {
            cfg,
            conn: tokio::sync::Mutex::new(None),
        })
    }

    fn path(&self, name: &str) -> Result<String, StoreError> {
        check_name(name)?;
        let root = self.cfg.path.trim_end_matches('/');
        Ok(if root.is_empty() && self.cfg.path.starts_with('/') {
            format!("/{name}")
        } else if root.is_empty() {
            name.to_string()
        } else {
            join_prefix_keep_root(root, name)
        })
    }

    /// Подключиться, проверить ключ хоста и войти. Возвращает и отпечаток
    /// хоста, если сервер успел его прислать.
    async fn connect(&self) -> Result<Conn, StoreError> {
        let seen = Arc::new(Mutex::new(None));
        let handler = HostCheck {
            expected: self.cfg.host_key_sha256.clone(),
            seen: seen.clone(),
        };
        let config = Arc::new(client::Config {
            inactivity_timeout: Some(Duration::from_secs(120)),
            keepalive_interval: Some(Duration::from_secs(30)),
            ..Default::default()
        });
        let addr = (self.cfg.host.trim().to_string(), self.cfg.port);
        let res = tokio::time::timeout(CONNECT_TIMEOUT, client::connect(config, addr, handler))
            .await
            .map_err(|_| StoreError::Timeout)?;
        let mut handle = match res {
            Ok(h) => h,
            Err(e) => {
                let got = seen.lock().unwrap_or_else(|e| e.into_inner()).clone();
                return Err(match (e, got) {
                    (russh::Error::UnknownKey, Some(got)) => StoreError::HostKey {
                        expected: self.cfg.host_key_sha256.trim().to_string(),
                        got,
                    },
                    (russh::Error::IO(e), _) => StoreError::Network(e.to_string()),
                    (russh::Error::ConnectionTimeout | russh::Error::KeepaliveTimeout, _) => {
                        StoreError::Timeout
                    }
                    (e, _) => StoreError::Network(e.to_string()),
                });
            }
        };
        let auth = tokio::time::timeout(CONNECT_TIMEOUT, async {
            match &self.cfg.auth {
                SftpAuth::Password(p) => handle.authenticate_password(&self.cfg.user, p).await,
                SftpAuth::Key {
                    private_key,
                    passphrase,
                } => {
                    let key = russh::keys::decode_secret_key(private_key, passphrase.as_deref())
                        .map_err(russh::Error::Keys)?;
                    let hash = handle
                        .best_supported_rsa_hash()
                        .await
                        .ok()
                        .flatten()
                        .flatten();
                    handle
                        .authenticate_publickey(
                            &self.cfg.user,
                            PrivateKeyWithHashAlg::new(Arc::new(key), hash),
                        )
                        .await
                }
            }
        })
        .await
        .map_err(|_| StoreError::Timeout)?
        .map_err(|e| StoreError::Network(e.to_string()))?;
        if !auth.success() {
            return Err(StoreError::Auth(format!(
                "сервер не принял вход пользователя «{}»",
                self.cfg.user
            )));
        }
        let sftp = SftpSession::new(open_sftp_channel(&handle).await?)
            .await
            .map_err(|e| StoreError::Other(format!("сервер не дал SFTP: {e}")))?;
        sftp.set_timeout(OP_TIMEOUT_SECS);
        // shortcut: без второго канала (MaxSessions 1) чтение папки не
        // ограничено по ходу, предел проверяется после ответа сервера; переделать,
        // если russh-sftp даст READDIR поверх уже открытой сессии.
        let raw = match open_sftp_channel(&handle).await {
            Ok(ch) => {
                let raw = RawSftpSession::new(ch);
                raw.set_timeout(OP_TIMEOUT_SECS);
                match raw.init().await {
                    Ok(_) => Some(Arc::new(raw)),
                    Err(e) => {
                        tracing::warn!(
                            "второй канал SFTP не поднялся ({e}), папки читаются основным"
                        );
                        None
                    }
                }
            }
            Err(e) => {
                tracing::warn!("сервер не дал второй канал SFTP ({e}), папки читаются основным");
                None
            }
        };
        Ok(Conn {
            _handle: handle,
            link: Link {
                sftp: Arc::new(sftp),
                raw,
            },
        })
    }

    async fn session(&self) -> Result<Link, StoreError> {
        let mut g = self.conn.lock().await;
        if let Some(c) = g.as_ref()
            && !c._handle.is_closed()
        {
            return Ok(c.link.clone());
        }
        let c = self.connect().await?;
        let s = c.link.clone();
        *g = Some(c);
        Ok(s)
    }

    /// Выполнить операцию; после сетевой ошибки соединение сбрасывается,
    /// следующая операция подключится заново.
    async fn run<T, F, Fut>(&self, f: F) -> Result<T, StoreError>
    where
        F: FnOnce(Arc<SftpSession>) -> Fut,
        Fut: std::future::Future<Output = Result<T, StoreError>>,
    {
        self.run_link(|l| f(l.sftp)).await
    }

    /// Как [`Self::run`], но с обеими сессиями (для обхода папок).
    async fn run_link<T, F, Fut>(&self, f: F) -> Result<T, StoreError>
    where
        F: FnOnce(Link) -> Fut,
        Fut: std::future::Future<Output = Result<T, StoreError>>,
    {
        let s = self.session().await?;
        let r = f(s).await;
        if r.as_ref().is_err_and(StoreError::retryable) {
            *self.conn.lock().await = None;
        }
        r
    }

    /// Отпечаток ключа хоста, без входа (для первой настройки).
    pub async fn host_fingerprint(&self) -> Result<String, StoreError> {
        let probe = Self {
            cfg: SftpConfig {
                host_key_sha256: String::new(),
                ..self.cfg.clone()
            },
            conn: tokio::sync::Mutex::new(None),
        };
        match probe.connect().await {
            Err(StoreError::HostKey { got, .. }) => Ok(got),
            Err(e) => Err(e),
            Ok(_) => Err(StoreError::Other("сервер не прислал ключ хоста".into())),
        }
    }
}

async fn open_sftp_channel(
    handle: &client::Handle<HostCheck>,
) -> Result<russh::ChannelStream<client::Msg>, StoreError> {
    let channel = handle
        .channel_open_session()
        .await
        .map_err(|e| StoreError::Network(e.to_string()))?;
    channel
        .request_subsystem(true, "sftp")
        .await
        .map_err(|e| StoreError::Network(e.to_string()))?;
    Ok(channel.into_stream())
}

/// Записи одной папки (без `.` и `..`): через второй канал с пределом по ходу
/// чтения, а без него — через основную сессию с проверкой предела после.
async fn read_dir_any(
    l: &Link,
    path: &str,
    seen: &mut usize,
    dir: &str,
) -> Result<Option<Vec<(String, FileAttributes)>>, StoreError> {
    if let Some(raw) = &l.raw {
        return read_dir_capped(raw, path, seen, dir).await;
    }
    let rd = match l.sftp.read_dir(path.to_string()).await {
        Ok(rd) => rd,
        Err(e) if is_missing(&e) => return Ok(None),
        Err(e) => return Err(sftp_err(e)),
    };
    let mut out = Vec::new();
    for e in rd {
        let n = e.file_name();
        if n == "." || n == ".." {
            continue;
        }
        *seen += 1;
        if *seen > super::MAX_LIST {
            return Err(StoreError::Corrupt(format!(
                "в папке {dir} больше {} записей",
                super::MAX_LIST
            )));
        }
        out.push((n, e.metadata()));
    }
    Ok(Some(out))
}

/// Записи одной папки (без `.` и `..`) через OPENDIR/READDIR/CLOSE.
/// `seen` — сколько записей набрано за весь обход; после каждого ответа
/// READDIR он сверяется с [`super::MAX_LIST`], так что сервер, который
/// никогда не отвечает EOF, получит отказ, а не съест память. `None` —
/// папки нет.
async fn read_dir_capped(
    raw: &RawSftpSession,
    path: &str,
    seen: &mut usize,
    dir: &str,
) -> Result<Option<Vec<(String, FileAttributes)>>, StoreError> {
    let handle = match raw.opendir(path.to_string()).await {
        Ok(h) => h.handle,
        Err(e) if is_missing(&e) => return Ok(None),
        Err(e) => return Err(sftp_err(e)),
    };
    let r = collect_capped(|| raw.readdir(handle.clone()), seen, dir).await;
    // Закрыть хэндл и после отказа, иначе сервер держит его до обрыва.
    let closed = raw.close(handle).await;
    let out = r?;
    closed.map_err(sftp_err)?;
    Ok(Some(out))
}

/// Читать ответы READDIR до EOF, проверяя предел после каждого ответа.
/// По одной `.` и `..` на папку не в счёт, лишние считаются и не попадают
/// в результат; пустые ответы тоже ограничены, чтобы сервер не гонял цикл.
async fn collect_capped<F, Fut>(
    mut next: F,
    seen: &mut usize,
    dir: &str,
) -> Result<Vec<(String, FileAttributes)>, StoreError>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Result<Name, SftpError>>,
{
    let mut out = Vec::new();
    let (mut dot, mut dotdot, mut replies) = (false, false, 0usize);
    loop {
        let name = match next().await {
            Ok(n) => n,
            Err(SftpError::Status(s)) if s.status_code == StatusCode::Eof => return Ok(out),
            Err(e) => return Err(sftp_err(e)),
        };
        replies += 1;
        if replies > super::MAX_LIST {
            return Err(too_many(dir));
        }
        for f in name.files {
            match f.filename.as_str() {
                "." if !dot => dot = true,
                ".." if !dotdot => dotdot = true,
                n => {
                    *seen += 1;
                    if *seen > super::MAX_LIST {
                        return Err(too_many(dir));
                    }
                    if n != "." && n != ".." {
                        out.push((f.filename, f.attrs));
                    }
                }
            }
        }
    }
}

/// `root/name`, сохраняя `/` в начале абсолютного пути.
fn join_prefix_keep_root(root: &str, name: &str) -> String {
    if root.starts_with('/') {
        format!("/{}", join_prefix(root, name))
    } else {
        join_prefix(root, name)
    }
}

/// Создать папку и всех недостающих предков (ошибки «уже есть» не в счёт).
async fn mkdir_p(sftp: &SftpSession, dir: &str) -> Result<(), StoreError> {
    let mut cur = String::new();
    for (i, part) in dir.split('/').enumerate() {
        if part.is_empty() {
            if i == 0 {
                cur.push('/');
            }
            continue;
        }
        if !cur.is_empty() && !cur.ends_with('/') {
            cur.push('/');
        }
        cur.push_str(part);
        // Папку могло создать соседнее устройство между проверкой и mkdir.
        if !sftp.try_exists(cur.clone()).await.map_err(sftp_err)?
            && let Err(e) = sftp.create_dir(cur.clone()).await
            && !sftp.try_exists(cur.clone()).await.unwrap_or(false)
        {
            return Err(sftp_err(e));
        }
    }
    Ok(())
}

async fn write_file(sftp: &SftpSession, path: &str, data: &[u8]) -> Result<(), SftpError> {
    let mut f = sftp.create(path.to_string()).await?;
    f.write_all(data).await?;
    f.shutdown().await?;
    Ok(())
}

fn too_many(dir: &str) -> StoreError {
    StoreError::Corrupt(format!("в {dir} больше {} объектов", super::MAX_LIST))
}

fn parent(path: &str) -> Option<&str> {
    path.rfind('/')
        .map(|i| &path[..i])
        .filter(|p| !p.is_empty())
}

impl ObjectStore for SftpStore {
    fn put<'a>(&'a self, name: &'a str, data: Vec<u8>) -> StoreFuture<'a, ()> {
        Box::pin(async move {
            let path = self.path(name)?;
            self.run(|s| async move {
                let tmp = format!("{path}.tmp-{}", uuid::Uuid::now_v7().simple());
                if let Err(e) = write_file(&s, &tmp, &data).await {
                    if !is_missing(&e) {
                        return Err(sftp_err(e));
                    }
                    if let Some(dir) = parent(&path) {
                        mkdir_p(&s, dir).await?;
                    }
                    write_file(&s, &tmp, &data).await.map_err(sftp_err)?;
                }
                // SFTP v3 не переименовывает поверх существующего файла:
                // сервер отвечает отказом, тогда удалить и повторить. Обрыв
                // или тайм-аут — просто ошибка, загрузка повторится.
                if let Err(e) = s.rename(tmp.clone(), path.clone()).await {
                    if !is_refusal(&e) {
                        return Err(sftp_err(e));
                    }
                    let _ = s.remove_file(path.clone()).await;
                    if let Err(e) = s.rename(tmp.clone(), path.clone()).await {
                        let _ = s.remove_file(tmp).await;
                        return Err(sftp_err(e));
                    }
                }
                Ok(())
            })
            .await
        })
    }

    fn get<'a>(&'a self, name: &'a str, max: u64) -> StoreFuture<'a, Option<Vec<u8>>> {
        Box::pin(async move {
            let path = self.path(name)?;
            self.run(|s| async move {
                let f = match s.open(path).await {
                    Ok(f) => f,
                    Err(e) if is_missing(&e) => return Ok(None),
                    Err(e) => return Err(sftp_err(e)),
                };
                // Не больше предела и ещё байта: сервер, отдающий бесконечный
                // файл, не раздует память.
                let mut out = Vec::new();
                f.take(max + 1)
                    .read_to_end(&mut out)
                    .await
                    .map_err(|e| StoreError::Network(e.to_string()))?;
                if out.len() as u64 > max {
                    return Err(super::too_large(name, max));
                }
                Ok(Some(out))
            })
            .await
        })
    }

    fn head<'a>(&'a self, name: &'a str) -> StoreFuture<'a, Option<ObjectInfo>> {
        Box::pin(async move {
            let path = self.path(name)?;
            self.run(|s| async move {
                match s.metadata(path).await {
                    Ok(m) => Ok(Some(ObjectInfo {
                        name: name.to_string(),
                        size: m.size.unwrap_or(0),
                        modified_ms: m.mtime.map_or(0, |t| t as u64 * 1000),
                    })),
                    Err(e) if is_missing(&e) => Ok(None),
                    Err(e) => Err(sftp_err(e)),
                }
            })
            .await
        })
    }

    fn delete<'a>(&'a self, name: &'a str) -> StoreFuture<'a, ()> {
        Box::pin(async move {
            let path = self.path(name)?;
            self.run(|s| async move {
                match s.remove_file(path).await {
                    Ok(()) => Ok(()),
                    Err(e) if is_missing(&e) => Ok(()),
                    Err(e) => Err(sftp_err(e)),
                }
            })
            .await
        })
    }

    fn list<'a>(&'a self, dir: &'a str) -> StoreFuture<'a, Vec<ObjectInfo>> {
        Box::pin(async move {
            let root = self.path(dir)?;
            self.run_link(|l| async move {
                let mut out = Vec::new();
                // Обход: (путь на сервере, имя без префикса, глубина).
                let mut queue = vec![(root, dir.to_string(), 0)];
                let mut seen = 0usize;
                while let Some((path, rel, depth)) = queue.pop() {
                    let Some(rd) = read_dir_any(&l, &path, &mut seen, dir).await? else {
                        continue;
                    };
                    for (n, m) in rd {
                        let (p, r) = (format!("{path}/{n}"), format!("{rel}/{n}"));
                        if m.is_dir() {
                            // Глубже раскладки Clipwire ничего не кладёт.
                            if depth < super::MAX_DEPTH {
                                queue.push((p, r, depth + 1));
                            }
                        } else if !n.contains(".tmp-") {
                            out.push(ObjectInfo {
                                name: r,
                                size: m.size.unwrap_or(0),
                                modified_ms: m.mtime.map_or(0, |t| t as u64 * 1000),
                            });
                        }
                    }
                }
                Ok(out)
            })
            .await
        })
    }

    fn delete_dir<'a>(&'a self, dir: &'a str) -> StoreFuture<'a, ()> {
        Box::pin(async move {
            let root = self.path(dir)?;
            self.run_link(|l| async move {
                let s = &l.sftp;
                // Сначала файлы, потом папки от самых глубоких.
                let mut dirs = vec![root.clone()];
                let mut queue = vec![(root, 0)];
                let mut seen = 0usize;
                while let Some((path, depth)) = queue.pop() {
                    let Some(rd) = read_dir_any(&l, &path, &mut seen, dir).await? else {
                        continue;
                    };
                    for (n, m) in rd {
                        let p = format!("{path}/{n}");
                        if m.is_dir() {
                            // Папку глубже раскладки не обходим: удалить её
                            // не выйдет, и это честная ошибка.
                            if depth < super::MAX_DEPTH {
                                dirs.push(p.clone());
                                queue.push((p, depth + 1));
                            }
                        } else if let Err(e) = s.remove_file(p).await
                            && !is_missing(&e)
                        {
                            return Err(sftp_err(e));
                        }
                    }
                }
                for d in dirs.iter().rev() {
                    if let Err(e) = s.remove_dir(d.clone()).await
                        && !is_missing(&e)
                    {
                        return Err(sftp_err(e));
                    }
                }
                Ok(())
            })
            .await
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(path: &str) -> SftpConfig {
        SftpConfig {
            host: "h".into(),
            port: 22,
            user: "u".into(),
            auth: SftpAuth::Password("p".into()),
            path: path.into(),
            host_key_sha256: String::new(),
        }
    }

    #[test]
    fn fingerprints_compare_loosely() {
        let a = normalize_fingerprint("SHA256:abcDEF+/=");
        assert_eq!(a, "SHA256:abcDEF+/");
        assert_eq!(normalize_fingerprint(" abcDEF+/ "), a);
        assert_ne!(normalize_fingerprint("SHA256:abcdef+/"), a);
    }

    #[test]
    fn paths_keep_root() {
        let s = SftpStore::new(cfg("/srv/clip/")).unwrap();
        assert_eq!(s.path("ab/meta").unwrap(), "/srv/clip/ab/meta");
        let s = SftpStore::new(cfg("clip")).unwrap();
        assert_eq!(s.path("ab/meta").unwrap(), "clip/ab/meta");
        let s = SftpStore::new(cfg("")).unwrap();
        assert_eq!(s.path("ab").unwrap(), "ab");
        assert!(s.path("../etc").is_err());
        assert_eq!(parent("/a/b/c"), Some("/a/b"));
        assert_eq!(parent("c"), None);
    }

    #[test]
    fn rename_retried_only_after_refusal() {
        use russh_sftp::protocol::Status;
        let st = |code| {
            SftpError::Status(Status {
                id: 0,
                status_code: code,
                error_message: String::new(),
                language_tag: String::new(),
            })
        };
        assert!(is_refusal(&st(StatusCode::Failure)));
        assert!(!is_refusal(&st(StatusCode::ConnectionLost)));
        assert!(!is_refusal(&SftpError::Timeout));
        assert!(!is_refusal(&SftpError::IO("обрыв".into())));
    }

    fn entry(n: &str) -> russh_sftp::protocol::File {
        russh_sftp::protocol::File {
            filename: n.into(),
            longname: String::new(),
            attrs: FileAttributes::default(),
        }
    }

    fn eof() -> SftpError {
        SftpError::Status(russh_sftp::protocol::Status {
            id: 0,
            status_code: StatusCode::Eof,
            error_message: String::new(),
            language_tag: String::new(),
        })
    }

    #[tokio::test]
    async fn readdir_stops_at_limit_without_eof() {
        // Сервер без EOF: каждый ответ — тысяча новых записей.
        let mut replies = 0usize;
        let mut seen = 0usize;
        let r = collect_capped(
            || {
                replies += 1;
                let files = (0..1000).map(|i| entry(&format!("f{i}"))).collect();
                async move { Ok(Name { id: 0, files }) }
            },
            &mut seen,
            "x",
        )
        .await;
        assert!(matches!(r, Err(StoreError::Corrupt(_))));
        // Остановились на первом ответе, перевалившем за предел.
        assert_eq!(replies, super::super::MAX_LIST / 1000 + 1);

        // Пустые ответы без EOF тоже не крутятся вечно.
        let mut seen = 0usize;
        let r = collect_capped(
            || async {
                Ok(Name {
                    id: 0,
                    files: vec![],
                })
            },
            &mut seen,
            "x",
        )
        .await;
        assert!(matches!(r, Err(StoreError::Corrupt(_))));
    }

    #[tokio::test]
    async fn readdir_skips_dots_and_counts_across_folders() {
        let mut step = 0;
        let mut seen = 5usize;
        let got = collect_capped(
            || {
                step += 1;
                let r = match step {
                    1 => Ok(Name {
                        id: 0,
                        files: vec![entry("."), entry(".."), entry("a")],
                    }),
                    // Повторная «.» — подозрительно: считается, но не выдаётся.
                    2 => Ok(Name {
                        id: 0,
                        files: vec![entry("b"), entry(".")],
                    }),
                    _ => Err(eof()),
                };
                async move { r }
            },
            &mut seen,
            "x",
        )
        .await
        .unwrap();
        let names: Vec<_> = got.iter().map(|(n, _)| n.as_str()).collect();
        assert_eq!(names, ["a", "b"]);
        assert_eq!(seen, 8);
    }

    #[test]
    fn bad_private_key_is_refused_early() {
        let mut c = cfg("/x");
        c.auth = SftpAuth::Key {
            private_key: "не ключ".into(),
            passphrase: None,
        };
        assert!(matches!(SftpStore::new(c), Err(StoreError::Auth(_))));
    }
}
