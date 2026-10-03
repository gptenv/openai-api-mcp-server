CREATE TABLE IF NOT EXISTS webhook_subscriptions (
    id TEXT PRIMARY KEY,
    event_type TEXT NOT NULL,
    name TEXT NOT NULL,
    instructions TEXT NOT NULL,
    model TEXT NOT NULL,
    enabled INTEGER NOT NULL DEFAULT 1,
    created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))
);

CREATE INDEX IF NOT EXISTS webhook_subscriptions_event_type_enabled
    ON webhook_subscriptions(event_type, enabled);

CREATE TABLE IF NOT EXISTS webhook_deliveries (
    delivery_id TEXT PRIMARY KEY,
    event_id TEXT,
    event_type TEXT NOT NULL,
    payload_json TEXT NOT NULL,
    acknowledgment_json TEXT NOT NULL,
    received_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))
);

CREATE INDEX IF NOT EXISTS webhook_deliveries_received_at
    ON webhook_deliveries(received_at DESC);

CREATE TABLE IF NOT EXISTS webhook_automation_runs (
    id TEXT PRIMARY KEY,
    delivery_id TEXT NOT NULL REFERENCES webhook_deliveries(delivery_id),
    subscription_id TEXT NOT NULL REFERENCES webhook_subscriptions(id),
    status TEXT NOT NULL,
    response_json TEXT,
    error TEXT,
    updated_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    UNIQUE(delivery_id, subscription_id)
);

CREATE INDEX IF NOT EXISTS webhook_automation_runs_delivery
    ON webhook_automation_runs(delivery_id);
