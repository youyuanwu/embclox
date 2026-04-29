// Azure Gen1 VM that boots an embclox kernel directly from a VHD.
//
// Prerequisite: storage.bicep deployed to a (typically separate) RG
// and the VHD uploaded as a page blob.
//
// Usage:
//   storageRg=embclox-storage
//   vmRg=embclox-vm
//   sa=$(az deployment group show -g $storageRg -n storage \
//         --query properties.outputs.storageAccount.value -o tsv)
//   vhdUri="https://${sa}.blob.core.windows.net/vhds/hyperv.vhd"
//   az deployment group create -g $vmRg --template-file tests/infra/vm.bicep \
//     --parameters vhdBlobUri=$vhdUri storageAccountName=$sa \
//                  storageResourceGroup=$storageRg
//
// Re-deploying with an updated VHD: re-upload the blob to storage RG,
// then in the VM RG:
//   az vm deallocate -g $vmRg -n <vmName>
//   az disk delete -g $vmRg -n <vmName>-osdisk --yes
//   az deployment group create -g $vmRg --template-file tests/infra/vm.bicep ...
// Storage RG is never touched.

var baseName = resourceGroup().name
var location = resourceGroup().location

@description('URI of the VHD page blob in Azure Storage.')
param vhdBlobUri string

@description('Storage account name (output by storage.bicep).')
param storageAccountName string

@description('Resource group containing the storage account. Defaults to the VM resource group; set this when storage lives in a separate RG (recommended pattern, keeps storage durable across VM teardowns).')
param storageResourceGroup string = resourceGroup().name

@description('VM size (must support Gen1).')
@allowed([
  'Standard_A1_v2'
  'Standard_A2_v2'
  'Standard_D2s_v3'
  'Standard_D2_v3'
  'Standard_D2s_v5'
])
param vmSize string = 'Standard_D2s_v3'

@description('Enable auto-shutdown schedule.')
param enableAutoShutdown bool = true

@description('Auto-shutdown time (24h format).')
param shutdownTime string = '1900'

// Naming
var vmName = baseName
var vnetName = '${baseName}-vnet'
var nsgName = '${baseName}-nsg'
var nicName = '${baseName}-nic'
var publicIpName = '${baseName}-pip'
var osDiskName = '${baseName}-osdisk'

// Reference the storage account that storage.bicep created.
// Scoped explicitly so the storage account can live in a different RG.
resource storageAccount 'Microsoft.Storage/storageAccounts@2023-05-01' existing = {
  name: storageAccountName
  scope: resourceGroup(storageResourceGroup)
}

// NSG — allow inbound TCP 1234 (echo server)
resource nsg 'Microsoft.Network/networkSecurityGroups@2024-07-01' = {
  name: nsgName
  location: location
  properties: {
    securityRules: [
      {
        name: 'AllowEchoServer'
        properties: {
          protocol: 'TCP'
          sourcePortRange: '*'
          destinationPortRange: '1234'
          sourceAddressPrefix: '*'
          destinationAddressPrefix: '*'
          access: 'Allow'
          priority: 300
          direction: 'Inbound'
        }
      }
    ]
  }
}

// VNet
resource vnet 'Microsoft.Network/virtualNetworks@2024-07-01' = {
  name: vnetName
  location: location
  properties: {
    addressSpace: { addressPrefixes: ['10.0.0.0/16'] }
    subnets: [
      {
        name: 'default'
        properties: {
          addressPrefix: '10.0.0.0/24'
          networkSecurityGroup: { id: nsg.id }
        }
      }
    ]
  }
}

resource subnet 'Microsoft.Network/virtualNetworks/subnets@2024-07-01' existing = {
  parent: vnet
  name: 'default'
}

// Public IP
resource publicIp 'Microsoft.Network/publicIPAddresses@2024-07-01' = {
  name: publicIpName
  location: location
  sku: { name: 'Standard' }
  properties: {
    publicIPAddressVersion: 'IPv4'
    publicIPAllocationMethod: 'Static'
  }
}

// NIC
resource nic 'Microsoft.Network/networkInterfaces@2024-07-01' = {
  name: nicName
  location: location
  properties: {
    ipConfigurations: [
      {
        name: 'ipconfig1'
        properties: {
          privateIPAllocationMethod: 'Dynamic'
          publicIPAddress: { id: publicIp.id }
          subnet: { id: subnet.id }
          primary: true
        }
      }
    ]
    enableAcceleratedNetworking: false
    networkSecurityGroup: { id: nsg.id }
  }
}

// Managed disk imported from the uploaded VHD page blob.
resource osDisk 'Microsoft.Compute/disks@2024-03-02' = {
  name: osDiskName
  location: location
  properties: {
    osType: 'Linux'
    hyperVGeneration: 'V1'
    creationData: {
      createOption: 'Import'
      sourceUri: vhdBlobUri
      storageAccountId: storageAccount.id
    }
  }
}

// Gen1 VM booting from the managed disk.
resource vm 'Microsoft.Compute/virtualMachines@2024-11-01' = {
  name: vmName
  location: location
  properties: {
    hardwareProfile: { vmSize: vmSize }
    storageProfile: {
      osDisk: {
        osType: 'Linux'
        name: osDiskName
        createOption: 'Attach'
        managedDisk: { id: osDisk.id }
      }
    }
    networkProfile: {
      networkInterfaces: [
        { id: nic.id, properties: { primary: true } }
      ]
    }
    diagnosticsProfile: {
      bootDiagnostics: {
        enabled: true
        storageUri: storageAccount.properties.primaryEndpoints.blob
      }
    }
  }
}

// Auto-shutdown
resource shutdownSchedule 'microsoft.devtestlab/schedules@2018-09-15' = if (enableAutoShutdown) {
  name: 'shutdown-computevm-${vmName}'
  location: location
  properties: {
    status: 'Enabled'
    taskType: 'ComputeVmShutdownTask'
    dailyRecurrence: { time: shutdownTime }
    timeZoneId: 'UTC'
    notificationSettings: { status: 'Disabled', timeInMinutes: 30, notificationLocale: 'en' }
    targetResourceId: vm.id
  }
}

output vmName string = vm.name
output publicIpAddress string = publicIp.properties.ipAddress
output serialConsole string = 'az serial-console connect -g ${resourceGroup().name} -n ${vmName}'
output bootDiagnostics string = 'az vm boot-diagnostics get-boot-log -g ${resourceGroup().name} -n ${vmName}'
