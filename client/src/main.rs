#![cfg_attr(windows, windows_subsystem = "windows")]

#[cfg(windows)]
mod network;
#[cfg(windows)]
mod popup;
#[cfg(windows)]
mod tray;

#[cfg(windows)]
fn main() {
    if let Err(error) = tray::run() {
        tray::report_error(&format!("{error:#}"));
        std::process::exit(1);
    }
}

#[cfg(not(windows))]
fn main() {
    eprintln!("MaggotDrop Client requires Windows.");
    std::process::exit(1);
}
