#[derive(Clone, Copy)]
pub(super) struct AnchorNeedle {
    pub(super) needle: &'static str,
    pub(super) score: i32,
}

pub(super) const MEMORY_ANCHOR_SCAN_MAX_LINES: usize = 2_000;
pub(super) const MEMORY_ANCHOR_SCAN_MAX_BYTES: usize = 200_000;

pub(super) const DOC_ANCHOR_NEEDLES: &[AnchorNeedle] = &[
    AnchorNeedle {
        needle: "cargo test",
        score: 120,
    },
    AnchorNeedle {
        needle: "pytest",
        score: 120,
    },
    AnchorNeedle {
        needle: "go test",
        score: 120,
    },
    AnchorNeedle {
        needle: "npm test",
        score: 120,
    },
    AnchorNeedle {
        needle: "yarn test",
        score: 120,
    },
    AnchorNeedle {
        needle: "pnpm test",
        score: 120,
    },
    AnchorNeedle {
        needle: "clippy",
        score: 110,
    },
    AnchorNeedle {
        needle: "cargo run",
        score: 105,
    },
    AnchorNeedle {
        needle: "npm run dev",
        score: 105,
    },
    AnchorNeedle {
        needle: "npm start",
        score: 105,
    },
    AnchorNeedle {
        needle: "quick start",
        score: 95,
    },
    AnchorNeedle {
        needle: "getting started",
        score: 95,
    },
    AnchorNeedle {
        needle: "project invariants",
        score: 110,
    },
    AnchorNeedle {
        needle: "invariants",
        score: 95,
    },
    AnchorNeedle {
        needle: "инвариант",
        score: 95,
    },
    AnchorNeedle {
        needle: "philosophy",
        score: 85,
    },
    AnchorNeedle {
        needle: "философ",
        score: 85,
    },
    AnchorNeedle {
        needle: "architecture",
        score: 85,
    },
    AnchorNeedle {
        needle: "архитектур",
        score: 85,
    },
    AnchorNeedle {
        needle: "protocol",
        score: 80,
    },
    AnchorNeedle {
        needle: "протокол",
        score: 80,
    },
    AnchorNeedle {
        needle: "contract",
        score: 80,
    },
    AnchorNeedle {
        needle: "контракт",
        score: 80,
    },
    AnchorNeedle {
        needle: "install",
        score: 70,
    },
    AnchorNeedle {
        needle: "usage",
        score: 60,
    },
    AnchorNeedle {
        needle: "configuration",
        score: 70,
    },
    AnchorNeedle {
        needle: ".env.example",
        score: 70,
    },
    AnchorNeedle {
        needle: "docker",
        score: 45,
    },
];

pub(super) const CONFIG_ANCHOR_NEEDLES: &[AnchorNeedle] = &[
    AnchorNeedle {
        needle: "test",
        score: 80,
    },
    AnchorNeedle {
        needle: "lint",
        score: 70,
    },
    AnchorNeedle {
        needle: "clippy",
        score: 70,
    },
    AnchorNeedle {
        needle: "fmt",
        score: 60,
    },
    AnchorNeedle {
        needle: "format",
        score: 60,
    },
    AnchorNeedle {
        needle: "build",
        score: 60,
    },
    AnchorNeedle {
        needle: "run",
        score: 55,
    },
    AnchorNeedle {
        needle: "scripts",
        score: 80,
    },
    AnchorNeedle {
        needle: "run:",
        score: 55,
    },
];

pub(super) const ENTRYPOINT_ANCHOR_NEEDLES: &[AnchorNeedle] = &[
    AnchorNeedle {
        needle: "fn main",
        score: 120,
    },
    AnchorNeedle {
        needle: "func main(",
        score: 120,
    },
    AnchorNeedle {
        needle: "def main",
        score: 120,
    },
    AnchorNeedle {
        needle: "public static void main",
        score: 120,
    },
    AnchorNeedle {
        needle: "int main(",
        score: 120,
    },
    AnchorNeedle {
        needle: "app.listen",
        score: 90,
    },
    AnchorNeedle {
        needle: "createserver",
        score: 80,
    },
];
