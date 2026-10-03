use serde_yaml::{Mapping, Value};
use std::{collections::HashSet, env, fs, path::PathBuf};

const METHODS: [&str; 8] = [
    "get", "post", "put", "patch", "delete", "head", "options", "trace",
];

fn get<'a>(value: &'a Value, key: &str) -> Option<&'a Value> {
    value.as_mapping()?.get(Value::String(key.to_string()))
}

fn insert(mapping: &mut Mapping, key: &str, value: Value) {
    mapping.insert(Value::String(key.to_string()), value);
}

fn resolve_ref<'a>(spec: &'a Value, reference: &str) -> Option<&'a Value> {
    let mut value = spec;
    for segment in reference.strip_prefix("#/")?.split('/') {
        let segment = segment.replace("~1", "/").replace("~0", "~");
        value = value.as_mapping()?.get(Value::String(segment))?;
    }
    Some(value)
}

fn collect_refs(value: &Value, references: &mut Vec<String>) {
    match value {
        Value::Mapping(mapping) => {
            if let Some(Value::String(reference)) = mapping.get(Value::String("$ref".into())) {
                references.push(reference.clone());
            }
            for child in mapping.values() {
                collect_refs(child, references);
            }
        }
        Value::Sequence(sequence) => {
            for child in sequence {
                collect_refs(child, references);
            }
        }
        _ => {}
    }
}

fn run() -> Result<(), Box<dyn std::error::Error>> {
    println!("cargo:rerun-if-changed=openapi.yaml");
    let source: Value = serde_yaml::from_str(&fs::read_to_string("openapi.yaml")?)?;
    let source_paths = get(&source, "paths")
        .and_then(Value::as_mapping)
        .ok_or("OpenAPI document has no paths object")?;
    let mut paths = Mapping::new();

    for (path, path_item) in source_paths {
        let Some(path) = path.as_str() else { continue };
        let Some(path_item_source) = path_item.as_mapping() else {
            continue;
        };
        let mut path_item_output = Mapping::new();
        for key in ["parameters", "servers"] {
            if let Some(value) = path_item_source.get(Value::String(key.into())) {
                insert(&mut path_item_output, key, value.clone());
            }
        }
        for method in METHODS {
            let Some(operation) = path_item_source.get(Value::String(method.into())) else {
                continue;
            };
            let Some(operation_source) = operation.as_mapping() else {
                continue;
            };
            let mut operation_output = Mapping::new();
            for key in [
                "operationId",
                "summary",
                "description",
                "deprecated",
                "parameters",
                "requestBody",
                "servers",
            ] {
                if let Some(value) = operation_source.get(Value::String(key.into())) {
                    insert(&mut operation_output, key, value.clone());
                }
            }
            insert(
                &mut path_item_output,
                method,
                Value::Mapping(operation_output),
            );
        }
        insert(&mut paths, path, Value::Mapping(path_item_output));
    }

    let mut minimal = Mapping::new();
    for key in ["openapi", "servers"] {
        if let Some(value) = get(&source, key) {
            insert(&mut minimal, key, value.clone());
        }
    }
    insert(&mut minimal, "paths", Value::Mapping(paths.clone()));

    let mut webhooks = Mapping::new();
    if let Some(source_webhooks) = get(&source, "webhooks").and_then(Value::as_mapping) {
        for (name, webhook) in source_webhooks {
            let Some(name) = name.as_str() else { continue };
            let Some(post) = get(webhook, "post") else {
                continue;
            };
            let mut webhook_item = Mapping::new();
            for key in ["description", "requestBody"] {
                if let Some(value) = get(post, key) {
                    insert(&mut webhook_item, key, value.clone());
                }
            }
            let mut post_item = Mapping::new();
            insert(&mut post_item, "post", Value::Mapping(webhook_item));
            insert(&mut webhooks, name, Value::Mapping(post_item));
        }
    }
    insert(&mut minimal, "webhooks", Value::Mapping(webhooks.clone()));

    let mut references = Vec::new();
    collect_refs(&Value::Mapping(paths), &mut references);
    collect_refs(&Value::Mapping(webhooks), &mut references);
    let mut visited = HashSet::new();
    let mut retained_components = Mapping::new();
    while let Some(reference) = references.pop() {
        if !visited.insert(reference.clone()) {
            continue;
        }
        let Some(rest) = reference.strip_prefix("#/components/") else {
            continue;
        };
        let mut parts = rest.splitn(2, '/');
        let Some(component_type) = parts.next() else {
            continue;
        };
        let Some(component_name) = parts.next() else {
            continue;
        };
        let decoded_name = component_name.replace("~1", "/").replace("~0", "~");
        let Some(target) = resolve_ref(&source, &reference) else {
            continue;
        };
        let component_map = retained_components
            .entry(Value::String(component_type.to_string()))
            .or_insert_with(|| Value::Mapping(Mapping::new()));
        let Some(component_map) = component_map.as_mapping_mut() else {
            continue;
        };
        if !component_map.contains_key(Value::String(decoded_name.clone())) {
            insert(component_map, &decoded_name, target.clone());
            collect_refs(target, &mut references);
        }
    }
    insert(
        &mut minimal,
        "components",
        Value::Mapping(retained_components),
    );

    let json = serde_json::to_vec(&Value::Mapping(minimal))?;
    let output =
        PathBuf::from(env::var_os("OUT_DIR").ok_or("OUT_DIR is not set")?).join("openapi.json");
    fs::write(output, json)?;
    Ok(())
}

fn main() {
    if let Err(error) = run() {
        panic!("failed to prepare OpenAPI metadata: {error}");
    }
}
