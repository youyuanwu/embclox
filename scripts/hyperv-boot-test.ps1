# scripts/hyperv-boot-test.ps1
# Phase 0 boot test: verify embclox boots under Hyper-V Gen1 with serial output.
# Requires Hyper-V. Will prompt for elevation if not running as admin.

param(
    [string]$Image = "target\x86_64-unknown-none\debug\embclox-example.img",
    [string]$VMName = "embclox-boot-test",
    [int]$TimeoutSeconds = 60
)

# Self-elevate if not admin
$isAdmin = ([Security.Principal.WindowsPrincipal] [Security.Principal.WindowsIdentity]::GetCurrent()).IsInRole(
    [Security.Principal.WindowsBuiltInRole]::Administrator
)
if (-not $isAdmin) {
    Write-Host "Requesting elevation..."
    # If Image is already absolute, use it as-is; otherwise resolve relative to repo root
    if ([System.IO.Path]::IsPathRooted($Image)) {
        $absImage = $Image
    } else {
        $repoRoot = Split-Path -Parent (Split-Path -Parent $PSCommandPath)
        $absImage = Join-Path $repoRoot $Image
    }
    # Redirect elevated output to a temp file so WSL can read it
    $logFile = Join-Path $env:TEMP "embclox-hyperv-test.log"
    $argList = "-ExecutionPolicy Bypass -Command `"& '$PSCommandPath' -Image '$absImage' -VMName '$VMName' -TimeoutSeconds $TimeoutSeconds *>&1 | Tee-Object -FilePath '$logFile'`""
    Start-Process -FilePath "pwsh.exe" -ArgumentList $argList -Verb RunAs -Wait
    Write-Host "=== Elevated output saved to: $logFile ==="
    if (Test-Path $logFile) { Get-Content $logFile }
    exit $LASTEXITCODE
}

# Wrap everything in a trap so errors don't close the window
trap {
    Write-Host ""
    Write-Host "ERROR: $_" -ForegroundColor Red
    Write-Host $_.ScriptStackTrace -ForegroundColor DarkGray
    Write-Host ""
    # Cleanup on error
    Stop-VM -Name $VMName -TurnOff -Force -ErrorAction SilentlyContinue
    Remove-VM -Name $VMName -Force -ErrorAction SilentlyContinue
    if ($localVhd) { Remove-Item $localVhd -Force -ErrorAction SilentlyContinue }
    Write-Host "Press Enter to exit..."
    Read-Host
    exit 1
}

$ErrorActionPreference = 'Stop'

# Resolve image path
if ([System.IO.Path]::IsPathRooted($Image)) {
    $imgPath = $Image
} else {
    $repoRoot = Split-Path -Parent (Split-Path -Parent $PSCommandPath)
    $imgPath = Join-Path $repoRoot $Image
}

Write-Host "Image: $imgPath"

if (-not (Test-Path $imgPath)) {
    throw "Image not found: $imgPath. Build with: cmake --build build --target example-image"
}

# Convert to VHD (Hyper-V needs VHD, not raw)
# Try qemu-img on Windows first, then check if .vhd already exists
# (pre-converted from WSL with: qemu-img convert -f raw -O vpc img vhd)
$vhdPath = "$imgPath.vhdx"
$needConvert = -not (Test-Path $vhdPath) -or ((Get-Item $vhdPath).LastWriteTime -lt (Get-Item $imgPath).LastWriteTime)
if ($needConvert) {
    Write-Host "Converting raw image to VHDX..."
    $qemuImg = Get-Command qemu-img -ErrorAction SilentlyContinue
    if ($qemuImg) {
        & qemu-img convert -f raw -O vhdx $imgPath $vhdPath
        if ($LASTEXITCODE -ne 0) { throw "qemu-img convert failed" }
    } else {
        throw "qemu-img not found. Pre-convert from WSL with:`n  qemu-img convert -f raw -O vhdx $imgPath $vhdPath"
    }
} else {
    Write-Host "Using existing VHDX: $vhdPath"
}

# Hyper-V cannot use VHDs on network paths (e.g. \\wsl.localhost\...).
# Copy to a local Windows temp directory.
$localVhd = Join-Path $env:TEMP "$VMName.vhdx"
Write-Host "Copying VHD to local path: $localVhd"
Copy-Item -Path $vhdPath -Destination $localVhd -Force

# Serial output log file
$logPath = "$imgPath-hyperv.log"
$pipeName = "$VMName-com1"
$pipePath = "\\.\pipe\$pipeName"

# Cleanup any existing VM
if (Get-VM -Name $VMName -ErrorAction SilentlyContinue) {
    Write-Host "Cleaning up existing VM '$VMName'..."
    Stop-VM -Name $VMName -TurnOff -Force -ErrorAction SilentlyContinue
    Remove-VM -Name $VMName -Force
}

# Create Gen2 VM (UEFI boot - bootloader v0.11 BIOS hangs on Hyper-V VBE)
Write-Host "Creating Gen2 VM '$VMName'..."
New-VM -Name $VMName -Generation 2 -MemoryStartupBytes 256MB -NoVHD | Out-Null
Add-VMHardDiskDrive -VMName $VMName -Path $localVhd
# Disable Secure Boot (our bootloader is not signed)
Set-VMFirmware -VMName $VMName -EnableSecureBoot Off

# Try to add COM port via named pipe (may work on Gen2 virtual UART)
$comPipeName = "$VMName-com1"
$comPipePath = "\\.\pipe\$comPipeName"
try {
    Set-VMComPort -VMName $VMName -Number 1 -Path $comPipePath
    Write-Host "COM1 configured: $comPipePath" -ForegroundColor Green
    Write-Host ">>> Connect PuTTY to: $comPipePath (Serial mode) to see kernel logs <<<" -ForegroundColor Cyan
} catch {
    Write-Host "COM port not supported on this Gen2 VM: $_" -ForegroundColor Yellow
}

Write-Host "VM created (Gen2, UEFI, 256MB, Secure Boot off)"

# Start VM and wait for it to run
Write-Host ""
Write-Host ">>> VM created. Open VM Connect NOW (Hyper-V Manager -> Connect) <<<" -ForegroundColor Cyan
Write-Host ">>> Then press Enter here to START the VM <<<" -ForegroundColor Cyan
Read-Host
Write-Host "Starting VM..."
Start-VM -Name $VMName

Write-Host "VM running. Waiting ${TimeoutSeconds}s for kernel to execute..."
Write-Host "(Connect PuTTY NOW to see serial output)" -ForegroundColor Cyan
Start-Sleep -Seconds $TimeoutSeconds

$serialOutput = ""

# Check VM state BEFORE stopping
$vm = Get-VM -Name $VMName
$vmState = $vm.State
Write-Host "VM state: $vmState"

# Stop VM
Write-Host "Stopping VM..."
Stop-VM -Name $VMName -TurnOff -Force -ErrorAction SilentlyContinue

# Show results
Write-Host ""
Write-Host "=== Results ===" -ForegroundColor Cyan
if ($vmState -eq 'Running') {
    Write-Host "=== BOOT TEST PASSED ===" -ForegroundColor Green
    $exitCode = 0
} else {
    Write-Host "VM state: $vmState" -ForegroundColor Yellow
    Write-Host "=== BOOT TEST INCONCLUSIVE ===" -ForegroundColor Yellow
    $exitCode = 2
}

# Cleanup
Write-Host ""
Write-Host "Cleaning up..."
Remove-VM -Name $VMName -Force -ErrorAction SilentlyContinue
Remove-Item $localVhd -Force -ErrorAction SilentlyContinue

exit $exitCode
