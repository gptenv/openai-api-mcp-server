# OpenAI API MCP Server

A Rust MCP server for the OpenAI REST API. It runs as a Cloudflare Worker and exposes the operations in `openapi.yaml` as MCP tools using the Streamable HTTP transport.

The current spec produces one tool for each of its 352 HTTP operations, plus webhook automation tools for its 21 webhook event types. The build trims the spec to request-side metadata, and the Worker derives tool schemas from that metadata. Updating `openapi.yaml` updates the tools on the next build and deployment.

## Deploy to Cloudflare Workers

Requirements: Rust with the `wasm32-unknown-unknown` target, and Node.js/npm for Wrangler.

```sh
rustup target add wasm32-unknown-unknown
npm install --save-dev wrangler
npx wrangler d1 create openai-api-mcp-server
npx wrangler queues create openai-webhook-events
npx wrangler queues create openai-webhook-events-dlq
# Copy the D1 database_id from the command output into wrangler.toml.
npx wrangler d1 migrations apply openai-api-mcp-server --remote
npx wrangler secret put OPENAI_API_KEY
npx wrangler secret put MCP_AUTH_TOKEN
npx wrangler deploy
```

`MIRROR_API_KEY` is the preferred upstream credential when using the configured mirror; `OPENAI_API_KEY` remains a fallback. `MCP_AUTH_TOKEN` protects the full MCP endpoint; clients must send it as `Authorization: Bearer <token>`. Keep all credential values in Worker secrets and do not put them in `wrangler.toml`.

For a private connection proof of concept, the Worker also exposes `/mcp-demo` without client authentication. It advertises only `demoStatus` and the read-only `mirrorCapabilities` operation. `demoStatus` confirms whether the Worker has the `MIRROR_API_KEY` secret without returning its value. `mirrorCapabilities` sends that secret as the Bearer credential and `X-Mirror-Session-Token` to the configured mirror's `/v1/capabilities` route. Set it with `printf '%s' "$MIRROR_API_KEY" | npx wrangler secret put MIRROR_API_KEY`; never put the value in a plugin manifest. The full `/mcp` endpoint remains protected by `MCP_AUTH_TOKEN`. The demo endpoint is public to anyone who knows its URL, so only these two operations are exposed there.

The unauthenticated `/mcp-public` endpoint advertises the full 352-operation OpenAI catalog without webhook tools and uses the same server-side mirror credential. It forwards every catalog operation to the configured Mirror backend. The backend's response determines whether a particular request succeeds. This public endpoint can be called by anyone and can access or run work through the configured ChatGPT account.

The shared bearer token is intended for development and MCP clients that can supply a fixed bearer credential. A published ChatGPT App connection requires OAuth 2.1 user authentication; ChatGPT does not accept a custom static API key for that flow. Put an OAuth-compatible identity provider or trusted gateway in front of this Worker, validate user tokens there, and have the gateway inject `MCP_AUTH_TOKEN`; alternatively, add provider-specific token validation here before publishing.

Set `OPENAI_API_BASE_URL` under `[vars]` in `wrangler.toml` when routing through a compatible API endpoint. The Worker sends `MIRROR_API_KEY` (or the `OPENAI_API_KEY` fallback) as both the Bearer credential and the `X-Mirror-Session-Token` header. Otherwise the server URL in `openapi.yaml` is used.

Set `PUBLIC_BASE_URL` in `[vars]` to the Worker origin, for example `https://openai-api-mcp-server.example.workers.dev`. After deploying, create one OpenAI project webhook endpoint at `https://<worker-origin>/webhooks/openai`, select the event types to receive, then save that endpoint's signing secret with `npx wrangler secret put OPENAI_WEBHOOK_SECRET`. The callback verifies OpenAI signatures and timestamps, records the full event, queues processing, and returns a small JSON acknowledgment.

The `webhook_subscribe` tool saves a model, instructions, and event type in D1. OpenAI must also be configured to deliver that event type to the callback URL. When an event arrives, Cloudflare Queues wakes the Rust consumer, which runs the saved instructions through the Responses API and stores the complete response object. The GPT does not stay awake between events; use `webhook_get_delivery` later to retrieve the full event payload, the exact acknowledgment returned to OpenAI, and each task's Responses API object. Delivery retries are deduplicated by webhook delivery ID and subscription.

The MCP endpoint is `/mcp`. `GET /` returns a health response and tool count. The MCP endpoint is stateless and responds to JSON-RPC POSTs with JSON; notifications receive HTTP 202. It handles initialization, ping, `tools/list`, and `tools/call`. Tool discovery returns the complete catalog in one response.

## Tool arguments

Path, query, and header parameters are exposed as top-level tool arguments. Properties from JSON request bodies are also exposed at the top level. Non-object request bodies use a `body` argument. Multipart file values use this object shape:

```json
{
  "filename": "data.jsonl",
  "content_type": "application/jsonl",
  "data_base64": "..."
}
```

Binary API responses are returned as base64 text; JSON, text, and event-stream responses are returned as text. API errors are returned as MCP tool results with `isError: true`.

## Local development

Run the Worker locally with Wrangler. Provide `MIRROR_API_KEY`, `MCP_AUTH_TOKEN`, and `OPENAI_WEBHOOK_SECRET` using local Worker secrets or a `.dev.vars` file (keep that file out of version control):

```sh
npx wrangler dev
```

The local MCP endpoint is typically `http://localhost:8787/mcp`.

## Compact plugin discovery

Hosts may impose a combined limit on tools across connected apps. The original
`/mcp` and `/mcp-public` endpoints continue to list the complete catalog.
Use `/mcp-compact` (same bearer authentication as `/mcp`) or
`/mcp-public-compact` (same public access as `/mcp-public`) for plugin connections
that need to stay below the host limit. Each compact endpoint advertises two tools:

- `discoverOperations`: returns all operation names and summaries, filters with
  `query`, or returns a complete input schema with `operation`.
- `callOperation`: accepts `operation` and an `arguments` object, and dispatches
  through the existing operation handler. Availability, upstream errors, and
  credentials have the same behavior as the corresponding original endpoint.

Discover an operation's schema before calling it. The authenticated compact
endpoint also includes webhook tools; the public compact endpoint excludes them.
A saved plugin connection update may require reconnecting or starting a new chat
before the host refreshes its callable tool registry.
