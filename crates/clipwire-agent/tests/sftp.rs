//! SFTP-хранилище против настоящего SSH-сервера в этом же процессе (`russh`
//! сервер + `russh-sftp` поверх файловой системы в памяти): ключ хоста, вход
//! паролем и ключом, «Проверить подключение», загрузка и скачивание файла,
//! чистка. Семантика как у OpenSSH `sftp-server` v3: переименование поверх
//! существующего файла — ошибка, папки создаются только при наличии родителя.

use std::collections::{BTreeMap, HashMap};
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use clipwire_agent::blobs::{UploadJob, file_stamp};
use clipwire_agent::storage::external::{self, Probe, Waits};
use clipwire_agent::storage::sftp::SftpStore;
use clipwire_agent::storage::{ObjectStore, StoreError};
use clipwire_core::storage::{SftpAuth, SftpConfig, StorageConfig, group_dir};
use clipwire_core::{BlobId, GroupKey};
use russh::keys::ssh_key::LineEnding;
use russh::keys::ssh_key::private::Ed25519Keypair;
use russh::keys::{HashAlg, PrivateKey};
use russh::server::{Auth, ChannelOpenHandle, Msg, Session, VerifiedPublicKey};
use russh::{Channel, ChannelId};
use russh_sftp::protocol::{
    Attrs, Data, File, FileAttributes, Handle, Name, OpenFlags, Status, StatusCode,
};

const USER: &str = "clip";
const PASSWORD: &str = "пароль-для-теста";

// ------------------------------------------------------------ файловая система

#[derive(Default)]
struct Fs {
    /// Путь → содержимое и mtime (секунды).
    files: BTreeMap<String, (Vec<u8>, u32)>,
    dirs: std::collections::BTreeSet<String>,
}

fn now_secs() -> u32 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as u32
}

fn parent(p: &str) -> String {
    match p.rfind('/') {
        Some(0) => "/".into(),
        Some(i) => p[..i].into(),
        None => "/".into(),
    }
}

impl Fs {
    fn attrs_of(&self, p: &str) -> Option<FileAttributes> {
        let mut a = FileAttributes::empty();
        if let Some((d, m)) = self.files.get(p) {
            a.size = Some(d.len() as u64);
            a.mtime = Some(*m);
            a.set_regular(true);
            a.permissions = Some(a.permissions.unwrap_or(0) | 0o644);
            Some(a)
        } else if self.dirs.contains(p) {
            a.size = Some(0);
            a.mtime = Some(now_secs());
            a.set_dir(true);
            Some(a)
        } else {
            None
        }
    }

    fn children(&self, dir: &str) -> Vec<String> {
        let prefix = if dir == "/" {
            "/".to_string()
        } else {
            format!("{dir}/")
        };
        let direct = |p: &&String| p.starts_with(&prefix) && !p[prefix.len()..].contains('/');
        self.files
            .keys()
            .filter(direct)
            .chain(self.dirs.iter().filter(direct).filter(|d| *d != "/"))
            .cloned()
            .collect()
    }
}

enum H {
    File(String),
    Dir(String, bool),
}

struct Sftp {
    fs: Arc<Mutex<Fs>>,
    handles: HashMap<String, H>,
    next: u64,
}

fn ok(id: u32) -> Status {
    Status {
        id,
        status_code: StatusCode::Ok,
        error_message: "Ok".into(),
        language_tag: "en-US".into(),
    }
}

impl russh_sftp::server::Handler for Sftp {
    type Error = StatusCode;

    fn unimplemented(&self) -> Self::Error {
        StatusCode::OpUnsupported
    }

    async fn open(
        &mut self,
        id: u32,
        filename: String,
        pflags: OpenFlags,
        _attrs: FileAttributes,
    ) -> Result<Handle, Self::Error> {
        {
            let mut fs = self.fs.lock().unwrap();
            let exists = fs.files.contains_key(&filename);
            if pflags.contains(OpenFlags::CREATE) {
                if !fs.dirs.contains(&parent(&filename)) {
                    return Err(StatusCode::NoSuchFile);
                }
                if !exists || pflags.contains(OpenFlags::TRUNCATE) {
                    fs.files.insert(filename.clone(), (Vec::new(), now_secs()));
                }
            } else if !exists {
                return Err(StatusCode::NoSuchFile);
            }
        }
        self.next += 1;
        let h = format!("h{}", self.next);
        self.handles.insert(h.clone(), H::File(filename));
        Ok(Handle { id, handle: h })
    }

    async fn close(&mut self, id: u32, handle: String) -> Result<Status, Self::Error> {
        self.handles.remove(&handle);
        Ok(ok(id))
    }

    async fn read(
        &mut self,
        id: u32,
        handle: String,
        offset: u64,
        len: u32,
    ) -> Result<Data, Self::Error> {
        let Some(H::File(p)) = self.handles.get(&handle) else {
            return Err(StatusCode::Failure);
        };
        let fs = self.fs.lock().unwrap();
        let (d, _) = fs.files.get(p).ok_or(StatusCode::NoSuchFile)?;
        let off = offset as usize;
        if off >= d.len() {
            return Err(StatusCode::Eof);
        }
        let end = (off + len as usize).min(d.len());
        Ok(Data {
            id,
            data: d[off..end].to_vec(),
        })
    }

    async fn write(
        &mut self,
        id: u32,
        handle: String,
        offset: u64,
        data: Vec<u8>,
    ) -> Result<Status, Self::Error> {
        let Some(H::File(p)) = self.handles.get(&handle) else {
            return Err(StatusCode::Failure);
        };
        let mut fs = self.fs.lock().unwrap();
        let (d, m) = fs.files.get_mut(p).ok_or(StatusCode::NoSuchFile)?;
        let off = offset as usize;
        if d.len() < off + data.len() {
            d.resize(off + data.len(), 0);
        }
        d[off..off + data.len()].copy_from_slice(&data);
        *m = now_secs();
        Ok(ok(id))
    }

    async fn lstat(&mut self, id: u32, path: String) -> Result<Attrs, Self::Error> {
        self.stat(id, path).await
    }

    async fn stat(&mut self, id: u32, path: String) -> Result<Attrs, Self::Error> {
        let attrs = self
            .fs
            .lock()
            .unwrap()
            .attrs_of(&path)
            .ok_or(StatusCode::NoSuchFile)?;
        Ok(Attrs { id, attrs })
    }

    async fn fstat(&mut self, id: u32, handle: String) -> Result<Attrs, Self::Error> {
        let p = match self.handles.get(&handle) {
            Some(H::File(p) | H::Dir(p, _)) => p.clone(),
            None => return Err(StatusCode::Failure),
        };
        self.stat(id, p).await
    }

    async fn setstat(
        &mut self,
        id: u32,
        _path: String,
        _attrs: FileAttributes,
    ) -> Result<Status, Self::Error> {
        Ok(ok(id))
    }

    async fn fsetstat(
        &mut self,
        id: u32,
        _handle: String,
        _attrs: FileAttributes,
    ) -> Result<Status, Self::Error> {
        Ok(ok(id))
    }

    async fn opendir(&mut self, id: u32, path: String) -> Result<Handle, Self::Error> {
        if !self.fs.lock().unwrap().dirs.contains(&path) {
            return Err(StatusCode::NoSuchFile);
        }
        self.next += 1;
        let h = format!("d{}", self.next);
        self.handles.insert(h.clone(), H::Dir(path, false));
        Ok(Handle { id, handle: h })
    }

    async fn readdir(&mut self, id: u32, handle: String) -> Result<Name, Self::Error> {
        let Some(H::Dir(p, done)) = self.handles.get_mut(&handle) else {
            return Err(StatusCode::Failure);
        };
        if *done {
            return Err(StatusCode::Eof);
        }
        *done = true;
        let fs = self.fs.lock().unwrap();
        let files = fs
            .children(p)
            .into_iter()
            .map(|c| {
                let name = c.rsplit('/').next().unwrap().to_string();
                File::new(name, fs.attrs_of(&c).unwrap())
            })
            .collect();
        Ok(Name { id, files })
    }

    async fn remove(&mut self, id: u32, filename: String) -> Result<Status, Self::Error> {
        match self.fs.lock().unwrap().files.remove(&filename) {
            Some(_) => Ok(ok(id)),
            None => Err(StatusCode::NoSuchFile),
        }
    }

    async fn mkdir(
        &mut self,
        id: u32,
        path: String,
        _attrs: FileAttributes,
    ) -> Result<Status, Self::Error> {
        let mut fs = self.fs.lock().unwrap();
        if !fs.dirs.contains(&parent(&path)) {
            return Err(StatusCode::NoSuchFile);
        }
        if fs.dirs.contains(&path) || fs.files.contains_key(&path) {
            return Err(StatusCode::Failure);
        }
        fs.dirs.insert(path);
        Ok(ok(id))
    }

    async fn rmdir(&mut self, id: u32, path: String) -> Result<Status, Self::Error> {
        let mut fs = self.fs.lock().unwrap();
        if !fs.dirs.contains(&path) {
            return Err(StatusCode::NoSuchFile);
        }
        if !fs.children(&path).is_empty() {
            return Err(StatusCode::Failure);
        }
        fs.dirs.remove(&path);
        Ok(ok(id))
    }

    async fn realpath(&mut self, id: u32, path: String) -> Result<Name, Self::Error> {
        Ok(Name {
            id,
            files: vec![File::dummy(path)],
        })
    }

    async fn rename(
        &mut self,
        id: u32,
        oldpath: String,
        newpath: String,
    ) -> Result<Status, Self::Error> {
        let mut fs = self.fs.lock().unwrap();
        // Как sftp-server v3: поверх существующего — отказ.
        if fs.files.contains_key(&newpath) {
            return Err(StatusCode::Failure);
        }
        let f = fs.files.remove(&oldpath).ok_or(StatusCode::NoSuchFile)?;
        fs.files.insert(newpath, f);
        Ok(ok(id))
    }
}

// ----------------------------------------------------------------- SSH-сервер

#[derive(Clone)]
struct Ssh {
    fs: Arc<Mutex<Fs>>,
    client_key: russh::keys::PublicKey,
    /// Сколько было попыток входа (любым способом).
    auths: Arc<AtomicUsize>,
    channels: Arc<tokio::sync::Mutex<HashMap<ChannelId, Channel<Msg>>>>,
}

impl russh::server::Handler for Ssh {
    type Error = russh::Error;

    async fn auth_password(&mut self, user: &str, password: &str) -> Result<Auth, Self::Error> {
        self.auths.fetch_add(1, Ordering::SeqCst);
        Ok(if user == USER && password == PASSWORD {
            Auth::Accept
        } else {
            Auth::reject()
        })
    }

    async fn auth_publickey(
        &mut self,
        user: &str,
        key: VerifiedPublicKey<'_>,
    ) -> Result<Auth, Self::Error> {
        self.auths.fetch_add(1, Ordering::SeqCst);
        Ok(
            if user == USER && key.public_key.key_data() == self.client_key.key_data() {
                Auth::Accept
            } else {
                Auth::reject()
            },
        )
    }

    async fn channel_open_session(
        &mut self,
        channel: Channel<Msg>,
        reply: ChannelOpenHandle,
        _session: &mut Session,
    ) -> Result<(), Self::Error> {
        self.channels.lock().await.insert(channel.id(), channel);
        reply.accept().await;
        Ok(())
    }

    async fn subsystem_request(
        &mut self,
        channel_id: ChannelId,
        name: &str,
        session: &mut Session,
    ) -> Result<(), Self::Error> {
        let channel = self.channels.lock().await.remove(&channel_id);
        match (name, channel) {
            ("sftp", Some(channel)) => {
                session.channel_success(channel_id)?;
                let h = Sftp {
                    fs: self.fs.clone(),
                    handles: HashMap::new(),
                    next: 0,
                };
                tokio::spawn(russh_sftp::server::run(channel.into_stream(), h));
            }
            _ => session.channel_failure(channel_id)?,
        }
        Ok(())
    }
}

struct Server {
    addr: SocketAddr,
    fingerprint: String,
    fs: Arc<Mutex<Fs>>,
    auths: Arc<AtomicUsize>,
    client_key: PrivateKey,
}

fn key_from_seed(seed: u8) -> PrivateKey {
    PrivateKey::from(Ed25519Keypair::from_seed(&[seed; 32]))
}

async fn start() -> Server {
    let host_key = key_from_seed(1);
    let client_key = key_from_seed(2);
    let fingerprint = host_key
        .public_key()
        .fingerprint(HashAlg::Sha256)
        .to_string();
    let fs = Arc::new(Mutex::new(Fs::default()));
    fs.lock().unwrap().dirs.insert("/".into());
    let auths = Arc::new(AtomicUsize::new(0));
    let config = Arc::new(russh::server::Config {
        auth_rejection_time: Duration::from_millis(10),
        auth_rejection_time_initial: Some(Duration::from_millis(0)),
        keys: vec![host_key],
        ..Default::default()
    });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let handler = Ssh {
        fs: fs.clone(),
        client_key: client_key.public_key().clone(),
        auths: auths.clone(),
        channels: Arc::default(),
    };
    tokio::spawn(async move {
        loop {
            let Ok((sock, _)) = listener.accept().await else {
                return;
            };
            let (config, h) = (config.clone(), handler.clone());
            tokio::spawn(async move {
                if let Ok(s) = russh::server::run_stream(config, sock, h).await {
                    let _ = s.await;
                }
            });
        }
    });
    Server {
        addr,
        fingerprint,
        fs,
        auths,
        client_key,
    }
}

fn cfg(s: &Server, auth: SftpAuth, host_key: &str) -> SftpConfig {
    SftpConfig {
        host: "127.0.0.1".into(),
        port: s.addr.port(),
        user: USER.into(),
        auth,
        path: "/srv/clip".into(),
        host_key_sha256: host_key.into(),
    }
}

fn password() -> SftpAuth {
    SftpAuth::Password(PASSWORD.into())
}

// ---------------------------------------------------------------------- тесты

#[tokio::test(flavor = "multi_thread")]
async fn probe_checks_host_key_and_login() {
    let s = start().await;
    let g = GroupKey([1; 32]).group_id();
    // Корень `/srv` есть, `/srv/clip` создаст само хранилище.
    s.fs.lock().unwrap().dirs.insert("/srv".into());

    // Первая настройка: только отпечаток, без входа (пароль неизвестному
    // хосту не уходит).
    let first = external::probe(&StorageConfig::Sftp(cfg(&s, password(), "")), &g)
        .await
        .unwrap();
    assert_eq!(
        first,
        Probe::ConfirmHostKey {
            host_key: s.fingerprint.clone()
        }
    );
    assert_eq!(s.auths.load(Ordering::SeqCst), 0);

    // Подтверждённый отпечаток и пароль — запись, чтение, удаление.
    let ok = external::probe(
        &StorageConfig::Sftp(cfg(&s, password(), &s.fingerprint)),
        &g,
    )
    .await
    .unwrap();
    assert_eq!(
        ok,
        Probe::Ok {
            host_key: Some(s.fingerprint.clone()),
            insecure_http: false
        }
    );
    // Пробный объект удалён, папка группы осталась пустой.
    assert!(s.fs.lock().unwrap().files.is_empty());

    // Другой ключ хоста — отказ с отпечатком, который прислал сервер.
    let before = s.auths.load(Ordering::SeqCst);
    let wrong = "SHA256:AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA";
    match external::probe(&StorageConfig::Sftp(cfg(&s, password(), wrong)), &g).await {
        Err(StoreError::HostKey { got, .. }) => assert_eq!(got, s.fingerprint),
        other => panic!("ждали HostKey, пришло {other:?}"),
    }
    assert_eq!(s.auths.load(Ordering::SeqCst), before, "вход не пробовался");

    // Неверный пароль.
    let bad = SftpAuth::Password("не тот".into());
    assert!(matches!(
        external::probe(&StorageConfig::Sftp(cfg(&s, bad, &s.fingerprint)), &g).await,
        Err(StoreError::Auth(_))
    ));

    // Вход ключом OpenSSH.
    let pem = s.client_key.to_openssh(LineEnding::LF).unwrap().to_string();
    let key = SftpAuth::Key {
        private_key: pem,
        passphrase: None,
    };
    assert!(matches!(
        external::probe(&StorageConfig::Sftp(cfg(&s, key, &s.fingerprint)), &g).await,
        Ok(Probe::Ok { .. })
    ));
    // Чужой ключ сервер не принимает.
    let other = key_from_seed(9)
        .to_openssh(LineEnding::LF)
        .unwrap()
        .to_string();
    let other = SftpAuth::Key {
        private_key: other,
        passphrase: None,
    };
    assert!(matches!(
        external::probe(&StorageConfig::Sftp(cfg(&s, other, &s.fingerprint)), &g).await,
        Err(StoreError::Auth(_))
    ));
}

fn tmp(tag: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("clipwire-sftp-{tag}-{}", uuid::Uuid::now_v7()));
    std::fs::create_dir_all(&d).unwrap();
    d
}

#[tokio::test(flavor = "multi_thread")]
async fn file_roundtrip_overwrite_list_and_cleanup() {
    let s = start().await;
    s.fs.lock().unwrap().dirs.insert("/srv".into());
    let store = SftpStore::new(cfg(&s, password(), &s.fingerprint)).unwrap();
    let key = GroupKey([3; 32]);
    let g = key.group_id();

    let dir = tmp("rt");
    let src = dir.join("src.bin");
    let data: Vec<u8> = (0..(2 * clipwire_core::blob::CHUNK + 99))
        .map(|i| (i % 249) as u8)
        .collect();
    std::fs::write(&src, &data).unwrap();
    let (size, modified_ms) = file_stamp(&src).unwrap();
    let blob = BlobId([5; 16]);
    let job = UploadJob {
        blob,
        path: src.to_string_lossy().into_owned(),
        size,
        modified_ms,
        group: String::new(),
        queued_ms: 0,
        outbox: false,
        external: Some(1),
    };
    // meta пишется дважды (начало и конец): второй раз — поверх
    // существующего файла, чего SFTP v3 сам не умеет.
    external::upload(&store, &key, &job, |_| {}).await.unwrap();
    let meta = external::get_meta(&store, &key, blob)
        .await
        .unwrap()
        .unwrap();
    assert!(meta.complete);
    let names: Vec<String> = store
        .list(&group_dir(&g))
        .await
        .unwrap()
        .into_iter()
        .map(|o| o.name)
        .collect();
    assert_eq!(names.len(), 4, "{names:?}");
    assert!(names.iter().all(|n| n.starts_with(&group_dir(&g))));
    // Ни одного временного файла не осталось.
    assert!(
        s.fs.lock()
            .unwrap()
            .files
            .keys()
            .all(|k| !k.contains(".tmp-"))
    );

    let dest = dir.join("out").join("f.bin");
    let got = external::download(&store, &key, blob, size, &dest, Waits::default(), |_| {})
        .await
        .unwrap();
    assert_eq!(std::fs::read(got).unwrap(), data);

    // Чистка по сроку удаляет файлы и пустые папки.
    let later = clipwire_agent::history::now_ms() as u64 + 48 * 3600 * 1000;
    assert_eq!(
        external::cleanup_at(&store, &g, 24, later).await.unwrap(),
        1
    );
    let fs = s.fs.lock().unwrap();
    assert!(fs.files.is_empty());
    assert!(
        !fs.dirs
            .iter()
            .any(|d| d.contains(&format!("{}/", group_dir(&g)))),
        "{:?}",
        fs.dirs
    );
    let _ = std::fs::remove_dir_all(&dir);
}
