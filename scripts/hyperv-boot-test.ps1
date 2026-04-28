# scripts/hyperv-boot-test.ps1
#
# Hyper-V Gen1 boot + VMBus test for the Hyper-V example kernel.
#
# Creates a Gen1 VM, attaches the ISO as a DVD, reads serial output
# from COM1 named pipe, and checks for VMBus initialization.
#
# Prerequisites:
#   - Hyper-V enabled (Windows feature)
#   - Build the ISO first (from WSL or Linux):
#       cmake -B build
#       cmake --build build --target hyperv-image
#     This produces build/hyperv.iso
#
# Usage (from PowerShell or WSL):
#   .\scripts\hyperv-boot-test.ps1
#   .\scripts\hyperv-boot-test.ps1 -Elevate
#   .\scripts\hyperv-boot-test.ps1 -Iso build\hyperv.iso
#   .\scripts\hyperv-boot-test.ps1 -Iso build\hyperv.iso -TimeoutSeconds 120
#
# From WSL:
#   powershell.exe -ExecutionPolicy Bypass -File scripts/hyperv-boot-test.ps1 -Iso build/hyperv.iso

param(
    [string]$Iso = "build\hyperv.iso",
    [string]$VMName = "embclox-hyperv-test",
    [int]$TimeoutSeconds = 120,
    [string]$SwitchName = "Default Switch",
    [string]$StaticVMIp = "172.19.192.50",
    [switch]$Elevate
)

# Self-elevate when -Elevate is passed
if ($Elevate) {
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
        $logFile = Join-Path $env:TEMP "embclox-hyperv-test.log"
        $argList = "-ExecutionPolicy Bypass -Command `"& '$PSCommandPath' -Iso '$absIso' -VMName '$VMName' -TimeoutSeconds $TimeoutSeconds *>&1 | Tee-Object -FilePath '$logFile'`""
        Start-Process -FilePath "pwsh.exe" -ArgumentList $argList -Verb RunAs -Wait
        Write-Host "=== Elevated output saved to: $logFile ==="
        if (Test-Path $logFile) { Get-Content $logFile }
        exit $LASTEXITCODE
    }
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
    throw "ISO not found: $isoPath. Build with: cmake --build build --target hyperv-image"
}

# Copy ISO to local Windows path (Hyper-V can't use \\wsl.localhost paths)
$localIso = Join-Path $env:TEMP "$VMName.iso"
Write-Host "Copying ISO to local path: $localIso"
Copy-Item -Path $isoPath -Destination $localIso -Force

# Cleanup any existing VM
if (Get-VM -Name $VMName -ErrorAction SilentlyContinue) {
    Write-Host "Cleaning up existing VM '$VMName'..."
    Stop-VM -Name $VMName -TurnOff -Force -ErrorAction SilentlyContinue
    Remove-VM -Name $VMName -Force
}

# Create Gen1 VM (BIOS boot — COM1 serial works, VMBus available)
Write-Host "Creating Gen1 VM '$VMName'..."
$pipeName = "$VMName-com1"
$pipePath = "\\.\pipe\$pipeName"

New-VM -Name $VMName -Generation 1 -MemoryStartupBytes 256MB -NoVHD | Out-Null
Set-VMDvdDrive -VMName $VMName -Path $localIso
Set-VMComPort -VMName $VMName -Number 1 -Path $pipePath
Write-Host "COM1 configured: $pipePath"

# Attach the (single, default) NIC to a vSwitch so the synthetic NetVSC
# device has somewhere to send/receive frames.
if ($SwitchName) {
    $vSwitch = Get-VMSwitch -Name $SwitchName -ErrorAction SilentlyContinue
    if ($vSwitch) {
        Get-VMNetworkAdapter -VMName $VMName | Connect-VMNetworkAdapter -SwitchName $SwitchName
        Write-Host "Network adapter connected to vSwitch: $SwitchName"
    } else {
        Write-Host "vSwitch '$SwitchName' not found - leaving NIC unconnected" -ForegroundColor Yellow
    }
}

# Default Switch on Windows accumulates *Permanent* ARP entries from past
# DHCP leases. Once an IP is bound to a (now-stale) MAC, gratuitous ARP
# from the new VM cannot override it and TCP connects time out. Try to
# remove any stale entry for our static VM IP. Best-effort: if the
# current shell isn't elevated this silently no-ops.
if ($StaticVMIp) {
    try {
        Get-NetNeighbor -InterfaceAlias "vEthernet ($SwitchName)" -IPAddress $StaticVMIp -ErrorAction SilentlyContinue |
            Remove-NetNeighbor -Confirm:$false -ErrorAction SilentlyContinue
        Write-Host "Cleared any stale ARP entry for $StaticVMIp"
    } catch {
        Write-Host "Could not clear ARP for ${StaticVMIp}: $_" -ForegroundColor Yellow
    }
}

Write-Host "VM created (Gen1, 256MB, DVD boot, COM1 serial)"

# Start VM
Write-Host "Starting VM..."
Start-VM -Name $VMName

Write-Host ""
Write-Host "=== Serial Output (COM1) ==="

# Read serial output from named pipe via a separate process
$readerScript = @"
try {
    `$pipe = New-Object System.IO.Pipes.NamedPipeClientStream('.', '$pipeName', [System.IO.Pipes.PipeDirection]::In)
    `$pipe.Connect(10000)
    `$reader = New-Object System.IO.StreamReader(`$pipe)
    while (-not `$reader.EndOfStream) {
        `$line = `$reader.ReadLine()
        Write-Output `$line
    }
} catch {
    # Pipe closed or timeout
}
"@

$readerProc = Start-Process -FilePath "pwsh.exe" `
    -ArgumentList "-NoProfile", "-Command", $readerScript `
    -PassThru -NoNewWindow -RedirectStandardOutput (Join-Path $env:TEMP "$VMName-serial.log")

# Wait for serial output, checking for markers
$serialLog = Join-Path $env:TEMP "$VMName-serial.log"
$bootPassed = $false
$vmbusPassed = $false
$netvscPassed = $false
$phase4bReady = $false
$echoVerified = $false
$vmIp = $null
$deadline = (Get-Date).AddSeconds($TimeoutSeconds)

while ((Get-Date) -lt $deadline) {
    Start-Sleep -Milliseconds 500

    if (Test-Path $serialLog) {
        $content = Get-Content $serialLog -Raw -ErrorAction SilentlyContinue
        if ($content) {
            if ($content -match "HYPERV BOOT PASSED") { $bootPassed = $true }
            if ($content -match "VMBUS INIT PASSED") { $vmbusPassed = $true }
            if ($content -match "NETVSC INIT PASSED") { $netvscPassed = $true }
            if (-not $vmIp -and ($content -match "PHASE4B: IPv4 configured: (\d+\.\d+\.\d+\.\d+)")) {
                $vmIp = $Matches[1]
                Write-Host "Detected VM IP: $vmIp" -ForegroundColor Cyan
            }
            if ($content -match "PHASE4B ECHO READY") { $phase4bReady = $true }
            if ($content -match "Halting\.") { break }
            if ($content -match "PANIC:") { break }
        }
    }

    # Once the echo task is ready and we know the IP, try a TCP echo round-trip.
    if ($phase4bReady -and $vmIp -and -not $echoVerified) {
        Write-Host "Attempting TCP echo to ${vmIp}:1234 ..." -ForegroundColor Cyan
        try {
            $client = New-Object System.Net.Sockets.TcpClient
            $iar = $client.BeginConnect($vmIp, 1234, $null, $null)
            if ($iar.AsyncWaitHandle.WaitOne(3000) -and $client.Connected) {
                $client.EndConnect($iar)
                $stream = $client.GetStream()
                $stream.ReadTimeout = 3000
                $stream.WriteTimeout = 3000
                $payload = [System.Text.Encoding]::ASCII.GetBytes("EMBCLOX-PHASE4B-PING")
                $stream.Write($payload, 0, $payload.Length)
                $stream.Flush()
                Start-Sleep -Milliseconds 250
                $rxBuf = New-Object byte[] 64
                $n = $stream.Read($rxBuf, 0, $rxBuf.Length)
                $reply = [System.Text.Encoding]::ASCII.GetString($rxBuf, 0, $n)
                Write-Host "TCP echo reply: '$reply' ($n bytes)" -ForegroundColor Cyan
                if ($reply -eq "EMBCLOX-PHASE4B-PING") {
                    $echoVerified = $true
                    Write-Host "TCP echo: VERIFIED" -ForegroundColor Green
                }
                $stream.Close()
                $client.Close()
            } else {
                Write-Host "TCP connect timeout" -ForegroundColor Yellow
                $client.Close()
            }
        } catch {
            Write-Host "TCP echo error: $_" -ForegroundColor Yellow
        }
        # Echo loop runs forever; once we've verified, stop the VM.
        if ($echoVerified) { break }
    }

    # Check if VM is still running
    $vm = Get-VM -Name $VMName -ErrorAction SilentlyContinue
    if (-not $vm -or $vm.State -ne 'Running') { break }
}

# Kill reader process
if ($readerProc -and -not $readerProc.HasExited) { $readerProc.Kill() }

# Display serial output
if (Test-Path $serialLog) {
    $lines = Get-Content $serialLog
    foreach ($line in $lines) { Write-Host $line }
    Write-Host ""
    Write-Host "=== End Serial Output ==="
    Write-Host ""
    $lineCount = $lines.Count
    Write-Host "Serial log saved to: $serialLog - $lineCount lines"
} else {
    Write-Host "(no serial output captured)"
    Write-Host "=== End Serial Output ==="
}

# Stop and cleanup
$vm = Get-VM -Name $VMName -ErrorAction SilentlyContinue
if ($vm) {
    Write-Host "VM state: $($vm.State)"
    Write-Host "Stopping VM..."
    Stop-VM -Name $VMName -TurnOff -Force -ErrorAction SilentlyContinue
}

# Results
Write-Host ""
Write-Host "=== Results ===" -ForegroundColor Cyan
$exitCode = 0

if ($bootPassed -or $vmbusPassed) {
    Write-Host "Boot: PASSED" -ForegroundColor Green
} else {
    Write-Host "Boot: FAILED" -ForegroundColor Red
    $exitCode = 1
}

if ($vmbusPassed) {
    Write-Host "VMBus: PASSED" -ForegroundColor Green
} else {
    Write-Host "VMBus: NOT TESTED (expected on QEMU)" -ForegroundColor Yellow
}

if ($netvscPassed) {
    Write-Host "NetVSC: PASSED" -ForegroundColor Green
} else {
    Write-Host "NetVSC: NOT TESTED" -ForegroundColor Yellow
}

if ($phase4bReady) {
    Write-Host "Phase 4b (embassy stack ready): READY" -ForegroundColor Green
} else {
    Write-Host "Phase 4b (embassy stack ready): NOT REACHED" -ForegroundColor Yellow
    $exitCode = 1
}

if ($echoVerified) {
    Write-Host "TCP echo @ port 1234: VERIFIED" -ForegroundColor Green
} else {
    Write-Host "TCP echo @ port 1234: NOT VERIFIED" -ForegroundColor Yellow
}

# Cleanup
Write-Host ""
Write-Host "Cleaning up..."
Remove-VM -Name $VMName -Force -ErrorAction SilentlyContinue
Remove-Item $localIso -Force -ErrorAction SilentlyContinue

exit $exitCode
