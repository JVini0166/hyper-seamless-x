#[cfg(windows)]
mod host;

#[cfg(windows)]
fn main() -> protocol::Result<()> {
    host::run()
}

#[cfg(not(windows))]
fn main() {
    eprintln!("Execute o host em uma sessão interativa do Windows 11.");
}
