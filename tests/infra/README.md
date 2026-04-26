# Azure Gen1 VM Deployment — Bare-Metal Kernel

Deploys an Azure Gen1 VM that boots the embclox Tulip kernel directly
from a VHD. The kernel boots and serial output is available via Azure
serial console / boot diagnostics.

> **Known limitation**: Azure Gen1 VMs do NOT expose a legacy DEC 21140
> Tulip NIC on the PCI bus. The only PCI devices visible are chipset
> bridges (8086:7192, 8086:7110) and a Hyper-V synthetic VGA (1414:5353).
> Networking on Azure requires the VMBus netvsc driver (future work).
> The TCP echo server currently works only on **local Hyper-V Gen1** VMs
> with a Legacy Network Adapter.

## Prerequisites

- Azure CLI installed and logged in (`az login`)
- VHD built locally: `cmake --build build --target tulip-vhd`

## Deploy

### 1. Create resource group and infra

```sh
rg=embclox-test
az group create --name $rg --location westus2

# Deploy storage account + network (no VM yet)
az deployment group create -g $rg \
  --template-file tests/infra/main.bicep
```

### 2. Upload VHD

```sh
# Get the storage account name and upload command from outputs
az deployment group show -g $rg -n main --query properties.outputs

# Upload (use the uploadCommand from output, or manually):
sa=$(az deployment group show -g $rg -n main --query properties.outputs.storageAccount.value -o tsv)
az storage blob upload --account-name $sa -c vhds \
  -f build/tulip.vhd -n tulip.vhd --type page --overwrite
```

### 3. Deploy VM

```sh
vhdUri="https://${sa}.blob.core.windows.net/vhds/tulip.vhd"
az deployment group create -g $rg \
  --template-file tests/infra/main.bicep \
  --parameters vhdBlobUri=$vhdUri
```

## View Serial Output

```sh
# Boot diagnostics log (captured serial output)
az vm boot-diagnostics get-boot-log -g $rg -n $rg

# Interactive serial console (requires VM running)
az serial-console connect -g $rg -n $rg
```

## Test TCP Echo

```sh
# Get public IP
ip=$(az vm list-ip-addresses -g $rg -n $rg \
  --query '[0].virtualMachine.network.publicIpAddresses[0].ipAddress' -o tsv)

# Send echo test
echo "hello-azure" | nc -w3 $ip 1234
```

## Parameters

| Parameter | Default | Description |
|-----------|---------|-------------|
| `vhdBlobUri` | (required) | URI of the VHD page blob in Azure Storage |
| `vmSize` | Standard_D2s_v3 | VM size (must support Gen1) |
| `enableAutoShutdown` | true | Auto-shutdown at 19:00 UTC |
| `shutdownTime` | 1900 | Shutdown time (24h format) |

## Manage VM

```sh
# Stop (deallocate to stop billing)
az vm deallocate -g $rg -n $rg

# Start
az vm start -g $rg -n $rg

# Redeploy with updated VHD (re-upload + delete/recreate disk)
az storage blob upload --account-name $sa -c vhds \
  -f build/tulip.vhd -n tulip.vhd --type page --overwrite
az vm deallocate -g $rg -n $rg
az disk delete -g $rg -n ${rg}-osdisk --yes
az deployment group create -g $rg \
  --template-file tests/infra/main.bicep \
  --parameters vhdBlobUri=$vhdUri
```

## Cleanup

```sh
az group delete --name $rg --yes --no-wait
```
