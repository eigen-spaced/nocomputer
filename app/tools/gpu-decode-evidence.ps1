# Hardware-decode evidence for the WebCodecs viewer on Windows.
#
# Runs visible playback in each mode, then sums the per-process counter
# \GPU Engine(*engtype_VideoDecode)\Running Time (100 ns units, cumulative) over the viewer
# and all its descendant msedgewebview2.exe processes. The WebView2 instance is private to
# this app (its own user-data folder) and loads one page, so nothing else decodes video in
# those processes during the run.
#
# Usage, from app/src-tauri after `cargo build`. Execution policy may block running this file
# directly; paste its contents into PowerShell instead of changing the policy.
#   $Runs = 3; <paste>

param([int]$Runs = 3, [int]$SettleSeconds = 4)

$exe = ".\target\debug\nocomputer-viewer.exe"
$fixture = "..\..\build\synthetic-srgb.h264"

function Get-Descendants([int]$root) {
    $all = Get-CimInstance Win32_Process
    $ids = @($root)
    do {
        $added = $false
        foreach ($p in $all) {
            if ($ids -contains $p.ParentProcessId -and -not ($ids -contains $p.ProcessId)) { $ids += $p.ProcessId; $added = $true }
        }
    } while ($added)
    $all | Where-Object { $ids -contains $_.ProcessId }
}

foreach ($mode in "--require-hardware", "--software") {
    for ($i = 1; $i -le $Runs; $i++) {
        $viewer = Start-Process -FilePath $exe -ArgumentList $mode, $fixture -PassThru
        Start-Sleep -Seconds $SettleSeconds  # 1 s clip; playback has ended and holds its last frame
        $procs = Get-Descendants $viewer.Id
        $samples = (Get-Counter "\GPU Engine(*engtype_VideoDecode)\Running Time").CounterSamples
        $total = 0; $where = @()
        foreach ($p in $procs) {
            $sum = ($samples | Where-Object { $_.InstanceName -like "pid_$($p.ProcessId)_*" } | Measure-Object CookedValue -Sum).Sum
            if ($sum -gt 0) {
                $type = if ($p.CommandLine -match "--type=(\S+)") { $Matches[1] } else { $p.Name }
                $where += "$type"
            }
            $total += $sum
        }
        "{0} run {1}: VideoDecode {2:N1} ms in [{3}] ({4} processes)" -f $mode, $i, ($total / 10000), ($where -join ","), $procs.Count
        Stop-Process -Id $viewer.Id -Force
        Start-Sleep -Seconds 1
    }
}
