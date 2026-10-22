PORT ?= COM3
CARGO ?= cargo
ESPFLASH ?= espflash

.PHONY: test build-host build-firmware prod run-host

test:
	$(CARGO) test --workspace
	$(CARGO) build -p codex-phone-host

build-host:
	$(CARGO) build -p codex-phone-host --release

build-firmware:
	$(CARGO) +esp build --manifest-path firmware/Cargo.toml --release

prod: build-host build-firmware
	$(ESPFLASH) flash --port $(PORT) firmware/target/xtensa-esp32-espidf/release/codex-phone-firmware

run-host:
	$(CARGO) run -p codex-phone-host --release
