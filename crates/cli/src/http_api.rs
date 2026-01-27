use crate::command::{CommandResponse, CommandStatus, ResponseMeta};
use crate::server_security::AuthToken;
use axum::{
    body::Body,
    http::{header::AUTHORIZATION, HeaderMap, Response as HttpResponse, StatusCode},
    response::Response,
};
use context_protocol::{serialize_json, ErrorEnvelope};

pub(crate) fn is_authorized(headers: &HeaderMap, token: &AuthToken) -> bool {
    let Some(value) = headers.get(AUTHORIZATION) else {
        return false;
    };
    let Ok(value) = value.to_str() else {
        return false;
    };
    token.matches_http_authorization_header(value)
}

pub(crate) fn error_response(code: &str, message: String) -> CommandResponse {
    let hint = match code {
        "unauthorized" => Some(
            "If the server is started with CONTEXT_AUTH_TOKEN, include Authorization: Bearer <token>. To disable auth, unset CONTEXT_AUTH_TOKEN and restart the server."
                .to_string(),
        ),
        "invalid_request" => Some(
            "Verify the request is valid JSON and matches the Command API schema."
                .to_string(),
        ),
        _ => Some("Check the request against the Command API schema.".to_string()),
    };

    CommandResponse {
        status: CommandStatus::Error,
        message: Some(message.clone()),
        error: Some(ErrorEnvelope {
            code: code.to_string(),
            message,
            details: None,
            hint,
            next_actions: Vec::new(),
        }),
        hints: Vec::new(),
        next_actions: Vec::new(),
        data: serde_json::Value::Null,
        meta: ResponseMeta::default(),
    }
}

pub(crate) fn build_response(
    status: StatusCode,
    response: CommandResponse,
) -> Result<Response, StatusCode> {
    let bytes = serialize_json(&response)
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
        .into_bytes();

    let mut builder = HttpResponse::builder()
        .status(status)
        .header("content-type", "application/json");

    if status == StatusCode::UNAUTHORIZED {
        builder = builder.header("www-authenticate", "Bearer");
    }

    Ok(builder
        .body(Body::from(bytes))
        .expect("valid HTTP response"))
}
