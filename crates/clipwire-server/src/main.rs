use anyhow::Result;
use clipwire_server::{Config, Identity};
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
    let running = clipwire_server::start(&config).await?;
    tracing::info!(
        relay = %config.relay_bind,
        quic = %config.quic_bind,
        public = %config.public_url,
        "сервер запущен"
    );
    tracing::info!("код сервера: {}", running.info.encode());
    tokio::signal::ctrl_c().await?;
    tracing::info!("остановка");
    running.shutdown().await
}
