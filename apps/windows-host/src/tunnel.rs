use protocol::{DEFAULT_PORT, Endpoint, Result};
use serde_json::Value;
use std::time::Duration;

#[cfg(windows)]
pub struct ManagedNgrok(std::process::Child);

#[cfg(windows)]
impl ManagedNgrok {
    pub fn start(selected: Option<&std::path::Path>, authtoken: Option<&str>) -> Result<Self> {
        use std::{
            net::{Ipv4Addr, SocketAddr, TcpStream},
            os::windows::process::CommandExt,
            process::Stdio,
        };
        use windows_sys::Win32::System::Threading::CREATE_NO_WINDOW;

        let inspection = SocketAddr::from((Ipv4Addr::LOCALHOST, 4040));
        if TcpStream::connect_timeout(&inspection, Duration::from_millis(150)).is_ok() {
            return Err("Já há um processo na porta de inspeção do ngrok. Use Copiar link do ngrok se o túnel já estiver configurado.".into());
        }
        let executable = if let Some(selected) = selected {
            if !valid_executable(selected) {
                return Err("Selecione o arquivo ngrok.exe no Windows".into());
            }
            selected.to_path_buf()
        } else {
            let path = std::env::var_os("PATH").unwrap_or_default();
            std::env::split_paths(&path)
                .map(|path| path.join("ngrok.exe"))
                .find(|path| valid_executable(path))
                .ok_or("Use Selecionar ngrok.exe para localizar o agente no Windows")?
        };
        let process = agent_command(&executable, authtoken)
            .creation_flags(CREATE_NO_WINDOW)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()?;
        Ok(Self(process))
    }

    pub fn try_wait(&mut self) -> std::io::Result<Option<std::process::ExitStatus>> {
        self.0.try_wait()
    }
}

#[cfg(any(windows, test))]
fn agent_command(executable: &std::path::Path, authtoken: Option<&str>) -> std::process::Command {
    let mut command = std::process::Command::new(executable);
    let upstream = format!("http://127.0.0.1:{DEFAULT_PORT}");
    command.args(["http", &upstream, "--inspect=false"]);
    if let Some(authtoken) = authtoken {
        command.env("NGROK_AUTHTOKEN", authtoken);
    }
    command
}

#[cfg(windows)]
pub fn valid_executable(path: &std::path::Path) -> bool {
    path.is_absolute()
        && path
            .file_name()
            .is_some_and(|name| name.eq_ignore_ascii_case("ngrok.exe"))
        && path.is_file()
}

#[cfg(windows)]
impl Drop for ManagedNgrok {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

pub fn ngrok_address() -> Result<String> {
    let catalog = read_catalog("http://127.0.0.1:4040/api/endpoints")?;
    select_endpoint(&catalog).ok_or_else(|| {
        format!("Nenhum endpoint ngrok encaminha para 127.0.0.1:{DEFAULT_PORT}").into()
    })
}

fn read_catalog(url: &str) -> Result<Value> {
    let config = ureq::Agent::config_builder()
        .proxy(None)
        .max_redirects(0)
        .timeout_global(Some(Duration::from_secs(2)))
        .build();
    let mut response = ureq::Agent::new_with_config(config)
        .get(url)
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
    Ok(serde_json::from_str(&body)?)
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
            let priority = match address.split_once("://")?.0 {
                "https" => 0,
                "tcp" => 1,
                "http" => 2,
                _ => return None,
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
    fn passes_the_token_only_in_the_agent_environment() {
        let executable = std::path::Path::new("ngrok.exe");
        let command = agent_command(executable, Some("ngrok_pat_test_value"));
        assert_eq!(command.get_program(), executable.as_os_str());
        assert_eq!(
            command.get_args().collect::<Vec<_>>(),
            ["http", "http://127.0.0.1:47651", "--inspect=false"]
        );
        let (key, value) = command.get_envs().next().unwrap();
        assert_eq!(key, "NGROK_AUTHTOKEN");
        assert_eq!(value.unwrap(), "ngrok_pat_test_value");
        assert_eq!(agent_command(executable, None).get_envs().count(), 0);
    }

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
    #[test]
    fn reads_endpoint_catalog_from_the_agent_http_api() -> Result<()> {
        use std::{
            io::{BufRead, BufReader, Write},
            net::TcpListener,
            thread,
        };
        let listener = TcpListener::bind("127.0.0.1:0")?;
        let endpoint = format!("http://{}/api/endpoints", listener.local_addr()?);
        let agent = thread::spawn(move || -> Result<()> {
            let (mut connection, _) = listener.accept()?;
            connection.set_read_timeout(Some(Duration::from_secs(2)))?;
            let mut request = BufReader::new(&connection);
            let mut line = String::new();
            request.read_line(&mut line)?;
            assert_eq!(line.trim_end(), "GET /api/endpoints HTTP/1.1");
            loop {
                line.clear();
                if request.read_line(&mut line)? == 0 || line == "\r\n" {
                    break;
                }
            }
            let body = r#"{"endpoints":[{"url":"https://example.ngrok.app","upstream":{"url":"http://127.0.0.1:47651"}}]}"#;
            write!(
                connection,
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n{body}",
                body.len()
            )?;
            Ok(())
        });
        let catalog = read_catalog(&endpoint)?;
        assert_eq!(
            select_endpoint(&catalog).as_deref(),
            Some("https://example.ngrok.app")
        );
        agent.join().expect("test agent panicked")?;
        Ok(())
    }
}
