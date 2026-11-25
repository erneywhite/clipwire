// В релизе без консольного окна: это приложение в трее.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

fn main() {
    // `--mcp`: мост для ИИ-клиента на stdio. Ни Tauri, ни трея, ни проверки
    // единственного экземпляра: это отдельный короткоживущий процесс, который
    // ИИ-клиент запускает сам и общается с ним через stdin/stdout.
    if std::env::args_os().skip(1).any(|a| a == "--mcp") {
        clipwire_desktop_lib::mcp_bridge::run();
        return;
    }
    clipwire_desktop_lib::run();
}
