## Overview

A fast command-line program that synchronizes a local directory with Google Drive. It does not
use any background processes. 

Due to technical limitations of how Google Drive stores files (which
is essentially object storage), translating between file-system paths and Google Drive File IDs
requires listing a substantial amount of files. Synchronization naturally needs to determine a list
of changes first, and therefore requires listing files. This program is approximately 8x faster
than `rclone` at listing files, and therefore much more pleasant to use for everyday synchronization.

Note that the commands `push` / `pull` require explicit interactive 
confirmation of all changes by default.

## Usage

1. Create a directory, e.g. `~/gdrive`.
2. Create a Google OAUth Client at https://console.cloud.google.com/apis/credentials and download the JSON file containing its `client_id` and `client_secret`.
3. Run `push-pull-drive init google-drive <path to downloaded json file>` from within your chosen directory (e.g. `~/gdrive`) and follow the prompts.
4. Run `push-pull-drive pull` to download all files from your Google Drive. This can also be run with an optional argument to only pull a specific folder.
5. Once you made changes, run `push-pull-drive push` to apply your changes to the remote Google Drive files.

## Building

Build the program using `cargo build --release`. Copy the resulting binary somewhere, e.g. to `~/.local/bin`.

## Caveats

- This was built to support my specific use-case: Synchronize a single local directory regularly (e.g. daily) with Google Drive for backup purposes and also be to able to view the files through the Google Drive web UI (e.g. from my phone). All other use-cases probably have issues.
- Assumes a case-sensitive file system (so macOS and Windows are unlikely to work properly in all edge-cases).
- Only supports "binary" Google Drive Files. All other files (e.g. Google Docs documents) are ignored.
- Google Drive does not technically have paths (file and folder names need not be unique within a folder name and also they may contain `/`).
    - If you several files / folders of the same name in a single folder, the program will deterministically pick one of them and ignore the others.
    - If you have files or folders that contain `/` any operations on them will fail at runtime.