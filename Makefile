.PHONY: install

COMPLETION_SHELL ?= $(shell s=$$(basename "$${SHELL:-sh}"); if [ "$$s" = nu ]; then echo nushell; elif [ "$$s" = pwsh ] || [ "$$s" = powershell.exe ]; then echo powershell; else echo "$$s"; fi)

install:
	cargo install --path .
	cargo bench-compare completions $(COMPLETION_SHELL) --install
