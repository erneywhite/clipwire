//! Настройки группы на устройстве (`docs/storage.md`): какое хранилище файлов
//! выбрано, доступы к нему, срок хранения.
//!
//! Настройки приходят с сервера (`ServerMsg::GroupConfig`, зашифрованы ключом
//! группы) и лежат локально в `group_config.bin`, защищённые так же, как ключ
//! группы ([`crate::secret`], DPAPI на Windows): без сервера устройство
//! продолжает ходить в своё хранилище. Побеждает большая
//! [`GroupConfig::version`]; меньшей, чем уже есть, устройство не верит
//! (сервер не откатит настройки). Если в элементе соседа видна версия
//! больше своей ([`GroupStorage::note_version`]), свои настройки считаются
//! устаревшими: устройство просит их у сервера заново и, пока не получит, во
//! внешнее хранилище не ходит.
//!
//! Интерфейс работает через [`StorageController`]: текущие настройки без
//! секретов ([`StorageView`]), полные — только по явному запросу формы,
//! сохранить и проверить подключение.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};
use clipwire_core::storage::{GroupConfig, MAX_CONFIG_BYTES, SftpAuth, StorageConfig};
use clipwire_core::{GroupId, GroupKey};
use serde::Serialize;
use tokio::sync::{Notify, mpsc, oneshot, watch};

use crate::devices::Control;
use crate::storage::external::Probe;
use crate::storage::{ObjectStore, StoreError, open_store};

/// Файл настроек группы в каталоге состояния.
pub const CONFIG_FILE: &str = "group_config.bin";
/// Отметка последней чистки внешнего хранилища по сроку (мс Unix).
pub const CLEANED_FILE: &str = "storage_cleaned";
/// Не чаще этого чистить внешнее хранилище.
pub const CLEANUP_EVERY: std::time::Duration = std::time::Duration::from_secs(3600);
/// Предел одного прохода чистки: недоверенное хранилище не должно держать
/// её вечно (пока она идёт, новая не запускается).
pub const CLEANUP_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10 * 60);

fn file(dir: &Path) -> PathBuf {
    dir.join(CONFIG_FILE)
}

/// Настройки группы `key` с диска. Файл другой группы или битый — `None`.
pub fn load_local(dir: &Path, key: &GroupKey) -> Option<GroupConfig> {
    let raw = std::fs::read(file(dir)).ok()?;
    let sealed = crate::secret::unprotect(&raw)
        .map_err(|e| tracing::warn!("{CONFIG_FILE} не расшифровался: {e:#}"))
        .ok()?;
    key.open_config(&sealed).ok()
}

/// Сохранить настройки: зашифрованы ключом группы и ещё раз защищены, как
/// ключ группы на диске.
pub fn save_local(dir: &Path, key: &GroupKey, cfg: &GroupConfig) -> Result<()> {
    std::fs::create_dir_all(dir)?;
    let sealed = key
        .seal_config(cfg)
        .context("настройки группы не зашифровались")?;
    crate::state::write_atomic(&file(dir), &crate::secret::protect(&sealed)?)
}

/// Ключ группы сменился: переложить настройки под новый ключ.
pub fn reseal_local(dir: &Path, old: &GroupKey, new: &GroupKey) -> Result<()> {
    match load_local(dir, old) {
        Some(cfg) => save_local(dir, new, &cfg),
        None => Ok(()),
    }
}

pub fn remove_local(dir: &Path) -> Result<()> {
    for p in [file(dir), dir.join(CLEANED_FILE)] {
        match std::fs::remove_file(&p) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e).with_context(|| format!("не удалился {}", p.display())),
        }
    }
    Ok(())
}

/// Настройки для показа: без секретов (ключа, пароля, закрытого ключа).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct StorageView {
    pub version: u64,
    /// `"server"`, `"s3"` или `"sftp"`.
    pub kind: &'static str,
    pub file_ttl_hours: u32,
    pub s3: Option<S3View>,
    pub sftp: Option<SftpView>,
    /// S3 по HTTP (только локальная сеть): показать предупреждение.
    pub insecure_http: bool,
    /// Хранилище по этим настройкам не открылось (файлы идут через сервер).
    pub error: Option<String>,
    /// Доступы к хранилищу были у исключённого устройства: их надо сменить
    /// и сохранить настройки заново ([`GroupConfig::access_exposed`]).
    pub access_exposed: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct S3View {
    pub endpoint: String,
    pub region: String,
    pub bucket: String,
    pub prefix: String,
    pub path_style: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SftpView {
    pub host: String,
    pub port: u16,
    pub user: String,
    pub path: String,
    pub host_key_sha256: String,
    /// `"password"` или `"key"`.
    pub auth: &'static str,
}

impl StorageView {
    pub fn of(cfg: &GroupConfig, error: Option<String>) -> Self {
        let (kind, s3, sftp, insecure_http) = match &cfg.storage {
            StorageConfig::Server => ("server", None, None, false),
            StorageConfig::S3(c) => (
                "s3",
                Some(S3View {
                    endpoint: c.endpoint.clone(),
                    region: c.region.clone(),
                    bucket: c.bucket.clone(),
                    prefix: c.prefix.clone(),
                    path_style: c.path_style,
                }),
                None,
                c.endpoint.trim().starts_with("http://"),
            ),
            StorageConfig::Sftp(c) => (
                "sftp",
                None,
                Some(SftpView {
                    host: c.host.clone(),
                    port: c.port,
                    user: c.user.clone(),
                    path: c.path.clone(),
                    host_key_sha256: c.host_key_sha256.clone(),
                    auth: match c.auth {
                        SftpAuth::Password(_) => "password",
                        SftpAuth::Key { .. } => "key",
                    },
                }),
                false,
            ),
        };
        Self {
            version: cfg.version,
            kind,
            file_ttl_hours: cfg.file_ttl_hours,
            s3,
            sftp,
            insecure_http,
            error,
            access_exposed: cfg.access_exposed && !matches!(cfg.storage, StorageConfig::Server),
        }
    }
}

/// Что сообщить интерфейсу.
#[derive(Debug, Clone)]
pub enum StorageEvent {
    /// Настройки группы изменились (здесь или на другом устройстве).
    Changed(Box<StorageView>),
    /// Внешнее хранилище недоступно, файл ушёл через сервер Clipwire.
    Warning(String),
}

pub type StorageSink = Arc<dyn Fn(StorageEvent) + Send + Sync>;

struct Current {
    cfg: GroupConfig,
    store: Option<Arc<dyn ObjectStore>>,
    error: Option<String>,
    /// Наибольшая версия настроек, замеченная в элементах соседей. Больше
    /// `cfg.version` — свои настройки устарели.
    // shortcut: только в памяти; после перезапуска устройство снова узнает
    // о новой версии из следующего элемента с файлом.
    seen: u64,
}

/// Текущие настройки группы и открытое по ним хранилище. Общие для движка
/// (применяет пришедшее с сервера), передачи файлов и интерфейса.
pub struct GroupStorage {
    key: GroupKey,
    dir: PathBuf,
    cur: Mutex<Current>,
    sink: Mutex<Option<StorageSink>>,
    /// Попросить движок заново запросить настройки у сервера.
    refresh: Notify,
    /// Версия применённых настроек — для тех, кто ждёт новых.
    applied: watch::Sender<u64>,
}

fn build(cfg: &GroupConfig) -> (Option<Arc<dyn ObjectStore>>, Option<String>) {
    match open_store(&cfg.storage) {
        Ok(s) => (s, None),
        Err(e) => {
            tracing::warn!("хранилище группы не открылось: {e}");
            (None, Some(e.to_string()))
        }
    }
}

impl GroupStorage {
    /// Настройки с диска (или по умолчанию — хранилище сервера).
    pub fn load(key: GroupKey, dir: &Path) -> Arc<Self> {
        let cfg = load_local(dir, &key).unwrap_or_default();
        let (store, error) = build(&cfg);
        let (applied, _) = watch::channel(cfg.version);
        Arc::new(Self {
            key,
            dir: dir.to_path_buf(),
            cur: Mutex::new(Current {
                cfg,
                store,
                error,
                seen: 0,
            }),
            sink: Mutex::new(None),
            refresh: Notify::new(),
            applied,
        })
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Current> {
        self.cur.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub fn group(&self) -> GroupId {
        self.key.group_id()
    }

    pub fn set_sink(&self, sink: StorageSink) {
        *self.sink.lock().unwrap_or_else(|e| e.into_inner()) = Some(sink);
    }

    fn emit(&self, e: StorageEvent) {
        let sink = self.sink.lock().unwrap_or_else(|e| e.into_inner()).clone();
        if let Some(s) = sink {
            s(e);
        }
    }

    /// Предупредить интерфейс (внешнее хранилище недоступно и т. п.).
    pub fn warn(&self, msg: String) {
        tracing::warn!("{msg}");
        self.emit(StorageEvent::Warning(msg));
    }

    /// Полные настройки, с секретами: только для формы настроек.
    pub fn config(&self) -> GroupConfig {
        self.lock().cfg.clone()
    }

    pub fn view(&self) -> StorageView {
        let c = self.lock();
        StorageView::of(&c.cfg, c.error.clone())
    }

    /// Внешнее хранилище, если выбрано и открылось: версия настроек и само
    /// хранилище. `None` — файлы идут через сервер Clipwire; так же, пока
    /// настройки устарели ([`Self::note_version`]): по старым доступам не
    /// пишем и не чистим.
    pub fn external(&self) -> Option<(u64, Arc<dyn ObjectStore>)> {
        let c = self.lock();
        if c.seen > c.cfg.version {
            return None;
        }
        c.store.clone().map(|s| (c.cfg.version, s))
    }

    /// В элементе соседа файл лежит по настройкам версии `v`. Если она
    /// больше своей, свои настройки устарели: попросить их у сервера заново.
    /// `true`, если устарели.
    pub fn note_version(&self, v: u64) -> bool {
        let stale = {
            let mut c = self.lock();
            if v > c.cfg.version {
                c.seen = c.seen.max(v);
                true
            } else {
                false
            }
        };
        if stale {
            tracing::info!(v, "у соседа настройки группы новее, запрашиваю их заново");
            self.refresh.notify_one();
        }
        stale
    }

    /// Ждать, пока движок попросит настройки заново ([`Self::note_version`]).
    pub async fn refresh_wanted(&self) {
        self.refresh.notified().await;
    }

    /// Дождаться настроек версии не меньше `v` (не дольше `t`). `true` —
    /// дождались.
    pub async fn wait_version(&self, v: u64, t: std::time::Duration) -> bool {
        let mut rx = self.applied.subscribe();
        tokio::time::timeout(t, rx.wait_for(|&cur| cur >= v))
            .await
            .is_ok_and(|r| r.is_ok())
    }

    /// Подменить хранилище своим (папка, заглушка) при тех же настройках:
    /// для сквозных тестов, в настройках группы такого выбора нет.
    #[doc(hidden)]
    pub fn use_store(&self, version: u64, store: Arc<dyn ObjectStore>) {
        let mut c = self.lock();
        c.cfg.version = version;
        c.store = Some(store);
        c.error = None;
        drop(c);
        self.applied.send_replace(version);
    }

    /// Применить настройки, если они новее текущих: сохранить на диск,
    /// открыть хранилище, сообщить интерфейсу. `true`, если применены.
    pub fn apply(&self, cfg: GroupConfig) -> bool {
        let view = {
            let mut c = self.lock();
            if cfg.version <= c.cfg.version {
                return false;
            }
            if let Err(e) = save_local(&self.dir, &self.key, &cfg) {
                tracing::warn!("настройки группы не сохранились: {e:#}");
            }
            let (store, error) = build(&cfg);
            let seen = c.seen;
            *c = Current {
                cfg,
                store,
                error,
                seen,
            };
            StorageView::of(&c.cfg, c.error.clone())
        };
        self.applied.send_replace(view.version);
        tracing::info!(
            version = view.version,
            kind = view.kind,
            "настройки группы применены"
        );
        self.emit(StorageEvent::Changed(Box::new(view)));
        true
    }

    /// Применить зашифрованные настройки с сервера.
    pub fn apply_sealed(&self, sealed: &[u8]) -> bool {
        match self.key.open_config(sealed) {
            Ok(cfg) => self.apply(cfg),
            Err(e) => {
                tracing::warn!("настройки группы с сервера не открылись: {e}");
                false
            }
        }
    }

    /// Зашифровать текущие настройки ключом `key` (для новой группы при
    /// смене ключа или чтобы вернуть их серверу). `None` — настроек нет.
    pub fn seal_current(&self, key: &GroupKey) -> Option<Vec<u8>> {
        let cfg = self.config();
        if cfg.version == 0 {
            return None;
        }
        key.seal_config(&cfg).ok()
    }

    /// Настройки для новой группы при смене ключа. Если исключают
    /// устройство, а у группы внешнее хранилище, доступы к нему остаются у
    /// исключённого: настройки уходят с пометкой [`GroupConfig::access_exposed`]
    /// и новой версией, интерфейс всех устройств попросит их сменить.
    /// Доступы сами по себе не сбрасываются: молча вернуть файлы на сервер
    /// Clipwire было бы хуже. `None` — настроек нет.
    pub fn seal_for_rekey(&self, key: &GroupKey, removing: bool) -> Option<Vec<u8>> {
        let mut cfg = self.config();
        if cfg.version == 0 {
            return None;
        }
        if removing && !matches!(cfg.storage, StorageConfig::Server) {
            cfg.access_exposed = true;
            let now = crate::history::now_ms().max(0) as u64;
            cfg.version = now.max(cfg.version + 1);
        }
        key.seal_config(&cfg).ok()
    }

    /// Версия текущих настроек.
    pub fn version(&self) -> u64 {
        self.lock().cfg.version
    }

    /// Почистить внешнее хранилище по сроку, если с прошлой чистки прошёл
    /// [`CLEANUP_EVERY`] (отметка на диске переживает перезапуск).
    pub async fn maybe_cleanup(&self) {
        let Some((_, store)) = self.external() else {
            return;
        };
        // Часы устройства здесь решают только, как часто чистить; что
        // удалять, решают часы хранилища (`external::cleanup`).
        let now = crate::history::now_ms().max(0) as u64;
        let mark = self.dir.join(CLEANED_FILE);
        let last = std::fs::read_to_string(&mark)
            .ok()
            .and_then(|s| s.trim().parse::<u64>().ok())
            .unwrap_or(0);
        // Отметка из будущего (часы перевели назад) не должна выключать
        // чистку до тех пор.
        if last <= now && now - last < CLEANUP_EVERY.as_millis() as u64 {
            return;
        }
        let _ = crate::state::write_atomic(&mark, now.to_string().as_bytes());
        let ttl = self.config().file_ttl_hours;
        let group = self.group();
        let run = crate::storage::external::cleanup(store.as_ref(), &group, ttl);
        match tokio::time::timeout(CLEANUP_TIMEOUT, run).await {
            Ok(Ok(0)) => {}
            Ok(Ok(n)) => tracing::info!(n, "из внешнего хранилища удалены файлы старше срока"),
            Ok(Err(e)) => tracing::info!("чистка внешнего хранилища не прошла: {e}"),
            Err(_) => tracing::info!("чистка внешнего хранилища не уложилась в срок, прервана"),
        }
    }
}

/// Как интерфейс работает с настройками хранилища группы.
#[derive(Clone)]
pub struct StorageController {
    pub(crate) tx: mpsc::Sender<Control>,
    pub(crate) storage: Arc<GroupStorage>,
}

const OFFLINE: &str = "Нет связи с сервером. Попробуйте, когда устройство будет в сети";

impl StorageController {
    pub fn new(tx: mpsc::Sender<Control>, storage: Arc<GroupStorage>) -> Self {
        Self { tx, storage }
    }

    /// Текущие настройки без секретов.
    pub fn view(&self) -> StorageView {
        self.storage.view()
    }

    /// Полные настройки с секретами — только для заполнения формы.
    pub fn config_for_form(&self) -> GroupConfig {
        self.storage.config()
    }

    /// Сохранить настройки группы: они уходят на сервер и остальным
    /// устройствам. Версия ставится здесь (больше текущей). Нужна связь с
    /// сервером. Возвращает применённые настройки без секретов.
    pub async fn save(
        &self,
        storage: StorageConfig,
        file_ttl_hours: u32,
    ) -> Result<StorageView, String> {
        // Настройки, по которым хранилище даже не открывается, не сохраняем.
        open_store(&storage).map_err(|e| e.to_string())?;
        // Без подтверждённого отпечатка ни одно устройство не войдёт.
        if let StorageConfig::Sftp(c) = &storage
            && c.host_key_sha256.trim().is_empty()
        {
            return Err(
                "Сначала проверьте подключение и подтвердите отпечаток ключа сервера".into(),
            );
        }
        let now = crate::history::now_ms().max(0) as u64;
        // Сохранение снимает пометку «доступы у исключённого устройства»:
        // человек ввёл их заново.
        let cfg = GroupConfig {
            version: now.max(self.storage.version() + 1),
            storage,
            file_ttl_hours,
            access_exposed: false,
        };
        let sealed = self
            .storage
            .key
            .seal_config(&cfg)
            .map_err(|e| format!("Настройки не сохранились: {e}"))?;
        if sealed.len() > MAX_CONFIG_BYTES {
            return Err("Настройки слишком большие (длинный ключ SSH?)".into());
        }
        let (reply, rx) = oneshot::channel();
        self.tx
            .send(Control::SetGroupConfig {
                sealed,
                config: cfg,
                reply,
            })
            .await
            .map_err(|_| OFFLINE.to_string())?;
        rx.await.map_err(|_| OFFLINE.to_string())??;
        Ok(self.storage.view())
    }

    /// «Проверить подключение»: записать, прочитать и удалить пробный
    /// объект. Для SFTP без отпечатка — только отпечаток ключа хоста
    /// ([`Probe::ConfirmHostKey`]), без входа.
    pub async fn probe(&self, storage: &StorageConfig) -> Result<Probe, StoreError> {
        crate::storage::external::probe(storage, &self.storage.group()).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clipwire_core::storage::S3Config;

    fn dir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("clipwire-gcfg-{tag}-{}", uuid::Uuid::now_v7()));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn s3(version: u64) -> GroupConfig {
        GroupConfig {
            version,
            storage: StorageConfig::S3(S3Config {
                endpoint: "http://127.0.0.1:9000".into(),
                region: "us-east-1".into(),
                bucket: "clip".into(),
                prefix: String::new(),
                access_key: "AK".into(),
                secret_key: "секрет".into(),
                path_style: true,
            }),
            file_ttl_hours: 24,
            access_exposed: false,
        }
    }

    #[test]
    fn local_copy_is_per_group_and_newer_wins() {
        let d = dir("local");
        let key = GroupKey([1; 32]);
        let st = GroupStorage::load(key.clone(), &d);
        assert_eq!(st.view().kind, "server");
        assert!(st.external().is_none());
        assert!(st.apply(s3(5)));
        assert!(!st.apply(s3(5)), "та же версия не применяется повторно");
        assert!(!st.apply(GroupConfig::default()));
        assert_eq!(st.external().map(|(v, _)| v), Some(5));
        // На диске не открытым текстом (на Windows — DPAPI поверх шифра группы).
        let raw = std::fs::read(d.join(CONFIG_FILE)).unwrap();
        assert!(!String::from_utf8_lossy(&raw).contains("секрет"));
        // После перезапуска — те же настройки; другой ключ их не видит.
        assert_eq!(GroupStorage::load(key.clone(), &d).config(), s3(5));
        assert_eq!(
            GroupStorage::load(GroupKey([2; 32]), &d).config().version,
            0
        );
        // Смена ключа перекладывает настройки под новый.
        reseal_local(&d, &key, &GroupKey([2; 32])).unwrap();
        assert_eq!(GroupStorage::load(GroupKey([2; 32]), &d).config(), s3(5));
        remove_local(&d).unwrap();
        assert_eq!(
            GroupStorage::load(GroupKey([2; 32]), &d).config().version,
            0
        );
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn rollback_refused_and_newer_neighbour_version_makes_config_stale() {
        let d = dir("stale");
        let st = GroupStorage::load(GroupKey([3; 32]), &d);
        assert!(st.apply(s3(10)));
        // Сервер не откатит: меньшая версия не применяется, и после
        // перезапуска тоже (локальная копия помнит большую).
        assert!(!st.apply(s3(9)));
        let again = GroupStorage::load(GroupKey([3; 32]), &d);
        assert!(!again.apply(s3(9)));
        assert_eq!(again.version(), 10);
        // Сосед пишет файлы по версии 12: свои настройки устарели, во внешнее
        // хранилище по ним не ходим, пока не придут новые.
        assert!(!st.note_version(10));
        assert!(st.external().is_some());
        assert!(st.note_version(12));
        assert!(st.external().is_none());
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            // Движок получил просьбу запросить настройки.
            tokio::time::timeout(std::time::Duration::from_secs(1), st.refresh_wanted())
                .await
                .unwrap();
            assert!(
                !st.wait_version(12, std::time::Duration::from_millis(50))
                    .await
            );
            let waiter = st.wait_version(12, std::time::Duration::from_secs(5));
            let apply = async {
                tokio::time::sleep(std::time::Duration::from_millis(20)).await;
                assert!(st.apply(s3(12)));
            };
            let (ok, ()) = tokio::join!(waiter, apply);
            assert!(ok);
        });
        assert_eq!(st.external().map(|(v, _)| v), Some(12));
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn rekey_with_removal_marks_access_exposed() {
        let d = dir("exposed");
        let key = GroupKey([4; 32]);
        let new = GroupKey([5; 32]);
        let st = GroupStorage::load(key.clone(), &d);
        assert!(st.seal_for_rekey(&new, true).is_none(), "настроек нет");
        assert!(st.apply(s3(10)));
        // Без исключения — как есть.
        let same = new
            .open_config(&st.seal_for_rekey(&new, false).unwrap())
            .unwrap();
        assert_eq!(same, s3(10));
        // С исключением — пометка и новая версия, доступы те же.
        let marked = new
            .open_config(&st.seal_for_rekey(&new, true).unwrap())
            .unwrap();
        assert!(marked.access_exposed && marked.version > 10);
        assert_eq!(marked.storage, s3(10).storage);
        assert!(StorageView::of(&marked, None).access_exposed);
        // Хранилище сервера Clipwire: доступов нет, пометки тоже.
        let server = GroupStorage::load(key, &dir("exposed-srv"));
        assert!(server.apply(GroupConfig {
            version: 3,
            ..GroupConfig::default()
        }));
        let c = new
            .open_config(&server.seal_for_rekey(&new, true).unwrap())
            .unwrap();
        assert!(!c.access_exposed && c.version == 3);
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn view_has_no_secrets() {
        let v = StorageView::of(&s3(1), None);
        let json = serde_json::to_string(&v).unwrap();
        assert!(
            !json.contains("секрет") && !json.contains("\"AK\""),
            "{json}"
        );
        assert!(v.insecure_http);
        let sftp = GroupConfig {
            version: 1,
            storage: StorageConfig::Sftp(clipwire_core::storage::SftpConfig {
                host: "h".into(),
                port: 22,
                user: "u".into(),
                auth: SftpAuth::Key {
                    private_key: "-----BEGIN OPENSSH PRIVATE KEY-----".into(),
                    passphrase: Some("пароль".into()),
                },
                path: "/clip".into(),
                host_key_sha256: "SHA256:x".into(),
            }),
            file_ttl_hours: 24,
            access_exposed: false,
        };
        let json = serde_json::to_string(&StorageView::of(&sftp, None)).unwrap();
        assert!(
            !json.contains("пароль") && !json.contains("PRIVATE"),
            "{json}"
        );
        assert!(json.contains("\"auth\":\"key\""));
    }
}
