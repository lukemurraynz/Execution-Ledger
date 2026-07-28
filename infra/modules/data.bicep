targetScope = 'resourceGroup'

@description('Azure region for all resources.')
param location string

@description('Common tags applied to all resources.')
param tags object = {}

@description('PostgreSQL server name.')
param postgresServerName string

@description('PostgreSQL administrator login.')
param postgresAdminLogin string

@secure()
@description('PostgreSQL administrator password.')
param postgresAdminPassword string

@description('Execution Ledger runtime database name.')
param postgresDatabaseName string

@description('Resource ID of delegated PostgreSQL subnet.')
param postgresDelegatedSubnetId string

@description('Resource ID of PostgreSQL private DNS zone.')
param postgresPrivateDnsZoneId string

@description('Whether to provision dedicated runtime PostgreSQL roles via deployment script.')
param enableAppRoleProvisioning bool = true

var contributorRoleDefinitionId = subscriptionResourceId(
  'Microsoft.Authorization/roleDefinitions',
  'b24988ac-6180-42a0-ab88-20f7382dd24c'
)

module postgresServer 'br/public:avm/res/db-for-postgre-sql/flexible-server:0.14.0' = {
  name: 'postgresql-flexible-server-${postgresServerName}'
  params: {
    name: postgresServerName
    location: location
    tags: tags
    administratorLogin: postgresAdminLogin
    administratorLoginPassword: postgresAdminPassword
    skuName: 'Standard_B1ms'
    tier: 'Burstable'
    availabilityZone: -1
    version: '16'
    createMode: 'Create'
    authConfig: {
      activeDirectoryAuth: 'Enabled'
      // Runtime services authenticate to PostgreSQL via password-based connection strings.
      // Keep password auth enabled so api/worker/scheduler/drasi can connect.
      passwordAuth: 'Enabled'
    }
    storageSizeGB: 32
    backupRetentionDays: 7
    geoRedundantBackup: 'Disabled'
    highAvailability: 'Disabled'
    delegatedSubnetResourceId: postgresDelegatedSubnetId
    privateDnsZoneArmResourceId: postgresPrivateDnsZoneId
    publicNetworkAccess: 'Disabled'
    databases: [
      {
        name: postgresDatabaseName
        charset: 'UTF8'
        collation: 'en_US.utf8'
      }
    ]
    configurations: [
      {
        name: 'wal_level'
        source: 'user-override'
        value: 'logical'
      }
      {
        name: 'max_replication_slots'
        source: 'user-override'
        value: '10'
      }
      {
        name: 'max_wal_senders'
        source: 'user-override'
        value: '10'
      }
      {
        name: 'require_secure_transport'
        source: 'user-override'
        value: 'on'
      }
      {
        // Runtime schema migration creates pgcrypto for gen_random_uuid defaults.
        name: 'azure.extensions'
        source: 'user-override'
        value: 'pgcrypto'
      }
    ]
  }
}

resource postgresServerResource 'Microsoft.DBforPostgreSQL/flexibleServers@2024-08-01' existing = {
  name: postgresServerName
}

resource appRoleScriptIdentity 'Microsoft.ManagedIdentity/userAssignedIdentities@2023-01-31' = if (enableAppRoleProvisioning) {
  name: 'id-app-role-script-${postgresServerName}'
  location: location
}

resource appRoleScriptIdentityRoleAssignment 'Microsoft.Authorization/roleAssignments@2022-04-01' = if (enableAppRoleProvisioning) {
  name: guid(postgresServerResource.id, appRoleScriptIdentity.name, contributorRoleDefinitionId)
  scope: postgresServerResource
  properties: {
    roleDefinitionId: contributorRoleDefinitionId
    principalId: appRoleScriptIdentity!.properties.principalId!
    principalType: 'ServicePrincipal'
  }
}

// Execution Ledger runtime app role — least-privilege alternative to admin credential.
// Admin credential stays scoped to IaC operations only.
// Uses az postgres flexible-server execute which runs SQL against the server over its private endpoint.
resource appRoleScript 'Microsoft.Resources/deploymentScripts@2023-08-01' = if (enableAppRoleProvisioning) {
  name: 'create-app-role-${postgresServerName}'
  location: location
  kind: 'AzureCLI'
  identity: {
    type: 'UserAssigned'
    userAssignedIdentities: {
      '${appRoleScriptIdentity.id}': {}
    }
  }
  properties: {
    azCliVersion: '2.74.0'
    retentionInterval: 'PT1H'
    forceUpdateTag: 'v1'
    containerSettings: {
      containerGroupName: 'app-role-script-${postgresServerName}'
    }
    environmentVariables: [
      {
        name: 'PGHOST'
        value: postgresServer.outputs.fqdn!
      }
      {
        name: 'PGDATABASE'
        value: postgresDatabaseName
      }
      {
        name: 'PGUSER'
        value: postgresAdminLogin
      }
      {
        name: 'PGPASSWORD'
        // [SECURITY] PGPASSWORD passed as env var to deployment script.
        // The az postgres flexible-server execute command tunnels through ARM,
        // so password auth on the server can be Disabled while this still works.
        // However, the password value is visible in ARM deployment history to
        // anyone with Microsoft.Resources/deployments/read. For government:
        // consider migrating this script to a managed identity approach
        // (az postgres flexible-server execute with --identity instead of
        // --admin-password) when the az CLI version supports it.
        secureValue: postgresAdminPassword
      }
      {
        name: 'APP_PASSWORD'
        value: guid(postgresServerName)
      }
    ]
    scriptContent: '''
# Create least-privilege roles for runtime and replication.
# az postgres flexible-server execute authenticates via the admin credentials passed as env vars.
az extension add --name rdbms-connect --upgrade --only-show-errors > /dev/null

az postgres flexible-server execute \
  --name "$PGHOST" \
  --admin-user "$PGUSER" \
  --admin-password "$PGPASSWORD" \
  --database "$PGDATABASE" \
  --sql-content "
    -- Remove default public schema privileges that allow arbitrary table creation by PUBLIC
    REVOKE CREATE ON SCHEMA public FROM PUBLIC;

    -- App role: runtime credential for api/worker/scheduler services.
    -- Idempotent for repeat deployments.
    DO \$\$
    BEGIN
      IF NOT EXISTS (SELECT 1 FROM pg_roles WHERE rolname = 'execution_ledger_app') THEN
        CREATE ROLE execution_ledger_app LOGIN PASSWORD '$APP_PASSWORD';
      ELSE
        ALTER ROLE execution_ledger_app WITH LOGIN PASSWORD '$APP_PASSWORD';
      END IF;
    END
    \$\$;

    GRANT CONNECT ON DATABASE $PGDATABASE TO execution_ledger_app;
    GRANT USAGE, SELECT, INSERT, UPDATE, DELETE ON SCHEMA public TO execution_ledger_app;
    GRANT USAGE, SELECT ON ALL SEQUENCES IN SCHEMA public TO execution_ledger_app;

    -- Drasi replication role: narrower scope than app role (no mutations, no DDL).
    -- Idempotent for repeat deployments.
    DO \$\$
    BEGIN
      IF NOT EXISTS (SELECT 1 FROM pg_roles WHERE rolname = 'execution_ledger_drasi') THEN
        CREATE ROLE execution_ledger_drasi LOGIN PASSWORD '$APP_PASSWORD';
      ELSE
        ALTER ROLE execution_ledger_drasi WITH LOGIN PASSWORD '$APP_PASSWORD';
      END IF;
    END
    \$\$;

    GRANT CONNECT ON DATABASE $PGDATABASE TO execution_ledger_drasi;
    GRANT USAGE, SELECT ON SCHEMA public TO execution_ledger_drasi;
    GRANT SELECT ON ALL TABLES IN SCHEMA public TO execution_ledger_drasi;
    GRANT REPLICATION ON DATABASE $PGDATABASE TO execution_ledger_drasi;

    -- Revoke dangerous privileges that azure_pg_admin grants by default
    REVOKE CREATEROLE FROM execution_ledger_app;
    REVOKE BYPASSRLS FROM execution_ledger_app;
    REVOKE CREATEROLE FROM execution_ledger_drasi;
    REVOKE BYPASSRLS FROM execution_ledger_drasi;
  "
'''
    supportingScriptUris: []
    timeout: 'PT5M'
  }
  dependsOn: [
    appRoleScriptIdentityRoleAssignment
  ]
}

output postgresServerName string = postgresServer.outputs.name
output postgresDatabaseName string = postgresDatabaseName
output postgresServerFqdn string = postgresServer.outputs.fqdn!
output appUser string = 'execution_ledger_app'
#disable-next-line outputs-should-not-contain-secrets // value is secret name only, not the secret itself
output appUserPasswordSecretName string = 'execution-ledger-app-password'
