# Bono

Bono is a desktop AI race engineer and spotter for sim racing. Ask questions by voice or text and get spoken answers grounded in live telemetry: fuel range, tyre condition, lap times, nearby cars, and race standings.

Built in Rust with SDL3 and egui, Bono uses local Whisper speech recognition and Piper text-to-speech. The engineer supports Bono Cloud, Claude, Ollama, and OpenAI-compatible APIs. An automatic spotter reads telemetry directly to call traffic, flags, car alerts, and weather changes.

## Simulator support

Both current simulator integrations run on Windows:

- **Le Mans Ultimate:** a plugin records telemetry, scoring, and weather data. Bono detects Steam installations and can install, update, and enable its plugin from the setup guide or Simulators menu. Close the game before installing or updating it.
- **RaceRoom Racing Experience:** Bono reads the game's shared memory directly; no Bono plugin is required.

The application contains Linux and macOS build paths, but its current simulator adapters are Windows-only.

## Getting started

Launch `bono.exe` from a packaged build, keeping its `assets/`, `phonemizer/`, and `plugins/` folders alongside it. To build your own copy, see [Building from source](#building-from-source).

The first-run setup guide walks through:

1. Choosing the interface language and downloading speech models.
2. Detecting your simulator and setting up its plugin, if needed.
3. Selecting a language-model provider and model.
4. Choosing and testing your microphone.
5. Choosing an output device and downloading a voice.
6. Binding a push-to-talk key or wheel button, plus an optional conversation-reset button.
7. Performing a radio check.

Reopen the guide from Settings at any time. Start your simulator, enter a session, and ask questions such as “How many laps of fuel do I have?” or “What was my last lap time?” You can also type in the chat. Voice recognition requires a Whisper model; spoken replies require a Piper voice and the phonemizer helper.

## Language-model providers

Choose the provider and model in the Provider menu. The model must support tool calls so the engineer can query telemetry.

- **Bono Cloud:** offered only while signed in to a Bono account the website authorizes for it. It has no settings: the website chooses the model, the backend serving it, and its parameters. The app calls an OpenAI-compatible chat completions API at `/api/cloud/v1` on `BONO_ACCOUNT_ORIGIN` (`GET models`, streamed `POST chat/completions`), sending the account's access token as the bearer token. The website answers `403` to accounts without access, and Bono hides the provider for them. The website must deploy this endpoint before the provider can be used.
- **Claude** (the default): enter an API key in the app, or launch it with `ANTHROPIC_API_KEY` set. The client also accepts `ANTHROPIC_AUTH_TOKEN` when no API key is set, and an optional `ANTHROPIC_WORKSPACE_ID`.
- **Ollama:** run an Ollama server and pull a model before selecting it in Bono. The default address is `http://127.0.0.1:11434`; override it in the app or with `OLLAMA_HOST`.
- **OpenAI-compatible API:** use a server implementing chat completions and model listing. The default base URL is OpenAI's, `https://api.openai.com/v1`; for a local server such as llama.cpp, set it to e.g. `http://127.0.0.1:8080/v1`. Set the base URL in the app, including `/v1` where required, and enter an API key in the app or use `OPENAI_API_KEY` if authentication is required. This adapter supports local servers such as llama.cpp and LM Studio as well as hosted endpoints. For llama.cpp, enable tool calling with `--jinja`.

Environment variables must be available to the Bono process; the application does not automatically load a `.env` file. Credentials entered in the app are persisted in its JSON configuration.

Claude's **Prompt cache lifetime** setting defaults to `1h`; choose `5m` for shorter caching. The JSON setting is `model.claude.cache_ttl`. It applies to both the system prompt and conversation cache. One-hour cache writes cost more, but keep the cache available between questions more than five minutes apart.

Speech recognition and synthesis run locally. With a remote language-model endpoint, questions and telemetry returned by the engineer's tools are sent to that endpoint. Initial speech-model downloads require internet access.

## Troubleshooting

### Speech recognition is very slow while driving

Whisper runs on the same GPU as your simulator. With VSync off (or no frame-rate cap), the simulator renders as many frames as it can and keeps the GPU fully busy, so transcribing a question can take 10–30 seconds instead of about one. Enable VSync or cap the frame rate in the simulator to leave the GPU some headroom. Alternatively, untick **Use the GPU** in the Speech To Text menu to run Whisper on the CPU instead.

The log (`%APPDATA%\bono\bono\output.log` on Windows) records how long each transcription stage took.

## Account integration

The application's `account` module implements browser login, logout, account lookup, and session
restoration. Configure `BONO_ACCOUNT_ORIGIN` with the website origin at build time
(for example, `https://your-site.example`). When configured, app startup loads
the saved account and refreshes its session in the background. Without this setting,
account integration is disabled. HTTP origins are accepted only on loopback for development.

The account handle is available through `AppState::account()`. Clone it into a background
task and call `login().await` or `logout().await`. The topbar's **Account** menu opens
browser login, shows the signed-in user's name and email, and offers **Sign out**.
While login is pending, **Cancel** closes the local callback listener. Account operations
run in the background; progress and errors appear in the menu.
`login()` opens the system browser for website sign-in and explicit approval, then receives a browser
redirect on a temporary HTTP listener bound to `127.0.0.1`
on an OS-assigned port. The callback carries a single-use authorization code and random
state; the app exchanges the code over HTTPS using PKCE. Tokens never travel in the
browser redirect. `begin_login()` and `complete_login()` are also available separately.
`subscribe()` reports cached identity changes, `me()` validates the account with the
server, and `access_token()` refreshes access on demand. Do not log bearer tokens.

Refresh credentials persist in Windows Credential Manager, macOS Keychain, or Linux
Secret Service, rather than the JSON configuration. Linux requires a running Secret
Service and development DBus libraries when building. Access tokens stay in memory.
Temporary network/server failures retain the saved login; an expired or revoked refresh
credential requires a new login. Cached identity supports offline use but does not grant
server authorization. Run one account manager per OS user and website origin; independent
app processes can race server-side token rotation.

Logout immediately clears in-memory access and persists signed-out state. If the server
is unreachable, revocation is retried on startup or the next account operation. Logout
returns an error if revocation is pending, or if the OS credential store cannot persist
the change. Closing the app does not log out. A working OS credential store is required
to guarantee login/logout persistence across restarts.

The website must first deploy the matching `/api/desktop/auth/` endpoints and apply its
`0001_desktop_auth.sql` migration. The website checkout's `docs/desktop-auth.md` documents
the protocol, token lifetimes, and deployment configuration. Neither deployment nor a
production migration is performed by the desktop application.

Run the account module's tests (requires the application's native build prerequisites):

```sh
cargo test -p bono account::tests
```

### Loopback login protocol

The website must support this protocol before this desktop version can sign in:

1. Bono binds `127.0.0.1:0` and calls `POST /api/desktop/auth/start` with
   `{code_challenge, code_challenge_method: "S256", redirect_uri, state}`.
   `redirect_uri` is `http://127.0.0.1:<port>/callback`; `state` is random per attempt.
   The response is `{verification_uri, expires_in}` (maximum 1800 seconds), with
   `verification_uri` on the configured website origin.
2. The website keeps the callback, state, and PKCE challenge bound to the login request.
   After website sign-in and explicit approval, it redirects to the callback with
   `?code=<short-lived-single-use-code>&state=<original-state>`. Denial redirects with
   `?error=access_denied&state=<original-state>`. Validate the literal loopback address,
   callback path, and dynamic port before accepting a redirect URI.
3. Bono validates the callback path, Host, state, and unambiguous code/error parameters.
   It calls `POST /api/desktop/auth/exchange` with `{code, code_verifier, redirect_uri}`.
   The website must validate PKCE and the exact original redirect URI, consume the code
   once, and return the existing token/profile response. No tokens belong in redirect URLs.
4. Bono saves credentials before showing success in the browser, then closes the listener.
   Cancel, timeout, or dropping the login task also closes it. Existing `refresh`, `me`,
   and `logout` endpoints and credential persistence are unchanged.

The desktop no longer calls the polling `/api/desktop/auth/token` endpoint. Retire it on
the website once supported older desktop releases no longer depend on it; `/exchange`
remains necessary for the authorization-code exchange.

## Building from source

### Prerequisites

- A recent Rust toolchain with Cargo and Rust 2024 edition support. The project does not currently pin a Rust version.
- A C/C++ build toolchain, CMake, and Clang/libclang for generated bindings and native dependencies.
- On Windows: Visual Studio Build Tools with C++ tools and the Windows SDK, plus Ninja on `PATH`. The repository configures CMake to use Ninja for `x86_64-pc-windows-msvc`.
- For the default build: the CUDA toolkit and Vulkan SDK. A CPU-only build is available below.
- Internet access to fetch Cargo dependencies and build-time resources. The Windows build downloads the RaceRoom API header.

SDL3 and SQLite are built from source through Cargo dependencies.

### Run a development build

From the repository root:

```sh
cargo build -p bono-phonemizer
cargo run -p bono
```

Without the CUDA and Vulkan SDKs, use the CPU-only build:

```sh
cargo build -p bono-phonemizer
cargo run -p bono --no-default-features
```

If Just is installed, `just run` also builds the helper and starts the app with default features. Debug builds load assets from the repository's `assets/` directory.

### Build the Le Mans Ultimate plugin

On Windows, the plugin requires SDK headers from an installed copy of Le Mans Ultimate, under `Support/SharedMemoryInterface`. The build script searches Steam libraries automatically. If discovery fails, set `LMU_DIR` to the game's installation folder:

```powershell
$env:LMU_DIR = 'D:\SteamLibrary\steamapps\common\Le Mans Ultimate'
cargo build -p bono-lmu
```

The resulting `target/debug/bono_lmu.dll` sits alongside the development executable, where Bono can find it for installation. The plugin writes its telemetry database under the game's `UserData/bono/` directory.

### Speech-recognition backends

Cargo features control Whisper acceleration:

- `cuda` and `vulkan` are enabled by default; they require their respective SDKs (`CUDA_PATH` on Windows and `VULKAN_SDK`).
- `hipblas` enables ROCm on Linux (`HIP_PATH`).
- `intel-sycl` enables Intel oneAPI (`icx`/`icpx`).
- `openblas` enables OpenBLAS (`BLAS_INCLUDE_DIRS`, optionally `OPENBLAS_PATH`).

For a single backend, use, for example:

```sh
cargo run -p bono --no-default-features --features vulkan
```

Apple targets enable Metal automatically. The optional `all-languages` feature expands language detection for typed questions to all languages with available voices, adding roughly 180 MB of model data to the binary.

### Package a release

```sh
cargo xtask dist
```

This builds release binaries and recreates `dist/` with the application, assets, phonemizer, espeak-ng data and license, and (on Windows) the LMU plugin. Windows packaging therefore requires the LMU installation and SDK headers. The task uses the application's default CUDA and Vulkan features; it does not forward extra Cargo feature flags.

Windows packaging also includes DirectML, the Vulkan loader, and app-local x64 Visual C++ runtime DLLs for the application and phonemizer. Visual C++ redistributables are located through `VCToolsRedistDir` or Visual Studio (`vswhere`); the Vulkan loader comes from `VULKAN_SDK/Bin` or the installed Windows runtime. Missing required runtimes stop packaging. The Visual C++ installer is included in `dist/prerequisites`: run it before using the LMU plugin, which is loaded by the game process. Windows and compatible GPU drivers remain prerequisites; Whisper and Piper models are downloaded by the application.

Release builds load assets next to the executable, so use the complete `dist/` folder when running or distributing a release.

## Development

- `src/`: desktop interface, speech pipeline, model providers, engineer tools, simulator adapters, and spotter.
- `sim/`: shared simulator types, SQLite schema, and telemetry recorder.
- `lmu/`: Le Mans Ultimate plugin and its C++ SDK bridge.
- `phonemizer/`: separate espeak-ng helper process for speech synthesis.
- `i18n/`: translations and translation macros; strings live in `i18n/intl.json`.
- `include/bono/`: C headers shared by plugins and generated Rust bindings.
- `skill/`: race-engineer instructions and telemetry references, embedded at build time.
- `xtask/`: release packaging.

Useful checks, using CPU-only application features:

```sh
cargo fmt --all -- --check
cargo build -p bono-phonemizer
cargo test -p bono --no-default-features
cargo test -p bono-sim -p bono-i18n -p bono-phonemizer
```

Some integration tests are ignored by default because they require live provider credentials, a recorded telemetry database, or a voice model. See the tests in `src/provider/`, `src/engineer.rs`, and `src/synthesizer.rs` before enabling them. Building or testing the entire workspace on Windows also builds the LMU plugin and requires its game SDK.

Settings are saved as `config.json` in SDL's per-user preference directory for organization/application `bono`/`bono`. Downloaded Whisper models and Piper voices are stored beneath that directory in `whisper/` and `piper/`. Prefer changing settings through the interface.

## License

The main project is licensed under the [MIT License](LICENSE). The separate `bono-phonemizer` helper links espeak-ng and is licensed under [GPL-3.0-or-later](phonemizer/COPYING). Downloaded voice models and other third-party dependencies carry their own licenses.
