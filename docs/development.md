---
created: 2025-11-06
updated: 2025-11-06
---

# Development Guide

This document explains how to set up the development environment, coding conventions, and development workflow for codexlog.

## Prerequisites

- **Go 1.25.2 or later**: The project is built with Go 1.25.2
- **Git**: For version control and tag-based versioning
- **make**: For running build tasks

## Local Setup

### Clone and Install Dependencies

```bash
git clone https://github.com/choplin/codexlog.git
cd codexlog
go mod download
```

### Tool Installation

The project manages development tools via the `tool` directive in `go.mod`:

```bash
# All tools are managed in go.mod and automatically available
# No manual installation required
```

Current tools:

- `golangci-lint`: Static analysis and linter
- `gofumpt`: Go code formatter (stricter than gofmt)

## Building

### Development Build

```bash
make build
```

The built binary will be placed at `bin/codexlog`.

### Version Injection

The version is automatically retrieved from `git describe` and injected at build time:

```makefile
VERSION ?= $(shell git describe --tags --dirty --always 2>/dev/null || echo dev)
LDFLAGS := -X main.version=$(VERSION)
```

- With a tag: `v0.1.0`
- Commits ahead of tag: `v0.1.0-1-g1234567`
- Dirty working directory: `v0.1.0-dirty`
- No tag: commit hash or `dev`

Check the version:

```bash
./bin/codexlog --version
```

## Testing

### Running Tests

```bash
# Run all tests
make test

# Verbose output
go test -v ./...

# With coverage
go test -v -race -coverprofile=coverage.txt -covermode=atomic ./...
```

### Test Data

Test data is located in the `testdata/sessions/` directory:

```
testdata/sessions/
├── 2025/
│   └── 01/
│       └── 15/
│           ├── 0193a4b2-8c90-7d4e-a123-456789abcdef.jsonl
│           └── 0193a4b3-1234-5678-9abc-def012345678.jsonl
```

Each `.jsonl` file contains various entry types. Session IDs must be in UUID format.

For complete details on the JSONL log format and all entry types, see [log-format.md](log-format.md).

## Linting and Formatting

### Running the Linter

```bash
make lint
```

This runs `golangci-lint`. Configuration is in `.golangci.yml`.

### Code Formatting

```bash
make fmt
```

This formats all `.go` files using `gofumpt`. `gofumpt` is stricter than standard `gofmt`.

**Note**: CI uses `make lint` instead of the GitHub Actions golangci-lint action to support Go 1.25.

## Code Style

### Package Structure

See [architecture.md](architecture.md#package-structure) for detailed package structure and responsibilities.

### Naming Conventions

- **Package names**: Short, lowercase, singular (`parser` not `parsers`)
- **Type names**: CamelCase, exported types start with uppercase
- **Constants**: Define in `const` blocks, use typed constants
- **Error messages**: Start with lowercase, no trailing period

### Error Handling

```go
// Good: descriptive error message
if err != nil {
    return fmt.Errorf("read session metadata: %w", err)
}

// Bad: no context
if err != nil {
    return err
}
```

### ContentBlock Usage

All content is normalized to `ContentBlock` arrays. See [log-format.md](log-format.md#contentblock-structure) for the complete ContentBlock specification and usage details.

## Adding New Features

See [architecture.md](architecture.md#extension-points) for detailed instructions on:

- Adding new commands
- Supporting new entry types
- Adding new output formats

## Debugging

### Inspecting Session Logs

When investigating a specific session during development:

```bash
# Display raw JSONL
./bin/codexlog view <session-id> --raw

# Show all entry types (no filters)
./bin/codexlog view <session-id> --all --format chat

# Show specific entry types only
./bin/codexlog view <session-id> -E response_item -T function_call
```

### Terminal Width and Color Handling

Terminal width is detected using `golang.org/x/term`:

```go
width, _, err := term.GetSize(int(f.Fd()))
```

Color output is enabled when:

- `--color` flag is specified
- OR stdout is a TTY and `--no-color` is not specified

During debugging:

```bash
# Force enable color
./bin/codexlog view <session-id> --format chat --color

# Disable color
./bin/codexlog view <session-id> --format chat --no-color
```

### Parser Debugging

Use the `--raw` flag to debug the parser:

```bash
./bin/codexlog view <session-id> --raw | jq
```

This outputs the filtered raw JSONL, making it easier to verify parsing logic.

## Pre-commit Hooks

The project has pre-commit hooks that automatically run `gofumpt` before commits.

Hook setup:

```bash
# Verify .git/hooks/pre-commit exists
cat .git/hooks/pre-commit
```

The hook automatically:

- Formats code with `gofumpt`
- Adds formatted files to the staging area

## Release Process

Releases are automated using GoReleaser and GitHub Actions.

### Releasing a New Version

1. Update `CHANGELOG.md` with release content
2. Create and push a version tag:
   ```bash
   git tag -a v0.2.0 -m "Release v0.2.0"
   git push origin v0.2.0
   ```
3. GitHub Actions automatically:
   - Runs `go test`
   - Builds binaries for all platforms (Linux/macOS, amd64/arm64)
   - Creates GitHub Release
   - Updates Homebrew tap (`choplin/homebrew-tap`)

### Homebrew Tap

The Homebrew formula is published to the `choplin/homebrew-tap` repository:

- Repository: https://github.com/choplin/homebrew-tap
- Formula location: Top level (not in `Formula/` directory)
- Auto-update: GoReleaser updates based on `.goreleaser.yml` configuration

### Testing Releases Locally

```bash
# GoReleaser snapshot build (without tag)
goreleaser release --snapshot --clean

# Check build artifacts
ls -la dist/
```

## CI Workflows

### `.github/workflows/ci.yml`

Runs on all pushes and pull requests:

- `make test`: Test execution with race detector
- `make lint`: Static analysis
- `make build`: Build verification and version check

### `.github/workflows/release.yml`

Runs when `v*` tags are pushed:

- Release build using GoReleaser
- GitHub Release creation
- Homebrew tap update

Required secrets:

- `GITHUB_TOKEN`: Automatically provided
- `HOMEBREW_TAP_GITHUB_TOKEN`: Personal access token with push permission to Homebrew tap
