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

## The key ceremony: root keys on hardware tokens (opifex.md §5.1, §14.9)

The three root keys live on three hardware tokens; any two sign. A YubiKey 5 does it
through its PIV application (Yubico's `ykman` and `libykcs11`: `brew install ykman yubico-piv-tool`).

1. On each YubiKey, make a P-256 key in slot 9a that never leaves the key, with a
   certificate so PKCS#11 shows its public key, and a touch for every signature:

   ```
   ykman piv access change-pin            # and change-puk, change-management-key --protect
   ykman piv keys generate --algorithm ECCP256 --pin-policy once --touch-policy always 9a root1.pub.pem
   ykman piv certificates generate --subject "CN=Atrium root 1" 9a root1.pub.pem
   ```

2. Name each key in a key file (no secret in it) and keep the public keys:

   ```
   M=$(brew --prefix)/lib/libykcs11.dylib
   opifex token list --module $M                       # serials and key ids (9a is id 01)
   opifex token ref --module $M --serial <serial> --id 01 --out root1.key --pub root1.pub.pem
   ```

3. Write the root and sign it with two of the tokens (each asks for its PIN and a touch):

   ```
   opifex catalog root-new site --name Atrium --root root1.key --root root2.key --root root3.key \
       --catalog catalog.pub.pem --platform platform.pub.pem
   opifex catalog root-sign site --key root1.key
   opifex catalog root-sign site --key root2.key
   ```

   `site/root.toml` also goes into the base system as `/etc/atrium/opifex/root.toml`.
   Keep the three tokens in three places: losing two before a handover leaves no way to sign.

**Adding admins or handing over.** Trust follows the current `root.toml`. Write the next
one with the new set of root keys (an admin's own token in one slot, or three new owners'
tokens), have two of the old keys and two of the new sign it, and publish:

```
opifex catalog root-new site --next yes --name Atrium --root a.key --root b.key --root c.key \
    --catalog catalog.pub.pem --platform platform.pub.pem
opifex catalog root-sign site --key root1.key   # old
opifex catalog root-sign site --key root2.key   # old
opifex catalog root-sign site --key a.key       # new
opifex catalog root-sign site --key b.key       # new
opifex catalog publish site --key catalog.pem
```

Machines adopt it at their next sync; from then on the old keys alone change nothing. A new
catalog or platform key goes in the same way. A replaced platform key is not dropped: `root-new
--next yes` keeps it with `until` set to the log's current size, so what it signed before still
checks and anything after is revoked. If it was stolen, add `--retire-at <entry>` with the
first entry you don't trust; machines then drop the thief's releases and tell anyone who
installed one (opifex.md §5.1.1). `scripts/opifex-token-check.sh` rehearses all of
this on software tokens.

## Signing a release built elsewhere (opifex.md §10.4)

The platform key can live on a fourth token on the key holder's own machine while builds run
remotely. Only the manifests travel:

```
# on the builder, and again on a second independent build
opifex sign --list out/*/ > list.txt
# locally, with the manifest folders copied over (no archives needed)
opifex sign out/*/ --key platform.key --expect list.txt
# copy each out/<pkg>/atrium.toml.sig back to the builder, which publishes
```

`--expect` refuses to sign anything if either build differs from the list.

