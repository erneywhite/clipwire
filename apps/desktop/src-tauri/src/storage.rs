//! Хранилище файлов группы (`docs/storage.md`): команды для карточки
//! «Хранилище файлов» в разделе «Синхронизация».
//!
//! Секреты (секретный ключ S3, пароль SFTP, закрытый ключ и пароль к нему) в
//! окно не уходят: форма получает пустые поля и признак «сохранён». Пустое
//! поле при сохранении или проверке значит «оставить сохранённый», но только
//! если не сменились адрес и ключ доступа (S3) или сервер, порт,
//! пользователь и подтверждённый отпечаток ключа сервера (SFTP): иначе
//! сохранённый секрет ушёл бы чужому серверу. Сменившийся ключ сервера может
//! значить перехват соединения, и после «Сбросить отпечаток» пароль или ключ
//! вводятся заново.

use std::sync::Mutex;

use clipwire_agent::group_config::StorageController;
use clipwire_agent::storage::StoreError;
use clipwire_agent::storage::external::Probe;
use clipwire_agent::storage::sftp::normalize_fingerprint;
use clipwire_core::storage::{GroupConfig, S3Config, SftpAuth, SftpConfig, StorageConfig};
use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Emitter, Manager};

use crate::AppState;

type CmdResult<T> = Result<T, String>;

/// Форма настроек. Обе вложенные формы есть всегда: при переключении вида
/// хранилища введённое не теряется.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct StorageForm {
    /// `"server"`, `"s3"` или `"sftp"`.
    pub kind: String,
    pub file_ttl_hours: u32,
    pub s3: S3Form,
    pub sftp: SftpForm,
}

#[derive(Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct S3Form {
    pub endpoint: String,
    pub region: String,
    pub bucket: String,
    pub prefix: String,
    pub access_key: String,
    /// Пусто — оставить сохранённый.
    pub secret_key: String,
    pub path_style: bool,
}

#[derive(Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SftpForm {
    pub host: String,
    pub port: u16,
    pub user: String,
    /// `"password"` или `"key"`.
    pub auth: String,
    pub password: String,
    pub private_key: String,
    pub passphrase: String,
    pub path: String,
    /// Подтверждённый отпечаток ключа хоста, пусто — ещё не подтверждён.
    pub host_key_sha256: String,
}

// Секреты не печатаем даже в отладочный вывод.
impl std::fmt::Debug for S3Form {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "S3Form({} / {} / {})",
            self.endpoint, self.bucket, self.prefix
        )
    }
}

impl std::fmt::Debug for SftpForm {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "SftpForm({}@{}:{} {})",
            self.user, self.host, self.port, self.path
        )
    }
}

/// Какие секреты уже сохранены (их значения в окно не отдаются).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct SavedSecrets {
    pub s3_secret: bool,
    pub sftp_password: bool,
    pub sftp_key: bool,
    pub sftp_passphrase: bool,
}

/// Предупреждение «файл ушёл через сервер».
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Warning {
    pub text: String,
    pub at_ms: i64,
}

/// Ответ `storage_get` и `storage_save`.
#[derive(Debug, Clone, Serialize)]
pub struct StorageDto {
    pub form: StorageForm,
    pub saved: SavedSecrets,
    /// Версия настроек: 0 — их ещё никто не сохранял.
    pub version: u64,
    /// Хранилище по сохранённым настройкам не открылось.
    pub error: Option<String>,
    /// S3 по HTTP (только локальная сеть).
    pub insecure_http: bool,
    /// Последнее предупреждение за этот запуск.
    pub warning: Option<Warning>,
    /// Доступы к хранилищу были у исключённого из группы устройства: их надо
    /// сменить у поставщика и сохранить заново.
    pub access_exposed: bool,
}

/// Итог «Проверить подключение».
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum ProbeDto {
    Ok {
        host_key: Option<String>,
        insecure_http: bool,
    },
    /// SFTP: отпечаток ещё не подтверждён, вход не выполнялся.
    ConfirmHostKey {
        host_key: String,
    },
    /// SFTP: сервер прислал не тот ключ, что подтверждён.
    HostKeyMismatch {
        expected: String,
        got: String,
    },
    Error {
        message: String,
    },
}

/// Последнее предупреждение хранилища: окно настроек могло быть закрыто,
/// когда оно пришло.
#[derive(Default)]
pub struct LastWarning(Mutex<Option<Warning>>);

/// Движок сообщил, что внешнее хранилище недоступно и файл ушёл через сервер.
pub fn on_warning(app: &AppHandle, text: String) {
    let w = Warning {
        text,
        at_ms: clipwire_agent::history::now_ms(),
    };
    *app.state::<LastWarning>()
        .0
        .lock()
        .unwrap_or_else(|e| e.into_inner()) = Some(w.clone());
    let _ = app.emit("clipwire://storage-warning", &w);
}

fn controller(app: &AppHandle) -> CmdResult<StorageController> {
    app.state::<AppState>()
        .service
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .as_ref()
        .and_then(|s| s.storage())
        .ok_or_else(|| "Хранилище выбирается, когда устройство в группе".into())
}

/// Форма из сохранённых настроек, без значений секретов.
fn to_form(cfg: &GroupConfig) -> (StorageForm, SavedSecrets) {
    let mut form = StorageForm {
        kind: "server".into(),
        file_ttl_hours: cfg.file_ttl_hours,
        s3: S3Form {
            region: "us-east-1".into(),
            path_style: true,
            ..Default::default()
        },
        sftp: SftpForm {
            port: 22,
            auth: "password".into(),
            ..Default::default()
        },
    };
    let mut saved = SavedSecrets::default();
    match &cfg.storage {
        StorageConfig::Server => {}
        StorageConfig::S3(c) => {
            form.kind = "s3".into();
            form.s3 = S3Form {
                endpoint: c.endpoint.clone(),
                region: c.region.clone(),
                bucket: c.bucket.clone(),
                prefix: c.prefix.clone(),
                access_key: c.access_key.clone(),
                secret_key: String::new(),
                path_style: c.path_style,
            };
            saved.s3_secret = !c.secret_key.is_empty();
        }
        StorageConfig::Sftp(c) => {
            form.kind = "sftp".into();
            form.sftp = SftpForm {
                host: c.host.clone(),
                port: c.port,
                user: c.user.clone(),
                auth: match c.auth {
                    SftpAuth::Password(_) => "password".into(),
                    SftpAuth::Key { .. } => "key".into(),
                },
                path: c.path.clone(),
                host_key_sha256: c.host_key_sha256.clone(),
                ..Default::default()
            };
            match &c.auth {
                SftpAuth::Password(p) => saved.sftp_password = !p.is_empty(),
                SftpAuth::Key { passphrase, .. } => {
                    saved.sftp_key = true;
                    saved.sftp_passphrase = passphrase.as_deref().is_some_and(|p| !p.is_empty());
                }
            }
        }
    }
    (form, saved)
}

/// Папка без пробелов и `/` по краям (у SFTP ведущий `/` значим: путь от корня).
fn clean_prefix(p: &str) -> String {
    p.trim().trim_matches('/').to_string()
}

fn clean_path(p: &str) -> String {
    let p = p.trim();
    let body = p.trim_matches('/');
    if p.starts_with('/') {
        format!("/{body}")
    } else {
        body.to_string()
    }
}

/// Настройки из формы. Пустой секрет берётся из сохранённых `current`, если
/// это то же хранилище (см. описание модуля).
fn from_form(form: &StorageForm, current: &StorageConfig) -> CmdResult<StorageConfig> {
    match form.kind.as_str() {
        "server" => Ok(StorageConfig::Server),
        "s3" => {
            let f = &form.s3;
            let endpoint = f.endpoint.trim().trim_end_matches('/').to_string();
            let access_key = f.access_key.trim().to_string();
            let secret_key = if !f.secret_key.is_empty() {
                f.secret_key.clone()
            } else {
                match current {
                    StorageConfig::S3(c)
                        if c.endpoint.trim().trim_end_matches('/') == endpoint
                            && c.access_key.trim() == access_key =>
                    {
                        c.secret_key.clone()
                    }
                    _ => return Err("Введите секретный ключ".into()),
                }
            };
            let region = f.region.trim();
            Ok(StorageConfig::S3(S3Config {
                endpoint,
                region: if region.is_empty() {
                    "us-east-1".into()
                } else {
                    region.to_string()
                },
                bucket: f.bucket.trim().to_string(),
                prefix: clean_prefix(&f.prefix),
                access_key,
                secret_key,
                path_style: f.path_style,
            }))
        }
        "sftp" => {
            let f = &form.sftp;
            if f.port == 0 {
                return Err("Порт — число от 1 до 65535".into());
            }
            let host = f.host.trim().to_string();
            let user = f.user.trim().to_string();
            // Пустой отпечаток в форме — только запрос отпечатка, без входа:
            // секрет при этом не отправляется, а сохранить без отпечатка
            // нельзя. Другой отпечаток — другой сервер, секрет заново.
            let form_key = f.host_key_sha256.trim();
            let same = match current {
                StorageConfig::Sftp(c)
                    if c.host.trim() == host
                        && c.port == f.port
                        && c.user.trim() == user
                        && (form_key.is_empty()
                            || normalize_fingerprint(form_key)
                                == normalize_fingerprint(&c.host_key_sha256)) =>
                {
                    Some(&c.auth)
                }
                _ => None,
            };
            let changed_key = matches!(current, StorageConfig::Sftp(c)
                if c.host.trim() == host && c.port == f.port && c.user.trim() == user)
                && same.is_none();
            let auth = match f.auth.as_str() {
                "password" => SftpAuth::Password(if !f.password.is_empty() {
                    f.password.clone()
                } else if let Some(SftpAuth::Password(p)) = same {
                    p.clone()
                } else if changed_key {
                    return Err("Ключ сервера изменился: введите пароль заново".into());
                } else {
                    return Err("Введите пароль".into());
                }),
                "key" => {
                    let new_pass = (!f.passphrase.is_empty()).then(|| f.passphrase.clone());
                    if !f.private_key.trim().is_empty() {
                        SftpAuth::Key {
                            private_key: f.private_key.trim().to_string() + "\n",
                            passphrase: new_pass,
                        }
                    } else if let Some(SftpAuth::Key {
                        private_key,
                        passphrase,
                    }) = same
                    {
                        SftpAuth::Key {
                            private_key: private_key.clone(),
                            passphrase: new_pass.or_else(|| passphrase.clone()),
                        }
                    } else if changed_key {
                        return Err("Ключ сервера изменился: вставьте закрытый ключ заново".into());
                    } else {
                        return Err("Вставьте закрытый ключ или выберите файл".into());
                    }
                }
                _ => return Err("Неизвестный способ входа".into()),
            };
            Ok(StorageConfig::Sftp(SftpConfig {
                host,
                port: f.port,
                user,
                auth,
                path: clean_path(&f.path),
                host_key_sha256: f.host_key_sha256.trim().to_string(),
            }))
        }
        other => Err(format!("Неизвестное хранилище «{other}»")),
    }
}

fn dto(ctl: &StorageController, app: &AppHandle) -> StorageDto {
    let view = ctl.view();
    let (form, saved) = to_form(&ctl.config_for_form());
    StorageDto {
        form,
        saved,
        version: view.version,
        error: view.error,
        insecure_http: view.insecure_http,
        access_exposed: view.access_exposed,
        warning: app
            .state::<LastWarning>()
            .0
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone(),
    }
}

/// Текущие настройки хранилища группы для формы.
#[tauri::command]
pub fn storage_get(app: AppHandle) -> CmdResult<StorageDto> {
    let ctl = controller(&app)?;
    Ok(dto(&ctl, &app))
}

/// Сохранить настройки: они уходят на сервер зашифрованными и оттуда на
/// остальные устройства группы.
#[tauri::command]
pub async fn storage_save(app: AppHandle, form: StorageForm) -> CmdResult<StorageDto> {
    let ctl = controller(&app)?;
    let storage = from_form(&form, &ctl.config_for_form().storage)?;
    ctl.save(storage, form.file_ttl_hours).await?;
    Ok(dto(&ctl, &app))
}

fn probe_dto(r: Result<Probe, StoreError>) -> ProbeDto {
    match r {
        Ok(Probe::Ok {
            host_key,
            insecure_http,
        }) => ProbeDto::Ok {
            host_key,
            insecure_http,
        },
        Ok(Probe::ConfirmHostKey { host_key }) => ProbeDto::ConfirmHostKey { host_key },
        Err(StoreError::HostKey { expected, got }) => ProbeDto::HostKeyMismatch { expected, got },
        Err(e) => ProbeDto::Error {
            message: e.to_string(),
        },
    }
}

/// «Проверить подключение»: записать, прочитать и удалить пробный объект.
#[tauri::command]
pub async fn storage_probe(app: AppHandle, form: StorageForm) -> CmdResult<ProbeDto> {
    let ctl = controller(&app)?;
    let storage = from_form(&form, &ctl.config_for_form().storage)?;
    Ok(probe_dto(ctl.probe(&storage).await))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s3_saved() -> StorageConfig {
        StorageConfig::S3(S3Config {
            endpoint: "https://s3.example.com".into(),
            region: "eu-1".into(),
            bucket: "clip".into(),
            prefix: "files".into(),
            access_key: "AK".into(),
            secret_key: "секрет".into(),
            path_style: true,
        })
    }

    fn sftp_saved(auth: SftpAuth) -> StorageConfig {
        StorageConfig::Sftp(SftpConfig {
            host: "nas.local".into(),
            port: 22,
            user: "clip".into(),
            auth,
            path: "/data/clip".into(),
            host_key_sha256: "SHA256:abc".into(),
        })
    }

    fn cfg(storage: StorageConfig) -> GroupConfig {
        GroupConfig {
            version: 3,
            storage,
            file_ttl_hours: 48,
            access_exposed: false,
        }
    }

    #[test]
    fn form_has_no_secrets() {
        let (form, saved) = to_form(&cfg(s3_saved()));
        assert_eq!(form.kind, "s3");
        assert!(form.s3.secret_key.is_empty() && saved.s3_secret);
        assert_eq!(form.file_ttl_hours, 48);
        let (form, saved) = to_form(&cfg(sftp_saved(SftpAuth::Key {
            private_key: "KEY".into(),
            passphrase: Some("pp".into()),
        })));
        assert_eq!(form.sftp.auth, "key");
        assert!(form.sftp.private_key.is_empty() && form.sftp.passphrase.is_empty());
        assert!(saved.sftp_key && saved.sftp_passphrase && !saved.sftp_password);
        let json = serde_json::to_string(&form).unwrap();
        assert!(!json.contains("KEY") && !json.contains("pp\""));
        // Без настроек — сервер и разумные значения по умолчанию.
        let (form, _) = to_form(&GroupConfig::default());
        assert_eq!(
            (form.kind.as_str(), form.sftp.port, form.s3.path_style),
            ("server", 22, true)
        );
    }

    #[test]
    fn empty_secret_kept_only_for_same_store() {
        let saved = s3_saved();
        let (mut form, _) = to_form(&cfg(saved.clone()));
        assert_eq!(from_form(&form, &saved).unwrap(), saved);
        // Другой адрес — сохранённый секрет туда не уходит.
        form.s3.endpoint = "https://evil.example.com".into();
        assert!(from_form(&form, &saved).is_err());
        form.s3.secret_key = "новый".into();
        match from_form(&form, &saved).unwrap() {
            StorageConfig::S3(c) => assert_eq!(c.secret_key, "новый"),
            _ => unreachable!(),
        }
    }

    #[test]
    fn sftp_secrets_and_paths() {
        let saved = sftp_saved(SftpAuth::Password("пароль".into()));
        let (mut form, _) = to_form(&cfg(saved.clone()));
        assert_eq!(from_form(&form, &saved).unwrap(), saved);
        form.sftp.user = "other".into();
        assert!(from_form(&form, &saved).is_err(), "другой пользователь");
        form.sftp.user = "clip".into();
        form.sftp.auth = "key".into();
        assert!(from_form(&form, &saved).is_err(), "ключа нет");
        form.sftp.private_key = "  KEY  ".into();
        form.sftp.path = " /data/clip/ ".into();
        match from_form(&form, &saved).unwrap() {
            StorageConfig::Sftp(c) => {
                assert_eq!(c.path, "/data/clip");
                assert!(
                    matches!(c.auth, SftpAuth::Key { private_key, passphrase: None } if private_key == "KEY\n")
                );
            }
            _ => unreachable!(),
        }
        form.sftp.port = 0;
        assert!(from_form(&form, &saved).is_err());
        assert_eq!(clean_path("clip/"), "clip");
        assert_eq!(clean_prefix(" /a/b/ "), "a/b");
    }

    #[test]
    fn saved_secret_not_sent_to_server_with_other_host_key() {
        let saved = sftp_saved(SftpAuth::Password("пароль".into()));
        let (mut form, _) = to_form(&cfg(saved.clone()));
        // Тот же отпечаток в другом написании — тот же сервер.
        form.sftp.host_key_sha256 = "sha256:abc==".into();
        assert!(from_form(&form, &saved).is_ok());
        // «Сбросить отпечаток»: запрос отпечатка без входа, секрет можно.
        form.sftp.host_key_sha256 = String::new();
        assert!(from_form(&form, &saved).is_ok());
        // Подтвердили другой отпечаток — сохранённый пароль туда не уходит.
        form.sftp.host_key_sha256 = "SHA256:evil".into();
        let e = from_form(&form, &saved).unwrap_err();
        assert!(e.contains("Ключ сервера изменился"), "{e}");
        form.sftp.password = "новый".into();
        match from_form(&form, &saved).unwrap() {
            StorageConfig::Sftp(c) => {
                assert!(matches!(c.auth, SftpAuth::Password(p) if p == "новый"));
                assert_eq!(c.host_key_sha256, "SHA256:evil");
            }
            _ => unreachable!(),
        }
        // Вход по ключу — так же.
        let saved = sftp_saved(SftpAuth::Key {
            private_key: "KEY".into(),
            passphrase: None,
        });
        let (mut form, _) = to_form(&cfg(saved.clone()));
        assert!(from_form(&form, &saved).is_ok());
        form.sftp.host_key_sha256 = "SHA256:evil".into();
        assert!(from_form(&form, &saved).is_err());
    }

    #[test]
    fn host_key_mismatch_is_separate() {
        let r = probe_dto(Err(StoreError::HostKey {
            expected: "SHA256:a".into(),
            got: "SHA256:b".into(),
        }));
        assert!(matches!(r, ProbeDto::HostKeyMismatch { .. }));
        let json = serde_json::to_string(&probe_dto(Err(StoreError::Timeout))).unwrap();
        assert!(json.contains("\"status\":\"error\""));
    }
}
