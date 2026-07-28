targetScope = 'resourceGroup'

@description('Azure region for all resources.')
param location string

@description('Common tags applied to all resources.')
param tags object = {}

@description('Base resource name prefix.')
param baseName string

@description('Environment label used for App Configuration key-values.')
param environmentLabel string

@description('Intent queue engine for scheduler and worker.')
@allowed([
  'local'
  'servicebus'
])
param queueEngine string

@description('Service Bus namespace host name.')
param serviceBusNamespaceHost string

@description('Service Bus queue name.')
param serviceBusQueueName string

@description('Optional MCP protected resource URL override.')
param mcpResourceUrl string = ''

@description('Optional comma-separated OAuth authorization server URLs for MCP metadata.')
param mcpAuthorizationServers string = ''

@description('Optional worker payload offload inline max bytes.')
param offloadInlineMaxBytes string = '5120'

@secure()
@description('Worker webhook destination URL.')
param workerWebhookUrl string

@secure()
@description('Optional APIM trust header secret for API hardening.')
param apimGatewaySecret string

@secure()
@description('Runtime database connection string (postgresAdminPassword embedded).')
param databaseUrl string

@description('PostgreSQL runtime app user name.')
param appUser string

@secure()
@description('PostgreSQL runtime app user password (matches the password set by the bootstrap script in data.bicep).')
param appUserPassword string

@description('Event Grid custom topic endpoint for Drasi WAL change events.')
param eventGridTopicEndpoint string = ''

@description('App Configuration seed mode: template writes key-values during ARM deployment, external defers seeding to post-provision automation.')
@allowed([
  'template'
  'external'
])
param appConfigSeedMode string = 'template'

var appConfigStoreName = take('appcs-${baseName}-${uniqueString(resourceGroup().id)}', 50)
var keyVaultName = take('kv-${baseName}-${uniqueString(resourceGroup().id)}', 24)
var labelSuffix = empty(environmentLabel) ? '' : '${'$'}${environmentLabel}'
var keyVaultRefContentType = 'application/vnd.microsoft.appconfig.keyvaultref+json;charset=utf-8'

module keyVault 'br/public:avm/res/key-vault/vault:0.13.0' = {
  name: 'keyvault-${baseName}'
  params: {
    name: keyVaultName
    location: location
    tags: tags
    sku: 'standard'
    enableRbacAuthorization: true
    enableVaultForDeployment: false
    enableVaultForTemplateDeployment: false
    enableVaultForDiskEncryption: false
    softDeleteRetentionInDays: 90
    enablePurgeProtection: true
    publicNetworkAccess: 'Disabled'
    secrets: [
      {
        name: 'execution-ledger-worker-webhook-url'
        value: workerWebhookUrl
        // [SECURITY] Raw webhook URL for Drasi WAL change events.
        // For government: migrate to Event Grid. The Drasi service (internal CAE)
        // should post to an Event Grid custom topic using managed identity
        // (System-assigned MI with EventGrid Data Sender role). Consumers
        // subscribe via Event Grid, eliminating raw webhook delivery risks.
        // Until then, this URL must point to a private endpoint within the VNet.
      }
      {
        name: 'execution-ledger-apim-gateway-secret'
        value: apimGatewaySecret
      }
      {
        name: 'execution-ledger-database-url'
        value: databaseUrl
      }
      {
        name: 'execution-ledger-app-user'
        value: appUser
      }
      {
        name: 'execution-ledger-app-password'
        value: appUserPassword
      }
    ]
  }
}

module appConfigStore 'br/public:avm/res/app-configuration/configuration-store:0.6.0' = {
  name: 'appconfig-${baseName}'
  params: {
    name: appConfigStoreName
    location: location
    tags: tags
    managedIdentities: {
      systemAssigned: true
    }
    sku: 'Standard'
    disableLocalAuth: false
    enablePurgeProtection: true
    publicNetworkAccess: 'Disabled'
    softDeleteRetentionInDays: 7
    keyValues: appConfigSeedMode == 'template' ? [
      {
        name: 'EXECUTION_LEDGER_QUEUE_ENGINE${labelSuffix}'
        value: queueEngine
      }
      {
        name: 'EXECUTION_LEDGER_SERVICEBUS_NAMESPACE${labelSuffix}'
        value: serviceBusNamespaceHost
      }
      {
        name: 'EXECUTION_LEDGER_SERVICEBUS_QUEUE_NAME${labelSuffix}'
        value: serviceBusQueueName
      }
      {
        name: 'EXECUTION_LEDGER_MCP_RESOURCE_URL${labelSuffix}'
        value: mcpResourceUrl
      }
      {
        name: 'EXECUTION_LEDGER_MCP_AUTHORIZATION_SERVERS${labelSuffix}'
        value: mcpAuthorizationServers
      }
      {
        name: 'EXECUTION_LEDGER_OFFLOAD_INLINE_MAX_BYTES${labelSuffix}'
        value: offloadInlineMaxBytes
      }
      {
        name: 'EXECUTION_LEDGER_WORKER_WEBHOOK_URL${labelSuffix}'
        value: '{"uri":"${keyVault.outputs.uri}secrets/execution-ledger-worker-webhook-url"}'
        contentType: keyVaultRefContentType
      }
      {
        name: 'EXECUTION_LEDGER_EVENT_GRID_TOPIC_ENDPOINT${labelSuffix}'
        value: eventGridTopicEndpoint
        // Event Grid custom topic for Drasi WAL change events.
        // Drasi posts CloudEvents here via managed identity (EventGrid Data Sender role).
        // Consumers subscribe to this topic instead of receiving raw webhook calls.
        // Falls back to EXECUTION_LEDGER_WORKER_WEBHOOK_URL if not configured.
      }
      {
        name: 'EXECUTION_LEDGER_APIM_GATEWAY_SECRET${labelSuffix}'
        value: '{"uri":"${keyVault.outputs.uri}secrets/execution-ledger-apim-gateway-secret"}'
        contentType: keyVaultRefContentType
      }
      {
        name: 'EXECUTION_LEDGER_DATABASE_URL${labelSuffix}'
        value: '{"uri":"${keyVault.outputs.uri}secrets/execution-ledger-database-url"}'
        contentType: keyVaultRefContentType
      }
      {
        name: 'EXECUTION_LEDGER_WORKER_MAX_CONCURRENT_INTENTS${labelSuffix}'
        value: '5'
      }
      {
        name: 'EXECUTION_LEDGER_PG_MAX_CONNECTIONS${labelSuffix}'
        value: '7'
      }
    ] : []
  }
}

output appConfigEndpoint string = appConfigStore.outputs.endpoint
output appConfigStoreName string = appConfigStore.outputs.name
output keyVaultName string = keyVault.outputs.name
