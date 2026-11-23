//! H.264 over ICE and DTLS-SRTP. Callers exchange SDP on their authenticated channel.

use crate::Result;
use std::{
    io::ErrorKind,
    net::{IpAddr, SocketAddr, UdpSocket},
    time::Instant,
};
use str0m::{
    Candidate, Event, Input, Output, Rtc, RtcConfig,
    change::{SdpAnswer, SdpOffer, SdpPendingOffer},
    media::{Direction, MediaData, MediaKind, MediaTime, Mid},
    net::{Protocol, Receive},
};

pub struct VideoPeer {
    rtc: Rtc,
    socket: UdpSocket,
    local: SocketAddr,
    started: Instant,
    video: Option<Mid>,
    receive: Vec<u8>,
}

impl VideoPeer {
    fn bind(ip: IpAddr) -> Result<Self> {
        let socket = UdpSocket::bind((ip, 0))?;
        socket.set_nonblocking(true)?;
        let local = socket.local_addr()?;
        let started = Instant::now();
        let mut rtc = RtcConfig::new()
            .clear_codecs()
            .enable_h264(true)
            .build(started);
        rtc.add_local_candidate(Candidate::host(local, "udp")?);
        Ok(Self {
            rtc,
            socket,
            local,
            started,
            video: None,
            receive: vec![0; 65_536],
        })
    }

    pub fn offer(ip: IpAddr) -> Result<(Self, String, SdpPendingOffer)> {
        let mut peer = Self::bind(ip)?;
        let mut change = peer.rtc.sdp_api();
        let mid = change.add_media(MediaKind::Video, Direction::RecvOnly, None, None, None);
        let (offer, pending) = change.apply().ok_or("Oferta WebRTC indisponível")?;
        peer.video = Some(mid);
        Ok((peer, offer.to_sdp_string(), pending))
    }

    pub fn answer(ip: IpAddr, offer: &str) -> Result<(Self, String)> {
        let mut peer = Self::bind(ip)?;
        let answer = peer
            .rtc
            .sdp_api()
            .accept_offer(SdpOffer::from_sdp_string(offer)?)?;
        Ok((peer, answer.to_sdp_string()))
    }

    pub fn accept_answer(&mut self, pending: SdpPendingOffer, answer: &str) -> Result<()> {
        self.rtc
            .sdp_api()
            .accept_answer(pending, SdpAnswer::from_sdp_string(answer)?)?;
        Ok(())
    }

    pub fn ready(&self) -> bool {
        self.video.is_some() && self.rtc.is_connected()
    }

    pub fn send(&mut self, frame: Vec<u8>) -> Result<()> {
        let mid = self.video.ok_or("Trilha de vídeo WebRTC indisponível")?;
        let writer = self
            .rtc
            .writer(mid)
            .ok_or("Trilha de vídeo WebRTC encerrada")?;
        let pt = writer
            .payload_params()
            .next()
            .ok_or("H.264 WebRTC indisponível")?
            .pt();
        let now = Instant::now();
        let timestamp = MediaTime::from_seconds(now.duration_since(self.started).as_secs_f64());
        writer.write(pt, now, timestamp, frame)?;
        Ok(())
    }

    /// Drains protocol work and returns at most one complete access unit per call.
    pub fn poll(&mut self) -> Result<Option<MediaData>> {
        let mut remaining_receives = 64;
        loop {
            match self.rtc.poll_output()? {
                Output::Transmit(tx) => match self.socket.send_to(&tx.contents, tx.destination) {
                    Ok(_) => {}
                    Err(error) if error.kind() == ErrorKind::WouldBlock => {}
                    Err(error) => return Err(error.into()),
                },
                Output::Event(Event::MediaAdded(media)) if media.kind == MediaKind::Video => {
                    self.video = Some(media.mid);
                }
                Output::Event(Event::MediaData(data)) if Some(data.mid) == self.video => {
                    return Ok(Some(data));
                }
                Output::Event(_) => {}
                Output::Timeout(deadline) => {
                    let now = Instant::now();
                    if deadline <= now {
                        self.rtc.handle_input(Input::Timeout(now))?;
                        continue;
                    }
                    if remaining_receives == 0 {
                        return Ok(None);
                    }
                    match self.socket.recv_from(&mut self.receive) {
                        Ok((len, source)) => {
                            remaining_receives -= 1;
                            if let Ok(contents) = self.receive[..len].try_into() {
                                self.rtc.handle_input(Input::Receive(
                                    now,
                                    Receive {
                                        proto: Protocol::Udp,
                                        source,
                                        destination: self.local,
                                        contents,
                                    },
                                ))?;
                            }
                        }
                        Err(error) if error.kind() == ErrorKind::WouldBlock => return Ok(None),
                        Err(error) => return Err(error.into()),
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{thread, time::Duration};

    #[test]
    fn sends_h264_over_ice_and_srtp_on_loopback() -> Result<()> {
        let ip = IpAddr::V4(std::net::Ipv4Addr::LOCALHOST);
        let (mut client, offer, pending) = VideoPeer::offer(ip)?;
        let (mut host, answer) = VideoPeer::answer(ip, &offer)?;
        client.accept_answer(pending, &answer)?;
        let sample = include_bytes!("../../../tests/fixtures/red-32x24.h264");
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut sent = false;
        while Instant::now() < deadline {
            host.poll()?;
            if let Some(frame) = client.poll()? {
                assert!(
                    frame
                        .data
                        .windows(5)
                        .any(|nal| { nal[..4] == [0, 0, 0, 1] && nal[4] & 0x1f == 5 })
                );
                return Ok(());
            }
            if !sent && host.ready() && client.ready() {
                host.send(sample.to_vec())?;
                sent = true;
            }
            thread::sleep(Duration::from_millis(2));
        }
        Err("O par WebRTC não recebeu o quadro H.264".into())
    }
}
