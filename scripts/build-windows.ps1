param(
    [Parameter(Mandatory = $true)][string]$VcpkgRoot,
    [string]$FixturePath = "",
    [string]$Generator = ""
)
$ErrorActionPreference = "Stop"
$projectRoot = Split-Path -Parent $PSScriptRoot
$toolchain = Join-Path $VcpkgRoot "scripts/buildsystems/vcpkg.cmake"
if (!(Test-Path -LiteralPath $toolchain -PathType Leaf)) { throw "Supply an existing bootstrapped vcpkg checkout via -VcpkgRoot." }
if (!(Get-Command cmake -ErrorAction SilentlyContinue)) { throw "CMake 3.24+ and Visual Studio C++ build tools are required." }
$buildDir = Join-Path $projectRoot "build/windows-viewer"
$configure = @("-S", (Join-Path $projectRoot "viewer"), "-B", $buildDir,
    "-A", "x64", "-DCMAKE_TOOLCHAIN_FILE=$toolchain", "-DVCPKG_TARGET_TRIPLET=x64-windows")
if ($Generator) { $configure += @("-G", $Generator) }
if ($FixturePath) {
    $fixture = (Resolve-Path -LiteralPath $FixturePath).Path
    $configure += "-DNOCOMPUTER_FIXTURE_PATH=$fixture"
}
& cmake @configure
if ($LASTEXITCODE -ne 0) { throw "CMake configuration failed ($LASTEXITCODE)." }
& cmake --build $buildDir --config Release --parallel
if ($LASTEXITCODE -ne 0) { throw "Viewer build failed ($LASTEXITCODE)." }
& ctest --test-dir $buildDir -C Release --output-on-failure
if ($LASTEXITCODE -ne 0) { throw "Viewer tests failed ($LASTEXITCODE)." }
Write-Host "Built $buildDir/Release/nocomputer-viewer.exe"
Write-Host "Windows D3D11/real display checks are separate from these software tests."
