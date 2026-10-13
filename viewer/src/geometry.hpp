#pragma once
#include <algorithm>
#include <cmath>

namespace viewer {
struct Rect { float x = 0, y = 0, w = 0, h = 0; };
// Dimensions are renderer output pixels, not OS logical window points.
inline Rect contentRect(int videoWidth, int videoHeight, int outputWidth, int outputHeight,
                        bool native, float panX = 0, float panY = 0) {
    if (videoWidth <= 0 || videoHeight <= 0 || outputWidth <= 0 || outputHeight <= 0
        || !std::isfinite(panX) || !std::isfinite(panY)) return {};
    const float scale = native ? 1.0f : std::min(float(outputWidth) / videoWidth, float(outputHeight) / videoHeight);
    const float w = videoWidth * scale, h = videoHeight * scale;
    const float dx = native ? std::clamp(panX, -std::max(0.0f, w - outputWidth) / 2,
                                        std::max(0.0f, w - outputWidth) / 2) : 0;
    const float dy = native ? std::clamp(panY, -std::max(0.0f, h - outputHeight) / 2,
                                        std::max(0.0f, h - outputHeight) / 2) : 0;
    return {std::round((outputWidth - w) / 2 + dx), std::round((outputHeight - h) / 2 + dy), w, h};
}
}
