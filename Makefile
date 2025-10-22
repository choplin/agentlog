BINARY := codexlog
CMD_DIR := ./cmd/$(BINARY)

.PHONY: all build run test fmt lint clean

all: build

build:
	go build -o bin/$(BINARY) $(CMD_DIR)

run: build
	bin/$(BINARY)

test:
	go test ./...

fmt:
	go tool mvdan.cc/gofumpt -w .

lint:
	go tool github.com/golangci/golangci-lint/cmd/golangci-lint run ./...

clean:
	rm -rf bin
