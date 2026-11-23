#![cfg_attr(windows, windows_subsystem = "windows")]

#[cfg(windows)]
mod h264;
#[cfg(windows)]
mod host;
#[cfg(windows)]
mod ui;

#[cfg(windows)]
fn main() -> protocol::Result<()> {
    ui::run();
    Ok(())
}

#[cfg(not(windows))]
fn main() {
    eprintln!("Execute o host em uma sessão interativa do Windows 11.");
}
