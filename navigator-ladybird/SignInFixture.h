#pragma once

#include "EditorPage.h"
#ifdef ATRIUM_I4_YOUTUBE
#include <LibWeb/HTML/EventLoop/EventLoop.h>
#include <LibWeb/HighResolutionTime/TimeOrigin.h>
#endif
#include <LibWebCommon/UIEvents/MouseButton.h>
#include <AK/ScopeGuard.h>
#include <LibURL/Parser.h>
#include <LibWeb/DOM/Document.h>
#include <LibWeb/HTML/HTMLButtonElement.h>
#include <LibWeb/HTML/HTMLInputElement.h>
#include <LibWeb/HTML/LocalTraversableNavigable.h>
#include <LibWeb/HTML/PaintConfig.h>
#include <LibWeb/HTML/Parser/HTMLParser.h>
#include <LibWeb/HTML/Parser/ParserScriptingMode.h>
#include <LibWeb/HTML/Scripting/Environments.h>
#include <LibWeb/HTML/Scripting/TemporaryExecutionContext.h>
#include <LibWeb/Layout/RenderDocument.h>
#include <LibWeb/Painting/PaintableTypes.h>
#include <sys/resource.h>
#include <csignal>
#include <malloc_np.h>
#include <LibGC/Heap.h>
#if defined(ATRIUM_I4_VIDEO) || defined(ATRIUM_I4_YOUTUBE)
#include <LibWeb/HTML/HTMLMediaElement.h>
#endif

struct NavigatorScene;
extern "C" NavigatorScene* navigator_scene_create(bool);
extern "C" void navigator_scene_destroy(NavigatorScene*);
extern "C" bool navigator_scene_sample(NavigatorScene*, Web::Layout::RustFFI::DocumentHost*, bool, bool);

#if defined(ATRIUM_MAIL_PAGE) && !defined(ATRIUM_INBOX_PAGE)
extern "C" void navigator_get_realm(void const*);
#endif
#ifdef ATRIUM_MAIL_PAGE
extern "C" bool navigator_is_vpn_page(NavigatorScene*);
#endif

namespace Navigator {

// The development provider sets this field from an inline script after a
// cookie-authenticated redirect. Exercise DOM, script, layout and scene building.
inline ErrorOr<void> paint_document(EditorPage& client, NavigatorScene* scene, StringView location, StringView html, bool check_fixture)
{
    auto nav = Web::HTML::LocalTraversableNavigable::create_a_new_top_level_traversable(client.page(), nullptr);
    client.page().set_top_level_traversable(nav);
#ifdef ATRIUM_MAIL_PAGE
    // A VPN sign-in page has the window's whole page area (about 1030 x 620
    // at 1280 x 840), not the Mail sheet's 640 x 360: an IdP's button below
    // its first screen must be on it, since the page does not scroll.
    nav->set_viewport_size(navigator_is_vpn_page(scene) ? Web::CSSPixelSize { 1024, 600 } : Web::CSSPixelSize { 640, 360 });
#elif defined(ATRIUM_I6_PAGE)
    // A live page (I6.1) covers the window's page area, not an island's
    // 640 x 360. Until navigatord tells the engine the placed size (I6.2,
    // just after it reports its start), this is a 1280 x 840 window's.
    nav->set_viewport_size({ 1024, 768 });
#else
    nav->set_viewport_size({ 640, 360 });
#endif
    auto doc = nav->active_document();
    auto parsed = URL::Parser::basic_parse(location);
    if (!parsed.has_value() || parsed->scheme() != "https"sv) return Error::from_string_literal("invalid document origin");
    auto& url = *parsed;
    doc->set_url(url);
    doc->set_origin(url.origin());
#ifdef ATRIUM_I4_YOUTUBE
    doc->set_referrer(u"https://org.atrium.navigator/"_utf16);
#endif
#if defined(ATRIUM_MAIL_PAGE) && !defined(ATRIUM_INBOX_PAGE)
    navigator_get_realm(&Web::HTML::relevant_realm(*doc));
#endif
    doc->remove_all_children();
#if defined(ATRIUM_MAIL_PAGE) || defined(ATRIUM_I6_PAGE)
    // Run "the end" of parsing too: module and deferred scripts run only
    // there, and DOMContentLoaded and load fire there. An IdP's sign-in page
    // (Keycloak's security-key pages) is module scripts (vpn.md "Scripts in
    // the sign-in window").
    Web::HTML::HTMLParser::create_from_byte_string(*doc, html, Web::HTML::ParserScriptingMode::Normal, "utf-8"sv)->run(url);
#else
    Web::HTML::HTMLParser::create_from_byte_string(*doc, html, Web::HTML::ParserScriptingMode::Normal, "utf-8"sv)->run();
#endif
#ifdef ATRIUM_I4_VIDEO
    // The Limen player owns this rectangle, including its playback controls.
    // Keep the element in layout so the native slot uses its CSS geometry.
    if (auto video = doc->get_element_by_id(u"clip"sv))
        video->set_attribute("style"_fly_string, u"visibility: hidden"_utf16);
#endif
    auto field = doc->get_element_by_id(u"account"sv);
    auto label = doc->get_element_by_id(u"prompt"sv);
    if (check_fixture && (!field || !is<Web::HTML::HTMLInputElement>(*field) || !label || label->text_content() != u"Enter your account"_utf16))
        return Error::from_string_literal("missing account input or script-generated prompt");

    Web::Layout::ForcedReadScope read { *doc };
    doc->update_layout(Web::DOM::UpdateLayoutReason::DumpDisplayList);
    if (!doc->record_display_list(read, Web::HTML::PaintConfig {}, Web::Painting::PaintCommandCacheMode::ReadWrite)
        || !navigator_scene_sample(scene, doc->layout_node_arena().host(), true, false))
        return Error::from_string_literal("development form could not paint");
    if (check_fixture) puts("development_account_input=1 inline_script=1 native_scene=1");
    return {};
}

#if defined(ATRIUM_MAIL_PAGE) || defined(ATRIUM_I1_COUNTER)
extern "C" int navigator_page_pointer(NavigatorScene*, float*, float*);

#ifdef ATRIUM_I1_COUNTER
extern "C" bool navigator_take_chapter(NavigatorScene*, double*);
extern "C" bool navigator_video_ready(NavigatorScene*);
extern "C" bool navigator_counter_resize(NavigatorScene*, u16, u16);
#endif
#ifdef ATRIUM_I6_PAGE
extern "C" bool navigator_take_page_size(NavigatorScene*, u16*, u16*);
extern "C" bool navigator_live_url_changed(NavigatorScene*, u8 const*, size_t);
extern "C" int navigator_take_file_picked(NavigatorScene*, u8*, size_t, size_t*);
extern "C" ssize_t navigator_live_get(u8 const*, size_t, u8*, size_t, u16*, u8*, size_t, size_t*);
extern "C" bool navigator_take_key(NavigatorScene*, u8*, u32*, u8*, bool*, u8*);
extern "C" bool navigator_live_copy(NavigatorScene*, u8 const*, size_t, u8 const*);
extern "C" bool navigator_live_paste(NavigatorScene*, u8 const*);
extern "C" size_t navigator_take_pasted(NavigatorScene*, u8*, size_t);

// A live page's key without text (painter.rs `live_key`) as LibWeb's code.
inline Web::UIEvents::KeyCode live_key_code(u8 special)
{
    using K = Web::UIEvents::KeyCode;
    switch (special) {
    case 1: return K::Key_Return;
    case 2: return K::Key_Backspace;
    case 3: return K::Key_Tab;
    case 4: return K::Key_Escape;
    case 5: return K::Key_Delete;
    case 6: return K::Key_Left;
    case 7: return K::Key_Right;
    case 8: return K::Key_Up;
    case 9: return K::Key_Down;
    case 10: return K::Key_Home;
    case 11: return K::Key_End;
    default: return K::Key_Invalid;
    }
}

// The HID modifier byte (either hand) as LibWeb's modifiers.
inline unsigned live_modifiers(u8 hid)
{
    unsigned m = 0;
    if (hid & 0x11) m |= Web::UIEvents::KeyModifier::Mod_Ctrl;
    if (hid & 0x22) m |= Web::UIEvents::KeyModifier::Mod_Shift;
    if (hid & 0x44) m |= Web::UIEvents::KeyModifier::Mod_Alt;
    if (hid & 0x88) m |= Web::UIEvents::KeyModifier::Mod_Super;
    return m;
}
#endif

// memoryd's first rung under pressure is TRIM, SIGINFO: "shed your caches
// but keep running" (atrium-memory-pressure.md). The handler only notes it;
// the page loop sheds between pumps, with no script on the stack: a full
// garbage collection, then jemalloc hands every arena's dirty pages back.
// An engine that ignored it would be asked to exit, then killed.
inline volatile sig_atomic_t trim_asked = 0;

inline void note_trim(int) { trim_asked = 1; }

inline void listen_for_trim()
{
    struct sigaction sa {};
    sa.sa_handler = note_trim;
    sigemptyset(&sa.sa_mask);
    sa.sa_flags = SA_RESTART;
    if (sigaction(SIGINFO, &sa, nullptr) != 0)
        warnln("island: no SIGINFO handler; memoryd's trim will go unheard");
}

inline void trim_if_asked()
{
    if (!trim_asked) return;
    trim_asked = 0;
    GC::Heap::the().collect_garbage(GC::Heap::CollectionType::CollectGarbage);
    // MALLCTL_ARENAS_ALL is 4096 in jemalloc.
    int purged = mallctl("arena.4096.purge", nullptr, nullptr, nullptr, 0);
    warnln("island: trimmed for memory pressure (garbage collected, arenas purged: {})", purged == 0 ? "yes" : "no");
}

inline int drive_page(EditorPage& client, NavigatorScene* scene, Core::EventLoop& loop)
{
    listen_for_trim();
    auto doc = client.page().local_traversable()->active_document();
    auto tree = doc->dom_tree_version();
    auto text = doc->character_data_version();
#if defined(ATRIUM_I4_VIDEO) || defined(ATRIUM_I4_YOUTUBE)
    bool video_reported = false;
#endif
#ifdef ATRIUM_I1_COUNTER
    Gfx::IntSize last_extent { 640, 360 };
#endif
#ifdef ATRIUM_I6_PAGE
    auto last_url = doc->url();
#endif
#if defined(ATRIUM_I4_YOUTUBE) || defined(ATRIUM_I6_PAGE)
    // A video or a live page runs for as long as the person keeps it: the
    // fixtures' tick bound (15,000 input polls, about 5 minutes) ended a
    // live page mid-use. navigatord ends the run; this only bounds the loop.
#    ifdef ATRIUM_I6_PAGE
    auto deadline = AK::MonotonicTime::now() + AK::Duration::from_seconds(12 * 3600);
#    else
    auto deadline = AK::MonotonicTime::now() + AK::Duration::from_seconds(3600);
#    endif
    for (unsigned tick = 0; AK::MonotonicTime::now() < deadline; ++tick) {
#else
    for (unsigned tick = 0; tick < 15000; ++tick) {
#endif
        // Run everything that is ready before navigator_page_pointer waits
        // up to 20 ms on frescod for input. One pump per tick made every
        // task hop (a resolved fetch, a promise reaction, a media event)
        // wait out that input poll, the likely cause of the ~200 ms gaps
        // between the I5 page's steps (navigator-islands.md, I5 trace).
        // Bounded, so a page that keeps posting tasks still reads input.
        for (int drained = 0; drained < 256 && loop.pump(Core::EventLoop::WaitMode::PollForEvents) > 0; ++drained) { }
        trim_if_asked();
#ifdef ATRIUM_I1_COUNTER
        if (client.closed) break;
#endif
        float x = 0, y = 0;
        auto action = navigator_page_pointer(scene, &x, &y);
#if defined(ATRIUM_I4_VIDEO) || defined(ATRIUM_I4_YOUTUBE)
        if (!video_reported || action == 4) {
#ifdef ATRIUM_I4_YOUTUBE
            auto element = doc->query_selector(u"#movie_player video"_utf16).release_value_but_fixme_should_propagate_errors();
#else
            auto element = doc->get_element_by_id(u"clip"sv);
#endif
            if (element && is<Web::HTML::HTMLMediaElement>(*element)) {
                auto& video = as<Web::HTML::HTMLMediaElement>(*element);
                if (video.ready_state() >= Web::HTML::HTMLMediaElement::ReadyState::HaveMetadata) {
                    if (!video_reported) {
                        if (!navigator_video_ready(scene)) return 4;
                        video_reported = true;
                    }
                    double seconds = 0;
                    if (action == 4 && navigator_take_chapter(scene, &seconds)) video.set_current_time(seconds);
                }
            }
        }
#endif
        if (action < 0) {
            warnln("island: the embed ended (pointer {}); ending", action);
            break;
        }
#ifdef ATRIUM_I6_PAGE
        // The page changed its own address (pushState, I6.2): the trail
        // follows. One it may not report (not plain HTTPS) is only logged.
        if (doc->url() != last_url) {
            last_url = doc->url();
            auto address = last_url.serialize();
            if (!navigator_live_url_changed(scene, address.bytes().data(), address.bytes().size()))
                warnln("live page: address {} not reported", address);
        }
#endif
#ifdef ATRIUM_I6_PAGE
        // The person's answer to the page's file input (I6.4): the picked
        // file's bytes are one GET on the fetch channel; no file, or one
        // that can't be fetched, closes the input empty.
        if (action == 6) {
            u8 name[256];
            size_t name_length = 0;
            int picked = navigator_take_file_picked(scene, name, sizeof(name), &name_length);
            if (picked >= 0) {
                Vector<Web::HTML::SelectedFile> files;
                if (picked == 1) {
                    auto buffer = ByteBuffer::create_uninitialized(8 * 1024 * 1024);
                    constexpr auto address = "https://live-page.atrium.invalid/picked-file"sv;
                    u16 status = 0;
                    u8 kind[128];
                    size_t kind_length = 0;
                    ssize_t got = buffer.is_error() ? -1 : navigator_live_get(reinterpret_cast<u8 const*>(address.characters_without_null_termination()), address.length(),
                        buffer.value().data(), buffer.value().size(), &status, kind, sizeof(kind), &kind_length);
                    if (got >= 0 && status == 200) {
                        auto contents = buffer.release_value();
                        contents.resize(static_cast<size_t>(got));
                        // The name is navigatord's (made safe), but never trusted to be UTF-8.
                        auto file_name = Utf16String::try_from_utf8(StringView { name, name_length });
                        files.append(Web::HTML::SelectedFile { file_name.is_error() ? Utf16String::from_utf8("file"sv) : file_name.release_value(), move(contents) });
                    } else {
                        warnln("live page: the picked file could not be fetched");
                    }
                }
                client.page().file_picker_closed(files.span());
            }
        }
        // The keyboard (I6.4). Ctrl-C and Ctrl-V are the clipboard's: the
        // selection, or the paste, goes with the press's token to
        // navigatord, and the UI spends it on Tabula. Ctrl-A selects all
        // unless the page took it. Anything else is the page's.
        if (action == 7) {
            u8 special = 0, mods = 0;
            u32 code_point = 0;
            bool pressed = false;
            u8 token[16] {};
            if (navigator_take_key(scene, &special, &code_point, &mods, &pressed, token)) {
                bool ctrl = (mods & 0x11) && !(mods & 0x44);
                if (pressed && ctrl && (code_point == 'c' || code_point == 'C')) {
                    auto navigable = client.page().hosted_focused_navigable();
                    auto utf8 = navigable ? navigable->selected_text().to_well_formed_utf8() : String {};
                    if (!utf8.is_empty() && !navigator_live_copy(scene, utf8.bytes().data(), utf8.bytes().size(), token))
                        warnln("live page: the copy was not handed over");
                } else if (pressed && ctrl && (code_point == 'v' || code_point == 'V')) {
                    if (!navigator_live_paste(scene, token))
                        warnln("live page: the paste was not asked for");
                } else if (pressed && ctrl && (code_point == 'a' || code_point == 'A')) {
                    // Select all is the browser's, as Ladybird's own
                    // WebContent does it, unless the page took the key
                    // first (preventDefault, or a key it handled).
                    auto result = client.page().handle_keydown(live_key_code(special), live_modifiers(mods), code_point, false, false);
                    if (result == Web::EventResult::Accepted) {
                        if (auto navigable = client.page().hosted_focused_navigable())
                            navigable->select_all();
                    }
                } else if (pressed) {
                    client.page().handle_keydown(live_key_code(special), live_modifiers(mods), code_point, false, true);
                } else {
                    client.page().handle_keyup(live_key_code(special), live_modifiers(mods), code_point, false);
                }
            }
        }
        // The text Tabula gave for the paste: typed where the focus is.
        if (action == 8) {
            auto buffer = ByteBuffer::create_uninitialized(16 * 1024);
            size_t length = buffer.is_error() ? 0 : navigator_take_pasted(scene, buffer.value().data(), buffer.value().size());
            if (length > 0) {
                auto text = StringView { buffer.value().data(), length };
                for (u32 code_point : Utf8View { text }) {
                    auto key = code_point == '\n' ? Web::UIEvents::KeyCode::Key_Return : Web::UIEvents::KeyCode::Key_Invalid;
                    client.page().handle_keydown(key, 0, code_point, false, true);
                    client.page().handle_keyup(key, 0, code_point, false);
                }
            }
        }
        // navigatord placed the page in a page area of this size (I6.2).
        u16 width = 0, height = 0;
        if (action == 5 && navigator_take_page_size(scene, &width, &height))
            client.page().local_traversable()->set_viewport_size({ static_cast<int>(width), static_cast<int>(height) });
#endif
        Web::DevicePixelPoint point { static_cast<int>(x), static_cast<int>(y) };
        if (action == 1) client.page().handle_mousedown(point, point, Web::UIEvents::MouseButton::Primary, Web::UIEvents::MouseButton::Primary, 0, 1);
        if (action == 2) client.page().handle_mouseup(point, point, Web::UIEvents::MouseButton::Primary, 0, 0);
        bool requested = client.take_frame_request();
#ifdef ATRIUM_I4_YOUTUBE
        if (requested) {
            auto& event_loop = Web::HTML::main_thread_event_loop();
            event_loop.request_rendering_update();
            event_loop.rendering_opportunity(Web::HighResolutionTime::unsafe_shared_current_time(), Web::HTML::EventLoop::RenderingOpportunitySource::LocalTimer);
            loop.pump(Core::EventLoop::WaitMode::PollForEvents);
        }
#endif
        if (!requested && !action && tree == doc->dom_tree_version() && text == doc->character_data_version())
            continue;
        tree = doc->dom_tree_version();
        text = doc->character_data_version();
        Web::Layout::ForcedReadScope read { *doc };
        doc->update_layout(Web::DOM::UpdateLayoutReason::DumpDisplayList);
#ifdef ATRIUM_I1_COUNTER
        if (auto root = doc->document_element()) {
            Gfx::IntSize extent { root->scroll_width(), root->scroll_height() };
            if (extent != last_extent && extent.width() > 0 && extent.height() > 0
                && extent.width() <= 16384 && extent.height() <= 16384) {
                if (!navigator_counter_resize(scene, extent.width(), extent.height())) {
                    warnln("island: the page's new extent {}x{} could not be reported; ending", extent.width(), extent.height());
                    return 6;
                }
                last_extent = extent;
            }
        }
#endif
        if (!doc->record_display_list(read, Web::HTML::PaintConfig {}, Web::Painting::PaintCommandCacheMode::ReadWrite)
            || !navigator_scene_sample(scene, doc->layout_node_arena().host(), true, false)) {
            // Loud: an island that stops painting otherwise just disappears.
            warnln("island: the page's scene could not be recorded or sent; ending");
            return 6;
        }
    }
    return 0;
}
#endif

#ifdef ATRIUM_I0_SIGN_IN
inline ErrorOr<void> run_sign_in(Gfx::Palette palette, Function<ErrorOr<StringView>(Web::Page&, URL::URL const&)> fetch)
{
    // The fixture launcher owns navigation. Each accepted replacement gets a fresh
    // PageClient and realm; only the assigned Fresco scene survives across documents.
    auto* scene = navigator_scene_create(true);
    if (!scene)
        return Error::from_string_literal("missing sign-in embed");
    ScopeGuard cleanup = [&] { navigator_scene_destroy(scene); };
    auto url = URL::Parser::basic_parse("https://idp.example/sign-in.html"sv).value();
    for (auto step : { ""sv, "authorize"sv, "password"sv, "callback"sv }) {
        auto host = EditorPage::create(palette);
        auto& client = *host;
        auto html = TRY(fetch(client.page(), url));
        auto nav = Web::HTML::LocalTraversableNavigable::create_a_new_top_level_traversable(client.page(), nullptr);
        client.page().set_top_level_traversable(nav);
        nav->set_viewport_size({ 640, 360 });
        auto doc = nav->active_document();
        doc->set_url(url);
        doc->set_origin(url.origin());
        doc->remove_all_children();
        Web::HTML::HTMLParser::create_from_byte_string(*doc, html, Web::HTML::ParserScriptingMode::Normal, "utf-8"sv)->run();
        auto status = doc->get_element_by_id(u"result"sv);
        if (!status) return Error::from_string_literal("missing fixture status");
        auto draw = [&] {
            Web::Layout::ForcedReadScope read { *doc };
            doc->update_layout(Web::DOM::UpdateLayoutReason::DumpDisplayList);
            return doc->record_display_list(read, Web::HTML::PaintConfig {}, Web::Painting::PaintCommandCacheMode::ReadWrite)
                && navigator_scene_sample(scene, doc->layout_node_arena().host(), true, false);
        };
        if (step == "password"sv) {
            auto input = doc->get_element_by_id(u"password"sv);
            auto button = doc->query_selector(u"button"_utf16).release_value_but_fixme_should_propagate_errors();
            if (!input || !is<Web::HTML::HTMLInputElement>(*input) || !button || !is<Web::HTML::HTMLButtonElement>(*button)
                || status->text_content() != u"Enter the public fixture password"_utf16 || client.navigation_refused || client.pending_navigation.has_value())
                return Error::from_string_literal("password step did not render");
            if (!draw()) return Error::from_string_literal("password paint failed");
            Web::HTML::TemporaryExecutionContext context { Web::HTML::relevant_realm(*doc) };
            auto& field = static_cast<Web::HTML::HTMLInputElement&>(*input);
            auto& submit = static_cast<Web::HTML::HTMLButtonElement&>(*button);
            if (field.set_value(u"wrong-public-value"sv).is_error()) return Error::from_string_literal("cannot set trial value");
            submit.click();
            if (status->text_content() != u"Fixture password refused"_utf16 || client.navigation_refused || client.pending_navigation.has_value())
                return Error::from_string_literal("rejection did not keep the password page");
            if (!draw()) return Error::from_string_literal("refusal paint failed");
            if (field.set_value(u"atrium-i0-fixture"sv).is_error()) return Error::from_string_literal("cannot set fixture value");
            submit.click();
            if (!field.value().is_empty()) return Error::from_string_literal("fixture failed to clear the field");
        }
        if (step == "callback"sv) {
            if (client.navigation_refused || client.pending_navigation.has_value() || status->text_content() != u"I0 fixture sign-in complete"_utf16 || !draw())
                return Error::from_string_literal("callback status or paint failed");
            struct rusage usage {};
            if (getrusage(RUSAGE_SELF, &usage)) return Error::from_errno(errno);
            outln("sign_in_complete=1 rejected=1 field_cleared=1 maxrss_kib={}", usage.ru_maxrss);
            return {};
        }
        if (client.navigation_refused || !client.pending_navigation.has_value()) return Error::from_string_literal("fixture script did not request navigation");
        url = client.pending_navigation.release_value();
    }
    return Error::from_string_literal("sign-in callback missing");
}
#endif
}
