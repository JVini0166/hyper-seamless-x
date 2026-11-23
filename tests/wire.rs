use protocol::{Frame, Hello, Request, VERSION};
use std::{io::Cursor, sync::Arc};

#[test]
fn tls_pairing_authenticates_and_rejects_wrong_token() -> protocol::Result<()> {
    let (pairing, key) = protocol::new_identity()?;
    let listener = std::net::TcpListener::bind("127.0.0.1:0")?;
    let addr = listener.local_addr()?;
    let config = Arc::new(pairing.server_config(key)?);
    let host_pairing = pairing.clone();
    let server = std::thread::spawn(move || -> protocol::Result<()> {
        let (socket, _) = listener.accept()?;
        let connection = rustls::ServerConnection::new(config)?;
        let mut stream = rustls::StreamOwned::new(connection, socket);
        let hello: Hello = protocol::receive(&mut stream)?;
        hello.verify(&host_pairing)?;
        let invalid: Hello = protocol::receive(&mut stream)?;
        assert!(invalid.verify(&host_pairing).is_err());
        protocol::send(&mut stream, &protocol::Response::Launched)
    });
    let socket = std::net::TcpStream::connect(addr)?;
    socket.set_read_timeout(Some(std::time::Duration::from_secs(5)))?;
    let conn = rustls::ClientConnection::new(
        Arc::new(pairing.client_config()?),
        protocol::SERVER_NAME.try_into()?,
    )?;
    let mut stream = rustls::StreamOwned::new(conn, socket);
    let mut hello = Hello {
        version: VERSION,
        token: pairing.token,
        request: Request::List,
    };
    protocol::send(&mut stream, &hello)?;
    hello.token[0] ^= 1;
    protocol::send(&mut stream, &hello)?;
    let response: protocol::Response = protocol::receive(&mut stream)?;
    assert!(matches!(response, protocol::Response::Launched));
    server.join().expect("host thread panicked")?;
    Ok(())
}

#[test]
fn video_round_trip_and_heartbeat() -> protocol::Result<()> {
    let pixels = vec![9; 128 * 64 * 4];
    let mut bytes = Vec::new();
    protocol::heartbeat(&mut bytes)?;
    Frame {
        width: 128,
        height: 64,
        bgra: pixels.clone(),
    }
    .send(&mut bytes)?;
    let mut stream = Cursor::new(bytes);
    assert!(Frame::receive(&mut stream)?.is_none());
    let frame = Frame::receive(&mut stream)?.expect("missing frame");
    assert_eq!((frame.width, frame.height, frame.bgra), (128, 64, pixels));
    Ok(())
}

#[test]
fn rejects_excessive_packet_lengths() {
    let bytes = u32::MAX.to_be_bytes();
    assert!(Frame::receive(&mut bytes.as_slice()).is_err());
    assert!(protocol::receive::<Hello>(&mut bytes.as_slice()).is_err());
}

#[test]
fn parses_ip_dns_tcp_and_ngrok_web_urls() -> protocol::Result<()> {
    use protocol::Endpoint;
    assert_eq!(
        Endpoint::parse("192.168.0.3")?.to_string(),
        "192.168.0.3:47651"
    );
    assert_eq!(Endpoint::parse("::1")?.to_string(), "[::1]:47651");
    assert_eq!(
        Endpoint::parse("tcp://0.tcp.ngrok.io:1234")?.to_string(),
        "0.tcp.ngrok.io:1234"
    );
    assert_eq!(
        Endpoint::parse("https://example.ngrok.app/")?.to_string(),
        "wss://example.ngrok.app/hyper-seamless-x"
    );
    assert!(Endpoint::parse("https://user:secret@example.ngrok.app").is_err());
    assert!(Endpoint::parse("tcp://example.com:0").is_err());
    Ok(())
}

#[test]
fn server_and_client_compare_code_over_tcp_and_websocket() -> protocol::Result<()> {
    use protocol::{Endpoint, PairingGate, PairingState, Response};
    use rustls::{ClientConnection, ServerConnection, StreamOwned};
    use std::{net::TcpListener, time::Duration};

    for scheme in ["tcp", "http"] {
        let (pairing, key) = protocol::new_identity()?;
        let listener = TcpListener::bind("127.0.0.1:0")?;
        let endpoint = Endpoint::parse(&format!("{scheme}://{}", listener.local_addr()?))?;
        let gate = PairingGate::default();
        let server_gate = gate.clone();
        let server_pairing = pairing.clone();
        let config = Arc::new(pairing.server_config(key)?);
        let server = std::thread::spawn(move || -> protocol::Result<()> {
            for operation in [Request::Pair, Request::Pair, Request::List] {
                let (socket, _) = listener.accept()?;
                let mut stream = StreamOwned::new(
                    ServerConnection::new(config.clone())?,
                    protocol::use_incoming(socket)?,
                );
                let hello: Hello = protocol::receive(&mut stream)?;
                match operation {
                    Request::Pair => {
                        assert!(matches!(hello.request, Request::Pair));
                        protocol::accept_pairing(
                            &mut stream,
                            &server_pairing,
                            &server_gate,
                            || true,
                        )?;
                    }
                    Request::List => {
                        hello.verify(&server_pairing)?;
                        assert!(matches!(hello.request, Request::List));
                        protocol::send(&mut stream, &Response::Windows(Vec::new()))?;
                    }
                    _ => unreachable!(),
                }
            }
            Ok(())
        });

        // Starting a tunnel does not enable the pairing prompt or disclose credentials.
        assert!(protocol::request_pairing(endpoint.clone(), |_| panic!("gate is closed")).is_err());
        gate.open()?;
        let profile = protocol::request_pairing(endpoint, |code| {
            assert_eq!(gate.state(), PairingState::Pending(code.clone()));
            gate.decide(&code, true);
            Ok(())
        })?;
        assert_eq!(profile.pairing.token, pairing.token);
        assert_eq!(profile.pairing.certificate, pairing.certificate);
        let mut stream = StreamOwned::new(
            ClientConnection::new(
                Arc::new(profile.pairing.client_config()?),
                protocol::SERVER_NAME.try_into()?,
            )?,
            profile.address.connect(Duration::from_secs(5))?,
        );
        protocol::send(
            &mut stream,
            &Hello {
                version: VERSION,
                token: profile.pairing.token,
                request: Request::List,
            },
        )?;
        let response: Response = protocol::receive(&mut stream)?;
        assert!(matches!(response, Response::Windows(windows) if windows.is_empty()));
        server.join().expect("host thread panicked")?;
    }
    Ok(())
}
