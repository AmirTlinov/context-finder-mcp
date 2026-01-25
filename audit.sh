#!/bin/bash

set -uo pipefail

echo "╔═══════════════════════════════════════════════════════════════════════════╗"
echo "║                        FLAGSHIP QUALITY AUDIT                             ║"
echo "╚═══════════════════════════════════════════════════════════════════════════╝"
echo

PROJECT_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
cd "$PROJECT_ROOT"

AUDIT_DIR="$PROJECT_ROOT/.context-finder/audit"
mkdir -p "$AUDIT_DIR"

AUDIT_CONTRACTS_LOG="$AUDIT_DIR/audit_contracts.log"
AUDIT_FMT_LOG="$AUDIT_DIR/audit_fmt.log"
AUDIT_CLIPPY_LOG="$AUDIT_DIR/audit_clippy.log"
AUDIT_CLIPPY_STRICT_LOG="$AUDIT_DIR/audit_clippy_strict.log"
AUDIT_SECURITY_LOG="$AUDIT_DIR/audit_security.log"
AUDIT_TESTS_LOG="$AUDIT_DIR/audit_tests.log"
AUDIT_COMPLEXITY_LOG="$AUDIT_DIR/audit_complexity.log"
AUDIT_DEPS_LOG="$AUDIT_DIR/audit_deps.log"
AUDIT_BUILD_LOG="$AUDIT_DIR/audit_build.log"
AUDIT_DOCS_LOG="$AUDIT_DIR/audit_docs.log"

RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
BLUE='\033[0;34m'
NC='\033[0m' # No Color

# Gate failures make the script exit non-zero at the end.
# Advisory failures are reported but do not fail the audit.
FAILURES=()
WARNINGS=()

run_step() {
    local step_no="$1"
    local total="$2"
    local title="$3"
    local log_file="$4"
    local mode="$5" # gate|advisory
    shift 5

    echo -e "${BLUE}[${step_no}/${total}] ${title}${NC}"
    echo "────────────────────────────────────────"

    "$@" 2>&1 | tee "$log_file"
    local rc=${PIPESTATUS[0]}

    if [ "$rc" -eq 0 ]; then
        echo -e "${GREEN}✓ ${title}${NC}"
    else
        if [ "$mode" = "gate" ]; then
            echo -e "${RED}✗ ${title}${NC}"
            FAILURES+=("${step_no}: ${title}")
        else
            echo -e "${YELLOW}⚠ ${title}${NC}"
            WARNINGS+=("${step_no}: ${title}")
        fi
    fi
    echo
}

TOTAL_STEPS=10

# ============================================================================
# 0. CONTRACTS - scripts/validate_contracts.sh (quality gate)
# ============================================================================

run_step 0 "$TOTAL_STEPS" "Contract Validation (contract-first)" "$AUDIT_CONTRACTS_LOG" gate \
    scripts/validate_contracts.sh

# ============================================================================
# 1. FORMATTING - Rustfmt (quality gate)
# ============================================================================

run_step 1 "$TOTAL_STEPS" "Rustfmt Check" "$AUDIT_FMT_LOG" gate \
    cargo fmt --all -- --check

# ============================================================================
# 2. CODE QUALITY - Clippy (quality gate)
# ============================================================================

run_step 2 "$TOTAL_STEPS" "Clippy (project gate: -D warnings)" "$AUDIT_CLIPPY_LOG" gate \
    cargo clippy --workspace --all-targets -- -D warnings

# ============================================================================
# 3. TESTS - cargo test (quality gate)
# ============================================================================

run_step 3 "$TOTAL_STEPS" "Tests (stub embeddings)" "$AUDIT_TESTS_LOG" gate \
    env CONTEXT_EMBEDDING_MODE="${CONTEXT_EMBEDDING_MODE:-stub}" \
        cargo test --workspace

# ============================================================================
# 4. BUILD VERIFICATION - release build (quality gate)
# ============================================================================

run_step 4 "$TOTAL_STEPS" "Build Verification (release)" "$AUDIT_BUILD_LOG" gate \
    cargo build --workspace --release

# ============================================================================
# 5. CLIPPY (STRICT) - core targets (optional gate)
# ============================================================================

if [ "${AUDIT_STRICT_CLIPPY:-0}" = "1" ]; then
    run_step 5 "$TOTAL_STEPS" "Clippy (strict: core targets)" "$AUDIT_CLIPPY_STRICT_LOG" gate \
        cargo clippy -q -p context-finder-mcp --bin context-finder-mcp --message-format=short -- \
            -D warnings \
            -D clippy::all \
            -D clippy::pedantic \
            -D clippy::nursery \
            -A clippy::missing_errors_doc \
            -A clippy::missing_panics_doc \
            -A clippy::module_name_repetitions
else
    echo -e "${BLUE}[5/${TOTAL_STEPS}] Clippy (strict: core targets)${NC}"
    echo "────────────────────────────────────────"
    echo -e "${YELLOW}⚠ Skipped (set AUDIT_STRICT_CLIPPY=1 to enable)${NC}"
    echo "Skipped strict clippy (set AUDIT_STRICT_CLIPPY=1 to enable)." > "$AUDIT_CLIPPY_STRICT_LOG"
    echo
fi

# ============================================================================
# 6. SECURITY AUDIT - cargo audit (advisory)
# ============================================================================

echo -e "${BLUE}[6/${TOTAL_STEPS}] Security Audit (dependencies)${NC}"
echo "────────────────────────────────────────"

if command -v cargo-audit &> /dev/null; then
    AUDIT_IGNORE_ARGS=()
    if [ -f "$PROJECT_ROOT/audit.toml" ]; then
        while read -r advisory; do
            if [ -n "$advisory" ]; then
                AUDIT_IGNORE_ARGS+=(--ignore "$advisory")
            fi
        done < <(grep -Eo "RUSTSEC-[0-9]{4}-[0-9]{4}" "$PROJECT_ROOT/audit.toml" | sort -u)
    fi

    cargo audit "${AUDIT_IGNORE_ARGS[@]}" 2>&1 | tee "$AUDIT_SECURITY_LOG"
    rc=${PIPESTATUS[0]}
    if [ "$rc" -eq 0 ]; then
        echo -e "${GREEN}✓ Security Audit (dependencies)${NC}"
    else
        echo -e "${YELLOW}⚠ Security Audit (dependencies)${NC}"
        WARNINGS+=("6: Security Audit (dependencies)")
    fi
else
    echo "cargo-audit not installed (run: cargo install cargo-audit)" > "$AUDIT_SECURITY_LOG"
    echo -e "${YELLOW}⚠ cargo-audit not installed (run: cargo install cargo-audit)${NC}"
    WARNINGS+=("6: Security Audit (dependencies) (cargo-audit not installed)")
fi
echo

# ============================================================================
# 7. CODE COMPLEXITY - tokei (advisory)
# ============================================================================

echo -e "${BLUE}[7/${TOTAL_STEPS}] Code Complexity Analysis${NC}"
echo "────────────────────────────────────────"

if command -v tokei &> /dev/null; then
    tokei crates/ --exclude "*.json" --exclude "*.md" 2>&1 | tee "$AUDIT_COMPLEXITY_LOG"
    echo -e "${GREEN}✓ Code metrics generated${NC}"
else
    {
        echo "tokei not installed (run: cargo install tokei)"
        echo
        echo "Rust files:"
        find crates -name "*.rs" | wc -l
        echo "Total lines:"
        find crates -name "*.rs" -exec cat {} \; | wc -l
    } 2>&1 | tee "$AUDIT_COMPLEXITY_LOG"
    echo -e "${YELLOW}↷ tokei not installed (run: cargo install tokei)${NC}"
fi
echo

# ============================================================================
# 8. DEPENDENCY TREE (advisory)
# ============================================================================

echo -e "${BLUE}[8/${TOTAL_STEPS}] Dependency Analysis${NC}"
echo "────────────────────────────────────────"

cargo tree --workspace --depth 1 2>&1 | head -50 | tee "$AUDIT_DEPS_LOG"
echo -e "${GREEN}✓ Dependencies checked${NC}"
echo

# ============================================================================
# 9. DOCUMENTATION (advisory)
# ============================================================================

echo -e "${BLUE}[9/${TOTAL_STEPS}] Documentation Check${NC}"
echo "────────────────────────────────────────"

cargo doc --workspace --no-deps 2>&1 | tee "$AUDIT_DOCS_LOG"

if [ ${PIPESTATUS[0]} -eq 0 ]; then
    echo -e "${GREEN}✓ Documentation builds${NC}"
else
    echo -e "${YELLOW}⚠ Documentation warnings${NC}"
    WARNINGS+=("9: Documentation Check")
fi
echo

# ============================================================================
# 10. ARCHITECTURAL REVIEW (info)
# ============================================================================

echo -e "${BLUE}[10/${TOTAL_STEPS}] Architectural Review${NC}"
echo "────────────────────────────────────────"

echo "Crate structure:"
ls -1 crates/

echo
echo "Public API surface:"
find crates/*/src/lib.rs -exec echo "{}:" \; -exec grep "^pub " {} \; | head -50

echo
echo -e "${GREEN}✓ Architecture reviewed${NC}"
echo

# ============================================================================
# SUMMARY
# ============================================================================

echo "╔═══════════════════════════════════════════════════════════════════════════╗"
echo "║                          AUDIT COMPLETE                                   ║"
echo "╚═══════════════════════════════════════════════════════════════════════════╝"
echo

if [ "${#FAILURES[@]}" -gt 0 ]; then
    echo -e "${RED}Gate failures:${NC}"
    printf '  - %s\n' "${FAILURES[@]}"
    echo
fi

if [ "${#WARNINGS[@]}" -gt 0 ]; then
    echo -e "${YELLOW}Advisory warnings:${NC}"
    printf '  - %s\n' "${WARNINGS[@]}"
    echo
fi

echo "Audit logs generated:"
echo "  - $AUDIT_CONTRACTS_LOG (contracts)"
echo "  - $AUDIT_FMT_LOG (fmt)"
echo "  - $AUDIT_CLIPPY_LOG (clippy gate)"
echo "  - $AUDIT_CLIPPY_STRICT_LOG (strict clippy, optional)"
echo "  - $AUDIT_SECURITY_LOG (security)"
echo "  - $AUDIT_TESTS_LOG (test results)"
echo "  - $AUDIT_COMPLEXITY_LOG (code metrics)"
echo "  - $AUDIT_DEPS_LOG (dependencies)"
echo "  - $AUDIT_BUILD_LOG (build output)"
echo "  - $AUDIT_DOCS_LOG (documentation)"
echo
echo "Review these logs for detailed findings."

if [ "${#FAILURES[@]}" -gt 0 ]; then
    exit 1
fi
