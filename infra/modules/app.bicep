targetScope = 'resourceGroup'

@description('Azure region for all resources.')
param location string

@description('Common tags applied to all resources.')
param tags object = {}

@description('Base resource name prefix.')
param baseName string

@description('Log Analytics workspace name.')
param logAnalyticsName string

@description('Container Apps environment name.')
param containerEnvironmentName string

@description('Container registry name.')
param acrName string

@description('API Container App name.')
param apiContainerAppName string

@description('Worker Container App name.')
param workerContainerAppName string

@description('Scheduler Container App name.')
param schedulerContainerAppName string

@description('Container image for execution-ledger-api.')
param apiImageName string

@description('Container image for execution-ledger-worker.')
param workerImageName string

@description('Container image for execution-ledger-scheduler.')
param schedulerImageName string

@description('Container image for execution-ledger-drasi (WAL reader).')
param drasiImageName string

@description('Drasi Container App name.')
param drasiContainerAppName string

@description('Expected Entra audience for API JWT validation.')
param entraAudience string

@description('Home Entra tenant ID seeded as active for bootstrap operations.')
param homeTenantId string

@description('Azure App Configuration endpoint used by runtime startup.')
param appConfigEndpoint string

@description('Azure App Configuration store name used for RBAC assignment.')
param appConfigStoreName string

@description('Azure Key Vault name used for secret-resolution RBAC assignment.')
param keyVaultName string

@description('Runtime environment label used by services and App Configuration.')
param runtimeEnvironmentName string = 'production'

@description('Intent queue engine for scheduler and worker: local or servicebus.')
@allowed([
  'local'
  'servicebus'
])
param queueEngine string = 'servicebus'

@secure()
@description('Runtime database connection string.')
param databaseUrl string

@description('Service Bus namespace name.')
param serviceBusNamespaceName string

@description('Service Bus namespace host used by runtime clients.')
param serviceBusNamespaceHost string

@description('Service Bus queue name.')
param serviceBusQueueName string

@description('Target queue depth per replica for worker Service Bus KEDA autoscale.')
param workerScaleQueueMessageCount string = '25'

@description('Maximum worker replicas when queue-engine is servicebus.')
@minValue(1)
@maxValue(50)
param workerScaleMaxReplicas int = 10

@description('Container Apps infrastructure subnet resource ID.')
param containerAppsInfrastructureSubnetId string

@description('ACR Pull role definition ID.')
param acrPullRoleDefinitionId string

@description('Private endpoints subnet resource ID.')
param privateEndpointsSubnetId string

@description('Service Bus private link DNS zone name.')
param serviceBusDnsZoneName string

@description('Key Vault private link DNS zone name.')
param keyVaultDnsZoneName string

@description('App Configuration private link DNS zone name.')
param appConfigDnsZoneName string

@description('ACR private link DNS zone name.')
param acrDnsZoneName string

@description('Event Grid custom topic resource ID for Drasi RBAC.')
param eventGridTopicResourceId string

@description('Event Grid private link DNS zone name.')
param eventGridDnsZoneName string

@description('Whether to create a private endpoint for Azure Container Registry.')
param enableAcrPrivateEndpoint bool = false

@description('Whether to create a private endpoint for Service Bus namespace.')
param enableServiceBusPrivateEndpoint bool = false

resource logAnalyticsWorkspace 'Microsoft.OperationalInsights/workspaces@2022-10-01' = {
  name: logAnalyticsName
  location: location
  tags: tags
  properties: {
    sku: {
      name: 'PerGB2018'
    }
    retentionInDays: 90
    workspaceCapping: {
      dailyQuotaGb: 1
    }
  }
}

module containerEnvironment 'br/public:avm/res/app/managed-environment:0.14.0' = {
  name: 'container-environment-${baseName}'
  params: {
    name: containerEnvironmentName
    location: location
    tags: tags
    infrastructureSubnetResourceId: containerAppsInfrastructureSubnetId
    internal: true
    publicNetworkAccess: 'Disabled'
    zoneRedundant: false
    appLogsConfiguration: {
      destination: 'log-analytics'
      logAnalyticsWorkspaceResourceId: logAnalyticsWorkspace.id
    }
  }
}

module containerRegistry 'br/public:avm/res/container-registry/registry:0.12.1' = {
  name: 'container-registry-${baseName}'
  params: {
    name: acrName
    location: location
    tags: tags
    acrSku: 'Basic'
    acrAdminUserEnabled: false
  }
}

resource containerRegistryResource 'Microsoft.ContainerRegistry/registries@2023-07-01' existing = {
  name: acrName
}

resource containerEnvironmentResource 'Microsoft.App/managedEnvironments@2024-03-01' existing = {
  name: containerEnvironmentName
}

// ── Private Endpoints ──────────────────────────────────────────────────────
// All data-plane services accessed over private endpoints only.
// Public network access is disabled on each service individually.

resource acrPrivateEndpoint 'Microsoft.Network/privateEndpoints@2024-10-01' = if (enableAcrPrivateEndpoint) {
  name: 'pe-acr-${baseName}'
  location: location
  tags: tags
  properties: {
    subnet: {
      id: privateEndpointsSubnetId
    }
    privateLinkServiceConnections: [
      {
        name: 'pe-acr-conn-${baseName}'
        properties: {
          privateLinkServiceId: containerRegistryResource.id
          groupIds: [
            'registry'
          ]
        }
      }
    ]
  }
  dependsOn: [
    containerRegistry
  ]
}

resource acrPrivateDnsZoneGroup 'Microsoft.Network/privateEndpoints/privateDnsZoneGroups@2024-10-01' = if (enableAcrPrivateEndpoint) {
  name: 'acr-dns-group'
  parent: acrPrivateEndpoint
  properties: {
    privateDnsZoneConfigs: [
      {
        name: 'acr-zone'
        properties: {
          privateDnsZoneId: resourceId(
            'Microsoft.Network/privateDnsZones',
            acrDnsZoneName
          )
        }
      }
    ]
  }
}

// Service Bus private endpoint — peering the namespace into the VNet
resource serviceBusPrivateEndpoint 'Microsoft.Network/privateEndpoints@2024-10-01' = if (enableServiceBusPrivateEndpoint) {
  name: 'pe-sb-${baseName}'
  location: location
  tags: tags
  properties: {
    subnet: {
      id: privateEndpointsSubnetId
    }
    privateLinkServiceConnections: [
      {
        name: 'pe-sb-conn-${baseName}'
        properties: {
          privateLinkServiceId: serviceBusNamespaceResource.id
          groupIds: [
            'namespace'
          ]
        }
      }
    ]
  }
}

resource serviceBusPrivateDnsZoneGroup 'Microsoft.Network/privateEndpoints/privateDnsZoneGroups@2024-10-01' = if (enableServiceBusPrivateEndpoint) {
  name: 'sb-dns-group'
  parent: serviceBusPrivateEndpoint
  properties: {
    privateDnsZoneConfigs: [
      {
        name: 'sb-zone'
        properties: {
          privateDnsZoneId: resourceId(
            'Microsoft.Network/privateDnsZones',
            serviceBusDnsZoneName
          )
        }
      }
    ]
  }
}

// Key Vault private endpoint
resource keyVaultPrivateEndpoint 'Microsoft.Network/privateEndpoints@2024-10-01' = {
  name: 'pe-kv-${baseName}'
  location: location
  tags: tags
  properties: {
    subnet: {
      id: privateEndpointsSubnetId
    }
    privateLinkServiceConnections: [
      {
        name: 'pe-kv-conn-${baseName}'
        properties: {
          privateLinkServiceId: keyVault.id
          groupIds: [
            'vault'
          ]
        }
      }
    ]
  }
}

resource keyVaultPrivateDnsZoneGroup 'Microsoft.Network/privateEndpoints/privateDnsZoneGroups@2024-10-01' = {
  name: 'kv-dns-group'
  parent: keyVaultPrivateEndpoint
  properties: {
    privateDnsZoneConfigs: [
      {
        name: 'kv-zone'
        properties: {
          privateDnsZoneId: resourceId(
            'Microsoft.Network/privateDnsZones',
            keyVaultDnsZoneName
          )
        }
      }
    ]
  }
}

// App Configuration private endpoint
resource appConfigPrivateEndpoint 'Microsoft.Network/privateEndpoints@2024-10-01' = {
  name: 'pe-appcfg-${baseName}'
  location: location
  tags: tags
  properties: {
    subnet: {
      id: privateEndpointsSubnetId
    }
    privateLinkServiceConnections: [
      {
        name: 'pe-appcfg-conn-${baseName}'
        properties: {
          privateLinkServiceId: appConfigStore.id
          groupIds: [
            'configurationStores'
          ]
        }
      }
    ]
  }
}

resource appConfigPrivateDnsZoneGroup 'Microsoft.Network/privateEndpoints/privateDnsZoneGroups@2024-10-01' = {
  name: 'appcfg-dns-group'
  parent: appConfigPrivateEndpoint
  properties: {
    privateDnsZoneConfigs: [
      {
        name: 'appcfg-zone'
        properties: {
          privateDnsZoneId: resourceId(
            'Microsoft.Network/privateDnsZones',
            appConfigDnsZoneName
          )
        }
      }
    ]
  }
}

// Event Grid private endpoint — Drasi posts to this from within the VNet
resource eventGridPrivateEndpoint 'Microsoft.Network/privateEndpoints@2024-10-01' = {
  name: 'pe-eg-${baseName}'
  location: location
  tags: tags
  properties: {
    subnet: {
      id: privateEndpointsSubnetId
    }
    privateLinkServiceConnections: [
      {
        name: 'pe-eg-conn-${baseName}'
        properties: {
          privateLinkServiceId: eventGridTopicResourceId
          groupIds: [
            'topic'
          ]
        }
      }
    ]
  }
}

resource eventGridPrivateDnsZoneGroup 'Microsoft.Network/privateEndpoints/privateDnsZoneGroups@2024-10-01' = {
  name: 'eg-dns-group'
  parent: eventGridPrivateEndpoint
  properties: {
    privateDnsZoneConfigs: [
      {
        name: 'eg-zone'
        properties: {
          privateDnsZoneId: resourceId(
            'Microsoft.Network/privateDnsZones',
            eventGridDnsZoneName
          )
        }
      }
    ]
  }
}

resource serviceBusNamespaceResource 'Microsoft.ServiceBus/namespaces@2024-01-01' existing = {
  name: serviceBusNamespaceName
}

resource serviceBusQueueResource 'Microsoft.ServiceBus/namespaces/queues@2024-01-01' existing = {
  parent: serviceBusNamespaceResource
  name: serviceBusQueueName
}

resource appConfigStore 'Microsoft.AppConfiguration/configurationStores@2024-06-01' existing = {
  name: appConfigStoreName
}

resource keyVault 'Microsoft.KeyVault/vaults@2024-11-01' existing = {
  name: keyVaultName
}

resource apiAcrIdentity 'Microsoft.ManagedIdentity/userAssignedIdentities@2023-01-31' = {
  name: take('id-${apiContainerAppName}-acrpull', 128)
  location: location
  tags: tags
}

resource workerAcrIdentity 'Microsoft.ManagedIdentity/userAssignedIdentities@2023-01-31' = {
  name: take('id-${workerContainerAppName}-acrpull', 128)
  location: location
  tags: tags
}

resource schedulerAcrIdentity 'Microsoft.ManagedIdentity/userAssignedIdentities@2023-01-31' = {
  name: take('id-${schedulerContainerAppName}-acrpull', 128)
  location: location
  tags: tags
}

resource drasiAcrIdentity 'Microsoft.ManagedIdentity/userAssignedIdentities@2023-01-31' = {
  name: take('id-${drasiContainerAppName}-acrpull', 128)
  location: location
  tags: tags
}

var serviceBusDataSenderRoleDefinitionId = subscriptionResourceId(
  'Microsoft.Authorization/roleDefinitions',
  '69a216fc-b8fb-44d8-bc22-1f3c2cd27a39'
)
var serviceBusDataReceiverRoleDefinitionId = subscriptionResourceId(
  'Microsoft.Authorization/roleDefinitions',
  '4f6d3b9b-027b-4f4c-9142-0e5a2a2247e0'
)
var appConfigDataReaderRoleDefinitionId = subscriptionResourceId(
  'Microsoft.Authorization/roleDefinitions',
  '516239f1-63e1-4d78-a4de-a74fb236a071'
)
var keyVaultSecretsUserRoleDefinitionId = subscriptionResourceId(
  'Microsoft.Authorization/roleDefinitions',
  '4633458b-17de-408a-b874-0445c86b69e6'
)
var workerScaleSettings = queueEngine == 'servicebus'
  ? {
      minReplicas: 1
      maxReplicas: workerScaleMaxReplicas
      rules: [
        {
          name: 'servicebus-intents'
          custom: {
            type: 'azure-servicebus'
            metadata: {
              namespace: serviceBusNamespaceName
              queueName: serviceBusQueueName
              messageCount: workerScaleQueueMessageCount
            }
            identity: 'system'
          }
        }
      ]
    }
  : {
      minReplicas: 1
      maxReplicas: 2
    }

module apiContainerApp 'br/public:avm/res/app/container-app:0.16.0' = {
  name: 'api-container-app-${baseName}'
  params: {
    name: apiContainerAppName
    location: location
    tags: tags
    environmentResourceId: containerEnvironment.outputs.resourceId
    managedIdentities: {
      systemAssigned: true
      userAssignedResourceIds: [
        apiAcrIdentity.id
      ]
    }
    activeRevisionsMode: 'Single'
    ingressExternal: true
    ingressTargetPort: 3000
    ingressTransport: 'auto'
    registries: [
      {
        server: containerRegistry.outputs.loginServer
        identity: apiAcrIdentity.id
      }
    ]
    secrets: [
      {
        name: 'database-url'
        value: databaseUrl
      }
    ]
    containers: [
      {
        name: 'api'
        image: apiImageName
        env: [
          {
            name: 'EXECUTION_LEDGER_DATABASE_URL'
            secretRef: 'database-url'
          }
          {
            name: 'EXECUTION_LEDGER_ENV'
            value: runtimeEnvironmentName
          }
          {
            name: 'EXECUTION_LEDGER_ENTRA_AUDIENCE'
            value: entraAudience
          }
          {
            name: 'EXECUTION_LEDGER_HOME_TENANT_ID'
            value: homeTenantId
          }
          {
            name: 'EXECUTION_LEDGER_APP_CONFIG_ENDPOINT'
            value: appConfigEndpoint
          }
          {
            name: 'EXECUTION_LEDGER_QUEUE_ENGINE'
            value: queueEngine
          }
          {
            name: 'EXECUTION_LEDGER_SERVICEBUS_NAMESPACE'
            value: serviceBusNamespaceHost
          }
          {
            name: 'EXECUTION_LEDGER_SERVICEBUS_QUEUE_NAME'
            value: serviceBusQueueName
          }
        ]
        resources: {
          cpu: json('0.5')
          memory: '1Gi'
        }
      }
    ]
    scaleSettings: {
      minReplicas: 1
      maxReplicas: 3
    }
  }
}

module workerContainerApp 'br/public:avm/res/app/container-app:0.16.0' = {
  name: 'worker-container-app-${baseName}'
  params: {
    name: workerContainerAppName
    location: location
    tags: tags
    environmentResourceId: containerEnvironment.outputs.resourceId
    managedIdentities: {
      systemAssigned: true
      userAssignedResourceIds: [
        workerAcrIdentity.id
      ]
    }
    disableIngress: true
    activeRevisionsMode: 'Single'
    registries: [
      {
        server: containerRegistry.outputs.loginServer
        identity: workerAcrIdentity.id
      }
    ]
    secrets: [
      {
        name: 'database-url'
        value: databaseUrl
      }
    ]
    containers: [
      {
        name: 'worker'
        image: workerImageName
        env: [
          {
            name: 'EXECUTION_LEDGER_DATABASE_URL'
            secretRef: 'database-url'
          }
          {
            name: 'EXECUTION_LEDGER_ENV'
            value: runtimeEnvironmentName
          }
          {
            name: 'EXECUTION_LEDGER_APP_CONFIG_ENDPOINT'
            value: appConfigEndpoint
          }
          {
            name: 'EXECUTION_LEDGER_ENTRA_AUDIENCE'
            value: entraAudience
          }
          {
            name: 'EXECUTION_LEDGER_HOME_TENANT_ID'
            value: homeTenantId
          }
          {
            name: 'EXECUTION_LEDGER_QUEUE_ENGINE'
            value: queueEngine
          }
          {
            name: 'EXECUTION_LEDGER_SERVICEBUS_NAMESPACE'
            value: serviceBusNamespaceHost
          }
          {
            name: 'EXECUTION_LEDGER_SERVICEBUS_QUEUE_NAME'
            value: serviceBusQueueName
          }
        ]
        resources: {
          cpu: json('0.5')
          memory: '1Gi'
        }
      }
    ]
    scaleSettings: workerScaleSettings
  }
}

module schedulerContainerApp 'br/public:avm/res/app/container-app:0.16.0' = {
  name: 'scheduler-container-app-${baseName}'
  params: {
    name: schedulerContainerAppName
    location: location
    tags: tags
    environmentResourceId: containerEnvironment.outputs.resourceId
    managedIdentities: {
      systemAssigned: true
      userAssignedResourceIds: [
        schedulerAcrIdentity.id
      ]
    }
    disableIngress: true
    activeRevisionsMode: 'Single'
    registries: [
      {
        server: containerRegistry.outputs.loginServer
        identity: schedulerAcrIdentity.id
      }
    ]
    secrets: [
      {
        name: 'database-url'
        value: databaseUrl
      }
    ]
    containers: [
      {
        name: 'scheduler'
        image: schedulerImageName
        env: [
          {
            name: 'EXECUTION_LEDGER_DATABASE_URL'
            secretRef: 'database-url'
          }
          {
            name: 'EXECUTION_LEDGER_ENV'
            value: runtimeEnvironmentName
          }
          {
            name: 'EXECUTION_LEDGER_APP_CONFIG_ENDPOINT'
            value: appConfigEndpoint
          }
          {
            name: 'EXECUTION_LEDGER_ENTRA_AUDIENCE'
            value: entraAudience
          }
          {
            name: 'EXECUTION_LEDGER_HOME_TENANT_ID'
            value: homeTenantId
          }
          {
            name: 'EXECUTION_LEDGER_QUEUE_ENGINE'
            value: queueEngine
          }
          {
            name: 'EXECUTION_LEDGER_SERVICEBUS_NAMESPACE'
            value: serviceBusNamespaceHost
          }
          {
            name: 'EXECUTION_LEDGER_SERVICEBUS_QUEUE_NAME'
            value: serviceBusQueueName
          }
        ]
        resources: {
          cpu: json('0.25')
          memory: '0.5Gi'
        }
      }
    ]
    scaleSettings: {
      minReplicas: 1
      maxReplicas: 1
    }
  }
}

module drasiContainerApp 'br/public:avm/res/app/container-app:0.16.0' = {
  name: 'drasi-container-app-${baseName}'
  params: {
    name: drasiContainerAppName
    location: location
    tags: tags
    environmentResourceId: containerEnvironment.outputs.resourceId
    managedIdentities: {
      systemAssigned: true
      userAssignedResourceIds: [
        drasiAcrIdentity.id
      ]
    }
    disableIngress: true
    activeRevisionsMode: 'Single'
    registries: [
      {
        server: containerRegistry.outputs.loginServer
        identity: drasiAcrIdentity.id
      }
    ]
    secrets: [
      {
        name: 'database-url'
        value: databaseUrl
      }
    ]
    containers: [
      {
        name: 'drasi'
        image: drasiImageName
        env: [
          {
            name: 'EXECUTION_LEDGER_DATABASE_URL'
            secretRef: 'database-url'
          }
          {
            name: 'EXECUTION_LEDGER_ENV'
            value: runtimeEnvironmentName
          }
          {
            name: 'EXECUTION_LEDGER_APP_CONFIG_ENDPOINT'
            value: appConfigEndpoint
          }
          {
            name: 'EXECUTION_LEDGER_ENTRA_AUDIENCE'
            value: entraAudience
          }
          {
            name: 'EXECUTION_LEDGER_HOME_TENANT_ID'
            value: homeTenantId
          }
          {
            name: 'EXECUTION_LEDGER_QUEUE_ENGINE'
            value: queueEngine
          }
          {
            name: 'EXECUTION_LEDGER_SERVICEBUS_NAMESPACE'
            value: serviceBusNamespaceHost
          }
          {
            name: 'EXECUTION_LEDGER_SERVICEBUS_QUEUE_NAME'
            value: serviceBusQueueName
          }
        ]
        resources: {
          cpu: json('0.5')
          memory: '1Gi'
        }
      }
    ]
    scaleSettings: {
      minReplicas: 1
      maxReplicas: 1
    }
  }
}

resource apiAcrPullRole 'Microsoft.Authorization/roleAssignments@2022-04-01' = {
  name: guid(apiContainerAppName, acrName, acrPullRoleDefinitionId)
  scope: containerRegistryResource
  properties: {
    roleDefinitionId: acrPullRoleDefinitionId
    principalId: apiAcrIdentity.properties.principalId
    principalType: 'ServicePrincipal'
  }
}

resource workerAcrPullRole 'Microsoft.Authorization/roleAssignments@2022-04-01' = {
  name: guid(workerContainerAppName, acrName, acrPullRoleDefinitionId)
  scope: containerRegistryResource
  properties: {
    roleDefinitionId: acrPullRoleDefinitionId
    principalId: workerAcrIdentity.properties.principalId
    principalType: 'ServicePrincipal'
  }
}

resource schedulerAcrPullRole 'Microsoft.Authorization/roleAssignments@2022-04-01' = {
  name: guid(schedulerContainerAppName, acrName, acrPullRoleDefinitionId)
  scope: containerRegistryResource
  properties: {
    roleDefinitionId: acrPullRoleDefinitionId
    principalId: schedulerAcrIdentity.properties.principalId
    principalType: 'ServicePrincipal'
  }
}

resource apiServiceBusDataSenderRole 'Microsoft.Authorization/roleAssignments@2022-04-01' = if (queueEngine == 'servicebus') {
  name: guid(apiContainerAppName, serviceBusQueueResource.id, serviceBusDataSenderRoleDefinitionId)
  scope: serviceBusQueueResource
  properties: {
    roleDefinitionId: serviceBusDataSenderRoleDefinitionId
    principalId: apiContainerApp.outputs.systemAssignedMIPrincipalId!
    principalType: 'ServicePrincipal'
  }
}

resource apiServiceBusDataReceiverRole 'Microsoft.Authorization/roleAssignments@2022-04-01' = if (queueEngine == 'servicebus') {
  name: guid(apiContainerAppName, serviceBusQueueResource.id, serviceBusDataReceiverRoleDefinitionId)
  scope: serviceBusQueueResource
  properties: {
    roleDefinitionId: serviceBusDataReceiverRoleDefinitionId
    principalId: apiContainerApp.outputs.systemAssignedMIPrincipalId!
    principalType: 'ServicePrincipal'
  }
}

resource workerServiceBusDataReceiverRole 'Microsoft.Authorization/roleAssignments@2022-04-01' = if (queueEngine == 'servicebus') {
  name: guid(workerContainerAppName, serviceBusQueueResource.id, serviceBusDataReceiverRoleDefinitionId)
  scope: serviceBusQueueResource
  properties: {
    roleDefinitionId: serviceBusDataReceiverRoleDefinitionId
    principalId: workerContainerApp.outputs.systemAssignedMIPrincipalId!
    principalType: 'ServicePrincipal'
  }
}

resource schedulerServiceBusDataSenderRole 'Microsoft.Authorization/roleAssignments@2022-04-01' = if (queueEngine == 'servicebus') {
  name: guid(schedulerContainerAppName, serviceBusQueueResource.id, serviceBusDataSenderRoleDefinitionId)
  scope: serviceBusQueueResource
  properties: {
    roleDefinitionId: serviceBusDataSenderRoleDefinitionId
    principalId: schedulerContainerApp.outputs.systemAssignedMIPrincipalId!
    principalType: 'ServicePrincipal'
  }
}

resource apiAppConfigDataReaderRole 'Microsoft.Authorization/roleAssignments@2022-04-01' = {
  name: guid(apiContainerAppName, appConfigStore.id, appConfigDataReaderRoleDefinitionId)
  scope: appConfigStore
  properties: {
    roleDefinitionId: appConfigDataReaderRoleDefinitionId
    principalId: apiContainerApp.outputs.systemAssignedMIPrincipalId!
    principalType: 'ServicePrincipal'
  }
}

resource workerAppConfigDataReaderRole 'Microsoft.Authorization/roleAssignments@2022-04-01' = {
  name: guid(workerContainerAppName, appConfigStore.id, appConfigDataReaderRoleDefinitionId)
  scope: appConfigStore
  properties: {
    roleDefinitionId: appConfigDataReaderRoleDefinitionId
    principalId: workerContainerApp.outputs.systemAssignedMIPrincipalId!
    principalType: 'ServicePrincipal'
  }
}

resource schedulerAppConfigDataReaderRole 'Microsoft.Authorization/roleAssignments@2022-04-01' = {
  name: guid(schedulerContainerAppName, appConfigStore.id, appConfigDataReaderRoleDefinitionId)
  scope: appConfigStore
  properties: {
    roleDefinitionId: appConfigDataReaderRoleDefinitionId
    principalId: schedulerContainerApp.outputs.systemAssignedMIPrincipalId!
    principalType: 'ServicePrincipal'
  }
}

resource apiKeyVaultSecretsUserRole 'Microsoft.Authorization/roleAssignments@2022-04-01' = {
  name: guid(apiContainerAppName, keyVault.id, keyVaultSecretsUserRoleDefinitionId)
  scope: keyVault
  properties: {
    roleDefinitionId: keyVaultSecretsUserRoleDefinitionId
    principalId: apiContainerApp.outputs.systemAssignedMIPrincipalId!
    principalType: 'ServicePrincipal'
  }
}

resource workerKeyVaultSecretsUserRole 'Microsoft.Authorization/roleAssignments@2022-04-01' = {
  name: guid(workerContainerAppName, keyVault.id, keyVaultSecretsUserRoleDefinitionId)
  scope: keyVault
  properties: {
    roleDefinitionId: keyVaultSecretsUserRoleDefinitionId
    principalId: workerContainerApp.outputs.systemAssignedMIPrincipalId!
    principalType: 'ServicePrincipal'
  }
}

resource schedulerKeyVaultSecretsUserRole 'Microsoft.Authorization/roleAssignments@2022-04-01' = {
  name: guid(schedulerContainerAppName, keyVault.id, keyVaultSecretsUserRoleDefinitionId)
  scope: keyVault
  properties: {
    roleDefinitionId: keyVaultSecretsUserRoleDefinitionId
    principalId: schedulerContainerApp.outputs.systemAssignedMIPrincipalId!
    principalType: 'ServicePrincipal'
  }
}

resource drasiAcrPullRole 'Microsoft.Authorization/roleAssignments@2022-04-01' = {
  name: guid(drasiContainerAppName, acrName, acrPullRoleDefinitionId)
  scope: containerRegistryResource
  properties: {
    roleDefinitionId: acrPullRoleDefinitionId
    principalId: drasiAcrIdentity.properties.principalId
    principalType: 'ServicePrincipal'
  }
}

resource drasiAppConfigDataReaderRole 'Microsoft.Authorization/roleAssignments@2022-04-01' = {
  name: guid(drasiContainerAppName, appConfigStore.id, appConfigDataReaderRoleDefinitionId)
  scope: appConfigStore
  properties: {
    roleDefinitionId: appConfigDataReaderRoleDefinitionId
    principalId: drasiContainerApp.outputs.systemAssignedMIPrincipalId!
    principalType: 'ServicePrincipal'
  }
}

resource drasiKeyVaultSecretsUserRole 'Microsoft.Authorization/roleAssignments@2022-04-01' = {
  name: guid(drasiContainerAppName, keyVault.id, keyVaultSecretsUserRoleDefinitionId)
  scope: keyVault
  properties: {
    roleDefinitionId: keyVaultSecretsUserRoleDefinitionId
    principalId: drasiContainerApp.outputs.systemAssignedMIPrincipalId!
    principalType: 'ServicePrincipal'
  }
}

var eventGridDataSenderRoleDefinitionId = subscriptionResourceId(
  'Microsoft.Authorization/roleDefinitions',
  'd5a91429-5739-47e2-a06b-3470a27159e7'
)

resource eventGridTopic 'Microsoft.EventGrid/topics@2025-02-15' existing = {
  name: last(split(eventGridTopicResourceId, '/'))
}

resource drasiEventGridDataSenderRole 'Microsoft.Authorization/roleAssignments@2022-04-01' = {
  name: guid(drasiContainerAppName, eventGridTopicResourceId, eventGridDataSenderRoleDefinitionId)
  scope: eventGridTopic
  properties: {
    roleDefinitionId: eventGridDataSenderRoleDefinitionId
    principalId: drasiContainerApp.outputs.systemAssignedMIPrincipalId!
    principalType: 'ServicePrincipal'
  }
}

output apiUrl string = 'https://${apiContainerApp.outputs.fqdn}'
output apiContainerAppResourceId string = apiContainerApp.outputs.resourceId
output workerContainerAppResourceId string = workerContainerApp.outputs.resourceId
output schedulerContainerAppResourceId string = schedulerContainerApp.outputs.resourceId
output drasiContainerAppResourceId string = drasiContainerApp.outputs.resourceId
output apiSystemAssignedPrincipalId string = apiContainerApp.outputs.systemAssignedMIPrincipalId!
output workerSystemAssignedPrincipalId string = workerContainerApp.outputs.systemAssignedMIPrincipalId!
output schedulerSystemAssignedPrincipalId string = schedulerContainerApp.outputs.systemAssignedMIPrincipalId!
output containerRegistryLoginServer string = containerRegistry.outputs.loginServer
output apiHostName string = apiContainerApp.outputs.fqdn
output containerEnvironmentStaticIp string = containerEnvironmentResource.properties.staticIp
