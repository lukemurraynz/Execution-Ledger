targetScope = 'resourceGroup'

// [SUSTAINABILITY]
// Region: User-configured via `location` param. Carbon intensity varies significantly
// across Azure regions. Prefer regions with documented renewable energy commitments
// when other constraints allow. Document the region decision in deployment notes.
//
// Compute: Three Container Apps (api, worker, scheduler). Worker scales via KEDA
// (queue-driven, min 1 / max 10). API and scheduler are fixed replicas. Consider
// scheduled scale-down for non-production environments.
//
// Storage: PostgreSQL Flexible Server (General Purpose). Lifecycle: no explicit
// retention policy in Bicep — add backup retention and data lifecycle policies
// post-deployment.
//
// Messaging: Service Bus Standard tier. Queue depth drives worker autoscale.
// No idle capacity when queue is empty (worker scales to min replicas).
//
// Observability: Log Analytics workspace. Set retention policy post-deployment
// (default 30 days). Diagnostic categories should be reviewed for ingestion cost.

@description('Azure region for all resources.')
param location string = resourceGroup().location

@description('AZD environment name (for deterministic resource naming).')
param environmentName string

@description('Common tags applied to all resources.')
param tags object = {}

@description('Container image for execution-ledger-api.')
param apiImageName string

@description('Container image for execution-ledger-worker.')
param workerImageName string

@description('Container image for execution-ledger-scheduler.')
param schedulerImageName string

@description('Container image for execution-ledger-drasi.')
param drasiImageName string

@description('PostgreSQL administrator login.')
param postgresAdminLogin string = 'ledgeradmin'

@secure()
@description('PostgreSQL administrator password.')
param postgresAdminPassword string

@description('Execution Ledger runtime database name.')
param postgresDatabaseName string = 'executionledger'

@description('Expected Entra audience for API JWT validation. Auto-generated from identity module. Override only if using a manually created app registration.')
param entraAudience string = ''

@secure()
@description('Worker webhook destination URL.')
param workerWebhookUrl string

@secure()
@description('Optional APIM trust header secret for API hardening.')
param apimGatewaySecret string

@description('APIM publisher display name.')
param apimPublisherName string

@description('APIM SKU. Developer is for non-production only (no SLA, no VNet). Standard V2 required for VNet integration.')
@allowed([
  'Developer'
  'StandardV2'
])
param apimSku string = 'StandardV2'

@description('APIM publisher email.')
param apimPublisherEmail string

@description('Entra tenant ID for APIM-level JWT validation. Use "common" for multi-tenant apps.')
param apimEntraTenantId string = 'common'

@description('CORS allowed origin for APIM gateway. Required for production — no default.')
param apimCorsAllowedOrigin string

@description('Tenant-scoped rate limit: max calls per minute per APIM subscription.')
param apimRateLimitCallsPerMinute int = 100

@description('Maximum request body size in bytes at the APIM gateway. Requests exceeding this are rejected with 413.')
param apimRequestMaxBodyBytes int = 1048576

@description('Optional MCP protected resource URL override.')
param mcpResourceUrl string = ''

@description('Optional comma-separated OAuth authorization server URLs for MCP metadata.')
param mcpAuthorizationServers string = ''

@description('Optional worker payload offload inline max bytes.')
param offloadInlineMaxBytes string = '5120'

@description('Intent queue engine for scheduler and worker: local or servicebus.')
@allowed([
  'local'
  'servicebus'
])
param queueEngine string = 'servicebus'

@description('Target queue depth per replica for worker Service Bus KEDA autoscale.')
param workerScaleQueueMessageCount string = '25'

@description('Maximum worker replicas when queue-engine is servicebus.')
@minValue(1)
@maxValue(50)
param workerScaleMaxReplicas int = 10

@description('Runtime PostgreSQL credential mode: app uses least-privilege role provisioning, admin uses PostgreSQL admin credentials directly.')
@allowed([
  'app'
  'admin'
])
param runtimeDatabaseCredentialMode string = 'app'

@description('App Configuration seed mode: template writes key-values during ARM deployment, external defers seeding to post-provision automation.')
@allowed([
  'template'
  'external'
])
param appConfigSeedMode string = 'template'

@description('API Management deployment mode: enabled deploys APIM gateway, disabled skips APIM resources.')
@allowed([
  'enabled'
  'disabled'
])
param apimDeploymentMode string = 'enabled'

@description('Address prefix for the workload virtual network.')
param virtualNetworkAddressPrefix string = '10.90.0.0/16'

@description('Address prefix for the Container Apps infrastructure subnet.')
param containerAppsInfrastructureSubnetPrefix string = '10.90.0.0/23'

@description('Address prefix for the delegated PostgreSQL subnet.')
param postgresDelegatedSubnetPrefix string = '10.90.2.0/24'

@description('Address prefix for the private endpoints subnet.')
param privateEndpointsSubnetPrefix string = '10.90.3.0/24'

@description('Address prefix for the APIM VNet integration subnet.')
param apimSubnetPrefix string = '10.90.4.0/24'

var baseName = toLower(replace(environmentName, '_', '-'))
var logAnalyticsName = take('law-${baseName}', 63)
var containerEnvironmentName = take('cae-${baseName}', 58)
var acrName = take(toLower('acr${uniqueString(resourceGroup().id, environmentName)}'), 50)
var postgresServerName = take('pg-${baseName}-${uniqueString(resourceGroup().id)}', 63)
var serviceBusNamespaceName = take('sb-${baseName}-${uniqueString(resourceGroup().id)}', 50)
var serviceBusQueueName = 'execution-ledger-intents'
var eventGridTopicName = take('egt-${baseName}-${uniqueString(resourceGroup().id)}', 50)
var postgresPrivateDnsZoneName = '${baseName}.postgres.database.azure.com'
var virtualNetworkName = take('vnet-${baseName}', 64)
var apiContainerAppName = take('${baseName}-api', 32)
var workerContainerAppName = take('${baseName}-worker', 32)
var schedulerContainerAppName = take('${baseName}-scheduler', 32)
var drasiContainerAppName = take('${baseName}-drasi', 32)
var acrPullRoleDefinitionId = subscriptionResourceId(
  'Microsoft.Authorization/roleDefinitions',
  '7f951dda-4ed3-4680-a7ca-43fe172d538d'
)

// ── Entra ID app registration (Microsoft.Graph) ────────────────────────────
// Creates app registration, scopes, and service principal when no manual audience is supplied.
// Deployment identity needs Application.ReadWrite.OwnedBy in Microsoft Graph.
module identity 'modules/identity.bicep' = {
  name: 'identity-${baseName}'
  params: {
    baseName: baseName
    entraTenantId: tenant().tenantId
  }
}

module network 'modules/network.bicep' = {
  name: 'network-${baseName}'
  params: {
    location: location
    tags: tags
    virtualNetworkName: virtualNetworkName
    virtualNetworkAddressPrefix: virtualNetworkAddressPrefix
    containerAppsInfrastructureSubnetPrefix: containerAppsInfrastructureSubnetPrefix
    postgresDelegatedSubnetPrefix: postgresDelegatedSubnetPrefix
    privateEndpointsSubnetPrefix: privateEndpointsSubnetPrefix
    apimSubnetPrefix: apimSubnetPrefix
    postgresPrivateDnsZoneName: postgresPrivateDnsZoneName
    postgresPrivateDnsZoneLinkName: '${baseName}-vnet-link'
  }
}

module data 'modules/data.bicep' = {
  name: 'data-${baseName}'
  params: {
    location: location
    tags: tags
    postgresServerName: postgresServerName
    postgresAdminLogin: postgresAdminLogin
    postgresAdminPassword: postgresAdminPassword
    postgresDatabaseName: postgresDatabaseName
    postgresDelegatedSubnetId: network.outputs.postgresDelegatedSubnetId
    postgresPrivateDnsZoneId: network.outputs.postgresPrivateDnsZoneId
    enableAppRoleProvisioning: runtimeDatabaseCredentialMode == 'app'
  }
}

module messaging 'modules/messaging.bicep' = {
  name: 'messaging-${baseName}'
  params: {
    location: location
    tags: tags
    serviceBusNamespaceName: serviceBusNamespaceName
    serviceBusQueueName: serviceBusQueueName
    eventGridTopicName: eventGridTopicName
  }
}

var effectiveEntraAudience = empty(entraAudience) ? identity.outputs.identifierUri : entraAudience

var databaseUrl = runtimeDatabaseCredentialMode == 'admin'
  ? 'postgresql://${postgresAdminLogin}:${postgresAdminPassword}@${data.outputs.postgresServerFqdn}:5432/${postgresDatabaseName}?sslmode=require'
  : 'postgresql://${data.outputs.appUser}:${guid(data.outputs.postgresServerName)}@${data.outputs.postgresServerFqdn}:5432/${postgresDatabaseName}?sslmode=require'

module configuration 'modules/configuration.bicep' = {
  name: 'configuration-${baseName}'
  params: {
    location: location
    tags: tags
    baseName: baseName
    environmentLabel: environmentName
    queueEngine: queueEngine
    serviceBusNamespaceHost: messaging.outputs.serviceBusNamespaceHost
    serviceBusQueueName: messaging.outputs.serviceBusQueueName
    mcpResourceUrl: mcpResourceUrl
    mcpAuthorizationServers: mcpAuthorizationServers
    offloadInlineMaxBytes: offloadInlineMaxBytes
    workerWebhookUrl: workerWebhookUrl
    apimGatewaySecret: apimGatewaySecret
    databaseUrl: databaseUrl
    appUser: runtimeDatabaseCredentialMode == 'admin' ? postgresAdminLogin : data.outputs.appUser
    appUserPassword: runtimeDatabaseCredentialMode == 'admin' ? postgresAdminPassword : guid(data.outputs.postgresServerName)
    eventGridTopicEndpoint: messaging.outputs.eventGridTopicEndpoint
    appConfigSeedMode: appConfigSeedMode
  }
}

module app 'modules/app.bicep' = {
  name: 'app-${baseName}'
  params: {
    location: location
    tags: tags
    baseName: baseName
    logAnalyticsName: logAnalyticsName
    containerEnvironmentName: containerEnvironmentName
    acrName: acrName
    apiContainerAppName: apiContainerAppName
    workerContainerAppName: workerContainerAppName
    schedulerContainerAppName: schedulerContainerAppName
    drasiContainerAppName: drasiContainerAppName
    apiImageName: apiImageName
    workerImageName: workerImageName
    schedulerImageName: schedulerImageName
    drasiImageName: drasiImageName
    appConfigEndpoint: configuration.outputs.appConfigEndpoint
    appConfigStoreName: configuration.outputs.appConfigStoreName
    keyVaultName: configuration.outputs.keyVaultName
    runtimeEnvironmentName: environmentName
    entraAudience: effectiveEntraAudience
    homeTenantId: subscription().tenantId
    databaseUrl: databaseUrl
    queueEngine: queueEngine
    serviceBusNamespaceName: messaging.outputs.serviceBusNamespaceName
    serviceBusNamespaceHost: messaging.outputs.serviceBusNamespaceHost
    serviceBusQueueName: messaging.outputs.serviceBusQueueName
    workerScaleQueueMessageCount: workerScaleQueueMessageCount
    workerScaleMaxReplicas: workerScaleMaxReplicas
    containerAppsInfrastructureSubnetId: network.outputs.containerAppsInfrastructureSubnetId
    privateEndpointsSubnetId: network.outputs.privateEndpointsSubnetId
    acrPullRoleDefinitionId: acrPullRoleDefinitionId
    serviceBusDnsZoneName: network.outputs.serviceBusDnsZoneName
    keyVaultDnsZoneName: network.outputs.keyVaultDnsZoneName
    appConfigDnsZoneName: network.outputs.appConfigDnsZoneName
    acrDnsZoneName: network.outputs.acrDnsZoneName
    eventGridDnsZoneName: network.outputs.eventGridDnsZoneName
    eventGridTopicResourceId: messaging.outputs.eventGridTopicResourceId
  }
}

module apim 'modules/apim.bicep' = if (apimDeploymentMode == 'enabled') {
  name: take('apim-${baseName}-${uniqueString(resourceGroup().id, deployment().name)}', 64)
  params: {
    location: location
    tags: tags
    baseName: baseName
    apiBackendUrl: 'http://${app.outputs.containerEnvironmentStaticIp}'
    apiBackendHost: app.outputs.apiHostName
    apimGatewaySecret: apimGatewaySecret
    publisherName: apimPublisherName
    publisherEmail: apimPublisherEmail
    entraTenantId: apimEntraTenantId
    entraAudience: effectiveEntraAudience
    corsAllowedOrigin: apimCorsAllowedOrigin
    rateLimitCallsPerMinute: apimRateLimitCallsPerMinute
    requestMaxBodyBytes: apimRequestMaxBodyBytes
    sku: apimSku
    apimSubnetResourceId: network.outputs.apimSubnetId
    virtualNetworkType: 'External'
  }
}

output apiUrl string = app.outputs.apiUrl
output apimGatewayUrl string = apimDeploymentMode == 'enabled' ? apim!.outputs.gatewayUrl : app.outputs.apiUrl
output EXECUTION_LEDGER_ENTRA_AUDIENCE string = effectiveEntraAudience
output apiContainerAppResourceId string = app.outputs.apiContainerAppResourceId
output workerContainerAppResourceId string = app.outputs.workerContainerAppResourceId
output schedulerContainerAppResourceId string = app.outputs.schedulerContainerAppResourceId
output drasiContainerAppResourceId string = app.outputs.drasiContainerAppResourceId
output appConfigEndpoint string = configuration.outputs.appConfigEndpoint
output keyVaultName string = configuration.outputs.keyVaultName
output postgresServerName string = data.outputs.postgresServerName
output postgresDatabaseName string = data.outputs.postgresDatabaseName
output containerRegistryLoginServer string = app.outputs.containerRegistryLoginServer
