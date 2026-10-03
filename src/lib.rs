use base64::{Engine, engine::general_purpose::STANDARD as BASE64};
use hmac::{Hmac, Mac};
use js_sys::Uint8Array;
use serde::{Deserialize, Serialize};
use serde_yaml::Value as Yaml;
use sha2::Sha256;
use std::sync::OnceLock;
use uuid::Uuid;
use worker::{Fetch, Headers, Method, Request, RequestInit, Response, Result, Url, event};
use worker::{MessageBatch, d1::D1Database};

const OPENAPI: &str = include_str!(concat!(env!("OUT_DIR"), "/openapi.json"));
const MCP_VERSION: &str = "2025-11-25";
const MCP_PATH: &str = "/mcp";
const MCP_DEMO_PATH: &str = "/mcp-demo";
const MCP_PUBLIC_PATH: &str = "/mcp-public";
const DEMO_TOOL_NAME: &str = "mirrorCapabilities";
const DEMO_STATUS_TOOL_NAME: &str = "demoStatus";
const DEMO_MODELS_TOOL_NAME: &str = "listModels";
const WEBHOOK_PATH: &str = "/webhooks/openai";

#[derive(Clone)]
struct Operation {
    path: String,
    method: String,
    name: String,
    summary: String,
    description: String,
    parameters: Vec<Yaml>,
    body_schema: Option<Yaml>,
    body_media_type: Option<String>,
    body_required: bool,
    deprecated: bool,
}

static OPERATIONS: OnceLock<std::result::Result<Vec<Operation>, String>> = OnceLock::new();
static SPEC: OnceLock<std::result::Result<Yaml, String>> = OnceLock::new();
static TOOL_LIST: OnceLock<std::result::Result<Vec<serde_json::Value>, String>> = OnceLock::new();

fn spec() -> Result<&'static Yaml> {
    match SPEC.get_or_init(|| {
        serde_yaml::from_str(OPENAPI).map_err(|e| format!("OpenAPI parse error: {e}"))
    }) {
        Ok(spec) => Ok(spec),
        Err(message) => Err(message.clone().into()),
    }
}

fn yaml_get<'a>(value: &'a Yaml, key: &str) -> Option<&'a Yaml> {
    value.as_mapping()?.get(Yaml::String(key.to_string()))
}

fn yaml_string(value: Option<&Yaml>) -> Option<String> {
    value.and_then(Yaml::as_str).map(ToOwned::to_owned)
}

fn load_operations() -> std::result::Result<Vec<Operation>, String> {
    let spec = match SPEC.get_or_init(|| {
        serde_yaml::from_str(OPENAPI).map_err(|e| format!("OpenAPI parse error: {e}"))
    }) {
        Ok(spec) => spec,
        Err(message) => return Err(message.clone()),
    };
    let paths = yaml_get(spec, "paths")
        .and_then(Yaml::as_mapping)
        .ok_or_else(|| "OpenAPI document has no paths object".to_string())?;
    let method_names = [
        "get", "post", "put", "patch", "delete", "head", "options", "trace",
    ];
    let mut operations = Vec::new();

    for (path, path_item) in paths {
        let Some(path) = path.as_str() else { continue };
        let inherited_parameters = yaml_get(path_item, "parameters")
            .and_then(Yaml::as_sequence)
            .cloned()
            .unwrap_or_default();
        let Some(path_mapping) = path_item.as_mapping() else {
            continue;
        };

        for method in method_names {
            let Some(operation) = path_mapping.get(Yaml::String(method.to_string())) else {
                continue;
            };
            let Some(operation_id) = yaml_string(yaml_get(operation, "operationId")) else {
                continue;
            };
            let mut parameters = inherited_parameters.clone();
            parameters.extend(
                yaml_get(operation, "parameters")
                    .and_then(Yaml::as_sequence)
                    .cloned()
                    .unwrap_or_default(),
            );

            let request_body = yaml_get(operation, "requestBody");
            let content = request_body
                .and_then(|v| yaml_get(v, "content"))
                .and_then(Yaml::as_mapping);
            let (body_media_type, body_schema) = content
                .and_then(|content| {
                    let preferred = [
                        "application/json",
                        "multipart/form-data",
                        "application/sdp",
                        "application/x-www-form-urlencoded",
                    ];
                    preferred
                        .iter()
                        .find_map(|kind| {
                            content
                                .get(Yaml::String((*kind).to_string()))
                                .map(|v| ((*kind).to_string(), yaml_get(v, "schema").cloned()))
                        })
                        .or_else(|| {
                            content.iter().find_map(|(kind, v)| {
                                Some((kind.as_str()?.to_string(), yaml_get(v, "schema").cloned()))
                            })
                        })
                })
                .map(|(media, schema)| (Some(media), schema))
                .unwrap_or((None, None));

            operations.push(Operation {
                path: path.to_string(),
                method: method.to_uppercase(),
                name: operation_id,
                summary: yaml_string(yaml_get(operation, "summary")).unwrap_or_default(),
                description: yaml_string(yaml_get(operation, "description")).unwrap_or_default(),
                parameters,
                body_schema,
                body_media_type,
                body_required: request_body
                    .and_then(|v| yaml_get(v, "required"))
                    .and_then(Yaml::as_bool)
                    .unwrap_or(false),
                deprecated: yaml_get(operation, "deprecated")
                    .and_then(Yaml::as_bool)
                    .unwrap_or(false),
            });
        }
    }
    operations.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(operations)
}

fn operations() -> Result<&'static Vec<Operation>> {
    match OPERATIONS.get_or_init(load_operations) {
        Ok(operations) => Ok(operations),
        Err(message) => Err(message.clone().into()),
    }
}

fn resolve_ref<'a>(spec: &'a Yaml, reference: &str) -> Option<&'a Yaml> {
    let mut node = spec;
    for part in reference.strip_prefix("#/")?.split('/') {
        let part = part.replace("~1", "/").replace("~0", "~");
        node = node.as_mapping()?.get(Yaml::String(part))?;
    }
    Some(node)
}

fn dereference<'a>(spec: &'a Yaml, value: &'a Yaml, depth: usize) -> &'a Yaml {
    if depth > 24 {
        return value;
    }
    yaml_string(yaml_get(value, "$ref"))
        .and_then(|reference| resolve_ref(spec, &reference))
        .map(|resolved| dereference(spec, resolved, depth + 1))
        .unwrap_or(value)
}

fn json_schema(spec: &Yaml, input: &Yaml, depth: usize) -> serde_json::Value {
    if depth > 24 {
        return serde_json::json!({});
    }
    let schema = dereference(spec, input, 0);

    if let Some(parts) = yaml_get(schema, "allOf").and_then(Yaml::as_sequence) {
        let mut merged = serde_json::Map::new();
        let mut properties = serde_json::Map::new();
        let mut required = Vec::new();
        for part in parts {
            if let Some(obj) = json_schema(spec, part, depth + 1).as_object() {
                for (key, value) in obj {
                    if key == "properties" {
                        if let Some(items) = value.as_object() {
                            properties.extend(items.clone());
                        }
                    } else if key == "required" {
                        if let Some(items) = value.as_array() {
                            required.extend(items.iter().cloned());
                        }
                    } else {
                        merged.insert(key.clone(), value.clone());
                    }
                }
            }
        }
        if !properties.is_empty() {
            merged.insert("properties".into(), properties.into());
        }
        if !required.is_empty() {
            merged.insert("required".into(), required.into());
        }
        return merged.into();
    }

    let mut result = serde_json::to_value(schema).unwrap_or_else(|_| serde_json::json!({}));
    let Some(object) = result.as_object_mut() else {
        return result;
    };
    object.remove("$ref");
    if yaml_string(yaml_get(schema, "type")).as_deref() == Some("string")
        && yaml_string(yaml_get(schema, "format")).as_deref() == Some("binary")
    {
        return serde_json::json!({
            "type": "object",
            "description": yaml_string(yaml_get(schema, "description")).unwrap_or_else(|| "Binary file data, base64 encoded.".to_string()),
            "properties": {
                "filename": {"type":"string", "description":"File name sent to the API."},
                "content_type": {"type":"string", "description":"Optional MIME type."},
                "data_base64": {"type":"string", "description":"File contents encoded as standard base64."}
            },
            "required": ["data_base64"]
        });
    }
    for key in ["properties", "items", "additionalProperties", "not"] {
        if let Some(value) = yaml_get(schema, key) {
            if key == "properties" {
                let mut props = serde_json::Map::new();
                if let Some(mapping) = value.as_mapping() {
                    for (name, prop_schema) in mapping {
                        if let Some(name) = name.as_str() {
                            props.insert(
                                name.to_string(),
                                json_schema(spec, prop_schema, depth + 1),
                            );
                        }
                    }
                }
                object.insert(key.to_string(), props.into());
            } else if key == "items" || key == "not" {
                object.insert(key.to_string(), json_schema(spec, value, depth + 1));
            }
        }
    }
    for key in ["oneOf", "anyOf"] {
        if let Some(items) = yaml_get(schema, key).and_then(Yaml::as_sequence) {
            object.insert(
                key.to_string(),
                items
                    .iter()
                    .map(|v| json_schema(spec, v, depth + 1))
                    .collect(),
            );
        }
    }
    result
}

fn operation_schema(spec: &Yaml, operation: &Operation) -> serde_json::Value {
    let mut properties = serde_json::Map::new();
    let mut required = Vec::new();
    for parameter in &operation.parameters {
        let parameter = dereference(spec, parameter, 0);
        let Some(name) = yaml_string(yaml_get(parameter, "name")) else {
            continue;
        };
        let Some(location) = yaml_string(yaml_get(parameter, "in")) else {
            continue;
        };
        let mut schema = yaml_get(parameter, "schema")
            .map(|v| json_schema(spec, v, 0))
            .unwrap_or_else(|| serde_json::json!({}));
        if let Some(description) = yaml_string(yaml_get(parameter, "description")) {
            if let Some(obj) = schema.as_object_mut() {
                obj.entry("description").or_insert(description.into());
            }
        }
        if location == "path"
            || yaml_get(parameter, "required")
                .and_then(Yaml::as_bool)
                .unwrap_or(false)
        {
            required.push(serde_json::Value::String(name.clone()));
        }
        properties.insert(name, schema);
    }

    if let Some(body_schema) = &operation.body_schema {
        let body_schema = dereference(spec, body_schema, 0);
        let schema = json_schema(spec, body_schema, 0);
        if yaml_string(yaml_get(body_schema, "type")).as_deref() == Some("object")
            || schema.get("properties").is_some()
        {
            if let Some(body_props) = schema
                .get("properties")
                .and_then(serde_json::Value::as_object)
            {
                for (name, schema) in body_props {
                    properties.insert(name.clone(), schema.clone());
                }
            }
            if operation.body_required {
                if let Some(body_required) =
                    schema.get("required").and_then(serde_json::Value::as_array)
                {
                    required.extend(body_required.iter().cloned());
                }
            }
        } else {
            properties.insert("body".into(), schema);
            if operation.body_required {
                required.push(serde_json::Value::String("body".into()));
            }
        }
    }

    required.sort_by_key(|v| v.as_str().unwrap_or_default().to_string());
    required.dedup();
    let mut schema = serde_json::json!({"type":"object", "properties": properties});
    if !required.is_empty() {
        schema["required"] = required.into();
    }
    schema
}

fn tool_list() -> Result<Vec<serde_json::Value>> {
    match TOOL_LIST.get_or_init(|| {
        let spec = match spec() {
            Ok(spec) => spec,
            Err(error) => return Err(error.to_string()),
        };
        let operations = match operations() {
            Ok(operations) => operations,
            Err(error) => return Err(error.to_string()),
        };
        Ok(operations
            .iter()
            .map(|operation| {
                let mut description = if !operation.summary.is_empty() {
                    operation.summary.clone()
                } else {
                    format!("{} {}", operation.method, operation.path)
                };
                if !operation.description.is_empty() {
                    description.push_str(". ");
                    description.push_str(&operation.description);
                }
                if operation.deprecated {
                    description.push_str(" (deprecated API operation)");
                }
                serde_json::json!({
                    "name": operation.name,
                    "title": if operation.summary.is_empty() { operation.name.clone() } else { operation.summary.clone() },
                    "description": description,
                    "annotations": {
                        "readOnlyHint": operation.method == "GET",
                        "destructiveHint": operation.method == "DELETE",
                        "openWorldHint": true
                    },
                    "inputSchema": operation_schema(spec, operation)
                })
            })
            .collect())
    }) {
        Ok(tools) => Ok(tools.clone()),
        Err(message) => Err(message.clone().into()),
    }
}

fn demo_tool_list() -> Result<Vec<serde_json::Value>> {
    let mut tools = vec![serde_json::json!({
        "name": DEMO_TOOL_NAME,
        "title": "Mirror capabilities",
        "description": "Read the mirror's capability metadata using the credential stored by the Worker.",
        "annotations": {"readOnlyHint": true, "destructiveHint": false, "openWorldHint": true},
        "inputSchema": {"type":"object","properties":{}}
    })];
    tools.push(serde_json::json!({
        "name": DEMO_STATUS_TOOL_NAME,
        "title": "Demo status",
        "description": "Check that this MCP server is running and its mirror credential is configured.",
        "annotations": {"readOnlyHint": true, "destructiveHint": false, "openWorldHint": false},
        "inputSchema": {"type":"object","properties":{}}
    }));
    if let Some(model_tool) = tool_list()?
        .into_iter()
        .find(|tool| tool["name"] == DEMO_MODELS_TOOL_NAME)
    {
        tools.push(model_tool);
    }
    Ok(tools)
}

fn public_tool_list() -> Result<Vec<serde_json::Value>> {
    let mut tools = tool_list()?;
    for tool in &mut tools {
        tool["inputSchema"] = serde_json::json!({"type":"object","additionalProperties":true});
    }
    Ok(tools)
}

#[cfg(test)]
mod demo_tests {
    use super::*;

    #[test]
    fn public_demo_exposes_only_read_only_tools() {
        let tools = demo_tool_list().unwrap();
        let names: Vec<_> = tools
            .iter()
            .filter_map(|tool| tool["name"].as_str())
            .collect();
        assert_eq!(
            names,
            [DEMO_TOOL_NAME, DEMO_STATUS_TOOL_NAME, DEMO_MODELS_TOOL_NAME]
        );
        assert!(
            tools
                .iter()
                .all(|tool| tool["annotations"]["readOnlyHint"] == true)
        );
    }

    #[test]
    fn public_catalog_lists_all_operations_together() {
        let tools = public_tool_list().unwrap();
        assert_eq!(tools.len(), 352);
        assert!(tools.iter().all(|tool| {
            !tool["description"]
                .as_str()
                .unwrap_or_default()
                .contains("Unavailable through Mirror")
        }));
    }
}

fn webhook_event_types() -> Result<Vec<(String, String, String)>> {
    let spec = spec()?;
    let Some(webhooks) = yaml_get(spec, "webhooks").and_then(Yaml::as_mapping) else {
        return Ok(Vec::new());
    };
    let mut events = Vec::new();
    for (name, webhook) in webhooks {
        let Some(name) = name.as_str() else { continue };
        let Some(post) = yaml_get(webhook, "post") else {
            continue;
        };
        let description = yaml_get(post, "requestBody")
            .and_then(|body| yaml_string(yaml_get(body, "description")))
            .unwrap_or_else(|| format!("OpenAI {name} webhook event"));
        let event_type = yaml_get(post, "requestBody")
            .and_then(|body| yaml_get(body, "content"))
            .and_then(|content| yaml_get(content, "application/json"))
            .and_then(|content| yaml_get(content, "schema"))
            .map(|schema| dereference(spec, schema, 0))
            .and_then(|schema| yaml_get(schema, "properties"))
            .and_then(|props| yaml_get(props, "type"))
            .and_then(|type_schema| yaml_get(type_schema, "enum"))
            .and_then(Yaml::as_sequence)
            .and_then(|values| values.first())
            .and_then(Yaml::as_str)
            .unwrap_or(name)
            .to_string();
        events.push((event_type, name.to_string(), description));
    }
    events.sort_by(|a, b| a.0.cmp(&b.0));
    Ok(events)
}

fn webhook_tools() -> Result<Vec<serde_json::Value>> {
    let event_types: Vec<serde_json::Value> = webhook_event_types()?
        .into_iter()
        .map(|(event_type, _, _)| serde_json::Value::String(event_type))
        .collect();
    Ok(vec![
        serde_json::json!({
            "name":"webhook_listener_info",
            "title":"Webhook listener setup",
            "description":"Show the callback URL and setup requirements for receiving OpenAI webhook events.",
            "inputSchema":{"type":"object","properties":{}},
            "annotations":{"readOnlyHint":true,"openWorldHint":false}
        }),
        serde_json::json!({
            "name":"webhook_subscribe",
            "title":"Subscribe to webhook event",
            "description":"Save an automated Responses API task to run when the selected OpenAI webhook event arrives. The callback URL must also be subscribed to this event in the OpenAI project webhook settings.",
            "inputSchema":{
                "type":"object",
                "properties":{
                    "event_type":{"type":"string","enum":event_types,"description":"OpenAI event type to listen for."},
                    "name":{"type":"string","description":"A label for this automation."},
                    "instructions":{"type":"string","description":"Instructions for the model to run when this event arrives."},
                    "model":{"type":"string","description":"Responses API model to use for each event."}
                },
                "required":["event_type","name","instructions","model"]
            },
            "annotations":{"readOnlyHint":false,"destructiveHint":false,"openWorldHint":true}
        }),
        serde_json::json!({
            "name":"webhook_unsubscribe",
            "title":"Unsubscribe webhook task",
            "description":"Disable one saved automation task. Other tasks subscribed to the same event remain active.",
            "inputSchema":{"type":"object","properties":{"subscription_id":{"type":"string"}},"required":["subscription_id"]},
            "annotations":{"readOnlyHint":false,"destructiveHint":true,"openWorldHint":false}
        }),
        serde_json::json!({
            "name":"webhook_list_subscriptions",
            "title":"List webhook tasks",
            "description":"List enabled and disabled event-triggered model tasks.",
            "inputSchema":{"type":"object","properties":{}},
            "annotations":{"readOnlyHint":true,"openWorldHint":false}
        }),
        serde_json::json!({
            "name":"webhook_list_deliveries",
            "title":"List webhook deliveries",
            "description":"List recent verified webhook events and the acknowledgment objects returned to OpenAI.",
            "inputSchema":{"type":"object","properties":{"limit":{"type":"integer","minimum":1,"maximum":100,"default":20}}},
            "annotations":{"readOnlyHint":true,"openWorldHint":false}
        }),
        serde_json::json!({
            "name":"webhook_get_delivery",
            "title":"Get webhook delivery",
            "description":"Return the complete verified event payload, the HTTP acknowledgment object returned to OpenAI, and any full Responses API objects generated by matching tasks.",
            "inputSchema":{"type":"object","properties":{"delivery_id":{"type":"string"}},"required":["delivery_id"]},
            "annotations":{"readOnlyHint":true,"openWorldHint":false}
        }),
        serde_json::json!({
            "name":"webhook_retry_delivery",
            "title":"Retry webhook tasks",
            "description":"Enqueue a previously received event again for any enabled subscriptions that have not completed. Completed tasks remain deduplicated.",
            "inputSchema":{"type":"object","properties":{"delivery_id":{"type":"string"}},"required":["delivery_id"]},
            "annotations":{"readOnlyHint":false,"destructiveHint":false,"openWorldHint":true}
        }),
    ])
}

#[derive(Deserialize)]
struct SubscriptionRow {
    id: String,
    event_type: String,
    name: String,
    instructions: String,
    model: String,
    enabled: i64,
    created_at: String,
}

#[derive(Deserialize)]
struct DeliveryRow {
    delivery_id: String,
    event_id: Option<String>,
    event_type: String,
    payload_json: String,
    acknowledgment_json: String,
    received_at: String,
}

#[derive(Deserialize)]
struct AutomationRunRow {
    id: String,
    subscription_id: String,
    status: String,
    response_json: Option<String>,
    error: Option<String>,
    updated_at: String,
}

fn arg_string<'a>(args: &'a serde_json::Value, name: &str) -> Result<&'a str> {
    args.get(name)
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| format!("missing or invalid {name} argument").into())
}

async fn d1_run(db: &D1Database, sql: &str, args: &[wasm_bindgen::JsValue]) -> Result<()> {
    db.prepare(sql).bind(args)?.run().await?;
    Ok(())
}

fn result_text(value: serde_json::Value) -> serde_json::Value {
    let text = serde_json::to_string_pretty(&value).unwrap_or_else(|_| value.to_string());
    serde_json::json!({"content":[{"type":"text","text":text}]})
}

async fn call_webhook_tool(
    name: &str,
    args: &serde_json::Value,
    env: &worker::Env,
) -> Result<serde_json::Value> {
    let db = env.d1("DB")?;
    match name {
        "webhook_listener_info" => {
            let base_url = env
                .var("PUBLIC_BASE_URL")
                .map(|value| value.to_string().trim_end_matches('/').to_string())
                .unwrap_or_else(|_| "<your-worker-origin>".to_string());
            Ok(result_text(serde_json::json!({
                "callback_url":format!("{base_url}{WEBHOOK_PATH}"),
                "required_worker_secret":"OPENAI_WEBHOOK_SECRET",
                "supported_event_types":webhook_event_types()?.into_iter().map(|(event_type, name, _)|serde_json::json!({"event_type":event_type,"spec_name":name})).collect::<Vec<_>>(),
                "setup":"Create one OpenAI project webhook endpoint at callback_url and select the event types you plan to handle. Store its signing secret as OPENAI_WEBHOOK_SECRET in this Worker. Then call webhook_subscribe to define the task for each event."
            })))
        }
        "webhook_subscribe" => {
            let event_type = arg_string(args, "event_type")?;
            if !webhook_event_types()?
                .iter()
                .any(|(kind, _, _)| kind == event_type)
            {
                return Err(format!("unsupported webhook event type: {event_type}").into());
            }
            let name = arg_string(args, "name")?;
            let instructions = arg_string(args, "instructions")?;
            let model = arg_string(args, "model")?;
            if instructions.len() > 16_000 {
                return Err("instructions must be no longer than 16000 bytes".into());
            }
            let id = Uuid::new_v4().to_string();
            d1_run(
                &db,
                "INSERT INTO webhook_subscriptions (id,event_type,name,instructions,model) VALUES (?,?,?,?,?)",
                &[
                    id.clone().into(),
                    event_type.to_string().into(),
                    name.to_string().into(),
                    instructions.to_string().into(),
                    model.to_string().into(),
                ],
            )
            .await?;
            Ok(result_text(
                serde_json::json!({"subscription_id":id,"event_type":event_type,"name":name,"model":model,"enabled":true}),
            ))
        }
        "webhook_unsubscribe" => {
            let id = arg_string(args, "subscription_id")?;
            d1_run(
                &db,
                "UPDATE webhook_subscriptions SET enabled=0 WHERE id=?",
                &[id.into()],
            )
            .await?;
            Ok(result_text(
                serde_json::json!({"subscription_id":id,"enabled":false}),
            ))
        }
        "webhook_list_subscriptions" => {
            let rows: Vec<SubscriptionRow> = db
                .prepare("SELECT id,event_type,name,instructions,model,enabled,created_at FROM webhook_subscriptions ORDER BY created_at DESC")
                .all()
                .await?
                .results()?;
            Ok(result_text(serde_json::to_value(rows_to_json(rows))?))
        }
        "webhook_list_deliveries" => {
            let limit = args
                .get("limit")
                .and_then(serde_json::Value::as_u64)
                .unwrap_or(20)
                .clamp(1, 100) as f64;
            let rows: Vec<DeliveryRow> = db
                .prepare("SELECT delivery_id,event_id,event_type,payload_json,acknowledgment_json,received_at FROM webhook_deliveries ORDER BY received_at DESC LIMIT ?")
                .bind(&[wasm_bindgen::JsValue::from_f64(limit)])?
                .all()
                .await?
                .results()?;
            Ok(result_text(serde_json::Value::Array(
                rows.into_iter().map(delivery_to_json).collect(),
            )))
        }
        "webhook_get_delivery" => {
            let id = arg_string(args, "delivery_id")?;
            let row: Option<DeliveryRow> = db
                .prepare("SELECT delivery_id,event_id,event_type,payload_json,acknowledgment_json,received_at FROM webhook_deliveries WHERE delivery_id=?")
                .bind(&[id.into()])?
                .first(None)
                .await?;
            let Some(row) = row else {
                return Err(format!("unknown webhook delivery: {id}").into());
            };
            let runs: Vec<AutomationRunRow> = db
                .prepare("SELECT id,subscription_id,status,response_json,error,updated_at FROM webhook_automation_runs WHERE delivery_id=? ORDER BY updated_at")
                .bind(&[id.into()])?
                .all()
                .await?
                .results()?;
            let mut value = delivery_to_json(row);
            value["automation_runs"] =
                serde_json::Value::Array(runs.into_iter().map(run_to_json).collect());
            Ok(result_text(value))
        }
        "webhook_retry_delivery" => {
            let id = arg_string(args, "delivery_id")?;
            let exists: Option<String> = db
                .prepare("SELECT delivery_id FROM webhook_deliveries WHERE delivery_id=?")
                .bind(&[id.into()])?
                .first(Some("delivery_id"))
                .await?;
            if exists.is_none() {
                return Err(format!("unknown webhook delivery: {id}").into());
            }
            env.queue("WEBHOOK_QUEUE")?
                .send(WebhookQueueMessage {
                    delivery_id: id.to_string(),
                })
                .await?;
            Ok(result_text(
                serde_json::json!({"delivery_id":id,"queued":true}),
            ))
        }
        _ => Err(format!("unknown webhook tool: {name}").into()),
    }
}

fn rows_to_json(rows: Vec<SubscriptionRow>) -> Vec<serde_json::Value> {
    rows.into_iter().map(|row| serde_json::json!({
        "id":row.id,"event_type":row.event_type,"name":row.name,"instructions":row.instructions,
        "model":row.model,"enabled":row.enabled != 0,"created_at":row.created_at
    })).collect()
}

fn delivery_to_json(row: DeliveryRow) -> serde_json::Value {
    serde_json::json!({
        "delivery_id":row.delivery_id,
        "event_id":row.event_id,
        "event_type":row.event_type,
        "payload":serde_json::from_str::<serde_json::Value>(&row.payload_json).unwrap_or(serde_json::Value::String(row.payload_json)),
        "acknowledgment":serde_json::from_str::<serde_json::Value>(&row.acknowledgment_json).unwrap_or(serde_json::Value::String(row.acknowledgment_json)),
        "received_at":row.received_at
    })
}

fn run_to_json(row: AutomationRunRow) -> serde_json::Value {
    serde_json::json!({
        "id":row.id,
        "subscription_id":row.subscription_id,
        "status":row.status,
        "response":row.response_json.and_then(|value|serde_json::from_str::<serde_json::Value>(&value).ok()),
        "error":row.error,
        "updated_at":row.updated_at
    })
}

#[derive(Serialize, Deserialize)]
pub struct WebhookQueueMessage {
    delivery_id: String,
}

#[derive(Deserialize)]
struct WebhookDeliveryQueueRow {
    event_type: String,
    payload_json: String,
}

#[derive(Deserialize)]
struct AutomationStatusRow {
    status: String,
}

fn verify_webhook_signature(
    secret: &str,
    delivery_id: &str,
    timestamp: &str,
    signature_header: &str,
    payload: &str,
) -> bool {
    let Ok(timestamp_seconds) = timestamp.parse::<i64>() else {
        return false;
    };
    let now_seconds = (js_sys::Date::now() / 1000.0) as i64;
    if (now_seconds - timestamp_seconds).abs() > 300 {
        return false;
    }
    let encoded_secret = secret.strip_prefix("whsec_").unwrap_or(secret);
    let Ok(key) = BASE64.decode(encoded_secret) else {
        return false;
    };
    let signed_payload = format!("{delivery_id}.{timestamp}.{payload}");
    for signature in signature_header.split_ascii_whitespace() {
        let Some((version, encoded_signature)) = signature.split_once(',') else {
            continue;
        };
        if version != "v1" {
            continue;
        }
        let Ok(signature_bytes) = BASE64.decode(encoded_signature) else {
            continue;
        };
        let Ok(mut mac) = Hmac::<Sha256>::new_from_slice(&key) else {
            return false;
        };
        mac.update(signed_payload.as_bytes());
        if mac.verify_slice(&signature_bytes).is_ok() {
            return true;
        }
    }
    false
}

async fn receive_webhook(mut request: Request, env: &worker::Env) -> Result<Response> {
    if request.method() != Method::Post {
        return Ok(Response::ok("Webhook endpoint accepts POST requests")?.with_status(405));
    }
    let secret = match env.secret("OPENAI_WEBHOOK_SECRET") {
        Ok(secret) => secret.to_string(),
        Err(_) => {
            return Ok(Response::ok("Webhook signing secret is not configured")?.with_status(503));
        }
    };
    let headers = request.headers();
    let delivery_id = headers.get("webhook-id")?.unwrap_or_default();
    let timestamp = headers.get("webhook-timestamp")?.unwrap_or_default();
    let signature = headers.get("webhook-signature")?.unwrap_or_default();
    let payload = request.text().await?;
    if delivery_id.is_empty()
        || !verify_webhook_signature(&secret, &delivery_id, &timestamp, &signature, &payload)
    {
        return Ok(Response::ok("Invalid webhook signature")?.with_status(400));
    }
    let event: serde_json::Value = match serde_json::from_str(&payload) {
        Ok(event) => event,
        Err(_) => return Ok(Response::ok("Invalid JSON payload")?.with_status(400)),
    };
    let event_id = event
        .get("id")
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default();
    let event_type = event
        .get("type")
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default();
    if event_id.is_empty() || event_type.is_empty() {
        return Ok(Response::ok("Webhook event requires id and type fields")?.with_status(400));
    }

    let acknowledgment = serde_json::json!({
        "received":true,
        "delivery_id":delivery_id,
        "event_id":event_id,
        "event_type":event_type
    });
    let db = env.d1("DB")?;
    d1_run(
        &db,
        "INSERT INTO webhook_deliveries (delivery_id,event_id,event_type,payload_json,acknowledgment_json) VALUES (?,?,?,?,?) ON CONFLICT(delivery_id) DO NOTHING",
        &[
            delivery_id.clone().into(),
            event_id.into(),
            event_type.into(),
            payload.into(),
            serde_json::to_string(&acknowledgment)?.into(),
        ],
    )
    .await?;
    env.queue("WEBHOOK_QUEUE")?
        .send(WebhookQueueMessage {
            delivery_id: delivery_id.clone(),
        })
        .await?;
    Response::from_json(&acknowledgment)
}

async fn execute_automation(
    db: &D1Database,
    env: &worker::Env,
    delivery_id: &str,
    event_type: &str,
    payload: &str,
    subscription: &SubscriptionRow,
) -> Result<()> {
    let prior: Option<AutomationStatusRow> = db
        .prepare(
            "SELECT status FROM webhook_automation_runs WHERE delivery_id=? AND subscription_id=?",
        )
        .bind(&[delivery_id.into(), subscription.id.clone().into()])?
        .first(None)
        .await?;
    if prior.as_ref().is_some_and(|row| row.status == "completed") {
        return Ok(());
    }

    let run_id = Uuid::new_v4().to_string();
    d1_run(
        db,
        "INSERT INTO webhook_automation_runs (id,delivery_id,subscription_id,status) VALUES (?,?,?,'running') ON CONFLICT(delivery_id,subscription_id) DO UPDATE SET status='running',error=NULL,updated_at=strftime('%Y-%m-%dT%H:%M:%fZ','now')",
        &[run_id.clone().into(), delivery_id.into(), subscription.id.clone().into()],
    )
    .await?;

    let base = env
        .var("OPENAI_API_BASE_URL")
        .map(|value| value.to_string())
        .unwrap_or_else(|_| "https://api.openai.com/v1".to_string())
        .trim_end_matches('/')
        .to_string();
    let body = serde_json::json!({
        "model":subscription.model,
        "instructions":subscription.instructions,
        "input":[{"role":"user","content":[{"type":"input_text","text":format!("Run the configured automation for this verified OpenAI webhook event.\nEvent type: {event_type}\nEvent payload:\n{payload}")}]}]
    });
    let headers = Headers::new();
    let api_key = env
        .secret("MIRROR_API_KEY")
        .or_else(|_| env.secret("OPENAI_API_KEY"))?
        .to_string();
    headers.set("Authorization", &format!("Bearer {api_key}"))?;
    headers.set("X-Mirror-Session-Token", &api_key)?;
    headers.set("Content-Type", "application/json")?;
    let mut init = RequestInit::new();
    init.with_method(Method::Post).with_headers(headers);
    init.with_body(Some(
        Uint8Array::from(serde_json::to_vec(&body)?.as_slice()).into(),
    ));
    let request = Request::new_with_init(&format!("{base}/responses"), &init)?;
    let mut response = Fetch::Request(request).send().await?;
    let status = response.status_code();
    let response_body = response.text().await?;
    if !(200..300).contains(&status) {
        d1_run(
            db,
            "UPDATE webhook_automation_runs SET status='failed',error=?,updated_at=strftime('%Y-%m-%dT%H:%M:%fZ','now') WHERE delivery_id=? AND subscription_id=?",
            &[format!("HTTP {status}: {response_body}").into(), delivery_id.into(), subscription.id.clone().into()],
        )
        .await?;
        return Err(format!("Responses API automation failed with HTTP {status}").into());
    }
    let response_json: serde_json::Value = serde_json::from_str(&response_body)?;
    d1_run(
        db,
        "UPDATE webhook_automation_runs SET status='completed',response_json=?,error=NULL,updated_at=strftime('%Y-%m-%dT%H:%M:%fZ','now') WHERE delivery_id=? AND subscription_id=?",
        &[serde_json::to_string(&response_json)?.into(), delivery_id.into(), subscription.id.clone().into()],
    )
    .await?;
    Ok(())
}

#[event(queue)]
pub async fn consume_webhook_events(
    batch: MessageBatch<WebhookQueueMessage>,
    env: worker::Env,
    _ctx: worker::Context,
) -> Result<()> {
    let db = env.d1("DB")?;
    for message in batch.messages()? {
        let delivery_id = &message.body().delivery_id;
        let delivery: Option<WebhookDeliveryQueueRow> = db
            .prepare("SELECT event_type,payload_json FROM webhook_deliveries WHERE delivery_id=?")
            .bind(&[delivery_id.into()])?
            .first(None)
            .await?;
        let Some(delivery) = delivery else { continue };
        let subscriptions: Vec<SubscriptionRow> = db
            .prepare("SELECT id,event_type,name,instructions,model,enabled,created_at FROM webhook_subscriptions WHERE event_type=? AND enabled=1")
            .bind(&[delivery.event_type.clone().into()])?
            .all()
            .await?
            .results()?;
        for subscription in subscriptions {
            execute_automation(
                &db,
                &env,
                delivery_id,
                &delivery.event_type,
                &delivery.payload_json,
                &subscription,
            )
            .await?;
        }
    }
    Ok(())
}

fn percent_encode(value: &str) -> String {
    percent_encoding::utf8_percent_encode(value, percent_encoding::NON_ALPHANUMERIC).to_string()
}

fn arg_to_string(value: &serde_json::Value) -> String {
    match value {
        serde_json::Value::String(v) => v.clone(),
        serde_json::Value::Null => String::new(),
        serde_json::Value::Array(items) => items
            .iter()
            .map(arg_to_string)
            .collect::<Vec<_>>()
            .join(","),
        serde_json::Value::Object(_) => value.to_string(),
        _ => value.to_string(),
    }
}

fn body_schema_shape(spec: &Yaml, schema: &Yaml) -> (bool, Vec<String>) {
    let schema = json_schema(spec, schema, 0);
    let is_object = schema.get("type").and_then(serde_json::Value::as_str) == Some("object")
        || schema.get("properties").is_some();
    let properties = schema
        .get("properties")
        .and_then(serde_json::Value::as_object)
        .map(|properties| properties.keys().cloned().collect())
        .unwrap_or_default();
    (is_object, properties)
}

fn append_multipart_part(
    body: &mut Vec<u8>,
    boundary: &str,
    name: &str,
    value: &serde_json::Value,
) -> Result<()> {
    if let Some(values) = value.as_array() {
        for value in values {
            append_multipart_part(body, boundary, name, value)?;
        }
        return Ok(());
    }
    let safe_name = name.replace(['\r', '\n', '"'], "_");
    body.extend_from_slice(format!("--{boundary}\r\n").as_bytes());
    if let Some(file) = value.as_object().filter(|v| v.contains_key("data_base64")) {
        let filename = file
            .get("filename")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("upload.bin")
            .replace(['\r', '\n', '"'], "_");
        let content_type = file
            .get("content_type")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("application/octet-stream")
            .replace(['\r', '\n'], "");
        let encoded = file
            .get("data_base64")
            .and_then(serde_json::Value::as_str)
            .ok_or("file data_base64 must be a string")?;
        let bytes = BASE64
            .decode(encoded)
            .map_err(|e| format!("invalid base64 file data for {name}: {e}"))?;
        body.extend_from_slice(format!("Content-Disposition: form-data; name=\"{safe_name}\"; filename=\"{filename}\"\r\nContent-Type: {content_type}\r\n\r\n").as_bytes());
        body.extend_from_slice(&bytes);
        body.extend_from_slice(b"\r\n");
    } else {
        body.extend_from_slice(
            format!(
                "Content-Disposition: form-data; name=\"{safe_name}\"\r\n\r\n{}\r\n",
                arg_to_string(value)
            )
            .as_bytes(),
        );
    }
    Ok(())
}

fn multipart_body(args: &serde_json::Map<String, serde_json::Value>) -> Result<(Vec<u8>, String)> {
    static BOUNDARY_COUNTER: std::sync::atomic::AtomicUsize =
        std::sync::atomic::AtomicUsize::new(1);
    let boundary = format!(
        "----mcp-openai-{}",
        BOUNDARY_COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    );
    let mut body = Vec::new();
    for (name, value) in args {
        append_multipart_part(&mut body, &boundary, name, value)?;
    }
    body.extend_from_slice(format!("--{boundary}--\r\n").as_bytes());
    Ok((body, format!("multipart/form-data; boundary={boundary}")))
}

async fn call_openai(
    operation: &Operation,
    arguments: &serde_json::Value,
    env: &worker::Env,
) -> Result<(u16, String)> {
    let args = arguments
        .as_object()
        .ok_or("tool arguments must be an object")?;
    let spec = spec()?;
    let server = yaml_get(&spec, "servers")
        .and_then(Yaml::as_sequence)
        .and_then(|v| v.first())
        .and_then(|v| yaml_string(yaml_get(v, "url")))
        .unwrap_or_else(|| "https://api.openai.com/v1".to_string());
    let base = env
        .var("OPENAI_API_BASE_URL")
        .map(|v| v.to_string())
        .unwrap_or(server)
        .trim_end_matches('/')
        .to_string();
    let mut path = operation.path.clone();
    let mut query = Vec::<(String, String)>::new();
    let mut extra_headers = Vec::<(String, String)>::new();
    let mut consumed = std::collections::HashSet::<String>::new();

    for parameter in &operation.parameters {
        let parameter = dereference(&spec, parameter, 0);
        let Some(name) = yaml_string(yaml_get(parameter, "name")) else {
            continue;
        };
        let Some(location) = yaml_string(yaml_get(parameter, "in")) else {
            continue;
        };
        if let Some(value) = args.get(&name) {
            consumed.insert(name.clone());
            match location.as_str() {
                "path" => {
                    path = path.replace(
                        &format!("{{{name}}}"),
                        &percent_encode(&arg_to_string(value)),
                    )
                }
                "query" => {
                    if let Some(values) = value.as_array() {
                        for item in values {
                            query.push((name.clone(), arg_to_string(item)));
                        }
                    } else {
                        query.push((name, arg_to_string(value)));
                    }
                }
                "header" => extra_headers.push((name, arg_to_string(value))),
                _ => {}
            }
        } else if location == "path" {
            return Err(format!("missing required path parameter: {name}").into());
        }
    }

    let mut body_args = serde_json::Map::new();
    let mut raw_body = None;
    if let Some(schema) = &operation.body_schema {
        let (is_object, fields) = body_schema_shape(&spec, schema);
        if is_object {
            for name in fields {
                if let Some(value) = args.get(&name) {
                    consumed.insert(name.clone());
                    body_args.insert(name.clone(), value.clone());
                }
            }
            // Keep optional extension fields accepted by OpenAI API request schemas.
            for (name, value) in args {
                if !consumed.contains(name) && name != "body" {
                    body_args.insert(name.clone(), value.clone());
                    consumed.insert(name.clone());
                }
            }
        } else if let Some(value) = args.get("body") {
            raw_body = Some(value.clone());
            consumed.insert("body".into());
        } else if operation.body_required {
            return Err("missing required body argument".into());
        }
    }
    if let Some(extra) = args.keys().find(|key| !consumed.contains(*key)) {
        return Err(format!("unknown tool argument: {extra}").into());
    }

    let mut url = Url::parse(&format!("{base}{path}"))?;
    for (name, value) in query {
        url.query_pairs_mut().append_pair(&name, &value);
    }

    let headers = Headers::new();
    let api_key = env
        .secret("MIRROR_API_KEY")
        .or_else(|_| env.secret("OPENAI_API_KEY"))?
        .to_string();
    headers.set("Authorization", &format!("Bearer {api_key}"))?;
    headers.set("X-Mirror-Session-Token", &api_key)?;
    headers.set("Accept", "application/json, text/event-stream, */*")?;
    if operation.path.starts_with("/assistants")
        || operation.path.starts_with("/threads")
        || operation.path.starts_with("/vector_stores")
    {
        headers.set("OpenAI-Beta", "assistants=v2")?;
    } else if operation.path.starts_with("/chatkit") {
        headers.set("OpenAI-Beta", "chatkit_beta=v1")?;
    }
    for (name, value) in extra_headers {
        headers.set(&name, &value)?;
    }

    let mut init = RequestInit::new();
    init.with_method(Method::from(operation.method.clone()));
    let mut body_bytes: Option<Vec<u8>> = None;
    if operation.method != "GET" && operation.method != "HEAD" {
        if let Some(media_type) = &operation.body_media_type {
            if media_type == "multipart/form-data" {
                let (bytes, content_type) = multipart_body(&body_args)?;
                headers.set("Content-Type", &content_type)?;
                body_bytes = Some(bytes);
            } else if media_type == "application/json" {
                let value = if raw_body.is_some() {
                    raw_body.unwrap()
                } else {
                    serde_json::Value::Object(body_args)
                };
                headers.set("Content-Type", "application/json")?;
                body_bytes = Some(serde_json::to_vec(&value)?);
            } else {
                let value = raw_body.unwrap_or_else(|| serde_json::Value::Object(body_args));
                headers.set("Content-Type", media_type)?;
                body_bytes = Some(arg_to_string(&value).into_bytes());
            }
        }
    }
    if let Some(bytes) = body_bytes {
        init.with_body(Some(Uint8Array::from(bytes.as_slice()).into()));
    }
    init.with_headers(headers);
    let request = Request::new_with_init(url.as_str(), &init)?;
    let mut response = Fetch::Request(request).send().await?;
    let status = response.status_code();
    let content_type = response.headers().get("content-type")?.unwrap_or_default();
    let bytes = response.bytes().await?;
    let text = if content_type.contains("json")
        || content_type.starts_with("text/")
        || content_type.contains("event-stream")
        || content_type.contains("xml")
    {
        String::from_utf8_lossy(&bytes).into_owned()
    } else {
        format!(
            "Binary API response ({} bytes, content-type: {}). Base64: {}",
            bytes.len(),
            content_type,
            BASE64.encode(bytes)
        )
    };
    Ok((status, text))
}

fn json_response(
    value: &serde_json::Value,
    status: u16,
    protocol_version: &str,
) -> Result<Response> {
    let mut response = Response::from_json(value)?.with_status(status);
    response.headers_mut().set("Cache-Control", "no-store")?;
    response
        .headers_mut()
        .set("Access-Control-Allow-Origin", "*")?;
    response.headers_mut().set(
        "Access-Control-Allow-Headers",
        "Content-Type, Mcp-Session-Id, Last-Event-ID, MCP-Protocol-Version, Authorization",
    )?;
    response
        .headers_mut()
        .set("MCP-Protocol-Version", protocol_version)?;
    response
        .headers_mut()
        .set("Access-Control-Expose-Headers", "MCP-Protocol-Version")?;
    response
        .headers_mut()
        .set("Access-Control-Allow-Methods", "GET, POST, OPTIONS")?;
    Ok(response)
}

fn rpc_error(id: serde_json::Value, code: i32, message: impl Into<String>) -> serde_json::Value {
    serde_json::json!({"jsonrpc":"2.0", "id":id, "error":{"code":code,"message":message.into()}})
}

async fn handle_rpc(
    value: serde_json::Value,
    env: &worker::Env,
    demo: bool,
    public: bool,
) -> Result<Option<serde_json::Value>> {
    if !value.is_object() || value.get("jsonrpc").and_then(serde_json::Value::as_str) != Some("2.0")
    {
        return Ok(Some(rpc_error(
            value.get("id").cloned().unwrap_or(serde_json::Value::Null),
            -32600,
            "Invalid Request",
        )));
    }
    let id = value.get("id").cloned();
    let method = value
        .get("method")
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default();
    let params = value
        .get("params")
        .cloned()
        .unwrap_or_else(|| serde_json::json!({}));
    if id.is_none() {
        // Notifications receive HTTP 202 and never a JSON-RPC response.
        return Ok(None);
    }
    let id = id.unwrap();
    let result = match method {
        "initialize" => {
            let requested = params
                .get("protocolVersion")
                .and_then(serde_json::Value::as_str)
                .unwrap_or(MCP_VERSION);
            let version = if ["2025-03-26", "2025-06-18", "2025-11-25"].contains(&requested) {
                requested
            } else {
                MCP_VERSION
            };
            serde_json::json!({"protocolVersion":version,"capabilities":{"tools":{"listChanged":false}},"serverInfo":{"name":"openai-api-mcp-server","version":env!("CARGO_PKG_VERSION")}})
        }
        "ping" => serde_json::json!({}),
        "tools/list" => match if demo {
            demo_tool_list()
        } else if public {
            public_tool_list()
        } else {
            tool_list()
        } {
            Ok(mut tools) => {
                if !demo && !public {
                    match webhook_tools() {
                        Ok(extra) => tools.extend(extra),
                        Err(error) => return Ok(Some(rpc_error(id, -32603, error.to_string()))),
                    }
                }
                serde_json::json!({"tools": tools})
            }
            Err(error) => return Ok(Some(rpc_error(id, -32603, error.to_string()))),
        },
        "tools/call" => {
            let Some(name) = params.get("name").and_then(serde_json::Value::as_str) else {
                return Ok(Some(rpc_error(
                    id,
                    -32602,
                    "tools/call requires a tool name",
                )));
            };
            let arguments = params
                .get("arguments")
                .cloned()
                .unwrap_or_else(|| serde_json::json!({}));
            if demo
                && name != DEMO_TOOL_NAME
                && name != DEMO_STATUS_TOOL_NAME
                && name != DEMO_MODELS_TOOL_NAME
            {
                return Ok(Some(rpc_error(id, -32602, format!("unknown tool: {name}"))));
            }
            if demo && name == DEMO_STATUS_TOOL_NAME {
                let configured = env.secret("MIRROR_API_KEY").is_ok();
                let result = serde_json::json!({
                    "content":[{"type":"text","text":serde_json::json!({
                        "status":"ok",
                        "mirrorCredentialConfigured":configured
                    }).to_string()}],
                    "isError":false
                });
                return Ok(Some(
                    serde_json::json!({"jsonrpc":"2.0","id":id,"result":result}),
                ));
            }
            if demo && name == DEMO_TOOL_NAME {
                let operation = Operation {
                    path: "/capabilities".into(),
                    method: "GET".into(),
                    name: DEMO_TOOL_NAME.into(),
                    summary: "Mirror capabilities".into(),
                    description: String::new(),
                    parameters: Vec::new(),
                    body_schema: None,
                    body_media_type: None,
                    body_required: false,
                    deprecated: false,
                };
                let result = match call_openai(&operation, &arguments, env).await {
                    Ok((status, body)) => serde_json::json!({
                        "content":[{"type":"text","text":body}],
                        "isError": !(200..300).contains(&status)
                    }),
                    Err(error) => serde_json::json!({
                        "content":[{"type":"text","text":format!("Mirror call failed: {error}")}],
                        "isError":true
                    }),
                };
                return Ok(Some(
                    serde_json::json!({"jsonrpc":"2.0","id":id,"result":result}),
                ));
            }
            if public && name.starts_with("webhook_") {
                return Ok(Some(rpc_error(id, -32602, format!("unknown tool: {name}"))));
            }
            if name.starts_with("webhook_") {
                let result = match call_webhook_tool(name, &arguments, env).await {
                    Ok(result) => result,
                    Err(error) => {
                        serde_json::json!({"content":[{"type":"text","text":format!("Webhook tool failed: {error}")}],"isError":true})
                    }
                };
                return Ok(Some(
                    serde_json::json!({"jsonrpc":"2.0","id":id,"result":result}),
                ));
            }
            let Some(operation) = operations()?
                .iter()
                .find(|operation| operation.name == name)
            else {
                return Ok(Some(rpc_error(id, -32602, format!("unknown tool: {name}"))));
            };
            match call_openai(operation, &arguments, env).await {
                Ok((status, body)) => {
                    let is_error = !(200..300).contains(&status);
                    let text = if is_error {
                        format!("OpenAI API returned HTTP {status}:\n{body}")
                    } else {
                        body
                    };
                    serde_json::json!({"content":[{"type":"text","text":text}],"isError":is_error})
                }
                Err(error) => {
                    serde_json::json!({"content":[{"type":"text","text":format!("Tool call failed: {error}")}],"isError":true})
                }
            }
        }
        _ => {
            return Ok(Some(rpc_error(
                id,
                -32601,
                format!("Method not found: {method}"),
            )));
        }
    };
    Ok(Some(
        serde_json::json!({"jsonrpc":"2.0","id":id,"result":result}),
    ))
}

#[event(fetch)]
pub async fn main(mut req: Request, env: worker::Env, _ctx: worker::Context) -> Result<Response> {
    let path = req.path();
    let method = req.method();
    let mut protocol_version = req
        .headers()
        .get("MCP-Protocol-Version")?
        .unwrap_or_else(|| MCP_VERSION.to_string());
    if method == Method::Options {
        let mut response = Response::empty()?.with_status(204);
        response
            .headers_mut()
            .set("Access-Control-Allow-Origin", "*")?;
        response.headers_mut().set(
            "Access-Control-Allow-Headers",
            "Content-Type, Mcp-Session-Id, Last-Event-ID, MCP-Protocol-Version, Authorization",
        )?;
        response
            .headers_mut()
            .set("Access-Control-Allow-Methods", "GET, POST, OPTIONS")?;
        return Ok(response);
    }
    if path == WEBHOOK_PATH {
        return receive_webhook(req, &env).await;
    }
    if path == "/" && method == Method::Get {
        let count = operations()?.len();
        return json_response(
            &serde_json::json!({"name":"openai-api-mcp-server","transport":"streamable-http","endpoint":MCP_PATH,"tools":count}),
            200,
            &protocol_version,
        );
    }
    if path != MCP_PATH && path != MCP_DEMO_PATH && path != MCP_PUBLIC_PATH {
        return json_response(
            &serde_json::json!({"error":"Not found"}),
            404,
            &protocol_version,
        );
    }
    if method != Method::Post {
        let mut response = Response::ok("MCP endpoint accepts POST requests")?.with_status(405);
        response.headers_mut().set("Allow", "POST, OPTIONS")?;
        response
            .headers_mut()
            .set("Access-Control-Allow-Origin", "*")?;
        return Ok(response);
    }
    if path == MCP_PATH {
        let configured_token = match env.secret("MCP_AUTH_TOKEN") {
            Ok(token) => token.to_string(),
            Err(_) => {
                return json_response(
                    &serde_json::json!({"error":"MCP_AUTH_TOKEN is not configured"}),
                    503,
                    &protocol_version,
                );
            }
        };
        let provided_token = req.headers().get("Authorization")?.unwrap_or_default();
        let provided_token = provided_token.strip_prefix("Bearer ").unwrap_or_default();
        if !constant_time_eq(configured_token.as_bytes(), provided_token.as_bytes()) {
            let mut response = Response::ok("Unauthorized")?.with_status(401);
            response.headers_mut().set("WWW-Authenticate", "Bearer")?;
            response
                .headers_mut()
                .set("Access-Control-Allow-Origin", "*")?;
            return Ok(response);
        }
    }
    let value: serde_json::Value = match req.json().await {
        Ok(value) => value,
        Err(error) => {
            return json_response(
                &rpc_error(
                    serde_json::Value::Null,
                    -32700,
                    format!("Parse error: {error}"),
                ),
                400,
                &protocol_version,
            );
        }
    };
    if value.get("method").and_then(serde_json::Value::as_str) == Some("initialize") {
        if let Some(requested) = value
            .pointer("/params/protocolVersion")
            .and_then(serde_json::Value::as_str)
            .filter(|version| ["2025-03-26", "2025-06-18", "2025-11-25"].contains(version))
        {
            protocol_version = requested.to_string();
        }
    }
    if value.is_array() {
        return json_response(
            &rpc_error(
                serde_json::Value::Null,
                -32600,
                "JSON-RPC batches are not supported",
            ),
            400,
            &protocol_version,
        );
    }
    match handle_rpc(value, &env, path == MCP_DEMO_PATH, path == MCP_PUBLIC_PATH).await? {
        Some(response) => json_response(&response, 200, &protocol_version),
        None => {
            let mut response = Response::empty()?.with_status(202);
            response
                .headers_mut()
                .set("Access-Control-Allow-Origin", "*")?;
            response.headers_mut().set("Cache-Control", "no-store")?;
            Ok(response)
        }
    }
}

fn constant_time_eq(expected: &[u8], provided: &[u8]) -> bool {
    let mut difference = expected.len() ^ provided.len();
    let max_len = expected.len().max(provided.len());
    for index in 0..max_len {
        let left = expected.get(index).copied().unwrap_or(0);
        let right = provided.get(index).copied().unwrap_or(0);
        difference |= usize::from(left ^ right);
    }
    difference == 0
}
