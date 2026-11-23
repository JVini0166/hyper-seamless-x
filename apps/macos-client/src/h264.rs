use protocol::{Frame, Result};
use std::{ffi::c_void, ptr::NonNull};

unsafe extern "C" {
    fn hsx_decoder_create(
        sps: *const u8,
        sps_len: usize,
        pps: *const u8,
        pps_len: usize,
        max_pixels: u64,
        out: *mut *mut c_void,
        width: *mut u32,
        height: *mut u32,
    ) -> i32;
    fn hsx_decoder_destroy(decoder: *mut c_void);
    fn hsx_decoder_decode(
        decoder: *mut c_void,
        sample: *const u8,
        length: usize,
        output: *mut u8,
        width: u32,
        height: u32,
    ) -> i32;
}

struct Session {
    decoder: NonNull<c_void>,
    width: u32,
    height: u32,
}

impl Session {
    fn new(sps: &[u8], pps: &[u8]) -> Result<Self> {
        let (mut width, mut height) = (0, 0);
        let mut decoder = std::ptr::null_mut();
        // The native function copies parameter sets into the format description.
        let status = unsafe {
            hsx_decoder_create(
                sps.as_ptr(),
                sps.len(),
                pps.as_ptr(),
                pps.len(),
                protocol::MAX_PIXELS as u64,
                &mut decoder,
                &mut width,
                &mut height,
            )
        };
        if status != 0 {
            return Err(format!("VideoToolbox recusou a configuração H.264 ({status})").into());
        }
        Ok(Self {
            decoder: NonNull::new(decoder).ok_or("Decodificador H.264 indisponível")?,
            width,
            height,
        })
    }

    fn decode(&mut self, sample: &[u8]) -> Result<Frame> {
        let mut bgra = vec![0; self.width as usize * self.height as usize * 4];
        // The native decoder writes exactly width*height*4 bytes and returns synchronously.
        let status = unsafe {
            hsx_decoder_decode(
                self.decoder.as_ptr(),
                sample.as_ptr(),
                sample.len(),
                bgra.as_mut_ptr(),
                self.width,
                self.height,
            )
        };
        if status != 0 {
            return Err(format!("VideoToolbox recusou o quadro H.264 ({status})").into());
        }
        Ok(Frame {
            width: self.width,
            height: self.height,
            bgra,
        })
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        unsafe { hsx_decoder_destroy(self.decoder.as_ptr()) };
    }
}

#[derive(Default)]
pub struct Decoder {
    sps: Vec<u8>,
    pps: Vec<u8>,
    session: Option<Session>,
}

impl Decoder {
    pub fn decode(&mut self, unit: &[u8]) -> Result<Option<Frame>> {
        let mut sample = Vec::with_capacity(unit.len() + 16);
        let mut has_picture = false;
        for nal in annex_b_nals(unit)? {
            let kind = nal[0] & 0x1f;
            if nal[0] & 0x80 != 0 {
                return Err("NAL H.264 inválido".into());
            }
            match kind {
                7 | 8 => {
                    if nal.len() > 4096 {
                        return Err("Parâmetros H.264 acima do limite".into());
                    }
                    let parameter = if kind == 7 {
                        &mut self.sps
                    } else {
                        &mut self.pps
                    };
                    if parameter.as_slice() != nal {
                        *parameter = nal.to_vec();
                        self.session = None;
                    }
                }
                1..=5 | 6 => {
                    has_picture |= kind <= 5;
                    sample.extend_from_slice(&(nal.len() as u32).to_be_bytes());
                    sample.extend_from_slice(nal);
                }
                _ => {}
            }
        }
        if !has_picture {
            return Ok(None);
        }
        if self.session.is_none() {
            if self.sps.is_empty() || self.pps.is_empty() {
                return Err(
                    "O host precisa enviar SPS e PPS antes do primeiro quadro H.264".into(),
                );
            }
            self.session = Some(Session::new(&self.sps, &self.pps)?);
        }
        self.session.as_mut().unwrap().decode(&sample).map(Some)
    }
}

fn annex_b_nals(unit: &[u8]) -> Result<Vec<&[u8]>> {
    let mut starts = unit
        .windows(3)
        .enumerate()
        .filter_map(|(index, bytes)| (bytes == [0, 0, 1]).then_some(index));
    let first = starts
        .next()
        .ok_or("Unidade H.264 sem delimitador Annex B")?;
    if unit[..first].iter().any(|&byte| byte != 0) {
        return Err("Prefixo Annex B inválido".into());
    }
    let mut begin = first + 3;
    let mut units = Vec::new();
    for end in starts.chain(std::iter::once(unit.len())) {
        let payload = &unit[begin..end];
        let len = payload
            .iter()
            .rposition(|&byte| byte != 0)
            .map_or(0, |last| last + 1);
        let nal = &payload[..len];
        if nal.is_empty() || units.len() >= 4096 {
            return Err("Segmentos NAL inválidos".into());
        }
        units.push(nal);
        begin = end + 3;
    }
    Ok(units)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_h264_annex_b_with_video_toolbox() -> Result<()> {
        let sample = include_bytes!("../../../tests/fixtures/red-32x24.h264");
        let frame = Decoder::default().decode(sample)?.expect("missing picture");
        assert_eq!((frame.width, frame.height), (32, 24));
        assert_eq!(frame.bgra.len(), 32 * 24 * 4);
        let [blue, green, red, _] = frame.bgra[..4] else {
            unreachable!()
        };
        assert!(
            red > 200 && green < 25 && blue < 25,
            "unexpected BGRA {blue} {green} {red}"
        );
        Ok(())
    }
}
