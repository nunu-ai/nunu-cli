# nunu-cli

CLI tool for uploading build artifacts to Nunu.ai

## Installation

```bash
# Build from source
cargo build --release
```

## Configuration

`nunu-cli` supports multiple ways to provide configuration, with the following priority (highest to lowest):

1. **CLI arguments** - Direct command-line options
2. **Environment variables** - `NUNU_API_TOKEN`, `NUNU_PROJECT_ID`, `NUNU_API_URL`
3. **Config file via --config** - Explicitly specified config file
4. **Project config file** - `./nunu.json`, `./.nunu/config.json` (also `./config.json`, `./.config.json`)
5. **User config file** - `~/.config/nunu/config.json`
6. **Default values**

Environment variables or config file is recommended, use `nunu.json` or store it in a location with no naming conflicts.

### Config File Format

Create a `config.json` file in one of the supported locations:

```json
{
  "api_token": "your-api-token-here",
  "project_id": "your-project-id-here",
  "api_url": "https://nunu.ai/api"
}
```

All fields are optional. See `config.example.json` for a template.

## Usage

### Upload a single file

```bash
# without platform inference (for ambiguous file extensions)
nunu-cli upload path/to/build.zip \
  --name "My Build" --platform windows \
  --token {my_token} --project {my_project}

# with platform inference (for known file extensions)
nunu-cli upload build.exe \
  --name "Inferred Platform" \
  --token {my_token} --project {my_project}
```

### Upload multiple files

```bash
# template name (Multi-platform Build 1, Multi-platform Build 2 ...)
nunu-cli upload build1.zip build2.zip \
  --name "Multi-platform Build" \
  --platform windows \
  --token {my_token} --project {my_project}
```

### Using environment variables

```bash
export NUNU_API_TOKEN="your-token"
export NUNU_PROJECT_ID="your-project-id"
nunu-cli upload build.apk \
  --name "Android Build" --platform android
```

### Using a config file

```bash
# Use default config locations
nunu-cli upload build.ipa --name "iOS Build" --platform ios-native

# Or specify a config file
nunu-cli --config /path/to/config.json upload build.exe --name "Build"
```

## Options

### Global Options

- `--help` - Display a help message with all options and usage.
- `-v, --verbose` - Enable verbose output. Can be used multiple times for increased verbosity:
  - `-v`: INFO level (general progress)
  - `-vv`: DEBUG level (detailed debugging)
  - `-vvv`: TRACE level (maximum detail)
- `-c, --config <PATH>` - Path to config file (JSON format)

### Upload Options

- `--token <TOKEN>` - API token for authentication (or use `NUNU_API_TOKEN` env var)
- `--project-id <ID>` - Project ID (or use `NUNU_PROJECT_ID` env var)
- `--name <NAME>` - Build name (required)
- `--platform <PLATFORM>` - Target platform (auto-detected from file extension if not specified)
  - Supported: `windows`, `macos`, `linux`, `android`, `ios-native`, `ios-simulator`, `web`
- `--description <DESC>` - Build description (optional)
- `--upload-timeout <MINUTES>` - Upload timeout in minutes (1-1440)
- `--auto-delete` - Automatically delete old builds if storage limits are exceeded
- `--deletion-policy <POLICY>` - Deletion policy: `least_recent` (default) or `oldest`
- `--force-multipart` - Force multipart upload even for small files
- `--parallel <N>` - Number of parallel uploads/parts (1-32, default: 4)
- `--tags <TAGS>` - List of user-defined tags (comma-separated)

## Platform Detection

The CLI can auto-detect the platform from file extensions:

- `.exe`, `.msi` → `windows`
- `.dmg`, `.pkg` → `macos`
- `.deb`, `.rpm`, `.appimage` → `linux`
- `.apk` → `android`
- `.ipa` → `ios-native`

For ambiguous files (`.zip`, `.tar`, etc.), explicitly specify `--platform`.

## Parallel Uploads

Control concurrency with the `--parallel` flag:

```bash
# Upload 8 files concurrently
nunu-cli upload *.zip --name "Batch Upload" --parallel 8

# For large multipart uploads, use higher parallelism
nunu-cli upload large-file.zip --name "Large Build" --parallel 16
```

## Verbose Mode

The CLI supports four levels of verbosity:

### Normal Mode (Default - No flag)
Shows a clean 2-line display with only warnings and errors:
- Current status message
- Progress bar for active upload

```bash
nunu-cli upload build.zip --name "Build"
```

### Info Mode (`-v`)
Shows INFO logs - general progress information:
```bash
nunu-cli -v upload build.zip --name "Build"
```

### Debug Mode (`-vv`)
Shows DEBUG logs - detailed debugging information:
```bash
nunu-cli -vv upload build.zip --name "Debug Build"
```

### Trace Mode (`-vvv`)
Shows TRACE logs - maximum detail for troubleshooting:
```bash
nunu-cli -vvv upload build.zip --name "Trace Build"
```

## Graceful Shutdown

The CLI handles termination signals gracefully and will attempt to abort active uploads:

- **SIGINT** (Ctrl+C): Interactive cancellation - exits with code 130
- **SIGTERM** (Unix/Linux only): Graceful termination request (e.g., from `kill`, Docker, systemd) - exits with code 143

When a termination signal is received:
1. All active uploads are identified
2. Abort requests are sent to the server for each upload
3. The program exits cleanly

This ensures proper cleanup in CI/CD environments, containers, and system shutdowns.

## Full Examples

```bash
# Simple upload with config file
nunu-cli upload MyGame.apk --name "Android Release v1.0"

# Upload with all options
nunu-cli upload MyGame.exe \
  --name "Windows Release v1.0" \
  --platform windows \
  --description "Stable release with bug fixes" \
  --auto-delete \
  --deletion-policy oldest \
  --parallel 8 \
  --verbose

# Multi-file upload with custom config
nunu-cli --config ./ci-config.json upload \
  builds/windows.exe \
  builds/macos.dmg \
  builds/linux.appimage \
  --name "v2.0.0 Multi-platform" \
  --parallel 4
```
