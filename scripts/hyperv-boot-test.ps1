# scripts/hyperv-boot-test.ps1
#
# Hyper-V Gen1 boot + VMBus + NetVSC TCP echo test for the Hyper-V
# example kernel.
#
# Creates a Gen1 VM, attaches the ISO as a DVD, reads serial output
# from COM1 named pipe, attaches the VM to the dedicated `embclox-test`
# Internal vSwitch, and probes TCP echo on port 1234.
#
# Prerequisites:
#   - Hyper-V enabled (Windows feature)
#   - Run scripts/hyperv-setup-vswitch.ps1 ONCE as Administrator to
#     create the dedicated `embclox-test` Internal vSwitch
#     (host = 192.168.234.1/24, VMs use 192.168.234.50/24).
#     Without this the script will fall back to leaving the NIC
#     unconnected, and TCP echo will be skipped.
#   - Build the ISO first (from WSL or Linux):
#       cmake -B build
#       cmake --build build --target hyperv-image
#     This produces build/hyperv.iso
#
# Usage (from PowerShell or WSL):
#   .\scripts\hyperv-boot-test.ps1
#   .\scripts\hyperv-boot-test.ps1 -Elevate
#   .\scripts\hyperv-boot-test.ps1 -Iso build\hyperv.iso
#   .\scripts\hyperv-boot-test.ps1 -SwitchName 'Default Switch'  # not recommended
#
# From WSL:
#   powershell.exe -ExecutionPolicy Bypass -File scripts/hyperv-boot-test.ps1 -Iso build/hyperv.iso
#
# Why a dedicated Internal vSwitch instead of Default Switch:
# Hyper-V's Default Switch (NAT/ICS) accumulates Permanent ARP entries
# for every IP it has ever DHCP-leased, often for the entire /20 it
# manages. Once it has bound 192.168.234.50 (or whatever IP) to a
# stale VM MAC, no new VM can use that IP because ARP lookups from the
# host return the wrong MAC and TCP times out. The dedicated Internal
# vSwitch has no DHCP server, no Permanent ARP table, and is reset
# every time you re-run hyperv-setup-vswitch.ps1.

param(
    [string]$Iso = "build\hyperv.iso",
    [string]$VMName = "embclox-hyperv-test",
    [int]$TimeoutSeconds = 90,
    [string]$SwitchName = "embclox-test",
    [string]$ExpectedHostIp = "192.168.234.1",
    [string]$StaticMac = "00155DEB0100",
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

# --- Pre-flight checks ---
# These run BEFORE any VM is created so misconfiguration fails fast with
# a clear remediation, instead of a 90-second boot ending in a silent
# 'TCP echo NOT VERIFIED' that's actually a host networking problem.

if (-not $SwitchName) {
    throw "Pre-flight: -SwitchName must be specified (default: embclox-test). Run scripts/hyperv-setup-vswitch.ps1 to create it."
}

$vSwitch = Get-VMSwitch -Name $SwitchName -ErrorAction SilentlyContinue
if (-not $vSwitch) {
    throw @"
Pre-flight: vSwitch '$SwitchName' does not exist.

The VM's NetVSC NIC needs a vSwitch to attach to or the host cannot
reach it. To create the dedicated 'embclox-test' Internal vSwitch, run
the following ONCE in an elevated PowerShell prompt:

    powershell.exe -ExecutionPolicy Bypass -File scripts/hyperv-setup-vswitch.ps1

(The setup script is idempotent and also assigns 192.168.234.1/24 to
the host vEthernet adapter so the host can reach the VM at .50.)
"@
}

if ($vSwitch.SwitchType -ne 'Internal') {
    Write-Host "WARNING: vSwitch '$SwitchName' is type '$($vSwitch.SwitchType)' (expected Internal). Static-IP test path may not behave as documented." -ForegroundColor Yellow
}

if ($ExpectedHostIp) {
    $ifAlias = "vEthernet ($SwitchName)"
    $hostIp = Get-NetIPAddress -InterfaceAlias $ifAlias -AddressFamily IPv4 -ErrorAction SilentlyContinue |
              Where-Object { $_.IPAddress -eq $ExpectedHostIp }
    if (-not $hostIp) {
        throw @"
Pre-flight: host adapter '$ifAlias' is missing IP '$ExpectedHostIp'.

The VM boots with a static IP in the same subnet (192.168.234.50/24)
and the host needs the matching gateway address ($ExpectedHostIp/24)
to be reachable. Without it the host cannot route to the VM and the
TCP echo probe will silently time out.

To fix, run the following ONCE in an elevated PowerShell prompt:

    powershell.exe -ExecutionPolicy Bypass -File scripts/hyperv-setup-vswitch.ps1

To skip this check (e.g. for a DHCP-only test scenario), pass
-ExpectedHostIp '' to the script.
"@
    }
}

Write-Host "Pre-flight checks passed: vSwitch '$SwitchName' present, host IP '$ExpectedHostIp' configured." -ForegroundColor Green

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

# Attach the (single, default) NIC to the vSwitch we pre-validated above.
Get-VMNetworkAdapter -VMName $VMName | Connect-VMNetworkAdapter -SwitchName $SwitchName
Write-Host "Network adapter connected to vSwitch: $SwitchName"

# Pin a static MAC so the host's ARP cache stays valid across runs.
# Hyper-V dynamic MACs change every New-VM (00:15:5D:...:dc, dd, de, ...);
# the host's ARP entry for 192.168.234.50 then points at the previous run's
# MAC and Windows ignores the new VM's gratuitous ARP, silently breaking
# the TCP probe. The chosen MAC is in Hyper-V's prefix (00:15:5D) with a
# distinctive suffix (EB:01:00 -> "embclox 1 0") that won't collide with
# Hyper-V dynamic allocation (Hyper-V increments from a host-derived base
# in the lower bytes; EB:01:00 is well outside any allocator range).
Set-VMNetworkAdapter -VMName $VMName -StaticMacAddress $StaticMac
Write-Host "Static MAC pinned: $StaticMac"

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
} elseif ($phase4bReady) {
    # Reaching ECHO READY means the kernel is fine - if echo still failed,
    # the host networking is misconfigured and CI must surface it.
    Write-Host "TCP echo @ port 1234: FAILED (kernel reached ECHO READY but host TCP probe timed out)" -ForegroundColor Red
    Write-Host "  Likely cause: vSwitch port not bridging traffic, host firewall, or stale ARP." -ForegroundColor Red
    Write-Host "  Check: Get-NetIPAddress -InterfaceAlias 'vEthernet ($SwitchName)'; arp -a $vmIp" -ForegroundColor DarkGray
    $exitCode = 1
} else {
    Write-Host "TCP echo @ port 1234: NOT TESTED (kernel never reached ECHO READY)" -ForegroundColor Yellow
}

# Cleanup
Write-Host ""
Write-Host "Cleaning up..."
Remove-VM -Name $VMName -Force -ErrorAction SilentlyContinue
Remove-Item $localIso -Force -ErrorAction SilentlyContinue

exit $exitCode
