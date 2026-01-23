use rmcp::handler::server::tool::{ToolCallContext, ToolRouter};
use rmcp::model::{CallToolResult, ErrorCode, JsonObject, Tool};
use rmcp::ErrorData;
use serde_json::{json, Map, Value};
use std::borrow::Cow;

use super::ContextFinderService;

#[derive(Clone)]
pub(super) struct ToolRouterWithParamHints<S> {
    inner: ToolRouter<S>,
}

impl<S> ToolRouterWithParamHints<S>
where
    S: Send + Sync + 'static,
{
    pub(super) fn new(inner: ToolRouter<S>) -> Self {
        Self { inner }
    }

    pub(super) fn list_all(&self) -> Vec<Tool> {
        self.inner.list_all()
    }
}

impl ToolRouterWithParamHints<ContextFinderService> {
    pub(super) async fn call(
        &self,
        context: ToolCallContext<'_, ContextFinderService>,
    ) -> Result<CallToolResult, ErrorData> {
        let tool_name = context.name.to_string();
        let args = context.arguments.clone();

        match self.inner.call(context).await {
            Ok(result) => Ok(result),
            Err(err) => Err(enrich_invalid_params(
                &self.inner,
                &tool_name,
                args.as_ref(),
                err,
            )),
        }
    }
}

fn enrich_invalid_params<S>(
    router: &ToolRouter<S>,
    tool_name: &str,
    args: Option<&JsonObject>,
    mut err: ErrorData,
) -> ErrorData {
    if err.code != ErrorCode::INVALID_PARAMS {
        return err;
    }

    let schema = router
        .map
        .get(tool_name)
        .map(|route| route.attr.input_schema.as_ref());
    let hint = schema.and_then(|schema| build_schema_hint(schema, args, err.message.as_ref()));

    let mut message = format!("Invalid parameters for tool '{tool_name}': {}", err.message);
    if let Some(hint) = hint.as_deref() {
        message.push_str(" Hint: ");
        message.push_str(hint);
    }
    if message.len() > 900 {
        message.truncate(900);
        message.push('…');
    }

    if err.data.is_none() {
        if let Some(schema) = schema {
            if let Some(required) = schema
                .get("required")
                .and_then(Value::as_array)
                .map(|arr| {
                    arr.iter()
                        .filter_map(Value::as_str)
                        .map(ToString::to_string)
                        .collect::<Vec<_>>()
                })
                .filter(|v: &Vec<String>| !v.is_empty())
            {
                err.data = Some(json!({
                    "tool": tool_name,
                    "required": required,
                }));
            }
        }
    }

    err.message = Cow::Owned(message);
    err
}

fn build_schema_hint(
    schema: &Map<String, Value>,
    args: Option<&JsonObject>,
    err_message: &str,
) -> Option<String> {
    let required = schema.get("required").and_then(Value::as_array)?;
    let required: Vec<String> = required
        .iter()
        .filter_map(Value::as_str)
        .map(ToString::to_string)
        .collect();
    if required.is_empty() {
        return None;
    }

    let missing = extract_serde_field(err_message, "missing field `");
    let unknown = extract_serde_field(err_message, "unknown field `");

    let required_list = required
        .iter()
        .take(6)
        .cloned()
        .collect::<Vec<_>>()
        .join(", ");

    let example = build_required_example(schema, args, missing.as_deref(), &required);

    let mut out = String::new();
    if let Some(missing) = missing.as_deref() {
        out.push_str(&format!("missing required field `{missing}`. "));
    }
    if let Some(unknown) = unknown.as_deref() {
        out.push_str(&format!("unknown field `{unknown}`. "));
    }
    out.push_str(&format!("Required: {required_list}."));
    if let Some(example) = example {
        out.push_str(&format!(" Example: {example}"));
    }
    Some(out)
}

fn extract_serde_field(message: &str, prefix: &str) -> Option<String> {
    let start = message.find(prefix)? + prefix.len();
    let rest = &message[start..];
    let end = rest.find('`')?;
    let field = rest[..end].trim();
    if field.is_empty() {
        None
    } else {
        Some(field.to_string())
    }
}

fn build_required_example(
    schema: &Map<String, Value>,
    args: Option<&JsonObject>,
    missing_field: Option<&str>,
    required: &[String],
) -> Option<String> {
    let props = schema.get("properties").and_then(Value::as_object);
    let mut out = Map::new();

    for field in required.iter().take(4) {
        if out.contains_key(field) {
            continue;
        }
        let prop_schema = props.and_then(|m| m.get(field));
        let value = placeholder_value(field, prop_schema, args, missing_field);
        out.insert(field.clone(), value);
    }

    serde_json::to_string(&Value::Object(out)).ok()
}

fn placeholder_value(
    field: &str,
    schema: Option<&Value>,
    args: Option<&JsonObject>,
    missing_field: Option<&str>,
) -> Value {
    if field == "path" {
        return Value::String(".".to_string());
    }

    // Common slip: user passes `path=src/...` but the tool expects `focus`.
    if field == "focus" && missing_field == Some("focus") {
        if let Some(value) = args.and_then(|a| a.get("path")).and_then(Value::as_str) {
            if !value.trim().is_empty() {
                return Value::String(value.to_string());
            }
        }
    }

    if let Some(schema) = schema {
        if let Some(value) = schema.get("default") {
            return value.clone();
        }
        if let Some(value) = schema
            .get("examples")
            .and_then(Value::as_array)
            .and_then(|arr| arr.first())
        {
            return value.clone();
        }
        if let Some(value) = schema
            .get("enum")
            .and_then(Value::as_array)
            .and_then(|arr| arr.first())
        {
            return value.clone();
        }

        if let Some(ty) = schema.get("type").and_then(Value::as_str) {
            return match ty {
                "string" => Value::String("...".to_string()),
                "integer" | "number" => Value::Number(0.into()),
                "boolean" => Value::Bool(false),
                "array" => Value::Array(Vec::new()),
                "object" => Value::Object(Map::new()),
                _ => Value::String("...".to_string()),
            };
        }
    }

    // Fallback: keep examples compact.
    Value::String("...".to_string())
}
