# The Atrium catalog (template for `atrium-os/catalog`)

This folder is what the `atrium-os/catalog` repository holds (docs/spec/opifex.md §6.4,
§10.1). Copy it to the root of that repository.

- `site/` is the catalog as served on GitHub Pages (`https://atrium-os.github.io/catalog/`):
  `root.toml` and `root.toml.sig` (from the key ceremony), `head`, `head.sig`, `entries/`, and
  `sdk/` (the SDK crate registry). Machines fetch it whole and check everything.
- `incoming/*.toml` is a release request (see `publish.sh`). Archives are uploaded to GitHub
  Releases of `atrium-os/catalog-blobs` first.
- `keys/platform/*.pem`: the platform public keys, for checking the SDK registry's signature.
- Secrets: `CATALOG_KEY`, the catalog key's PKCS#8 PEM. Root keys never touch CI. The platform
  key signs packages and the SDK index at release, never here.
- Settings: Pages from GitHub Actions; branch protection on `main` (reviews), so a release
  lands only through a reviewed pull request.
