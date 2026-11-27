# Starts the NVIDIA Series S profile (1080p output, 540p render by default).
# Usage: scripts\nvidiaS.ps1 -Exe <path to your game executable> [play-nvidia.ps1 options]
& (Join-Path $PSScriptRoot 'play-nvidia.ps1') @args
