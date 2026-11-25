#pragma once

#include <AK/Error.h>
#include <AK/StdLibExtras.h>
#include <LibCore/AnonymousBuffer.h>
#include <LibGfx/Color.h>
#include <LibGfx/Palette.h>
#include <LibGfx/SystemTheme.h>

namespace Navigator {

// The engine's system palette, built in memory (the engine reads no theme
// file). Every role is opaque: LibWeb paints a text selection with the
// palette's Selection wash (InactiveSelection while its navigable lacks
// focus), so a role left zero draws a selection that is there but unseen.
// The washes are Atrium's amber accent (pergola theme tokens); LibWeb turns
// an opaque wash translucent itself.
inline ErrorOr<Gfx::Palette> engine_palette()
{
    auto buffer = TRY(Core::AnonymousBuffer::create_with_size(sizeof(Gfx::SystemTheme)));
    auto* theme = buffer.data<Gfx::SystemTheme>();
    for (auto& color : theme->color)
        color = Gfx::Color(Gfx::Color::Black).value();
    auto set = [&](Gfx::ColorRole role, Gfx::Color color) { theme->color[to_underlying(role)] = color.value(); };
    using R = Gfx::ColorRole;
    set(R::Accent, Gfx::Color(0x9e, 0x66, 0x28));
    set(R::Selection, Gfx::Color(0xd6, 0x9e, 0x5c));
    set(R::LightSelection, Gfx::Color(0xd6, 0x9e, 0x5c));
    set(R::DarkSelection, Gfx::Color(0x9e, 0x66, 0x28));
    set(R::InactiveSelection, Gfx::Color(0xc8, 0xc2, 0xb8));
    set(R::Window, Gfx::Color(0xf4, 0xf1, 0xec));
    set(R::Button, Gfx::Color(0xf4, 0xf1, 0xec));
    set(R::Base, Gfx::Color(Gfx::Color::White));
    set(R::White, Gfx::Color(Gfx::Color::White));
    set(R::Tooltip, Gfx::Color(0xff, 0xff, 0xe1));
    set(R::PlaceholderText, Gfx::Color(0x80, 0x80, 0x80));
    set(R::ThreedShadow1, Gfx::Color(0x80, 0x80, 0x80));
    set(R::Gutter, Gfx::Color(0xf4, 0xf1, 0xec));
    set(R::Link, Gfx::Color(0x7b, 0x4f, 0x1e));
    set(R::ColorSchemeBackground, Gfx::Color(Gfx::Color::White));
    theme->flag[to_underlying(Gfx::FlagRole::IsDark)] = false;
    return Gfx::Palette(Gfx::PaletteImpl::create_with_anonymous_buffer(move(buffer)));
}

}
