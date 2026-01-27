use assert_cmd::prelude::*;
use std::process::Command;

#[test]
fn serve_http_refuses_non_loopback_without_public() {
    Command::new(assert_cmd::cargo::cargo_bin!("context"))
        .env("CONTEXT_EMBEDDING_MODE", "stub")
        .args(["serve-http", "--bind", "0.0.0.0:0"])
        .assert()
        .failure()
        .stderr(predicates::str::contains("Refusing to bind"));
}

#[test]
fn serve_http_public_requires_auth_token() {
    Command::new(assert_cmd::cargo::cargo_bin!("context"))
        .env("CONTEXT_EMBEDDING_MODE", "stub")
        .args(["serve-http", "--public", "--bind", "0.0.0.0:0"])
        .assert()
        .failure()
        .stderr(predicates::str::contains("--public requires an auth token"));
}

#[test]
fn serve_grpc_refuses_non_loopback_without_public() {
    Command::new(assert_cmd::cargo::cargo_bin!("context"))
        .env("CONTEXT_EMBEDDING_MODE", "stub")
        .args(["serve-grpc", "--bind", "0.0.0.0:0"])
        .assert()
        .failure()
        .stderr(predicates::str::contains("Refusing to bind"));
}

#[test]
fn serve_grpc_public_requires_auth_token() {
    Command::new(assert_cmd::cargo::cargo_bin!("context"))
        .env("CONTEXT_EMBEDDING_MODE", "stub")
        .args(["serve-grpc", "--public", "--bind", "0.0.0.0:0"])
        .assert()
        .failure()
        .stderr(predicates::str::contains("--public requires an auth token"));
}
