help: ## Show this help
	@grep -E '^[a-z]+:.*##' $(MAKEFILE_LIST) | sed 's/:.*## /\t/'

lint: ## Check formatting + clippy
	cargo fmt --check
	cargo clippy -- -D warnings

fix: ## Auto-fix formatting + clippy
	cargo fmt
	cargo clippy --fix --allow-dirty

coverage: ## HTML coverage report (cargo-tarpaulin)
	cargo tarpaulin --out html

mutants: ## Mutation testing (cargo-mutants)
	cargo mutants
