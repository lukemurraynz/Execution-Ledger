targetScope = 'resourceGroup'

@description('Azure region for all resources.')
param location string

@description('Common tags applied to all resources.')
param tags object = {}

@description('Base resource name prefix.')
param baseName string

@description('Backend API base URL.')
param apiBackendUrl string

@description('Backend API host header value used for internal Container Apps routing.')
param apiBackendHost string

@secure()
@description('Shared APIM trust header secret forwarded to the backend API.')
param apimGatewaySecret string

@description('APIM publisher display name.')
param publisherName string

@description('APIM publisher email.')
param publisherEmail string

@description('Entra tenant ID for OpenID Connect discovery. Use "common" for multi-tenant apps.')
param entraTenantId string = 'common'

@description('Expected Entra audience (app ID URI) for JWT validation.')
param entraAudience string

@description('Allowed CORS origin. Use "*" to allow all origins (dev only).')
param corsAllowedOrigin string = '*'

@description('Tenant-scoped rate limit: max calls per minute per APIM subscription.')
param rateLimitCallsPerMinute int = 100

@description('Maximum request body size in bytes. Requests exceeding this are rejected with 413.')
param requestMaxBodyBytes int = 1048576

@description('APIM SKU. Developer has no SLA and no VNet support. Use StandardV2 for production.')
@allowed([
  'Developer'
  'StandardV2'
])
param sku string = 'StandardV2'

@description('APIM subnet resource ID for VNet integration.')
param apimSubnetResourceId string

@description('APIM virtual network type. Must be non-None when subnetResourceId is provided.')
@allowed([
  'External'
  'Internal'
])
param virtualNetworkType string = 'External'

var apimName = take('apim-${baseName}-${uniqueString(resourceGroup().id)}', 50)
var apiName = 'execution-ledger'
var apiPath = 'execution-ledger'
var entraLoginEndpoint = environment().authentication.loginEndpoint
var openIdConfigUrl = '${entraLoginEndpoint}${entraTenantId}/v2.0/.well-known/openid-configuration'

// ── Policy files ──────────────────────────────────────────────────────────
// Policies live in standalone .xml files under modules/apim-policies/.
// This gives proper XML syntax highlighting, clean diffs, and avoids Bicep
// string-escaping issues. The APIM {{named-value}} syntax passes through
// unchanged since loadTextContent() treats content as raw text.
var apiPolicyXml = loadTextContent('apim-policies/api-policy.xml')
var jwtAuthFragmentXml = loadTextContent('apim-policies/fragment-jwt-auth.xml')
var correlationFragmentXml = loadTextContent('apim-policies/fragment-correlation-id.xml')

#disable-next-line BCP081 // AVM 0.12.0 uses api-version 2025-05-01; type defs not yet in Bicep registry. Module deploys correctly.
module apim 'br/public:avm/res/api-management/service:0.12.0' = {
  name: 'apim-${baseName}'
  params: {
    name: apimName
    location: location
    tags: tags
    managedIdentities: {
      systemAssigned: true
    }
    sku: sku
    skuCapacity: 1
    publicNetworkAccess: 'Enabled'
    virtualNetworkType: virtualNetworkType
    subnetResourceId: apimSubnetResourceId
    publisherName: publisherName
    publisherEmail: publisherEmail
    namedValues: [
      {
        name: 'execution-ledger-gateway-secret'
        displayName: 'execution-ledger-gateway-secret'
        secret: true
        value: apimGatewaySecret
      }
      {
        name: 'entra-openid-config-url'
        displayName: 'entra-openid-config-url'
        value: openIdConfigUrl
      }
      {
        name: 'entra-audience'
        displayName: 'entra-audience'
        value: entraAudience
      }
      {
        name: 'cors-allowed-origin'
        displayName: 'cors-allowed-origin'
        value: corsAllowedOrigin
      }
      {
        name: 'rate-limit-calls-per-minute'
        displayName: 'rate-limit-calls-per-minute'
        value: string(rateLimitCallsPerMinute)
      }
      {
        name: 'request-max-body-bytes'
        displayName: 'request-max-body-bytes'
        value: string(requestMaxBodyBytes)
      }
      {
        name: 'backend-host-header'
        displayName: 'backend-host-header'
        value: apiBackendHost
      }
    ]
    apis: [
      {
        name: apiName
        displayName: 'Execution Ledger API'
        path: apiPath
        protocols: [
          'https'
        ]
        subscriptionRequired: false
        serviceUrl: apiBackendUrl
        // (11) Per-operation scoping: separate GET, POST, DELETE + OPTIONS for CORS
        operations: [
          {
            name: 'read-ops'
            displayName: 'Read operations (GET)'
            method: 'GET'
            urlTemplate: '/{*path}'
            templateParameters: [
              {
                name: 'path'
                type: 'string'
                required: true
              }
            ]
            responses: [
              {
                statusCode: 200
              }
            ]
          }
          {
            name: 'write-ops'
            displayName: 'Write operations (POST)'
            method: 'POST'
            urlTemplate: '/{*path}'
            templateParameters: [
              {
                name: 'path'
                type: 'string'
                required: true
              }
            ]
            responses: [
              {
                statusCode: 200
              }
            ]
          }
          {
            name: 'delete-ops'
            displayName: 'Delete operations (DELETE)'
            method: 'DELETE'
            urlTemplate: '/{*path}'
            templateParameters: [
              {
                name: 'path'
                type: 'string'
                required: true
              }
            ]
            responses: [
              {
                statusCode: 200
              }
            ]
          }
          {
            name: 'cors-preflight'
            displayName: 'CORS preflight (OPTIONS)'
            method: 'OPTIONS'
            urlTemplate: '/{*path}'
            templateParameters: [
              {
                name: 'path'
                type: 'string'
                required: true
              }
            ]
            responses: [
              {
                statusCode: 200
              }
            ]
          }
        ]
        policies: [
          {
            name: 'policy'
            format: 'rawxml'
            value: apiPolicyXml
          }
        ]
      }
    ]
  }
}

// (9) Policy fragments — reusable auth and correlation patterns
// Created as child resources under the APIM service deployed by the AVM module.
// Consumers reference them via <include-fragment fragment-id="pf-jwt-auth" /> etc.

resource apimService 'Microsoft.ApiManagement/service@2024-05-01' existing = {
  name: apimName
}

resource jwtAuthFragment 'Microsoft.ApiManagement/service/policyFragments@2024-05-01' = {
  name: 'pf-jwt-auth'
  parent: apimService
  dependsOn: [apim]
  properties: {
    description: 'JWT validation with OpenID Connect discovery. Validates token structure, issuer, audience, and expiry. Does NOT check per-operation claims — the backend is authoritative for fine-grained authorization.'
    format: 'rawxml'
    value: jwtAuthFragmentXml
  }
}

resource correlationFragment 'Microsoft.ApiManagement/service/policyFragments@2024-05-01' = {
  name: 'pf-correlation-id'
  parent: apimService
  dependsOn: [apim]
  properties: {
    description: 'Propagates x-request-id and generates x-correlation-id when absent. Injects into request (inbound) and response (outbound) for end-to-end tracing.'
    format: 'rawxml'
    value: correlationFragmentXml
  }
}

var apimGatewayUrl = apimService.properties.gatewayUrl

output gatewayUrl string = apimGatewayUrl
output apiBaseUrl string = '${apimGatewayUrl}/${apiPath}'
