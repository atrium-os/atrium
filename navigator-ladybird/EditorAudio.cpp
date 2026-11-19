// The offline editor fixture receives no audio capability. Keep the upstream
// decoder factory failure path without loading a codec library in the jail.
#include <LibMedia/FFmpeg/FFmpegAudioConverter.h>
#include <LibMedia/FFmpeg/FFmpegAudioDecoder.h>
#include <LibMedia/FFmpeg/FFmpegAudioEncoder.h>

namespace Media::FFmpeg {

static DecoderError audio_unavailable()
{
    return DecoderError::with_description(DecoderErrorCategory::PermanentlyUnavailable,
        "Navigator editor has no audio capability"sv);
}

ErrorOr<NonnullOwnPtr<FFmpegAudioConverter>> FFmpegAudioConverter::try_create()
{
    return Error::from_errno(ENOTSUP);
}

DecoderErrorOr<NonnullOwnPtr<FFmpegAudioEncoder>> FFmpegAudioEncoder::try_create(CodecID, Audio::SampleSpecification const&, int)
{
    return audio_unavailable();
}

DecoderErrorOr<NonnullOwnPtr<FFmpegAudioDecoder>> FFmpegAudioDecoder::try_create(CodecID, Audio::SampleSpecification const&, ReadonlyBytes)
{
    return audio_unavailable();
}

FFmpegAudioConverter::~FFmpegAudioConverter() = default;
FFmpegAudioEncoder::~FFmpegAudioEncoder() = default;
FFmpegAudioDecoder::~FFmpegAudioDecoder() = default;

ErrorOr<void> FFmpegAudioConverter::set_output_sample_specification(Audio::SampleSpecification) { return Error::from_errno(ENOTSUP); }
ErrorOr<void> FFmpegAudioConverter::push_block(AudioBlock const&) { return Error::from_errno(ENOTSUP); }
DecoderErrorOr<void> FFmpegAudioConverter::retrieve_block(AudioBlock&) { return audio_unavailable(); }
void FFmpegAudioConverter::signal_end_of_stream() { VERIFY_NOT_REACHED(); }
void FFmpegAudioConverter::flush() { VERIFY_NOT_REACHED(); }

DecoderErrorOr<void> FFmpegAudioEncoder::receive_pcm_data(AK::Duration, AudioBlock const&) { return audio_unavailable(); }
DecoderErrorOr<bool> FFmpegAudioEncoder::write_next_packet(Packet&) { return audio_unavailable(); }

DecoderErrorOr<void> FFmpegAudioDecoder::receive_coded_data(CodedFrame const&) { return audio_unavailable(); }
DecoderErrorOr<void> FFmpegAudioDecoder::write_next_block(AudioBlock&) { return audio_unavailable(); }
void FFmpegAudioDecoder::signal_end_of_stream() { VERIFY_NOT_REACHED(); }
void FFmpegAudioDecoder::flush() { VERIFY_NOT_REACHED(); }

}
