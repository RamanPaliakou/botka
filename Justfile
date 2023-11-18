default:
	@just --list

# Autoformat code (rust, nix, yaml)
fmt:
	cargo fmt
	nixfmt flake.nix
	prettier -w \
		config.example.yaml \
		residents-timeline/{index.ts,package.json,tsconfig.json} \
		;

# Run lints and tests
check:
	cargo clippy --all-targets -- --deny warnings --cfg clippy
	cargo test

# Regenerate src/schema.rs from diesel migrations
schema:
	rm -f diesel.tmp.db
	diesel --database-url diesel.tmp.db migration run
	rm -f diesel.tmp.db

# Regenerate hashes.nix
hashes:
	#!/usr/bin/env bash
	set -euo pipefail
	HASH=$(prefetch-npm-deps residents-timeline/package-lock.json)
	{
		echo '# Automatically generated by `just hashes`. Do not edit.'
		echo "{ residents-timeline = \"${HASH}\"; }"
	} > hashes.nix
