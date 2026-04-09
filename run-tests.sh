#!/usr/bin/env bash
set -euo pipefail

# Enhanced test runner for the Agent Orchestrator
# Supports different test types, environments, and reporting

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
TEST_CONFIG="${SCRIPT_DIR}/tests/test_config.toml"
REPORTS_DIR="${SCRIPT_DIR}/test-reports"

# Colors for output
RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
BLUE='\033[0;34m'
NC='\033[0m' # No Color

# Default values
ENVIRONMENT="local"
COVERAGE=false
VERBOSE=false
PARALLEL=true
WATCH=false

usage() {
    echo "Usage: $0 [COMMAND] [OPTIONS]"
    echo ""
    echo "Commands:"
    echo "  unit           Run unit tests only"
    echo "  integration    Run integration tests only"
    echo "  e2e            Run end-to-end tests only"
    echo "  performance    Run performance/load tests"
    echo "  chaos          Run chaos engineering tests"
    echo "  regression     Run regression test suite"
    echo "  all            Run all tests (default)"
    echo "  watch          Run tests in watch mode"
    echo "  clean          Clean test artifacts"
    echo ""
    echo "Options:"
    echo "  --env ENV      Test environment (local|ci|staging)"
    echo "  --coverage     Enable code coverage reporting"
    echo "  --verbose      Enable verbose output"
    echo "  --no-parallel  Disable parallel test execution"
    echo "  --help         Show this help message"
    echo ""
    echo "Examples:"
    echo "  $0 unit --coverage"
    echo "  $0 integration --env ci"
    echo "  $0 all --verbose"
    echo "  $0 watch"
}

log() {
    echo -e "${BLUE}[$(date '+%H:%M:%S')]${NC} $*"
}

success() {
    echo -e "${GREEN}✓${NC} $*"
}

error() {
    echo -e "${RED}✗${NC} $*" >&2
}

warning() {
    echo -e "${YELLOW}⚠${NC} $*"
}

run_cargo_test_target() {
    local target="$1"
    shift

    local cargo_args=("$@")
    local test_args=()

    if [ "$PARALLEL" = false ]; then
        test_args+=(--test-threads=1)
    fi

    if [ ${#test_args[@]} -gt 0 ]; then
        cargo test --test "$target" "${cargo_args[@]}" -- "${test_args[@]}"
    else
        cargo test --test "$target" "${cargo_args[@]}"
    fi
}

run_cargo_test_filter() {
    local filter="$1"
    shift

    local cargo_args=("$@")
    local test_args=()

    if [ "$PARALLEL" = false ]; then
        test_args+=(--test-threads=1)
    fi

    if [ ${#test_args[@]} -gt 0 ]; then
        cargo test "$filter" "${cargo_args[@]}" -- "${test_args[@]}"
    else
        cargo test "$filter" "${cargo_args[@]}"
    fi
}

setup_reports_dir() {
    mkdir -p "$REPORTS_DIR"
}

check_dependencies() {
    if ! command -v cargo >/dev/null 2>&1; then
        error "cargo is required but not installed"
        exit 1
    fi
    
    if [ "$COVERAGE" = true ] && ! command -v cargo-tarpaulin >/dev/null 2>&1; then
        warning "cargo-tarpaulin not found. Installing..."
        cargo install cargo-tarpaulin
    fi
}

run_lints() {
    log "Running lints and format checks..."
    
    if ! cargo fmt --check; then
        error "Code formatting issues found. Run 'cargo fmt' to fix."
        return 1
    fi
    
    if ! cargo clippy -- -D warnings; then
        error "Clippy found issues"
        return 1
    fi
    
    success "Lints passed"
}

run_unit_tests() {
    log "Running unit tests..."
    
    local cargo_args=()
    [ "$VERBOSE" = true ] && cargo_args+=(--verbose)
    [ "$PARALLEL" = true ] && cargo_args+=(--jobs "$(nproc)")
    
    if [ "$COVERAGE" = true ]; then
        cargo tarpaulin \
            --out Html --output-dir "$REPORTS_DIR" \
            --include-tests \
            --test unit_tests \
            "${cargo_args[@]}"
    else
        run_cargo_test_target unit_tests "${cargo_args[@]}"
    fi
    
    success "Unit tests completed"
}

run_integration_tests() {
    log "Running integration tests..."
    
    local cargo_args=()
    [ "$VERBOSE" = true ] && cargo_args+=(--verbose)
    
    # Set environment variables for integration tests
    export TEST_ENV="$ENVIRONMENT"
    export RUST_LOG="${RUST_LOG:-info}"
    
    run_cargo_test_target integration_tests "${cargo_args[@]}"
    
    success "Integration tests completed"
}

run_e2e_tests() {
    log "Running end-to-end tests..."
    
    local cargo_args=()
    [ "$VERBOSE" = true ] && cargo_args+=(--verbose)
    
    # E2E tests need single-threaded execution to avoid conflicts
    export TEST_ENV="$ENVIRONMENT"
    export RUST_LOG="${RUST_LOG:-warn}"
    
    run_cargo_test_target e2e "${cargo_args[@]}"
    
    success "E2E tests completed"
}

run_performance_tests() {
    log "Running performance tests..."
    
    # Performance tests always run single-threaded for accurate measurements
    export TEST_ENV="$ENVIRONMENT"
    export RUST_LOG="${RUST_LOG:-error}"
    
    PARALLEL=false run_cargo_test_filter performance --release
    
    success "Performance tests completed"
}

run_chaos_tests() {
    log "Running chaos engineering tests..."
    
    export TEST_ENV="$ENVIRONMENT"
    export RUST_LOG="${RUST_LOG:-warn}"
    export CHAOS_TESTING=true
    
    PARALLEL=false run_cargo_test_filter chaos
    
    success "Chaos tests completed"
}

run_regression_tests() {
    log "Running regression test suite..."
    
    export TEST_ENV="$ENVIRONMENT"
    export RUST_LOG="${RUST_LOG:-info}"
    export REGRESSION_MODE=true
    
    run_cargo_test_filter regression
    
    success "Regression tests completed"
}

run_all_tests() {
    log "Running complete test suite..."
    
    run_lints
    run_unit_tests
    run_integration_tests
    run_e2e_tests
    
    success "All tests completed successfully!"
}

watch_tests() {
    log "Starting test watcher..."
    
    if ! command -v cargo-watch >/dev/null 2>&1; then
        warning "cargo-watch not found. Installing..."
        cargo install cargo-watch
    fi
    
    cargo watch -x "test --lib --bins" -x "test --test unit_tests"
}

clean_test_artifacts() {
    log "Cleaning test artifacts..."
    
    rm -rf "$REPORTS_DIR"
    rm -rf target/debug/deps/agent_orchestrator-*
    rm -rf /tmp/agent-orchestrator-tests* 2>/dev/null || true
    
    success "Test artifacts cleaned"
}

generate_test_report() {
    if [ ! -d "$REPORTS_DIR" ]; then
        return
    fi
    
    log "Generating test report summary..."
    
    cat > "$REPORTS_DIR/summary.md" << EOF
# Test Report Summary

Generated: $(date)
Environment: $ENVIRONMENT
Coverage Enabled: $COVERAGE

## Test Results

- Unit Tests: $([ -f "$REPORTS_DIR/unit.log" ] && echo "✓" || echo "⚠")
- Integration Tests: $([ -f "$REPORTS_DIR/integration.log" ] && echo "✓" || echo "⚠")
- E2E Tests: $([ -f "$REPORTS_DIR/e2e.log" ] && echo "✓" || echo "⚠")

$([ -f "$REPORTS_DIR/tarpaulin-report.html" ] && echo "## Coverage Report Available: tarpaulin-report.html" || echo "")

EOF
    
    success "Test report generated: $REPORTS_DIR/summary.md"
}

main() {
    local command="${1:-all}"
    shift || true
    
    # Parse options
    while [[ $# -gt 0 ]]; do
        case $1 in
            --env)
                ENVIRONMENT="$2"
                shift 2
                ;;
            --coverage)
                COVERAGE=true
                shift
                ;;
            --verbose)
                VERBOSE=true
                export RUST_LOG="${RUST_LOG:-debug}"
                shift
                ;;
            --no-parallel)
                PARALLEL=false
                shift
                ;;
            --help)
                usage
                exit 0
                ;;
            *)
                error "Unknown option: $1"
                usage
                exit 1
                ;;
        esac
    done
    
    setup_reports_dir
    check_dependencies
    
    case $command in
        unit)
            run_unit_tests
            ;;
        integration)
            run_integration_tests
            ;;
        e2e)
            run_e2e_tests
            ;;
        performance)
            run_performance_tests
            ;;
        chaos)
            run_chaos_tests
            ;;
        regression)
            run_regression_tests
            ;;
        all)
            run_all_tests
            ;;
        watch)
            watch_tests
            ;;
        clean)
            clean_test_artifacts
            ;;
        *)
            error "Unknown command: $command"
            usage
            exit 1
            ;;
    esac
    
    generate_test_report
}

main "$@"
