# PhotoCraft FrameForge importer

`read_archive(bytes)` reads version 1 `frameforge-project` ZIPs in memory with
256-entry, 32 MiB per entry, 128 MiB total and 500-layer limits. Stored and raw
DEFLATE entries are accepted. Entry names, sizes, CRCs, local headers and payload
bounds are checked; inflation stops at the remaining expansion budget.

`import_into(&mut Session, &Archive, &ImportOptions)` creates a new RGB/8 document
at 72 ppi with #111111 background. It returns the document index, a layer ID map,
warnings and missing fonts. A failed import removes its partial document.
FrameForge's top-first stack is inserted bottom-first. Images remain embedded
Smart Objects, with vector masks for clipped slots. Text remains paragraph type,
with explicit authored line breaks, shrink-to-fit, vertical centring, outside
stroke and drop shadow. Unsupported layers/features produce named warnings.

The optional font resolver receives `(family, weight)` and returns TTF/OTF bytes.
No fonts or client media are included. Native UI uses installed fonts; the web
uses bundled fonts and reports missing families. Server integrations can provide
their own resolver.

CLI:

```sh
photocraft-cli convert design.frameforge render.png --fonts-dir /external/fonts
photocraft-cli convert design.frameforge editable.psd --fonts-dir /external/fonts
```

External golden comparison (outputs must be outside this checkout):

```sh
FRAMEFORGE_FIXTURES=/external/fixture-sets \
FRAMEFORGE_FONTS_DIR=/external/fonts \
FRAMEFORGE_EVIDENCE=/external/evidence \
cargo test -p photocraft-frameforge golden_fixtures -- --ignored --nocapture
```

This compares all seven CCN/Tomme fixtures and writes renders, reference/render/
abs-diff×4 panels, deterministic RGBA metrics and one editable PSD. The ordinary
suite builds synthetic archives in memory and needs no external files.
