#include "CheckConfinement.h"
#include <sys/socket.h>
#include <sys/stat.h>
#include "EditorPage.h"
#include "PinnedFonts.h"
#include <LibCore/EventLoop.h>
#if defined(ATRIUM_I0_SIGN_IN) || defined(ATRIUM_I1_COUNTER)
#include "SignInFixture.h"
#endif
#include <LibURL/Parser.h>
#include <LibWeb/Loader/LoadRequest.h>
#include <LibWeb/Loader/ResourceLoader.h>
#include <LibGfx/SystemTheme.h>
#include <LibWeb/Bindings/MainThreadVM.h>
#if defined(ATRIUM_MAIL_PAGE) || defined(ATRIUM_I4_VIDEO) || defined(ATRIUM_I4_YOUTUBE) || defined(ATRIUM_I6_PAGE)
#include <LibWeb/Fetch/Fetching/FetchEngine.h>
#include <LibWeb/HTML/Scripting/Agent.h>
#endif
#include <LibWeb/Platform/EventLoopPlugin.h>

static constexpr char fixture[] = {
#if defined(ATRIUM_I4_YOUTUBE)
0
#elif defined(ATRIUM_I4_VIDEO)
#embed "local-player.html"
#elif defined(ATRIUM_I1_COUNTER)
#embed "counter.html"
#elif defined(ATRIUM_I0_SIGN_IN)
#embed "sign-in.html"
#else
#embed "editor.html"
#endif
};

extern "C" NavigatorScene* navigator_counter_scene_create();
extern "C" bool navigator_fixture_channel(int);
extern "C" int navigator_standby_fetch();
extern "C" size_t navigator_fixture_get(u8 const*, size_t, u8*, size_t);
extern "C" size_t navigator_live_page(u8*, size_t, size_t*, u8*, size_t);
#if defined(ATRIUM_I4_VIDEO) || defined(ATRIUM_I4_YOUTUBE)
extern "C" bool navigator_video_channel(int);
#endif
extern "C" bool navigator_counter_started(NavigatorScene*, u8 const*, size_t, u8 const*, size_t);
extern "C" void navigator_counter_ended(NavigatorScene*, bool);
#ifdef ATRIUM_MAIL_PAGE
extern "C" NavigatorScene* navigator_mail_scene_open(bool, u8 const**, size_t*, u8 const**, size_t*);
extern "C" bool navigator_is_vpn_page(NavigatorScene*);
extern "C" bool navigator_vpn_forms(NavigatorScene*, u8 const*, size_t);
extern "C" void navigator_mail_scene_wait(NavigatorScene*);
#endif

// The I0 loader completes its one bounded broker request synchronously.
static ByteBuffer document_body;
static bool document_ok;

static bool fetch_fixture(Web::Page& page, URL::URL const& url)
{
    auto& heap = GC::Heap::the();
    document_ok = false;
    document_body.clear();
    if (!Web::ResourceLoader::is_initialized()) Web::ResourceLoader::initialize(heap);
    Web::LoadRequest request(HTTP::HeaderList::create());
    request.set_url(url);
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
    bool mail_probe = false;
#ifdef ATRIUM_I0_SIGN_IN
    struct stat input {};
    mail_probe = (argc == 2 && strcmp(argv[1], "--mail-first-screen-check") == 0)
        || (argc == 1 && !embedded && fstat(STDIN_FILENO, &input) == 0 && (S_ISREG(input.st_mode) || S_ISFIFO(input.st_mode)));
#endif
    if (argc != 1 && !embedded && !mail_probe)
        return 1;
    // A standby (navigator-islands.md D7) is started with /dev/null in
    // place of its fetch socket: it warms up first, then waits on its Limen
    // channel for the socket, which comes only when an island is asked for.
    [[maybe_unused]] bool standby = false;
#ifndef ATRIUM_MAIL_PAGE
    auto attach_fetch = [](int channel) {
#if defined(ATRIUM_I4_VIDEO) || defined(ATRIUM_I4_YOUTUBE)
        if (channel < 0 || !navigator_video_channel(channel)) return false;
#endif
        return channel >= 0 && navigator_fixture_channel(channel);
    };
#endif
    if (embedded) {
#ifndef ATRIUM_MAIL_PAGE
        struct stat out {};
        standby = fstat(STDOUT_FILENO, &out) == 0 && S_ISCHR(out.st_mode);
        int channel = standby ? -1 : dup(STDOUT_FILENO);
        if ((!standby && channel < 0) || dup2(STDERR_FILENO, STDOUT_FILENO) < 0)
            return 1;
        if (!standby && !attach_fetch(channel))
            return 1;
#endif
    }
    if (!Navigator::enter_fixture_capability_mode())
        return 1;
    if (Navigator::install_pinned_fonts().is_error())
        return 2;
    Core::EventLoop loop;
    Web::Platform::EventLoopPlugin::install(*new Web::Platform::EventLoopPlugin);
    Web::Bindings::initialize_agent(Web::HTML::AgentType::SimilarOriginWindow);
#if defined(ATRIUM_MAIL_PAGE) || defined(ATRIUM_I4_VIDEO) || defined(ATRIUM_I4_YOUTUBE) || defined(ATRIUM_I6_PAGE)
    auto& agent = Web::HTML::surrounding_agent();
    agent.fetch_executor = Web::Fetch::Fetching::create_inline_fetch_executor(agent.network_activity_observers);
#endif
    auto theme = Core::AnonymousBuffer::create_with_size(sizeof(Gfx::SystemTheme));
    if (theme.is_error())
        return 3;
    auto palette = Gfx::Palette(Gfx::PaletteImpl::create_with_anonymous_buffer(theme.release_value()));
    auto client = Navigator::EditorPage::create(move(palette));
    // Warm: fonts, event loop, JS agent and page client exist, and no site
    // input has been seen. Wait here for the island.
#ifndef ATRIUM_MAIL_PAGE
    if (standby && !attach_fetch(navigator_standby_fetch()))
        return 1;
#endif
#if defined(ATRIUM_I6_PAGE)
    // A whole page live (navigator-islands.md I6.1): the page comes from
    // navigatord as the answer to "this island's page"; the engine never
    // names it. Its resources go through the loader to navigator_live_get.
    if (!embedded) return 1;
    auto* scene = navigator_counter_scene_create();
    if (!scene) return 4;
    ScopeGuard cleanup = [&] { navigator_scene_destroy(scene); };
    if (!Web::ResourceLoader::is_initialized())
        Web::ResourceLoader::initialize(GC::Heap::the());
    auto storage = ByteBuffer::create_uninitialized(8 * 1024 * 1024);
    if (storage.is_error()) return 4;
    document_body = storage.release_value();
    static u8 page_url[8192];
    size_t page_url_length = 0;
    auto body_length = navigator_live_page(page_url, sizeof(page_url), &page_url_length, document_body.data(), document_body.size());
    if (!body_length || body_length > document_body.size() || document_body.try_resize(body_length).is_error()) return 4;
    StringView location { page_url, page_url_length };
    client->page().set_system_visibility_state(Web::HTML::VisibilityState::Visible);
    auto result = Navigator::paint_document(*client, scene, location, StringView(document_body.bytes()), false);
    if (result.is_error()) return 4;
    auto document = client->page().local_traversable()->active_document();
    auto origin = document->origin().serialize();
    auto title = document->title().to_well_formed_utf8();
    if (!navigator_counter_started(scene, origin.bytes().data(), origin.byte_count(), title.bytes().data(), title.byte_count()))
        return 4;
    client->island_scene = scene;
    auto status = Navigator::drive_page(*client, scene, loop);
    navigator_counter_ended(scene, status != 0);
    return status;
#elif defined(ATRIUM_I1_COUNTER)
    if (!embedded) return 1;
#ifdef ATRIUM_I4_YOUTUBE
    constexpr auto location = "https://www.youtube-nocookie.com/embed/M7lc1UVf-VE"sv;
#elif defined(ATRIUM_I4_VIDEO)
    constexpr auto location = "https://widget.example/navigator-i4/local-player.html"sv;
#else
    constexpr auto location = "https://widget.example/counter.html"sv;
#endif
    auto* scene = navigator_counter_scene_create();
    if (!scene) return 4;
    ScopeGuard cleanup = [&] { navigator_scene_destroy(scene); };
#ifdef ATRIUM_I4_YOUTUBE
    if (!Web::ResourceLoader::is_initialized())
        Web::ResourceLoader::initialize(GC::Heap::the());
    auto storage = ByteBuffer::create_uninitialized(512 * 1024);
    if (storage.is_error()) return 4;
    document_body = storage.release_value();
    auto body_length = navigator_fixture_get(location.bytes().data(), location.length(), document_body.data(), document_body.size());
    if (!body_length || body_length > document_body.size() || document_body.try_resize(body_length).is_error()) return 4;
#else
    if (!fetch_fixture(client->page(), URL::Parser::basic_parse(location).value()))
        return 4;
#endif
    // The broker placed this counter before delivering its granted document.
    client->page().set_system_visibility_state(Web::HTML::VisibilityState::Visible);
    auto result = Navigator::paint_document(*client, scene, location, StringView(document_body.bytes()), false);
    if (result.is_error()) return 4;
    auto document = client->page().local_traversable()->active_document();
    auto origin = document->origin().serialize();
    auto title = document->title().to_well_formed_utf8();
    if (!navigator_counter_started(scene, origin.bytes().data(), origin.byte_count(), title.bytes().data(), title.byte_count()))
        return 4;
    client->island_scene = scene;
    auto status = Navigator::drive_page(*client, scene, loop);
    navigator_counter_ended(scene, status != 0);
    return status;
#elif defined(ATRIUM_I0_SIGN_IN)
#ifdef ATRIUM_MAIL_PAGE
    if (!embedded) return 1;
    u8 const* url = nullptr;
    u8 const* body = nullptr;
    size_t url_len = 0, body_len = 0;
#ifdef ATRIUM_INBOX_PAGE
    constexpr bool inbox = true;
#else
    constexpr bool inbox = false;
#endif
    auto* scene = navigator_mail_scene_open(inbox, &url, &url_len, &body, &body_len);
    if (!scene) return 4;
    ScopeGuard cleanup = [&] { navigator_scene_destroy(scene); };
    if (!Web::ResourceLoader::is_initialized())
        Web::ResourceLoader::initialize(GC::Heap::the());
    auto painted = Navigator::paint_document(*client, scene, { url, url_len }, { body, body_len }, false);
    if (painted.is_error()) return 4;
    if (navigator_is_vpn_page(scene)) {
        auto doc = client->page().local_traversable()->active_document();
        auto root = doc->document_element();
        if (!root) return 4;
        auto html = root->inner_html();
        if (html.is_exception()) return 4;
        auto utf8 = html.release_value().to_well_formed_utf8();
        if (!navigator_vpn_forms(scene, utf8.bytes().data(), utf8.byte_count())) return 4;
    }
    return Navigator::drive_page(*client, scene, loop);
#endif
    if (mail_probe) {
        // The development gate pipes the HTML that came back through the
        // cookie courier. The jailed process has no path or network grant.
        char html[65537];
        auto size = fread(html, 1, sizeof(html), stdin);
        if (!size || size == sizeof(html) || ferror(stdin))
            return 4;
        auto* scene = navigator_scene_create(false);
        if (!scene) return 4;
        ScopeGuard cleanup = [&] { navigator_scene_destroy(scene); };
        auto probe = Navigator::paint_document(*client, scene, "https://login.microsoftonline.com/first"sv, { html, size }, true);
        if (probe.is_error()) {
            warnln("Mail development document: {}", probe.error());
            return 4;
        }
        return 0;
    }
    auto result = Navigator::run_sign_in(client->palette(), [](Web::Page& page, URL::URL const& url) -> ErrorOr<StringView> {
        if (!fetch_fixture(page, url))
            return Error::from_string_literal("broker did not return the common fixture");
        return StringView(document_body.bytes());
    });
#else
    StringView html { fixture, sizeof(fixture) };
    if (embedded) {
        if (!fetch_fixture(client->page(), URL::Parser::basic_parse("https://editor.example/editor.html"sv).value())) {
            warnln("Fixture broker GET failed");
            return 4;
        }
        html = StringView(document_body.bytes());
        outln("nuntius=GET fixture_matches=1 bytes={}", html.length());
    }
    auto result = client->load_fixture(html, embedded);
#endif
    if (result.is_error()) {
        warnln("LibWeb fixture failed: {}", result.error());
        return 4;
    }
    return 0;
}
