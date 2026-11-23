use protocol::{DEFAULT_PORT, Endpoint, Result};
use serde_json::Value;
use std::time::Duration;

#[cfg(windows)]
pub struct ManagedNgrok(std::process::Child);

#[cfg(windows)]
impl ManagedNgrok {
    pub fn start() -> Result<Self> {
        use std::{
            net::{Ipv4Addr, SocketAddr, TcpStream},
            os::windows::process::CommandExt,
            process::{Command, Stdio},
        };
        use windows_sys::Win32::System::Threading::CREATE_NO_WINDOW;

        let inspection = SocketAddr::from((Ipv4Addr::LOCALHOST, 4040));
        if TcpStream::connect_timeout(&inspection, Duration::from_millis(150)).is_ok() {
            return Err("Já há um processo na porta de inspeção do ngrok. Use Copiar link do ngrok se o túnel já estiver configurado.".into());
        }
        let path = std::env::var_os("PATH").ok_or("ngrok.exe não encontrado no PATH do Windows")?;
        let executable = std::env::split_paths(&path)
            .filter(|path| path.is_absolute())
            .map(|path| path.join("ngrok.exe"))
            .find(|path| path.is_file())
            .ok_or("ngrok.exe não encontrado no PATH do Windows")?;
        let upstream = format!("http://127.0.0.1:{DEFAULT_PORT}");
        let process = Command::new(executable)
            .args(["http", &upstream, "--inspect=false"])
            .creation_flags(CREATE_NO_WINDOW)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()?;
        Ok(Self(process))
    }
}

#[cfg(windows)]
impl Drop for ManagedNgrok {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

pub fn ngrok_address() -> Result<String> {
    let config = ureq::Agent::config_builder()
        .proxy(None)
        .max_redirects(0)
        .timeout_global(Some(Duration::from_secs(2)))
        .build();
    let mut response = ureq::Agent::new_with_config(config)
        .get("http://127.0.0.1:4040/api/endpoints")
        .call()
        .map_err(|_| "Agente ngrok indisponível no endereço local padrão")?;
    if response.status() != 200 {
        return Err("O agente ngrok não retornou a lista de endpoints".into());
    }
    let body = response
        .body_mut()
        .with_config()
        .limit(131_072)
        .read_to_string()?;
    let catalog: Value = serde_json::from_str(&body)?;
    select_endpoint(&catalog).ok_or_else(|| {
        format!("Nenhum endpoint ngrok encaminha para 127.0.0.1:{DEFAULT_PORT}").into()
    })
}

fn select_endpoint(catalog: &Value) -> Option<String> {
    catalog["endpoints"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|endpoint| {
            let upstream = endpoint["upstream"]["url"].as_str()?;
            let authority = upstream
                .split_once("://")
                .map_or(upstream, |(_, authority)| authority);
            let (host, port) = authority.trim_end_matches('/').rsplit_once(':')?;
            if !matches!(host, "127.0.0.1" | "localhost")
                || port.parse::<u16>().ok()? != DEFAULT_PORT
            {
                return None;
            }
            let address = endpoint["url"].as_str()?;
            Endpoint::parse(address).ok()?;
            let priority = if address.starts_with("https://") {
                0
            } else {
                1
            };
            Some((priority, address))
        })
        .min_by_key(|(priority, _)| *priority)
        .map(|(_, address)| address.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn selects_only_an_endpoint_forwarding_to_the_local_host() {
        let catalog = serde_json::json!({"endpoints": [
            {"url": "https://unrelated.ngrok.app", "upstream": {"url": "http://localhost:8080"}},
            {"url": "tcp://example.ngrok.io:12345", "upstream": {"url": "tcp://127.0.0.1:47651"}},
            {"url": "https://correct.ngrok.app", "upstream": {"url": "http://localhost:47651"}}
        ]});
        assert_eq!(
            select_endpoint(&catalog).as_deref(),
            Some("https://correct.ngrok.app")
        );
        assert!(select_endpoint(&serde_json::json!({"endpoints": []})).is_none());
    }
}
