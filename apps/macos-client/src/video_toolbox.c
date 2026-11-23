#include <CoreFoundation/CoreFoundation.h>
#include <CoreMedia/CoreMedia.h>
#include <CoreVideo/CoreVideo.h>
#include <VideoToolbox/VideoToolbox.h>
#include <stdint.h>
#include <stdlib.h>
#include <string.h>

typedef struct {
    CMVideoFormatDescriptionRef format;
    VTDecompressionSessionRef session;
} HsxDecoder;

typedef struct {
    uint8_t *bytes;
    size_t width;
    size_t height;
    OSStatus result;
} HsxImage;

static void on_image(void *context, void *source, OSStatus status,
                     VTDecodeInfoFlags flags, CVImageBufferRef image,
                     CMTime presentation, CMTime duration) {
    (void)context; (void)flags; (void)presentation; (void)duration;
    HsxImage *out = source;
    out->result = status;
    if (status != noErr || image == NULL) {
        if (status == noErr) out->result = kVTVideoDecoderBadDataErr;
        return;
    }
    if (CVPixelBufferGetWidth(image) != out->width ||
        CVPixelBufferGetHeight(image) != out->height ||
        CVPixelBufferGetPixelFormatType(image) != kCVPixelFormatType_32BGRA ||
        CVPixelBufferGetBytesPerRow(image) < out->width * 4) {
        out->result = kVTVideoDecoderBadDataErr;
        return;
    }
    out->result = CVPixelBufferLockBaseAddress(image, kCVPixelBufferLock_ReadOnly);
    if (out->result != noErr) return;
    const uint8_t *base = CVPixelBufferGetBaseAddress(image);
    const size_t stride = CVPixelBufferGetBytesPerRow(image);
    if (base == NULL) {
        out->result = kVTVideoDecoderBadDataErr;
    } else {
        for (size_t row = 0; row < out->height; ++row) {
            memcpy(out->bytes + row * out->width * 4, base + row * stride, out->width * 4);
        }
    }
    CVPixelBufferUnlockBaseAddress(image, kCVPixelBufferLock_ReadOnly);
}

void hsx_decoder_destroy(HsxDecoder *decoder) {
    if (decoder->session) {
        VTDecompressionSessionInvalidate(decoder->session);
        CFRelease(decoder->session);
    }
    if (decoder->format) CFRelease(decoder->format);
    free(decoder);
}

OSStatus hsx_decoder_create(const uint8_t *sps, size_t sps_len,
                            const uint8_t *pps, size_t pps_len, uint64_t max_pixels,
                            HsxDecoder **out, uint32_t *width, uint32_t *height) {
    HsxDecoder *decoder = calloc(1, sizeof(*decoder));
    if (decoder == NULL) return kVTAllocationFailedErr;
    const uint8_t *sets[] = {sps, pps};
    const size_t lengths[] = {sps_len, pps_len};
    OSStatus status = CMVideoFormatDescriptionCreateFromH264ParameterSets(
        kCFAllocatorDefault, 2, sets, lengths, 4, &decoder->format);
    if (status != noErr) goto fail;
    CMVideoDimensions dimensions = CMVideoFormatDescriptionGetDimensions(decoder->format);
    if (dimensions.width <= 0 || dimensions.height <= 0 ||
        (uint64_t)dimensions.width * (uint64_t)dimensions.height > max_pixels) {
        status = kVTVideoDecoderBadDataErr;
        goto fail;
    }
    const void *decoder_key = kVTVideoDecoderSpecification_EnableHardwareAcceleratedVideoDecoder;
    const void *decoder_value = kCFBooleanTrue;
    CFDictionaryRef settings = CFDictionaryCreate(
        NULL, &decoder_key, &decoder_value, 1,
        &kCFTypeDictionaryKeyCallBacks, &kCFTypeDictionaryValueCallBacks);
    const int32_t bgra = kCVPixelFormatType_32BGRA;
    CFNumberRef pixel_format = CFNumberCreate(NULL, kCFNumberSInt32Type, &bgra);
    const void *pixel_key = kCVPixelBufferPixelFormatTypeKey;
    const void *pixel_value = pixel_format;
    CFDictionaryRef attributes = pixel_format ? CFDictionaryCreate(
        NULL, &pixel_key, &pixel_value, 1,
        &kCFTypeDictionaryKeyCallBacks, &kCFTypeDictionaryValueCallBacks) : NULL;
    const VTDecompressionOutputCallbackRecord callback = {on_image, NULL};
    status = (settings && attributes) ? VTDecompressionSessionCreate(
        NULL, decoder->format, settings, attributes, &callback, &decoder->session)
        : kVTAllocationFailedErr;
    if (attributes) CFRelease(attributes);
    if (pixel_format) CFRelease(pixel_format);
    if (settings) CFRelease(settings);
    if (status != noErr) goto fail;
    *width = (uint32_t)dimensions.width;
    *height = (uint32_t)dimensions.height;
    *out = decoder;
    return noErr;

fail:
    hsx_decoder_destroy(decoder);
    return status;
}

OSStatus hsx_decoder_decode(HsxDecoder *decoder, const uint8_t *sample,
                            size_t length, uint8_t *output, uint32_t width, uint32_t height) {
    CMBlockBufferRef block = NULL;
    CMSampleBufferRef buffer = NULL;
    OSStatus status = CMBlockBufferCreateWithMemoryBlock(
        NULL, NULL, length, kCFAllocatorDefault, NULL, 0, length,
        kCMBlockBufferAssureMemoryNowFlag, &block);
    if (status == noErr) status = CMBlockBufferReplaceDataBytes(sample, block, 0, length);
    if (status == noErr) status = CMSampleBufferCreateReady(
        NULL, block, decoder->format, 1, 0, NULL, 1, &length, &buffer);
    if (status == noErr) {
        HsxImage result = {output, width, height, kVTVideoDecoderBadDataErr};
        // A zero flag mask makes the output callback synchronous.
        status = VTDecompressionSessionDecodeFrame(decoder->session, buffer, 0, &result, NULL);
        if (status == noErr) status = result.result;
    }
    if (buffer) CFRelease(buffer);
    if (block) CFRelease(block);
    return status;
}

int32_t hsx_decoder_hardware(HsxDecoder *decoder) {
    CFTypeRef value = NULL;
    const OSStatus status = VTSessionCopyProperty(
        decoder->session, kVTDecompressionPropertyKey_UsingHardwareAcceleratedVideoDecoder,
        NULL, &value);
    int32_t result = -1;
    if (status == noErr && value && CFGetTypeID(value) == CFBooleanGetTypeID()) {
        result = CFBooleanGetValue((CFBooleanRef)value) ? 1 : 0;
    }
    if (value) CFRelease(value);
    return result;
}
