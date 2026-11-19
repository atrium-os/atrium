#include "CheckConfinement.h"
#include <sys/socket.h>
#include "EditorPage.h"
#include "PinnedFonts.h"
#include <LibCore/EventLoop.h>
#include <LibURL/Parser.h>
#include <LibWeb/Loader/LoadRequest.h>
#include <LibWeb/Loader/ResourceLoader.h>
#include <LibGfx/SystemTheme.h>
#include <LibWeb/Bindings/MainThreadVM.h>
#include <LibWeb/Platform/EventLoopPlugin.h>

static constexpr char fixture[] = {
#embed "editor.html"
};

extern "C" bool navigator_fixture_channel(int);

// The I0 loader completes its one bounded broker request synchronously.
static ByteBuffer document_body;
static bool document_ok;

static bool fetch_fixture(Web::Page& page)
{
    auto& heap = GC::Heap::the();
    Web::ResourceLoader::initialize(heap);
    Web::LoadRequest request(HTTP::HeaderList::create());
    request.set_url(URL::Parser::basic_parse("https://editor.example/editor.html"sv));
    request.set_page(page);
    Web::ResourceLoader::the().load(request,
        GC::create_function(heap, [](Web::Fetch::Engine::NetworkTransportHandle*, HTTP::HeaderList const&, Optional<u32>, Optional<String> const&, Requests::CacheState) {}),
        GC::create_function(heap, [](Web::Fetch::Engine::NetworkTransportHandle*, Requests::ResponseData data) { document_body = MUST(ByteBuffer::copy(data.bytes())); }),
        GC::create_function(heap, [](Core::ImmutableBytes) {}),
        GC::create_function(heap, [](bool ok, Requests::RequestTimingInfo const&, Optional<StringView>, Optional<Web::ResourceLoader::NetworkRequestEnd> const&) { document_ok = ok; }));
    return document_ok && document_body.size() == sizeof(fixture) && !memcmp(document_body.data(), fixture, sizeof(fixture));
}

int main(int argc, char** argv)
{
    bool embedded = argc == 2 && strcmp(argv[1], "--limen") == 0;
    int type = 0;
    socklen_t length = sizeof(type);
    if (argc == 1 && getsockopt(STDIN_FILENO, SOL_SOCKET, SO_TYPE, &type, &length) == 0)
        embedded = true;
    if (argc != 1 && !embedded)
        return 1;
    if (embedded) {
        int channel = dup(STDOUT_FILENO);
        if (channel < 0 || dup2(STDERR_FILENO, STDOUT_FILENO) < 0 || !navigator_fixture_channel(channel))
            return 1;
    }
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
    StringView html { fixture, sizeof(fixture) };
    if (embedded) {
        if (!fetch_fixture(client->page())) {
            warnln("Fixture broker GET failed");
            return 4;
        }
        html = StringView(document_body.bytes());
        outln("nuntius=GET fixture_matches=1 bytes={}", html.length());
    }
    auto result = client->load_fixture(html, embedded);
    if (result.is_error()) {
        warnln("LibWeb fixture failed: {}", result.error());
        return 4;
    }
    return 0;
}
