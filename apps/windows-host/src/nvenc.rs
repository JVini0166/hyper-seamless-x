use libloading::os::windows::{LOAD_LIBRARY_SEARCH_SYSTEM32, Library};
use mini_nvcodec::{API_VERSION_LADDER, NvencApiVersion, guids::*, nvenc_sys::*};
use protocol::{Frame, Result};
use std::{ffi::c_void, ptr};
use windows::{
    Win32::{
        Foundation::HMODULE,
        Graphics::{
            Direct3D::D3D_DRIVER_TYPE_UNKNOWN,
            Direct3D11::*,
            Dxgi::{Common::*, CreateDXGIFactory1, IDXGIAdapter1, IDXGIDevice, IDXGIFactory1},
        },
    },
    core::Interface,
};

fn check(status: NVENCSTATUS) -> Result<()> {
    if status == NVENCSTATUS::SUCCESS {
        Ok(())
    } else {
        Err(format!("NVENC retornou {}", status.0).into())
    }
}

macro_rules! nv_call {
    ($api:expr, $method:ident $(, $arg:expr)* $(,)?) => {{
        let entry = $api.$method.ok_or(concat!("NVENC não oferece ", stringify!($method)))?;
        check(unsafe { entry($($arg),*) })
    }};
}

pub struct Encoder {
    // Keep the driver and its device alive until after Drop releases NVENC handles.
    _library: Library,
    _device: ID3D11Device,
    api: NV_ENCODE_API_FUNCTION_LIST,
    version: NvencApiVersion,
    session: *mut c_void,
    registered: NV_ENC_REGISTERED_PTR,
    bitstream: NV_ENC_OUTPUT_PTR,
    context: ID3D11DeviceContext,
    texture: ID3D11Texture2D,
    width: u32,
    height: u32,
    frame_index: u32,
    bitrate: u32,
}

// The encoder exclusively owns its handles. Callers serialize access to the
// session and its immediate context, including capture callbacks.
unsafe impl Send for Encoder {}

pub(crate) fn target_bitrate(width: u32, height: u32, limit: u32) -> u32 {
    let native = width
        .saturating_mul(height)
        .saturating_mul(2)
        .max(1_500_000);
    if limit == 0 {
        native
    } else {
        native.min(limit)
    }
}

impl Encoder {
    pub fn new(width: u32, height: u32, limit: u32) -> Result<Self> {
        let (device, context) = nvidia_device()?;
        Self::with_device(width, height, limit, device, context)
    }

    pub fn from_capture_device(
        width: u32,
        height: u32,
        limit: u32,
        device: &ID3D11Device,
        context: &ID3D11DeviceContext,
    ) -> Result<Self> {
        let dxgi: IDXGIDevice = device.cast()?;
        if unsafe { dxgi.GetAdapter()?.GetDesc()? }.VendorId != 0x10de {
            return Err("A captura está em outro adaptador".into());
        }
        Self::with_device(width, height, limit, device.clone(), context.clone())
    }

    fn with_device(
        width: u32,
        height: u32,
        limit: u32,
        device: ID3D11Device,
        context: ID3D11DeviceContext,
    ) -> Result<Self> {
        if width == 0
            || height == 0
            || !width.is_multiple_of(2)
            || !height.is_multiple_of(2)
            || u64::from(width) * u64::from(height) > protocol::MAX_PIXELS as u64
        {
            return Err("Dimensões incompatíveis com NVENC".into());
        }
        // Load only the installed Windows driver library, never a sibling DLL.
        let library =
            unsafe { Library::load_with_flags("nvEncodeAPI64.dll", LOAD_LIBRARY_SEARCH_SYSTEM32)? };
        let (api, version) = unsafe {
            let max = library.get::<PNVENCODEAPIGETMAXSUPPORTEDVERSION>(
                b"NvEncodeAPIGetMaxSupportedVersion\0",
            )?;
            let create =
                library.get::<PNVENCODEAPICREATEINSTANCE>(b"NvEncodeAPICreateInstance\0")?;
            let mut driver_max = 0;
            check(max(&mut driver_max))?;
            let version = API_VERSION_LADDER
                .into_iter()
                .find(|version| version.supported_by(driver_max))
                .ok_or("Driver NVENC incompatível")?;
            let mut api: NV_ENCODE_API_FUNCTION_LIST = std::mem::zeroed();
            api.version = version.function_list_ver();
            check(create(&mut api))?;
            (api, version)
        };
        let desc = D3D11_TEXTURE2D_DESC {
            Width: width,
            Height: height,
            MipLevels: 1,
            ArraySize: 1,
            Format: DXGI_FORMAT_B8G8R8A8_UNORM,
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            Usage: D3D11_USAGE_DEFAULT,
            BindFlags: D3D11_BIND_SHADER_RESOURCE.0 as u32,
            ..Default::default()
        };
        let mut texture = None;
        unsafe { device.CreateTexture2D(&desc, None, Some(&mut texture))? };
        let texture = texture.ok_or("Textura NVENC indisponível")?;
        let mut session = ptr::null_mut();
        // NVENC requires all reserved input fields to be zero.
        let mut open: NV_ENC_OPEN_ENCODE_SESSION_EX_PARAMS = unsafe { std::mem::zeroed() };
        open.version = version.open_encode_session_ex_params_ver();
        open.apiVersion = version.nvencapi_version();
        open.deviceType = NV_ENC_DEVICE_TYPE::DIRECTX;
        open.device = device.as_raw();
        nv_call!(api, nvEncOpenEncodeSessionEx, &mut open, &mut session)?;
        if session.is_null() {
            return Err("Sessão NVENC indisponível".into());
        }
        let mut encoder = Self {
            _library: library,
            _device: device,
            api,
            version,
            session,
            context,
            texture,
            width,
            height,
            frame_index: 0,
            bitrate: target_bitrate(width, height, limit),
            registered: ptr::null_mut(),
            bitstream: ptr::null_mut(),
        };
        encoder.initialize()?;
        Ok(encoder)
    }

    fn initialize(&mut self) -> Result<()> {
        let tuning = NV_ENC_TUNING_INFO::ULTRA_LOW_LATENCY;
        let mut preset: NV_ENC_PRESET_CONFIG = unsafe { std::mem::zeroed() };
        preset.version = self.version.preset_config_ver();
        preset.presetCfg.version = self.version.config_ver();
        nv_call!(
            self.api,
            nvEncGetEncodePresetConfigEx,
            self.session,
            NV_ENC_CODEC_H264_GUID,
            NV_ENC_PRESET_P3_GUID,
            tuning,
            &mut preset
        )?;
        preset.presetCfg.gopLength = 60;
        preset.presetCfg.frameIntervalP = 1;
        let rate = &mut preset.presetCfg.rcParams;
        rate.rateControlMode = NV_ENC_PARAMS_RC_MODE::CBR;
        rate.averageBitRate = self.bitrate;
        rate.maxBitRate = rate.averageBitRate;
        rate.vbvBufferSize = rate.averageBitRate / 30;
        rate.vbvInitialDelay = rate.vbvBufferSize;
        rate.set_enableLookahead(0);
        let mut params: NV_ENC_INITIALIZE_PARAMS = unsafe { std::mem::zeroed() };
        params.version = self.version.initialize_params_ver();
        params.encodeGUID = NV_ENC_CODEC_H264_GUID;
        params.presetGUID = NV_ENC_PRESET_P3_GUID;
        params.encodeWidth = self.width;
        params.encodeHeight = self.height;
        params.darWidth = self.width;
        params.darHeight = self.height;
        params.frameRateNum = 30;
        params.frameRateDen = 1;
        params.enablePTD = 1;
        params.tuningInfo = tuning;
        params.encodeConfig = &mut preset.presetCfg;
        nv_call!(self.api, nvEncInitializeEncoder, self.session, &mut params)?;

        let mut input: NV_ENC_REGISTER_RESOURCE = unsafe { std::mem::zeroed() };
        input.version = self.version.register_resource_ver();
        input.resourceType = NV_ENC_INPUT_RESOURCE_TYPE::DIRECTX;
        input.resourceToRegister = self.texture.as_raw();
        input.width = self.width;
        input.height = self.height;
        input.bufferFormat = NV_ENC_BUFFER_FORMAT::ARGB; // BGRA byte order on little-endian Windows.
        input.bufferUsage = NV_ENC_BUFFER_USAGE::INPUT_IMAGE;
        nv_call!(self.api, nvEncRegisterResource, self.session, &mut input)?;
        self.registered = input.registeredResource;

        let mut output: NV_ENC_CREATE_BITSTREAM_BUFFER = unsafe { std::mem::zeroed() };
        output.version = self.version.create_bitstream_buffer_ver();
        nv_call!(
            self.api,
            nvEncCreateBitstreamBuffer,
            self.session,
            &mut output
        )?;
        self.bitstream = output.bitstreamBuffer;
        if self.registered.is_null() || self.bitstream.is_null() {
            return Err("Buffers NVENC indisponíveis".into());
        }
        Ok(())
    }

    pub fn encode(&mut self, frame: &Frame) -> Result<Vec<u8>> {
        if (frame.width, frame.height) != (self.width, self.height)
            || frame.bgra.len() != self.width as usize * self.height as usize * 4
        {
            return Err("Dimensões da captura mudaram".into());
        }
        // The texture is no longer mapped by NVENC when the upload starts.
        unsafe {
            self.context.UpdateSubresource(
                &self.texture,
                0,
                None,
                frame.bgra.as_ptr().cast(),
                self.width * 4,
                0,
            );
        }
        self.encode_uploaded(false)
    }

    pub fn matches(&self, width: u32, height: u32, limit: u32) -> bool {
        (self.width, self.height) == (width, height)
            && self.bitrate == target_bitrate(width, height, limit)
    }

    pub fn encode_capture_texture(
        &mut self,
        source: &ID3D11Texture2D,
        force_keyframe: bool,
    ) -> Result<Vec<u8>> {
        let mut desc = D3D11_TEXTURE2D_DESC::default();
        unsafe { source.GetDesc(&mut desc) };
        if desc.Width < self.width
            || desc.Height < self.height
            || desc.Format != DXGI_FORMAT_B8G8R8A8_UNORM
            || desc.SampleDesc.Count != 1
        {
            return Err("Textura de captura incompatível com NVENC".into());
        }
        let region = D3D11_BOX {
            left: 0,
            top: 0,
            front: 0,
            right: self.width,
            bottom: self.height,
            back: 1,
        };
        // Both textures belong to the capture device; this copy stays on the GPU.
        unsafe {
            self.context
                .CopySubresourceRegion(&self.texture, 0, 0, 0, 0, source, 0, Some(&region))
        };
        self.encode_uploaded(force_keyframe)
    }

    fn encode_uploaded(&mut self, force_keyframe: bool) -> Result<Vec<u8>> {
        let mut input: NV_ENC_MAP_INPUT_RESOURCE = unsafe { std::mem::zeroed() };
        input.version = self.version.map_input_resource_ver();
        input.registeredResource = self.registered;
        nv_call!(self.api, nvEncMapInputResource, self.session, &mut input)?;
        let result = self.encode_mapped(&input, force_keyframe);
        let cleanup = nv_call!(
            self.api,
            nvEncUnmapInputResource,
            self.session,
            input.mappedResource
        );
        let unit = result?;
        cleanup?;
        self.frame_index = self.frame_index.wrapping_add(1);
        Ok(unit)
    }

    fn encode_mapped(
        &self,
        input: &NV_ENC_MAP_INPUT_RESOURCE,
        force_keyframe: bool,
    ) -> Result<Vec<u8>> {
        let mut picture: NV_ENC_PIC_PARAMS = unsafe { std::mem::zeroed() };
        picture.version = self.version.pic_params_ver();
        picture.inputWidth = self.width;
        picture.inputHeight = self.height;
        picture.inputPitch = self.width * 4;
        picture.inputBuffer = input.mappedResource;
        picture.outputBitstream = self.bitstream;
        picture.bufferFmt = input.mappedBufferFmt;
        picture.pictureStruct = NV_ENC_PIC_STRUCT::FRAME;
        picture.frameIdx = self.frame_index;
        picture.inputTimeStamp = u64::from(self.frame_index) * 333_333;
        picture.inputDuration = 333_333;
        if force_keyframe || self.frame_index.is_multiple_of(60) {
            picture.encodePicFlags = NV_ENC_PIC_FLAG_FORCEIDR | NV_ENC_PIC_FLAG_OUTPUT_SPSPPS;
        }
        nv_call!(self.api, nvEncEncodePicture, self.session, &mut picture)?;

        let mut buffer: NV_ENC_LOCK_BITSTREAM = unsafe { std::mem::zeroed() };
        buffer.version = self.version.lock_bitstream_ver();
        buffer.outputBitstream = self.bitstream;
        nv_call!(self.api, nvEncLockBitstream, self.session, &mut buffer)?;
        let bytes = if !buffer.bitstreamBufferPtr.is_null()
            && (1..=16 * 1024 * 1024).contains(&buffer.bitstreamSizeInBytes)
        {
            // The driver owns the bytes until unlock.
            unsafe {
                std::slice::from_raw_parts(
                    buffer.bitstreamBufferPtr.cast::<u8>(),
                    buffer.bitstreamSizeInBytes as usize,
                )
            }
            .to_vec()
        } else {
            Vec::new()
        };
        nv_call!(self.api, nvEncUnlockBitstream, self.session, self.bitstream)?;
        if bytes.is_empty() {
            return Err("NVENC retornou um quadro inválido".into());
        }
        Ok(bytes)
    }
}

impl Drop for Encoder {
    fn drop(&mut self) {
        unsafe {
            if let Some(destroy) = self.api.nvEncDestroyBitstreamBuffer
                && !self.bitstream.is_null()
            {
                destroy(self.session, self.bitstream);
            }
            if let Some(unregister) = self.api.nvEncUnregisterResource
                && !self.registered.is_null()
            {
                unregister(self.session, self.registered);
            }
            if let Some(destroy) = self.api.nvEncDestroyEncoder {
                destroy(self.session);
            }
        }
    }
}

fn nvidia_device() -> Result<(ID3D11Device, ID3D11DeviceContext)> {
    let factory: IDXGIFactory1 = unsafe { CreateDXGIFactory1()? };
    for index in 0..32 {
        let Ok(adapter): windows::core::Result<IDXGIAdapter1> =
            (unsafe { factory.EnumAdapters1(index) })
        else {
            break;
        };
        if unsafe { adapter.GetDesc1()? }.VendorId != 0x10de {
            continue;
        }
        let (mut device, mut context) = (None, None);
        unsafe {
            D3D11CreateDevice(
                &adapter,
                D3D_DRIVER_TYPE_UNKNOWN,
                HMODULE::default(),
                D3D11_CREATE_DEVICE_BGRA_SUPPORT,
                None,
                D3D11_SDK_VERSION,
                Some(&mut device),
                None,
                Some(&mut context),
            )?;
        }
        return Ok((
            device.ok_or("Dispositivo NVIDIA indisponível")?,
            context.ok_or("Contexto NVIDIA indisponível")?,
        ));
    }
    Err("Nenhum adaptador NVIDIA encontrado".into())
}
