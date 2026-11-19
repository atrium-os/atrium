#include "CheckConfinement.h"
#include "PinnedFonts.h"
#include <LibGfx/Font/Font.h>
#include <LibWeb/Platform/FontPlugin.h>
#include <harfbuzz/hb.h>

int main()
{
    if (!Navigator::enter_fixture_capability_mode())
        return 1;
    if (Navigator::install_pinned_fonts().is_error())
        return 2;
    auto family = Web::Platform::FontPlugin::the().generic_font_name(Web::Platform::GenericFont::Monospace);
    if (family != "IBM Plex Mono"_fly_string)
        return 3;
    auto font = Gfx::FontDatabase::the().get(family, 12, 400, Gfx::FontWidth::Normal, 0);
    if (!font || font->family() != family)
        return 4;
    auto* buffer = hb_buffer_create();
    hb_buffer_add_utf8(buffer, "beta", 4, 0, 4);
    hb_buffer_guess_segment_properties(buffer);
    hb_shape(font->harfbuzz_font(), buffer, nullptr, 0);
    unsigned count = 0;
    auto* glyphs = hb_buffer_get_glyph_infos(buffer, &count);
    auto* positions = hb_buffer_get_glyph_positions(buffer, nullptr);
    if (count != 4)
        return 5;
    for (unsigned i = 0; i < count; ++i) {
        if (!glyphs[i].codepoint || glyphs[i].codepoint != font->glyph_id_for_code_point("beta"[i]) || positions[i].x_advance <= 0)
            return 6;
    }
    hb_buffer_destroy(buffer);
    printf("monospace=IBM-Plex-Mono shaped=beta glyphs=%u\n", count);
}
