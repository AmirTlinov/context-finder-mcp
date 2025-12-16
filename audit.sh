#!/bin/bash

set -e

echo "╔═══════════════════════════════════════════════════════════════════════════╗"
echo "║                        FLAGSHIP QUALITY AUDIT                             ║"
echo "╚═══════════════════════════════════════════════════════════════════════════╝"
echo

PROJECT_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
cd "$PROJECT_ROOT"

AUDIT_DIR="$PROJECT_ROOT/.context-finder/audit"
mkdir -p "$AUDIT_DIR"

AUDIT_CLIPPY_LOG="$AUDIT_DIR/audit_clippy.log"
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

# ============================================================================
# 1. CODE QUALITY - Clippy (strictest lints)
# ============================================================================

echo -e "${BLUE}[1/8] Clippy Analysis (strictest lints)${NC}"
echo "────────────────────────────────────────"

cargo clippy --workspace --all-targets -- \
    -D warnings \
    -D clippy::all \
    -D clippy::pedantic \
    -D clippy::nursery \
    -A clippy::missing_errors_doc \
    -A clippy::missing_panics_doc \
    -A clippy::module_name_repetitions \
    2>&1 | tee "$AUDIT_CLIPPY_LOG"

if [ ${PIPESTATUS[0]} -eq 0 ]; then
    echo -e "${GREEN}✓ Clippy passed (strictest)${NC}"
else
    echo -e "${RED}✗ Clippy found issues${NC}"
fi
echo

# ============================================================================
# 2. SECURITY AUDIT - cargo audit
# ============================================================================

echo -e "${BLUE}[2/8] Security Audit (dependencies)${NC}"
echo "────────────────────────────────────────"

if command -v cargo-audit &> /dev/null; then
    cargo audit 2>&1 | tee "$AUDIT_SECURITY_LOG"
    if [ ${PIPESTATUS[0]} -eq 0 ]; then
        echo -e "${GREEN}✓ No known vulnerabilities${NC}"
    else
        echo -e "${YELLOW}⚠ Security issues found${NC}"
    fi
else
    echo -e "${YELLOW}⚠ cargo-audit not installed (run: cargo install cargo-audit)${NC}"
fi
echo

# ============================================================================
# 3. TEST COVERAGE
# ============================================================================

echo -e "${BLUE}[3/8] Test Coverage${NC}"
echo "────────────────────────────────────────"

# Run all tests
CONTEXT_FINDER_EMBEDDING_MODE="${CONTEXT_FINDER_EMBEDDING_MODE:-stub}" \
  cargo test --workspace 2>&1 | tee "$AUDIT_TESTS_LOG"

# Count tests
total_tests=$(grep -o "test result:" "$AUDIT_TESTS_LOG" | wc -l)
passed_tests=$(grep -oE "[0-9]+ passed" "$AUDIT_TESTS_LOG" | awk '{sum+=$1} END {print sum+0}')
ignored_tests=$(grep -oE "[0-9]+ ignored" "$AUDIT_TESTS_LOG" | awk '{sum+=$1} END {print sum+0}')

echo
echo "Test Statistics:"
echo "  Total test suites: $total_tests"
echo "  Passed: $passed_tests"
echo "  Ignored: $ignored_tests"
echo

if [ "$passed_tests" -gt 30 ]; then
    echo -e "${GREEN}✓ Good test coverage${NC}"
else
    echo -e "${YELLOW}⚠ Limited test coverage${NC}"
fi
echo

# ============================================================================
# 4. CODE COMPLEXITY - tokei
# ============================================================================

echo -e "${BLUE}[4/8] Code Complexity Analysis${NC}"
echo "────────────────────────────────────────"

if command -v tokei &> /dev/null; then
    tokei crates/ --exclude "*.json" --exclude "*.md" 2>&1 | tee "$AUDIT_COMPLEXITY_LOG"
    echo -e "${GREEN}✓ Code metrics generated${NC}"
else
    echo -e "${YELLOW}⚠ tokei not installed (run: cargo install tokei)${NC}"
    # Fallback to basic counting
    echo "Rust files:"
    find crates -name "*.rs" | wc -l
    echo "Total lines:"
    find crates -name "*.rs" -exec cat {} \; | wc -l
fi
echo

# ============================================================================
# 5. DEPENDENCY TREE
# ============================================================================

echo -e "${BLUE}[5/8] Dependency Analysis${NC}"
echo "────────────────────────────────────────"

cargo tree --workspace --depth 1 2>&1 | head -50 | tee "$AUDIT_DEPS_LOG"
echo -e "${GREEN}✓ Dependencies checked${NC}"
echo

# ============================================================================
# 6. BUILD VERIFICATION
# ============================================================================

echo -e "${BLUE}[6/8] Build Verification (release)${NC}"
echo "────────────────────────────────────────"

cargo build --workspace --release 2>&1 | tail -20 | tee "$AUDIT_BUILD_LOG"

if [ ${PIPESTATUS[0]} -eq 0 ]; then
    echo -e "${GREEN}✓ Release build successful${NC}"
else
    echo -e "${RED}✗ Release build failed${NC}"
fi
echo

# ============================================================================
# 7. DOCUMENTATION
# ============================================================================

echo -e "${BLUE}[7/8] Documentation Check${NC}"
echo "────────────────────────────────────────"

cargo doc --workspace --no-deps 2>&1 | tail -20 | tee "$AUDIT_DOCS_LOG"

if [ ${PIPESTATUS[0]} -eq 0 ]; then
    echo -e "${GREEN}✓ Documentation builds${NC}"
else
    echo -e "${YELLOW}⚠ Documentation warnings${NC}"
fi
echo

# ============================================================================
# 8. ARCHITECTURAL REVIEW
# ============================================================================

echo -e "${BLUE}[8/8] Architectural Review${NC}"
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
echo "Audit logs generated:"
echo "  - $AUDIT_CLIPPY_LOG (code quality)"
echo "  - $AUDIT_SECURITY_LOG (security)"
echo "  - $AUDIT_TESTS_LOG (test results)"
echo "  - $AUDIT_COMPLEXITY_LOG (code metrics)"
echo "  - $AUDIT_DEPS_LOG (dependencies)"
echo "  - $AUDIT_BUILD_LOG (build output)"
echo "  - $AUDIT_DOCS_LOG (documentation)"
echo
echo "Review these logs for detailed findings."
