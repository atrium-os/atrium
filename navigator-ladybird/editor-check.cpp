#include "CheckConfinement.h"
#include "EditorPage.h"
#include "PinnedFonts.h"
#include <LibCore/EventLoop.h>
#include <LibGfx/SystemTheme.h>
#include <LibWeb/Bindings/MainThreadVM.h>
#include <LibWeb/Platform/EventLoopPlugin.h>

static constexpr char fixture[] = {
#embed "editor.html"
};

int main()
{
    if (!Navigator::enter_fixture_capability_mode())
        return 1;
    if (Navigator::install_pinned_fonts().is_error())
        return 2;
    Core::EventLoop loop;
    Web::Platform::EventLoopPlugin::install(*new Web::Platform::EventLoopPlugin);
    Web::Bindings::initialize_agent(Web::HTML::AgentType::SimilarOriginWindow);
    auto theme = Core::AnonymousBuffer::create_with_size(sizeof(Gfx::SystemTheme));
    if (theme.is_error())
        return 3;
    auto palette = Gfx::Palette(Gfx::PaletteImpl::create_with_anonymous_buffer(theme.release_value()));
    auto client = Navigator::EditorPage::create(move(palette));
    auto result = client->load_fixture({ fixture, sizeof(fixture) });
    if (result.is_error()) {
        warnln("LibWeb fixture failed: {}", result.error());
        return 4;
    }
    puts("editor_lines=128 caret_offset=17");
    return 0;
}
