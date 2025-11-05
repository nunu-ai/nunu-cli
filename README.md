# nunu-cli

CLI tool for uploading build artifacts to nunu.ai.

## Installation

Download the latest release from [GitHub Releases](https://github.com/nunu-ai/nunu-cli/releases).

### Linux

**Latest version (recommended):**
```bash
curl -L -O https://github.com/nunu-ai/nunu-cli/releases/latest/download/nunu-cli-linux-x86_64
chmod +x nunu-cli-linux-x86_64
sudo mv nunu-cli-linux-x86_64 /usr/local/bin/nunu-cli
```

**Specific version:**
```bash
VERSION=0.1.15  # Replace with desired version
curl -L -O https://github.com/nunu-ai/nunu-cli/releases/download/v${VERSION}/nunu-cli-linux-x86_64
chmod +x nunu-cli-linux-x86_64
sudo mv nunu-cli-linux-x86_64 /usr/local/bin/nunu-cli
```

### macOS

**ARM64 (Apple Silicon) - Latest version (recommended):**
```bash
curl -L -O https://github.com/nunu-ai/nunu-cli/releases/latest/download/nunu-cli-macos-arm64
chmod +x nunu-cli-macos-arm64
sudo mv nunu-cli-macos-arm64 /usr/local/bin/nunu-cli
```

**x86_64 (Intel) - Latest version:**
```bash
curl -L -O https://github.com/nunu-ai/nunu-cli/releases/latest/download/nunu-cli-macos-x86_64
chmod +x nunu-cli-macos-x86_64
sudo mv nunu-cli-macos-x86_64 /usr/local/bin/nunu-cli
```

**Specific version:**
```bash
VERSION=0.1.15  # Replace with desired version
ARCH=arm64      # or x86_64 for Intel Macs
curl -L -O https://github.com/nunu-ai/nunu-cli/releases/download/v${VERSION}/nunu-cli-macos-${ARCH}
chmod +x nunu-cli-macos-${ARCH}
sudo mv nunu-cli-macos-${ARCH} /usr/local/bin/nunu-cli
```

> **Note:** Apple Silicon Macs can run either binary. The ARM64 version is native and recommended for best performance.

### Windows

**Latest version:** Download [nunu-cli-windows-x86_64.exe](https://github.com/nunu-ai/nunu-cli/releases/latest/download/nunu-cli-windows-x86_64.exe) and rename to `nunu-cli.exe`.

**Specific version:** Visit the [releases page](https://github.com/nunu-ai/nunu-cli/releases), select your version, and download `nunu-cli-windows-x86_64.exe`.

## Quick Start
```bash
# Set credentials (get from Project Admin → API Keys)
export NUNU_API_TOKEN=your_token
export NUNU_PROJECT_ID=your_project_id

# Upload a build
nunu-cli upload build/app.apk --name "Production v1.2.3"
```

Platform is automatically detected from file extension.

## Usage
```bash
# Single file
nunu-cli upload <file> --name "Build Name"

# Pattern matching when filename is unknown (common in CI/CD)
nunu-cli upload "build/app-v*.apk" --name "Android Release"
nunu-cli upload "dist/myapp-*.exe" --name "Windows Build"

# With options
nunu-cli upload "build/app-*.exe" \
  --name "Windows Build" \
  --description "Release build" \
  --auto-delete \
  --tags "version:1.2.3,env:prod"

# Show all options
nunu-cli upload --help
```

### File Pattern Matching

Use glob patterns when you don't know the exact filename:

- `*` - Matches any characters: `app-v*.apk`, `build-*.zip`
- `?` - Matches single character: `app-?.apk`
- `[...]` - Matches character sets: `app-[123].apk`, `build-[0-9].exe`

Common use case: Build tools add version numbers or timestamps to filenames. Pattern matching lets you upload without knowing the exact name.

If multiple files match, each becomes a separate build with the filename appended to your name template.

### Key Options

- `--platform <PLATFORM>` - Target platform (auto-detected if possible)
- `--auto-delete` - Auto-delete old builds when storage is full
- `--tags <TAGS>` - Comma-separated tags for organization
- `--parallel <N>` - Parallel uploads (1-32, default: 4)
- `-v, --verbose` - Enable detailed logging

### Platform Detection

Automatically detected: `.apk` (android), `.ipa` (ios-native), `.exe/.msi` (windows), `.dmg/.pkg` (macos), `.deb/.rpm/.appimage` (linux)

For ambiguous files (`.zip`, `.tar`), specify `--platform` explicitly.

## Configuration

### Environment Variables (Recommended)
```bash
export NUNU_API_TOKEN=your_token
export NUNU_PROJECT_ID=your_project_id
```

### Config File

Create `nunu.json` in your project:
```json
{
  "api_token": "your_token",
  "project_id": "your_project_id"
}
```

The CLI automatically searches for config files in the following order:
1. `--config` flag (if specified)
2. `./nunu.json` (project root)
3. `./.nunu/config.json` (hidden directory in project root)
4. `~/.config/nunu/config.json` (user-level config on Linux/macOS)
5. `%APPDATA%/nunu/config.json` (user-level config on Windows)



## Automatic Metadata Collection

The CLI automatically detects and collects metadata from your environment:

**Git information** (via git commands):
- Commit hash, branch, author, message
- PR number and details (when available)
- Repository URL and provider (GitHub, GitLab, etc.)

**CI/CD information** (via environment variables):
- Automatically detects: GitHub Actions, GitLab CI, Jenkins, CircleCI, Travis CI, Azure Pipelines, Bitrise
- Collects: Build number, workflow name, run URL, triggered by, runner info

**Build information**:
- Timestamp, uploader, CLI version

Metadata is collected automatically when:
- Running inside a git repository (for VCS info)
- Running in a CI/CD environment (for CI info)

No additional configuration required.

## CI/CD Integration

### GitHub Actions
```yaml
- name: Upload build
  env:
    NUNU_API_TOKEN: ${{ secrets.NUNU_API_TOKEN }}
    NUNU_PROJECT_ID: ${{ secrets.NUNU_PROJECT_ID }}
  run: nunu-cli upload "build/app-*.apk" --name "Build ${{ github.run_number }}"
```

For convenience in GitHub Actions, use our [GitHub Action](https://github.com/nunu-ai/upload-build-action) which wraps the CLI.

### Other CI/CD Systems

The CLI automatically detects and collects metadata from:
- GitLab CI
- Jenkins
- CircleCI
- Travis CI
- Azure Pipelines
- Bitrise

Works with any CI/CD system. See [documentation](https://docs.nunu.ai) for examples.

## Features

- ✅ Automatic platform detection from file extension
- ✅ File pattern matching with glob patterns
- ✅ Large file support (multipart uploads for files >3GB)
- ✅ Parallel uploads for speed
- ✅ Automatic metadata collection from git and CI/CD environments
- ✅ Smart storage management with auto-delete
- ✅ Progress tracking and graceful cancellation

## Documentation

Full documentation available at [docs.nunu.ai](https://docs.nunu.ai) (requires authentication).

- Configuration options and best practices
- CI/CD integration examples
- API reference for advanced usage
- Troubleshooting guide

## Support

Contact us through nunu.ai for support and questions.
