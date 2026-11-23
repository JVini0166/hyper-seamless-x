use protocol::{Frame, Result};
use std::{ffi::c_void, ptr::NonNull};

unsafe extern "C" {
    fn hsx_encoder_create(width: u32, height: u32, out: *mut *mut c_void) -> i32;
    fn hsx_encoder_destroy(handle: *mut c_void);
    fn hsx_encoder_encode(
        handle: *mut c_void,
        pixels: *const u8,
        output: *mut *const u8,
        len: *mut usize,
        header: *mut *const u8,
        header_len: *mut usize,
    ) -> i32;
}

struct MediaFoundationEncoder {
    handle: NonNull<c_void>,
    width: u32,
    height: u32,
    first: bool,
}

impl MediaFoundationEncoder {
    pub fn new(width: u32, height: u32) -> Result<Self> {
        if width == 0
            || height == 0
            || width % 2 != 0
            || height % 2 != 0
            || u64::from(width) * u64::from(height) > protocol::MAX_PIXELS as u64
        {
            return Err("Dimensões incompatíveis com o encoder H.264".into());
        }
        let mut handle = std::ptr::null_mut();
        // Media Foundation and its COM apartment are owned by the current thread.
        let status = unsafe { hsx_encoder_create(width, height, &mut handle) };
        if status < 0 {
            return Err(format!("Media Foundation recusou o encoder H.264 ({status:#x})").into());
        }
        Ok(Self {
            handle: NonNull::new(handle).ok_or("Encoder H.264 indisponível")?,
            width,
            height,
            first: true,
        })
    }

    pub fn matches(&self, frame: &Frame) -> bool {
        (self.width, self.height) == (frame.width, frame.height)
    }

    pub fn encode(&mut self, frame: &Frame) -> Result<Vec<u8>> {
        if !self.matches(frame)
            || frame.bgra.len() != self.width as usize * self.height as usize * 4
        {
            return Err("Dimensões de captura mudaram".into());
        }
        let (mut output, mut header) = (std::ptr::null(), std::ptr::null());
        let (mut len, mut header_len) = (0, 0);
        // Native output is borrowed until the next call and copied before returning.
        let status = unsafe {
            hsx_encoder_encode(
                self.handle.as_ptr(),
                frame.bgra.as_ptr(),
                &mut output,
                &mut len,
                &mut header,
                &mut header_len,
            )
        };
        if status < 0 || output.is_null() || len == 0 {
            return Err(
                format!("Media Foundation não produziu um quadro H.264 ({status:#x})").into(),
            );
        }
        let output = unsafe { std::slice::from_raw_parts(output, len) };
        if !output.starts_with(&[0, 0, 1]) && !output.starts_with(&[0, 0, 0, 1]) {
            return Err("Encoder retornou H.264 fora do formato Annex B".into());
        }
        let mut unit = Vec::new();
        if self.first {
            let has = |bytes: &[u8], kind| {
                bytes
                    .windows(4)
                    .any(|bytes| bytes[..3] == [0, 0, 1] && bytes[3] & 0x1f == kind)
            };
            if !has(output, 5) {
                return Err("O primeiro quadro do encoder H.264 não é IDR".into());
            }
            if !has(output, 7) || !has(output, 8) {
                if header.is_null() || header_len == 0 {
                    return Err("Encoder H.264 não forneceu SPS/PPS".into());
                }
                let header = unsafe { std::slice::from_raw_parts(header, header_len) };
                if !has(header, 7) || !has(header, 8) {
                    return Err("Encoder H.264 não forneceu SPS/PPS em Annex B".into());
                }
                unit.extend_from_slice(header);
            }
            self.first = false;
        }
        unit.extend_from_slice(output);
        Ok(unit)
    }
}

impl Drop for MediaFoundationEncoder {
    fn drop(&mut self) {
        unsafe { hsx_encoder_destroy(self.handle.as_ptr()) };
    }
}

enum Backend {
    Nvidia(Box<crate::nvenc::Encoder>),
    MediaFoundation(MediaFoundationEncoder),
}

pub struct Encoder {
    width: u32,
    height: u32,
    backend: Backend,
    first: bool,
}

impl Encoder {
    pub fn new(width: u32, height: u32) -> Result<Self> {
        let backend = match crate::nvenc::Encoder::new(width, height) {
            Ok(nvidia) => Backend::Nvidia(Box::new(nvidia)),
            Err(_) => Backend::MediaFoundation(MediaFoundationEncoder::new(width, height)?),
        };
        Ok(Self {
            width,
            height,
            backend,
            first: true,
        })
    }

    pub fn matches(&self, frame: &Frame) -> bool {
        (self.width, self.height) == (frame.width, frame.height)
    }

    pub fn encode(&mut self, frame: &Frame) -> Result<Vec<u8>> {
        match &mut self.backend {
            Backend::Nvidia(encoder) => {
                if let Ok(unit) = encoder.encode(frame)
                    && (!self.first || ready_to_decode(&unit))
                {
                    self.first = false;
                    return Ok(unit);
                }
                self.backend =
                    Backend::MediaFoundation(MediaFoundationEncoder::new(self.width, self.height)?);
            }
            Backend::MediaFoundation(encoder) => return encoder.encode(frame),
        }
        match &mut self.backend {
            Backend::MediaFoundation(encoder) => encoder.encode(frame),
            Backend::Nvidia(_) => unreachable!(),
        }
    }
}

fn ready_to_decode(unit: &[u8]) -> bool {
    (unit.starts_with(&[0, 0, 1]) || unit.starts_with(&[0, 0, 0, 1]))
        && [5, 7, 8].into_iter().all(|kind| {
            unit.windows(4)
                .any(|bytes| bytes[..3] == [0, 0, 1] && bytes[3] & 0x1f == kind)
        })
}
