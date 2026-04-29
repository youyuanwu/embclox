// Storage account + blob container for embclox VHD uploads and VM boot
// diagnostics. Deploy this ONCE, typically in a SEPARATE resource
// group from the VM (so the VM RG can be torn down and recreated
// without re-uploading the multi-MB VHD).
//
// Usage:
//   az group create -n embclox-storage -l westus2
//   az deployment group create -g embclox-storage --name storage \
//     --template-file tests/infra/storage.bicep
//
// Then upload the VHD as a page blob:
//   sa=$(az deployment group show -g embclox-storage -n storage \
//         --query properties.outputs.storageAccount.value -o tsv)
//   az storage blob upload --account-name $sa -c vhds \
//     -f build/hyperv.vhd -n hyperv.vhd --type page --overwrite
//
// Then deploy the VM in its own RG (see vm.bicep).

var baseName = resourceGroup().name
var location = resourceGroup().location

// Storage account name: lowercase, no hyphens, max 24 chars.
var saName = replace(toLower(baseName), '-', '')
var saNameTrunc = length(saName) > 24 ? substring(saName, 0, 24) : saName

resource storageAccount 'Microsoft.Storage/storageAccounts@2023-05-01' = {
  name: saNameTrunc
  location: location
  sku: { name: 'Standard_LRS' }
  kind: 'StorageV2'
}

resource blobService 'Microsoft.Storage/storageAccounts/blobServices@2023-05-01' = {
  parent: storageAccount
  name: 'default'
}

resource vhdContainer 'Microsoft.Storage/storageAccounts/blobServices/containers@2023-05-01' = {
  parent: blobService
  name: 'vhds'
}

@description('Filename of the VHD (used in the example upload command).')
param vhdName string = 'hyperv.vhd'

output storageAccount string = saNameTrunc
output blobEndpoint string = storageAccount.properties.primaryEndpoints.blob
output vhdBlobUri string = '${storageAccount.properties.primaryEndpoints.blob}vhds/${vhdName}'
output uploadCommand string = 'az storage blob upload --account-name ${saNameTrunc} -c vhds -f build/${vhdName} -n ${vhdName} --type page --overwrite'
