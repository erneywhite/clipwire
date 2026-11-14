<p align="center">
  <img src="docs/brand/clipwire-icon.svg" width="128" alt="Clipwire logo">
</p>

<h1 align="center">Clipwire</h1>

<p align="center">
  <b>A shared clipboard for your computers, through your own server</b><br>
  <i>Copy here, paste there.</i>
</p>

<p align="center">
  <a href="https://github.com/erneywhite/clipwire/actions/workflows/ci.yml"><img src="https://github.com/erneywhite/clipwire/actions/workflows/ci.yml/badge.svg" alt="CI"></a>
  <a href="https://github.com/erneywhite/clipwire/releases/latest"><img src="https://img.shields.io/github/v/release/erneywhite/clipwire?label=release" alt="Latest release"></a>
  <a href="LICENSE"><img src="https://img.shields.io/badge/license-AGPL--3.0-blue" alt="License: AGPL-3.0"></a>
  <a href="https://ko-fi.com/erneywhite"><img src="https://img.shields.io/badge/Ko--fi-support-ff5e5b?logo=ko-fi&logoColor=white" alt="Ko-fi"></a>
</p>

<p align="center">
  <b>English</b> · <a href="README.ru.md">Русский</a>
</p>

<p align="center">
  <img src="docs/screenshots/preview.png" width="802" alt="Paste window with an image preview">
</p>

Clipwire is a free shared clipboard. Copy something on one computer, paste it on another.
Text with formatting, images, files and folders travel through a server you run yourself,
and everything is end-to-end encrypted: the server only ever sees ciphertext, and the key
lives on your devices.

The server is a single Docker container. The client keeps a searchable, encrypted history and
a small paste window that opens next to the mouse cursor. Without a server the client still
works as a local clipboard history on one computer, and you can connect a server later.

Right now there is a client for Windows. Clients for macOS and Linux are planned.

Found a bug or have an idea? Tell about it in [Issues](https://github.com/erneywhite/clipwire/issues).

## Contents

- [Getting started](#getting-started)
- [Paste window](#paste-window)
- [What gets synced](#what-gets-synced)
- [Image editor](#image-editor)
- [Devices and groups](#devices-and-groups)
- [File storage (S3 / SFTP)](#file-storage-s3--sftp)
- [Privacy and security](#privacy-and-security)
- [Updates](#updates)
- [Settings](#settings)
- [Self-hosted server](#self-hosted-server)
- [Building from source and tests](#building-from-source-and-tests)
- [Project layout](#project-layout)
- [Support](#support)
- [License](#license)

## Getting started

<p align="center">
  <img src="docs/screenshots/settings-sync.png" width="560" alt="Sync settings: create a group or join one">
</p>

1. Download `Clipwire_*_x64-setup.exe` from [Releases](https://github.com/erneywhite/clipwire/releases)
   and run it. It installs for the current user and does not need administrator rights.
2. Start Clipwire. It lives in the tray; press Ctrl+Shift+V to open the paste window.

**Without a server.** History and the paste window work on a single computer. You can connect
a server at any time: the last 50 entries of the local history go to the group, and the group's
history is merged into the local one. The clipboard itself is not touched.

**Connecting a server.** Set up the server first (see [Self-hosted server](#self-hosted-server)); it
prints a server code that looks like `cwserver1-…`.

1. On the first device open **Sync**, enter the server code and create a group.
2. Clipwire shows an invitation code, `clipwire1-…`. Enter it on your other devices.

The invitation code contains the group's encryption key. Share it only with yourself.

## Paste window

Press Ctrl+Shift+V (configurable). The window opens next to the mouse cursor.

| Keys | Action |
|---|---|
| Up, Down, mouse | select |
| Enter, double click | paste where the text cursor is (and send to your other devices) |
| Shift+Enter | paste without formatting |
| Ctrl+1…9 | paste the entry with that number |
| Space | preview (when the search field is empty) |
| Ctrl+P, click the pin on the right of a row | pin or unpin |
| Ctrl+E | edit an image |
| Delete (with an empty search), Shift+Delete, the bin icon in a row | delete from history |
| Right click, Menu key, Shift+F10 | entry menu: paste, preview, edit, pin, delete |
| Tab | switch between History and Pinned |
| Alt+Left, Alt+Right, the icons next to the tabs | filter: text, links, images, files |
| Typing | search |

In the preview you can fix text before pasting: Ctrl+Enter pastes the edited version,
Ctrl+S saves it to the history.

**Deleting.** By default a deleted entry disappears only on this device. Turn on
**Delete on all devices** under Sync and the deletion is sent to the rest of the group as well.
Devices that are switched off apply it when they connect, within the server's retention period.
Entries pinned on another device are not deleted. The server does not know what was deleted: the
deletion note is encrypted with the group key. This needs server 0.1.3 or newer.

## What gets synced

### Text and formatting

Copying from a browser, Word or a mail client keeps HTML and RTF along with the text. Pasting
inserts the formatted version; Shift+Enter (or the plain-text button in the preview) inserts just
the text, and a setting can make that the default. Such entries are marked `HTML` or `RTF` in the
list. The preview renders them in an isolated frame: scripts do not run and external images and
fonts are not loaded, so opening a message does not tell the sender that you read it. If the
formatted text is too big to send, the other devices receive it as plain text.

### Images

A copied image (a screenshot, a picture from a browser or an editor) is sent to your other
devices as lossless PNG and shown as a large thumbnail with its size and weight. Space opens it
in full. If the clipboard holds both text and an image (Excel and some editors do this), the
text is synced. Images over about 7.5 MB as PNG are not sent; the server limit for one item is
8 MiB by default.

### Files and folders

Files and folders copied in Explorer appear in the history as a card with name, size and path.
Pasting puts them back on the clipboard the way Explorer does, so you can paste them into a
folder, a messenger or an email. An image file (png, jpg, webp, gif, bmp) gets a thumbnail, a
preview and the image editor. When several files are copied, the preview lets you untick some
and paste only the rest.

The preview (Space) shows the beginning of text files (up to 64 KB), opens PDFs in a built-in
viewer and plays video (mp4, webm, mov) and audio (mp3, m4a, wav, ogg, flac) with a built-in
player. Video is read from disk in chunks, so size does not matter. PDFs and media open only if
the start of the file matches the format, so a file with a wrong extension does not open as a
page. The window can only reach files that are in the history. **Show in folder** opens
Explorer on the file.

**Sending to other devices.** Inside a group:

- A file up to the threshold (100 MB by default; set it under Sync, 0 means do not upload) is
  uploaded to the server right away, encrypted, and a card appears on every device. Each file
  gets its own key derived from the group key and is encrypted in 1 MiB parts, so the server
  sees only ciphertext.
- Pasting on another device downloads the file into a cache (`%APPDATA%\Clipwire\config\files`)
  and puts it on the clipboard as an ordinary file. Upload and download progress shows in the
  row, and uploaded files are marked **↑ on the server** for 24 hours (the mark survives a
  restart and does not depend on the server's `CLIPWIRE_TTL_HOURS`).
- Transfers resume after a dropped connection, and the upload queue survives a restart. If a
  file changes while it is being uploaded, the upload is cancelled.
- A folder is sent as one uncompressed tar archive and unpacked into the cache on the other
  device. Links are left out of the archive, and folders with more than 100,000 entries or nested
  deeper than 64 levels are not sent.
- Files and folders above the threshold are not uploaded. The receiving device fetches them
  directly from the source device over iroh while that device is online. The source first checks
  that the requester knows the group key, so outsiders get nothing. The row says "directly from
  device X", and if the source is off, the download reports that the device is not online.

Keep the devices of a group on the same version: versions from before direct transfer do not
understand the newer file entries. Protocol details are in
[docs/files-transfer.md](docs/files-transfer.md).

**Download cache.** A downloaded copy lives as long as its history entry exists; deleting the
entry, clearing the history or the retention cleanup removes it (this also runs at startup).
Under Sync, **Files** shows how much space the cache takes, and **Clear download cache** removes
everything except what is downloading right now. Removed files are downloaded again when pasted.

### Outlook attachments

An attachment copied in Outlook, and an email itself (it becomes a `.msg` file), also land in the
history as files. Other programs that put files without a path on the clipboard, such as
archivers, work the same way. Clipwire saves them to `%APPDATA%\Clipwire\config\virtual` and
treats them as regular files afterwards.

One copy takes up to 500 files with a total size not above the upload threshold (100 MB if the
threshold is 0). If it is bigger, or the source does not respond for 30 seconds, the copy is
skipped. If something else was copied while an attachment was still being read, the attachment
appears in the history but does not go to the clipboard or to other devices. Saved files are
marked as downloaded from the internet, so Windows and Office warn before opening them. They stay
while a history entry refers to them. An image copied in Firefox or Thunderbird stays an image,
even though the browser also puts a file on the clipboard.

### Link previews

When the clipboard holds a link, Space shows a card with the page title, description and image
taken from its og tags. Clipwire loads it itself instead of using a built-in browser, so the page
is never opened or executed: it reads only the first 512 KB of HTML and one image up to 2 MB,
with an 8 second timeout.

Links that look single-use (magic-link sign-in, password reset, email confirmation, signed file
links) are not loaded at all, because opening such a link can burn it. Links to this computer or
to the local network (localhost, 192.168.x.x, 10.x.x.x and the like) are not loaded either, even
after a redirect. **Open in browser** opens the link in your default browser. Previews can be
turned off under Privacy.

### Source app

Each entry shows where it was copied from: a small icon and the program name ("Notepad",
"Telegram"). The name comes from the exe's description and the 32×32 icon from the exe itself.
Search also matches the program name. The name and icon travel with the entry, so the other
devices of the group see the source too. Detection works on Windows.

## Image editor

Press Ctrl+E (or **Edit** in the preview) to annotate a screenshot and paste it straight away.

Tools: arrow (A), pen (P), marker (M), rectangle (R), ellipse (E), text in a label (T), blur (B),
for example to cover a password or an address, and crop (C). Select mode (V) moves and deletes
finished shapes. Keys 1–6 switch colours, `[` and `]` change the thickness. Shift gives a square,
a circle or an arrow at a multiple of 45°. Ctrl+Z undoes.

Ctrl+Enter pastes the result into the window you came from, Ctrl+S only saves it. The edited
picture becomes a new history entry and goes to the other devices; the original stays.

## Devices and groups

Under Sync you see the devices in the group: name, OS, a short fingerprint, whether the device is
online and when it was last seen. Every device publishes its name and OS itself in an encrypted
card signed with its key, so the server cannot read or forge them. A device running an old
Clipwire version shows up as "unknown device".

**Removing a device.** A lost or sold computer can be removed with **Remove**. The group key is
replaced, the remaining devices get the new key on their own (switched-off ones when they come
online, within 30 days), and the removed device loses access to the server, the files and direct
transfer. On it, syncing stops and Sync offers **Leave group** and **Try again**. Its key and
history are not wiped automatically, because the server can say "you were removed" without a key
change, and what was already on the device stays there. A new key is accepted only if it was
issued by a device that this computer has already seen in the list; after the change, settings
say which device changed it.

**Changing the group key** does the same without removing anyone, which is useful if the
invitation code ended up in the wrong place. The group's history on the server is erased, local
history is untouched and the invitation code changes. Devices without a card do not receive the
new key, and Clipwire warns about it before you confirm.

Details of the protocol are in [docs/devices.md](docs/devices.md).

## File storage (S3 / SFTP)

By default files up to the threshold go to the Clipwire server. A group can keep them in its own
storage instead: S3-compatible (AWS, MinIO, Backblaze B2, Cloudflare R2) or an SFTP server. This
helps when the server has little disk space.

Open **Sync → File storage** (the card appears only when the device is in a group) and pick
**Clipwire server**, **S3-compatible** or **SFTP**, set a retention period for files, then use
**Test connection** and save. The test writes, reads and deletes a probe object.

- The settings are encrypted with the group key and spread to the other devices through the
  server, which never sees the credentials. The same ciphertext that would go to the server goes
  to the storage.
- S3 works only over HTTPS; plain HTTP is allowed for `localhost` and local network addresses.
- For SFTP the host key is checked against a fingerprint you confirm during the first test, and
  the password is never sent to an unknown host. A private key can be pasted or chosen as a file.
- Saved secrets (secret key, password, private key) are never shown again. An empty field means
  "keep as is".
- If the storage is unreachable, the file goes through the Clipwire server and a warning appears.
- Devices delete old files themselves, at most once an hour, by the retention period and by the
  storage's own clock. For S3 it is worth adding the same lifecycle rule to the bucket.
- If a device is removed from the group, or some devices have no card when the key changes, they
  still hold access to the storage. Clipwire then asks you to rotate the S3 key or the SSH
  password and key.

Full description: [docs/storage.md](docs/storage.md).

## Privacy and security

- Everything is encrypted end to end. The server sees only ciphertext; the key exists only on
  your devices.
- The local history is stored in SQLite, and every entry is encrypted with a local key protected
  by DPAPI.
- Content that password managers mark as private is never synced. Programs can also be excluded
  from syncing (password managers are on the list by default).
- Files downloaded from other devices are marked as coming from the internet, so Windows warns
  before running them.
- An item copied on another device more than 10 minutes ago (for example one that arrived late)
  goes to the history only and does not touch the clipboard.
- Link cards do not execute the page and do not load single-use links (see
  [Link previews](#link-previews)).
- Leaving a group also deletes the cache of downloaded files.
- Settings and keys live in `%APPDATA%\Clipwire`.

## Updates

Clipwire updates itself quietly. Every 6 hours it checks GitHub releases and downloads a new
version in the background. It installs without an installer window once all Clipwire windows are
closed, no files are being transferred (direct transfers included), sync is not paused, and
neither the program nor the clipboard was used for a couple of minutes. If it did not manage
before you quit, it installs at the next start. After an update you get a notification, and a
paused sync stays paused. If an installation fails, you are told, and Clipwire does not retry
that version on its own, only from the button.

The update is checked against a digital signature, and the version number in the server's reply
is compared with the version inside the signature, so a tampered file or an old installer under a
new number will not install.

**Update automatically** under General can be turned off: updates are then installed only from
the button in About or from the tray menu. More in [docs/updates.md](docs/updates.md).

## Settings

<p align="center">
  <img src="docs/screenshots/settings-general.png" width="560" alt="General settings">
</p>

- Three themes (Graphite, Paper, Signal), each in light and dark, or follow the system.
- Language: English, Русский or as in the system.
- Launch at login: the program starts minimized to the tray.
- Copy sound: a short click when something new enters the history. Pasting from the history and
  copying the same thing again are silent. Windows only.
- History retention, from a day to forever (pinned entries are kept forever).
- Device name, invitation code (**Show code**), leaving the group.
- Paste window size (one slider with a live preview next to it) and the hotkey.
- Programs that must not be synced (password managers by default).
- Hiding Clipwire windows during screen sharing.
- Link previews on or off, formatted text always plain, update settings.

The **About** page shows the version and the update check, and below it a status block: the
connection to the server and how it runs (direct over UDP or through the relay, with latency),
when an item last went out and came in, the group's file storage, how many entries the history
has on this device and how much space downloaded files take, how many items and files of the
group sit on the server, how much of the server's quota for files is used
(`CLIPWIRE_BLOB_QUOTA`), and how many devices are in the group and how many are online. Server
numbers need server 0.1.2 or newer. The Ko-fi button is there too.

## Self-hosted server

The server is one container with the iroh relay built in. It needs HTTPS (a reverse proxy in
front of the relay) and, optionally, a UDP port for direct QUIC connections. Without UDP
everything works through the relay, only slightly slower.

GitHub Actions builds the image: `ghcr.io/erneywhite/clipwire-server:latest` (the latest `main`)
and `:X.Y.Z` for releases.

**Through a Docker control panel or plain `docker compose`:**

1. Create a compose project from [deploy/docker-compose.image.yml](deploy/docker-compose.image.yml),
   set `CLIPWIRE_PUBLIC_URL` to your domain and start it.
2. Create a reverse proxy for that domain pointing to `http://127.0.0.1:3340`, with SSL and
   WebSocket enabled. If your proxy has no WebSocket switch, add the block from
   [deploy/nginx-clipwire.conf](deploy/nginx-clipwire.conf) to the site config.
3. Optionally open UDP 7843 in the firewall.
4. Optionally hide the iroh-relay status page at the domain root: add a `location = /` with a
   redirect to the proxy config. An example is at the end of
   [deploy/nginx-clipwire.conf](deploy/nginx-clipwire.conf).
5. Find the `server code: cwserver1-…` line in the container log (Clipwire's own log line is in
   Russian: `код сервера: cwserver1-…`). The first device connects with it. `clipwire-server
   info` prints the same.

**Updating:** pull the new image and recreate the container.

**Checking from outside:** `cargo run -p clipwire-server --example probe -- cwserver1-…` connects
two clients through the relay, sends an encrypted item between them and shows which path the
packets took.

**Building the image yourself:** run `docker compose up -d --build` in `deploy`
([deploy/docker-compose.yml](deploy/docker-compose.yml)).

The server key, the relay token and the database live in the Docker volume `clipwire-data`. If
you delete the key, the server's address changes and every device has to be connected again.

### Environment variables

| Variable | Default | Meaning |
|---|---|---|
| `CLIPWIRE_PUBLIC_URL` | required | public address, for example `https://clip.example.com` |
| `CLIPWIRE_DATA_DIR` | `/data` in the image | key, token, database |
| `CLIPWIRE_RELAY_BIND` | `0.0.0.0:3340` | HTTP of the built-in relay |
| `CLIPWIRE_QUIC_BIND` | `0.0.0.0:7843` | UDP for direct connections |
| `CLIPWIRE_RELAY_TOKEN` | generated | relay access token, if you want your own (up to 256 characters) |
| `CLIPWIRE_MAX_ITEM_BYTES` | `8388608` | limit for one encrypted item (8 MiB, enough for images) |
| `CLIPWIRE_HISTORY_LEN` | `50` | how many latest items to keep per group |
| `CLIPWIRE_TTL_HOURS` | `24` | how long to keep an item |
| `CLIPWIRE_SELF_RELAY_URL` | `http://127.0.0.1:<relay port>` | how the server connects to its own relay |
| `CLIPWIRE_MAX_FILE_BYTES` | `104857600` | limit for one file (100 MiB) |
| `CLIPWIRE_BLOB_QUOTA` | `2147483648` | file space per group (2 GiB) |
| `CLIPWIRE_BLOB_TOTAL_QUOTA` | `21474836480` | file space for the whole server (20 GiB) |
| `CLIPWIRE_BLOB_STALL_SECS` | `120` | how long to wait for the next part of a file before an upload counts as abandoned |
| `CLIPWIRE_RESERVE_TTL_SECS` | `600` | after how long space reserved for a file is released if the upload never started |
| `CLIPWIRE_MAX_RESERVES` | `512` | how many files one device may keep reserved without a single uploaded part |
| `CLIPWIRE_MAX_GROUPS` | `100` | how many groups the server can hold (creating a group needs the server code; groups with no devices are deleted after 90 days) |
| `CLIPWIRE_HISTORY_TOTAL_BYTES` | `1073741824` | total limit of item history per server (1 GiB); the excess is evicted from the largest group |
| `CLIPWIRE_MAX_DEVICE_CONNS` | `4` | simultaneous connections from one device |

### What the server stores

- Encrypted items, for the retention period `CLIPWIRE_TTL_HOURS`, up to `CLIPWIRE_HISTORY_LEN`
  per group.
- Encrypted files in `<CLIPWIRE_DATA_DIR>/blobs`, removed after the same period.
- If a group chose its own storage (S3 or SFTP), only its encrypted settings, up to 16 KB, which
  the server hands out to the group's devices. They are deleted on a key change and together with
  the group.
- Encrypted device cards (name, OS, a key for changing the group key). Only devices can read them.
- Packets with the new key and the old device list after a removal, for 30 days, so that
  switched-off devices can still pick up their key. After that only a tombstone remains: logging
  in with the old key gets "removed" and does not create the group again. Tombstones do not count
  toward `CLIPWIRE_MAX_GROUPS`.
- Encrypted deletion notes.

The server sees ciphertext only. When a device is removed, the server immediately erases the
history and files of the old group, hands the remaining devices the new key (packed separately
for each one) and tells the removed device that it was excluded. See
[docs/devices.md](docs/devices.md) and [docs/architecture.md](docs/architecture.md).

## Building from source and tests

You need Rust stable (tested on 1.100), Node.js 22+, and on Windows the MSVC Build Tools.

```
cargo test --workspace
cd apps/desktop && npm ci && npx tauri dev
```

On macOS raise the open files limit first: the default is 256 per process, and the parallel
network tests start dozens of iroh endpoints and fail with "Too many open files":

```
ulimit -n 10240 && cargo test --workspace
```

A release is built by GitHub Actions on a `vX.Y.Z` tag ([release.yml](.github/workflows/release.yml)):
the installer, signed update files and `latest.json`. The version comes from the workspace
`Cargo.toml`.

Icons, installer images and the logo in this README are generated from the sources in
`docs/brand/` with `node apps/desktop/scripts/icons.mjs`, then
`npx tauri icon apps/desktop/icons-src/icon-1024.png` (from `apps/desktop`).

**What the tests cover.**

- `crates/clipwire-server/tests/e2e.rs` starts a real server with the relay on 127.0.0.1 and runs
  several clients through it.
- Agent tests in `crates/clipwire-agent/tests` also use a real server. `sync.rs` covers the sync
  engine; `devices.rs` covers device cards and group key changes (removing a device, a switched-off
  device signing in after a change, a change without removal, a change by an unknown device);
  `files.rs` covers file transfer (upload and download into the cache, resuming after a drop,
  downloading during an upload, server refusal, direct transfer from a device, folders through
  the server and directly); `storage.rs` covers group settings through the server and files
  through external storage (a disk folder instead of a bucket) with fallback to the server;
  `sftp.rs` covers the SFTP storage against an in-process SSH server (host key, password and key
  login, connection test, cleanup).
- The S3 signature is checked against the official AWS examples
  (`crates/clipwire-agent/src/storage/sigv4.rs`).

**Manual live tests.** `s3_live.rs` checks S3 storage on a real server: the connection test,
writing and reading, a file in several parts, a listing longer than 1000 objects, cleanup by age
and the error messages (wrong key, no bucket, no connection). The bucket must already exist;
tested on MinIO:

```bash
CLIPWIRE_S3_ENDPOINT=http://127.0.0.1:9000 CLIPWIRE_S3_BUCKET=clipwire-test CLIPWIRE_S3_ACCESS=... CLIPWIRE_S3_SECRET=... cargo test -p clipwire-agent --test s3_live -- --ignored
```

`sftp_live.rs` checks SFTP against a real OpenSSH: the host key fingerprint, password login and
key login with a passphrase, a multi-part file, cleanup, a wrong password and a foreign
fingerprint. Also tested with `MaxSessions 1`:

```bash
CLIPWIRE_SFTP_HOST=127.0.0.1 CLIPWIRE_SFTP_PORT=2222 CLIPWIRE_SFTP_USER=... CLIPWIRE_SFTP_PASSWORD=... CLIPWIRE_SFTP_PATH=/folder CLIPWIRE_SFTP_KEY=path/to/key CLIPWIRE_SFTP_KEY_PASS=... cargo test -p clipwire-agent --test sftp_live -- --ignored
```

**Frontend tests.** The pure logic of the settings window is tested without a build, in Node
23.6+ (which strips types by itself). Run from `apps/desktop`:
`node src/lib/storage.test.ts` (File storage card fields, saved secrets, host key reset),
`node src/lib/i18n.test.ts`, `devices.test.ts`, `overview.test.ts` and `transfer.test.ts`.

The agent crate also has a console client for debugging: `create`, `join`, `run` (sync without a
UI), `listen`, `send <text>`. The `CLIPWIRE_HOME` variable sets another settings directory, so
two clients can run on one machine.

## Project layout

- `apps/desktop`: the app (Tauri 2 and Svelte 5): tray icon, paste window at the cursor,
  settings window, auto-update.
- `crates/clipwire-agent`: sync engine, OS clipboard access, local history and the debugging
  console client.
- `crates/clipwire-server`: the server with the built-in iroh-relay, device groups, item delivery
  and history with a TTL.
- `crates/clipwire-core`: item format, encryption, server codes and invitations.
- `docs/`: [architecture](docs/architecture.md), [research of existing tools](docs/research.md),
  [files-transfer](docs/files-transfer.md), [devices](docs/devices.md),
  [storage](docs/storage.md), [updates](docs/updates.md) and the brand sources (`docs/brand/`).
  These documents are in Russian.
- `deploy/`: compose files and a reverse proxy example for the server.

## Support

Clipwire is free. If it saves you time, you can [support the author on Ko-fi](https://ko-fi.com/erneywhite).
Bugs and ideas go to [Issues](https://github.com/erneywhite/clipwire/issues).

## License

[AGPL-3.0](LICENSE). You may use, change and distribute Clipwire. If you publish a modified
version, including a server that people reach over a network, you must publish its source code
under the same license.

Author: Erney White
