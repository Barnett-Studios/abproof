# Test fixture — attribution

A minimal 2-node slice of the RED-baseline corpus, used only by abproof's unit tests so
they run standalone (decoupled from the full corpus, which lives in `Barnett-Studios/corpus`).

- `py-add` — reproduced verbatim.
- `cpp-all-your-base` — meta + seed **minus `test/catch.hpp`** (the 668 KB vendored Catch2
  single-header is omitted; the unit tests only parse `meta.yaml`). The full node lives in the
  corpus repo.

Both are Exercism-derived, MIT © Exercism and contributors. See `Barnett-Studios/corpus`
`ATTRIBUTION.md` for the complete provenance (Exercism MIT + Catch2 BSL-1.0 + Gradle Apache-2.0).
