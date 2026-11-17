# The system font library's folders, and a helper that puts them in a VM.
# Source it; vm_install_fonts needs the caller's g (ssh) and put (scp).
#
#   . "$ROOT/scripts/vm-fonts.sh"; vm_install_fonts
#
# The VM copy is replaced only when the fonts changed (a stamp of their hash).
FONT_LIBRARY_DIRS="liberation carlito caladea ibm-plex-serif noto source fira roboto inter"

vm_install_fonts() {
    _stamp=$(cd "$ROOT/fonts" && find $FONT_LIBRARY_DIRS -type f | sort | xargs cat | sha256sum | cut -c1-16)
    if g "test \"\$(cat /usr/share/fonts/atrium/.stamp 2>/dev/null)\" = $_stamp"; then return 0; fi
    _t=$(mktemp -d)
    tar -C "$ROOT/fonts" -czf "$_t/fonts.tgz" README.md $FONT_LIBRARY_DIRS
    put "$_t/fonts.tgz" /tmp/atrium-fonts.tgz
    rm -rf "$_t"
    g "rm -rf /usr/share/fonts/atrium && mkdir -p /usr/share/fonts/atrium && tar -xzf /tmp/atrium-fonts.tgz -C /usr/share/fonts/atrium && chmod -R a+rX,go-w /usr/share/fonts/atrium && echo $_stamp > /usr/share/fonts/atrium/.stamp && rm /tmp/atrium-fonts.tgz"
}
