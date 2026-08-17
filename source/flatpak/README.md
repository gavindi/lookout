# Flatpak packaging

`io.github.gavindi.Lookout.json` is the Flatpak manifest, built by the `flatpak` job of `.github/workflows/build.yaml` (pushed to the `build` branch). This document records what's confirmed and what's still needed to build the bundle locally.

## What's confirmed

The `finish-args` GOA permission (`--talk-name=org.gnome.OnlineAccounts`) is verified against [Geary's own Flatpak manifest](https://github.com/flathub/org.gnome.Geary) (a real, shipping GOA-based mail client), not guessed. Geary also talks to `--talk-name=org.gnome.ControlCenter` to deep-link into account settings, which this manifest includes too.

The manifest installs the desktop file, AppStream metainfo, icon, and GSettings schema (compiled with `glib-compile-schemas`), and the GOA settings deep-link works sandboxed: `online_accounts.rs` activates `org.gnome.Settings`' `launch-panel` action (GNOME 48+) or `org.gnome.ControlCenter`'s `ActivatePanel` (≤47) over D-Bus, only falling back to spawning `gnome-control-center` when neither answers - the Flatpak build is granted `--talk-name` for both.

## What's generated

1. **`cargo-sources.json`** - Flatpak builds offline, so every crate dependency needs to be pre-vendored and checksummed. CI generates it from the tracked `source/Cargo.lock` with Flathub's [`flatpak-cargo-generator.py`](https://github.com/flatpak/flatpak-builder-tools/tree/master/cargo) (`build.yaml`'s flatpak job), the same way it's done locally:
   ```
   python3 flatpak-cargo-generator.py /path/to/source/Cargo.lock -o source/flatpak/cargo-sources.json
   ```

## To build the bundle locally

`flatpak-builder` is not part of this repo's dev environment, and building also needs the `org.gnome.Sdk//49` + `org.freedesktop.Sdk.Extension.rust-stable` runtimes installed (`flatpak install org.gnome.Sdk//49 org.freedesktop.Sdk.Extension.rust-stable//49`). Once those exist:

```
flatpak-builder --user --install build-dir source/flatpak/io.github.gavindi.Lookout.json
flatpak run io.github.gavindi.Lookout
```

Then confirm GOA accounts are actually visible from inside the sandbox (the real point of this packaging): if `--talk-name=org.gnome.OnlineAccounts` isn't sufficient in practice, account discovery will fail silently or with a D-Bus `AccessDenied`/`ServiceUnknown` error at the `GoaClient::connect()`/`list_mail_accounts()` call in `lookout-goa`.
