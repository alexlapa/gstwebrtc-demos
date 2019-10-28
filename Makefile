fmt: cargo.fmt

lint: cargo.lint

up: docker.up

down: docker.down

cargo.fmt:
	cargo +nightly fmt --all --manifest-path server/Cargo.toml

cargo.lint:
	cargo +nightly clippy --all --manifest-path server/Cargo.toml -- -D clippy::pedantic -D warnings

docker.up:
	docker-compose up -d

docker.down:
	docker-compose down