// В релизе без консольного окна: это приложение в трее.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

fn main() {
    clipwire_desktop_lib::run();
}
