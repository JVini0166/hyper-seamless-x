#define WIN32_LEAN_AND_MEAN
#define NOMINMAX
#include <windows.h>
#include <mfapi.h>
#include <mferror.h>
#include <mfidl.h>
#include <mftransform.h>
#include <codecapi.h>
#include <wmcodecdsp.h>
#include <wrl/client.h>
#include <cstdint>
#include <cstring>
#include <new>
#include <vector>

using Microsoft::WRL::ComPtr;

struct Encoder {
    ComPtr<IMFTransform> transform;
    UINT32 width = 0;
    UINT32 height = 0;
    bool com_started = false;
    bool mf_started = false;
    LONGLONG timestamp = 0;
    std::vector<std::uint8_t> output;
    std::vector<std::uint8_t> header;

    ~Encoder() {
        transform.Reset();
        if (mf_started) MFShutdown();
        if (com_started) CoUninitialize();
    }
};

#define HSX_TRY(operation) do { const HRESULT result = (operation); if (FAILED(result)) return result; } while (0)

static constexpr UINT32 frames_per_second = 60;

static HRESULT make_type(UINT32 width, UINT32 height, REFGUID subtype, IMFMediaType **result) {
    ComPtr<IMFMediaType> type;
    HSX_TRY(MFCreateMediaType(&type));
    HSX_TRY(type->SetGUID(MF_MT_MAJOR_TYPE, MFMediaType_Video));
    HSX_TRY(type->SetGUID(MF_MT_SUBTYPE, subtype));
    HSX_TRY(MFSetAttributeSize(type.Get(), MF_MT_FRAME_SIZE, width, height));
    HSX_TRY(MFSetAttributeRatio(type.Get(), MF_MT_FRAME_RATE, frames_per_second, 1));
    HSX_TRY(MFSetAttributeRatio(type.Get(), MF_MT_PIXEL_ASPECT_RATIO, 1, 1));
    HSX_TRY(type->SetUINT32(MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive));
    HSX_TRY(type->SetUINT32(MF_MT_YUV_MATRIX, MFVideoTransferMatrix_BT601));
    HSX_TRY(type->SetUINT32(MF_MT_VIDEO_NOMINAL_RANGE, MFNominalRange_16_235));
    *result = type.Detach();
    return S_OK;
}

static HRESULT initialize(Encoder &encoder, UINT32 width, UINT32 height, UINT32 bitrate) {
    HSX_TRY(CoInitializeEx(nullptr, COINIT_MULTITHREADED));
    encoder.com_started = true;
    HSX_TRY(MFStartup(MF_VERSION, MFSTARTUP_LITE));
    encoder.mf_started = true;
    HSX_TRY(CoCreateInstance(CLSID_CMSH264EncoderMFT, nullptr, CLSCTX_INPROC_SERVER,
                             IID_PPV_ARGS(&encoder.transform)));
    ComPtr<ICodecAPI> codec;
    if (SUCCEEDED(encoder.transform.As(&codec))) {
        VARIANT value{};
        value.vt = VT_BOOL;
        value.boolVal = VARIANT_TRUE;
        codec->SetValue(&CODECAPI_AVLowLatencyMode, &value);
        value.vt = VT_UI4;
        value.ulVal = 0;
        codec->SetValue(&CODECAPI_AVEncMPVDefaultBPictureCount, &value);
    }

    ComPtr<IMFMediaType> input;
    ComPtr<IMFMediaType> output;
    HSX_TRY(make_type(width, height, MFVideoFormat_H264, &output));
    HSX_TRY(output->SetUINT32(MF_MT_AVG_BITRATE, bitrate));
    HSX_TRY(output->SetUINT32(MF_MT_MPEG2_PROFILE, eAVEncH264VProfile_Base));
    HSX_TRY(encoder.transform->SetOutputType(0, output.Get(), 0));
    HSX_TRY(make_type(width, height, MFVideoFormat_NV12, &input));
    HSX_TRY(input->SetUINT32(MF_MT_DEFAULT_STRIDE, width));
    HSX_TRY(encoder.transform->SetInputType(0, input.Get(), 0));
    HSX_TRY(encoder.transform->ProcessMessage(MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, 0));
    HSX_TRY(encoder.transform->ProcessMessage(MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0));
    encoder.width = width;
    encoder.height = height;
    return S_OK;
}

static void to_nv12(const std::uint8_t *bgra, std::uint8_t *nv12, UINT32 width, UINT32 height) {
    auto *chroma = nv12 + static_cast<size_t>(width) * height;
    for (UINT32 y = 0; y < height; y += 2) {
        for (UINT32 x = 0; x < width; x += 2) {
            int r = 0, g = 0, b = 0;
            for (UINT32 dy = 0; dy < 2; ++dy) {
                for (UINT32 dx = 0; dx < 2; ++dx) {
                    const size_t index = static_cast<size_t>(y + dy) * width + x + dx;
                    const auto *pixel = bgra + index * 4;
                    nv12[index] = static_cast<std::uint8_t>(
                        16 + ((66 * pixel[2] + 129 * pixel[1] + 25 * pixel[0] + 128) >> 8));
                    r += pixel[2];
                    g += pixel[1];
                    b += pixel[0];
                }
            }
            const size_t index = static_cast<size_t>(y / 2) * width + x;
            chroma[index] = static_cast<std::uint8_t>(128 + ((-38 * r - 74 * g + 112 * b + 512) >> 10));
            chroma[index + 1] = static_cast<std::uint8_t>(128 + ((112 * r - 94 * g - 18 * b + 512) >> 10));
        }
    }
}

static HRESULT encode(Encoder &encoder, const std::uint8_t *pixels) {
    const DWORD bytes = encoder.width * encoder.height * 3 / 2;
    ComPtr<IMFMediaBuffer> input_buffer;
    HSX_TRY(MFCreateMemoryBuffer(bytes, &input_buffer));
    BYTE *target = nullptr;
    HSX_TRY(input_buffer->Lock(&target, nullptr, nullptr));
    to_nv12(pixels, target, encoder.width, encoder.height);
    HSX_TRY(input_buffer->Unlock());
    HSX_TRY(input_buffer->SetCurrentLength(bytes));

    ComPtr<IMFSample> input;
    HSX_TRY(MFCreateSample(&input));
    HSX_TRY(input->AddBuffer(input_buffer.Get()));
    constexpr LONGLONG frame_duration = 10000000 / frames_per_second;
    HSX_TRY(input->SetSampleTime(encoder.timestamp));
    HSX_TRY(input->SetSampleDuration(frame_duration));
    HSX_TRY(encoder.transform->ProcessInput(0, input.Get(), 0));
    encoder.timestamp += frame_duration;

    MFT_OUTPUT_STREAM_INFO info{};
    HSX_TRY(encoder.transform->GetOutputStreamInfo(0, &info));
    ComPtr<IMFSample> provided;
    if (!(info.dwFlags & MFT_OUTPUT_STREAM_PROVIDES_SAMPLES)) {
        ComPtr<IMFMediaBuffer> output_buffer;
        const DWORD capacity = info.cbSize ? info.cbSize : encoder.width * encoder.height * 4;
        HSX_TRY(MFCreateAlignedMemoryBuffer(capacity, info.cbAlignment ? info.cbAlignment - 1 : 0, &output_buffer));
        HSX_TRY(MFCreateSample(&provided));
        HSX_TRY(provided->AddBuffer(output_buffer.Get()));
    }
    MFT_OUTPUT_DATA_BUFFER output{};
    output.pSample = provided.Get();
    DWORD status = 0;
    const HRESULT hr = encoder.transform->ProcessOutput(0, 1, &output, &status);
    if (output.pEvents) output.pEvents->Release();
    ComPtr<IMFSample> owned;
    if (output.pSample != provided.Get()) owned.Attach(output.pSample);
    if (FAILED(hr)) return hr;
    if (!output.pSample) return E_UNEXPECTED;

    ComPtr<IMFMediaBuffer> encoded;
    HSX_TRY(output.pSample->ConvertToContiguousBuffer(&encoded));
    BYTE *data = nullptr;
    DWORD size = 0;
    HSX_TRY(encoded->GetCurrentLength(&size));
    if (size == 0 || size > 16 * 1024 * 1024) return E_UNEXPECTED;
    encoder.output.resize(size);
    HSX_TRY(encoded->Lock(&data, nullptr, nullptr));
    memcpy(encoder.output.data(), data, size);
    HSX_TRY(encoded->Unlock());
    if (encoder.timestamp == frame_duration) {
        ComPtr<IMFMediaType> type;
        UINT32 size = 0;
        if (SUCCEEDED(encoder.transform->GetOutputCurrentType(0, &type)) &&
            SUCCEEDED(type->GetBlobSize(MF_MT_MPEG_SEQUENCE_HEADER, &size)) && size <= 8192) {
            encoder.header.resize(size);
            if (FAILED(type->GetBlob(MF_MT_MPEG_SEQUENCE_HEADER, encoder.header.data(), size, nullptr))) {
                encoder.header.clear();
            }
        }
    }
    return S_OK;
}

extern "C" HRESULT hsx_encoder_create(UINT32 width, UINT32 height, UINT32 bitrate, Encoder **out) {
    auto *encoder = new (std::nothrow) Encoder();
    if (!encoder) return E_OUTOFMEMORY;
    const HRESULT status = initialize(*encoder, width, height, bitrate);
    if (FAILED(status)) {
        delete encoder;
        return status;
    }
    *out = encoder;
    return S_OK;
}

extern "C" void hsx_encoder_destroy(Encoder *encoder) { delete encoder; }

extern "C" HRESULT hsx_encoder_encode(Encoder *encoder, const std::uint8_t *pixels,
                                     const std::uint8_t **output, size_t *length,
                                     const std::uint8_t **header, size_t *header_length) {
    try {
        HSX_TRY(encode(*encoder, pixels));
        *output = encoder->output.data();
        *length = encoder->output.size();
        *header = encoder->header.data();
        *header_length = encoder->header.size();
        return S_OK;
    } catch (const std::bad_alloc &) {
        return E_OUTOFMEMORY;
    }
}
