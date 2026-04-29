# Azure Gen1 VM Deployment

Deploys an Azure Gen1 VM that boots an embclox kernel directly from a
VHD. Boot diagnostics expose the kernel's serial output via the Azure
serial console.

## Layout: two resource groups

We use **separate resource groups** for storage and for the VM. This
lets you tear down and recreate the VM (during driver iteration)
without re-uploading the multi-MB VHD page blob each time.

| RG | Bicep | Contains | Frequency |
|----|-------|----------|-----------|
| `embclox-storage` | `storage.bicep` | Storage account + `vhds` container + uploaded VHD blob | Once per environment |
| `embclox-vm` | `vm.bicep` | NSG, VNet, public IP, NIC, OS disk, VM, auto-shutdown | Re-create per VHD iteration |

Both RGs should be in the same Azure region so cross-RG blob URL
imports stay fast and free.

## Which VHD?

| VHD | Driver | Use on Azure? |
|-----|--------|---------------|
| `build/hyperv.vhd` | NetVSC (VMBus) | ✅ **yes** — production path |
| `build/tulip.vhd` | DEC 21140 (PCI) | ❌ no — Azure Gen1 doesn't expose Tulip on PCI |

Azure Gen1 VMs only expose chipset bridges + a Hyper-V synthetic VGA on
PCI. Networking goes through VMBus NetVSC, so the hyperv example is the
only one that actually does I/O on Azure.

## Prerequisites

- Azure CLI (`az login`)
- VHD built locally:
  ```sh
  cmake -B build
  cmake --build build --target hyperv-vhd
  ```

## Deploy

### 1. Provision storage (once)

```sh
location=westus2
storageRg=embclox-storage

az group create --name $storageRg --location $location
az deployment group create -g $storageRg \
  --name storage \
  --template-file tests/infra/storage.bicep
```

### 2. Upload the VHD page blob

```sh
sa=$(az deployment group show -g $storageRg -n storage \
  --query properties.outputs.storageAccount.value -o tsv)

az storage blob upload --account-name $sa -c vhds \
  -f build/hyperv.vhd -n hyperv.vhd --type page --overwrite
```

### 3. Deploy the VM (in its own RG)

```sh
vmRg=embclox-vm
vhdUri="https://${sa}.blob.core.windows.net/vhds/hyperv.vhd"

az group create --name $vmRg --location $location
az deployment group create -g $vmRg \
  --name vm \
  --template-file tests/infra/vm.bicep \
  --parameters vhdBlobUri=$vhdUri \
               storageAccountName=$sa \
               storageResourceGroup=$storageRg
```

## Verify

```sh
ip=$(az deployment group show -g $vmRg -n vm \
  --query properties.outputs.publicIpAddress.value -o tsv)

# Serial output (boot diagnostics) — look for "PHASE4B: DHCP assigned: ..."
az vm boot-diagnostics get-boot-log -g $vmRg -n $vmRg

# TCP echo
echo "hello-azure" | nc -w3 $ip 1234
```

The kernel uses DHCP — Azure has a production-grade DHCP server that
just works.

## Iterate (rebuild VHD, redeploy VM)

```sh
# Rebuild and re-upload (storage RG only)
cmake --build build --target hyperv-vhd
az storage blob upload --account-name $sa -c vhds \
  -f build/hyperv.vhd -n hyperv.vhd --type page --overwrite

# Recreate the VM (VM RG only — storage stays untouched)
az vm deallocate -g $vmRg -n $vmRg
az disk delete -g $vmRg -n ${vmRg}-osdisk --yes
az deployment group create -g $vmRg \
  --name vm \
  --template-file tests/infra/vm.bicep \
  --parameters vhdBlobUri=$vhdUri \
               storageAccountName=$sa \
               storageResourceGroup=$storageRg
```

For aggressive iteration just delete the whole VM RG and redeploy
step 3:
```sh
az group delete --name $vmRg --yes
az group create --name $vmRg --location $location
# ... step 3 again
```

## Parameters

### `storage.bicep`

| Parameter | Default | Notes |
|-----------|---------|-------|
| `vhdName` | `hyperv.vhd` | Filename surfaced in `uploadCommand` output. |

### `vm.bicep`

| Parameter | Default | Notes |
|-----------|---------|-------|
| `vhdBlobUri` | (required) | URI of the page blob, output by `storage.bicep`. |
| `storageAccountName` | (required) | Storage account name, output by `storage.bicep`. |
| `storageResourceGroup` | current RG | Set this when storage lives in a separate RG (recommended pattern). |
| `vmSize` | `Standard_D2s_v3` | Must support Gen1 (most v3/v5 sizes do). |
| `enableAutoShutdown` | `true` | Auto-shutdown at 19:00 UTC to limit billing. |
| `shutdownTime` | `1900` | 24h format. |

## Manage

```sh
# Stop (deallocate to stop billing)
az vm deallocate -g $vmRg -n $vmRg

# Start
az vm start -g $vmRg -n $vmRg
```

## Cleanup

```sh
# Tear down VM only (cheap; can recreate from same VHD blob)
az group delete --name $vmRg --yes --no-wait

# Tear down everything including storage and the VHD blob
az group delete --name $storageRg --yes --no-wait
```
