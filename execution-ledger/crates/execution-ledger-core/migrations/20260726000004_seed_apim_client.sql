INSERT INTO EXECUTION_LEDGER_api_clients (client_id, display_name, status, scopes)
VALUES (
    'execution-ledger-apim',
    'Execution Ledger APIM gateway',
    'active',
    '["platform:read","platform:write","mcp:execute"]'::jsonb
)
ON CONFLICT (client_id) DO NOTHING;
