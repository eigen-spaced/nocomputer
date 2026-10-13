#include "geometry.hpp"
#include <SDL3/SDL.h>
#include <SDL3/SDL_main.h>
extern "C" {
#include <libavcodec/avcodec.h>
#include <libavformat/avformat.h>
#include <libavutil/hwcontext.h>
#include <libavutil/pixdesc.h>
#include <libswscale/swscale.h>
}
#include <chrono>
#include <filesystem>
#include <iostream>
#include <memory>
#include <stdexcept>
#include <string>
#include <thread>
#include <vector>

namespace {
void check(int result, const char* operation) {
    if (result >= 0) return;
    char error[AV_ERROR_MAX_STRING_SIZE];
    av_strerror(result, error, sizeof(error));
    throw std::runtime_error(std::string(operation) + ": " + error);
}
void sdl(bool ok, const char* operation) {
    if (!ok) throw std::runtime_error(std::string(operation) + ": " + SDL_GetError());
}
struct Options {
    std::string path;
    bool checkOnly = false, smoke = false;
    enum Hardware { automatic, software, required } hardware = automatic;
};
Options options(int argc, char** argv) {
    Options result;
    for (int i = 1; i < argc; ++i) {
        const std::string argument(argv[i]);
        if (argument == "--check") result.checkOnly = true;
        else if (argument == "--smoke") result.smoke = true;
        else if (argument == "--software") result.hardware = Options::software;
        else if (argument == "--require-hardware") result.hardware = Options::required;
        else if (argument == "--help") {
            std::cout << "nocomputer-viewer [--software|--require-hardware] [--check|--smoke] fixture.h264\n"
                "Local synthetic fixture only, <=8 MiB. F fullscreen, N native/fit, arrows pan, R reset, Esc quit.\n";
            std::exit(0);
        } else if (argument.empty() || argument[0] == '-' || !result.path.empty()) {
            throw std::runtime_error("Unknown option or multiple input files; use --help");
        } else result.path = argument;
    }
    if (result.path.empty()) throw std::runtime_error("Supply a local synthetic .h264 fixture; use --help");
    const auto path = std::filesystem::u8path(result.path);
    if (!std::filesystem::is_regular_file(path) || path.extension() != ".h264")
        throw std::runtime_error("Only an existing local regular .h264 file is accepted (no URLs)");
    const auto size = std::filesystem::file_size(path);
    if (size == 0 || size > 8 * 1024 * 1024) throw std::runtime_error("Fixture must be 1 byte to 8 MiB");
    if (result.checkOnly && result.smoke) throw std::runtime_error("Choose --check or --smoke, not both");
    // Absolute filesystem path avoids FFmpeg interpreting a relative colon as a protocol.
    result.path = std::filesystem::absolute(path).u8string();
    return result;
}
struct Decoder {
    AVFormatContext* format = nullptr;
    AVCodecContext* codec = nullptr;
    AVBufferRef* device = nullptr;
    int stream = -1;
    AVPixelFormat hardwareFormat = AV_PIX_FMT_NONE;
    bool attemptedHardware = false;
    ~Decoder() {
        avcodec_free_context(&codec);
        av_buffer_unref(&device);
        avformat_close_input(&format);
    }
    static AVPixelFormat selectFormat(AVCodecContext* context, const AVPixelFormat* formats) {
        auto& self = *static_cast<Decoder*>(context->opaque);
        for (auto candidate = formats; *candidate != AV_PIX_FMT_NONE; ++candidate)
            if (*candidate == self.hardwareFormat) return *candidate;
        // Do not claim success or silently switch modes if hardware negotiation fails.
        return AV_PIX_FMT_NONE;
    }
    void enableD3D11(const AVCodec* decoder, Options::Hardware mode) {
        // FFmpeg's device interface is portable, so this setup code is compiled
        // on Mac too. Only Windows invokes it; runtime/driver proof still needs Windows.
        for (int i = 0; const auto* config = avcodec_get_hw_config(decoder, i); ++i) {
            if (config->device_type == AV_HWDEVICE_TYPE_D3D11VA
                && (config->methods & AV_CODEC_HW_CONFIG_METHOD_HW_DEVICE_CTX)) {
                hardwareFormat = config->pix_fmt;
                break;
            }
        }
        const int result = hardwareFormat == AV_PIX_FMT_NONE ? AVERROR(ENOSYS)
            : av_hwdevice_ctx_create(&device, AV_HWDEVICE_TYPE_D3D11VA, nullptr, nullptr, 0);
        if (result >= 0) {
            codec->hw_device_ctx = av_buffer_ref(device);
            if (!codec->hw_device_ctx) throw std::bad_alloc();
            codec->opaque = this;
            codec->get_format = selectFormat;
            attemptedHardware = true;
        } else {
            if (mode == Options::required) check(result, "D3D11 hardware unavailable");
            std::cerr << "D3D11 unavailable; explicitly falling back to software decoding.\n";
        }
    }
    void open(const Options& opt) {
        AVDictionary* attributes = nullptr;
        av_dict_set(&attributes, "protocol_whitelist", "file", 0);
        const int opened = avformat_open_input(&format, opt.path.c_str(), av_find_input_format("h264"), &attributes);
        av_dict_free(&attributes);
        check(opened, "Open fixture");
        check(avformat_find_stream_info(format, nullptr), "Inspect H.264");
        stream = av_find_best_stream(format, AVMEDIA_TYPE_VIDEO, -1, -1, nullptr, 0);
        check(stream, "Find video stream");
        const auto* parameters = format->streams[stream]->codecpar;
        if (parameters->codec_id != AV_CODEC_ID_H264) throw std::runtime_error("Fixture is not H.264");
        const AVCodec* decoder = avcodec_find_decoder(AV_CODEC_ID_H264);
        if (!decoder) throw std::runtime_error("FFmpeg build has no H.264 decoder");
        codec = avcodec_alloc_context3(decoder);
        if (!codec) throw std::bad_alloc();
        check(avcodec_parameters_to_context(codec, parameters), "Configure decoder");
        codec->thread_count = 1;
        codec->err_recognition = AV_EF_EXPLODE;
        codec->max_pixels = 16 * 1024 * 1024;
        if (opt.hardware != Options::software) {
#ifdef _WIN32
            enableD3D11(decoder, opt.hardware);
#else
            if (opt.hardware == Options::required) throw std::runtime_error("D3D11 hardware test requires Windows");
            std::cerr << "Non-Windows smoke test: software decoding only.\n";
#endif
        }
        check(avcodec_open2(codec, decoder, nullptr), "Open decoder (try --software if hardware fails)");
    }
};
struct FrameFree { void operator()(AVFrame* value) const { av_frame_free(&value); } };
struct PacketFree { void operator()(AVPacket* value) const { av_packet_free(&value); } };
using Frame = std::unique_ptr<AVFrame, FrameFree>;
using Packet = std::unique_ptr<AVPacket, PacketFree>;
struct Display {
    SDL_Window* window = nullptr;
    SDL_Renderer* renderer = nullptr;
    SDL_Texture* texture = nullptr;
    SwsContext* converter = nullptr;
    std::vector<uint8_t> pixels;
    int width = 0, height = 0;
    bool native = false, fullscreen = false, quit = false;
    float panX = 0, panY = 0;
    std::string rendererName = "none";
    ~Display() {
        sws_freeContext(converter);
        SDL_DestroyTexture(texture);
        SDL_DestroyRenderer(renderer);
        SDL_DestroyWindow(window);
        SDL_Quit();
    }
    void open(bool smoke) {
        sdl(SDL_Init(SDL_INIT_VIDEO), "SDL initialization");
        window = SDL_CreateWindow("NoComputer — offline video PoC (F fullscreen, N native, Esc quit)",
            1000, 700, SDL_WINDOW_RESIZABLE | SDL_WINDOW_HIGH_PIXEL_DENSITY | (smoke ? SDL_WINDOW_HIDDEN : 0));
        sdl(window != nullptr, "Create window");
        renderer = SDL_CreateRenderer(window, nullptr);
        sdl(renderer != nullptr, "Create renderer");
        rendererName = SDL_GetRendererName(renderer);
    }
    void events() {
        SDL_Event event;
        while (SDL_PollEvent(&event)) {
            if (event.type == SDL_EVENT_QUIT) quit = true;
            if (event.type != SDL_EVENT_KEY_DOWN || event.key.repeat) continue;
            switch (event.key.key) {
            case SDLK_ESCAPE: quit = true; break;
            case SDLK_F: fullscreen = !fullscreen; sdl(SDL_SetWindowFullscreen(window, fullscreen), "Fullscreen"); break;
            case SDLK_N: native = !native; panX = panY = 0; break;
            case SDLK_R: panX = panY = 0; break;
            case SDLK_LEFT: panX += 64; break;
            case SDLK_RIGHT: panX -= 64; break;
            case SDLK_UP: panY += 64; break;
            case SDLK_DOWN: panY -= 64; break;
            default: break;
            }
        }
    }
    void upload(AVFrame* frame) {
        if (frame->width <= 0 || frame->height <= 0 || frame->width > 8192 || frame->height > 8192
            || int64_t(frame->width) * frame->height > 16 * 1024 * 1024)
            throw std::runtime_error("Unsupported frame dimensions");
        if (width != frame->width || height != frame->height) {
            SDL_DestroyTexture(texture);
            texture = nullptr;
            width = frame->width; height = frame->height;
            pixels.resize(size_t(width) * height * 4);
            texture = SDL_CreateTexture(renderer, SDL_PIXELFORMAT_BGRA32, SDL_TEXTUREACCESS_STREAMING, width, height);
            sdl(texture != nullptr, "Create texture");
            sdl(SDL_SetTextureBlendMode(texture, SDL_BLENDMODE_NONE), "Opaque texture");
        }
        converter = sws_getCachedContext(converter, width, height, AVPixelFormat(frame->format),
            width, height, AV_PIX_FMT_BGRA, SWS_BILINEAR, nullptr, nullptr, nullptr);
        if (!converter) throw std::runtime_error("Pixel conversion setup failed");
        // Honour the matrix and video range. This PoC does not implement ICC/HDR display management.
        const int matrix = frame->colorspace == AVCOL_SPC_BT709 ? SWS_CS_ITU709
            : frame->colorspace == AVCOL_SPC_BT2020_NCL ? SWS_CS_BT2020 : SWS_CS_DEFAULT;
        check(sws_setColorspaceDetails(converter, sws_getCoefficients(matrix), frame->color_range == AVCOL_RANGE_JPEG,
            sws_getCoefficients(matrix), 1, 0, 1 << 16, 1 << 16), "Color conversion");
        uint8_t* destination[] = { pixels.data(), nullptr, nullptr, nullptr };
        const int stride[] = { width * 4, 0, 0, 0 };
        const int rows = sws_scale(converter, frame->data, frame->linesize, 0, height, destination, stride);
        if (rows != height) throw std::runtime_error("Incomplete pixel conversion");
        sdl(SDL_UpdateTexture(texture, nullptr, pixels.data(), width * 4), "Upload texture");
    }
    void draw() {
        if (!texture || quit) return;
        int w = 0, h = 0;
        sdl(SDL_GetRenderOutputSize(renderer, &w, &h), "Renderer dimensions");
        if (w <= 0 || h <= 0) return;
        const auto rect = viewer::contentRect(width, height, w, h, native, panX, panY);
        const SDL_FRect destination{rect.x, rect.y, rect.w, rect.h};
        sdl(SDL_SetTextureScaleMode(texture, native ? SDL_SCALEMODE_NEAREST : SDL_SCALEMODE_LINEAR), "Texture scale");
        sdl(SDL_SetRenderDrawColor(renderer, 0, 0, 0, 255), "Background");
        sdl(SDL_RenderClear(renderer), "Clear renderer");
        sdl(SDL_RenderTexture(renderer, texture, nullptr, &destination), "Render texture");
        sdl(SDL_RenderPresent(renderer), "Present");
    }
};
int run(const Options& opt) {
    Decoder decoder;
    decoder.open(opt);
    Display display;
    if (!opt.checkOnly) display.open(opt.smoke);
    Frame frame(av_frame_alloc()), transferred(av_frame_alloc());
    Packet packet(av_packet_alloc());
    if (!frame || !transferred || !packet) throw std::bad_alloc();
    int frames = 0, hardwareFrames = 0, width = 0, height = 0;
    int range = 0, matrix = 0, primaries = 0, transfer = 0;
    double decodeMs = 0;
    auto nextFrame = std::chrono::steady_clock::now();
    auto drain = [&]() {
        for (;;) {
            const auto start = std::chrono::steady_clock::now();
            const int result = avcodec_receive_frame(decoder.codec, frame.get());
            decodeMs += std::chrono::duration<double, std::milli>(std::chrono::steady_clock::now() - start).count();
            if (result == AVERROR(EAGAIN) || result == AVERROR_EOF) break;
            check(result, "Decode frame (try --software if hardware fails)");
            if (++frames > 10000) throw std::runtime_error("Fixture frame limit exceeded");
            width = frame->width; height = frame->height;
            if (width <= 0 || height <= 0 || width > 8192 || height > 8192 || int64_t(width) * height > 16 * 1024 * 1024)
                throw std::runtime_error("Unsupported frame dimensions");
            AVFrame* rendered = frame.get();
            if (frame->format == decoder.hardwareFormat && decoder.attemptedHardware) {
                ++hardwareFrames;
                // Explicit diagnostic GPU→CPU copy before SDL upload. Not zero-copy or production latency.
                if (!opt.checkOnly) {
                    av_frame_unref(transferred.get());
                    check(av_hwframe_transfer_data(transferred.get(), frame.get(), 0), "Transfer hardware frame");
                    check(av_frame_copy_props(transferred.get(), frame.get()), "Copy frame color properties");
                    rendered = transferred.get();
                }
            }
            range = frame->color_range; matrix = frame->colorspace;
            primaries = frame->color_primaries; transfer = frame->color_trc;
            if (!opt.checkOnly) {
                display.events();
                if (display.quit) { av_frame_unref(frame.get()); break; }
                display.upload(rendered);
                if (!opt.smoke) {
                    // Fixed 30 Hz fixture playback, not a live stream scheduler or latency measurement.
                    while (!display.quit && std::chrono::steady_clock::now() < nextFrame) {
                        display.events();
                        SDL_Delay(1);
                    }
                    nextFrame = std::chrono::steady_clock::now() + std::chrono::microseconds(33333);
                }
                display.draw();
            }
            av_frame_unref(frame.get());
        }
    };
    int read = 0;
    while (!display.quit && (read = av_read_frame(decoder.format, packet.get())) >= 0) {
        if (packet->stream_index == decoder.stream) {
            if (packet->size > 1024 * 1024) throw std::runtime_error("Encoded packet exceeds 1 MiB");
            const auto start = std::chrono::steady_clock::now();
            check(avcodec_send_packet(decoder.codec, packet.get()), "Submit packet");
            decodeMs += std::chrono::duration<double, std::milli>(std::chrono::steady_clock::now() - start).count();
            drain();
        }
        av_packet_unref(packet.get());
    }
    if (!display.quit) {
        if (read != AVERROR_EOF) check(read, "Read fixture");
        check(avcodec_send_packet(decoder.codec, nullptr), "Drain decoder");
        drain();
    }
    if (frames == 0) throw std::runtime_error("No decoded frames");
    if (decoder.attemptedHardware && hardwareFrames != frames) throw std::runtime_error("Hardware did not decode every frame");
    std::cout << "{\"frames\":" << frames << ",\"hardwareFrames\":" << hardwareFrames
        << ",\"width\":" << width << ",\"height\":" << height
        << ",\"decodeCallMilliseconds\":" << decodeMs
        << ",\"colorRange\":" << range << ",\"colorMatrix\":" << matrix
        << ",\"colorPrimaries\":" << primaries << ",\"colorTransfer\":" << transfer
        << ",\"ffmpegVersion\":\"" << av_version_info() << "\",\"renderer\":\"" << display.rendererName
        << "\",\"completed\":" << (display.quit ? "false" : "true") << "}\n" << std::flush;
    if (!opt.checkOnly && !opt.smoke) {
        while (!display.quit) { display.events(); display.draw(); SDL_Delay(16); }
    }
    return 0;
}
}
int main(int argc, char** argv) {
    try { return run(options(argc, argv)); }
    catch (const std::exception& error) { std::cerr << "Viewer error: " << error.what() << '\n'; return 1; }
}
