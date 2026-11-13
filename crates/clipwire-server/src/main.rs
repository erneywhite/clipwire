use anyhow::Result;
use clipwire_server::{Config, Guards, Identity};
use tracing_subscriber::EnvFilter;

const USAGE: &str = "\
clipwire-server — сервер Clipwire

Команды:
  run    запустить сервер (по умолчанию)
  info   напечатать код сервера для первого устройства

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
            println!("{}", id.server_info(&config.public_url).encode());
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
    tracing::info!("код сервера: {}", running.info.encode());
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
