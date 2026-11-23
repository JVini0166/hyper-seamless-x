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
    fn hsx_decoder_hardware(decoder: *mut c_void) -> i32;
    fn hsx_decoder_decode(
        decoder: *mut c_void,
        sample: *const u8,
        length: usize,
        output: *mut u8,
        width: u32,
        height: u32,
    ) -> i32;
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DecoderMode {
    Hardware,
    Software,
    Unreported,
}

struct Session {
    decoder: NonNull<c_void>,
    width: u32,
    height: u32,
    mode: DecoderMode,
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
            mode: DecoderMode::Unreported,
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
        if self.mode == DecoderMode::Unreported {
            self.mode = match unsafe { hsx_decoder_hardware(self.decoder.as_ptr()) } {
                1 => DecoderMode::Hardware,
                0 => DecoderMode::Software,
                _ => DecoderMode::Unreported,
            };
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
    pub fn mode(&self) -> DecoderMode {
        self.session
            .as_ref()
            .map_or(DecoderMode::Unreported, |session| session.mode)
    }

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

pub fn contains_idr(unit: &[u8]) -> bool {
    unit.windows(4)
        .any(|bytes| bytes[..3] == [0, 0, 1] && bytes[3] & 0x1f == 5)
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
    #[ignore = "local performance measurement"]
    fn benchmark_4k_decode() -> Result<()> {
        use std::time::Instant;

        let sample = include_bytes!("../../../tests/fixtures/blue-4096x2160.h264");
        let mut decoder = Decoder::default();
        for _ in 0..8 {
            assert!(decoder.decode(sample)?.is_some());
        }
        let mut milliseconds = [0.0; 120];
        let started = Instant::now();
        for duration in &mut milliseconds {
            let frame_started = Instant::now();
            let frame = decoder.decode(sample)?.expect("missing 4K picture");
            *duration = frame_started.elapsed().as_secs_f64() * 1000.;
            std::hint::black_box(frame);
        }
        let throughput = milliseconds.len() as f64 / started.elapsed().as_secs_f64();
        milliseconds.sort_by(f64::total_cmp);
        let percentile = |p: usize| milliseconds[(milliseconds.len() * p).div_ceil(100) - 1];
        eprintln!(
            "4K H.264 decode {:?} · 120 frames · {:.1} fps · p50 {:.2} ms · p95 {:.2} ms",
            decoder.mode(),
            throughput,
            percentile(50),
            percentile(95)
        );
        Ok(())
    }

    #[test]
    fn decodes_full_width_4k_h264_at_the_capture_limit() -> Result<()> {
        let sample = include_bytes!("../../../tests/fixtures/blue-4096x2160.h264");
        let mut decoder = Decoder::default();
        let frame = decoder.decode(sample)?.expect("missing 4K picture");
        assert_eq!((frame.width, frame.height), (4096, 2160));
        assert_eq!(frame.bgra.len(), protocol::MAX_PIXELS * 4);
        assert!(frame.bgra.chunks_exact(4).all(|pixel| pixel[3] == 255));
        for offset in [0, frame.bgra.len() / 2, frame.bgra.len() - 4] {
            let pixel = &frame.bgra[offset..offset + 4];
            assert!(pixel[0] > 200 && pixel[1] < 25 && pixel[2] < 25);
        }
        eprintln!("4K H.264 decoder: {:?}", decoder.mode());
        Ok(())
    }

    #[test]
    fn decodes_h264_annex_b_with_video_toolbox() -> Result<()> {
        let sample = include_bytes!("../../../tests/fixtures/red-32x24.h264");
        let mut decoder = Decoder::default();
        let frame = decoder.decode(sample)?.expect("missing picture");
        eprintln!("VideoToolbox fixture decoder: {:?}", decoder.mode());
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
    #[test]
    fn decoder_uses_new_dimensions_after_h264_parameter_sets_change() -> Result<()> {
        let mut decoder = Decoder::default();
        for (sample, expected, channel) in [
            (
                include_bytes!("../../../tests/fixtures/red-32x24.h264").as_slice(),
                (32, 24),
                2,
            ),
            (
                include_bytes!("../../../tests/fixtures/blue-48x32.h264").as_slice(),
                (48, 32),
                0,
            ),
        ] {
            let frame = decoder.decode(sample)?.expect("missing picture");
            assert_eq!((frame.width, frame.height), expected);
            assert_eq!(frame.bgra.len(), (expected.0 * expected.1 * 4) as usize);
            assert!(frame.bgra[channel] > 200);
        }
        Ok(())
    }
}
