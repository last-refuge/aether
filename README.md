# Aerie

Google Drive for Linux. Your files, everywhere.

Aerie is a native Linux Google Drive app built with Rust, GTK4 and libadwaita. All Drive access runs on your computer and talks directly to Google; there is no Aerie-hosted backend.

The project is in early development. See the [implementation plan](llm-docs/implementation-plan.md) and [brand guidelines](llm-docs/brand-guidelines.md).

## Building

Requires Rust 1.92+, GTK 4.20+, libadwaita 1.6+ and Pango 1.56+. On Arch Linux:

```sh
sudo pacman -S --needed gtk4 libadwaita rustup
cargo build --workspace
./target/debug/aerie
```

## Signing in

Aerie reads its Google OAuth client from `~/.config/aerie/oauth-client.json`. Until a public client ships with Aerie, create a Desktop app OAuth client in a Google Cloud project and save its JSON there. Then choose **Connect Google Drive**.

Aerie stores the sign-in in the system keyring and caches Drive metadata in `~/.local/state/aerie/`. Choose **Sign Out** in the main menu to remove both. Your files on Drive are untouched.

## Syncing folders

Open **Sync** in the sidebar, choose an empty local folder (Aerie suggests `~/Aerie`), and pick folders from the top of My Drive. Edits, renames and deletions then flow both ways. Deletions go to the trash on the other side. When both sides changed a file, Aerie keeps both. Sync runs in a small background service, `aerie-service`, which the app starts and which keeps syncing after you close the window. It exits on its own when there's nothing to sync. Pause or stop syncing on the Sync page to stop it.

To open a drive’s synced files in your file manager, click the folder button in the header or right-click the drive in the sidebar and choose **Open local folder**. This also works while syncing is paused.

## Spikes

`cargo run -p aerie-drive --example drive_spike` exercises sign-in, listing and change tracking from the terminal (add `-- --watch` to follow changes). `cargo run -p aerie-drive --example write_spike` runs upload and overwrite tests in an "Aerie test" folder, with access only to files Aerie creates. `cargo run -p aerie-sync --example sync_spike` runs live sync scenarios there.

## Layout

| Crate | Responsibility |
| --- | --- |
| `aerie-app` | GTK application, views and components |
| `aerie-service` | Background service that runs the engine for the app |
| `aerie-engine` | Sign-in, change polling, transfers and sync passes |
| `aerie-core` | Shared domain models and commands |
| `aerie-drive` | OAuth and Drive API adapter |
| `aerie-sync` | Reconciliation and transfer scheduling |
| `aerie-store` | SQLite metadata cache, schema and migrations |

## License

Aerie's code is [MIT](LICENSE). Bundled assets keep their own licenses: Inter and JetBrains Mono under the SIL Open Font License (`assets/fonts`), and Lucide icons under ISC (`assets/icons/LICENSE-lucide.txt`; refresh them with `scripts/import-lucide.py`).
