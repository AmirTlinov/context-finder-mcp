# ADR 0001: Server Safety (Bind Guard + Optional Auth)

Date: 2026-01-26

## Context

Context exposes a Command API over HTTP (`serve-http`) and gRPC (`serve-grpc`).

By default these are developer-facing integration surfaces intended to run on loopback.
However, it is easy to accidentally bind to `0.0.0.0` / a public interface, which could expose:

- project paths (via requests),
- code snippets,
- operational metadata.

We want the safe default to be hard to misuse, without forcing a heavyweight auth stack.

## Decision

1) Non-loopback binds require explicit opt-in via `--public`.
2) `--public` requires an auth token (`--auth-token` or `CONTEXT_AUTH_TOKEN`).
3) When an auth token is set, the server requires `Authorization: Bearer <token>` (HTTP) or
   `authorization` metadata (gRPC).

The OpenAPI contract declares an optional Bearer scheme.

## Consequences

- Accidental public exposure becomes a hard error by default.
- Local loopback usage remains simple (no auth required).
- Operators who want to expose the service must make the decision explicit and provide a token.

## Alternatives considered

- Always require auth: rejected (hurts local DX).
- mTLS-only: rejected (too heavy as a default).
- IP allowlist only: rejected (fragile, not portable).
