pub(super) const PROJECT_IDENTITY_DOCS: &[&str] = &[
    "README.md",
    "docs/README.md",
    "AGENTS.md",
    "PHILOSOPHY.md",
    "ARCHITECTURE.md",
    "docs/ARCHITECTURE.md",
    "docs/QUICK_START.md",
    "DEVELOPMENT.md",
    "CONTRIBUTING.md",
];

pub(super) const MODULE_DOC_HINTS: &[&str] = &["README.md", "AGENTS.md", "docs/README.md"];

pub(super) const ENTRYPOINT_HINTS: &[&str] = &[
    "Cargo.toml",
    "package.json",
    "pyproject.toml",
    "go.mod",
    "README.md",
];

pub(super) const MODULE_ENTRYPOINT_HINTS: &[&str] = &[
    "src/main.rs",
    "src/lib.rs",
    "main.go",
    "main.py",
    "app.py",
    "src/main.py",
    "src/app.py",
    "src/index.ts",
    "src/index.js",
    "src/main.ts",
    "src/main.js",
];

pub(super) const CONTRACT_HINTS: &[&str] = &[
    "docs/contracts/protocol.md",
    "docs/contracts/README.md",
    "docs/contracts/runtime.md",
    "docs/contracts/quality_gates.md",
    "ARCHITECTURE.md",
    "docs/ARCHITECTURE.md",
    "README.md",
    "proto/command.proto",
    "contracts/http/v1/openapi.json",
    "contracts/http/v1/openapi.yaml",
    "contracts/http/v1/openapi.yml",
    "openapi.json",
    "openapi.yaml",
    "openapi.yml",
];

pub(super) const CONTRACT_FRONT_DOOR_DOCS: &[&str] = &["README.md", "readme.md"];

pub(super) const CONFIG_DOC_HINTS: &[&str] =
    &["README.md", "docs/QUICK_START.md", "DEVELOPMENT.md"];

pub(super) const CONFIG_FILE_HINTS: &[&str] = &[
    "config/.env.example",
    "config/.env.sample",
    "config/.env.template",
    "config/.env.dist",
    "config/docker-compose.yml",
    "config/docker-compose.yaml",
    "configs/.env.example",
    "configs/docker-compose.yml",
    "configs/docker-compose.yaml",
    "config/config.yml",
    "config/config.yaml",
    "config/settings.yml",
    "config/settings.yaml",
    "configs/config.yml",
    "configs/config.yaml",
    "configs/settings.yml",
    "configs/settings.yaml",
];
