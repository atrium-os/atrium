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

## Running a catalog of your own (opifex.md §6.5)

The same folder, workflow and `publish.sh` run any publisher's catalog. A third-party catalog
carries apps only, never `org.atrium.` ids, and only ids under domains its publishers proved.

1. Make the catalog and its keys (keep `keys/` out of the repository):

   ```
   opifex catalog init site --name "Example Apps" --keys ~/catalog-keys
   ```

   This writes `site/root.toml`, signed by 2 of 3 new root keys, and a first head. Move
   `root1.pem`..`root3.pem` offline: any two replace keys and recover a publisher that lost
   its key. `catalog.pem` becomes the repository's `CATALOG_KEY` secret.

2. Register each publisher (its own P-256 key; Arca or `openssl genpkey` both work):

   ```
   opifex catalog register site --name "Example Ltd" --domain example.com \
       --key example.pub.pem --sign example.pem
   ```

   It prints what the publisher puts on each domain: a TXT record
   `_atrium-publisher.example.com` or a file `https://example.com/.well-known/atrium-publisher`,
   both containing `atrium-publisher pub:…`. Every machine checks it itself before it offers
   the publisher's apps.

3. Publish: the publisher signs its bundle (`opifex pack <dir> --sign example.pem`), archives
   it (`opifex archive`), uploads the archive anywhere on https, and opens a pull request adding
   `incoming/<name>.toml` (archive URL, sha256, arch). The workflow checks it; on merge it is
   appended and the head signed. `opifex catalog add` finds the publisher by its signature.

4. Keys change by record: `opifex catalog publisher site rotate --id pub:… --key new.pub.pem
   --sign old.pem --sign new.pem`; a lost key is replaced with `recover`, signed by two root
   keys and the new key, and every machine shows people the recovery. `scope` changes the name
   or domains (new domains need their proof too).

5. People add it from a link or Curia with a press on the install sheet
   (`opifex_sheet::add_catalog("https://<you>.github.io/<repo>/")`); an administrator can add
   it for everyone with `opifex follow <url> <root sha256> --everyone`.
