//! Локальный канал между мостом MCP (`clipwire-desktop --mcp`) и
//! приложением: именованный канал на Windows, unix-сокет на macOS и Linux.
//!
//! Где канал, мост узнаёт из файла `<папка данных>/mcp/endpoint` (одна
//! строка). Кадры — postcard с длиной из `clipwire_core::proto::io`, один
//! запрос — один ответ, по очереди.
//!
//! Защита: на Windows имя канала случайное на каждый запуск, первый экземпляр
//! создаётся с `first_pipe_instance` (чужой процесс не займёт имя раньше), а
//! доступ по DACL только у текущего пользователя и SYSTEM; удалённые клиенты
//! отклоняются. На unix сокет лежит в каталоге 0700 и сам с правами 0600.

use std::future::Future;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use clipwire_core::ai::{AiError, AiRequest, AiResponse};
use clipwire_core::proto::MAX_FRAME;
use clipwire_core::proto::io::{IoError, read_msg_limited, write_msg};
use tokio::io::{AsyncRead, AsyncWrite};

/// Каталог канала внутри папки данных.
pub fn mcp_dir(data_dir: &Path) -> PathBuf {
    data_dir.join("mcp")
}

/// Файл, в котором записан адрес канала.
pub fn endpoint_file(data_dir: &Path) -> PathBuf {
    mcp_dir(data_dir).join("endpoint")
}

/// Адрес канала из файла (`None` — файла нет или он пустой).
pub fn read_endpoint(data_dir: &Path) -> Option<String> {
    let s = std::fs::read_to_string(endpoint_file(data_dir)).ok()?;
    let s = s.trim();
    (!s.is_empty()).then(|| s.to_string())
}

/// Одно соединение: запросы по одному, по порядку.
pub trait Session: Send + 'static {
    fn handle(&mut self, req: AiRequest) -> impl Future<Output = AiResponse> + Send;
}

/// Обслужить соединение до его закрытия.
pub async fn serve<S: Session, T: AsyncRead + AsyncWrite + Unpin>(mut io: T, mut session: S) {
    loop {
        let req = match read_msg_limited::<_, AiRequest>(&mut io, MAX_FRAME).await {
            Ok(Some(r)) => r,
            Ok(None) => return,
            // Кадр прочитан целиком, но не разобрался (мост новее): поток не
            // сбился, можно ответить и ждать дальше.
            Err(IoError::Frame(clipwire_core::proto::FrameError::Decode(e))) => {
                tracing::debug!("запрос моста MCP не разобрался: {e}");
                let resp = AiResponse::Error {
                    code: AiError::Bad,
                    message: "Clipwire did not understand the request; update Clipwire".into(),
                };
                if write_msg(&mut io, &resp).await.is_err() {
                    return;
                }
                continue;
            }
            Err(e) => {
                tracing::debug!("соединение моста MCP закрыто: {e}");
                return;
            }
        };
        // Пока запрос обрабатывается (окно согласия ждёт человека до 120 с),
        // следим за соединением: мост закрывает его, когда ИИ-клиент бросил
        // вызов. Тогда бросаем и обработку, и вопрос «положить в буфер»:
        // иначе человек мог бы потом согласиться на запись, которую уже никто
        // не ждёт. Вопрос про нового клиента живёт дальше своей задачей
        // (`Conn::admit`): его ответ запомнится. Запросы идут по одному, так что до ответа мост ничего не
        // шлёт: любой байт здесь — закрытие или нарушение протокола.
        let resp = {
            let handling = session.handle(req);
            tokio::pin!(handling);
            let mut probe = [0u8; 1];
            tokio::select! {
                resp = &mut handling => resp,
                r = tokio::io::AsyncReadExt::read(&mut io, &mut probe) => {
                    match r {
                        Ok(0) | Err(_) => tracing::debug!("мост MCP закрыл соединение, не дождавшись ответа"),
                        Ok(_) => tracing::debug!("мост MCP прислал данные до ответа на запрос"),
                    }
                    return;
                }
            }
        };
        match write_msg(&mut io, &resp).await {
            Ok(()) => {}
            // Ответ не влез в кадр: сказать об этом вместо ответа.
            Err(IoError::Frame(e)) => {
                tracing::warn!("ответ мосту MCP не отправился: {e}");
                let resp = AiResponse::Error {
                    code: AiError::Bad,
                    message: "the answer is too large for the Clipwire channel".into(),
                };
                if write_msg(&mut io, &resp).await.is_err() {
                    return;
                }
            }
            Err(e) => {
                tracing::debug!("соединение моста MCP оборвалось: {e}");
                return;
            }
        }
    }
}

/// Занятый канал: принимает соединения, пока жив.
pub struct Bound {
    endpoint: String,
    inner: imp::Listener,
}

impl Bound {
    pub fn endpoint(&self) -> &str {
        &self.endpoint
    }

    /// Принимать соединения и обслуживать каждое своей задачей. `make`
    /// получает номер процесса на том конце (`None` — система не сказала).
    /// Не возвращается, пока канал работает; остановка этой задачи закрывает
    /// и все соединения (как выход приложения).
    pub async fn run<S: Session>(mut self, make: impl Fn(Option<u32>) -> S + Send + 'static) {
        let mut conns = tokio::task::JoinSet::new();
        loop {
            tokio::select! {
                accepted = self.inner.accept() => match accepted {
                    Ok(conn) => {
                        let s = make(imp::peer_pid(&conn));
                        conns.spawn(async move { serve(conn, s).await });
                    }
                    Err(e) => {
                        tracing::warn!("канал MCP не принял соединение: {e:#}");
                        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
                    }
                },
                // Закончившиеся соединения убираются из набора.
                Some(_) = conns.join_next(), if !conns.is_empty() => {}
            }
        }
    }
}

/// Убрать файл адреса и сокет, если файл всё ещё указывает на `endpoint`.
pub fn cleanup(data_dir: &Path, endpoint: &str) {
    if read_endpoint(data_dir).as_deref() == Some(endpoint) {
        let _ = std::fs::remove_file(endpoint_file(data_dir));
        #[cfg(unix)]
        let _ = std::fs::remove_file(endpoint);
    }
}

/// Занять канал для папки данных `data_dir` и записать его адрес. `None` —
/// канал уже держит другой запущенный экземпляр с той же папкой данных.
pub async fn bind(data_dir: &Path) -> Result<Option<Bound>> {
    let dir = mcp_dir(data_dir);
    clipwire_agent::state::ensure_private_dir(&dir)?;
    if let Some(old) = read_endpoint(data_dir)
        && imp::alive(&old).await
    {
        return Ok(None);
    }
    let (endpoint, inner) = imp::listen(data_dir)?;
    clipwire_agent::state::write_atomic(
        &endpoint_file(data_dir),
        format!("{endpoint}\n").as_bytes(),
    )
    .context("адрес канала MCP не записался")?;
    Ok(Some(Bound { endpoint, inner }))
}

#[cfg(windows)]
pub use imp::pipe_server_is_current_user;

/// Подключиться к каналу как мост (для проверок).
#[cfg(test)]
pub async fn connect(endpoint: &str) -> Result<imp::Client> {
    imp::connect(endpoint).await
}

#[cfg(windows)]
mod imp {
    use std::ffi::c_void;
    use std::path::Path;

    use anyhow::{Context, Result};
    use tokio::net::windows::named_pipe::{ClientOptions, NamedPipeServer, ServerOptions};
    use windows::Win32::Foundation::{CloseHandle, HANDLE, HLOCAL, LocalFree};
    use windows::Win32::Security::Authorization::{
        ConvertSidToStringSidW, ConvertStringSecurityDescriptorToSecurityDescriptorW,
        SDDL_REVISION_1,
    };
    use windows::Win32::Security::{
        EqualSid, GetTokenInformation, PSECURITY_DESCRIPTOR, SECURITY_ATTRIBUTES, TOKEN_QUERY,
        TOKEN_USER, TokenUser,
    };
    use windows::Win32::System::Pipes::{GetNamedPipeClientProcessId, GetNamedPipeServerProcessId};
    use windows::Win32::System::Threading::{
        GetCurrentProcess, OpenProcess, OpenProcessToken, PROCESS_QUERY_LIMITED_INFORMATION,
    };
    use windows::core::{PCWSTR, PWSTR};

    #[cfg(test)]
    pub type Client = tokio::net::windows::named_pipe::NamedPipeClient;

    /// ERROR_PIPE_BUSY: канал есть, но все его экземпляры заняты.
    const PIPE_BUSY: i32 = 231;
    /// Сколько раз по 50 мс ждать свободного экземпляра в `alive`.
    const BUSY_TRIES: usize = 20;

    /// Описатель безопасности «только текущий пользователь и SYSTEM».
    struct Security {
        sd: PSECURITY_DESCRIPTOR,
        attrs: SECURITY_ATTRIBUTES,
    }

    // SAFETY: описатель — память LocalAlloc, которой владеет только эта
    // структура; читается системой при создании экземпляров канала.
    unsafe impl Send for Security {}
    unsafe impl Sync for Security {}

    impl Security {
        fn current_user() -> Result<Box<Self>> {
            let sid = user_sid()?;
            // P — защищённый DACL (без наследования), GA — полный доступ.
            let sddl: Vec<u16> = format!("D:P(A;;GA;;;{sid})(A;;GA;;;SY)")
                .encode_utf16()
                .chain(Some(0))
                .collect();
            let mut sd = PSECURITY_DESCRIPTOR::default();
            // SAFETY: строка с нулём на конце живёт до конца вызова.
            unsafe {
                ConvertStringSecurityDescriptorToSecurityDescriptorW(
                    PCWSTR(sddl.as_ptr()),
                    SDDL_REVISION_1,
                    &mut sd,
                    None,
                )
            }
            .context("описатель безопасности канала MCP не собрался")?;
            let mut b = Box::new(Self {
                sd,
                attrs: SECURITY_ATTRIBUTES {
                    nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
                    lpSecurityDescriptor: std::ptr::null_mut(),
                    bInheritHandle: false.into(),
                },
            });
            b.attrs.lpSecurityDescriptor = b.sd.0;
            Ok(b)
        }

        fn ptr(&self) -> *mut c_void {
            &self.attrs as *const SECURITY_ATTRIBUTES as *mut c_void
        }
    }

    impl Drop for Security {
        fn drop(&mut self) {
            // SAFETY: память выделила ConvertStringSecurityDescriptor…
            unsafe {
                let _ = LocalFree(Some(HLOCAL(self.sd.0)));
            }
        }
    }

    /// `TOKEN_USER` процесса: буфер, в начале которого лежит структура
    /// (из `u64`, чтобы указатель внутри был выровнен).
    ///
    /// # Safety
    /// `process` — открытый описатель процесса с правом
    /// `PROCESS_QUERY_LIMITED_INFORMATION` или псевдоописатель текущего.
    unsafe fn process_user(process: HANDLE) -> Result<Vec<u64>> {
        // SAFETY: обычная последовательность WinAPI; буфер нужного размера,
        // маркер закрывается.
        unsafe {
            let mut token = HANDLE::default();
            OpenProcessToken(process, TOKEN_QUERY, &mut token)
                .context("маркер процесса не открылся")?;
            let mut len = 0u32;
            let _ = GetTokenInformation(token, TokenUser, None, 0, &mut len);
            let mut buf = vec![0u64; (len as usize).div_ceil(8).max(1)];
            let res = GetTokenInformation(
                token,
                TokenUser,
                Some(buf.as_mut_ptr() as *mut c_void),
                len,
                &mut len,
            );
            let _ = CloseHandle(token);
            res.context("пользователь процесса не прочитался")?;
            Ok(buf)
        }
    }

    /// Канал держит процесс того же пользователя, что и мы. Имена каналов
    /// видны всем пользователям машины: после падения Clipwire другой
    /// пользователь мог занять имя из старого файла адреса. Не удалось
    /// проверить — считаем, что чужой.
    pub fn pipe_server_is_current_user(pipe: &impl std::os::windows::io::AsRawHandle) -> bool {
        let pipe = HANDLE(pipe.as_raw_handle());
        // SAFETY: `pipe` — открытый клиентский конец канала; описатель
        // процесса закрывается, буферы TOKEN_USER живут до сравнения SID.
        unsafe {
            let mut pid = 0u32;
            if GetNamedPipeServerProcessId(pipe, &mut pid).is_err() {
                return false;
            }
            let Ok(process) = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) else {
                return false;
            };
            let theirs = process_user(process);
            let _ = CloseHandle(process);
            let (Ok(theirs), Ok(mine)) = (theirs, process_user(GetCurrentProcess())) else {
                return false;
            };
            let a = &*(theirs.as_ptr() as *const TOKEN_USER);
            let b = &*(mine.as_ptr() as *const TOKEN_USER);
            EqualSid(a.User.Sid, b.User.Sid).is_ok()
        }
    }

    /// Процесс, который подключился к экземпляру канала.
    pub fn peer_pid(pipe: &NamedPipeServer) -> Option<u32> {
        use std::os::windows::io::AsRawHandle;
        let mut pid = 0u32;
        // SAFETY: `pipe` — подключённый серверный экземпляр канала.
        unsafe { GetNamedPipeClientProcessId(HANDLE(pipe.as_raw_handle()), &mut pid) }.ok()?;
        (pid != 0).then_some(pid)
    }

    /// SID текущего пользователя строкой (`S-1-5-21-…`).
    fn user_sid() -> Result<String> {
        // SAFETY: псевдоописатель текущего процесса; буфер TOKEN_USER живёт
        // до конца блока, строку SID освобождает LocalFree.
        unsafe {
            let buf = process_user(GetCurrentProcess())?;
            let user = &*(buf.as_ptr() as *const TOKEN_USER);
            let mut s = PWSTR::null();
            ConvertSidToStringSidW(user.User.Sid, &mut s).context("SID не прочитался")?;
            let out = s.to_string();
            let _ = LocalFree(Some(HLOCAL(s.0 as *mut c_void)));
            out.context("SID не прочитался")
        }
    }

    pub struct Listener {
        name: String,
        security: Box<Security>,
        next: NamedPipeServer,
    }

    fn create(name: &str, security: &Security, first: bool) -> std::io::Result<NamedPipeServer> {
        let mut opts = ServerOptions::new();
        opts.first_pipe_instance(first).reject_remote_clients(true);
        // SAFETY: `security` живёт дольше вызова; система копирует описатель.
        unsafe { opts.create_with_security_attributes_raw(name, security.ptr()) }
    }

    pub fn listen(_data_dir: &Path) -> Result<(String, Listener)> {
        let key = clipwire_agent::state::random_key()?;
        let hex: String = key[..16].iter().map(|b| format!("{b:02x}")).collect();
        let name = format!(r"\\.\pipe\clipwire-mcp-{hex}");
        let security = Security::current_user()?;
        let next = create(&name, &security, true).context("канал MCP не создался")?;
        Ok((
            name.clone(),
            Listener {
                name,
                security,
                next,
            },
        ))
    }

    impl Listener {
        pub async fn accept(&mut self) -> Result<NamedPipeServer> {
            self.next
                .connect()
                .await
                .context("подключение к каналу MCP")?;
            // Следующий экземпляр — до того, как отдать этот: клиент, который
            // придёт сразу, не получит «канала нет».
            let fresh = create(&self.name, &self.security, false)
                .context("новый экземпляр канала MCP не создался")?;
            Ok(std::mem::replace(&mut self.next, fresh))
        }
    }

    /// Жив ли канал другого нашего экземпляра. Канал под этим именем, который
    /// держит другой пользователь, не наш: тогда `bind` занимает новое
    /// случайное имя и переписывает файл адреса.
    pub async fn alive(endpoint: &str) -> bool {
        if !endpoint.starts_with(r"\\.\pipe\") {
            return false;
        }
        // Наш сервер держит свободный экземпляр всегда, кроме мгновения между
        // подключением и созданием следующего. Канал, занятый дольше, чужой,
        // и проверить его владельца нельзя.
        for _ in 0..BUSY_TRIES {
            match ClientOptions::new().open(endpoint) {
                Ok(pipe) => return pipe_server_is_current_user(&pipe),
                Err(e) if e.raw_os_error() == Some(PIPE_BUSY) => {
                    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                }
                Err(_) => return false,
            }
        }
        false
    }

    #[cfg(test)]
    pub async fn connect(endpoint: &str) -> Result<Client> {
        Ok(ClientOptions::new().open(endpoint)?)
    }
}

#[cfg(unix)]
mod imp {
    use std::path::{Path, PathBuf};

    use anyhow::{Context, Result};
    use tokio::net::{UnixListener, UnixStream};

    #[cfg(test)]
    pub type Client = UnixStream;

    /// Длиннее этого путь сокета в `sun_path` не влезает (на macOS предел
    /// 104 байта, на Linux 108), берём с запасом.
    const MAX_SOCK_PATH: usize = 100;

    pub struct Listener(UnixListener);

    /// Где лежит сокет: в папке данных, а если путь слишком длинный — в
    /// каталоге выполнения пользователя или в своём каталоге в /tmp.
    fn sock_path(data_dir: &Path) -> Result<PathBuf> {
        let p = super::mcp_dir(data_dir).join("sock");
        if p.as_os_str().len() <= MAX_SOCK_PATH {
            return Ok(p);
        }
        use std::hash::{Hash, Hasher};
        let mut h = std::collections::hash_map::DefaultHasher::new();
        data_dir.hash(&mut h);
        let tag = format!("{:08x}", h.finish() as u32);
        if let Some(run) = std::env::var_os("XDG_RUNTIME_DIR").filter(|d| !d.is_empty()) {
            return Ok(PathBuf::from(run).join(format!("clipwire-{tag}.sock")));
        }
        // SAFETY: geteuid без аргументов и не может завершиться ошибкой.
        let uid = unsafe { libc::geteuid() };
        let dir = PathBuf::from(format!("/tmp/clipwire-{uid}"));
        // Каталог в общем /tmp мог заранее создать другой пользователь
        // (или подложить ссылку): сокет кладём только в свой каталог 0700.
        check_own_dir(&dir, uid, true)?;
        clipwire_agent::state::ensure_private_dir(&dir)?;
        check_own_dir(&dir, uid, false)?;
        Ok(dir.join(format!("{tag}.sock")))
    }

    /// Каталог — настоящий каталог (не ссылка), принадлежит `uid` и закрыт
    /// для остальных. `may_be_missing` — его ещё может не быть.
    fn check_own_dir(dir: &Path, uid: u32, may_be_missing: bool) -> Result<()> {
        use std::os::unix::fs::MetadataExt;
        let m = match std::fs::symlink_metadata(dir) {
            Ok(m) => m,
            Err(e) if may_be_missing && e.kind() == std::io::ErrorKind::NotFound => {
                return Ok(());
            }
            Err(e) => {
                return Err(e).with_context(|| format!("каталог {} недоступен", dir.display()));
            }
        };
        if !m.file_type().is_dir() || m.uid() != uid {
            anyhow::bail!(
                "каталог {} не принадлежит текущему пользователю",
                dir.display()
            );
        }
        if !may_be_missing && m.mode() & 0o077 != 0 {
            anyhow::bail!("каталог {} доступен другим пользователям", dir.display());
        }
        Ok(())
    }

    pub fn listen(data_dir: &Path) -> Result<(String, Listener)> {
        use std::os::unix::fs::PermissionsExt;
        let path = sock_path(data_dir)?;
        // Сокет от прошлого запуска (живого хозяина уже проверили).
        let _ = std::fs::remove_file(&path);
        let l = UnixListener::bind(&path)
            .with_context(|| format!("сокет MCP {} не создался", path.display()))?;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))
            .context("права на сокет MCP не выставились")?;
        Ok((path.to_string_lossy().into_owned(), Listener(l)))
    }

    impl Listener {
        pub async fn accept(&mut self) -> Result<UnixStream> {
            Ok(self.0.accept().await.context("подключение к сокету MCP")?.0)
        }
    }

    pub async fn alive(endpoint: &str) -> bool {
        UnixStream::connect(endpoint).await.is_ok()
    }

    /// Процесс на том конце сокета (SO_PEERCRED на Linux, LOCAL_PEEREPID
    /// на macOS).
    pub fn peer_pid(sock: &UnixStream) -> Option<u32> {
        let pid = sock.peer_cred().ok()?.pid()?;
        u32::try_from(pid).ok().filter(|&p| p != 0)
    }

    #[cfg(test)]
    pub async fn connect(endpoint: &str) -> Result<Client> {
        Ok(UnixStream::connect(endpoint).await?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clipwire_core::ai::{AI_PROTOCOL, AiAccess};
    use clipwire_core::proto::io::read_msg;

    /// Сессия-заглушка: отвечает на Hello и Status.
    struct Echo;

    impl Session for Echo {
        async fn handle(&mut self, req: AiRequest) -> AiResponse {
            match req {
                AiRequest::Hello { protocol, .. } => AiResponse::Hello {
                    protocol,
                    access: AiAccess::Read,
                },
                AiRequest::Status => AiResponse::Saved {
                    id: "status".into(),
                },
                _ => AiResponse::Error {
                    code: AiError::Bad,
                    message: "нет".into(),
                },
            }
        }
    }

    #[tokio::test]
    async fn hello_and_status_over_real_channel() {
        let dir = std::env::temp_dir().join(format!(
            "clipwire-chan-{}",
            clipwire_agent::state::random_key().unwrap()[0..6]
                .iter()
                .map(|b| format!("{b:02x}"))
                .collect::<String>()
        ));
        let bound = bind(&dir).await.unwrap().expect("канал свободен");
        let endpoint = bound.endpoint().to_string();
        assert_eq!(read_endpoint(&dir).as_deref(), Some(endpoint.as_str()));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&endpoint).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
        }
        let peer = std::sync::Arc::new(std::sync::Mutex::new(None));
        let seen = peer.clone();
        let server = tokio::spawn(bound.run(move |pid| {
            *seen.lock().unwrap() = Some(pid);
            Echo
        }));

        let mut c = connect(&endpoint).await.unwrap();
        // Канал держит наш же процесс: проверка владельца его пропускает.
        #[cfg(windows)]
        assert!(pipe_server_is_current_user(&c));
        write_msg(
            &mut c,
            &AiRequest::Hello {
                protocol: AI_PROTOCOL,
                client: "test".into(),
                client_version: "1".into(),
            },
        )
        .await
        .unwrap();
        let r: AiResponse = read_msg(&mut c).await.unwrap().unwrap();
        assert_eq!(
            r,
            AiResponse::Hello {
                protocol: AI_PROTOCOL,
                access: AiAccess::Read
            }
        );
        // На том конце — наш же процесс.
        assert_eq!(*peer.lock().unwrap(), Some(Some(std::process::id())));
        write_msg(&mut c, &AiRequest::Status).await.unwrap();
        let r: AiResponse = read_msg(&mut c).await.unwrap().unwrap();
        assert_eq!(
            r,
            AiResponse::Saved {
                id: "status".into()
            }
        );
        // Мусорный кадр не рвёт соединение.
        {
            use tokio::io::AsyncWriteExt;
            c.write_all(&[0, 0, 0, 2, 0xff, 0xff]).await.unwrap();
        }
        let r: AiResponse = read_msg(&mut c).await.unwrap().unwrap();
        assert!(matches!(
            r,
            AiResponse::Error {
                code: AiError::Bad,
                ..
            }
        ));
        // Второе соединение работает параллельно с первым.
        let mut c2 = connect(&endpoint).await.unwrap();
        write_msg(&mut c2, &AiRequest::Status).await.unwrap();
        let r: AiResponse = read_msg(&mut c2).await.unwrap().unwrap();
        assert_eq!(
            r,
            AiResponse::Saved {
                id: "status".into()
            }
        );

        // Пока канал жив, второй экземпляр его не занимает.
        assert!(bind(&dir).await.unwrap().is_none());
        assert_eq!(read_endpoint(&dir).as_deref(), Some(endpoint.as_str()));

        server.abort();
        let _ = server.await;
        drop(c);
        drop(c2);
        // Задачи соединений замечают закрытие и отпускают экземпляры канала.
        for _ in 0..100 {
            if !imp::alive(&endpoint).await {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        cleanup(&dir, &endpoint);
        assert!(read_endpoint(&dir).is_none());
        // Устаревший файл адреса (хозяина нет) не мешает занять канал снова.
        std::fs::write(endpoint_file(&dir), format!("{endpoint}\n")).unwrap();
        let again = bind(&dir).await.unwrap().expect("старый адрес мёртв");
        cleanup(&dir, again.endpoint());
        let _ = std::fs::remove_dir_all(dir);
    }
}
