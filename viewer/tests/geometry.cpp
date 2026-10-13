#include "../src/geometry.hpp"
#include <iostream>
#include <limits>
#include <stdexcept>

void require(bool value) { if (!value) throw std::runtime_error("geometry test failed"); }
bool near(float a, float b) { return std::abs(a - b) < 0.001f; }
int main() {
    const auto fit = viewer::contentRect(2560, 1600, 1366, 768, false);
    require(near(fit.w, 1228.8f) && near(fit.h, 768) && fit.x == 69 && fit.y == 0);
    const auto native = viewer::contentRect(2560, 1600, 1366, 768, true, 10000, -10000);
    require(native.w == 2560 && native.h == 1600 && native.x == 0 && native.y == -832);
    const auto small = viewer::contentRect(320, 240, 1280, 960, true);
    require(small.w == 320 && small.h == 240 && small.x == 480 && small.y == 360);
    require(viewer::contentRect(0, 1, 1, 1, false).w == 0);
    require(viewer::contentRect(1, 1, 1, 1, true, std::numeric_limits<float>::infinity()).w == 0);
    std::cout << "geometry: fit, native, pan clamping, small images, invalid input passed\n";
}
