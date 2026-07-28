targetScope = 'resourceGroup'

@description('Azure region for all resources.')
param location string

@description('Common tags applied to all resources.')
param tags object = {}

@description('Virtual network name.')
param virtualNetworkName string

@description('Address prefix for the workload virtual network.')
param virtualNetworkAddressPrefix string

@description('Address prefix for the Container Apps infrastructure subnet.')
param containerAppsInfrastructureSubnetPrefix string

@description('Address prefix for the delegated PostgreSQL subnet.')
param postgresDelegatedSubnetPrefix string

@description('Address prefix for the private endpoints subnet.')
param privateEndpointsSubnetPrefix string

@description('Address prefix for the APIM VNet integration subnet.')
param apimSubnetPrefix string

@description('Private DNS zone name for PostgreSQL.')
param postgresPrivateDnsZoneName string

@description('Private DNS zone VNet link name for PostgreSQL.')
param postgresPrivateDnsZoneLinkName string

var networkWatcherName = 'NetworkWatcher_${toLower(location)}'
var virtualNetworkDeploymentName = take('vnet-${virtualNetworkName}', 64)
var postgresPrivateDnsZoneDeploymentName = take('pdns-pg-${uniqueString(resourceGroup().id, postgresPrivateDnsZoneName)}', 64)
var serviceBusDnsZoneDeploymentName = take('pdns-sb-${uniqueString(resourceGroup().id, virtualNetworkName)}', 64)
var keyVaultDnsZoneDeploymentName = take('pdns-kv-${uniqueString(resourceGroup().id, virtualNetworkName)}', 64)
var appConfigDnsZoneDeploymentName = take('pdns-appcfg-${uniqueString(resourceGroup().id, virtualNetworkName)}', 64)
var acrDnsZoneDeploymentName = take('pdns-acr-${uniqueString(resourceGroup().id, virtualNetworkName)}', 64)
var eventGridDnsZoneDeploymentName = take('pdns-eg-${uniqueString(resourceGroup().id, virtualNetworkName)}', 64)
var apimSubnetNetworkSecurityGroupName = take('nsg-${virtualNetworkName}-apim', 80)

resource apimSubnetNetworkSecurityGroup 'Microsoft.Network/networkSecurityGroups@2024-10-01' = {
  name: apimSubnetNetworkSecurityGroupName
  location: location
  tags: tags
}

// ── Virtual Network ────────────────────────────────────────────────────────

module virtualNetwork 'br/public:avm/res/network/virtual-network:0.10.0' = {
  name: virtualNetworkDeploymentName
  params: {
    name: virtualNetworkName
    location: location
    tags: tags
    addressPrefixes: [
      virtualNetworkAddressPrefix
    ]
    subnets: [
      {
        name: 'container-apps-infra'
        addressPrefix: containerAppsInfrastructureSubnetPrefix
        delegation: 'Microsoft.App/environments'
      }
      {
        name: 'postgres-delegated'
        addressPrefix: postgresDelegatedSubnetPrefix
        delegation: 'Microsoft.DBforPostgreSQL/flexibleServers'
      }
      {
        name: 'private-endpoints'
        addressPrefix: privateEndpointsSubnetPrefix
      }
      {
        name: 'apim'
        addressPrefix: apimSubnetPrefix
        delegation: 'Microsoft.Web/serverFarms'
        networkSecurityGroupResourceId: apimSubnetNetworkSecurityGroup.id
      }
    ]
  }
}

resource networkWatcher 'Microsoft.Network/networkWatchers@2024-10-01' = {
  name: networkWatcherName
  location: location
  tags: tags
}

// ── Private DNS Zones ──────────────────────────────────────────────────────

// PostgreSQL — custom delegated zone (not standard private link)
module postgresPrivateDnsZone 'br/public:avm/res/network/private-dns-zone:0.8.1' = {
  name: postgresPrivateDnsZoneDeploymentName
  params: {
    name: postgresPrivateDnsZoneName
    location: 'global'
    tags: tags
    virtualNetworkLinks: [
      {
        name: postgresPrivateDnsZoneLinkName
        virtualNetworkResourceId: virtualNetwork.outputs.resourceId
        registrationEnabled: false
        location: 'global'
      }
    ]
  }
}

// Service Bus private link DNS
var serviceBusDnsZoneName = 'privatelink.servicebus.windows.net'
module serviceBusDnsZone 'br/public:avm/res/network/private-dns-zone:0.8.1' = {
  name: serviceBusDnsZoneDeploymentName
  params: {
    name: serviceBusDnsZoneName
    location: 'global'
    tags: tags
    virtualNetworkLinks: [
      {
        name: 'sb-vnet-link-${virtualNetworkName}'
        virtualNetworkResourceId: virtualNetwork.outputs.resourceId
        registrationEnabled: false
        location: 'global'
      }
    ]
  }
}

// Key Vault private link DNS
var keyVaultDnsZoneName = 'privatelink.vaultcore.azure.net'
module keyVaultDnsZone 'br/public:avm/res/network/private-dns-zone:0.8.1' = {
  name: keyVaultDnsZoneDeploymentName
  params: {
    name: keyVaultDnsZoneName
    location: 'global'
    tags: tags
    virtualNetworkLinks: [
      {
        name: 'kv-vnet-link-${virtualNetworkName}'
        virtualNetworkResourceId: virtualNetwork.outputs.resourceId
        registrationEnabled: false
        location: 'global'
      }
    ]
  }
}

// App Configuration private link DNS
var appConfigDnsZoneName = 'privatelink.azconfig.io'
module appConfigDnsZone 'br/public:avm/res/network/private-dns-zone:0.8.1' = {
  name: appConfigDnsZoneDeploymentName
  params: {
    name: appConfigDnsZoneName
    location: 'global'
    tags: tags
    virtualNetworkLinks: [
      {
        name: 'appcfg-vnet-link-${virtualNetworkName}'
        virtualNetworkResourceId: virtualNetwork.outputs.resourceId
        registrationEnabled: false
        location: 'global'
      }
    ]
  }
}

// ACR private link DNS
var acrDnsZoneName = 'privatelink.azurecr.io'
module acrDnsZone 'br/public:avm/res/network/private-dns-zone:0.8.1' = {
  name: acrDnsZoneDeploymentName
  params: {
    name: acrDnsZoneName
    location: 'global'
    tags: tags
    virtualNetworkLinks: [
      {
        name: 'acr-vnet-link-${virtualNetworkName}'
        virtualNetworkResourceId: virtualNetwork.outputs.resourceId
        registrationEnabled: false
        location: 'global'
      }
    ]
  }
}

// Event Grid private link DNS
var eventGridDnsZoneName = 'privatelink.eventgrid.azure.net'
module eventGridDnsZone 'br/public:avm/res/network/private-dns-zone:0.8.1' = {
  name: eventGridDnsZoneDeploymentName
  params: {
    name: eventGridDnsZoneName
    location: 'global'
    tags: tags
    virtualNetworkLinks: [
      {
        name: 'eg-vnet-link-${virtualNetworkName}'
        virtualNetworkResourceId: virtualNetwork.outputs.resourceId
        registrationEnabled: false
        location: 'global'
      }
    ]
  }
}

// ── Outputs ────────────────────────────────────────────────────────────────

output containerAppsInfrastructureSubnetId string = virtualNetwork.outputs.subnetResourceIds[0]
output postgresDelegatedSubnetId string = virtualNetwork.outputs.subnetResourceIds[1]
output privateEndpointsSubnetId string = virtualNetwork.outputs.subnetResourceIds[2]
output apimSubnetId string = virtualNetwork.outputs.subnetResourceIds[3]
output postgresPrivateDnsZoneId string = postgresPrivateDnsZone.outputs.resourceId
output serviceBusDnsZoneName string = serviceBusDnsZoneName
output keyVaultDnsZoneName string = keyVaultDnsZoneName
output appConfigDnsZoneName string = appConfigDnsZoneName
output acrDnsZoneName string = acrDnsZoneName
output eventGridDnsZoneName string = eventGridDnsZoneName
output virtualNetworkName string = virtualNetwork.outputs.name
