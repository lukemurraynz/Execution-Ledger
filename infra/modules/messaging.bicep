targetScope = 'resourceGroup'

@description('Azure region for all resources.')
param location string

@description('Common tags applied to all resources.')
param tags object = {}

@description('Service Bus namespace name.')
param serviceBusNamespaceName string

@description('Intent queue name.')
param serviceBusQueueName string

@description('Event Grid custom topic name for Drasi WAL change events.')
param eventGridTopicName string

module serviceBusNamespace 'br/public:avm/res/service-bus/namespace:0.17.0' = {
  name: 'service-bus-namespace-${serviceBusNamespaceName}'
  params: {
    name: serviceBusNamespaceName
    location: location
    tags: tags
    skuObject: {
      name: 'Standard'
    }
    zoneRedundant: false
    disableLocalAuth: true
    publicNetworkAccess: 'Enabled'
    queues: [
      {
        name: serviceBusQueueName
        enablePartitioning: true
        maxDeliveryCount: 10
        lockDuration: 'PT1M'
        defaultMessageTimeToLive: 'P14D'
        deadLetteringOnMessageExpiration: true
        // [OPERATIONS] No dead-letter queue depth alerting is configured here.
        // Poison messages (maxDeliveryCount exceeded or TTL expired) accumulate in
        // $deadletterqueue silently. Set up an Azure Monitor alert on
        // DeadletteredMessages metric (namespace: Microsoft.ServiceBus/Namespaces)
        // with a threshold appropriate for the workload. Recovery is manual via
        // /platform/recovery/servicebus-poison?limit=N or the CLI equivalent.
      }
    ]
  }
}

// ── Event Grid custom topic — secure alternative to raw webhook delivery ──
// Drasi WAL change events are posted here via managed identity instead of
// a raw webhook URL. Consumers subscribe to this topic rather than receiving
// direct webhook calls.

resource eventGridTopic 'Microsoft.EventGrid/topics@2025-02-15' = {
  name: eventGridTopicName
  location: location
  tags: tags
  properties: {
    inputSchema: 'CloudEventSchemaV1_0'
    publicNetworkAccess: 'Disabled'
    disableLocalAuth: true
  }
}

output serviceBusNamespaceName string = serviceBusNamespace.outputs.name
output serviceBusNamespaceHost string = '${serviceBusNamespace.outputs.name}.servicebus.windows.net'
output serviceBusQueueName string = serviceBusQueueName
output eventGridTopicEndpoint string = eventGridTopic.properties.endpoint
output eventGridTopicResourceId string = eventGridTopic.id
