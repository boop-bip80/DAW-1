# Vendored crates

Permissively licensed crates we carry small patches for. Each directory is the published crate
unchanged except for patches marked `ArtCraft patch:`. Remove the vendored copy once upstream
releases the fix.

| Crate | Version | Licence | Patch |
|---|---|---|---|
| wgpu-hal | 30.0.1 | MIT OR Apache-2.0 | Metal: `supported_msl_version` compiles a trivial kernel at the OS-chosen MSL version and steps down until the device's compiler accepts one. Legacy Intel/NVIDIA GPUs (stock or OpenCore-patched Intel Macs) rejected `-std=metal3.2`, so device creation failed (storytold/photocraft#1859, storytold/artcraft#2126, storytold/artcraft#2190). Upstream: [gfx-rs/wgpu#10592](https://github.com/gfx-rs/wgpu/issues/10592). |
