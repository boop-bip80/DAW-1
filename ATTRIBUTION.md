# Attribution

Every asset bundled with or committed to LiVi Wave is listed here with its author, source and
license. `cargo xtask assets` fails CI when a tracked asset file has no row.

**Rule:** no Avid, Pro Tools, Adobe or other proprietary icons, images, sounds or presets — ever.
Assets must be original, public domain (CC0) or permissively licensed. See `AGENTS.md`.

## Icons

All UI icons (tools, transport, track buttons) are drawn procedurally in
`crates/ui-egui/src/icons.rs` by the LiVi Wave contributors (MIT OR Apache-2.0). No UI icon image
files are used. The app icon (`assets/app-icon/`, an AI-generated image supplied by the owner; see `assets/app-icon/README.md`) is listed under Files below.

## Audio

The demo session's audio (drums, bass, pad) and its MIDI parts are synthesised in code in
`crates/engine/src/demo.rs` by the LiVi Wave contributors (MIT OR Apache-2.0). No sample files
are bundled.

## Fonts

LiVi Wave bundles no font files. It uses the default fonts shipped inside the `egui` crate
(Ubuntu-Light: Ubuntu Font Licence 1.0; Hack: MIT; Noto Emoji: OFL-1.1; emoji-icon-font: MIT)
and, at runtime, the operating system's own UI fonts when available (never redistributed).

## Bundled egui font icons

These dependency fonts are embedded by the pinned egui font definitions; no font file is added
here. The subset uses default_fonts; the parent face uses monochrome_emoji_fonts.

| Asset | Author | Source | Licence |
|---|---|---|---|
| `epaint_default_fonts/fonts/egui-icons.ttf` | John Slegers (emoji-icon-font) | https://github.com/storytold/egui/blob/6d518fc799681571963842f95b55a6b68431b15a/crates/epaint_default_fonts/fonts/egui-icons.txt; original https://github.com/jslegers/emoji-icon-font | MIT (complete copyright and permission text in NOTICE) |
| `epaint_default_fonts/fonts/emoji-icon-font.ttf` | John Slegers | https://github.com/storytold/egui/blob/6d518fc799681571963842f95b55a6b68431b15a/crates/epaint_default_fonts/fonts/emoji-icon-font-mit-license.txt; original https://github.com/jslegers/emoji-icon-font | MIT (complete copyright and permission text in NOTICE) |

## Files

| Path | Author | Source | License |
|---|---|---|---|
| `docs/brand/artcraft-logo.svg` | ArtCraft Team | craftrules `assets/brand/` | ArtCraft brand terms (`docs/brand/LICENSE-brand.txt`) |
| `docs/brand/artcraft-logo.png` | ArtCraft Team | craftrules `assets/brand/` | ArtCraft brand terms |
| `docs/brand/artcraft-logo-white.svg` | ArtCraft Team | craftrules `assets/brand/` | ArtCraft brand terms |
| `docs/brand/artcraft-logo-white.png` | ArtCraft Team | craftrules `assets/brand/` | ArtCraft brand terms |
| `docs/brand/artcraft-mark.svg` | ArtCraft Team | craftrules `assets/brand/` | ArtCraft brand terms |
| `docs/brand/artcraft-mark.png` | ArtCraft Team | craftrules `assets/brand/` | ArtCraft brand terms |
| `docs/brand/artcraft-mark-black.svg` | ArtCraft Team | craftrules `assets/brand/` | ArtCraft brand terms |
| `docs/brand/artcraft-mark-black.png` | ArtCraft Team | craftrules `assets/brand/` | ArtCraft brand terms |
| `assets/app-icon/source/liviwave-icon-source.png` | LiVi Wave owner (generated with Google Gemini from the owner's prompt) | original; the full generated banner the icons are cropped from | MIT OR Apache-2.0, as released by the owner |
| `assets/app-icon/icon.svg` | LiVi Wave owner | cropped from the source image above (SVG wrapper around a 512 px PNG) | MIT OR Apache-2.0, as released by the owner |
| `packaging/macos/dmg/` (all files) | @XusBadia | original (macOS DMG window background and layout, from the app icon; see `packaging/macos/dmg/README.md`) | MIT OR Apache-2.0 |
| `assets/app-icon/liviwave-1024.png` | LiVi Wave contributors | cropped from the owner's generated image (see source row above) | MIT OR Apache-2.0 |
| `assets/app-icon/liviwave-macos-512.png` | LiVi Wave contributors | cropped from the owner's generated image (see source row above) | MIT OR Apache-2.0 |
| `assets/app-icon/liviwave.icns` | LiVi Wave contributors | cropped from the owner's generated image (see source row above) | MIT OR Apache-2.0 |
| `assets/app-icon/liviwave.ico` | LiVi Wave contributors | cropped from the owner's generated image (see source row above) | MIT OR Apache-2.0 |
| `assets/app-icon/hicolor/` | LiVi Wave contributors | cropped from the owner's generated image (see source row above) (16–512 px PNGs and the scalable SVG) | MIT OR Apache-2.0 |
| `docs/images/` | LiVi Wave contributors | screenshots of LiVi Wave rendered from the synthesised demo session | MIT OR Apache-2.0 |
