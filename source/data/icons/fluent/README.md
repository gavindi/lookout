# Fluent icon pack (optional)

This directory holds the aliased, monochrome SVGs that make up Lookout's
optional **Fluent** icon pack (OUTLOOK_PARITY.md, UX-80). The default remains
the desktop's GNOME icon theme; the pack is a partial theme named
`Lookout-Fluent`, selected from the Mail **View** ribbon's **Icons ▾** menu or
Config → Appearance → **Icon style**.

- `manifest.tsv` — the source of truth: `<Lookout icon name>`, the Fluent
  asset name, and `regular`/`filled`. Each alias is named after an icon name
  Lookout already requests, so enabling the pack needs no call-site changes.
- `scalable/actions/` — generated aliases. Do not edit by hand.
- `LICENSE` — Microsoft's MIT licence for the Fluent System Icons.

Regenerate after editing the manifest:

```sh
python3 tools/fluent-icons.py --fetch
python3 tools/fluent-icons.py --check
```

The import is pinned to `463d58b8186f874ab2f51cf0a08b4ff5097c912f` of
[microsoft/fluentui-system-icons](https://github.com/microsoft/fluentui-system-icons).
The generated `index.theme` is created at runtime (see
`source/crates/app/src/icon_style.rs`) because it inherits the user's current
GNOME theme; only the SVG aliases are bundled in the GResource.
