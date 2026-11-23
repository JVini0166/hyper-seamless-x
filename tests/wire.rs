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
