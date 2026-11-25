use anyhow::Result;
use clipwire_server::{Config, Guards, Identity};
use tracing_subscriber::EnvFilter;

const USAGE: &str = "\
clipwire-server — сервер Clipwire

Команды:
  run    запустить сервер (по умолчанию)
  info   напечатать код сервера (cwserver2-…) для устройства администратора

Настройки задаются переменными окружения, см. README.";

#[tokio::main]
async fn main() -> Result<()> {
    // В docker logs и journald цветовые коды только мешают.
    let ansi = std::io::IsTerminal::is_terminal(&std::io::stdout());
    tracing_subscriber::fmt()
        .with_ansi(ansi)
        .with_env_filter(
            EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| EnvFilter::new("info,iroh=warn,iroh_relay=warn")),
        )
        .init();

    let cmd = std::env::args().nth(1).unwrap_or_else(|| "run".into());
    match cmd.as_str() {
        "run" => run().await,
        "info" => {
            let config = Config::from_env()?;
            let id = Identity::load_or_create(&config.data_dir, config.relay_token.as_deref())?;
            println!("{}", id.admin_info(&config.public_url).encode());
            Ok(())
        }
        "-h" | "--help" | "help" => {
            println!("{USAGE}");
            Ok(())
        }
        other => anyhow::bail!("неизвестная команда {other}\n\n{USAGE}"),
    }
}

async fn run() -> Result<()> {
    let config = Config::from_env()?;
    let guards = Guards::from_env()?;
    let running = clipwire_server::start_with(&config, &guards).await?;
    tracing::info!(
        relay = %config.relay_bind,
        quic = %config.quic_bind,
        public = %config.public_url,
        "сервер запущен"
    );
    // В коде сервера ключ администратора, а лог контейнера часто видят
    // больше людей, чем нужно. Поэтому код печатается только при первом
    // запуске, когда ключ только что создан, или по явной просьбе
    // (`CLIPWIRE_PRINT_SERVER_CODE=1`: в панели без консоли контейнера это
    // единственный способ его узнать).
    if running.admin_key_created || print_code_requested() {
        tracing::info!("код сервера: {}", running.server_code());
    } else {
        tracing::info!(
            "код сервера (cwserver2-…) печатает `docker exec <контейнер> clipwire-server info`, \
             либо он будет в логе после перезапуска с CLIPWIRE_PRINT_SERVER_CODE=1"
        );
    }
    stop_signal().await?;
    tracing::info!("остановка");
    // Не даём корректной остановке тянуться дольше, чем Docker ждёт до SIGKILL.
    match tokio::time::timeout(std::time::Duration::from_secs(5), running.shutdown()).await {
        Ok(res) => res,
        Err(_) => {
            tracing::warn!("остановка затянулась, выхожу");
            Ok(())
        }
    }
}

/// Просят ли напечатать код сервера при запуске (`CLIPWIRE_PRINT_SERVER_CODE`).
fn print_code_requested() -> bool {
    std::env::var("CLIPWIRE_PRINT_SERVER_CODE")
        .is_ok_and(|v| matches!(v.trim(), "1" | "true" | "yes"))
}

/// Ctrl+C в консоли или SIGTERM от Docker. В контейнере сервер работает как
/// PID 1, а такому процессу ядро без обработчика SIGTERM не доставляет:
/// `docker stop` ждал 10 секунд и убивал контейнер с кодом 137.
async fn stop_signal() -> Result<()> {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        let mut term = signal(SignalKind::terminate())?;
        tokio::select! {
            r = tokio::signal::ctrl_c() => r?,
            _ = term.recv() => {}
        }
        Ok(())
    }
    #[cfg(not(unix))]
    {
        tokio::signal::ctrl_c().await?;
        Ok(())
    }
}
