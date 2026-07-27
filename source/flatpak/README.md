# Flatpak packaging

`io.github.gavindi.Lookout.json` is a **spike**, not yet a working build - see the plan's own flagged risk item: "Flatpak D-Bus permission wiring needed for GOA to be reachable from a sandboxed build - verify early since it affects distributability." This resolves the permissions question; it does not yet produce a runnable Flatpak.

## What's confirmed

The `finish-args` GOA permission (`--talk-name=org.gnome.OnlineAccounts`) is verified against [Geary's own Flatpak manifest](https://github.com/flathub/org.gnome.Geary) (a real, shipping GOA-based mail client), not guessed. Geary also talks to `--talk-name=org.gnome.ControlCenter` to deep-link into account settings, which this manifest includes too.

## What's missing before this builds

1. **`cargo-sources.json`** - Flatpak builds offline, so every crate dependency needs to be pre-vendored and checksummed. Generate it with Flathub's [`flatpak-cargo-generator.py`](https://github.com/flatpak/flatpak-builder-tools/tree/master/cargo) against `source/Cargo.lock`:
   ```
   python3 flatpak-cargo-generator.py /path/to/source/Cargo.lock -o flatpak/cargo-sources.json
   ```
   Not run yet in this environment (no network-capable Python tooling available here). Given ~150 transitive dependencies (rustls, tokio, gtk4-rs, webkit6-rs, zbus, ...), expect a large generated file.
2. **`flatpak-builder` isn't installed** in this environment (`sudo apt-get install flatpak-builder`), and building also needs the `org.gnome.Sdk//49` + `org.freedesktop.Sdk.Extension.rust-stable` runtimes installed (`flatpak install org.gnome.Sdk//49 org.freedesktop.Sdk.Extension.rust-stable//49`). Neither has been attempted here.
3. **The "Open Online Accounts Settings" button won't work sandboxed as currently written.** `lookout-app/src/window.rs` spawns `gnome-control-center` via `std::process::Command`, which only works unsandboxed - a Flatpak app can't exec arbitrary host binaries. Under Flatpak this needs to go through `org.gnome.ControlCenter`'s D-Bus activation instead (hence that `--talk-name` above). Not yet implemented; the current code path is untested inside a sandbox.

## Once the above is done

```
flatpak-builder --user --install build-dir flatpak/io.github.gavindi.Lookout.json
flatpak run io.github.gavindi.Lookout
```

Then confirm GOA accounts are actually visible from inside the sandbox (the real point of this spike) - if `--talk-name=org.gnome.OnlineAccounts` isn't sufficient in practice, account discovery will fail silently or with a D-Bus `AccessDenied`/`ServiceUnknown` error at the `GoaClient::connect()`/`list_mail_accounts()` call in `lookout-goa`.
