# Starts the NVIDIA Series X profile (2160p output, 1080p render by default).
# Usage: scripts\nvidiaX.ps1 -Exe <path to your game executable> [play-nvidia.ps1 options]
& (Join-Path $PSScriptRoot 'play-nvidia.ps1') -SeriesX @args
