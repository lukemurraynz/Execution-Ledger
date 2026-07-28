targetScope = 'resourceGroup'

@description('Base resource name prefix for deterministic naming.')
param baseName string

@description('Entra tenant ID. Defaults to deployment tenant.')
param entraTenantId string = tenant().tenantId

// ── Entra ID identity layer ────────────────────────────────────────────────
// Provisions app/group names and deterministic audience for post-provision setup.
//
// Cleanup: azd predown hook deletes the app + group so azd down is clean.

var appDisplayName = 'execution-ledger-${baseName}'
var identifierUri = 'api://${entraTenantId}/execution-ledger-${baseName}-${uniqueString(resourceGroup().id, entraTenantId)}'
var adminGroupName = 'exec-ledger-admins-${baseName}'

// ── Outputs ────────────────────────────────────────────────────────────────

output identifierUri string = identifierUri
output adminGroupName string = adminGroupName
output appDisplayName string = appDisplayName
