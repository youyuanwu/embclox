// Deploys an Azure Gen1 VM that boots a bare-metal Tulip kernel from a VHD.
//
// Two-step deployment:
//   1. Deploy infra (storage account + network), then upload VHD:
//      az deployment group create -g <rg> --template-file tests/infra/main.bicep
//      az storage blob upload --account-name <sa> -c vhds \
//        -f build/tulip.vhd -n tulip.vhd --type page
//   2. Redeploy with VHD URI to create the VM:
//      az deployment group create -g <rg> --template-file tests/infra/main.bicep \
//        --parameters vhdBlobUri=https://<sa>.blob.core.windows.net/vhds/tulip.vhd

var baseName = resourceGroup().name
var location = resourceGroup().location

@description('URI of the VHD blob in Azure Storage (page blob). Leave empty on first deploy to create infra only.')
param vhdBlobUri string = ''

@description('VM size (must support Gen1)')
@allowed([
  'Standard_A1_v2'
  'Standard_A2_v2'
  'Standard_D2s_v3'
  'Standard_D2_v3'
  'Standard_D2s_v5'
])
param vmSize string = 'Standard_D2s_v3'

@description('Enable auto-shutdown schedule')
param enableAutoShutdown bool = true

@description('Auto-shutdown time (24h format)')
param shutdownTime string = '1900'

// Naming
var vmName = baseName
var vnetName = '${baseName}-vnet'
var nsgName = '${baseName}-nsg'
var nicName = '${baseName}-nic'
var publicIpName = '${baseName}-pip'
var osDiskName = '${baseName}-osdisk'
var saName = replace(toLower(baseName), '-', '')
var saNameTrunc = length(saName) > 24 ? substring(saName, 0, 24) : saName

// Storage account for VHD uploads and boot diagnostics
resource storageAccount 'Microsoft.Storage/storageAccounts@2023-05-01' = {
  name: saNameTrunc
  location: location
  sku: { name: 'Standard_LRS' }
  kind: 'StorageV2'
}

// Blob container for VHD images
resource blobService 'Microsoft.Storage/storageAccounts/blobServices@2023-05-01' = {
  parent: storageAccount
  name: 'default'
}

resource vhdContainer 'Microsoft.Storage/storageAccounts/blobServices/containers@2023-05-01' = {
  parent: blobService
  name: 'vhds'
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

// Managed disk from the uploaded VHD (only when vhdBlobUri is provided)
resource osDisk 'Microsoft.Compute/disks@2024-03-02' = if (!empty(vhdBlobUri)) {
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

// Gen1 VM booting from the managed disk (only when vhdBlobUri is provided)
resource vm 'Microsoft.Compute/virtualMachines@2024-11-01' = if (!empty(vhdBlobUri)) {
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
resource shutdownSchedule 'microsoft.devtestlab/schedules@2018-09-15' = if (enableAutoShutdown && !empty(vhdBlobUri)) {
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

// Outputs
var vhdBlobUrl = '${storageAccount.properties.primaryEndpoints.blob}vhds/tulip.vhd'
output storageAccount string = saNameTrunc
output uploadCommand string = 'az storage blob upload --account-name ${saNameTrunc} -c vhds -f build/tulip.vhd -n tulip.vhd --type page --overwrite'
output vhdBlobUri string = vhdBlobUrl
#disable-next-line BCP318
output vmName string = !empty(vhdBlobUri) ? vm.name : '(not deployed — provide vhdBlobUri)'
output serialConsole string = 'az serial-console connect -g ${resourceGroup().name} -n ${vmName}'
output bootDiagnostics string = 'az vm boot-diagnostics get-boot-log -g ${resourceGroup().name} -n ${vmName}'
