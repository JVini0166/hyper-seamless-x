use crate::{
    ConnectionProfile, Endpoint, Hello, Pairing, Request, Response, Result, SERVER_NAME, VERSION,
    Wire,
};
use rustls::{
    ClientConfig, ClientConnection, DigitallySignedStruct, Error, ServerConnection,
    SignatureScheme, StreamOwned,
    client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier},
    crypto::{WebPkiSupportedAlgorithms, verify_tls12_signature, verify_tls13_signature},
    pki_types::{CertificateDer, ServerName, UnixTime},
};
use std::{
    sync::{Arc, Mutex, mpsc},
    time::{Duration, Instant},
};

const OPEN_TIME: Duration = Duration::from_secs(90);
const APPROVAL_TIME: Duration = Duration::from_secs(55);
const EXPORT_LABEL: &[u8] = b"EXPORTER-Hyper-Seamless-X-Pairing-v2";

fn pairing_code(exported: [u8; 8]) -> String {
    let digits = u64::from_be_bytes(exported) % 100_000_000;
    format!("{:04}-{:04}", digits / 10_000, digits % 10_000)
}

#[derive(Debug)]
struct CompareOnScreen(WebPkiSupportedAlgorithms);

impl ServerCertVerifier for CompareOnScreen {
    fn verify_server_cert(
        &self,
        _: &CertificateDer<'_>,
        _: &[CertificateDer<'_>],
        _: &ServerName<'_>,
        _: &[u8],
        _: UnixTime,
    ) -> std::result::Result<ServerCertVerified, Error> {
        // This configuration can only request pairing. The UI must compare a code
        // bound to this handshake before the client stores the peer certificate.
        Ok(ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        sig: &DigitallySignedStruct,
    ) -> std::result::Result<HandshakeSignatureValid, Error> {
        verify_tls12_signature(message, cert, sig, &self.0)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        sig: &DigitallySignedStruct,
    ) -> std::result::Result<HandshakeSignatureValid, Error> {
        verify_tls13_signature(message, cert, sig, &self.0)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.0.supported_schemes()
    }
}

fn pairing_client_config() -> Result<ClientConfig> {
    let provider = rustls::crypto::ring::default_provider();
    let verifier = Arc::new(CompareOnScreen(provider.signature_verification_algorithms));
    let mut config = ClientConfig::builder_with_provider(provider.into())
        .with_protocol_versions(&[&rustls::version::TLS13])?
        .dangerous()
        .with_custom_certificate_verifier(verifier)
        .with_no_client_auth();
    config.resumption = rustls::client::Resumption::disabled();
    Ok(config)
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PairingState {
    Closed,
    Open,
    Pending(String),
}

#[derive(Default)]
struct Gate {
    open_until: Option<Instant>,
    pending: Option<(String, mpsc::SyncSender<bool>)>,
}

#[derive(Clone, Default)]
pub struct PairingGate(Arc<Mutex<Gate>>);

impl PairingGate {
    pub fn open(&self) -> Result<()> {
        let mut gate = self.0.lock().map_err(|_| "Pareamento indisponível")?;
        if gate.pending.is_none() {
            gate.open_until = Some(Instant::now() + OPEN_TIME);
        }
        Ok(())
    }

    pub fn state(&self) -> PairingState {
        let Ok(gate) = self.0.lock() else {
            return PairingState::Closed;
        };
        if let Some((code, _)) = &gate.pending {
            return PairingState::Pending(code.clone());
        }
        if gate.open_until.is_some_and(|until| until > Instant::now()) {
            PairingState::Open
        } else {
            PairingState::Closed
        }
    }

    pub fn decide(&self, code: &str, accept: bool) {
        if let Ok(mut gate) = self.0.lock()
            && gate
                .pending
                .as_ref()
                .is_some_and(|(pending_code, _)| pending_code == code)
            && let Some((_, decision)) = gate.pending.take()
        {
            let _ = decision.try_send(accept);
        }
    }

    pub fn close(&self) {
        if let Ok(mut gate) = self.0.lock() {
            gate.open_until = None;
            gate.pending = None;
        }
    }

    pub fn approve(&self, code: String, on_pending: impl FnOnce() -> Result<()>) -> Result<bool> {
        let receive = {
            let mut gate = self.0.lock().map_err(|_| "Pareamento indisponível")?;
            if !gate.open_until.is_some_and(|until| until > Instant::now())
                || gate.pending.is_some()
            {
                return Err("No Windows, clique em Autorizar novo Mac antes de conectar".into());
            }
            let (send, receive) = mpsc::sync_channel(1);
            gate.open_until = None;
            gate.pending = Some((code.clone(), send));
            receive
        };
        let result = on_pending().map(|()| receive.recv_timeout(APPROVAL_TIME).unwrap_or(false));
        if let Ok(mut gate) = self.0.lock()
            && gate
                .pending
                .as_ref()
                .is_some_and(|(pending_code, _)| pending_code == &code)
        {
            gate.pending = None;
        }
        result
    }
}

pub fn request_pairing(
    endpoint: Endpoint,
    on_code: impl FnOnce(String) -> Result<()>,
) -> Result<ConnectionProfile> {
    let wire = endpoint.connect(Duration::from_secs(65))?;
    let conn = ClientConnection::new(Arc::new(pairing_client_config()?), SERVER_NAME.try_into()?)?;
    let mut stream = StreamOwned::new(conn, wire);
    crate::send(
        &mut stream,
        &Hello {
            version: VERSION,
            token: [0; 32],
            request: Request::Pair,
        },
    )?;
    match crate::receive(&mut stream)? {
        Response::PairPending => {}
        Response::Error(message) => return Err(message.into()),
        _ => return Err("Resposta de pareamento inesperada".into()),
    }
    let exported = stream
        .conn
        .export_keying_material([0u8; 8], EXPORT_LABEL, None)?;
    on_code(pairing_code(exported))?;
    let pairing = match crate::receive(&mut stream)? {
        Response::Paired(pairing) => pairing,
        Response::Error(message) => return Err(message.into()),
        _ => return Err("Resposta de aprovação inesperada".into()),
    };
    let certificate = stream
        .conn
        .peer_certificates()
        .and_then(|certs| certs.first())
        .ok_or("Certificado do Windows indisponível")?;
    if certificate.as_ref() != pairing.certificate.as_slice() {
        return Err("O certificado recebido diverge da conexão".into());
    }
    Ok(ConnectionProfile {
        address: endpoint,
        pairing,
    })
}

pub fn accept_pairing(
    stream: &mut StreamOwned<ServerConnection, Wire>,
    pairing: &Pairing,
    gate: &PairingGate,
    is_active: impl FnOnce() -> bool,
) -> Result<()> {
    let exported = stream
        .conn
        .export_keying_material([0u8; 8], EXPORT_LABEL, None)?;
    let response = match gate.approve(pairing_code(exported), || {
        crate::send(stream, &Response::PairPending)
    }) {
        Ok(true) if is_active() => Response::Paired(pairing.clone()),
        Ok(_) => Response::Error("Pareamento recusado ou expirado".into()),
        Err(error) => Response::Error(error.to_string()),
    };
    crate::send(stream, &response)
}
