#[cfg(target_os = "macos")]
mod client;
#[cfg(target_os = "macos")]
mod h264;

#[cfg(target_os = "macos")]
fn main() -> protocol::Result<()> {
    client::run()
}

#[cfg(not(target_os = "macos"))]
fn main() {
    eprintln!("Execute o cliente no macOS.");
}
