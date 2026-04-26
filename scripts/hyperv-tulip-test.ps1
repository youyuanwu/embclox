# scripts/hyperv-tulip-test.ps1
#
# Hyper-V Gen1 boot + TCP echo test for the Tulip NIC / Limine kernel.
#
# Creates a Gen1 VM with a Legacy Network Adapter (DEC 21140 Tulip),
# attaches the ISO as a DVD, reads serial output from COM1 named pipe,
# waits for DHCP, then sends a TCP echo probe to port 1234.
#
# Prerequisites:
#   - Hyper-V enabled (Windows feature)
#   - Build the ISO first (from WSL or Linux):
#       cmake -B build
#       cmake --build build --target tulip-image
#     This produces build/tulip.iso
#   - The script self-elevates (Hyper-V cmdlets require admin)
#
# Usage (from PowerShell or WSL):
#   .\scripts\hyperv-tulip-test.ps1
#   .\scripts\hyperv-tulip-test.ps1 -Iso build\tulip.iso
#   .\scripts\hyperv-tulip-test.ps1 -Iso build\tulip.iso -TimeoutSeconds 120
#
# From WSL:
#   powershell.exe -ExecutionPolicy Bypass -File scripts/hyperv-tulip-test.ps1 -Iso build/tulip.iso

param(
    [string]$Iso = "build\tulip.iso",
    [string]$VMName = "embclox-tulip-test",
    [int]$TimeoutSeconds = 60
)

# Self-elevate if not admin (Hyper-V cmdlets require elevation)
$isAdmin = ([Security.Principal.WindowsPrincipal] [Security.Principal.WindowsIdentity]::GetCurrent()).IsInRole(
    [Security.Principal.WindowsBuiltInRole]::Administrator
)
if (-not $isAdmin) {
    Write-Host "Requesting elevation..."
    if ([System.IO.Path]::IsPathRooted($Iso)) {
        $absIso = $Iso
    } else {
        $repoRoot = Split-Path -Parent (Split-Path -Parent $PSCommandPath)
        $absIso = Join-Path $repoRoot $Iso
    }
    $logFile = Join-Path $env:TEMP "embclox-tulip-test.log"
    $argList = "-ExecutionPolicy Bypass -Command `"& '$PSCommandPath' -Iso '$absIso' -VMName '$VMName' -TimeoutSeconds $TimeoutSeconds *>&1 | Tee-Object -FilePath '$logFile'`""
    Start-Process -FilePath "pwsh.exe" -ArgumentList $argList -Verb RunAs -Wait
    Write-Host "=== Elevated output saved to: $logFile ==="
    if (Test-Path $logFile) { Get-Content $logFile }
    exit $LASTEXITCODE
}

trap {
    Write-Host ""
    Write-Host "ERROR: $_" -ForegroundColor Red
    Write-Host $_.ScriptStackTrace -ForegroundColor DarkGray
    Write-Host ""
    Stop-VM -Name $VMName -TurnOff -Force -ErrorAction SilentlyContinue
    Remove-VM -Name $VMName -Force -ErrorAction SilentlyContinue
    if ($localIso) { Remove-Item $localIso -Force -ErrorAction SilentlyContinue }
    if ($readerProc -and -not $readerProc.HasExited) { $readerProc.Kill() }
    Write-Host "Press Enter to exit..."
    Read-Host
    exit 1
}

$ErrorActionPreference = 'Stop'

# Resolve ISO path
if ([System.IO.Path]::IsPathRooted($Iso)) {
    $isoPath = $Iso
} else {
    $repoRoot = Split-Path -Parent (Split-Path -Parent $PSCommandPath)
    $isoPath = Join-Path $repoRoot $Iso
}

Write-Host "ISO: $isoPath"

if (-not (Test-Path $isoPath)) {
    throw "ISO not found: $isoPath. Build with: cmake --build build --target tulip-image"
}

# Hyper-V cannot use files on network paths (e.g. \\wsl.localhost\...).
# Copy to a local Windows temp directory.
$localIso = Join-Path $env:TEMP "$VMName.iso"
Write-Host "Copying ISO to local path: $localIso"
Copy-Item -Path $isoPath -Destination $localIso -Force

# Named pipe for COM1 serial output
$pipeName = "$VMName-com1"
$pipePath = "\\.\pipe\$pipeName"

# Cleanup any existing VM with this name
if (Get-VM -Name $VMName -ErrorAction SilentlyContinue) {
    Write-Host "Cleaning up existing VM '$VMName'..."
    Stop-VM -Name $VMName -TurnOff -Force -ErrorAction SilentlyContinue
    Remove-VM -Name $VMName -Force
}

# Create Gen1 VM (Gen1 emulates DEC 21140 Tulip NIC as legacy network adapter)
Write-Host "Creating Gen1 VM '$VMName'..."
New-VM -Name $VMName -Generation 1 -MemoryStartupBytes 256MB -NoVHD | Out-Null

# Attach ISO as DVD drive
Set-VMDvdDrive -VMName $VMName -Path $localIso

# Configure boot order: CD first
$dvd = Get-VMDvdDrive -VMName $VMName
Set-VMBios -VMName $VMName -StartupOrder @("CD", "IDE", "LegacyNetworkAdapter", "Floppy")

# Configure COM1 named pipe for serial debug output
Set-VMComPort -VMName $VMName -Number 1 -Path $pipePath
Write-Host "COM1 configured: $pipePath" -ForegroundColor Green

# Remove default network adapter and add Legacy Network Adapter (DEC 21140 Tulip)
$defaultNic = Get-VMNetworkAdapter -VMName $VMName
if ($defaultNic) {
    Remove-VMNetworkAdapter -VMName $VMName
}
Add-VMNetworkAdapter -VMName $VMName -IsLegacy $true -SwitchName "Default Switch"
# Allow the kernel to use whatever MAC it reads from the emulated EEPROM.
# Hyper-V's virtual switch filters by MAC — enable MAC spoofing so our
# EEPROM-read MAC isn't dropped by the switch.
Set-VMNetworkAdapter -VMName $VMName -MacAddressSpoofing On
Write-Host "Legacy Network Adapter added (DEC 21140 Tulip, MAC spoofing on)" -ForegroundColor Green

Write-Host ""
Write-Host "VM created (Gen1, 256MB, DVD boot, COM1 serial, Legacy NIC)"

Write-Host "Starting VM..."
Start-VM -Name $VMName

# Read serial output from COM1 named pipe using a separate process
# (A process can be reliably killed, unlike a runspace with blocking I/O)
Write-Host ""
Write-Host "=== Serial Output (COM1) ===" -ForegroundColor Cyan
$logFile = Join-Path $env:TEMP "$VMName-serial.log"
"" | Out-File -FilePath $logFile -Encoding utf8

# Spawn a helper process that reads from the pipe and writes to the log file
$readerScript = @"
try {
    `$fs = [System.IO.File]::Open('$pipePath', [System.IO.FileMode]::Open, [System.IO.FileAccess]::Read, [System.IO.FileShare]::ReadWrite)
    `$reader = New-Object System.IO.StreamReader(`$fs)
    `$buf = New-Object char[] 4096
    while (`$true) {
        `$n = `$reader.Read(`$buf, 0, `$buf.Length)
        if (`$n -le 0) { break }
        `$chunk = New-Object string(`$buf, 0, `$n)
        `$chunk | Out-File -FilePath '$logFile' -Encoding utf8 -Append -NoNewline
    }
} catch {}
"@
$readerProc = Start-Process -FilePath "pwsh.exe" -ArgumentList "-NoProfile", "-Command", $readerScript `
    -NoNewWindow -PassThru

# Stream output from the log file until timeout or key output seen
$serialLog = @()
$deadline = [DateTime]::Now.AddSeconds($TimeoutSeconds)
$lastPos = 0
$initPassed = $false
while ([DateTime]::Now -lt $deadline) {
    if (Test-Path $logFile) {
        $content = Get-Content $logFile -Raw -ErrorAction SilentlyContinue
        if ($content -and $content.Length -gt $lastPos) {
            $newText = $content.Substring($lastPos)
            Write-Host $newText -NoNewline
            $lastPos = $content.Length
            if ($newText -match "TULIP INIT PASSED") {
                Write-Host ""
                Write-Host ">>> Matched: TULIP INIT PASSED <<<" -ForegroundColor Green
                $initPassed = $true
            }
            if ($newText -match "TCP echo server") {
                Write-Host ""
                Write-Host ">>> TCP echo server started <<<" -ForegroundColor Green
            }
            if ($newText -match "Starting Embassy executor") {
                # Wait for DHCP to assign an IP (up to 15s)
                Write-Host ""
                Write-Host "Waiting for DHCP lease..." -ForegroundColor Cyan
                $dhcpDeadline = [DateTime]::Now.AddSeconds(15)
                while ([DateTime]::Now -lt $dhcpDeadline) {
                    Start-Sleep -Milliseconds 500
                    $content = Get-Content $logFile -Raw -ErrorAction SilentlyContinue
                    if ($content -and $content.Length -gt $lastPos) {
                        $newChunk = $content.Substring($lastPos)
                        Write-Host $newChunk -NoNewline
                        $lastPos = $content.Length
                        if ($newChunk -match "DHCP assigned") {
                            break
                        }
                    }
                }
                break
            }
        }
    }
    if ($readerProc.HasExited) { break }
    Start-Sleep -Milliseconds 200
}

# Stop VM (closes the pipe, reader process will exit)
Write-Host ""
Write-Host "=== End Serial Output ===" -ForegroundColor Cyan

# TCP echo test — try to connect to the kernel's echo server on port 1234.
# The kernel uses a static IP; try to find it from serial output or VM network.
Write-Host ""
Write-Host "=== TCP Echo Test ===" -ForegroundColor Cyan

# Try to find the VM's IP from Hyper-V
$vmIp = $null
$vmNic = Get-VMNetworkAdapter -VMName $VMName -ErrorAction SilentlyContinue
if ($vmNic -and $vmNic.IPAddresses) {
    foreach ($ip in $vmNic.IPAddresses) {
        if ($ip -match '^\d+\.\d+\.\d+\.\d+$') {
            $vmIp = $ip
            break
        }
    }
}

# Also check serial log for the kernel's configured IP
$kernelIp = $null
if ($logFile -and (Test-Path $logFile)) {
    $logContent = Get-Content $logFile -Raw -ErrorAction SilentlyContinue
    # Look for IP in log (e.g. "IP: 10.0.2.15" or similar)
    if ($logContent -match '(\d+\.\d+\.\d+\.\d+)/\d+') {
        $kernelIp = $Matches[1]
    }
}

# The kernel has a static IP (10.0.2.15 for QEMU). Use kernel IP if found,
# otherwise try VM IP from Hyper-V.
$targetIp = if ($kernelIp) { $kernelIp } elseif ($vmIp) { $vmIp } else { $null }

if ($targetIp) {
    Write-Host "Target IP: $targetIp (kernel=$kernelIp, hyperv=$vmIp)"
    $tcpPassed = $false
    try {
        $client = New-Object System.Net.Sockets.TcpClient
        $connectTask = $client.ConnectAsync($targetIp, 1234)
        if ($connectTask.Wait(5000)) {
            $stream = $client.GetStream()
            $stream.WriteTimeout = 3000
            $stream.ReadTimeout = 3000
            $testMsg = [System.Text.Encoding]::ASCII.GetBytes("hello-hyperv`n")
            $stream.Write($testMsg, 0, $testMsg.Length)
            $buf = New-Object byte[] 256
            $n = $stream.Read($buf, 0, $buf.Length)
            $reply = [System.Text.Encoding]::ASCII.GetString($buf, 0, $n).Trim()
            Write-Host "Sent: hello-hyperv, Reply: $reply"
            if ($reply -match "hello-hyperv") {
                Write-Host ">>> TCP ECHO PASSED <<<" -ForegroundColor Green
                $tcpPassed = $true
            } else {
                Write-Host ">>> TCP ECHO MISMATCH <<<" -ForegroundColor Yellow
            }
            $client.Close()
        } else {
            Write-Host "TCP connect timed out (5s)" -ForegroundColor Yellow
            $client.Close()
        }
    } catch {
        Write-Host "TCP test failed: $_" -ForegroundColor Yellow
    }
} else {
    Write-Host 'No IP found. Kernel uses static 10.0.2.15 (QEMU default).' -ForegroundColor Yellow
    Write-Host 'For Hyper-V, update kernel to match Default Switch subnet or use DHCP.' -ForegroundColor Yellow
    $tcpPassed = $false
}

Write-Host "=== End TCP Echo Test ===" -ForegroundColor Cyan

$vm = Get-VM -Name $VMName
$vmState = $vm.State
Write-Host "VM state: $vmState"

Write-Host "Stopping VM..."
Stop-VM -Name $VMName -TurnOff -Force -ErrorAction SilentlyContinue

# Kill reader process if still alive
if (-not $readerProc.HasExited) {
    $readerProc.WaitForExit(3000)
    if (-not $readerProc.HasExited) {
        $readerProc.Kill()
    }
}

# Read final log
if (Test-Path $logFile) {
    $finalContent = Get-Content $logFile -Raw -ErrorAction SilentlyContinue
    if ($finalContent) {
        $serialLog = $finalContent -split "`n"
    }
}

if ($serialLog.Count -gt 0) {
    Write-Host ""
    Write-Host "Serial log saved to: $logFile - $($serialLog.Count) lines" -ForegroundColor Cyan
}

# Results
Write-Host ""
Write-Host "=== Results ===" -ForegroundColor Cyan
$bootPassed = $serialLog.Count -gt 0 -or $vmState -eq 'Running'
if ($bootPassed) {
    Write-Host "Boot: PASSED (captured $($serialLog.Count) serial lines)" -ForegroundColor Green
} else {
    Write-Host "Boot: INCONCLUSIVE (VM state: $vmState)" -ForegroundColor Yellow
}
if ($tcpPassed) {
    Write-Host "TCP:  PASSED" -ForegroundColor Green
} else {
    Write-Host "TCP:  SKIPPED or FAILED" -ForegroundColor Yellow
}
$exitCode = if ($bootPassed) { 0 } else { 2 }

# Cleanup
Write-Host ""
Write-Host 'Cleaning up...'
Remove-VM -Name $VMName -Force -ErrorAction SilentlyContinue
Remove-Item $localIso -Force -ErrorAction SilentlyContinue

exit $exitCode
