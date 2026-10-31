# In-app updates

## Intent and agreed behavior

Users can update Bono without manually replacing its files. The installed build determines its release channel: stable builds offer only newer stable releases, nightly builds offer only newer nightlies. The user explicitly downloads an update, then separately installs it. A new download replaces a previously downloaded, uninstalled update only after the new download is complete and validated. Installation uses a separate helper, as selected by the user.

## Existing system and scope

The supported distribution is Windows x86-64, published by `.github/workflows/windows.yml` as `bono-windows-x86_64.zip` in `racebono/bono` GitHub releases. The ZIP contains the executable, assets, phonemizer, plugins, and runtime DLLs. `build.rs` currently identifies nightly versions through `BONO_COMMIT`. `App::begin_quit` already provides asynchronous shutdown.

This change supports in-app installation for official Windows x86-64 distributions. Other platforms and source builds display their version and an unavailable explanation, without offering installation. No channel switching, automatic downloads, automatic installation, forced elevation, or simulator-plugin deployment into game directories is included. User configuration, downloaded speech models, credentials, and telemetry caches remain outside the installed bundle and are preserved.

## Approaches

1. Download and open a ZIP for manual replacement: simpler, but leaves installation to the user.
2. Stage the full bundle and launch a separate helper: handles Windows executable locking and updates dependent files together. **Selected by the user.**

The following implementation choices are proposed by the agent, rather than separately requested by the user.

## Release identity and selection

CI embeds explicit channel and exact release tag in official builds, alongside the displayed package version and commit. Compute the nightly tag before compilation and reuse it when publishing; unmarked local builds are not treated as official stable releases. Stable release tags must be valid semantic versions, optionally prefixed by `v`; select the highest version strictly greater than the installed version, excluding drafts, prereleases, and nightly tags. Stable publication validates the tag against the Cargo package version.

Nightlies must be prereleases with the existing `nightly-YYYY-MM-DD-RUN_ID-RUN_ATTEMPT` tag format. Compare parsed numeric date/run/attempt tuples with the installed tag; the same tag or an older tag never offers an update. Ignore other prereleases and malformed tags. This also detects a rebuilt nightly at the same commit. Enumerate paginated releases rather than assuming GitHub's latest-release endpoint includes nightlies or sorts semantic versions. Only candidates with the correct platform archive and SHA-256 checksum asset are installable; select the newest complete candidate.

The workflow publishes a SHA-256 sidecar for the distribution ZIP. Downloads use HTTPS from the configured official GitHub repository and its asset delivery redirects, with status validation, timeouts, bounded archive sizes, streamed disk writes, and checksum verification. Checksums detect incomplete/corrupt downloads; they do not provide an independent publisher signature. No arbitrary update-source setting is added.

## User flow

Check in the background on startup and periodically (every six hours while running), with a manual **Check for updates** action in Settings. Show the installed version and channel. A small topbar indication leads to the update controls when an update is available or downloaded. No download/install action appears when there is no matching newer release.

Show **Download update** and the target version only when a newer matching release is available. During download, show progress and disable duplicate operations. Failures show a retryable error without losing an existing staged update. A validated, downloaded version exposes **Install downloaded update**, including its exact version. If an even newer version becomes available, offer its download while retaining the install action for the existing download until replacement succeeds. Installing does not require an internet connection. Download completion never quits or installs automatically.

The install action states that Bono will close and restart. Prepare and launch the helper in the background, and require its ready acknowledgement before initiating normal app shutdown. Startup/preparation failure leaves Bono running and the staged download intact. The helper waits for the original process to exit, with a bounded timeout (120 seconds), before changing installed files. On success it restarts Bono. On failure it records a persistent readable result; a surviving or restarted Bono surfaces the result and retry guidance. The helper must not open a console window.

## Durable staging and replacement

Use a dedicated updates subdirectory of the existing per-user preference directory. Each download has its own temporary generation; partial generations are never installable. Verify SHA-256, validate archive paths and contents, and extract into that generation before making it ready. Reject traversal, absolute paths, links/reparse points, duplicate/conflicting paths, unsupported entries, excessive extracted size, and bundles missing required executables/runtime directories. Preserve exact release identity and expected checksum in a staged manifest.

Commit a complete generation through a durable ready-manifest replacement, then delete superseded staged generations. The old generation and ready manifest remain valid on network errors, validation errors, disk-full errors, or interruption before commit. Startup restores the committed download, revalidates it before installation, and cleans orphaned partial generations in the background. Successful installation clears the downloaded marker; an already installed or older staged version is never offered for installation. Check/download/install operations are serialized across app instances using an OS-backed exclusive update lock.

## Helper and installation safety

Add a small Rust helper workspace package with no SDL/audio/model dependencies, built and included by `xtask dist`. Copy the helper outside the installation directory before executing it so its own installed copy can be replaced. Pass an explicit operation manifest through a file, not shell-generated commands. Use documented process and filesystem APIs. The helper independently validates its manifest, source generation, destination, and release channel before mutation; no recursive deletion or write may escape the resolved dedicated staging, backup, or installation roots.

Update the full distribution, including runtime DLLs and bundled simulator plugins. Track distribution-owned paths in a packaged bundle manifest; preserve unrelated files in the installation directory. Back up affected existing files before replacing any; remove obsolete distribution-owned files only when identified by the previous bundle manifest. An incomplete installation restores backed-up files and removes newly introduced files. Keep a recovery journal and backup until the replacement is committed so interruption can be recovered before the next installation. Failure to restart does not roll back an otherwise committed installation, but is reported. Never silently elevate: unwritable installation directories yield an actionable error before Bono exits.

Recovery must work even if `bono.exe` cannot launch. Preserve the copied helper as `Bono Update Recovery.exe` alongside its operation manifest in the per-user updates recovery directory. Launching that helper without arguments recovers the adjacent interrupted operation and restarts the restored Bono, or records an actionable failure without further mutation. Before changing installed files, write `BONO-UPDATE-RECOVERY.txt` in the installation directory with the exact recovery executable path and instructions; the helper and this pointer remain available after interruption. Package documentation explains this entry point. Recovery execution takes the same exclusive lock and validates the journal and paths again. Test recovery from interruption after each file-mutation phase, including when the app executable is missing.

## Responsiveness and boundaries

All network requests, cache discovery, download writes, hashing, extraction, process spawning, filesystem validation, installation preparation, and waiting occur on async I/O tasks or blocking workers. UI work is limited to rendering snapshots, sending commands, and nonblocking result polling. No worker joins, synchronous disk I/O, sleeps, or process waits are introduced on the UI thread. Use brief lock scopes and existing runtime/task patterns, without assuming undocumented library behavior. UI strings follow the existing localization system.

## Validation and acceptance

Tests cover stable/nightly isolation, exact-current and older versions, semantic-version ordering, nightly numeric ordering, malformed tags, missing assets, pagination, and unsupported builds. Local HTTP fixtures cover API errors, timeouts, corrupt/truncated downloads, checksum mismatch, and progress/result transitions. Filesystem tests prove failed replacement downloads preserve the old ready generation and successful replacement removes it only after commit; restart restores the staged version and stale versions cannot install. Helper tests use temporary fake installations for complete replacement, preservation of unrelated files, obsolete owned-file removal, rollback, recovery, invalid paths, concurrent-operation exclusion, and timeout/error handling. Verify helper handoff requests normal shutdown only after acknowledgement.

Before building on Windows choose one short writable absolute Cargo target path unique to this checkout and preserve the Ninja configuration. Run ordinary application tests with `cargo test -p bono --no-default-features`, plus helper tests and formatting checks. No CUDA/Vulkan behavior changes are intended. Report native dependency/SDK or sandbox blockers accurately. Document an end-to-end manual check with two packaged versions, including separate download/install, app restart, offline install, and retaining an old download after a failed new one.
