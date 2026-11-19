#include "PinnedFonts.h"
#include <AK/Vector.h>
#include <AK/NeverDestroyed.h>
#include <LibWeb/Platform/FontPlugin.h>
#include <LibGfx/Font/Font.h>
#include <LibGfx/Font/FontDatabase.h>

namespace Navigator {

struct FontSource {
    StringView family;
    u16 weight;
    bool italic;
    ReadonlyBytes bytes;
};

#include "PinnedFaces.inc"

class PinnedFonts final : public Gfx::SystemFontProvider {
public:
    AK_ALLOC_WITH_KMALLOC;
    ErrorOr<void> load()
    {
        for (auto const& source : sources)
            TRY(m_faces.try_append(TRY(Gfx::Typeface::try_load_from_temporary_memory(source.bytes))));
        return {};
    }

    ReadonlyBytes bytes_for(Gfx::Typeface const& typeface) const
    {
        for (size_t i = 0; i < m_faces.size(); ++i) {
            if (m_faces[i].ptr() == &typeface)
                return sources[i].bytes;
        }
        return {};
    }

    void for_each_typeface_with_family_name(FlyString const& family, Function<void(Gfx::Typeface const&)> visit) override
    {
        for (size_t i = 0; i < m_faces.size(); ++i) {
            if (sources[i].family.equals_ignoring_ascii_case(family))
                visit(*m_faces[i]);
        }
    }

    RefPtr<Gfx::Font> get_font(FlyString const& family, float size, unsigned weight, unsigned, unsigned slope,
        Optional<Gfx::FontVariationSettings> const& variations, Optional<Gfx::ShapeFeatures> const& features) override
    {
        auto face = match(family, weight, slope);
        if (!face)
            return {};
        return face->font(size, variations.value_or({}), features.value_or({}));
    }

    RefPtr<Gfx::Font> get_font_for_code_point(u32 cp, float size, u16 weight, u16, u8 slope, bool) override
    {
        for (auto family : { "IBM Plex Sans"sv, "DejaVu Sans"sv, "IBM Plex Mono"sv, "DejaVu Sans Mono"sv }) {
            auto face = match(family, weight, slope);
            if (face && face->glyph_id_for_code_point(cp))
                return face->font(size);
        }
        return {};
    }

    Optional<FlyString> resolve_generic_family(StringView name, u16, u8) override
    {
        if (name == "monospace"sv || name == "ui-monospace"sv)
            return "IBM Plex Mono"_fly_string;
        if (name == "sans-serif"sv || name == "serif"sv || name == "system-ui"sv || name == "cursive"sv || name == "fantasy"sv)
            return "IBM Plex Sans"_fly_string;
        return {};
    }

private:
    RefPtr<Gfx::Typeface> match(StringView family, unsigned weight, unsigned slope) const
    {
        RefPtr<Gfx::Typeface> best;
        unsigned best_score = 3;
        for (size_t i = 0; i < m_faces.size(); ++i) {
            auto const& source = sources[i];
            if (!source.family.equals_ignoring_ascii_case(family))
                continue;
            unsigned score = (source.italic != (slope != 0) ? 2 : 0) + (source.weight != (weight > 500 ? 700 : 400));
            if (!best || score < best_score) {
                best = m_faces[i];
                best_score = score;
            }
        }
        return best;
    }

    Vector<NonnullRefPtr<Gfx::Typeface>> m_faces;
};

static PinnedFonts const* s_installed_fonts;

ErrorOr<void> install_pinned_fonts()
{
    auto& database = Gfx::FontDatabase::the();
    if (database.has_system_font_provider())
        return Error::from_string_literal("Navigator requires exclusive ownership of the LibWeb font provider");
    auto provider = TRY(try_make<PinnedFonts>());
    TRY(provider->load());
    auto* pinned = provider.ptr();
    auto& installed = database.install_system_font_provider(move(provider));
    s_installed_fonts = pinned;
    static NeverDestroyed<Web::Platform::FontPlugin> plugin(false, &installed);
    Web::Platform::FontPlugin::install(*plugin);
    return {};
}

}

extern "C" u8 const* navigator_pinned_sfnt(void const* font, size_t* length)
{
    *length = 0;
    if (!font || !Navigator::s_installed_fonts)
        return nullptr;
    auto bytes = Navigator::s_installed_fonts->bytes_for(static_cast<Gfx::Font const*>(font)->typeface());
    *length = bytes.size();
    return bytes.data();
}
